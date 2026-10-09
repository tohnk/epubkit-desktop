//! Stylesheet cleanup: dropping rules nothing in the book matches, and
//! removing embedded fonts. The CSS half of `html_cleaner.py`.
//!
//! Rules are found with `cssparser`, the tokenizer of Firefox's and Servo's
//! style engines, and cut out of the text where they stand. Nothing else is
//! touched: what the reading engine gets is the book's own CSS, comments,
//! hacks and spelling included, less the rules that went. Reprinting a parsed
//! stylesheet rewords it — `cssutils`, which the reference used, into what it
//! understood, a modern CSS library into syntax older engines do not read.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::ops::Range;
use std::path::Path;

use std::sync::LazyLock;

use cssparser::{Delimiter, ParseError, Parser, ParserInput, Token};
use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};
use libxml::bindings::xmlNodePtr;
use libxml::tree::{Document, Node, NodeType};

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
            RuleKind::Style { selectors, .. } => {
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
        removed += edit_style_element(&content.doc, &style, embedded_font_cuts);
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
/// What an entity reference stands for is part of the CSS too, but cannot be
/// edited where it is written: `url(&cdn;cover.png)` is not `url(cover.png)`.
/// An edit beside one leaves it as it is. When an edit runs into one, the
/// style's entities are written out as what they stand for, into the text
/// before them, and edited with it; where there is no text, into new text put
/// in their place. One that cannot be read ([`can_be_read`]), one a doctype
/// that is never loaded declares, say, stays where it is, nothing is written
/// out past it, and it stops the edits that run into it, since what it hides
/// could change where they end. One declared to stand for nothing is read as
/// that.
pub(crate) fn edit_style_element(
    doc: &Document,
    style: &Node,
    edit: impl FnOnce(&str) -> Vec<Edit>,
) -> usize {
    let mut parts: Vec<StylePart> = Vec::new();
    let mut css = String::new();
    let mut known = HashMap::new();
    for node in style.get_child_nodes() {
        let (entity, readable) = match node.get_type() {
            Some(NodeType::TextNode | NodeType::CDataSectionNode) => (false, true),
            Some(NodeType::EntityRefNode) => (true, can_be_read(doc, &node, &mut known)),
            _ => continue,
        };
        let start = css.len();
        css.push_str(&node.get_content());
        parts.push(StylePart {
            node,
            range: start..css.len(),
            entity,
            readable,
        });
    }

    if parts.is_empty() {
        return 0;
    }
    let mut edits = edit(&css);

    // Entities are written out only when an edit runs into one that can be.
    let readable: Vec<&Range<usize>> = parts
        .iter()
        .filter(|part| part.entity && part.readable)
        .map(|part| &part.range)
        .collect();
    let write_out = run_into(&readable, &edits).contains(&true);

    // Where each part's text goes as it is edited: a text or CDATA part's into
    // itself. A readable entity's, when entities are written out, goes into
    // the text before it, or after it if it comes first, but never past an
    // entity that cannot be read, which keeps its place. Readable entities
    // with no text on their side of those go into new text, put before the
    // first of them; the slots past the parts' are those.
    let mut slots: Vec<Option<usize>> = parts
        .iter()
        .enumerate()
        .map(|(index, part)| (!part.entity).then_some(index))
        .collect();
    let mut new_texts: Vec<usize> = Vec::new();
    if write_out {
        let mut start = 0;
        while start < parts.len() {
            let end = parts[start..]
                .iter()
                .position(|part| part.entity && !part.readable)
                .map_or(parts.len(), |length| start + length);
            if start < end {
                let mut owner = match (start..end).find(|&index| !parts[index].entity) {
                    Some(first) => first,
                    None => {
                        new_texts.push(start);
                        parts.len() + new_texts.len() - 1
                    }
                };
                for index in start..end {
                    if parts[index].entity {
                        slots[index] = Some(owner);
                    } else {
                        owner = index;
                    }
                }
            }
            start = end + 1;
        }
    }

    // An edit that runs into an entity that stays is not made.
    let staying: Vec<&Range<usize>> = parts
        .iter()
        .zip(&slots)
        .filter(|(part, slot)| part.entity && slot.is_none())
        .map(|(part, _)| &part.range)
        .collect();
    let blocked = run_into(&staying, &edits);
    edits = edits
        .into_iter()
        .zip(blocked)
        .filter_map(|(edit, blocked)| (!blocked).then_some(edit))
        .collect();
    if edits.is_empty() {
        return 0;
    }

    // Which part a byte of `css` is in: the first to end after it. The end of
    // `css` is in the last. The parts are in order, so this is a search.
    let part_at = |at: usize| {
        parts
            .partition_point(|part| part.range.end <= at)
            .min(parts.len() - 1)
    };
    let mut contents = vec![String::new(); parts.len() + new_texts.len()];
    let keep = |contents: &mut [String], mut from: usize, to: usize| {
        while from < to {
            let part = part_at(from);
            let until = to.min(parts[part].range.end);
            if let Some(slot) = slots[part] {
                contents[slot].push_str(&css[from..until]);
            }
            from = until;
        }
    };

    let mut made = 0;
    let mut kept_from = 0;
    for (range, replacement) in &edits {
        // Never an entity that stays: an edit that ran into one is gone.
        let Some(slot) = slots[part_at(range.start)] else {
            continue;
        };
        keep(&mut contents, kept_from, range.start);
        contents[slot].push_str(replacement);
        kept_from = range.end;
        made += 1;
    }
    keep(&mut contents, kept_from, css.len());

    // New text first, while the entities it stands in for are still there to
    // put it before. Nothing in the tree has changed if one cannot be made.
    // None goes beside other text, so libxml2 has none to merge it into.
    let mut made_texts = Vec::new();
    for (index, &before) in new_texts.iter().enumerate() {
        let content = &contents[parts.len() + index];
        if content.is_empty() {
            continue;
        }
        let Ok(text) = Node::new_text(content, doc) else {
            return 0;
        };
        made_texts.push((before, text));
    }
    for (before, mut text) in made_texts {
        parts[before].node.clone().add_prev_sibling(&mut text).ok();
    }

    for ((part, slot), content) in parts.iter().zip(&slots).zip(contents) {
        let mut node = part.node.clone();
        if part.entity {
            // Written out into the text it went to.
            if slot.is_some() {
                node.unlink();
            }
        } else if content != css[part.range.clone()] {
            // Text and CDATA take what they are given as it reads.
            if content.is_empty() {
                node.unlink();
            } else {
                node.set_content(&content).ok();
            }
        }
    }

    made
}

/// One piece of a `<style>`'s CSS: a text or CDATA child, or an entity
/// reference, and where what it holds or stands for is in the CSS.
struct StylePart {
    node: Node,
    range: Range<usize>,
    entity: bool,
    /// Whether what it stands for can be read ([`can_be_read`]).
    readable: bool,
}

/// Whether what the entity `reference` stands for can be read, so written out
/// as text: the document declares it, as libxml2 shows by giving a reference
/// the declaration as its child, and one to an undeclared entity nothing; its
/// value is in the document, not in a file it names, which is never loaded;
/// and its value holds nothing but text and entities that can be read in turn.
/// Written out, an entity it refers to would be lost, and an element too,
/// whose text would become CSS.
///
/// `known` holds what is known of the declarations already looked through,
/// so that each is looked through once.
fn can_be_read(doc: &Document, reference: &Node, known: &mut HashMap<xmlNodePtr, bool>) -> bool {
    let Some(declaration) = reference.get_first_child() else {
        return false;
    };
    let key = declaration.node_ptr();
    if let Some(&readable) = known.get(&key) {
        return readable;
    }
    // An entity that refers to itself, which libxml2 refuses to read, would
    // end here.
    known.insert(key, false);

    // A declaration reads back as written: a value in quotes for one in the
    // document, a SYSTEM or PUBLIC identifier for one in a file.
    let in_document = doc
        .node_to_string(&declaration)
        .strip_prefix("<!ENTITY ")
        .and_then(|rest| rest.split_once(' '))
        .is_some_and(|(_, value)| value.starts_with(['"', '\'']));
    let readable = in_document
        && declaration
            .get_child_nodes()
            .iter()
            .all(|node| match node.get_type() {
                Some(NodeType::EntityRefNode) => can_be_read(doc, node, known),
                Some(NodeType::ElementNode) => false,
                _ => true,
            });
    known.insert(key, readable);
    readable
}

/// Whether each of `edits` runs into one of `entities`: takes in some of what
/// one stands for, or, for one that stands for nothing that can be read, has
/// it inside. Both are in order, so each entity is passed once.
fn run_into(entities: &[&Range<usize>], edits: &[Edit]) -> Vec<bool> {
    let wholly_before = |entity: &Range<usize>, edit: &Range<usize>| {
        if entity.is_empty() {
            entity.start <= edit.start
        } else {
            entity.end <= edit.start
        }
    };

    let mut next = 0;
    edits
        .iter()
        .map(|(edit, _)| {
            while next < entities.len() && wholly_before(entities[next], edit) {
                next += 1;
            }
            // The first entity that is not wholly before the edit is in it,
            // if it starts before the edit ends.
            entities
                .get(next)
                .is_some_and(|entity| entity.start < edit.end)
        })
        .collect()
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
        if let RuleKind::At { name, children, .. } = rule.kind {
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
    /// A style rule, where its selectors are written, and where its
    /// declarations are, inside its braces.
    Style {
        selectors: Range<usize>,
        declarations: Range<usize>,
    },
    /// An at-rule, its name lowercased, where what follows the name is
    /// written, up to its block or its end, and the rules inside it, if it
    /// is a grouping rule read into.
    At {
        name: String,
        prelude: Range<usize>,
        children: Vec<Rule>,
    },
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
                let prelude_start = parser.position().byte_index();
                let mut prelude = prelude_start..prelude_start;
                let mut children = Vec::new();
                loop {
                    prelude.end = parser.position().byte_index();
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
                RuleKind::At {
                    name,
                    prelude,
                    children,
                }
            }
            first => {
                // A style rule's selectors run up to its block. Skipping the
                // blanks before each token also gets past the inside of a
                // bracket the last one opened, so the selectors end after it.
                let mut selectors_end = start;
                let mut declarations = start..start;
                let mut token = first;
                loop {
                    if matches!(token, Token::CurlyBracketBlock) {
                        let open = parser.position();
                        skip_block(parser);
                        // Less the closing brace, which a block left open at
                        // the end does not have.
                        let block = parser.slice_from(open);
                        let length = block.strip_suffix('}').unwrap_or(block).len();
                        declarations = open.byte_index()..open.byte_index() + length;
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
                    declarations,
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

// ------------------------------------------------------- how boxes are sized

/// What the CSS styling an element says of the size of its box, as far as
/// Light Novel mode needs to know: whether an image shown in it, or as it,
/// would fit there turned, or as the pages of a split one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BoxSizing {
    /// A `height` or `max-height` that is a length of its own, in pixels,
    /// ems and the like, which the box cannot grow past.
    pub fixed_height: bool,
    /// A `height` or `max-height` that is a share of the screen, in `vh` and
    /// the like.
    pub screen_height: bool,
    /// A `width` other than `auto`.
    pub width: bool,
    /// An `aspect-ratio`, which keeps the box one shape.
    pub aspect_ratio: bool,
    /// `position: absolute` or `fixed`, which lays the box over whatever else
    /// is there.
    pub positioned: bool,
    /// A `transform` or `rotate`, which turns, slants or moves what the box
    /// shows.
    pub transformed: bool,
}

impl BoxSizing {
    /// Add what `other` says to what this says.
    pub(crate) fn add(&mut self, other: BoxSizing) {
        self.fixed_height |= other.fixed_height;
        self.screen_height |= other.screen_height;
        self.width |= other.width;
        self.aspect_ratio |= other.aspect_ratio;
        self.positioned |= other.positioned;
        self.transformed |= other.transformed;
    }
}

/// A book's rules that size boxes: what each says, and what its selectors
/// ask of the elements it styles.
#[derive(Debug, Clone, Default)]
pub(crate) struct BoxRules {
    rules: Vec<(Vec<Subject>, BoxSizing)>,
}

impl BoxRules {
    /// Add the rules of `css` that size boxes. Those in `@media` and other
    /// grouping rules count, as they hold on some screen, but for those only
    /// for print.
    pub(crate) fn read(&mut self, css: &str) {
        self.add(css, rules(css, MAX_NESTING));
    }

    fn add(&mut self, css: &str, rules: Vec<Rule>) {
        for rule in rules {
            match rule.kind {
                RuleKind::Style {
                    selectors,
                    declarations,
                } => {
                    let sizing = declared_sizing(&css[declarations]);
                    if sizing != BoxSizing::default() {
                        self.rules.push((subjects(&css[selectors]), sizing));
                    }
                }
                RuleKind::At {
                    name,
                    prelude,
                    children,
                } => {
                    if !(name == "media" && only_for_print(&css[prelude])) {
                        self.add(css, children);
                    }
                }
            }
        }
    }

    /// What the rules say of `element`'s box: everything any rule that could
    /// style it says, whether or not another says otherwise.
    pub(crate) fn sizing(&self, element: &Node) -> BoxSizing {
        let mut sizing = BoxSizing::default();
        if self.rules.is_empty() {
            return sizing;
        }

        let name = element
            .get_name()
            .rsplit(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let class = element.get_attribute_no_ns("class").unwrap_or_default();
        let classes: Vec<&str> = class.split_ascii_whitespace().collect();
        let id = element.get_attribute_no_ns("id");
        for (subjects, said) in &self.rules {
            if subjects
                .iter()
                .any(|subject| subject.could_be(&name, &classes, id.as_deref()))
            {
                sizing.add(*said);
            }
        }
        sizing
    }
}

/// What a list of declarations, a rule's or a `style` attribute's, says of
/// the size of the box it styles.
pub(crate) fn declared_sizing(declarations: &str) -> BoxSizing {
    let mut sizing = BoxSizing::default();
    let mut input = ParserInput::new(declarations);
    let mut parser = Parser::new(&mut input);

    while !parser.is_exhausted() {
        // A declaration that cannot be read is passed over, as CSS passes
        // over it.
        let _ = parser.parse_until_after(Delimiter::Semicolon, |declaration| {
            let property = declaration.expect_ident()?.to_ascii_lowercase();
            declaration.expect_colon()?;
            match property.as_str() {
                "height" | "max-height" | "block-size" | "max-block-size" => {
                    match height(declaration, true) {
                        Height::Fixed => sizing.fixed_height = true,
                        Height::Screen => sizing.screen_height = true,
                        Height::Open => {}
                    }
                }
                "width" | "inline-size" => sizing.width |= is_set(declaration),
                "aspect-ratio" => sizing.aspect_ratio |= is_set(declaration),
                "position" => sizing.positioned |= lays_over(declaration),
                "transform" | "-webkit-transform" | "-moz-transform" | "-ms-transform"
                | "-o-transform" | "rotate" => sizing.transformed |= is_set(declaration),
                _ => while declaration.next().is_ok() {},
            }
            Ok::<_, ParseError<()>>(())
        });
    }

    sizing
}

/// How a `height` or `max-height` sizes a box.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Height {
    /// As what is in the box, or what the box is in, has it: `auto`, `none`,
    /// a percentage.
    #[default]
    Open,
    /// A share of the screen.
    Screen,
    /// A length of its own.
    Fixed,
}

/// The units that are shares of the screen, the viewport.
const SCREEN_UNITS: &[&str] = &[
    "vw", "vh", "vi", "vb", "vmin", "vmax", "svw", "svh", "svi", "svb", "svmin", "svmax", "lvw",
    "lvh", "lvi", "lvb", "lvmin", "lvmax", "dvw", "dvh", "dvi", "dvb", "dvmin", "dvmax",
];

/// How the value `parser` holds sizes a box's height: as its most fixed
/// length does, in `calc()` and the like too. A bare number `outside` them is
/// a length, `0` or pixels to an engine that takes it so.
fn height(parser: &mut Parser<'_, '_>, outside: bool) -> Height {
    let mut most = Height::Open;
    loop {
        let this = match parser.next() {
            Err(_) => break,
            Ok(Token::Dimension { unit, .. })
                if SCREEN_UNITS
                    .iter()
                    .any(|screen| unit.eq_ignore_ascii_case(screen)) =>
            {
                Height::Screen
            }
            Ok(Token::Dimension { .. }) => Height::Fixed,
            Ok(Token::Number { .. }) if outside => Height::Fixed,
            Ok(Token::Function(_) | Token::ParenthesisBlock) => {
                read_block(parser, |inner| height(inner, false))
            }
            Ok(_) => Height::Open,
        };
        most = most.max(this);
    }
    most
}

/// Words that leave a box as it would be: `auto`, `none`, the keywords every
/// property takes, and the `important` of `!important`.
const LEFT_AS_IS: &[&str] = &[
    "auto",
    "none",
    "initial",
    "inherit",
    "unset",
    "revert",
    "revert-layer",
    "important",
];

/// Does the value `parser` holds set something, rather than leave the box
/// as it would be?
fn is_set(parser: &mut Parser<'_, '_>) -> bool {
    let mut set = false;
    while let Ok(token) = parser.next() {
        set |= !matches!(token, Token::Delim('!'))
            && !matches!(token, Token::Ident(word)
                if LEFT_AS_IS.iter().any(|left| word.eq_ignore_ascii_case(left)));
    }
    set
}

/// Does the `position` `parser` holds lay the box over the page, out of the
/// run of what else is there?
fn lays_over(parser: &mut Parser<'_, '_>) -> bool {
    let mut over = false;
    while let Ok(token) = parser.next() {
        over |= matches!(token, Token::Ident(word)
            if word.eq_ignore_ascii_case("absolute") || word.eq_ignore_ascii_case("fixed"));
    }
    over
}

/// Is a media query list, what follows `@media`, only for print? Each of its
/// queries has to be.
fn only_for_print(queries: &str) -> bool {
    queries.split(',').all(|query| {
        let query = query.trim().to_ascii_lowercase();
        let query = query.strip_prefix("only ").unwrap_or(&query).trim_start();
        query == "print" || query.starts_with("print ")
    })
}

/// What the last compound of a selector, the one naming the element it
/// styles, asks of it: its name, its classes and its id. Whatever else it
/// asks, an attribute or a place among its siblings, and whatever the rest of
/// the selector asks of the elements around it, are taken to hold.
#[derive(Debug, Clone, Default)]
struct Subject {
    name: Option<String>,
    classes: Vec<String>,
    ids: Vec<String>,
}

impl Subject {
    /// Could an element of this `name`, `classes` and `id` be the subject?
    fn could_be(&self, name: &str, classes: &[&str], id: Option<&str>) -> bool {
        self.name.as_deref().is_none_or(|wanted| wanted == name)
            && self
                .classes
                .iter()
                .all(|class| classes.contains(&class.as_str()))
            && self.ids.iter().all(|wanted| id == Some(wanted.as_str()))
    }
}

/// The pseudo-elements CSS 2 wrote with one colon.
const LEGACY_PSEUDO_ELEMENTS: &[&str] = &["before", "after", "first-line", "first-letter"];

/// The pseudo-classes of something a reader does, which a page as it is shown
/// is not: hovered over, focused, followed.
const ACTION_PSEUDO_CLASSES: &[&str] = &[
    "hover",
    "active",
    "focus",
    "focus-visible",
    "focus-within",
    "target",
    "visited",
];

/// The subjects of a selector list's selectors, but for those that style no
/// element as it is shown: a pseudo-element's box, `::before` say, or one
/// while it is hovered over. A list with a selector CSS cannot read, a hash
/// that is no id, has none, as CSS then drops the rule.
fn subjects(selectors: &str) -> Vec<Subject> {
    let mut input = ParserInput::new(selectors);
    let mut parser = Parser::new(&mut input);
    let mut subjects = Vec::new();
    let mut subject = Subject::default();
    // Whether the selector read so far styles no element as it is shown.
    let mut styles_none = false;
    // Whether a combinator follows the compound read so far, which is then
    // not the subject.
    let mut combined = false;
    let mut after_dot = false;
    let mut colons = 0;

    loop {
        let token = match parser.next_including_whitespace() {
            Ok(token) => token.clone(),
            Err(_) => break,
        };
        match token {
            Token::Comma => {
                let read = std::mem::take(&mut subject);
                if !std::mem::take(&mut styles_none) {
                    subjects.push(read);
                }
                (combined, after_dot, colons) = (false, false, 0);
                continue;
            }
            Token::WhiteSpace(_) | Token::Delim('>' | '+' | '~') => {
                combined = true;
                continue;
            }
            _ => {}
        }
        if std::mem::take(&mut combined) {
            subject = Subject::default();
        }

        let class = std::mem::take(&mut after_dot);
        let pseudo = std::mem::take(&mut colons);
        match token {
            Token::Ident(name) if class => subject.classes.push(name.to_string()),
            Token::Ident(name) if pseudo > 0 => {
                let is =
                    |names: &[&str]| names.iter().any(|known| name.eq_ignore_ascii_case(known));
                if pseudo > 1 || is(LEGACY_PSEUDO_ELEMENTS) || is(ACTION_PSEUDO_CLASSES) {
                    styles_none = true;
                } else if name.eq_ignore_ascii_case("root") {
                    subject.name = Some("html".to_string());
                }
            }
            // `:not()`, `:is()` and the like are taken to hold; a function
            // after two colons is a pseudo-element.
            Token::Function(_) => styles_none |= pseudo > 1,
            Token::Ident(name) => subject.name = Some(name.to_ascii_lowercase()),
            // `*`, or `|` after a namespace, which the name follows.
            Token::Delim('*' | '|') => subject.name = None,
            Token::Delim('.') => after_dot = true,
            Token::IDHash(id) => subject.ids.push(id.to_string()),
            Token::Hash(_) => return Vec::new(),
            Token::Colon => colons = pseudo + 1,
            _ => {}
        }
    }
    if !styles_none {
        subjects.push(subject);
    }
    subjects
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What each way of writing a size says of a box.
    #[test]
    fn declarations_size_boxes_by_what_they_set() {
        let fixed = BoxSizing {
            fixed_height: true,
            ..BoxSizing::default()
        };
        let screen = BoxSizing {
            screen_height: true,
            ..BoxSizing::default()
        };
        let open = BoxSizing::default();
        let cases = [
            ("height: 200px", fixed),
            ("max-height:12em", fixed),
            ("block-size: 3in", fixed),
            ("height: 0", fixed),
            ("height: calc(100% - 2em)", fixed),
            ("height: 100vh", screen),
            ("max-height: 95dvh !important", screen),
            ("height: 100%", open),
            ("height: auto !important", open),
            ("max-height: none", open),
            ("height: calc(100% * 0.5)", open),
            ("height: var(--page)", open),
            ("min-height: 10em", open),
            (
                "width: auto; position: relative; transform: none; aspect-ratio: auto",
                open,
            ),
            // One that cannot be read is passed over, and the next read.
            ("height 200px; HEIGHT: 4EM", fixed),
        ];
        for (declarations, expected) in cases {
            assert_eq!(declared_sizing(declarations), expected, "{declarations}");
        }

        let set = declared_sizing(
            "width: 100%; aspect-ratio: 5 / 2; position: absolute; -webkit-transform: rotate(90deg)",
        );
        assert!(set.width && set.aspect_ratio && set.positioned && set.transformed);
        assert!(declared_sizing("rotate: 90deg").transformed);
        assert!(declared_sizing("position: fixed").positioned);
    }

    /// Which elements a rule is taken to style: those its selector's last
    /// compound could name, whatever is around them, but no pseudo-element,
    /// no state a page as shown is not in, and nothing for print only.
    #[test]
    fn rules_size_the_elements_their_selectors_could_name() {
        let content = html::parse_content(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body>
<div id="frame" class="frame wide"><p id="text" class="frame">Text</p><img id="image" src="a.png" alt=""/></div>
</body></html>"#,
        )
        .unwrap();
        let element = |id: &str| {
            xml::find_nodes(&content.doc, &format!("//*[@id='{id}']"))
                .unwrap()
                .remove(0)
        };
        let html = xml::find_nodes(&content.doc, "/*").unwrap().remove(0);
        let framed = |css: &str, node: &Node| {
            let mut rules = BoxRules::default();
            rules.read(css);
            rules.sizing(node).fixed_height
        };

        let (div, p, img) = (element("frame"), element("text"), element("image"));
        assert!(framed(".frame { height: 2em }", &div));
        assert!(framed(".frame.wide { height: 2em }", &div));
        assert!(!framed(".frame.narrow { height: 2em }", &div));
        assert!(framed("div.frame { height: 2em }", &div));
        assert!(!framed("div.frame { height: 2em }", &p));
        assert!(framed("#frame { height: 2em }", &div));
        assert!(!framed("#frame { height: 2em }", &p));
        assert!(framed("body section > .frame + img { height: 2em }", &img));
        assert!(framed("p, IMG { height: 2em }", &img));
        assert!(framed("*|img { height: 2em }", &img));
        assert!(framed(
            "@media screen, print { @supports (display: grid) { img { height: 2em } } }",
            &img
        ));

        assert!(!framed("div::before, div:after { height: 2em }", &div));
        assert!(framed("div::before, div { height: 2em }", &div));
        assert!(!framed("img:hover, a:focus img { height: 2em }", &img));
        assert!(framed(
            "img:first-child, img:not(.wide) { height: 2em }",
            &img
        ));
        assert!(!framed("@media print { img { height: 2em } } @media only print and (color) { img { height: 2em } }", &img));
        assert!(!framed("#1a, img { height: 2em }", &img));
        assert!(!framed(":root { height: 2em }", &div));
        assert!(framed(":root { height: 2em }", &html));
        assert!(!framed(
            "@page { height: 2em } @font-face { height: 2em }",
            &img
        ));
    }
}
