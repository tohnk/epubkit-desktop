//! OPF metadata: extraction, user edits, store-tag stripping, and output
//! filenames. Port of `metadata_handler.py`.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use libxml::tree::{Document, Node};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use unicode_normalization::UnicodeNormalization;

use crate::xml;
use crate::{Error, Result};

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

/// A filename template longer than a filename is a mistake.
const MAX_TEMPLATE_CHARS: usize = 200;

/// The year in a `dc:date`, which may be a bare year, a date, or a timestamp.
static YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(\d{4})\b").unwrap());

/// What a custom filename template can fill in.
pub const TEMPLATE_FIELDS: &[&str] = &[
    "title",
    "author",
    "year",
    "series",
    "series_index",
    "language",
    "original",
];

/// How an optimized book's file is named.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FilenameFormat {
    /// The name of the file it came from.
    Original,
    TitleAuthor,
    #[default]
    AuthorTitle,
    Title,
    /// Filled in from [`FilenameOptions::template`].
    Custom,
}

impl FilenameFormat {
    pub const ALL: [FilenameFormat; 5] = [
        FilenameFormat::Original,
        FilenameFormat::TitleAuthor,
        FilenameFormat::AuthorTitle,
        FilenameFormat::Title,
        FilenameFormat::Custom,
    ];

    /// As written in the settings file, on the command line and in the page.
    pub fn name(self) -> &'static str {
        match self {
            FilenameFormat::Original => "original",
            FilenameFormat::TitleAuthor => "title-author",
            FilenameFormat::AuthorTitle => "author-title",
            FilenameFormat::Title => "title",
            FilenameFormat::Custom => "custom",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.name() == name)
    }
}

/// The chosen [`FilenameFormat`], and the template `Custom` uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilenameOptions {
    pub format: FilenameFormat,
    /// Kept whatever the format, so choosing Custom again brings it back.
    pub template: String,
}

impl Default for FilenameOptions {
    fn default() -> Self {
        Self {
            format: FilenameFormat::AuthorTitle,
            template: "{title} - {author}".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    pub title: String,
    pub author: String,
    /// From `dc:date`, the first four-digit year in it.
    pub year: String,
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
        year: YEAR
            .captures(&dc_text(doc, "date")?)
            .map(|found| found[1].to_string())
            .unwrap_or_default(),
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
    finish_filename(&join_parts(author.trim(), title.trim()))
}

/// Name an optimized book from its metadata. `original` is the name of the
/// file it came from.
pub fn output_filename(
    metadata: &Metadata,
    options: &FilenameOptions,
    original: &str,
) -> Result<String> {
    let title = metadata.title.trim();
    let author = metadata.author.trim();
    let original = Path::new(original)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let original = strip_epub_extension(&original);

    let name = match options.format {
        FilenameFormat::Original => original.to_string(),
        FilenameFormat::TitleAuthor => join_parts(title, author),
        FilenameFormat::AuthorTitle => join_parts(author, title),
        FilenameFormat::Title => title.to_string(),
        FilenameFormat::Custom => render_template(
            &options.template,
            &[
                ("title", title),
                ("author", author),
                ("year", metadata.year.trim()),
                ("series", metadata.series.trim()),
                ("series_index", metadata.series_index.trim()),
                ("language", metadata.language.trim()),
                ("original", original),
            ],
        )?,
    };

    Ok(finish_filename(&name))
}

/// Check a custom template, returning the name it gives an example book, or
/// what is wrong with it.
pub fn check_template(template: &str) -> Result<String> {
    let example = Metadata {
        title: "The Long Afternoon".into(),
        author: "Marguerite Vale".into(),
        year: "2026".into(),
        series: "Afternoons".into(),
        series_index: "2".into(),
        language: "en".into(),
        ..Metadata::default()
    };
    let options = FilenameOptions {
        format: FilenameFormat::Custom,
        template: template.to_string(),
    };
    output_filename(&example, &options, "long-afternoon.epub")
}

/// `preferred`, or if something is already there, the first free
/// `name (2).epub`, `name (3).epub`, … beside it.
///
/// A name made from a book's metadata can be the name of a file that already
/// exists — the book itself, when it keeps its original name — and a finished
/// book should never replace it. What is free now may not be by the time a
/// book is moved there, so [`publish`] does not trust this answer: it claims
/// the name in the same step as the move.
pub fn unused_path(preferred: &Path) -> PathBuf {
    candidates(preferred)
        .find(|candidate| !taken(candidate))
        .expect("an unused suffix always exists")
}

/// A new file in `directory` to write a book into before it has a name.
///
/// The name of a finished book comes from its metadata, which is not known
/// until the run is over. Writing in the folder the book is going to lets
/// [`publish`] move it there rather than copy it, wherever the filesystem
/// allows.
///
/// The file is made for this run alone — never one that was already there, nor
/// a link to one — and is deleted when it is dropped unpublished, so a run
/// that fails leaves nothing behind and takes nothing with it.
pub fn staging_file(directory: &Path) -> Result<NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(".epubkit-").suffix(".part");

    // Temporary files are private by default; this one becomes a book, which
    // should be as readable as any other file its owner makes.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }

    builder
        .tempfile_in(directory)
        .map_err(|e| Error::io(directory, e))
}

/// Move a finished book from its [`staging_file`] to `preferred`, or, if
/// something is already there, to the first free `name (2).epub`,
/// `name (3).epub`, … beside it. Returns where it went.
///
/// Each name is claimed in the same step as the book goes there, so a file
/// there already, or one that appears in the meantime — another run's book,
/// or anything else — is never replaced: the book moves on to the next name.
pub fn publish(mut staged: NamedTempFile, preferred: &Path) -> Result<PathBuf> {
    for candidate in candidates(preferred) {
        let claimed = match staged.persist_noclobber(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(refused) => {
                staged = refused.file;
                if refused.error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                // Some filesystems can neither rename a file without replacing
                // another nor link one. Any can make a file only if no file is
                // there, and the book is copied into that instead.
                copy_into_new_file(&mut staged, &candidate)
            }
        };

        match claimed {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(Error::io(&candidate, error)),
        }
    }

    unreachable!("an unused suffix always exists")
}

/// Copy `staged` into a new file at `path`, made only if nothing is there. A
/// copy that fails takes its file with it: the file is this run's own.
fn copy_into_new_file(staged: &mut NamedTempFile, path: &Path) -> io::Result<()> {
    let mut out = OpenOptions::new().write(true).create_new(true).open(path)?;

    let source = staged.as_file_mut();
    let copied = source
        .seek(SeekFrom::Start(0))
        .and_then(|_| io::copy(source, &mut out))
        .and_then(|_| out.sync_all());

    if copied.is_err() {
        drop(out);
        let _ = fs::remove_file(path);
    }
    copied
}

// ---------------------------------------------------------------- internals

/// `preferred`, then `name (2).epub`, `name (3).epub`, … beside it.
fn candidates(preferred: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    let stem = preferred
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "optimized".to_string());
    let parent = preferred.parent().unwrap_or(Path::new(""));

