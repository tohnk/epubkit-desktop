//! OPF manifest and spine handling: content-file classification, reference
//! rewriting after images are renamed, SVG cover unwrapping, and table of
//! contents validation and regeneration. Port of `epub_structure.py`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

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

/// How an image that replaces an SVG wrapper fills the page.
const FULL_PAGE_STYLE: &str = "max-width:100%;max-height:100%;display:block;margin:auto";

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

/// Where `href`, decoded, leads from `base`, a directory inside the book's
/// `root` — or `None` if it leads out of the book.
///
/// The book is untrusted, and its hrefs become paths the pipeline reads,
/// rewrites and deletes. Joined as they stand, an absolute href would replace
/// the base and `..` would climb out of it, handing the pipeline files that
/// are not the book's. A leading `/` starts from the book's root instead, as
/// a URL inside an EPUB container does.
pub fn resolve_href(root: &Path, base: &Path, href: &str) -> Option<PathBuf> {
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    let from_base = base.strip_prefix(root).ok()?;

    for component in from_base.components().chain(Path::new(href).components()) {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            Component::RootDir => parts.clear(),
            Component::ParentDir => {
                parts.pop()?;
            }
            // A drive or share names another filesystem altogether.
            Component::Prefix(_) => return None,
        }
    }

    Some(
        parts
            .iter()
            .fold(root.to_path_buf(), |path, part| path.join(part)),
    )
}

