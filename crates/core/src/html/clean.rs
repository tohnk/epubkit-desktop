//! Structural cleanup passes over XHTML content documents: attribute
//! stripping, blank-element collapsing, and chapter page breaks. The remaining
//! half of `html_cleaner.py` (the CSS passes live in [`crate::css`]).

use libxml::tree::{Node, NodeType};

use super::{parse_content, serialize_content};
use crate::{xml, Result};

/// Attributes worth their bytes on a device with 380KB of RAM: everything that
/// affects rendering, linking or table structure.
const KEEP_ATTRS: &[&str] = &[
    "class",
    "id",
    "href",
    "src",
    "style",
    "alt",
    "title",
    "type",
    "name",
    "content",
    "charset",
    "http-equiv",
    "xmlns",
    "version",
    "media-type",
    "properties",
    "rel",
    "media",
    "width",
    "height",
    "colspan",
    "rowspan",
    "scope",
    "headers",
    "border",
    "cellpadding",
    "cellspacing",
    "lang",
];

/// Attribute families no e-ink reader acts on.
const STRIP_ATTR_PREFIXES: &[&str] = &["data-", "aria-"];

/// Interaction and accessibility attributes with no meaning on a device that
/// has neither a pointer nor a screen reader.
const STRIP_ATTRS: &[&str] = &[
    "role",
    "tabindex",
    "accesskey",
    "draggable",
    "contenteditable",
    "spellcheck",
    "autocorrect",
    "autocapitalize",
    "autofocus",
    "dir",
    "translate",
    "inputmode",
    "enterkeyhint",
    "hidden",
    "inert",
    "popover",
];

/// The rule injected by [`add_chapter_page_breaks`].
const PAGE_BREAK_CSS: &str = "\nh1, h2 { page-break-before: always; }\n";

/// Remove attributes that only add parsing overhead. Returns the cleaned
/// document and how many attributes went.
pub fn strip_unnecessary_attributes(xhtml_bytes: &[u8]) -> Result<(Vec<u8>, usize)> {
    let content = parse_content(xhtml_bytes)?;
    let mut removed = 0;

    for mut node in xml::find_nodes(&content.doc, "//*")? {
        let doomed: Vec<String> = node
            .get_attributes()
            .into_keys()
            .filter(|name| should_strip(name))
            .collect();

        for name in doomed {
            if node.remove_attribute(&name).is_ok() {
                removed += 1;
            }
        }
    }

    if removed == 0 {
        return Ok((xhtml_bytes.to_vec(), 0));
    }

    Ok((serialize_content(&content), removed))
}

/// Collapse runs of empty `<p>` and `<div>` elements used as vertical spacing,
/// keeping the first of each run. Returns the cleaned document and how many
/// elements went.
///
/// Runs are counted among siblings. The reference tracked them in document
/// order instead, so an empty paragraph in one part of the tree could pair with
/// an unrelated one elsewhere and get dropped; grouping by sibling is what
/// "consecutive empty paragraphs" means.
pub fn normalize_whitespace(xhtml_bytes: &[u8]) -> Result<(Vec<u8>, usize)> {
    let content = parse_content(xhtml_bytes)?;
    let mut removed = 0;

    for parent in xml::find_nodes(&content.doc, "//*")? {
        let mut run: Vec<Node> = Vec::new();

        for child in parent.get_child_nodes() {
            match child.get_type() {
                // Layout whitespace between blocks does not break a run.
                Some(NodeType::TextNode) if child.get_content().trim().is_empty() => continue,
                Some(NodeType::ElementNode) if is_empty_block(&child) => run.push(child),
                _ => collapse_run(&mut run, &mut removed),
            }
        }

        collapse_run(&mut run, &mut removed);
    }

    if removed == 0 {
        return Ok((xhtml_bytes.to_vec(), 0));
    }

    Ok((serialize_content(&content), removed))
}

/// Add a stylesheet rule starting each chapter heading on a new page, unless
/// the document already says something about page breaks.
pub fn add_chapter_page_breaks(xhtml_bytes: &[u8]) -> Result<Vec<u8>> {
    let content = parse_content(xhtml_bytes)?;

    let Some(mut head) = xml::find_first(&content.doc, &format!("//{}", xml::local("head")))?
    else {
        // No head to hang a stylesheet off; leave the document alone.
        return Ok(xhtml_bytes.to_vec());
    };

    let already_handled =
        xml::find_nodes_under(&content.doc, &head, &format!(".//{}", xml::local("style")))?
            .iter()
            .any(|style| style.get_content().contains("page-break-before"));

    if already_handled {
        return Ok(xhtml_bytes.to_vec());
    }

    // Inherit head's namespace so the new element stays in the XHTML one.
    let namespace = head.get_namespace();
    if let Ok(mut style) = head.new_child(namespace, "style") {
        style.set_attribute("type", "text/css").ok();
        style.set_content(PAGE_BREAK_CSS).ok();
    }

    Ok(serialize_content(&content))
}

// ---------------------------------------------------------------- internals

fn should_strip(attribute_name: &str) -> bool {
    let name = attribute_name
        .rsplit(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();

    if KEEP_ATTRS.contains(&name.as_str()) {
        return false;
    }

    STRIP_ATTR_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || STRIP_ATTRS.contains(&name.as_str())
}

/// A `<p>` or `<div>` holding neither text nor elements — spacing, not content.
fn is_empty_block(node: &Node) -> bool {
    let name = node
        .get_name()
        .rsplit(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();

    if name != "p" && name != "div" {
        return false;
    }

    let has_element_child = node
        .get_child_nodes()
        .iter()
        .any(|child| child.get_type() == Some(NodeType::ElementNode));

    !has_element_child && node.get_content().trim().is_empty()
}

/// Keep the first element of a run and unlink the rest.
///
/// Unlike lxml, where text following an element is stored *on* that element as
/// its tail and has to be reattached by hand before removal, libxml2 keeps it
/// in a separate sibling text node — so unlinking cannot swallow prose.
fn collapse_run(run: &mut Vec<Node>, removed: &mut usize) {
    if run.len() > 1 {
        for mut node in run.drain(1..) {
            node.unlink();
            *removed += 1;
        }
    }
    run.clear();
}
