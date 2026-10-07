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
use std::collections::HashMap;
use std::sync::LazyLock;

use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};
use libxml::parser::{Parser, ParserOptions};
use libxml::tree::{Document, NodeType, SaveOptions};
use regex::bytes::Regex;

use super::{ContentDocument, HtmlRepair, Repaired};
use crate::xml::{self, hardened_options};
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

/// A general entity a DOCTYPE's internal subset declares with a plain value:
/// not a parameter entity, and not one fetched from elsewhere.
static ENTITY_DECLARED: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"<!ENTITY\s+([A-Za-z_:][\w.:-]*)\s+(?:"([^"]*)"|'([^']*)')\s*>"#).unwrap()
});

/// What a chapter's source has besides its text, for [`kept_the_text`].
static COMMENTS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?s)<!--.*?-->").unwrap());
static TAGS: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"<[^>]*>").unwrap());
static CHARACTER_REFERENCES: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"&#?[A-Za-z0-9]+;").unwrap());

/// A reference to a named entity.
static ENTITY_REFERENCE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"&([A-Za-z_:][\w.:-]*);").unwrap());

/// How much filling in a subset's entities may grow a chapter, past which
/// they are left as references: a few may be long, and used often, but not
/// so much as to make a small chapter enormous.
const MAX_ENTITY_GROWTH: usize = 1 << 20;

const XHTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

/// SVG's element names that are not all lowercase, from the HTML standard's
/// table for putting back what its parser lowercased.
const SVG_ELEMENTS: &[&str] = &[
    "altGlyph",
    "altGlyphDef",
    "altGlyphItem",
    "animateColor",
    "animateMotion",
    "animateTransform",
    "clipPath",
    "feBlend",
    "feColorMatrix",
    "feComponentTransfer",
    "feComposite",
    "feConvolveMatrix",
    "feDiffuseLighting",
    "feDisplacementMap",
    "feDistantLight",
    "feDropShadow",
    "feFlood",
    "feFuncA",
    "feFuncB",
    "feFuncG",
    "feFuncR",
    "feGaussianBlur",
    "feImage",
    "feMerge",
    "feMergeNode",
    "feMorphology",
    "feOffset",
    "fePointLight",
    "feSpecularLighting",
    "feSpotLight",
    "feTile",
    "feTurbulence",
    "foreignObject",
    "glyphRef",
    "linearGradient",
    "radialGradient",
    "textPath",
];

/// SVG's attribute names that are not all lowercase, likewise.
const SVG_ATTRIBUTES: &[&str] = &[
    "attributeName",
    "attributeType",
    "baseFrequency",
    "baseProfile",
    "calcMode",
    "clipPathUnits",
    "diffuseConstant",
    "edgeMode",
    "filterUnits",
    "glyphRef",
    "gradientTransform",
    "gradientUnits",
    "kernelMatrix",
    "kernelUnitLength",
    "keyPoints",
    "keySplines",
    "keyTimes",
    "lengthAdjust",
    "limitingConeAngle",
    "markerHeight",
    "markerUnits",
    "markerWidth",
    "maskContentUnits",
    "maskUnits",
    "numOctaves",
    "pathLength",
    "patternContentUnits",
    "patternTransform",
    "patternUnits",
    "pointsAtX",
    "pointsAtY",
    "pointsAtZ",
    "preserveAlpha",
    "preserveAspectRatio",
    "primitiveUnits",
    "refX",
    "refY",
    "repeatCount",
    "repeatDur",
    "requiredExtensions",
    "requiredFeatures",
    "specularConstant",
    "specularExponent",
    "spreadMethod",
    "startOffset",
    "stdDeviation",
    "stitchTiles",
    "surfaceScale",
    "systemLanguage",
    "tableValues",
    "targetX",
    "targetY",
    "textLength",
    "viewBox",
    "viewTarget",
    "xChannelSelector",
    "yChannelSelector",
    "zoomAndPan",
];

/// MathML's, likewise.
const MATHML_ATTRIBUTES: &[&str] = &["definitionURL"];

/// How UTF-16 XML without a byte order mark begins: `<?` in two-byte units.
const UTF16LE_START: &[u8] = b"<\0?\0";
const UTF16BE_START: &[u8] = b"\0<\0?";

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

