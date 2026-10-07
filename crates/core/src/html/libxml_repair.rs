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

use encoding_rs::{Encoding, ISO_2022_JP, UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};
use libxml::parser::{Parser, ParserOptions};
use libxml::tree::{Document, Node, NodeType, SaveOptions};
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

/// What may be a `<meta>` naming an encoding, as `charset="…"` or inside
/// `content="…; charset=…"`, wherever it is written: in a comment, a script
/// or another tag's attribute too. Only the parsed chapter says which are
/// `<meta>` elements ([`declare_utf8_in_meta`]); this says which chapters
/// are worth asking.
static META_DECLARED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i-u)<meta\s[^>]*?\bcharset\s*=\s*["']?\s*([^\s"'/>;]+)"#).unwrap()
});

/// A general entity a DOCTYPE's internal subset declares with a plain value:
/// not a parameter entity, and not one fetched from elsewhere.
static ENTITY_DECLARED: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"<!ENTITY\s+([A-Za-z_:][\w.:-]*)\s+(?:"([^"]*)"|'([^']*)')\s*>"#).unwrap()
});

/// A CDATA section that ends.
static CDATA_SECTIONS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?s)<!\[CDATA\[.*?\]\]>").unwrap());

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

/// What every ISO-2022 escape starts with.
const ESCAPE: u8 = 0x1B;

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
            // Renamed where they stand, so they keep their order, and the
            // chapter comes out the same every time.
            for mut attribute in xml::find_nodes_under(doc, &element, "@*")? {
                let name = attribute.get_name();
                if let Some(proper) = attributes
                    .iter()
                    .find(|proper| proper.to_ascii_lowercase() == name)
                {
                    attribute.set_name(proper).ok();
                }
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
///
/// It is done in one walk over the tree, rather than a query for each kind of
/// node: libxml2 sorts what a query finds into document order, and a long run
/// of comments with no element between them took time growing with the
/// square of its length to sort.
fn make_legal_xml(doc: &Document) {
    let mut parents = vec![doc.as_node()];
    while let Some(parent) = parents.pop() {
        for mut node in parent.get_child_nodes() {
            match node.get_type() {
                Some(NodeType::CommentNode) => {
                    let mut text = node.get_content();
                    if text.contains("--") || text.ends_with('-') {
                        while text.contains("--") {
                            text = text.replace("--", "- -");
                        }
                        if text.ends_with('-') {
                            text.push(' ');
                        }
                        node.set_content(&text).ok();
                    }
                }
                Some(NodeType::PiNode) => {
                    let name = node.get_name();
                    if name.eq_ignore_ascii_case("xml") || !is_xml_name(&name) {
                        node.unlink();
                    }
                }
                Some(NodeType::ElementNode) => {
                    make_legal_element(&mut node);
                    parents.push(node);
                }
                Some(NodeType::TextNode) => {
                    let content = node.get_content();
                    if content.contains(is_forbidden_in_xml) {
                        node.set_content(&content.replace(is_forbidden_in_xml, " "))
                            .ok();
                    }
                }
                _ => {}
            }
        }
    }
}

/// [`make_legal_xml`] for one element, its name and attributes, and the text
/// of a stylesheet or script. Its text is then made legal with the rest.
fn make_legal_element(element: &mut Node) {
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

/// A chapter as UTF-8, however it was saved, saying so in its XML
/// declaration. A `<meta>` that says otherwise is put right once the chapter
/// is parsed ([`declare_utf8_in_meta`]).
///
/// Both parsers read what comes back, so a chapter is decoded the same way
/// whether or not it has a markup error in it. EPUB content documents are
/// UTF-8, so:
///
/// - A byte order mark decides first, and UTF-16 can also be told by how its
///   declaration starts.
/// - A chapter that names ISO-2022-JP and has its escapes in it is in it
///   ([`is_iso_2022_jp`]).
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
    } else if is_iso_2022_jp(input) {
        ISO_2022_JP.decode_without_bom_handling(input).0
    } else if let Ok(text) = std::str::from_utf8(input) {
        Cow::Borrowed(text)
    } else if let Some(encoding) = declared_legacy_encoding(input) {
        encoding.decode_without_bom_handling(input).0
    } else {
        Cow::Owned(utf8_with_stray_bytes(input))
    };

    // The declaration is found where it opens the chapter, so whatever stood
    // before it goes first.
    match declare_utf8(without_junk_before_root(text)) {
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
    let mut doctype_read = false;
    while let Some(offset) = text[from..].find('<') {
        let at = from + offset;
        let rest = &text[at..];
        let length = if rest.starts_with("<?") {
            rest.find("?>").map(|end| end + 2)
        } else if rest.starts_with("<!--") {
            rest.find("-->").map(|end| end + 3)
        } else if rest.starts_with("<!") {
            // A chapter has one DOCTYPE. Its quotes and comments can take
            // reading to the end of the chapter, so any other declaration is
            // read as HTML reads one, and nothing is read through twice.
            let doctype = rest
                .get(..9)
                .is_some_and(|start| start.eq_ignore_ascii_case("<!DOCTYPE"));
            if doctype && !std::mem::replace(&mut doctype_read, true) {
                doctype_length(rest)
            } else {
                declaration_length(rest)
            }
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

/// How long the DOCTYPE `text` starts with is, internal subset and all, or
/// `None` if it never ends. A `]` or `>` in a quoted identifier or value, or in
/// a comment or processing instruction of the subset, ends nothing; but in one
/// whose quote or comment never ends, the first `]` and `>` do
/// ([`declaration_length`]).
fn doctype_length(text: &str) -> Option<usize> {
    well_formed_doctype_length(text).or_else(|| declaration_length(text))
}

/// How long the declaration `text` starts with is, read as an HTML parser
/// reads a broken DOCTYPE: to its first `>`, or, if a `[` comes before that,
/// to the first `>` after the first `]` after it.
fn declaration_length(text: &str) -> Option<usize> {
    let close = text.find('>')?;
    match text[..close].find('[') {
        Some(open) => {
            let subset_end = open + text[open..].find(']')?;
            Some(subset_end + text[subset_end..].find('>')? + 1)
        }
        None => Some(close + 1),
    }
}

/// [`doctype_length`] for a DOCTYPE whose quotes and comments all end.
fn well_formed_doctype_length(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let after = |from: usize, needle: &[u8]| {
        bytes[from..]
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|at| from + at + needle.len())
    };

    let mut at = 2;
    let mut in_subset = false;
    loop {
        match *bytes.get(at)? {
            quote @ (b'"' | b'\'') => {
                at += 1 + bytes[at + 1..].iter().position(|&byte| byte == quote)? + 1;
            }
            b'<' if in_subset && bytes[at..].starts_with(b"<!--") => at = after(at + 4, b"-->")?,
            b'<' if in_subset && bytes[at..].starts_with(b"<?") => at = after(at + 2, b"?>")?,
            b'[' if !in_subset => {
                in_subset = true;
                at += 1;
            }
            b']' if in_subset => {
                in_subset = false;
                at += 1;
            }
            b'>' if !in_subset => return Some(at + 1),
            _ => at += 1,
        }
    }
}

/// Where the comments and processing instructions of a DOCTYPE are, which
/// declare nothing. Those in a quoted value are the value's text.
fn doctype_asides(doctype: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = doctype.as_bytes();
    let after = |from: usize, needle: &[u8]| {
        bytes[from..]
            .windows(needle.len())
            .position(|window| window == needle)
            .map_or(bytes.len(), |at| from + at + needle.len())
    };

    let mut asides = Vec::new();
    let mut at = 2;
    while at < bytes.len() {
        let rest = &bytes[at..];
        if rest.starts_with(b"<!--") {
            let end = after(at + 4, b"-->");
            asides.push(at..end);
            at = end;
        } else if rest.starts_with(b"<?") {
            let end = after(at + 2, b"?>");
            asides.push(at..end);
            at = end;
        } else if let quote @ (b'"' | b'\'') = rest[0] {
            at += 1 + rest[1..]
                .iter()
                .position(|&byte| byte == quote)
                .map_or(rest.len() - 1, |length| length + 1);
        } else {
            at += 1;
        }
    }
    asides
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

    // As XML reads a subset: a declaration in a comment or processing
    // instruction declares nothing, and of two for one name the first counts.
    let asides = doctype_asides(doctype);
    let mut entities: HashMap<&str, &str> = HashMap::new();
    for declared in ENTITY_DECLARED.captures_iter(doctype) {
        // The asides are in order, so the one a declaration could be in is
        // found by a search.
        let at = declared.get(0).expect("always matched").start();
        let aside = asides.partition_point(|aside| aside.end <= at);
        if asides.get(aside).is_some_and(|aside| aside.contains(&at)) {
            continue;
        }
        if let (Some(name), Some(value)) = (declared.get(1), declared.get(2).or(declared.get(3))) {
            entities.entry(name.as_str()).or_insert(value.as_str());
        }
    }

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

/// `text` with any encoding its XML declaration names, other than UTF-8,
/// renamed UTF-8.
fn declare_utf8(text: Cow<'_, str>) -> Cow<'_, str> {
    let Some(label) = XML_DECLARED
        .captures(text.as_bytes())
        .and_then(|declared| declared.get(1))
        .filter(|label| names_other_than_utf8(label.as_bytes()))
    else {
        return text;
    };

    let range = label.range();
    Cow::Owned(format!(
        "{}utf-8{}",
        &text[..range.start],
        &text[range.end..]
    ))
}

/// Make every `<meta>` of `doc` that names an encoding other than UTF-8 name
/// UTF-8, which the chapter has been read as and is saved as: a `charset`, or
/// the charset in an `http-equiv="Content-Type"`'s `content`, as browsers read
/// them.
///
/// This is done on the parsed chapter, not its text, so that only real
/// `<meta>` elements change. The same words in a CDATA section, a comment or
/// another `<meta>`'s `content` are the chapter's text, and stay as written.
/// `source` is the text `doc` was parsed from: one that names nothing else
/// has nothing to change, and is not searched.
fn declare_utf8_in_meta(doc: &Document, source: &[u8]) -> Result<()> {
    let names_other = META_DECLARED
        .captures_iter(source)
        .filter_map(|declared| declared.get(1))
        .any(|label| names_other_than_utf8(label.as_bytes()));
    if !names_other {
        return Ok(());
    }

    for mut meta in xml::find_nodes(doc, &format!("//{}", xml::local("meta")))? {
        if let Some(charset) = meta.get_attribute_no_ns("charset") {
            if names_other_than_utf8(charset.as_bytes()) {
                meta.set_attribute("charset", "utf-8").ok();
            }
        }

        let pragma = meta
            .get_attribute_no_ns("http-equiv")
            .is_some_and(|name| name.trim().eq_ignore_ascii_case("content-type"));
        let Some(content) = meta.get_attribute_no_ns("content").filter(|_| pragma) else {
            continue;
        };
        if let Some(label) = content_charset(content.as_bytes()) {
            if names_other_than_utf8(content[label.clone()].as_bytes()) {
                let renamed = format!("{}utf-8{}", &content[..label.start], &content[label.end..]);
                meta.set_attribute("content", &renamed).ok();
            }
        }
    }

    Ok(())
}

/// Where the encoding a `<meta>`'s `content` names is, found as the HTML
/// standard's algorithm for extracting a character encoding from a meta
/// element finds it.
fn content_charset(bytes: &[u8]) -> Option<std::ops::Range<usize>> {
    let blank = |at: usize| bytes.get(at).is_some_and(u8::is_ascii_whitespace);
    let mut from = 0;

    loop {
        let found = bytes[from..]
            .windows(7)
            .position(|word| word.eq_ignore_ascii_case(b"charset"))?;
        let mut at = from + found + 7;
        while blank(at) {
            at += 1;
        }
        if bytes.get(at) != Some(&b'=') {
            from = at;
            continue;
        }
        at += 1;
        while blank(at) {
            at += 1;
        }

        return match bytes.get(at)? {
            quote @ (b'"' | b'\'') => {
                let end = at + 1 + bytes[at + 1..].iter().position(|byte| byte == quote)?;
                Some(at + 1..end)
            }
            _ => {
                let end = bytes[at..]
                    .iter()
                    .position(|byte| byte.is_ascii_whitespace() || *byte == b';')
                    .map_or(bytes.len(), |length| at + length);
                Some(at..end)
            }
        };
    }
}

/// Whether an encoding's name, as a chapter wrote it, is anything but UTF-8's.
/// A name nobody knows is not UTF-8's either.
fn names_other_than_utf8(label: &[u8]) -> bool {
    Encoding::for_label(label) != Some(UTF_8)
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

/// The encodings a chapter names, as far as they are known: in its XML
/// declaration, and then in its first `<meta>` that names one
/// ([`meta_charset`]), which is looked for only if the declaration's will not
/// do.
fn declared_encodings(input: &[u8]) -> impl Iterator<Item = &'static Encoding> + '_ {
    let in_declaration = XML_DECLARED
        .captures(input)
        .and_then(|declared| declared.get(1))
        .and_then(|label| Encoding::for_label(label.as_bytes()));

    in_declaration
        .into_iter()
        .chain(std::iter::once_with(|| meta_charset(input)).flatten())
}

/// The encoding the first `<meta>` of a chapter that names one names: its
/// `charset`, or the charset in an `http-equiv="Content-Type"`'s `content`.
///
/// The chapter is parsed to find it, as it will be parsed once it is decoded:
/// strictly if it is well-formed, by the HTML parser if not. Its markup is
/// ASCII in any encoding a chapter can name in it, so its bytes read one for
/// one as windows-1252 give the parser the same markup, and what the parser
/// takes for a `<meta>` is one, whatever a script, a CDATA section, a comment
/// or a DOCTYPE holds. Looked for in the text, a `<meta>` was found in each of
/// those in turn, or hidden by them.
///
/// A CDATA section is the chapter's text, as XHTML writes it, however the
/// HTML parser reads one: libxml2 2.9's reads markup in it. So the HTML parser
/// is not given those.
fn meta_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    let text = WINDOWS_1252.decode_without_bom_handling(bytes).0;
    let text = declare_utf8(without_junk_before_root(text));
    let doc = Parser::default()
        .parse_string_with_options(text.as_bytes(), hardened_options(false))
        .or_else(|_| {
            let text = CDATA_SECTIONS.replace_all(&text, "");
            let (bytes, options) = prepare_for_recovery(text.as_bytes());
            Parser::default_html().parse_string_with_options(&bytes, options)
        })
        .ok()?;

    // An XHTML `<META>` is no `<meta>`, but a chapter that wrote one meant it.
    let metas = xml::find_nodes(
        &doc,
        "//*[translate(local-name(), 'ATEM', 'atem') = 'meta']",
    )
    .ok()?;
    metas.iter().find_map(|meta| {
        let attributes = meta.get_attributes();
        let attribute = |name: &str| {
            attributes.get(name).or_else(|| {
                attributes
                    .iter()
                    .filter(|(written, _)| written.eq_ignore_ascii_case(name))
                    .min_by_key(|(written, _)| written.as_str())
                    .map(|(_, value)| value)
            })
        };

        let label = match attribute("charset") {
            Some(charset) => charset.as_bytes(),
            None => {
                let pragma = attribute("http-equiv")?;
                if !pragma.trim().eq_ignore_ascii_case("content-type") {
                    return None;
                }
                let content = attribute("content")?.as_bytes();
                &content[content_charset(content)?]
            }
        };
        Encoding::for_label(label)
    })
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
/// could not have been read as ASCII to find it. ISO-2022-JP, which can, is
/// [`is_iso_2022_jp`]'s to find.
fn declared_legacy_encoding(input: &[u8]) -> Option<&'static Encoding> {
    declared_encodings(input).find(|encoding| encoding.is_ascii_compatible() && *encoding != UTF_8)
}

/// Whether a chapter is in ISO-2022-JP: it says so, and has the escapes that
/// switch the encoding into and out of its Japanese character sets.
///
/// ISO-2022-JP is written in seven bits, so it is valid UTF-8 as well, and
/// read as UTF-8 its Japanese came out as ASCII gibberish. A chapter that
/// says it is ISO-2022-JP but has no escapes is ASCII, the same either way,
/// or has been re-encoded since and is UTF-8 now.
fn is_iso_2022_jp(input: &[u8]) -> bool {
    input.contains(&ESCAPE)
        && input
            .windows(2)
            .any(|pair| pair[0] == ESCAPE && matches!(pair[1], b'$' | b'('))
        && declared_encodings(input).any(|encoding| encoding == ISO_2022_JP)
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
        declare_utf8_in_meta(&doc, &text)?;
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
    make_legal_xml(&doc);
    declare_utf8_in_meta(&doc, &text)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `<meta>` charset is found as the parser that reads the chapter finds
    /// one: in a well-formed chapter as XML, in a malformed one as HTML.
    #[test]
    fn a_meta_charset_is_found_where_the_parser_finds_one() {
        use encoding_rs::{KOI8_R, SHIFT_JIS, WINDOWS_1251};

        let page = |head: &str| {
            format!("<html><head>{head}<title>T</title></head><body><p>x</p></body></html>")
        };
        let cases: &[(String, Option<&'static Encoding>)] = &[
            (
                page(r#"<meta charset="windows-1251"/>"#),
                Some(WINDOWS_1251),
            ),
            (page("<META CHARSET=KOI8-R>"), Some(KOI8_R)),
            (page(r#"<META CHARSET="koi8-r"/>"#), Some(KOI8_R)),
            (
                page(r#"<meta http-equiv="Content-Type" content="text/html; charset=shift_jis"/>"#),
                Some(SHIFT_JIS),
            ),
            // Content names a charset only for an http-equiv.
            (
                page(r#"<meta content="text/html; charset=shift_jis"/>"#),
                None,
            ),
            // A name nobody knows is passed over for the next.
            (
                page(r#"<meta charset="no-such-thing"/><meta charset="windows-1251"/>"#),
                Some(WINDOWS_1251),
            ),
            (
                page(r#"<!-- <meta charset="koi8-r"/> --><meta charset="windows-1251"/>"#),
                Some(WINDOWS_1251),
            ),
            // As XML, a CDATA section's `</script>` ends nothing.
            (
                page(
                    r#"<script><![CDATA["</script><meta charset='koi8-r'>"]]></script><meta charset="windows-1251"/>"#,
                ),
                Some(WINDOWS_1251),
            ),
            // As HTML, a script's `<!--` starts nothing.
            (
                page(r#"<script>"<!--"</script><meta charset="windows-1251">"#),
                Some(WINDOWS_1251),
            ),
            (
                format!(
                    "<!DOCTYPE html [<!-- ]> <script> -->]>{}",
                    page(r#"<meta charset="windows-1251"/>"#)
                ),
                Some(WINDOWS_1251),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(meta_charset(text.as_bytes()), *expected, "{text}");
        }
    }

    /// A DOCTYPE ends at its own `>`, not one in a quoted value, a comment or
    /// a processing instruction of its internal subset.
    #[test]
    fn a_doctype_ends_where_it_ends() {
        let after = "<html/>";
        for doctype in [
            "<!DOCTYPE html>",
            r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">"#,
            r#"<!DOCTYPE html SYSTEM "a>b">"#,
            r#"<!DOCTYPE html [<!ENTITY a "]>">]>"#,
            "<!DOCTYPE html [<!-- ]> --><?pi ]> ?>] >",
        ] {
            let text = format!("{doctype}{after}");
            assert_eq!(doctype_length(&text), Some(doctype.len()), "{doctype}");
        }
        // A quote that never ends ends at the first `]` and `>` after all.
        assert_eq!(
            doctype_length(r#"<!DOCTYPE html PUBLIC "never closed><html/>"#),
            Some(r#"<!DOCTYPE html PUBLIC "never closed>"#.len())
        );
        assert_eq!(doctype_length("<!DOCTYPE html"), None);
    }
}
