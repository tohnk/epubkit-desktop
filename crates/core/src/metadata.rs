//! OPF metadata: extraction, user edits, store-tag stripping, and output
//! filenames. Port of `metadata_handler.py`.

use libxml::tree::{Document, Node};
use unicode_normalization::UnicodeNormalization;

use crate::xml;
use crate::Result;

pub const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
pub const NS_OPF: &str = "http://www.idpf.org/2007/opf";

/// Reader- and store-specific metadata with no meaning outside the shop it
/// came from.
const STORE_META_NAMES: &[&str] = &[
    "calibre:timestamp",
    "calibre:title_sort",
    "calibre:author_link_map",
    "calibre:series",
    "calibre:series_index",
    "calibre:rating",
    "calibre:user_categories",
    "calibre:user_metadata",
    "ibooks:version",
    "ibooks:specified-fonts",
    "Sigil version",
    "dtb:uid",
];

const STORE_META_PREFIXES: &[&str] = &["calibre:", "ibooks:", "amazon:", "kindle:"];

/// Characters that cause trouble in filenames, and what to put in their place.
const FILENAME_REPLACEMENTS: &[(char, &str)] = &[
    ('/', "-"),
    ('\\', "-"),
    (':', " -"),
    ('*', ""),
    ('?', ""),
    ('"', "'"),
    ('<', ""),
    ('>', ""),
    ('|', "-"),
];

/// Leave room for the `.epub` extension within common filesystem limits.
const MAX_FILENAME_CHARS: usize = 200;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    pub title: String,
    pub author: String,
    pub series: String,
    pub series_index: String,
    pub language: String,
    pub cover_id: String,
    /// Cover path, relative to the OPF's directory.
    pub cover_href: String,
}

/// User-supplied overrides. `None` leaves the book's own value alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataEdits {
    pub title: Option<String>,
    pub author: Option<String>,
    pub language: Option<String>,
}

impl MetadataEdits {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.author.is_none() && self.language.is_none()
    }
}

/// Read metadata out of a parsed OPF package document.
pub fn extract_metadata(doc: &Document) -> Result<Metadata> {
    let mut metadata = Metadata {
        title: dc_text(doc, "title")?,
        author: dc_text(doc, "creator")?,
        language: dc_text(doc, "language")?,
        ..Metadata::default()
    };

    // Series lives in a Calibre `<meta name>` under EPUB 2, and in a
    // `<meta property>` under EPUB 3.
    for meta in meta_elements(doc)? {
        let name = meta.get_attribute("name").unwrap_or_default();
        let content = meta.get_attribute("content").unwrap_or_default();
        let property = meta.get_attribute("property").unwrap_or_default();
        let text = meta.get_content().trim().to_string();

        match (name.as_str(), property.as_str()) {
            ("calibre:series", _) if !content.is_empty() => metadata.series = content,
            ("calibre:series_index", _) if !content.is_empty() => metadata.series_index = content,
            (_, "belongs-to-collection") if !text.is_empty() => metadata.series = text,
            (_, "group-position") if !text.is_empty() => metadata.series_index = text,
            _ => {}
        }
    }

    metadata.cover_id = find_cover_id(doc)?;
    if !metadata.cover_id.is_empty() {
        for item in manifest_items(doc)? {
            if item.get_attribute("id").as_deref() == Some(metadata.cover_id.as_str()) {
                metadata.cover_href = item.get_attribute("href").unwrap_or_default();
                break;
            }
        }
    }

    Ok(metadata)
}

/// Apply user edits, creating Dublin Core elements that the book lacks.
pub fn update_metadata(doc: &Document, edits: &MetadataEdits) -> Result<()> {
    let Some(mut metadata_el) = xml::find_first(doc, &format!("//{}", xml::local("metadata")))?
    else {
        return Ok(());
    };

    for (local_name, value) in [
        ("title", edits.title.as_deref()),
        ("creator", edits.author.as_deref()),
        ("language", edits.language.as_deref()),
    ] {
        let Some(value) = value.filter(|v| !v.is_empty()) else {
            continue;
        };

        if let Some(mut existing) = dc_element(doc, local_name)? {
            existing.set_content(value).ok();
        } else {
            let namespace = xml::namespace_for(doc, &mut metadata_el, NS_DC)?;
            if let Ok(mut created) = metadata_el.new_child(namespace, local_name) {
                created.set_content(value).ok();
            }
        }
    }

    Ok(())
}