/// Put back the capitals the HTML parser took out of SVG and MathML names.
///
/// HTML is case-insensitive, and its parser lowercases every name. SVG's and
/// MathML's are not: `viewbox` and `<lineargradient>` mean nothing to an SVG
/// renderer, so a recovered illustration lost its scaling and gradients.
fn restore_case(doc: &Document) -> Result<()> {
    let inside = |root: &str| format!("//*[local-name()='{root}']/descendant-or-self::*");

    for (root, elements, attributes) in [
        ("svg", SVG_ELEMENTS, SVG_ATTRIBUTES),
        ("math", &[][..], MATHML_ATTRIBUTES),
    ] {
        for mut element in xml::find_nodes(doc, &inside(root))? {
            let name = element.get_name();
            if let Some(proper) = elements
                .iter()
                .find(|proper| proper.to_ascii_lowercase() == name)
            {
                element.set_name(proper).ok();
            }
            for (name, value) in element.get_attributes() {
                let Some(proper) = attributes
                    .iter()
                    .find(|proper| proper.to_ascii_lowercase() == name)
                else {
                    continue;
                };
                element.remove_attribute(&name).ok();
                element.set_attribute(proper, &value).ok();
            }
        }
    }

    Ok(())
}

/// Make what the HTML parser recovered legal XML.
///
/// HTML allows what XML does not, and the parser keeps it: `--` inside a
/// comment, attribute names like `&&` made of the words after a bare `<` (as
/// libxml2 2.14 reads one), an XML declaration in the middle of the body,
/// characters XML forbids written as references. And it keeps a stylesheet's
/// or script's text as it stands, the author's own `<![CDATA[` markers
/// included, which the XML writer would wrap in a CDATA section of its own.
fn make_legal_xml(doc: &Document) -> Result<()> {
    for mut comment in xml::find_nodes(doc, "//comment()")? {
        let mut text = comment.get_content();
        if text.contains("--") || text.ends_with('-') {
            while text.contains("--") {
                text = text.replace("--", "- -");
            }
            if text.ends_with('-') {
                text.push(' ');
            }
            comment.set_content(&text).ok();
        }
    }

    for mut instruction in xml::find_nodes(doc, "//processing-instruction()")? {
        let name = instruction.get_name();
        if name.eq_ignore_ascii_case("xml") || !is_xml_name(&name) {
            instruction.unlink();
        }
    }

    for mut element in xml::find_nodes(doc, "//*")? {
        if !is_xml_name(&element.get_name()) {
            element.set_name("span").ok();
        }
        for (name, value) in element.get_attributes() {
            if !is_xml_name(&name) {
                element.remove_attribute(&name).ok();
            } else if value.contains(is_forbidden_in_xml) {
                element
                    .set_attribute(&name, &value.replace(is_forbidden_in_xml, " "))
                    .ok();
            }
        }

        if matches!(
            element.get_name().to_ascii_lowercase().as_str(),
            "style" | "script"
        ) {
            for mut text in element.get_child_nodes() {
                let content = text.get_content();
                let inner = content.trim();
                if let Some(inner) = inner
                    .strip_prefix("<![CDATA[")
                    .and_then(|inner| inner.strip_suffix("]]>"))
                {
                    text.set_content(inner).ok();
                }
            }
        }
    }

    for mut text in xml::find_nodes(doc, "//text()")? {
        let content = text.get_content();
        if content.contains(is_forbidden_in_xml) {
            text.set_content(&content.replace(is_forbidden_in_xml, " "))
                .ok();
        }
    }

    Ok(())
}

/// Whether `name` can name an element, attribute or processing instruction
/// in XML. Simplified: anything non-ASCII is taken to be a name character.
fn is_xml_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || matches!(first, '_' | ':'))
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '-' | '.'))
}

/// Characters XML forbids outright, even as references.
fn is_forbidden_in_xml(c: char) -> bool {
    (c < ' ' && !matches!(c, '\t' | '\n' | '\r')) || matches!(c, '\u{fffe}' | '\u{ffff}')
}

/// Whether `doc`, recovered from `source`, kept at least half of its text.
///
/// libxml2 stops at its nesting limit, even raised, and hands back what it
/// has without an error. Recovery that loses most of a chapter is not
/// recovery; refusing it leaves the chapter as it was.
fn kept_the_text(source: &str, doc: &Document) -> bool {
    let printable = |text: &str| text.chars().filter(|c| !c.is_whitespace()).count();

    let source = COMMENTS.replace_all(source, "");
    let source = TAGS.replace_all(&source, "");
    let source = CHARACTER_REFERENCES.replace_all(&source, "&");
    let kept = doc
        .get_root_element()
        .map_or(0, |root| printable(&root.get_content()));

    kept * 2 >= printable(&source)
}

