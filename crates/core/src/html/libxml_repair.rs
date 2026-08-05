//! libxml2-backed [`HtmlRepair`].
//!
//! Uses the same C library as the Python implementation's lxml, so behaviour
//! stays comparable while the port is validated against the reference.
//!
//! # Why two parsers
//!
//! Well-formed input goes through libxml2's **XML** parser and is serialized
//! straight back — no heuristics touch a document that does not need them.
//!
//! Malformed input falls back to libxml2's **HTML** parser, matching what the
//! Python does. That choice is not arbitrary. Running the XML parser in
//! recovery mode over broken markup silently *deletes* content: a bare `&` in
//! prose disappears entirely, and unclosed block elements come back
//! incorrectly nested (`<p>a<p>b</p></p>`). The HTML parser keeps the `&` as
//! `&amp;` and closes the blocks the way a browser would.
//!
//! # Where this deliberately differs from the Python
//!
//! The reference implementation serializes its recovered tree with
//! `method='html'`. For malformed input made only of ordinary block and inline
//! elements that still yields well-formed XML, so most recovered chapters
//! round-trip fine. But HTML serialization writes void elements *unclosed* —
//! `<br>`, `<img>`, `<hr>` rather than `<br/>`, `<img/>`, `<hr/>` — and a
//! chapter containing any of those comes out as markup that is not well-formed
//! XHTML, which is what an EPUB content document is required to be. Line
//! breaks and images are common enough that this is not a corner case.
//!
//! This implementation serializes as XML, so void elements stay closed. It
//! also strips the two artifacts libxml2's HTML *parser* leaves on the tree —
//! a synthesized HTML 4.0 doctype and the source's XML declaration demoted to
//! a processing instruction — then re-emits one correct declaration. The
//! Python sidesteps those two by serializing the root element rather than the
//! whole document, which also means it emits no XML declaration at all.

use libxml::parser::Parser;
use libxml::tree::{Document, NodeType, SaveOptions};

use super::{ContentDocument, HtmlRepair, Repaired};
use crate::xml::hardened_options;
use crate::{Error, Result};

const XML_DECLARATION: &str = r#"<?xml version="1.0" encoding="utf-8"?>"#;

/// Repairs XHTML with libxml2, trying a strict parse before falling back to
/// error recovery.
#[derive(Debug, Default, Clone, Copy)]
pub struct LibxmlRepair {
    _private: (),
}

impl LibxmlRepair {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

/// Serialization tuned for EPUB content documents.
///
/// `format` must stay off: it re-indents the tree, which inserts whitespace
/// into mixed content and visibly alters prose. `xhtml` must stay off too —
/// libxml2's XHTML serializer helpfully injects a `<meta http-equiv=
/// "Content-Type">` into every `<head>`, which is content the book did not ask
/// for.
fn save_options(no_declaration: bool) -> SaveOptions {
    SaveOptions {
        format: false,
        no_declaration,
        no_empty_tags: false,
        no_xhtml: false,
        xhtml: false,
        as_xml: true,
        as_html: false,
        non_significant_whitespace: false,
    }
}

/// Remove the artifacts libxml2's HTML parser adds to a document that was
/// really XHTML: a synthesized HTML 4.0 doctype, and the source's own XML
/// declaration demoted to a processing instruction.
fn strip_html_parser_artifacts(doc: &mut Document) {
    doc.remove_internal_subset();

    let root = doc.as_node();
    for mut child in root.get_child_nodes() {
        if child.get_type() == Some(NodeType::PiNode)
            && child.get_name().eq_ignore_ascii_case("xml")
        {
            child.unlink();
        }
    }
}

/// Parse an EPUB content document, recovering if it is malformed.
///
/// Exposed so callers that need to *edit* a content document — rewriting image
/// references, unwrapping SVG covers — get the same parse and the same
/// serialization guarantees as the repair step, rather than reimplementing
/// them and diverging.
pub fn parse_content(input: &[u8]) -> Result<ContentDocument> {
    // Strict first. Success means the document was already well-formed.
    if let Ok(doc) = Parser::default().parse_string_with_options(input, hardened_options(false)) {
        return Ok(ContentDocument {
            doc,
            recovered: false,
        });
    }

    // Malformed. The HTML parser recovers without dropping text.
    let mut doc = Parser::default_html()
        .parse_string_with_options(input, hardened_options(true))
        .map_err(|e| Error::Xml(format!("unrecoverable XHTML: {e}")))?;

    strip_html_parser_artifacts(&mut doc);

    Ok(ContentDocument {
        doc,
        recovered: true,
    })
}

/// Serialize a content document back to XHTML bytes.
pub fn serialize_content(content: &ContentDocument) -> Vec<u8> {
    if !content.recovered {
        return content
            .doc
            .to_string_with_options(save_options(false))
            .into_bytes();
    }

    // A recovered document lost its declaration to `strip_html_parser_artifacts`;
    // put a correct one back.
    let body = content.doc.to_string_with_options(save_options(true));
    let mut out = String::with_capacity(XML_DECLARATION.len() + 1 + body.len());
    out.push_str(XML_DECLARATION);
    out.push('\n');
    out.push_str(body.trim_start());
    out.into_bytes()
}

impl HtmlRepair for LibxmlRepair {
    fn name(&self) -> &'static str {
        "libxml2"
    }

    fn repair(&self, input: &[u8]) -> Result<Repaired> {
        let content = parse_content(input)?;
        Ok(Repaired {
            bytes: serialize_content(&content),
            recovered: content.recovered,
        })
    }
}