    std::iter::once(preferred.to_path_buf())
        .chain((2..).map(move |n| parent.join(format!("{stem} ({n}).epub"))))
}

/// Whether anything is at `path`. A dangling symlink counts: writing to it
/// would write wherever it points.
fn taken(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

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

/// The fields that are present, joined with a dash.
fn join_parts(first: &str, second: &str) -> String {
    [first, second]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" - ")
}

fn strip_epub_extension(name: &str) -> &str {
    match name.len().checked_sub(".epub".len()) {
        Some(at) if name.is_char_boundary(at) && name[at..].eq_ignore_ascii_case(".epub") => {
            &name[..at]
        }
        _ => name,
    }
}

/// Make a name a safe filename with an `.epub` extension.
fn finish_filename(name: &str) -> String {
    let mut name = sanitize_filename(strip_epub_extension(name.trim()));

    // Truncate by characters, not bytes — the latter would split a multi-byte
    // codepoint and panic.
    if name.chars().count() > MAX_FILENAME_CHARS {
        name = name.chars().take(MAX_FILENAME_CHARS).collect();
        name = name.trim_end_matches([' ', '.', '-']).to_string();
    }

    // Nothing left, or nothing but dots, names no file.
    if name.is_empty() {
        name = "optimized".to_string();
    }

    format!("{name}.epub")
}

/// Fill in a custom template. `{field}` is replaced by its value, and `{{` and
/// `}}` stand for literal braces; nothing else in braces is allowed.
fn render_template(template: &str, fields: &[(&str, &str)]) -> Result<String> {
    let invalid = |message: String| Err(Error::FilenameTemplate(message));

    if template.trim().is_empty() {
        return invalid("Custom filename template cannot be empty".into());
    }
    if template.chars().count() > MAX_TEMPLATE_CHARS {
        return invalid(format!(
            "Filename template is longer than {MAX_TEMPLATE_CHARS} characters"
        ));
    }

    let mut out = String::new();
    let mut unknown = BTreeSet::new();
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut field = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('{') | None => {
                            return invalid(
                                "A '{' in the filename template is never closed".into(),
                            );
                        }
                        Some(c) => field.push(c),
                    }
                }
                if field.contains([':', '!']) {
                    return invalid(
                        "Filename template fields do not support formatting options".into(),
                    );
                }
                match fields.iter().find(|(name, _)| *name == field) {
                    Some((_, value)) => out.push_str(value),
                    None => {
                        unknown.insert(format!("{{{field}}}"));
                    }
                }
            }
            '}' => {
                return invalid(
                    "A '}' in the filename template has no '{'; write '}}' for a literal one"
                        .into(),
                );
            }
            c => out.push(c),
        }
    }

    if !unknown.is_empty() {
        let names: Vec<String> = unknown.into_iter().collect();
        return invalid(format!(
            "Unknown filename template field: {}",
            names.join(", ")
        ));
    }

    Ok(out)
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

    // A name ending in a dot or a dash looks broken, and Windows drops a
    // trailing dot altogether; leading ones hide the file or read as flags.
    collapsed.trim_matches([' ', '.', '-']).to_string()
}