/// Put the XHTML namespace back on a recovered `<html>` that has lost it: one
/// the HTML parser had to imply, say, because something stood before the real
/// one. Without it a reader that minds namespaces does not see XHTML at all.
fn restore_namespace(doc: &Document) {
    if let Some(mut root) = doc.get_root_element() {
        if root.get_name().eq_ignore_ascii_case("html") && root.get_attribute("xmlns").is_none() {
            root.set_attribute("xmlns", XHTML_NAMESPACE).ok();
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

/// A chapter as UTF-8, however it was saved, saying so in its XML declaration
/// and any `<meta>` charset.
///
/// Both parsers read what comes back, so a chapter is decoded the same way
/// whether or not it has a markup error in it. EPUB content documents are
/// UTF-8, so:
///
/// - A byte order mark decides first, and UTF-16 can also be told by how its
///   declaration starts.
/// - Bytes that are valid UTF-8 are UTF-8, whatever they declare: books
///   converted from old HTML often still declare ISO-8859-1 long after their
///   text was re-encoded.
/// - Other bytes are in the legacy encoding the chapter names, if it names one
///   that can be right ([`declared_legacy_encoding`]).
/// - Failing that, they are UTF-8 with stray bytes pasted in, each of which is
///   read as windows-1252, the encoding legacy text overwhelmingly was. A
///   chapter that is windows-1252 throughout reads the same way, since its
///   bytes are almost never valid UTF-8 as well.
///
/// Left to itself, libxml2 read a well-formed chapter declared ISO-8859-1 as
/// Latin-1, leaving its curly quotes and dashes as invisible control
/// characters, and the strict and recovering parsers disagreed about one
/// declared wrongly.
fn as_utf8(input: &[u8]) -> Cow<'_, [u8]> {
    let text: Cow<'_, str> = if let Some((encoding, bom)) = Encoding::for_bom(input) {
        encoding.decode_without_bom_handling(&input[bom..]).0
    } else if input.starts_with(UTF16LE_START) {
        UTF_16LE.decode_without_bom_handling(input).0
    } else if input.starts_with(UTF16BE_START) {
        UTF_16BE.decode_without_bom_handling(input).0
    } else if let Ok(text) = std::str::from_utf8(input) {
        Cow::Borrowed(text)
    } else if let Some(encoding) = declared_legacy_encoding(input) {
        encoding.decode_without_bom_handling(input).0
    } else {
        Cow::Owned(utf8_with_stray_bytes(input))
    };

    match without_junk_before_root(declare_utf8(text)) {
        Cow::Borrowed(text) => Cow::Borrowed(text.as_bytes()),
        Cow::Owned(text) => Cow::Owned(text.into_bytes()),
    }
}

/// `text` without what has no business before its root element: byte order
/// marks, stray or garbled by a trip through windows-1252 into "ï»¿", and
/// blanks ahead of the XML declaration. The HTML parser took any of them for
/// body text, and opened an implied `<html><body>`, dropping the real `<html>`
/// and `<head>` with their namespace and language.
fn without_junk_before_root(text: Cow<'_, str>) -> Cow<'_, str> {
    let root = root_start(&text);
    let prolog = &text[..root];
    let tidy = prolog
        .replace('\u{feff}', "")
        .replace("\u{ef}\u{bb}\u{bf}", "");
    let tidy = tidy.trim_start();
    if tidy.len() == prolog.len() {
        return text;
    }
    Cow::Owned(format!("{tidy}{}", &text[root..]))
}

/// Where the root element starts: the first `<` that does not open a
/// declaration, processing instruction, comment or DOCTYPE.
fn root_start(text: &str) -> usize {
    let mut from = 0;
    while let Some(offset) = text[from..].find('<') {
        let at = from + offset;
        let rest = &text[at..];
        let length = if rest.starts_with("<?") {
            rest.find("?>").map(|end| end + 2)
        } else if rest.starts_with("<!--") {
            rest.find("-->").map(|end| end + 3)
        } else if rest.starts_with("<!") {
            doctype_length(rest)
        } else {
            return at;
        };
        match length {
            Some(length) => from = at + length,
            None => return text.len(),
        }
    }
    text.len()
}

/// How long the DOCTYPE `text` starts with is, internal subset and all.
fn doctype_length(text: &str) -> Option<usize> {
    let close = text.find('>')?;
    match text.find('[') {
        Some(open) if open < close => {
            let subset_end = open + text[open..].find(']')?;
            Some(subset_end + text[subset_end..].find('>')? + 1)
        }
        _ => Some(close + 1),
    }
}

/// `text` without its DOCTYPE, if that has an internal subset, and with the
/// entities the subset declares filled in where they are used.
///
/// libxml2's HTML parser cannot read an internal subset. It stops the DOCTYPE
/// at the subset's first `>`, and the rest of the declarations become text.
/// Only plain values are filled in, and only so far, so a subset cannot blow a
/// chapter up. The strict XML parse needs none of this: it reads subsets.
fn without_internal_subset(text: &str) -> Cow<'_, str> {
    let root = root_start(text);
    let Some(start) = text[..root].find("<!DOCTYPE") else {
        return Cow::Borrowed(text);
    };
    let Some(length) = doctype_length(&text[start..]) else {
        return Cow::Borrowed(text);
    };
    let doctype = &text[start..start + length];
    if !doctype.contains('[') {
        return Cow::Borrowed(text);
    }

    let entities: HashMap<&str, &str> = ENTITY_DECLARED
        .captures_iter(doctype)
        .filter_map(|declared| {
            let value = declared.get(2).or_else(|| declared.get(3))?;
            Some((declared.get(1)?.as_str(), value.as_str()))
        })
        .collect();

    let rest = &text[start + length..];
    let mut growth = 0usize;
    let filled = ENTITY_REFERENCE.replace_all(rest, |reference: &regex::Captures| {
        match entities.get(&reference[1]) {
            Some(value) if growth + value.len() <= MAX_ENTITY_GROWTH => {
                growth += value.len();
                (*value).to_string()
            }
            _ => reference[0].to_string(),
        }
    });

    Cow::Owned(format!("{}{filled}", &text[..start]))
}

/// `text` with every control character XML forbids, NUL among them, made a
/// space, and the two noncharacters it forbids made U+FFFD. libxml2 2.9 lost
/// everything after a NUL, and dropped other controls from between words or
/// wrote them raw into attributes; 2.14 shows a U+FFFD for each.
fn without_forbidden_characters(text: &[u8]) -> Cow<'_, [u8]> {
    let forbidden = |byte: u8| byte < 0x20 && !matches!(byte, b'\t' | b'\n' | b'\r');
    let noncharacter =
        |rest: &[u8]| rest.starts_with(b"\xEF\xBF\xBE") || rest.starts_with(b"\xEF\xBF\xBF");

    if !text
        .iter()
        .enumerate()
        .any(|(at, &byte)| forbidden(byte) || (byte == 0xEF && noncharacter(&text[at..])))
    {
        return Cow::Borrowed(text);
    }

    let mut clean = text.to_vec();
    for at in 0..clean.len() {
        if forbidden(clean[at]) {
            clean[at] = b' ';
        } else if clean[at] == 0xEF && noncharacter(&clean[at..]) {
            clean[at + 2] = 0xBD;
        }
    }
    Cow::Owned(clean)
}

