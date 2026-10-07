//! Stylesheet cleanup: dropping rules nothing in the book matches, and
//! removing embedded fonts. The CSS half of `html_cleaner.py`.
//!
//! Rules are found with `cssparser`, the tokenizer of Firefox's and Servo's
//! style engines, and cut out of the text where they stand. Nothing else is
//! touched: what the reading engine gets is the book's own CSS, comments,
//! hacks and spelling included, less the rules that went. Reprinting a parsed
//! stylesheet rewords it — `cssutils`, which the reference used, into what it
//! understood, a modern CSS library into syntax older engines do not read.

use std::collections::BTreeSet;
use std::fs;
use std::ops::Range;
use std::path::Path;

use std::sync::LazyLock;

use cssparser::{ParseError, Parser, ParserInput, Token};
use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};
use libxml::tree::{Node, NodeType};

use crate::html;
use crate::{xml, Error, Result};

/// Selectors that must never be dropped, whatever the content looks like.
const ALWAYS_KEEP: &[&str] = &["*", "html", "body"];

/// At-rules whose blocks hold rules rather than declarations, and which can
/// hold an `@font-face`.
const GROUPING_RULES: &[&str] = &[
    "media",
    "supports",
    "document",
    "-moz-document",
    "layer",
    "container",
    "scope",
    "starting-style",
];

/// How deep into grouping rules to look for fonts. Real books nest a level or
/// two. One nested deeper is left as it is there, rather than followed down a
/// stack that has to end somewhere.
const MAX_NESTING: usize = 16;

/// A `<style>` element's start tag, prefixed or not and in any case, as bytes:
/// what a chapter must have for [`remove_embedded_fonts_from_styles`] to have
/// anything to do.
static STYLE_ELEMENT: LazyLock<regex::bytes::Regex> =
    LazyLock::new(|| regex::bytes::Regex::new(r"(?i-u)<(?:[^\s>/:]+:)?style[\s>/]").unwrap());

/// An edit to some text: the bytes it replaces, and what replaces them.
pub(crate) type Edit = (Range<usize>, String);

/// What a stylesheet opens with to declare its encoding. CSS allows exactly
/// this form, and only at the very start.
const CHARSET_RULE_START: &[u8] = b"@charset \"";
const CHARSET_RULE_END: &[u8] = b"\";";

/// Read a stylesheet as text, in whatever encoding it was saved.
pub fn read_stylesheet(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
    Ok(decode_stylesheet(&bytes))
}

/// Decode a stylesheet the way browsers do: a byte order mark decides, then an
/// `@charset` rule naming a legacy encoding. Otherwise it is UTF-8 if its
/// bytes are valid UTF-8, and windows-1252 if not, the encoding legacy CSS
/// overwhelmingly was; that includes one declaring a UTF-8 its bytes belie.
///
/// What comes back is fit to save as UTF-8, which is how everything here is
/// saved: an `@charset` naming anything else is rewritten to name UTF-8, so a
/// stylesheet never declares one encoding while being in another.
pub fn decode_stylesheet(bytes: &[u8]) -> String {
    let encoding = match Encoding::for_bom(bytes) {
        Some((encoding, _)) => encoding,
        None => {
            // A stylesheet really in UTF-16 cannot spell an ASCII `@charset`,
            // so one claiming UTF-16 is wrong; CSS reads it as UTF-8.
            let declared = charset_label(bytes)
                .and_then(Encoding::for_label)
                .filter(|e| ![UTF_8, UTF_16BE, UTF_16LE].contains(e));
            match declared {
                Some(encoding) => encoding,
                None if std::str::from_utf8(bytes).is_ok() => UTF_8,
                None => WINDOWS_1252,
            }
        }
    };

    // `decode` strips a byte order mark, which a UTF-8 file has no need of.
    let (text, _, _) = encoding.decode(bytes);
    let text = text.into_owned();

    let Some(rule_length) = charset_rule_length(text.as_bytes()) else {
        return text;
    };
    if charset_label(text.as_bytes()).and_then(Encoding::for_label) == Some(UTF_8) {
        return text;
    }
    format!("@charset \"UTF-8\";{}", &text[rule_length..])
}

/// The label in a stylesheet's opening `@charset` rule, if it has one.
fn charset_label(bytes: &[u8]) -> Option<&[u8]> {
    let rest = bytes.strip_prefix(CHARSET_RULE_START)?;
    let end = rest
        .windows(CHARSET_RULE_END.len())
        .position(|window| window == CHARSET_RULE_END)?;
    Some(&rest[..end])
}

/// How many bytes the opening `@charset` rule takes up, if there is one.
fn charset_rule_length(bytes: &[u8]) -> Option<usize> {
    charset_label(bytes)
        .map(|label| CHARSET_RULE_START.len() + label.len() + CHARSET_RULE_END.len())
}

