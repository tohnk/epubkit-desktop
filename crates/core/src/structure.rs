//! OPF manifest and spine handling: content-file classification, reference
//! rewriting after images are renamed, SVG cover unwrapping, and table of
//! contents validation and regeneration. Port of `epub_structure.py`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use libxml::tree::{Document, Node};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};

use crate::html;
use crate::xml;
use crate::{Error, Result};

pub const NS_OPF: &str = "http://www.idpf.org/2007/opf";
pub const NS_NCX: &str = "http://www.daisy.org/z3986/2005/ncx/";
pub const NS_XLINK: &str = "http://www.w3.org/1999/xlink";

const NCX_MEDIA_TYPE: &str = "application/x-dtbncx+xml";

/// Media types the OPF may use for embedded fonts.
const FONT_MEDIA_TYPES: &[&str] = &[
    "application/font-woff",
    "application/font-woff2",
    "font/woff",
    "font/woff2",
    "font/ttf",
    "font/otf",
    "application/vnd.ms-opentype",
    "application/x-font-ttf",
];

const FONT_EXTENSIONS: &[&str] = &["ttf", "otf", "woff", "woff2"];

/// Characters escaped when writing an href back into the OPF. `/`, `:` and `@`
/// stay literal, matching the reference implementation's `quote(safe='/:@')`.
const HREF_ESCAPE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'#')
    .add(b'?')
    .add(b'{')
    .add(b'}')
    .add(b'%');

/// One `<item>` from the OPF manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestItem {
    pub id: String,
    /// As written in the OPF, so possibly percent-encoded.
    pub href: String,
    pub media_type: String,
    pub properties: String,
}

impl ManifestItem {
    /// The href with percent-escapes resolved, for comparing against paths.
    pub fn decoded_href(&self) -> String {
        decode(&self.href)
    }
}

/// Manifest files grouped by what the pipeline does with them. Paths are
/// absolute, resolved against the OPF's directory.
#[derive(Debug, Clone, Default)]
pub struct ContentFiles {
    pub xhtml: Vec<PathBuf>,
    pub css: Vec<PathBuf>,
    pub images: Vec<PathBuf>,
    pub fonts: Vec<PathBuf>,
    pub ncx: Vec<PathBuf>,
    pub other: Vec<PathBuf>,
}

/// What `fix_toc` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TocOutcome {
    /// An existing NCX checked out; nothing changed.
    Valid,
    /// An NCX was written, with this many entries.
    Generated(usize),
    /// Nothing could be done, for the stated reason.
    Skipped(String),
}

impl TocOutcome {
    /// A short description, for the processing report.
    pub fn describe(&self) -> String {
        match self {
            TocOutcome::Valid => "TOC is valid".to_string(),
            TocOutcome::Generated(n) => format!("Generated TOC with {n} entries"),
            TocOutcome::Skipped(reason) => reason.clone(),
        }
    }

    pub fn changed(&self) -> bool {
        matches!(self, TocOutcome::Generated(_))
    }
}

/// Read the OPF manifest.
pub fn manifest_items(doc: &Document) -> Result<Vec<ManifestItem>> {
    let nodes = xml::find_nodes(
        doc,
        &format!("//{}/{}", xml::local("manifest"), xml::local("item")),
    )?;

    Ok(nodes.iter().map(item_from_node).collect())
}

/// Read the spine's `idref`s, in reading order.
pub fn spine_idrefs(doc: &Document) -> Result<Vec<String>> {
    let nodes = xml::find_nodes(
        doc,
        &format!("//{}/{}", xml::local("spine"), xml::local("itemref")),
    )?;

    Ok(nodes
        .iter()
        .filter_map(|node| node.get_attribute("idref"))
        .filter(|idref| !idref.is_empty())
        .collect())
}

/// Spine entries paired with their manifest hrefs, skipping dangling idrefs.
pub fn spine_hrefs(doc: &Document) -> Result<Vec<(String, String)>> {
    let by_id: BTreeMap<String, String> = manifest_items(doc)?
        .into_iter()
        .map(|item| (item.id, item.href))
        .collect();

    Ok(spine_idrefs(doc)?
        .into_iter()
        .filter_map(|idref| {
            by_id
                .get(&idref)
                .filter(|href| !href.is_empty())
                .map(|href| (idref, href.clone()))
        })
        .collect())
}