/// `bytes` as UTF-8 wherever they are valid UTF-8, and as windows-1252 where
/// they are not.
fn utf8_with_stray_bytes(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        text.push_str(chunk.valid());
        text.push_str(&WINDOWS_1252.decode_without_bom_handling(chunk.invalid()).0);
    }
    text
}

/// `text` with any encoding its XML declaration or a `<meta>` names, other
/// than UTF-8, renamed UTF-8.
fn declare_utf8(text: Cow<'_, str>) -> Cow<'_, str> {
    let names_other = |label: &[u8]| Encoding::for_label(label) != Some(UTF_8);

    let mut stale: Vec<std::ops::Range<usize>> = XML_DECLARED
        .captures(text.as_bytes())
        .into_iter()
        .chain(META_DECLARED.captures_iter(text.as_bytes()))
        .filter_map(|declared| declared.get(1))
        .filter(|label| names_other(label.as_bytes()))
        .map(|label| label.range())
        .collect();
    if stale.is_empty() {
        return text;
    }
    stale.sort_by_key(|range| range.start);

    let mut renamed = String::with_capacity(text.len());
    let mut kept_from = 0;
    for range in stale {
        renamed.push_str(&text[kept_from..range.start]);
        renamed.push_str("utf-8");
        kept_from = range.end;
    }
    renamed.push_str(&text[kept_from..]);
    Cow::Owned(renamed)
}

/// Prepare malformed input, already UTF-8, for the HTML parser that recovers
/// it.
///
/// Left to itself, libxml2's HTML parser guesses the encoding, and the guess
/// has changed between releases: 2.9 reads undeclared bytes as UTF-8, 2.14 as
/// ISO-8859-1. Both also obey a `<meta>` charset.
///
/// Two things hold every release to UTF-8: a byte order mark, which settles
/// the encoding before anything in the document can, and `ignore_enc`,
/// without which 2.9 still lets a `<meta>` override the mark. (`libxml`'s
/// `encoding` option would be the direct route, but 0.3.21 frees the C string
/// it builds from it before libxml2 reads it.)
///
/// Only input with something to decode carries the mark. ASCII reads the same
/// in every encoding in question, and 2.9 looks for a mark only when at least
/// four bytes are there to look at, so on a chapter holding nothing else it
/// would come out as text.
///
/// `huge` lifts libxml2's nesting limit from 256 levels to 2048. At 256, a
/// chapter of unclosed `<div>`s lost everything after the 255th. The limits
/// it also lifts guard against entity expansion, which the HTML parser does
/// not do: it reads no DTD.
fn prepare_for_recovery(text: &[u8]) -> (Cow<'_, [u8]>, ParserOptions<'static>) {
    let text = std::str::from_utf8(text).expect("decoded to UTF-8 already");
    let text = match without_internal_subset(text) {
        Cow::Borrowed(text) => without_forbidden_characters(text.as_bytes()),
        Cow::Owned(text) => Cow::Owned(without_forbidden_characters(text.as_bytes()).into_owned()),
    };

    let bytes = if text.is_ascii() {
        text
    } else {
        Cow::Owned([UTF8_BOM, &text].concat())
    };
    let options = ParserOptions {
        ignore_enc: true,
        huge: true,
        ..hardened_options(true)
    };

    (bytes, options)
}

/// The legacy encoding a chapter that is not UTF-8 names, in its XML
/// declaration or else a `<meta>`.
///
/// libxml2 cannot be left to read this: its HTML parser ignores an encoding
/// named in an XML declaration, 2.9 reading on as Latin-1 and 2.14 as UTF-8,
/// and both read windows-1252's punctuation as Latin-1's controls, where
/// browsers take a declared ISO-8859-1 to mean windows-1252, as this does.
///
/// A name that cannot be right counts for nothing: a UTF-8 the bytes belie,
/// one nobody knows, or one of an encoding that is not ASCII-compatible, which
/// could not have been read as ASCII to find it.
fn declared_legacy_encoding(input: &[u8]) -> Option<&'static Encoding> {
    [XML_DECLARED.captures(input), META_DECLARED.captures(input)]
        .into_iter()
        .flatten()
        .filter_map(|declared| Encoding::for_label(&declared[1]))
        .find(|encoding| encoding.is_ascii_compatible() && *encoding != UTF_8)
}