/// Classify every manifest entry by what the pipeline needs to do with it.
/// `root` is where the book was unpacked; an entry that leads out of it is
/// left out.
pub fn find_content_files(root: &Path, opf_dir: &Path, doc: &Document) -> Result<ContentFiles> {
    let mut files = ContentFiles::default();

    for item in manifest_items(doc)? {
        let href = item.decoded_href();
        if href.is_empty() {
            continue;
        }
        let Some(path) = resolve_href(root, opf_dir, &href) else {
            continue;
        };
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
            .or_else(|| rename_by_filename(&decoded, rename_map))
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
///
/// A reference is resolved against the file's own directory and matched by
/// path, so two images that share a filename in different directories can be
/// renamed differently. `opf_dir` is what the rename map's paths are relative
/// to.
pub fn update_xhtml_references(
    opf_dir: &Path,
    path: &Path,
    rename_map: &BTreeMap<String, String>,
) -> Result<usize> {
    if rename_map.is_empty() {
        return Ok(0);
    }

    let renames = Renames::new(opf_dir, rename_map);
    let base = path.parent().unwrap_or(opf_dir);

    let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
    let content = html::parse_content(&bytes)?;
    let mut updated = 0;

    for mut node in xml::find_nodes(&content.doc, "//*")? {
        match local_name(&node).as_str() {
            "img" => {
                let src = node.get_attribute("src").unwrap_or_default();
                if let Some(new_src) = rewrite_reference(&src, base, &renames) {
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

                if let Some(new_value) = rewrite_reference(&value, base, &renames) {
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
            let new_style = rewrite_css_urls(&style, base, &renames);
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

/// Rewrite `url()` references in a stylesheet, resolved against its own
/// directory. Returns 1 if the file changed.
pub fn update_css_references(
    opf_dir: &Path,
    path: &Path,
    rename_map: &BTreeMap<String, String>,
) -> Result<usize> {
    if rename_map.is_empty() {
        return Ok(0);
    }

    let renames = Renames::new(opf_dir, rename_map);
    let base = path.parent().unwrap_or(opf_dir);

    let css = crate::css::read_stylesheet(path)?;
    let rewritten = rewrite_css_urls(&css, base, &renames);

    if rewritten == css {
        return Ok(0);
    }

    fs::write(path, rewritten).map_err(|e| Error::io(path, e))?;
    Ok(1)
}

/// Declare every page of each image Light Novel mode split, after the first,
/// which already has the image's own manifest entry. `reshaped` maps the path
/// of an image's first page to all its pages in reading order, relative to
/// the OPF's directory. Returns how many entries were added.
pub fn declare_reshaped_pages(
    doc: &Document,
    reshaped: &BTreeMap<String, Vec<String>>,
) -> Result<usize> {
    let items = manifest_items(doc)?;
    let mut ids: HashSet<String> = items.iter().map(|item| item.id.clone()).collect();
    let mut added = 0;

    for (first, pages) in reshaped {
        let base = items
            .iter()
            .find(|item| item.decoded_href() == *first)
            .map_or_else(|| "image".to_string(), |item| item.id.clone());

        for (index, page) in pages.iter().enumerate().skip(1) {
            let wanted = format!("{base}-{}", index + 1);
            let id = std::iter::once(wanted.clone())
                .chain((2..).map(|n| format!("{wanted}-{n}")))
                .find(|candidate| !ids.contains(candidate))
                .expect("an unused suffix always exists");

            add_image_to_opf(doc, &encode(page), &id)?;
            ids.insert(id);
            added += 1;
        }
    }

    Ok(added)
}

/// Show every page of each image Light Novel mode reshaped in one XHTML file.
/// `reshaped` is as for [`declare_reshaped_pages`]; references should already
/// point at the first page.
///
/// An `<img>` of a reshaped image loses its `width` and `height`, which give
/// the old shape, and is followed by a copy for each further page. An SVG
/// wrapper around one, its viewBox sized to the old shape too, gives way to a
/// plain `<img>` per page. Returns how many images changed, writing the file
/// only if any did.
pub fn show_reshaped_pages(
    opf_dir: &Path,
    path: &Path,
    reshaped: &BTreeMap<String, Vec<String>>,
) -> Result<usize> {
    if reshaped.is_empty() {
        return Ok(0);
    }

    let pages_by_target: HashMap<PathBuf, Vec<String>> = reshaped
        .iter()
        .map(|(first, pages)| {
            let names = pages.iter().map(|page| file_name_of(page)).collect();
            (normalize_path(&opf_dir.join(first)), names)
        })
        .collect();
    let base = path.parent().unwrap_or(opf_dir);

    let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
    let content = html::parse_content(&bytes)?;
    let mut changed = 0;

    // Both lists are taken before anything changes, so the images added below
    // are not visited in turn.
    let svgs = xml::find_nodes(&content.doc, &format!("//{}", xml::local("svg")))?;
    let images = xml::find_nodes(&content.doc, &format!("//{}", xml::local("img")))?;

    for mut svg in svgs {
        let inner =
            xml::find_nodes_under(&content.doc, &svg, &format!("./{}", xml::local("image")))?;
        let [image] = inner.as_slice() else {
            continue;
        };
        let href = image
            .get_attribute_ns("href", NS_XLINK)
            .or_else(|| image.get_attribute("href"))
            .unwrap_or_default();
        let Some(reference) = Reference::parse(&href) else {
            continue;
        };
        let Some(pages) = pages_by_target.get(&reference.target(base)) else {
            continue;
        };
        let Some(mut parent) = svg.get_parent() else {
            continue;
        };

        let namespace = parent.get_namespace();
        for page in pages {
            let Ok(mut img) = parent.new_child(namespace.clone(), "img") else {
                continue;
            };
            img.set_attribute("src", &reference.with_name(page)).ok();
            img.set_attribute("alt", "").ok();
            img.set_attribute("style", FULL_PAGE_STYLE).ok();
            svg.add_prev_sibling(&mut img).ok();
        }
        svg.unlink();
        changed += 1;
    }

    for mut image in images {
        let src = image.get_attribute("src").unwrap_or_default();
        let Some(reference) = Reference::parse(&src) else {
            continue;
        };
        let Some(pages) = pages_by_target.get(&reference.target(base)) else {
            continue;
        };
        let Some(mut parent) = image.get_parent() else {
            continue;
        };

        image.remove_attribute("width").ok();
        image.remove_attribute("height").ok();

        // Each further page is shown the way the first is: same class, style
        // and alt text, but no id, which must stay unique.
        let attributes = image.get_attributes_ns();
        let mut previous = image.clone();
        for page in &pages[1..] {
            let Ok(mut copy) = parent.new_child(image.get_namespace(), "img") else {
                continue;
            };
            for ((name, namespace), value) in &attributes {
                match namespace {
                    Some(namespace) => copy.set_attribute_ns(name, value, namespace).ok(),
                    None if name != "id" => copy.set_attribute(name, value).ok(),
                    None => None,
                };
            }
            copy.set_attribute("src", &reference.with_name(page)).ok();
            previous.add_next_sibling(&mut copy).ok();
            previous = copy;
        }
        changed += 1;
    }

    if changed > 0 {
        fs::write(path, html::serialize_content(&content)).map_err(|e| Error::io(path, e))?;
    }

    Ok(changed)
}

/// Replace SVG-wrapped cover images with a plain `<img>`.
///
/// Store and Gutenberg EPUBs often wrap the cover in an SVG with a viewBox,
/// which small e-ink readers render poorly or not at all. Only the first few
/// spine entries are examined — a cover later than that is not a cover.
pub fn fix_svg_covers(root: &Path, opf_dir: &Path, doc: &Document) -> Result<usize> {
    const SPINE_ENTRIES_TO_CHECK: usize = 3;

    let mut fixed = 0;

    for (_, href) in spine_hrefs(doc)?.into_iter().take(SPINE_ENTRIES_TO_CHECK) {
        let Some(path) = resolve_href(root, opf_dir, &decode(&href)) else {
            continue;
        };
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
            img.set_attribute("style", FULL_PAGE_STYLE).ok();

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
pub fn fix_toc(root: &Path, opf_dir: &Path, doc: &Document) -> Result<TocOutcome> {
    let spine = spine_hrefs(doc)?;
    if spine.is_empty() {
        return Ok(TocOutcome::Skipped("Empty spine".into()));
    }

    let existing_ncx = manifest_items(doc)?
        .into_iter()
        .find(|item| item.media_type == NCX_MEDIA_TYPE);

    let ncx_href = existing_ncx
        .as_ref()
        .map(|item| item.decoded_href())
        .unwrap_or_else(|| "toc.ncx".to_string());
    let Some(ncx_path) = resolve_href(root, opf_dir, &ncx_href) else {
        return Ok(TocOutcome::Skipped(
            "TOC left alone: it lies outside the book".into(),
        ));
    };

    if existing_ncx.is_some() && ncx_is_usable(root, &ncx_path)? {
        return Ok(TocOutcome::Valid);
    }

    let ncx_dir = ncx_path.parent().unwrap_or(root);
    let chapters = extract_chapters(root, opf_dir, ncx_dir, &spine);
    if chapters.is_empty() {
        return Ok(TocOutcome::Skipped(
            "TOC left alone: no chapter lies inside the book".into(),
        ));
    }
    write_ncx(&ncx_path, &chapters)?;

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

/// Resolve `.` and `..` without touching the filesystem, so that two spellings
/// of one path compare equal.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normal.components().next_back() {
                Some(Component::Normal(_)) => {
                    normal.pop();
                }
                // Nothing lies above the root.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => normal.push(".."),
            },
            other => normal.push(other),
        }
    }
    normal
}

/// Find a rename by filename alone, for a path that leads nowhere — one
/// written relative to the wrong directory, say. Only an unambiguous answer
/// counts: images sharing a filename in different directories can be renamed
/// differently, and then the filename cannot say which was meant.
fn rename_by_filename<'a>(
    path: &str,
    rename_map: &'a BTreeMap<String, String>,
) -> Option<&'a String> {
    let name = file_name_of(path);
    if name.is_empty() {
        return None;
    }

    let mut candidates = rename_map
        .iter()
        .filter(|(old, _)| file_name_of(old) == name)
        .map(|(_, new)| new);
    let first = candidates.next()?;
    let new_name = file_name_of(first);

    candidates
        .all(|other| file_name_of(other) == new_name)
        .then_some(first)
}

/// The rename map, indexed by the file each entry was renamed from.
struct Renames<'a> {
    rename_map: &'a BTreeMap<String, String>,
    by_source: HashMap<PathBuf, &'a String>,
}

impl<'a> Renames<'a> {
    fn new(opf_dir: &Path, rename_map: &'a BTreeMap<String, String>) -> Self {
        let by_source = rename_map
            .iter()
            .map(|(old, new)| (normalize_path(&opf_dir.join(old)), new))
            .collect();
        Self {
            rename_map,
            by_source,
        }
    }

    /// The new filename of the file `path` leads to from `base`, if that file
    /// was renamed.
    fn new_name(&self, base: &Path, path: &str) -> Option<String> {
        let target = normalize_path(&base.join(path));
        if let Some(new) = self.by_source.get(&target) {
            return Some(file_name_of(new));
        }

        // A path to a file that is still there names something that was not
        // renamed, whatever its filename shares with something that was.
        if target.is_file() {
            return None;
        }

        rename_by_filename(path, self.rename_map).map(|new| file_name_of(new))
    }
}

/// A reference to a file in the book, taken apart so that its filename can be
/// swapped while everything else stays as written.
struct Reference<'a> {
    directory: &'a str,
    name: &'a str,
    /// Any query or fragment.
    suffix: &'a str,
}

impl<'a> Reference<'a> {
    /// `None` for what is not a file in the book: a same-document fragment,
    /// or anything with a scheme, such as http: or data:.
    fn parse(reference: &'a str) -> Option<Self> {
        if reference.starts_with('#') || has_scheme(reference) {
            return None;
        }

        let (path, suffix) =
            reference.split_at(reference.find(['?', '#']).unwrap_or(reference.len()));
        let (directory, name) = path.split_at(path.rfind('/').map_or(0, |slash| slash + 1));

        (!name.is_empty()).then_some(Self {
            directory,
            name,
            suffix,
        })
    }

    /// The path, percent-decoded.
    fn path(&self) -> String {
        decode(&format!("{}{}", self.directory, self.name))
    }

    /// The file it names, from a document in `base`.
    fn target(&self, base: &Path) -> PathBuf {
        normalize_path(&base.join(self.path()))
    }

    /// The same reference naming `new_name`, percent-encoded only if the
    /// original name was.
    fn with_name(&self, new_name: &str) -> String {
        let new_name = if decode(self.name) == self.name {
            new_name.to_string()
        } else {
            encode(new_name)
        };
        format!("{}{new_name}{}", self.directory, self.suffix)
    }
}

/// Rewrite one reference if the file it names was renamed. `None` leaves it
/// alone.
fn rewrite_reference(reference: &str, base: &Path, renames: &Renames) -> Option<String> {
    let reference = Reference::parse(reference)?;
    let new_name = renames.new_name(base, &reference.path())?;
    Some(reference.with_name(&new_name))
}

/// Does `reference` open with a URL scheme rather than a path?
fn has_scheme(reference: &str) -> bool {
    reference.split_once(':').is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// Rewrite the `url(...)` targets in CSS text that name a renamed file. Every
/// other byte, quotes included, stays as written.
fn rewrite_css_urls(css: &str, base: &Path, renames: &Renames) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;

    while let Some(start) = rest.find("url(") {
        let open = start + "url(".len();
        let Some(close) = rest[open..].find(')').map(|end| open + end) else {
            // Unterminated url( — leave the remainder untouched.
            break;
        };
        out.push_str(&rest[..open]);

        let inner = &rest[open..close];
        let trimmed = inner.trim();
        let quote = trimmed.chars().next().filter(|c| *c == '"' || *c == '\'');
        let target = quote.map_or(trimmed, |q| trimmed.trim_matches(q));

        match (rewrite_reference(target, base, renames), quote) {
            (Some(new), Some(q)) => {
                out.push(q);
                out.push_str(&new);
                out.push(q);
            }
            (Some(new), None) => out.push_str(&new),
            (None, _) => out.push_str(inner),
        }

        rest = &rest[close..];
    }

    out.push_str(rest);
    out
}

/// An NCX counts as usable when it parses, declares at least one navPoint, and
/// every target it names exists on disk.
fn ncx_is_usable(root: &Path, ncx_path: &Path) -> Result<bool> {
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
            if !resolve_href(root, ncx_dir, &decode(file)).is_some_and(|path| path.exists()) {
                return Ok(false);
            }
        }
    }

    Ok(true)
}

/// Derive chapter titles from the spine, preferring `<title>` and falling back
/// to the first heading, then to a positional name.
///
/// The spine's hrefs are relative to the OPF, in `opf_dir`; each chapter's is
/// rewritten relative to `ncx_dir`, where the NCX naming it goes. A chapter
/// outside the book is left out.
fn extract_chapters(
    root: &Path,
    opf_dir: &Path,
    ncx_dir: &Path,
    spine: &[(String, String)],
) -> Vec<Chapter> {
    spine
        .iter()
        .enumerate()
        .filter_map(|(index, (_, href))| {
            let (file, fragment) = match href.split_once('#') {
                Some((file, fragment)) => (file, Some(fragment)),
                None => (href.as_str(), None),
            };
            let path = resolve_href(root, opf_dir, &decode(file))?;

            let mut href = relative_href(root, ncx_dir, &path)?;
            if let Some(fragment) = fragment {
                href.push('#');
                href.push_str(fragment);
            }

            Some(Chapter {
                title: chapter_title(&path).unwrap_or_else(|| format!("Chapter {}", index + 1)),
                href,
            })
        })
        .collect()
}

/// The href that leads from the directory `from` to `to`, both resolved inside
/// the book's `root`.
fn relative_href(root: &Path, from: &Path, to: &Path) -> Option<String> {
    let from: Vec<Component> = from.strip_prefix(root).ok()?.components().collect();
    let to: Vec<Component> = to.strip_prefix(root).ok()?.components().collect();
    let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();

    let up = std::iter::repeat_n("..".to_string(), from.len() - shared);
    let down = to[shared..]
        .iter()
        .map(|part| encode(&part.as_os_str().to_string_lossy()));

    Some(up.chain(down).collect::<Vec<_>>().join("/"))
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