/// Everything a document uses that a selector could match on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsedSelectors {
    pub classes: BTreeSet<String>,
    pub ids: BTreeSet<String>,
    pub elements: BTreeSet<String>,
}

impl UsedSelectors {
    /// Fold another document's usage into this one.
    pub fn merge(&mut self, other: &UsedSelectors) {
        self.classes.extend(other.classes.iter().cloned());
        self.ids.extend(other.ids.iter().cloned());
        self.elements.extend(other.elements.iter().cloned());
    }
}

/// Collect the element names, classes and ids one XHTML document uses.
pub fn collect_used_selectors(xhtml_bytes: &[u8]) -> Result<UsedSelectors> {
    let content = html::parse_content(xhtml_bytes)?;
    let mut used = UsedSelectors::default();

    for node in xml::find_nodes(&content.doc, "//*")? {
        let name = node
            .get_name()
            .rsplit(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !name.is_empty() {
            used.elements.insert(name);
        }

        if let Some(class_attr) = node.get_attribute("class") {
            for class in class_attr.split_whitespace() {
                used.classes.insert(class.to_string());
            }
        }

        if let Some(id) = node.get_attribute("id") {
            if !id.is_empty() {
                used.ids.insert(id);
            }
        }
    }

    Ok(used)
}

/// Drop style rules that nothing in the book can match. Returns the cleaned
/// stylesheet and how many rules went.
///
/// The test is deliberately generous: a rule survives if *any* part of *any* of
/// its selectors is in use, and anything with a pseudo-class, pseudo-element or
/// attribute selector is kept outright. Over-keeping costs a few bytes;
/// over-removing silently changes how the book looks.
///
/// Only top-level rules are considered. Anything inside an `@media` block is
/// left alone rather than filtered against markup that may not represent the
/// conditions the block applies to.
pub fn remove_unused_css(css_text: &str, used: &UsedSelectors) -> (String, usize) {
    let unused: Vec<Range<usize>> = rules(css_text, 0)
        .into_iter()
        .filter_map(|rule| match rule.kind {
            RuleKind::Style { selectors } => {
                (!selector_matches_used(&css_text[selectors], used)).then_some(rule.span)
            }
            RuleKind::At { .. } => None,
        })
        .collect();

    let removed = unused.len();
    (apply_edits(css_text, &cuts(css_text, &unused)), removed)
}

/// Remove `@font-face` rules, those inside `@media` and other grouping rules
/// included. Returns the cleaned stylesheet and how many went.
pub fn remove_embedded_fonts(css_text: &str) -> (String, usize) {
    let edits = embedded_font_cuts(css_text);
    (apply_edits(css_text, &edits), edits.len())
}

/// Remove `@font-face` rules from the `<style>` elements of an XHTML document,
/// as [`remove_embedded_fonts`] does from a stylesheet. Returns the document
/// and how many rules went; the document is untouched if none did.
pub fn remove_embedded_fonts_from_styles(xhtml_bytes: &[u8]) -> Result<(Vec<u8>, usize)> {
    // A chapter is not parsed to find it has no `<style>`. One in UTF-16, the
    // only encoding a chapter can be in that does not spell it so, is.
    if !xhtml_bytes.contains(&0) && !STYLE_ELEMENT.is_match(xhtml_bytes) {
        return Ok((xhtml_bytes.to_vec(), 0));
    }

    let content = html::parse_content(xhtml_bytes)?;
    let mut removed = 0;
    for style in xml::find_nodes(&content.doc, &format!("//{}", xml::local("style")))? {
        removed += edit_style_element(&style, embedded_font_cuts);
    }

    if removed == 0 {
        return Ok((xhtml_bytes.to_vec(), 0));
    }
    Ok((html::serialize_content(&content), removed))
}

/// Edit the stylesheet a `<style>` element holds. `edit` is given its CSS and
/// says what to change, in order and without overlaps. Returns how many
/// edits it made.
///
/// The CSS is the element's text and CDATA sections, in order, read as one
/// stylesheet, as a reading engine reads it: a rule can start in one and end
/// in the next, and the common `/*<![CDATA[*/ … /*]]>*/` makes every rule do
/// so. What an edit replaces is taken out of whichever of them it spans, and
/// what replaces it goes into the one it starts in, so that CDATA stays CDATA.
///
/// A `<style>` with an entity reference in it is left as it is. What the
/// entity stands for is part of the CSS, but cannot be edited where it is
/// written, and read as if it were not there, the CSS around it is misread:
/// `url(&cdn;cover.png)` is not `url(cover.png)`.
pub(crate) fn edit_style_element(style: &Node, edit: impl FnOnce(&str) -> Vec<Edit>) -> usize {
    let mut parts: Vec<(Node, Range<usize>)> = Vec::new();
    let mut css = String::new();
    for child in style.get_child_nodes() {
        match child.get_type() {
            Some(NodeType::TextNode | NodeType::CDataSectionNode) => {
                let start = css.len();
                css.push_str(&child.get_content());
                parts.push((child, start..css.len()));
            }
            Some(NodeType::EntityRefNode) => return 0,
            _ => {}
        }
    }

    let edits = edit(&css);
    if edits.is_empty() || parts.is_empty() {
        return 0;
    }

    // Which part a byte of `css` is in: the first to end after it. The end of
    // `css` is in the last. The parts are in order, so this is a search.
    let part_at = |at: usize| {
        parts
            .partition_point(|(_, range)| range.end <= at)
            .min(parts.len() - 1)
    };
    let mut contents = vec![String::new(); parts.len()];
    let keep = |contents: &mut [String], mut from: usize, to: usize| {
        while from < to {
            let part = part_at(from);
            let until = to.min(parts[part].1.end);
            contents[part].push_str(&css[from..until]);
            from = until;
        }
    };

    let mut kept_from = 0;
    for (range, replacement) in &edits {
        keep(&mut contents, kept_from, range.start);
        contents[part_at(range.start)].push_str(replacement);
        kept_from = range.end;
    }
    keep(&mut contents, kept_from, css.len());

    for ((mut part, range), content) in parts.iter().cloned().zip(contents) {
        if content == css[range] {
            continue;
        }
        // Text and CDATA take what they are given as it reads.
        if content.is_empty() {
            part.unlink();
        } else {
            part.set_content(&content).ok();
        }
    }

    edits.len()
}

/// `text` with `edits`, which are in order and do not overlap, made.
pub(crate) fn apply_edits(text: &str, edits: &[Edit]) -> String {
    if edits.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut kept_from = 0;
    for (range, replacement) in edits {
        out.push_str(&text[kept_from..range.start]);
        out.push_str(replacement);
        kept_from = range.end;
    }
    out.push_str(&text[kept_from..]);
    out
}

/// What removing every `@font-face` rule from `css` takes out.
fn embedded_font_cuts(css: &str) -> Vec<Edit> {
    let mut fonts = Vec::new();
    font_faces(rules(css, MAX_NESTING), &mut fonts);
    fonts.sort_by_key(|span| span.start);
    cuts(css, &fonts)
}

/// Where the `@font-face` rules among `rules`, and inside them, are.
fn font_faces(rules: Vec<Rule>, found: &mut Vec<Range<usize>>) {
    for rule in rules {
        if let RuleKind::At { name, children } = rule.kind {
            if name == "font-face" {
                found.push(rule.span);
            } else {
                font_faces(children, found);
            }
        }
    }
}

/// What taking the bytes in `spans`, which are in order and do not overlap,
/// out of `css` takes. Each goes with the blanks after it, up to and
/// including one line break, so a rule on a line of its own does not leave an
/// empty line behind.
fn cuts(css: &str, spans: &[Range<usize>]) -> Vec<Edit> {
    spans
        .iter()
        .map(|span| {
            let rest = &css[span.end..];
            let blanks = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            let line_break = match &rest[blanks..] {
                line if line.starts_with("\r\n") => 2,
                line if line.starts_with('\n') => 1,
                _ => 0,
            };
            (span.start..span.end + blanks + line_break, String::new())
        })
        .collect()
}

/// One rule of a stylesheet, where it stands in the text.
struct Rule {
    /// From its first token through its closing `}` or `;`, in bytes.
    span: Range<usize>,
    kind: RuleKind,
}

enum RuleKind {
    /// A style rule, and where its selectors are written.
    Style { selectors: Range<usize> },
    /// An at-rule, its name lowercased, and the rules inside it, if it is a
    /// grouping rule read into.
    At { name: String, children: Vec<Rule> },
}

/// The rules of `css`, reading into grouping rules `depth` levels deep.
///
/// CSS's own error recovery applies: something a browser would drop is a rule
/// like any other, which the selector test then keeps, so nothing is lost that
/// was not understood. Blocks not read into are skipped without recursion,
/// however deep they go.
fn rules(css: &str, depth: usize) -> Vec<Rule> {
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    rules_in(&mut parser, depth)
}

fn rules_in(parser: &mut Parser<'_, '_>, depth: usize) -> Vec<Rule> {
    let mut rules = Vec::new();

    loop {
        parser.skip_whitespace();
        let start = parser.position().byte_index();
        let Ok(first) = parser.next().cloned() else {
            break;
        };

        let kind = match first {
            // HTML comment markers mean nothing to CSS between rules.
            Token::CDO | Token::CDC => continue,
            Token::AtKeyword(name) => {
                let name = name.to_ascii_lowercase();
                let mut children = Vec::new();
                loop {
                    match parser.next() {
                        Ok(Token::CurlyBracketBlock) => {
                            if depth > 0 && GROUPING_RULES.contains(&name.as_str()) {
                                children = read_block(parser, |inner| rules_in(inner, depth - 1));
                            } else {
                                skip_block(parser);
                            }
                            break;
                        }
                        Ok(Token::Semicolon) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                RuleKind::At { name, children }
            }
            first => {
                // A style rule's selectors run up to its block. Skipping the
                // blanks before each token also gets past the inside of a
                // bracket the last one opened, so the selectors end after it.
                let mut selectors_end = start;
                let mut token = first;
                loop {
                    if matches!(token, Token::CurlyBracketBlock) {
                        skip_block(parser);
                        break;
                    }
                    parser.skip_whitespace();
                    selectors_end = parser.position().byte_index();
                    match parser.next() {
                        Ok(next) => token = next.clone(),
                        Err(_) => break,
                    }
                }
                RuleKind::Style {
                    selectors: start..selectors_end,
                }
            }
        };

        rules.push(Rule {
            span: start..parser.position().byte_index(),
            kind,
        });
    }

    rules
}

/// Read the block `parser` has just opened with `read`, leaving the parser
/// after its end.
fn read_block<T: Default>(parser: &mut Parser<'_, '_>, read: impl FnOnce(&mut Parser) -> T) -> T {
    parser
        .parse_nested_block(|inner| Ok::<_, ParseError<()>>(read(inner)))
        .unwrap_or_default()
}

/// Skip the block `parser` has just opened.
fn skip_block(parser: &mut Parser<'_, '_>) {
    read_block(parser, |_| ());
}

// ---------------------------------------------------------------- internals

/// Could this selector text match anything the book actually contains?
fn selector_matches_used(selector_text: &str, used: &UsedSelectors) -> bool {
    if ALWAYS_KEEP.contains(&selector_text.trim()) {
        return true;
    }

    selector_text
        .split(',')
        .any(|selector| single_selector_matches(selector.trim(), used))
}

fn single_selector_matches(selector: &str, used: &UsedSelectors) -> bool {
    if ALWAYS_KEEP.contains(&selector) {
        return true;
    }

    // State-dependent and attribute selectors are beyond what a static scan of
    // the markup can decide, so they stay. So does an escaped name, which this
    // scan would misread: `.\31 st` is the class `1st`. So does anything else
    // that is not a plain run of names and combinators, a comment, a
    // namespace or something that is not a selector at all.
    if !selector.chars().all(is_plain_selector_char) {
        return true;
    }

    let mut saw_name = false;

    for (kind, name) in selector_names(selector) {
        saw_name = true;
        let matched = match kind {
            NameKind::Class => used.classes.contains(name),
            NameKind::Id => used.ids.contains(name),
            NameKind::Element => used.elements.contains(&name.to_ascii_lowercase()),
        };
        if matched {
            return true;
        }
    }

    // A selector naming nothing recognizable is kept rather than guessed at.
    !saw_name
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameKind {
    Class,
    Id,
    Element,
}

/// Pull the class, id and element names out of one simple selector sequence.
///
/// A name runs over CSS's name characters, which include every non-ASCII
/// character: `.kapitelüberschrift` is one class, not the class `kapitel`
/// followed by an element `berschrift`.
fn selector_names(selector: &str) -> Vec<(NameKind, &str)> {
    let mut names = Vec::new();
    let bytes = selector.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let kind = match bytes[i] {
            b'.' => {
                i += 1;
                NameKind::Class
            }
            b'#' => {
                i += 1;
                NameKind::Id
            }
            c if c.is_ascii_alphabetic() => NameKind::Element,
            _ => {
                i += 1;
                continue;
            }
        };

        let start = i;
        while i < bytes.len() && is_name_byte(bytes[i]) {
            i += 1;
        }

        if i > start {
            names.push((kind, &selector[start..i]));
        } else {
            i += 1;
        }
    }

    names
}

/// What a selector made only of names, `*` and combinators is written with.
fn is_plain_selector_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || !c.is_ascii()
        || matches!(c, '-' | '_' | '.' | '#' | '*' | '>' | '+' | '~')
        || c.is_ascii_whitespace()
}

/// Every byte of a multi-byte UTF-8 character is non-ASCII, so testing bytes
/// keeps such a character whole and the slices on character boundaries.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || !byte.is_ascii()
}