/// Parse an EPUB content document, recovering if it is malformed.
///
/// Exposed so callers that need to *edit* a content document — rewriting image
/// references, unwrapping SVG covers — get the same parse and the same
/// serialization guarantees as the repair step, rather than reimplementing
/// them and diverging.
pub fn parse_content(input: &[u8]) -> Result<ContentDocument> {
    let text = as_utf8(input);

    // Strict first. Success means the document was already well-formed.
    if let Ok(doc) = Parser::default().parse_string_with_options(&text, hardened_options(false)) {
        return Ok(ContentDocument {
            doc,
            recovered: false,
        });
    }

    // Malformed. The HTML parser recovers without dropping text.
    let (bytes, options) = prepare_for_recovery(&text);
    let mut doc = Parser::default_html()
        .parse_string_with_options(&bytes, options)
        .map_err(|e| Error::Xml(format!("unrecoverable XHTML: {e}")))?;

    // Recovering an empty or blank file yields no element at all, depending
    // on the libxml2 release, either as a failure or as a document with no
    // root, which would serialize to a bare declaration. Either way there was
    // nothing to recover.
    if doc.get_root_element().is_none() {
        return Err(Error::Xml("unrecoverable XHTML: no content".into()));
    }

    if !kept_the_text(&String::from_utf8_lossy(&bytes), &doc) {
        return Err(Error::Xml(
            "unrecoverable XHTML: recovery would lose most of the text".into(),
        ));
    }

    strip_html_parser_artifacts(&mut doc);
    restore_namespace(&doc);
    restore_case(&doc)?;
    make_legal_xml(&doc)?;

    let content = ContentDocument {
        doc,
        recovered: true,
    };

    // Whatever was recovered has to read back strictly, or every later pass
    // would recover it again. Past the strict parse's own 256 levels too:
    // with the internal subset gone there are no entities to expand.
    let deep = ParserOptions {
        huge: true,
        ..hardened_options(false)
    };
    Parser::default()
        .parse_string_with_options(serialize_content(&content), deep)
        .map_err(|e| Error::Xml(format!("recovered XHTML is not well-formed: {e}")))?;

    Ok(content)
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