/// Classify every manifest entry by what the pipeline needs to do with it.
pub fn find_content_files(opf_dir: &Path, doc: &Document) -> Result<ContentFiles> {
    let mut files = ContentFiles::default();

    for item in manifest_items(doc)? {
        let href = item.decoded_href();
        if href.is_empty() {
            continue;
        }
        let path = opf_dir.join(&href);
        let media_type = item.media_type.to_ascii_lowercase();

        match media_type.as_str() {
            "application/xhtml+xml" | "text/html" => files.xhtml.push(path),
            "text/css" => files.css.push(path),
            NCX_MEDIA_TYPE => files.ncx.push(path),
            _ if media_type.starts_with("image/") => files.images.push(path),
            _ if FONT_MEDIA_TYPES.contains(&media_type.as_str()) => files.fonts.push(path),
            // Some books mislabel or omit the media type; fall back to the
            // extension before giving up on a file.
            _ if has_font_extension(&href) => files.fonts.push(path),
            _ => files.other.push(path),
        }
    }

    Ok(files)
}

/// Map old image paths to new ones, given the filenames the image step
/// produced. Keys and values are relative to the OPF's directory.
pub fn build_rename_map(processed: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();

    for (old_path, new_filename) in processed {
        let new_path = match Path::new(old_path).parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent
                .join(new_filename)
                .to_string_lossy()
                .replace('\\', "/"),
            _ => new_filename.clone(),
        };
        if *old_path != new_path {
            map.insert(old_path.clone(), new_path);
        }
    }

    map
}

/// Point renamed images' manifest entries at their new files. Returns how many
/// entries changed.
pub fn update_opf(doc: &Document, rename_map: &BTreeMap<String, String>) -> Result<usize> {
    if rename_map.is_empty() {
        return Ok(0);
    }

    let nodes = xml::find_nodes(
        doc,
        &format!("//{}/{}", xml::local("manifest"), xml::local("item")),
    )?;

    let mut updated = 0;
    for mut node in nodes {
        let href = node.get_attribute("href").unwrap_or_default();
        let decoded = decode(&href);

        let Some(new_path) = rename_map
            .get(&decoded)
            .or_else(|| rename_map.get(&href))
            .or_else(|| match_by_filename(&decoded, rename_map))
        else {
            continue;
        };

        node.set_attribute("href", &encode(new_path)).ok();
        // Every processed image comes out of the image step as a JPEG.
        node.set_attribute("media-type", "image/jpeg").ok();
        updated += 1;
    }

    Ok(updated)
}

