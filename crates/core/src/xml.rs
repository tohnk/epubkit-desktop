//! Thin libxml2 wrapper for the well-formed XML an EPUB carries:
//! `container.xml`, `encryption.xml`, the OPF package document and the NCX.
//!
//! XHTML content documents are *not* handled here — they are frequently
//! malformed and go through [`crate::html`], which layers recovery on top.
//!
//! Most helpers take XPath expressions written with `local-name()` rather than
//! registered prefixes. That is deliberate: real EPUBs declare the OPF and
//! Dublin Core namespaces inconsistently, or omit them, and the Python this
//! was ported from worked around it with a chain of fallback lookups. One
//! `local-name()` expression covers the whole chain.

use std::fs;
use std::path::Path;

use libxml::parser::{Parser, ParserOptions};
use libxml::tree::{Document, Namespace, Node, SaveOptions};
use libxml::xpath::Context;

use crate::{Error, Result};

/// Parser options hardened for untrusted input.
///
/// EPUBs arrive from the open internet, so the parser must not be a network
/// client or an amplifier:
///
/// - `no_net` forbids fetching external DTDs and entities.
/// - libxml2's `NOENT` flag is never set (this crate does not expose it), so
///   internal entity references are left as references instead of being
///   substituted. That is what stops a "billion laughs" payload from
///   inflating in memory.
/// - `huge` stays `false`, keeping libxml2's built-in depth and expansion
///   limits in force.
///
/// `no_error`/`no_warning` only silence libxml2's chatter on stderr; parse
/// failure is still detected from the returned result.
pub(crate) fn hardened_options(recover: bool) -> ParserOptions<'static> {
    ParserOptions {
        recover,
        no_error: true,
        no_warning: true,
        no_net: true,
        huge: false,
        ..ParserOptions::default()
    }
}

/// Parse well-formed XML from memory, refusing to guess at broken markup.
pub fn parse_strict(bytes: &[u8]) -> Result<Document> {
    Parser::default()
        .parse_string_with_options(bytes, hardened_options(false))
        .map_err(|e| Error::Xml(e.to_string()))
}

/// Parse well-formed XML from a file.
pub fn parse_file(path: &Path) -> Result<Document> {
    let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
    parse_strict(&bytes).map_err(|e| Error::Xml(format!("{}: {e}", path.display())))
}

/// Serialize a document to disk.
///
/// `format` indents the output. Safe for the OPF and NCX, which are data
/// documents; never use it for XHTML, where it would reflow prose.
pub fn write_file(doc: &Document, path: &Path, format: bool) -> Result<()> {
    let options = SaveOptions {
        format,
        no_declaration: false,
        no_empty_tags: false,
        no_xhtml: false,
        xhtml: false,
        as_xml: true,
        as_html: false,
        non_significant_whitespace: false,
    };
    fs::write(path, doc.to_string_with_options(options)).map_err(|e| Error::io(path, e))
}

/// Evaluate an XPath expression against the whole document.
pub fn find_nodes(doc: &Document, xpath: &str) -> Result<Vec<Node>> {
    let mut context =
        Context::new(doc).map_err(|_| Error::Xml("could not create XPath context".into()))?;
    context
        .findnodes(xpath, None)
        .map_err(|_| Error::Xml(format!("could not evaluate XPath: {xpath}")))
}

/// Evaluate an XPath expression relative to `node`.
pub fn find_nodes_under(doc: &Document, node: &Node, xpath: &str) -> Result<Vec<Node>> {
    let mut context =
        Context::new(doc).map_err(|_| Error::Xml("could not create XPath context".into()))?;
    context
        .findnodes(xpath, Some(node))
        .map_err(|_| Error::Xml(format!("could not evaluate XPath: {xpath}")))
}

/// The first node matching an XPath expression, if any.
pub fn find_first(doc: &Document, xpath: &str) -> Result<Option<Node>> {
    Ok(find_nodes(doc, xpath)?.into_iter().next())
}

/// Evaluate an XPath expression and collect one attribute from each match.
/// Namespace prefixes used in `xpath` must be supplied as `(prefix, uri)`.
pub fn attribute_values(
    doc: &Document,
    xpath: &str,
    attribute: &str,
    namespaces: &[(&str, &str)],
) -> Result<Vec<String>> {
    let mut context =
        Context::new(doc).map_err(|_| Error::Xml("could not create XPath context".into()))?;

    for (prefix, uri) in namespaces {
        context
            .register_namespace(prefix, uri)
            .map_err(|_| Error::Xml(format!("could not register namespace {prefix}")))?;
    }

    let nodes = context
        .findnodes(xpath, None)
        .map_err(|_| Error::Xml(format!("could not evaluate XPath: {xpath}")))?;

    Ok(nodes
        .iter()
        .filter_map(|node| node.get_attribute(attribute))
        .collect())
}

/// An XPath step matching an element by local name, ignoring namespaces.
pub fn local(name: &str) -> String {
    format!("*[local-name()='{name}']")
}

/// Find or declare a namespace with `href` on `node`.
///
/// Reuses a declaration already in scope where possible, so appending an
/// element does not litter the document with duplicate `xmlns:` attributes.
pub fn namespace_for(doc: &Document, node: &mut Node, href: &str) -> Result<Option<Namespace>> {
    if let Some(existing) = node
        .get_namespaces(doc)
        .into_iter()
        .find(|ns| ns.get_href() == href)
    {
        return Ok(Some(existing));
    }
    Namespace::new("", href, node)
        .map(Some)
        .map_err(|e| Error::Xml(format!("could not declare namespace {href}: {e}")))
}
