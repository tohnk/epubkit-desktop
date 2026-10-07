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
//! a synthesized HTML 4.0 doctype and the source's XML declaration, demoted to
//! a processing instruction or a comment — then re-emits one correct
//! declaration. The Python sidesteps those two by serializing the root element
//! rather than the whole document, which also means it emits no XML
//! declaration at all.

use std::borrow::Cow;
use std::sync::LazyLock;

use encoding_rs::{Encoding, UTF_8, WINDOWS_1252};
use libxml::parser::{Parser, ParserOptions};
use libxml::tree::{Document, NodeType, SaveOptions};
use regex::bytes::Regex;

use super::{ContentDocument, HtmlRepair, Repaired};
use crate::xml::hardened_options;
use crate::{Error, Result};

const XML_DECLARATION: &str = r#"<?xml version="1.0" encoding="utf-8"?>"#;

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// The encoding an XML declaration names. A declaration can only open the
/// document.
static XML_DECLARED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i-u)\A\s*<\?xml\s[^>]*?\bencoding\s*=\s*["']([^"'>]*)["']"#).unwrap()
});

/// The encoding a `<meta>` names, as `charset="…"` or inside `content="…;
/// charset=…"`.
static META_DECLARED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i-u)<meta\s[^>]*?\bcharset\s*=\s*["']?\s*([^\s"'/>;]+)"#).unwrap()
});

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
/// into mixed content and visibly alters prose. libxml2's XHTML serializer
/// must stay out too. It injects a `<meta http-equiv="Content-Type">` into
/// every `<head>`, copies each `<a name>` into an `id` that can duplicate
/// another, and mirrors `lang` into `xml:lang`. Leaving `xhtml` off is not
/// enough to keep it out, since libxml2 picks it for any XHTML 1.0 doctype;
/// `no_xhtml` is.
fn save_options(no_declaration: bool) -> SaveOptions {
    SaveOptions {
        format: false,
        no_declaration,
        no_empty_tags: false,
        no_xhtml: true,
        xhtml: false,
        as_xml: true,
        as_html: false,
        non_significant_whitespace: false,
    }
}

/// Remove the artifacts libxml2's HTML parser adds to a document that was
/// really XHTML: a synthesized HTML 4.0 doctype, and the source's own XML
/// declaration, demoted by 2.9 to a processing instruction and by 2.14, which
/// reads `<?` as HTML5 does, to a `<!--?xml …?-->` comment.
fn strip_html_parser_artifacts(doc: &mut Document) {
    doc.remove_internal_subset();

    let root = doc.as_node();
    for mut child in root.get_child_nodes() {
        let declaration = match child.get_type() {
            Some(NodeType::PiNode) => child.get_name().eq_ignore_ascii_case("xml"),
            Some(NodeType::CommentNode) => is_demoted_declaration(&child.get_content()),
            _ => false,
        };
        if declaration {
            child.unlink();
        }
    }
}

/// Whether a comment is what libxml2 2.14 makes of an XML declaration.
fn is_demoted_declaration(comment: &str) -> bool {
    comment
        .get(..4)
        .is_some_and(|start| start.eq_ignore_ascii_case("?xml"))
        && comment[4..].starts_with(|c: char| c.is_ascii_whitespace() || c == '?')
}

/// Prepare malformed input for the HTML parser that recovers it.
///
/// Left to itself, libxml2's HTML parser guesses the encoding, and the guess
/// has changed between releases: 2.9 reads undeclared bytes as UTF-8, 2.14 as
/// ISO-8859-1, so every "ä" in a malformed chapter comes back as "Ã¤". Both
/// also obey a `<meta>` charset, and books converted from old HTML often still
/// declare ISO-8859-1 long after their bytes were re-encoded as UTF-8.
///
/// EPUB content documents are UTF-8, so input that is valid UTF-8 is parsed as
/// UTF-8 whatever it declares. Input that is not is decoded here first, as
/// [`legacy_encoding`] decides, and so reaches the parser as UTF-8 as well.
///
/// Two things hold every release to UTF-8: a byte order mark, which settles
/// the encoding before anything in the document can, and `ignore_enc`,
/// without which 2.9 still lets a `<meta>` override the mark. (`libxml`'s
/// `encoding` option would be the direct route, but 0.3.21 frees the C string
/// it builds from it before libxml2 reads it.)
///
/// Only input with something to decode carries the mark, a mark of the file's
/// own included. ASCII reads the same in every encoding in question, and 2.9
/// looks for a mark only when at least four bytes are there to look at, so on
/// a chapter holding nothing else it would come out as text.
///
/// The strict XML parse needs none of this: XML settles the encoding from the
/// byte order mark and the XML declaration, defaulting to UTF-8.
fn prepare_for_recovery(input: &[u8]) -> (Cow<'_, [u8]>, ParserOptions<'static>) {
    let text = if std::str::from_utf8(input).is_ok() {
        Cow::Borrowed(input.strip_prefix(UTF8_BOM).unwrap_or(input))
    } else {
        // Decoding drops a byte order mark along with the rest of the old
        // encoding.
        let (text, _, _) = legacy_encoding(input).decode(input);
        Cow::Owned(text.into_owned().into_bytes())
    };

    let bytes = if text.is_ascii() {
        text
    } else {
        Cow::Owned([UTF8_BOM, &text].concat())
    };
    let options = ParserOptions {
        ignore_enc: true,
        ..hardened_options(true)
    };

    (bytes, options)
}

/// What a chapter that is not UTF-8 is written in.
///
/// A byte order mark decides, then an encoding the chapter names, in its XML
/// declaration or else a `<meta>`. Otherwise it is windows-1252, which legacy
/// text overwhelmingly was and which browsers take a declared ISO-8859-1 to
/// mean: it has curly quotes and dashes where Latin-1 has invisible controls.
///
/// libxml2 cannot be left to do this: its HTML parser ignores an encoding
/// named in an XML declaration, 2.9 reading on as Latin-1 and 2.14 as UTF-8,
/// and reads windows-1252's punctuation as Latin-1's controls.
///
/// A name that cannot be right counts for nothing: a UTF-8 the bytes belie,
/// one nobody knows, or one of an encoding that is not ASCII-compatible, which
/// could not have been read as ASCII to find it.
fn legacy_encoding(input: &[u8]) -> &'static Encoding {
    if let Some((encoding, _)) = Encoding::for_bom(input) {
        return encoding;
    }

    [XML_DECLARED.captures(input), META_DECLARED.captures(input)]
        .into_iter()
        .flatten()
        .filter_map(|declared| Encoding::for_label(&declared[1]))
        .find(|encoding| encoding.is_ascii_compatible() && *encoding != UTF_8)
        .unwrap_or(WINDOWS_1252)
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
    let (bytes, options) = prepare_for_recovery(input);
    let mut doc = Parser::default_html()
        .parse_string_with_options(bytes, options)
        .map_err(|e| Error::Xml(format!("unrecoverable XHTML: {e}")))?;

    // Recovering an empty or blank file yields no element at all, depending
    // on the libxml2 release, either as a failure or as a document with no
    // root, which would serialize to a bare declaration. Either way there was
    // nothing to recover.
    if doc.get_root_element().is_none() {
        return Err(Error::Xml("unrecoverable XHTML: no content".into()));
    }

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