/// Drop font entries from the manifest. Returns how many went.
pub fn update_opf_remove_fonts(doc: &Document, font_paths: &[PathBuf]) -> Result<usize> {
    let font_names: Vec<String> = font_paths
        .iter()
        .filter_map(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .collect();

    if font_names.is_empty() {
        return Ok(0);
    }

    let nodes = xml::find_nodes(
        doc,
        &format!("//{}/{}", xml::local("manifest"), xml::local("item")),
    )?;

    let mut removed = 0;
    for mut node in nodes {
        let href = decode(&node.get_attribute("href").unwrap_or_default());
        let name = Path::new(&href)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if !name.is_empty() && font_names.contains(&name) {
            node.unlink();
            removed += 1;
        }
    }

    Ok(removed)
}

/// Append an image to the manifest — used for a generated cover.
pub fn add_image_to_opf(doc: &Document, href: &str, id: &str) -> Result<()> {
    let Some(mut manifest) = xml::find_first(doc, &format!("//{}", xml::local("manifest")))? else {
        return Err(Error::InvalidEpub("OPF has no manifest".into()));
    };

    let namespace = xml::namespace_for(doc, &mut manifest, NS_OPF)?;
    let mut item = manifest
        .new_child(namespace, "item")
        .map_err(|e| Error::Xml(format!("could not add manifest item: {e}")))?;

    item.set_attribute("id", id).ok();
    item.set_attribute("href", href).ok();
    item.set_attribute("media-type", "image/jpeg").ok();

    Ok(())
}

/// Rewrite image references inside one XHTML file: `<img src>`, SVG
/// `<image xlink:href>`, and `url()` in inline styles. Returns how many
/// references changed, writing the file only if any did.
pub fn update_xhtml_references(
    path: &Path,
    rename_map: &BTreeMap<String, String>,
) -> Result<usize> {
    if rename_map.is_empty() {
        return Ok(0);
    }

    let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
    let content = html::parse_content(&bytes)?;
    let mut updated = 0;

    for mut node in xml::find_nodes(&content.doc, "//*")? {
        match local_name(&node).as_str() {
            "img" => {
                let src = node.get_attribute("src").unwrap_or_default();
                let new_src = resolve_reference(&src, rename_map);
                if new_src != src {
                    node.set_attribute("src", &new_src).ok();
                    updated += 1;
                }
            }
            // SVG's <image> carries its target in xlink:href, or plain href in
            // SVG 2 documents.
            "image" => {
                let xlink = node.get_attribute_ns("href", NS_XLINK);
                let value = xlink
                    .clone()
                    .unwrap_or_else(|| node.get_attribute("href").unwrap_or_default());
                let new_value = resolve_reference(&value, rename_map);

                if new_value != value {
                    let namespace = xlink
                        .is_some()
                        .then(|| xlink_namespace(&content.doc, &node));
                    match namespace.flatten() {
                        Some(ns) => {
                            node.set_attribute_ns("href", &new_value, &ns).ok();
                        }
                        None => {
                            node.set_attribute("href", &new_value).ok();
                        }
                    }
                    updated += 1;
                }
            }
            _ => {}
        }

        let style = node.get_attribute("style").unwrap_or_default();
        if style.contains("url(") {
            let new_style = rewrite_css_urls(&style, rename_map);
            if new_style != style {
                node.set_attribute("style", &new_style).ok();
                updated += 1;
            }
        }
    }

    if updated > 0 {
        fs::write(path, html::serialize_content(&content)).map_err(|e| Error::io(path, e))?;
    }

    Ok(updated)
}

/// Rewrite `url()` references in a stylesheet. Returns 1 if the file changed.
pub fn update_css_references(path: &Path, rename_map: &BTreeMap<String, String>) -> Result<usize> {
    if rename_map.is_empty() {
        return Ok(0);
    }

    let css = fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    let rewritten = rewrite_css_urls(&css, rename_map);

    if rewritten == css {
        return Ok(0);
    }

    fs::write(path, rewritten).map_err(|e| Error::io(path, e))?;
    Ok(1)
}

/// Replace SVG-wrapped cover images with a plain `<img>`.
///
/// Store and Gutenberg EPUBs often wrap the cover in an SVG with a viewBox,
/// which small e-ink readers render poorly or not at all. Only the first few
/// spine entries are examined — a cover later than that is not a cover.
pub fn fix_svg_covers(opf_dir: &Path, doc: &Document) -> Result<usize> {
    const SPINE_ENTRIES_TO_CHECK: usize = 3;

    let mut fixed = 0;

    for (_, href) in spine_hrefs(doc)?.into_iter().take(SPINE_ENTRIES_TO_CHECK) {
        let path = opf_dir.join(decode(&href));
        if !path.is_file() {
            continue;
        }

        let Ok(bytes) = fs::read(&path) else { continue };
        let Ok(content) = html::parse_content(&bytes) else {
            continue;
        };

        let mut fixed_here = 0;
        for mut svg in xml::find_nodes(&content.doc, &format!("//{}", xml::local("svg")))? {
            let images =
                xml::find_nodes_under(&content.doc, &svg, &format!("./{}", xml::local("image")))?;

            // A wrapper holds exactly one image. More than that is a real
            // illustration and must be left alone.
            if images.len() != 1 {
                continue;
            }

            let image = &images[0];
            let target = image
                .get_attribute_ns("href", NS_XLINK)
                .or_else(|| image.get_attribute("href"))
                .unwrap_or_default();
            if target.is_empty() {
                continue;
            }

            let Some(mut parent) = svg.get_parent() else {
                continue;
            };

            // Inherit the parent's namespace so the replacement stays in the
            // XHTML namespace rather than falling out of it.
            let namespace = parent.get_namespace();
            let Ok(mut img) = parent.new_child(namespace, "img") else {
                continue;
            };
            img.set_attribute("src", &target).ok();
            img.set_attribute("alt", "Cover").ok();
            img.set_attribute(
                "style",
                "max-width:100%;max-height:100%;display:block;margin:auto",
            )
            .ok();

            // `new_child` appends; move it into the SVG's position.
            svg.add_prev_sibling(&mut img).ok();
            svg.unlink();
            fixed_here += 1;
        }

        if fixed_here > 0 {
            fs::write(&path, html::serialize_content(&content)).map_err(|e| Error::io(&path, e))?;
            fixed += fixed_here;
        }
    }

    Ok(fixed)
}

/// Validate the table of contents, regenerating it from the spine when it is
/// missing, empty, or pointing at files that do not exist.
///
/// The reference implementation reported "Fixed N broken TOC references" while
/// its fix-up function was an empty stub, so a book with a broken TOC kept it.
/// Here a broken TOC is regenerated, which is what that comment intended.
pub fn fix_toc(opf_dir: &Path, doc: &Document) -> Result<TocOutcome> {
    let spine = spine_hrefs(doc)?;
    if spine.is_empty() {
        return Ok(TocOutcome::Skipped("Empty spine".into()));
    }

    let existing_ncx = manifest_items(doc)?
        .into_iter()
        .find(|item| item.media_type == NCX_MEDIA_TYPE);

    if let Some(item) = &existing_ncx {
        let ncx_path = opf_dir.join(item.decoded_href());
        if ncx_is_usable(&ncx_path)? {
            return Ok(TocOutcome::Valid);
        }
    }

    let chapters = extract_chapters(opf_dir, &spine);
    let ncx_href = existing_ncx
        .as_ref()
        .map(|item| item.decoded_href())
        .unwrap_or_else(|| "toc.ncx".to_string());

    write_ncx(&opf_dir.join(&ncx_href), &chapters)?;

    // A newly created NCX has to be declared, and pointed at from the spine.
    if existing_ncx.is_none() {
        add_ncx_to_opf(doc, &ncx_href)?;
    }

    Ok(TocOutcome::Generated(chapters.len()))
}

/// One entry in the generated table of contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chapter {
    pub title: String,
    pub href: String,
}

