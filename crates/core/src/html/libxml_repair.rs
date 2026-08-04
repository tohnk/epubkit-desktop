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
//! `method='html'`, which emits *HTML*: unclosed `<meta>` tags, an HTML 4.0
//! Transitional doctype, and the original XML declaration stranded after that
//! doctype. The result is not well-formed XHTML, which is what an EPUB content
//! document is required to be.
//!
//! This implementation serializes as XML instead, and strips the two artifacts
//! libxml2's HTML parser injects — the synthesized HTML doctype, and the
//! original XML declaration that the HTML parser demotes to a processing
//! instruction — then re-emits a single correct declaration. Output is
//! therefore well-formed where the Python's is not; expect this step to differ
//! when diffing the two implementations on books with malformed markup.

use libxml::parser::Parser;
use libxml::tree::{Document, NodeType, SaveOptions};

use super::{HtmlRepair, Repaired};
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

impl HtmlRepair for LibxmlRepair {
    fn name(&self) -> &'static str {
        "libxml2"
    }

    fn repair(&self, input: &[u8]) -> Result<Repaired> {
        // Strict first. Success means the document was already well-formed and
        // needs nothing but a clean reserialization.
        if let Ok(doc) = Parser::default().parse_string_with_options(input, hardened_options(false))
        {
            return Ok(Repaired {
                bytes: doc.to_string_with_options(save_options(false)).into_bytes(),
                recovered: false,
            });
        }

        // Malformed. The HTML parser recovers without dropping text.
        let mut doc = Parser::default_html()
            .parse_string_with_options(input, hardened_options(true))
            .map_err(|e| Error::Xml(format!("unrecoverable XHTML: {e}")))?;

        strip_html_parser_artifacts(&mut doc);

        let body = doc.to_string_with_options(save_options(true));
        let mut bytes = String::with_capacity(XML_DECLARATION.len() + 1 + body.len());
        bytes.push_str(XML_DECLARATION);
        bytes.push('\n');
        bytes.push_str(body.trim_start());

        Ok(Repaired {
            bytes: bytes.into_bytes(),
            recovered: true,
        })
    }
}