/// Remove store- and reader-specific `<meta>` entries. Returns how many went.
pub fn strip_store_metadata(doc: &Document) -> Result<usize> {
    let Some(metadata_el) = xml::find_first(doc, &format!("//{}", xml::local("metadata")))? else {
        return Ok(0);
    };

    let mut removed = 0;
    let candidates =
        xml::find_nodes_under(doc, &metadata_el, &format!(".//{}", xml::local("meta")))?;

    for mut meta in candidates {
        let name = meta.get_attribute("name").unwrap_or_default();
        let property = meta.get_attribute("property").unwrap_or_default();

        let is_store_tag = STORE_META_NAMES.contains(&name.as_str())
            || STORE_META_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix) || property.starts_with(prefix));

        if is_store_tag {
            meta.unlink();
            removed += 1;
        }
    }

    Ok(removed)
}

/// Build an `Author - Title.epub` filename, degrading gracefully when either
/// field is missing.
pub fn format_filename(title: &str, author: &str) -> String {
    let title = title.trim();
    let author = author.trim();

    let name = match (author.is_empty(), title.is_empty()) {
        (false, false) => format!("{author} - {title}"),
        (true, false) => title.to_string(),
        (false, true) => author.to_string(),
        (true, true) => "optimized".to_string(),
    };

    let mut name = sanitize_filename(&name);

    // Truncate by characters, not bytes — the latter would split a multi-byte
    // codepoint and panic.
    if name.chars().count() > MAX_FILENAME_CHARS {
        name = name.chars().take(MAX_FILENAME_CHARS).collect();
        name = name.trim_end().to_string();
    }

    format!("{name}.epub")
}

// ---------------------------------------------------------------- internals

/// Look up a Dublin Core element, preferring a correctly namespaced one but
/// accepting a bare local name — plenty of EPUBs omit the declaration.
fn dc_element(doc: &Document, local_name: &str) -> Result<Option<Node>> {
    let namespaced = format!("//*[local-name()='{local_name}' and namespace-uri()='{NS_DC}']");
    if let Some(node) = xml::find_first(doc, &namespaced)? {
        return Ok(Some(node));
    }
    xml::find_first(doc, &format!("//{}", xml::local(local_name)))
}

fn dc_text(doc: &Document, local_name: &str) -> Result<String> {
    Ok(dc_element(doc, local_name)?
        .map(|node| node.get_content().trim().to_string())
        .unwrap_or_default())
}

fn meta_elements(doc: &Document) -> Result<Vec<Node>> {
    xml::find_nodes(doc, &format!("//{}", xml::local("meta")))
}

fn manifest_items(doc: &Document) -> Result<Vec<Node>> {
    xml::find_nodes(
        doc,
        &format!("//{}/{}", xml::local("manifest"), xml::local("item")),
    )
}

/// Identify the cover image's manifest id, trying the three conventions books
/// actually use, in descending order of reliability.
fn find_cover_id(doc: &Document) -> Result<String> {
    let items = manifest_items(doc)?;

    // EPUB 3: properties="cover-image".
    for item in &items {
        if item
            .get_attribute("properties")
            .unwrap_or_default()
            .contains("cover-image")
        {
            return Ok(item.get_attribute("id").unwrap_or_default());
        }
    }

    // EPUB 2: <meta name="cover" content="id">.
    for meta in meta_elements(doc)? {
        if meta.get_attribute("name").as_deref() == Some("cover") {
            return Ok(meta.get_attribute("content").unwrap_or_default());
        }
    }

    // Last resort: an image whose id merely looks like a cover.
    for item in &items {
        let id = item.get_attribute("id").unwrap_or_default();
        let media_type = item.get_attribute("media-type").unwrap_or_default();
        if id.to_ascii_lowercase().contains("cover")
            && media_type.to_ascii_lowercase().starts_with("image/")
        {
            return Ok(id);
        }
    }

    Ok(String::new())
}

fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());

    for ch in name.chars() {
        match FILENAME_REPLACEMENTS.iter().find(|(from, _)| *from == ch) {
            Some((_, to)) => out.push_str(to),
            // Control characters would be legal in some filesystems and
            // baffling in all of them.
            None if ch.is_control() => {}
            None => out.push(ch),
        }
    }

    let out: String = out.nfc().collect();

    // Collapse runs of whitespace, and of dashes.
    let mut collapsed = String::with_capacity(out.len());
    let mut last: Option<char> = None;
    for ch in out.chars() {
        let ch = if ch.is_whitespace() { ' ' } else { ch };
        let repeated =
            matches!(last, Some(prev) if (prev == ' ' && ch == ' ') || (prev == '-' && ch == '-'));
        if !repeated {
            collapsed.push(ch);
        }
        last = Some(ch);
    }

    collapsed.trim().to_string()
}