// ---------------------------------------------------------------- internals

fn item_from_node(node: &Node) -> ManifestItem {
    ManifestItem {
        id: node.get_attribute("id").unwrap_or_default(),
        href: node.get_attribute("href").unwrap_or_default(),
        media_type: node.get_attribute("media-type").unwrap_or_default(),
        properties: node.get_attribute("properties").unwrap_or_default(),
    }
}

/// The xlink namespace as declared in scope at `node`, if it is.
fn xlink_namespace(doc: &Document, node: &Node) -> Option<libxml::tree::Namespace> {
    node.get_namespaces(doc)
        .into_iter()
        .find(|ns| ns.get_href() == NS_XLINK)
}

fn local_name(node: &Node) -> String {
    node.get_name()
        .rsplit(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn decode(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().to_string()
}

fn encode(value: &str) -> String {
    utf8_percent_encode(value, HREF_ESCAPE).to_string()
}

fn has_font_extension(href: &str) -> bool {
    Path::new(href)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| FONT_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

fn file_name_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Fall back to matching on filename alone, for references written relative to
/// a different directory than the manifest entry.
fn match_by_filename<'a>(
    href: &str,
    rename_map: &'a BTreeMap<String, String>,
) -> Option<&'a String> {
    let name = file_name_of(href);
    if name.is_empty() {
        return None;
    }
    rename_map
        .iter()
        .find(|(old, _)| file_name_of(old) == name)
        .map(|(_, new)| new)
}

/// Rewrite a single reference, preserving the directory prefix it was written
/// with and only swapping the filename.
fn resolve_reference(reference: &str, rename_map: &BTreeMap<String, String>) -> String {
    if reference.is_empty() {
        return reference.to_string();
    }

    let decoded = decode(reference);
    let name = file_name_of(&decoded);

    for (old_path, new_path) in rename_map {
        if name == file_name_of(old_path) {
            return decoded.replace(&name, &file_name_of(new_path));
        }
    }

    reference.to_string()
}

/// Rewrite `url(...)` targets in CSS text.
fn rewrite_css_urls(css: &str, rename_map: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;

    while let Some(start) = rest.find("url(") {
        let (before, from_url) = rest.split_at(start);
        out.push_str(before);

        let Some(end) = from_url.find(')') else {
            // Unterminated url( — leave the remainder untouched.
            out.push_str(from_url);
            return out;
        };

        let inner = &from_url["url(".len()..end];
        let trimmed = inner.trim().trim_matches(['\'', '"']);
        let rewritten = resolve_reference(trimmed, rename_map);

        out.push_str("url(");
        out.push_str(&rewritten);
        out.push(')');

        rest = &from_url[end + 1..];
    }

    out.push_str(rest);
    out
}

/// An NCX counts as usable when it parses, declares at least one navPoint, and
/// every target it names exists on disk.
fn ncx_is_usable(ncx_path: &Path) -> Result<bool> {
    if !ncx_path.is_file() {
        return Ok(false);
    }

    let Ok(doc) = xml::parse_file(ncx_path) else {
        return Ok(false);
    };

    let nav_points = xml::find_nodes(
        &doc,
        &format!("//{}//{}", xml::local("navMap"), xml::local("navPoint")),
    )?;
    if nav_points.is_empty() {
        return Ok(false);
    }

    let ncx_dir = ncx_path.parent().unwrap_or(Path::new("."));

    for nav_point in &nav_points {
        let contents =
            xml::find_nodes_under(&doc, nav_point, &format!("./{}", xml::local("content")))?;
        for content in contents {
            let src = content.get_attribute("src").unwrap_or_default();
            // Strip any fragment; the file is what has to exist.
            let file = src.split('#').next().unwrap_or_default();
            if file.is_empty() {
                continue;
            }
            if !ncx_dir.join(decode(file)).exists() {
                return Ok(false);
            }
        }
    }

    Ok(true)
}

/// Derive chapter titles from the spine, preferring `<title>` and falling back
/// to the first heading, then to a positional name.
fn extract_chapters(opf_dir: &Path, spine: &[(String, String)]) -> Vec<Chapter> {
    spine
        .iter()
        .enumerate()
        .map(|(index, (_, href))| Chapter {
            title: chapter_title(&opf_dir.join(decode(href)))
                .unwrap_or_else(|| format!("Chapter {}", index + 1)),
            href: href.clone(),
        })
        .collect()
}

fn chapter_title(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let content = html::parse_content(&bytes).ok()?;

    for xpath in [
        format!("//{}", xml::local("title")),
        format!("//{}", xml::local("h1")),
        format!("//{}", xml::local("h2")),
        format!("//{}", xml::local("h3")),
    ] {
        if let Ok(Some(node)) = xml::find_first(&content.doc, &xpath) {
            let text = node.get_content().trim().to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }

    None
}

fn write_ncx(ncx_path: &Path, chapters: &[Chapter]) -> Result<()> {
    let mut ncx = String::new();
    ncx.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    ncx.push_str(&format!(
        "<ncx xmlns=\"{NS_NCX}\" version=\"2005-1\">\n  <head>\n    <meta name=\"dtb:depth\" content=\"1\"/>\n  </head>\n"
    ));

    let doc_title = chapters
        .first()
        .map(|c| c.title.as_str())
        .unwrap_or("Unknown");
    ncx.push_str(&format!(
        "  <docTitle>\n    <text>{}</text>\n  </docTitle>\n  <navMap>\n",
        escape_xml_text(doc_title)
    ));

    for (index, chapter) in chapters.iter().enumerate() {
        let order = index + 1;
        ncx.push_str(&format!(
            "    <navPoint id=\"navPoint-{order}\" playOrder=\"{order}\">\n      <navLabel>\n        <text>{}</text>\n      </navLabel>\n      <content src=\"{}\"/>\n    </navPoint>\n",
            escape_xml_text(&chapter.title),
            escape_xml_attribute(&chapter.href),
        ));
    }

    ncx.push_str("  </navMap>\n</ncx>\n");

    if let Some(parent) = ncx_path.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    fs::write(ncx_path, ncx).map_err(|e| Error::io(ncx_path, e))
}

fn add_ncx_to_opf(doc: &Document, ncx_href: &str) -> Result<()> {
    let Some(mut manifest) = xml::find_first(doc, &format!("//{}", xml::local("manifest")))? else {
        return Err(Error::InvalidEpub("OPF has no manifest".into()));
    };

    let namespace = xml::namespace_for(doc, &mut manifest, NS_OPF)?;
    let mut item = manifest
        .new_child(namespace, "item")
        .map_err(|e| Error::Xml(format!("could not add NCX to manifest: {e}")))?;
    item.set_attribute("id", "ncx").ok();
    item.set_attribute("href", ncx_href).ok();
    item.set_attribute("media-type", NCX_MEDIA_TYPE).ok();

    // EPUB 2 readers find the NCX through the spine's toc attribute.
    if let Some(mut spine) = xml::find_first(doc, &format!("//{}", xml::local("spine")))? {
        spine.set_attribute("toc", "ncx").ok();
    }

    Ok(())
}

fn escape_xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_xml_attribute(text: &str) -> String {
    escape_xml_text(text).replace('"', "&quot;")
}
