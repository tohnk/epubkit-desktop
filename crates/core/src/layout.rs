//! How a book's CSS lays out the boxes its images are shown in, as far as
//! Light Novel mode needs to know: whether an image turned, or split into
//! pages, would still fit where it is shown.
//!
//! This is the cascade, read for a few properties: selectors matched against
//! the chapter, specificity, order and `!important`, `style` attributes and
//! the presentational `height` of tables and the like, `@media` and
//! `@import`. It is no browser, and what it cannot know, a pseudo-class it
//! does not read, a media query of the screen's size, where an element will
//! stand once an image's pages are added beside it, it takes to hold or not
//! as either would frame an image: a rule that only perhaps applies can frame
//! one, but never stops another from framing it.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use cssparser::{Delimiter, ParseError, Parser, ParserInput, Token};
use libxml::bindings::xmlNodePtr;
use libxml::tree::{Node, NodeType};

use crate::css::{self, RuleKind};

/// Whether something holds where the book is read: surely not, perhaps, or
/// surely. Ordered so that `and` is the least and `or` the most of two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Tri {
    No,
    Maybe,
    Yes,
}

impl Tri {
    fn of(holds: bool) -> Tri {
        if holds {
            Tri::Yes
        } else {
            Tri::No
        }
    }

    pub(crate) fn and(self, other: Tri) -> Tri {
        self.min(other)
    }

    fn or(self, other: Tri) -> Tri {
        self.max(other)
    }

    fn not(self) -> Tri {
        match self {
            Tri::No => Tri::Yes,
            Tri::Maybe => Tri::Maybe,
            Tri::Yes => Tri::No,
        }
    }
}

/// How deep into `calc()` and the like a value is read. One nested deeper
/// could be anything.
const MAX_VALUE_NESTING: usize = 32;

/// How many compounds a selector may have, `a b c` three, to be matched. One
/// with more could match anything.
const MAX_COMPOUNDS: usize = 32;

// ------------------------------------------------------------------ media

/// Whether a media query list holds on a reader's screen: surely for none,
/// `all` or `screen`; surely not for print, speech and the other kinds of
/// device; perhaps for a query of the screen's size or shape, or a kind this
/// does not know, `amzn-kf8` say.
pub(crate) fn media_applies(queries: &str) -> Tri {
    let queries = without_comments(queries);
    if queries.trim().is_empty() {
        return Tri::Yes;
    }
    queries
        .split(',')
        .map(|query| {
            let query = query.trim().to_ascii_lowercase();
            let mut words = query.split_ascii_whitespace().peekable();
            let negated = words.next_if_eq(&"not").is_some();
            words.next_if_eq(&"only");
            let rest: Vec<&str> = words.collect();
            let (kind, condition) = match rest.first() {
                None => return Tri::Maybe,
                Some(first) if first.starts_with('(') => ("all", true),
                Some(first) => (*first, rest.len() > 1),
            };
            let kind = match kind {
                "all" | "screen" => Tri::Yes,
                "print" | "speech" | "aural" | "braille" | "embossed" | "tty" | "tv"
                | "projection" | "handheld" => Tri::No,
                _ => Tri::Maybe,
            };
            let holds = if condition {
                kind.and(Tri::Maybe)
            } else {
                kind
            };
            if negated {
                holds.not()
            } else {
                holds
            }
        })
        .fold(Tri::No, Tri::or)
}

/// `text` without its CSS comments.
fn without_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        out.push(' ');
        rest = rest[start + 2..]
            .find("*/")
            .map_or("", |end| &rest[start + 2 + end + 2..]);
    }
    out.push_str(rest);
    out
}

// -------------------------------------------------------------- selectors

/// How specific a selector is: its ids, its classes, attributes and
/// pseudo-classes, and its element names and pseudo-elements.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Specificity(u32, u32, u32);

impl std::ops::Add for Specificity {
    type Output = Specificity;
    fn add(self, other: Specificity) -> Specificity {
        Specificity(
            self.0.saturating_add(other.0),
            self.1.saturating_add(other.1),
            self.2.saturating_add(other.2),
        )
    }
}

/// One selector of a list: its compounds from left to right, each but the
/// first after a combinator.
#[derive(Debug, Clone)]
struct Complex {
    compounds: Vec<Compound>,
    combinators: Vec<Combinator>,
    specificity: Specificity,
}

#[derive(Debug, Clone, Copy)]
enum Combinator {
    /// Whitespace: inside the compound before, at any depth.
    Descendant,
    /// `>`: just inside it.
    Child,
    /// `+`: just after it.
    Next,
    /// `~`: after it, among the same parent's children.
    Later,
}

#[derive(Debug, Clone, Default)]
struct Compound {
    /// The element name asked for, as written; `None` for any.
    name: Option<String>,
    /// Whether a namespace is asked for, which is not looked into.
    namespaced: bool,
    ids: Vec<String>,
    classes: Vec<String>,
    attributes: Vec<Attribute>,
    pseudo_classes: Vec<PseudoClass>,
    /// Whether it names a pseudo-element, whose box is never the element's.
    pseudo_element: bool,
}

#[derive(Debug, Clone)]
struct Attribute {
    name: String,
    namespaced: bool,
    test: Option<(AttributeTest, String)>,
    case_insensitive: bool,
}

#[derive(Debug, Clone, Copy)]
enum AttributeTest {
    Equals,
    Includes,
    DashMatch,
    Prefix,
    Suffix,
    Substring,
}

#[derive(Debug, Clone)]
enum PseudoClass {
    Not(Vec<Complex>),
    Is(Vec<Complex>),
    /// `:where()`, which counts for nothing in specificity.
    Where(Vec<Complex>),
    Nth {
        a: i32,
        b: i32,
        of_type: bool,
        from_end: bool,
        of: Option<Vec<Complex>>,
    },
    Root,
    Empty,
    Link,
    Lang(Vec<String>),
    /// One a page as it is shown is never in: hovered over, focused, visited.
    Never,
    /// One this does not read, which may hold.
    Unknown,
}

/// The selectors of a rule, as its prelude writes them. A list this cannot
/// read may be one a reader can, and stands for a selector that may match
/// any element, ahead of any other.
fn parse_selectors(text: &str) -> Vec<Complex> {
    let mut input = ParserInput::new(text);
    let mut parser = Parser::new(&mut input);
    match selector_list(&mut parser, false, css::MAX_NESTING) {
        Some(list) if !list.is_empty() && parser.is_exhausted() => list,
        _ => vec![Complex::anything()],
    }
}

/// A list of selectors to the end of `input`, `depth` levels of `:not()`
/// and the like deep at most. One `:is()` takes forgives a selector it
/// cannot read; any other list is lost with it.
fn selector_list(
    input: &mut Parser<'_, '_>,
    forgiving: bool,
    depth: usize,
) -> Option<Vec<Complex>> {
    let mut list = Vec::new();
    loop {
        let read = input.parse_until_before(Delimiter::Comma, |input| {
            complex(input, depth).ok_or_else(|| input.new_custom_error::<(), ()>(()))
        });
        match read {
            Ok(complex) => list.push(complex),
            Err(_) if forgiving => {}
            Err(_) => return None,
        }
        match input.next() {
            Ok(Token::Comma) => continue,
            Ok(_) => return None,
            Err(_) => return Some(list),
        }
    }
}

fn complex(input: &mut Parser<'_, '_>, depth: usize) -> Option<Complex> {
    let mut compounds = Vec::new();
    let mut combinators = Vec::new();
    input.skip_whitespace();
    loop {
        compounds.push(compound(input, depth)?);
        if compounds.len() > MAX_COMPOUNDS {
            return None;
        }
        let mut spaced = false;
        let combinator = loop {
            let state = input.state();
            let token = match input.next_including_whitespace() {
                Ok(token) => token.clone(),
                Err(_) => {
                    let specificity = compounds
                        .iter()
                        .map(Compound::specificity)
                        .fold(Specificity::default(), |sum, each| sum + each);
                    return Some(Complex {
                        compounds,
                        combinators,
                        specificity,
                    });
                }
            };
            match token {
                Token::WhiteSpace(_) => spaced = true,
                Token::Delim('>') => break Combinator::Child,
                Token::Delim('+') => break Combinator::Next,
                Token::Delim('~') => break Combinator::Later,
                _ if spaced => {
                    input.reset(&state);
                    break Combinator::Descendant;
                }
                _ => return None,
            }
        };
        input.skip_whitespace();
        combinators.push(combinator);
    }
}

fn compound(input: &mut Parser<'_, '_>, depth: usize) -> Option<Compound> {
    let mut compound = Compound::default();
    let mut read = false;

    // An element name or `*`, after a namespace or not.
    let state = input.state();
    match input.next_including_whitespace().cloned() {
        Ok(Token::Ident(name)) => {
            compound.name = Some(name.to_string());
            element_after_namespace(input, &mut compound)?;
            read = true;
        }
        Ok(Token::Delim('*')) => {
            element_after_namespace(input, &mut compound)?;
            read = true;
        }
        Ok(Token::Delim('|')) => {
            compound.namespaced = true;
            compound.name = element_name_after_namespace(input)?;
            read = true;
        }
        _ => input.reset(&state),
    }

    loop {
        let state = input.state();
        let Ok(token) = input.next_including_whitespace().cloned() else {
            break;
        };
        match token {
            Token::IDHash(id) => compound.ids.push(id.to_string()),
            // `#1st` is no id.
            Token::Hash(_) => return None,
            Token::Delim('.') => match input.next_including_whitespace() {
                Ok(Token::Ident(class)) => compound.classes.push(class.to_string()),
                _ => return None,
            },
            // Outside a nested rule, `&` is the root, or what scopes a rule.
            Token::Delim('&') => compound.pseudo_classes.push(PseudoClass::Unknown),
            Token::SquareBracketBlock => {
                let attribute = input
                    .parse_nested_block(|inner| {
                        attribute(inner).ok_or_else(|| inner.new_custom_error::<(), ()>(()))
                    })
                    .ok()?;
                compound.attributes.push(attribute);
            }
            Token::Colon => pseudo(input, &mut compound, depth)?,
            _ => {
                input.reset(&state);
                break;
            }
        }
        read = true;
    }
    read.then_some(compound)
}

/// After a name or `*`: if a `|` follows, that was the namespace, and the
/// element's name comes after it.
fn element_after_namespace(input: &mut Parser<'_, '_>, compound: &mut Compound) -> Option<()> {
    let state = input.state();
    if let Ok(Token::Delim('|')) = input.next_including_whitespace() {
        compound.namespaced = compound.name.is_some();
        compound.name = element_name_after_namespace(input)?;
    } else {
        input.reset(&state);
    }
    Some(())
}

/// An element's name after a namespace: `None` for `*`.
fn element_name_after_namespace(input: &mut Parser<'_, '_>) -> Option<Option<String>> {
    match input.next_including_whitespace().ok()? {
        Token::Ident(name) => Some(Some(name.to_string())),
        Token::Delim('*') => Some(None),
        _ => None,
    }
}

fn attribute(input: &mut Parser<'_, '_>) -> Option<Attribute> {
    input.skip_whitespace();
    let mut namespaced = false;
    let name = match input.next_including_whitespace().ok()?.clone() {
        Token::Ident(name) => {
            let state = input.state();
            if let Ok(Token::Delim('|')) = input.next_including_whitespace() {
                namespaced = true;
                match input.next_including_whitespace().ok()? {
                    Token::Ident(name) => name.to_string(),
                    _ => return None,
                }
            } else {
                input.reset(&state);
                name.to_string()
            }
        }
        Token::Delim('*') | Token::Delim('|') => {
            namespaced = true;
            let state = input.state();
            if !matches!(input.next_including_whitespace(), Ok(Token::Delim('|'))) {
                input.reset(&state);
            }
            match input.next_including_whitespace().ok()? {
                Token::Ident(name) => name.to_string(),
                _ => return None,
            }
        }
        _ => return None,
    };

    input.skip_whitespace();
    if input.is_exhausted() {
        return Some(Attribute {
            name,
            namespaced,
            test: None,
            case_insensitive: false,
        });
    }
    let test = match input.next().ok()? {
        Token::Delim('=') => AttributeTest::Equals,
        Token::IncludeMatch => AttributeTest::Includes,
        Token::DashMatch => AttributeTest::DashMatch,
        Token::PrefixMatch => AttributeTest::Prefix,
        Token::SuffixMatch => AttributeTest::Suffix,
        Token::SubstringMatch => AttributeTest::Substring,
        _ => return None,
    };
    let value = match input.next().ok()? {
        Token::Ident(value) | Token::QuotedString(value) => value.to_string(),
        _ => return None,
    };
    let case_insensitive = match input.next() {
        Err(_) => false,
        Ok(Token::Ident(flag)) if flag.eq_ignore_ascii_case("i") => true,
        Ok(Token::Ident(flag)) if flag.eq_ignore_ascii_case("s") => false,
        Ok(_) => return None,
    };
    input.is_exhausted().then_some(Attribute {
        name,
        namespaced,
        test: Some((test, value)),
        case_insensitive,
    })
}

/// The pseudo-class or pseudo-element after a `:`.
fn pseudo(input: &mut Parser<'_, '_>, compound: &mut Compound, depth: usize) -> Option<()> {
    let nth = |a, b, of_type, from_end| PseudoClass::Nth {
        a,
        b,
        of_type,
        from_end,
        of: None,
    };
    match input.next_including_whitespace().ok()?.clone() {
        Token::Colon => {
            match input.next_including_whitespace().ok()?.clone() {
                Token::Ident(_) => {}
                Token::Function(_) => skip_block(input)?,
                _ => return None,
            }
            compound.pseudo_element = true;
        }
        Token::Ident(name) => {
            let classes: Vec<PseudoClass> = match name.to_ascii_lowercase().as_str() {
                // The pseudo-elements CSS 2 wrote with one colon.
                "before" | "after" | "first-line" | "first-letter" => {
                    compound.pseudo_element = true;
                    return Some(());
                }
                "root" => vec![PseudoClass::Root],
                "empty" => vec![PseudoClass::Empty],
                "link" | "any-link" | "-webkit-any-link" => vec![PseudoClass::Link],
                "first-child" => vec![nth(0, 1, false, false)],
                "last-child" => vec![nth(0, 1, false, true)],
                "only-child" => vec![nth(0, 1, false, false), nth(0, 1, false, true)],
                "first-of-type" => vec![nth(0, 1, true, false)],
                "last-of-type" => vec![nth(0, 1, true, true)],
                "only-of-type" => vec![nth(0, 1, true, false), nth(0, 1, true, true)],
                "hover" | "active" | "focus" | "focus-visible" | "focus-within" | "target"
                | "target-within" | "visited" | "current" | "past" | "future" | "playing"
                | "paused" => vec![PseudoClass::Never],
                _ => vec![PseudoClass::Unknown],
            };
            compound.pseudo_classes.extend(classes);
        }
        Token::Function(name) => {
            if depth == 0 {
                return None;
            }
            let name = name.to_ascii_lowercase();
            let class = input
                .parse_nested_block(|inner| {
                    functional(&name, inner, depth - 1)
                        .ok_or_else(|| inner.new_custom_error::<(), ()>(()))
                })
                .ok()?;
            compound.pseudo_classes.push(class);
        }
        _ => return None,
    }
    Some(())
}

/// A pseudo-class written as a function, `name(…)`, its argument `input`.
fn functional(name: &str, input: &mut Parser<'_, '_>, depth: usize) -> Option<PseudoClass> {
    let nth = |input: &mut Parser<'_, '_>, of_type, from_end, of_allowed| {
        input.skip_whitespace();
        let (a, b) = cssparser::parse_nth(input).ok()?;
        input.skip_whitespace();
        let of = if of_allowed && input.try_parse(|i| i.expect_ident_matching("of")).is_ok() {
            Some(selector_list(input, false, depth)?)
        } else {
            None
        };
        input.is_exhausted().then_some(PseudoClass::Nth {
            a,
            b,
            of_type,
            from_end,
            of,
        })
    };
    match name {
        "not" => Some(PseudoClass::Not(selector_list(input, false, depth)?)),
        "is" | "matches" | "-webkit-any" | "-moz-any" => {
            Some(PseudoClass::Is(selector_list(input, true, depth)?))
        }
        "where" => Some(PseudoClass::Where(selector_list(input, true, depth)?)),
        "nth-child" => nth(input, false, false, true),
        "nth-last-child" => nth(input, false, true, true),
        "nth-of-type" => nth(input, true, false, false),
        "nth-last-of-type" => nth(input, true, true, false),
        "lang" => {
            let mut ranges = Vec::new();
            loop {
                match input.next() {
                    Ok(Token::Ident(range) | Token::QuotedString(range)) => {
                        ranges.push(range.to_ascii_lowercase())
                    }
                    Ok(Token::Comma) => {}
                    Ok(_) => return None,
                    Err(_) => break,
                }
            }
            Some(PseudoClass::Lang(ranges))
        }
        _ => {
            while input.next().is_ok() {}
            Some(PseudoClass::Unknown)
        }
    }
}

/// Step over the block `input` has just opened.
fn skip_block(input: &mut Parser<'_, '_>) -> Option<()> {
    input
        .parse_nested_block(|inner| {
            while inner.next().is_ok() {}
            Ok::<_, ParseError<()>>(())
        })
        .ok()
}

/// What reshaping a chapter's images may change in it, which matching takes
/// as not known: which siblings an element has where an image's pages are
/// added, and the pages themselves. Each keeps the image's attributes but
/// its size, `srcset` and `sizes`, and its id but on the first; its `src`
/// names a file of its own.
#[derive(Debug, Default)]
pub(crate) struct Reshaping {
    /// The elements whose children may change.
    pub(crate) changing: HashSet<xmlNodePtr>,
    /// The elements that stand for the pages of an image.
    pub(crate) pages: HashSet<xmlNodePtr>,
}

/// What matching a selector goes by: what reshaping images may change, and
/// how much matching is left. Past that, what is left to match may hold or
/// not.
#[derive(Clone, Copy)]
struct Matching<'a> {
    reshaping: &'a Reshaping,
    budget: &'a Cell<u64>,
}

impl Matching<'_> {
    /// Take a step from the budget: `false` once it is spent.
    fn step(&self) -> bool {
        let left = self.budget.get();
        self.budget.set(left.saturating_sub(1));
        left > 0
    }

    fn spent(&self) -> bool {
        self.budget.get() == 0
    }

    /// May the siblings `element` has change?
    fn moves(&self, element: &Node) -> bool {
        parent_element(element)
            .is_some_and(|parent| self.reshaping.changing.contains(&parent.node_ptr()))
    }

    fn is_page(&self, element: &Node) -> bool {
        self.reshaping.pages.contains(&element.node_ptr())
    }
}

impl Compound {
    fn specificity(&self) -> Specificity {
        let most = |list: &[Complex]| {
            list.iter()
                .map(|complex| complex.specificity)
                .max()
                .unwrap_or_default()
        };
        let pseudo_classes = self
            .pseudo_classes
            .iter()
            .map(|class| match class {
                PseudoClass::Not(list) | PseudoClass::Is(list) => most(list),
                PseudoClass::Where(_) => Specificity::default(),
                PseudoClass::Nth { of: Some(list), .. } => Specificity(0, 1, 0) + most(list),
                _ => Specificity(0, 1, 0),
            })
            .fold(Specificity::default(), |sum, each| sum + each);
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        Specificity(
            count(self.ids.len()),
            count(self.classes.len() + self.attributes.len()),
            u32::from(self.name.is_some()) + u32::from(self.pseudo_element),
        ) + pseudo_classes
    }

    fn matches(&self, element: &Node, matching: Matching) -> Tri {
        if !matching.step() {
            return Tri::Maybe;
        }
        if self.pseudo_element {
            return Tri::No;
        }
        let mut result = Tri::Yes;
        if let Some(name) = &self.name {
            result = result.and(compare(&element_name(element), name));
        }
        if self.namespaced {
            result = result.and(Tri::Maybe);
        }
        if result == Tri::No {
            return result;
        }

        if !self.ids.is_empty() {
            let id = element.get_attribute_no_ns("id").unwrap_or_default();
            // Of an image's pages, only the first keeps its id.
            let kept = if matching.is_page(element) {
                Tri::Maybe
            } else {
                Tri::Yes
            };
            for wanted in &self.ids {
                result = result.and(compare(&id, wanted)).and(kept);
            }
        }
        if !self.classes.is_empty() {
            let classes = element.get_attribute_no_ns("class").unwrap_or_default();
            for wanted in &self.classes {
                let found = classes
                    .split_ascii_whitespace()
                    .map(|class| compare(class, wanted))
                    .max()
                    .unwrap_or(Tri::No);
                result = result.and(found);
            }
        }
        for attribute in &self.attributes {
            if result == Tri::No {
                return result;
            }
            result = result.and(attribute.matches(element, matching));
        }
        for class in &self.pseudo_classes {
            if result == Tri::No {
                return result;
            }
            result = result.and(class.matches(element, matching));
        }
        result
    }
}

/// Is `found` what `wanted` asks for: surely if it is as written, perhaps if
/// it differs only in case, which an HTML reader ignores and an XML one
/// does not.
fn compare(found: &str, wanted: &str) -> Tri {
    if found == wanted {
        Tri::Yes
    } else if found.eq_ignore_ascii_case(wanted) {
        Tri::Maybe
    } else {
        Tri::No
    }
}

impl Attribute {
    fn matches(&self, element: &Node, matching: Matching) -> Tri {
        let page = matching.is_page(element);
        if page && !self.namespaced {
            match self.name.to_ascii_lowercase().as_str() {
                // Gone from every page.
                "width" | "height" | "srcset" | "sizes" => return Tri::No,
                // Each page names a file of its own.
                "src" if self.test.is_some() => return Tri::Maybe,
                "src" => return Tri::Yes,
                _ => {}
            }
        }

        let (value, sure) = if self.namespaced {
            (element.get_attribute(&self.name), Tri::Maybe)
        } else {
            match element.get_attribute_no_ns(&self.name) {
                Some(value) => (Some(value), Tri::Yes),
                None => (
                    element.get_attribute_no_ns(&self.name.to_ascii_lowercase()),
                    Tri::Maybe,
                ),
            }
        };
        let Some(value) = value else {
            return Tri::No;
        };
        // Of an image's pages, only the first keeps its id.
        let sure = if page && self.name.eq_ignore_ascii_case("id") {
            sure.and(Tri::Maybe)
        } else {
            sure
        };
        let Some((test, wanted)) = &self.test else {
            return sure;
        };
        let holds = |value: &str, wanted: &str| match test {
            AttributeTest::Equals => value == wanted,
            AttributeTest::Includes => {
                !wanted.is_empty()
                    && !wanted.contains(char::is_whitespace)
                    && value.split_ascii_whitespace().any(|word| word == wanted)
            }
            AttributeTest::DashMatch => {
                value == wanted
                    || value.starts_with(wanted) && value[wanted.len()..].starts_with('-')
            }
            AttributeTest::Prefix => !wanted.is_empty() && value.starts_with(wanted),
            AttributeTest::Suffix => !wanted.is_empty() && value.ends_with(wanted),
            AttributeTest::Substring => !wanted.is_empty() && value.contains(wanted),
        };
        let folded = holds(&value.to_lowercase(), &wanted.to_lowercase());
        let holds = if self.case_insensitive {
            Tri::of(folded)
        } else if holds(&value, wanted) {
            Tri::Yes
        } else if folded {
            // Some HTML attributes' values are compared regardless of case.
            Tri::Maybe
        } else {
            Tri::No
        };
        sure.and(holds)
    }
}

impl PseudoClass {
    fn matches(&self, element: &Node, matching: Matching) -> Tri {
        match self {
            PseudoClass::Not(list) => any(list, element, matching).not(),
            PseudoClass::Is(list) | PseudoClass::Where(list) => any(list, element, matching),
            PseudoClass::Root => Tri::of(parent_element(element).is_none()),
            PseudoClass::Empty => {
                let mut result = Tri::Yes;
                let mut child = element.get_first_child();
                while let Some(node) = child {
                    if !matching.step() {
                        return Tri::Maybe;
                    }
                    match node.get_type() {
                        Some(NodeType::ElementNode) => return Tri::No,
                        Some(NodeType::TextNode | NodeType::CDataSectionNode) => {
                            let text = node.get_content();
                            if !text.is_empty() {
                                // Blanks count as content in some readers
                                // and not in others.
                                if text.trim().is_empty() {
                                    result = Tri::Maybe;
                                } else {
                                    return Tri::No;
                                }
                            }
                        }
                        Some(NodeType::EntityRefNode) => return Tri::No,
                        _ => {}
                    }
                    child = node.get_next_sibling();
                }
                result
            }
            PseudoClass::Link => Tri::of(
                matches!(local_name(element).as_str(), "a" | "area" | "link")
                    && element.get_attribute_no_ns("href").is_some(),
            ),
            PseudoClass::Lang(ranges) => {
                let mut node = Some(element.clone());
                while let Some(current) = node {
                    if !matching.step() {
                        return Tri::Maybe;
                    }
                    let lang = current
                        .get_attribute_ns("lang", crate::xml::NS_XML)
                        .or_else(|| current.get_attribute_no_ns("lang"));
                    if let Some(lang) = lang {
                        let lang = lang.to_ascii_lowercase();
                        return Tri::of(ranges.iter().any(|range| {
                            lang == *range
                                || lang.starts_with(range.as_str())
                                    && lang[range.len()..].starts_with('-')
                        }));
                    }
                    node = parent_element(&current);
                }
                // The book's language may be the reader's to say.
                Tri::Maybe
            }
            PseudoClass::Nth {
                a,
                b,
                of_type,
                from_end,
                of,
            } => {
                let mut own = Tri::Yes;
                if let Some(of) = of {
                    own = any(of, element, matching);
                    if own == Tri::No {
                        return Tri::No;
                    }
                }
                // Where an image's pages are added, where an element stands
                // among its siblings is not known.
                if matching.moves(element) {
                    return Tri::Maybe.and(own);
                }
                let name = element.get_name();
                let counts = |sibling: &Node| -> Tri {
                    if *of_type {
                        Tri::of(sibling.get_name() == name)
                    } else if let Some(of) = of {
                        any(of, sibling, matching)
                    } else {
                        Tri::Yes
                    }
                };
                // Where it is among its siblings, at least, and how many
                // before it may count besides.
                let (mut position, mut uncertain) = (1i64, 0i64);
                let mut sibling = if *from_end {
                    next_element(element)
                } else {
                    previous_element(element)
                };
                while let Some(node) = sibling {
                    if !matching.step() {
                        return Tri::Maybe.and(own);
                    }
                    match counts(&node) {
                        Tri::Yes => position += 1,
                        Tri::Maybe => uncertain += 1,
                        Tri::No => {}
                    }
                    sibling = if *from_end {
                        next_element(&node)
                    } else {
                        previous_element(&node)
                    };
                }
                let (a, b) = (i64::from(*a), i64::from(*b));
                let holds = |n: i64| {
                    if a == 0 {
                        n == b
                    } else {
                        (n - b) % a == 0 && (n - b) / a >= 0
                    }
                };
                let mut at = (position..=position + uncertain).map(holds);
                let first = at.next().unwrap_or(false);
                let result = if at.all(|h| h == first) {
                    Tri::of(first)
                } else {
                    Tri::Maybe
                };
                result.and(own)
            }
            PseudoClass::Never => Tri::No,
            PseudoClass::Unknown => Tri::Maybe,
        }
    }
}

/// How matching a selector from one of its compounds came out: matched,
/// surely or perhaps, or not, and how far the failure goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Matched(Tri),
    /// Not from this element, though from the next one the combinator to
    /// its right leads to perhaps.
    NotHere,
    /// Not from this element, nor from any other sibling the closest
    /// combinator to its right that leads up through siblings leads to.
    NotAmongSiblings,
    /// Not from this element, nor from any element further up.
    NotAbove,
}

fn any(list: &[Complex], element: &Node, matching: Matching) -> Tri {
    list.iter()
        .map(|complex| complex.matches(element, matching))
        .fold(Tri::No, Tri::or)
}

impl Complex {
    /// What stands for a selector this cannot read: one that may match any
    /// element, more specific than any other.
    fn anything() -> Complex {
        Complex {
            compounds: vec![Compound {
                pseudo_classes: vec![PseudoClass::Unknown],
                ..Compound::default()
            }],
            combinators: Vec::new(),
            specificity: Specificity(u32::MAX, u32::MAX, u32::MAX),
        }
    }

    fn matches(&self, element: &Node, matching: Matching) -> Tri {
        match self.match_at(self.compounds.len() - 1, element, matching) {
            Outcome::Matched(holds) => holds,
            _ => Tri::No,
        }
    }

    /// Match the compounds up to `index` from `element`, which the one at
    /// `index` is matched against, and those before from the elements its
    /// combinators lead to. A way that fails tells how far the failure goes,
    /// as browsers' engines tell it, so that no element is tried again where
    /// it failed before, and matching takes no more than a few steps for
    /// each element and compound.
    fn match_at(&self, index: usize, element: &Node, matching: Matching) -> Outcome {
        let here = self.compounds[index].matches(element, matching);
        if here == Tri::No {
            return Outcome::NotHere;
        }
        if index == 0 {
            return Outcome::Matched(here);
        }
        let combinator = self.combinators[index - 1];
        let siblings = matches!(combinator, Combinator::Next | Combinator::Later);
        // Where an image's pages are added, which siblings come before an
        // element is not known.
        if siblings && matching.moves(element) {
            return Outcome::Matched(here.and(Tri::Maybe));
        }
        let step = |node: &Node| {
            if siblings {
                previous_element(node)
            } else {
                parent_element(node)
            }
        };
        let not_found = if siblings {
            Outcome::NotAmongSiblings
        } else {
            Outcome::NotAbove
        };

        let mut perhaps = false;
        let mut candidate = step(element);
        let failed = loop {
            let Some(node) = candidate else {
                break not_found;
            };
            if matching.spent() {
                perhaps = true;
                break not_found;
            }
            let result = self.match_at(index - 1, &node, matching);
            match (result, combinator) {
                (Outcome::Matched(Tri::Yes), _) => return Outcome::Matched(here),
                (Outcome::Matched(_), Combinator::Child | Combinator::Next) => {
                    return Outcome::Matched(here.and(Tri::Maybe))
                }
                (Outcome::Matched(_), _) => perhaps = true,
                (Outcome::NotAbove, _) => break Outcome::NotAbove,
                (_, Combinator::Next) => break result,
                (_, Combinator::Child) => break Outcome::NotAmongSiblings,
                (Outcome::NotAmongSiblings, Combinator::Later) => break result,
                _ => {}
            }
            candidate = step(&node);
        };
        if perhaps {
            Outcome::Matched(here.and(Tri::Maybe))
        } else {
            failed
        }
    }

    /// What an element must have to be matched, as the selector's last
    /// compound asks: an id, a class or a name, lowercased, or nothing that
    /// narrows it.
    fn key(&self) -> Option<String> {
        let last = self.compounds.last()?;
        if let Some(id) = last.ids.first() {
            Some(format!("#{}", id.to_ascii_lowercase()))
        } else if let Some(class) = last.classes.first() {
            Some(format!(".{}", class.to_ascii_lowercase()))
        } else {
            last.name.as_ref().map(|name| name.to_ascii_lowercase())
        }
    }
}

/// The keys of the selectors that may match `element`: see [`Complex::key`].
fn keys_of(element: &Node) -> Vec<String> {
    let mut keys = vec![local_name(element)];
    if let Some(id) = element.get_attribute_no_ns("id") {
        keys.push(format!("#{}", id.to_ascii_lowercase()));
    }
    if let Some(classes) = element.get_attribute_no_ns("class") {
        for class in classes.split_ascii_whitespace() {
            keys.push(format!(".{}", class.to_ascii_lowercase()));
        }
    }
    keys
}

fn parent_element(node: &Node) -> Option<Node> {
    node.get_parent()
        .filter(|parent| parent.get_type() == Some(NodeType::ElementNode))
}

fn previous_element(node: &Node) -> Option<Node> {
    let mut sibling = node.get_prev_sibling();
    while let Some(current) = sibling {
        if current.get_type() == Some(NodeType::ElementNode) {
            return Some(current);
        }
        sibling = current.get_prev_sibling();
    }
    None
}

fn next_element(node: &Node) -> Option<Node> {
    let mut sibling = node.get_next_sibling();
    while let Some(current) = sibling {
        if current.get_type() == Some(NodeType::ElementNode) {
            return Some(current);
        }
        sibling = current.get_next_sibling();
    }
    None
}

/// An element's name, as written, without a prefix.
fn element_name(node: &Node) -> String {
    let name = node.get_name();
    match name.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => name,
    }
}

/// An element's name, lowercased, as HTML names it.
fn local_name(node: &Node) -> String {
    element_name(node).to_ascii_lowercase()
}

// ------------------------------------------------------------ declarations

/// The properties that size or place a box, as far as Light Novel mode reads
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Property {
    Height,
    MaxHeight,
    Width,
    AspectRatio,
    Position,
    /// `transform` and the like, and `rotate`, `scale` and `translate`.
    Transform,
    /// Whether what overflows the box across the page is not shown as it
    /// is, which leaves what overflows it down the page hidden or scrolled
    /// too.
    OverflowX,
    /// Whether what overflows the box down the page is not shown as it is.
    OverflowY,
    /// Whether the box is sized as if it held nothing: `contain: size`.
    Contain,
}

const PROPERTIES: [Property; 9] = [
    Property::Height,
    Property::MaxHeight,
    Property::Width,
    Property::AspectRatio,
    Property::Position,
    Property::Transform,
    Property::OverflowX,
    Property::OverflowY,
    Property::Contain,
];

/// What a property's value does to a box, as far as that matters here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Value {
    /// Leaves it as it would be: `auto`, `none`, `visible`, `static`.
    Initial,
    /// A length of its own, in pixels, ems and the like.
    Length,
    /// A share of the screen, in `vh` and the like.
    Screen,
    /// A share of the box it is in, a percentage, which holds only where that
    /// box's own height is set.
    Percent,
    /// Set otherwise: a width, a ratio, a transform, `absolute` or `fixed`, an
    /// overflow that is hidden, a containment of size.
    Set,
    /// Whatever the box it is in has.
    Inherit,
    /// What cannot be told without the reader, a variable say.
    Unknown,
}

#[derive(Debug, Clone, Copy)]
struct Declaration {
    property: Property,
    value: Value,
    important: bool,
    /// Whether it may be of another property instead: a logical one, which is
    /// the box's height or its width as the page is written across or down.
    logical: bool,
}

/// The declarations of a rule's block or a `style` attribute that size or
/// place a box.
#[derive(Debug, Default)]
struct Declarations {
    own: Vec<Declaration>,
    /// Those of the rules nested in the block, whose selectors are not
    /// worked out.
    nested: Vec<Declaration>,
}

/// The units that are shares of the screen.
const SCREEN_UNITS: &[&str] = &[
    "vw", "vh", "vi", "vb", "vmin", "vmax", "svw", "svh", "svi", "svb", "svmin", "svmax", "lvw",
    "lvh", "lvi", "lvb", "lvmin", "lvmax", "dvw", "dvh", "dvi", "dvb", "dvmin", "dvmax",
];

/// The units that are shares of a box a container query names, or of the
/// screen.
const CONTAINER_UNITS: &[&str] = &["cqw", "cqh", "cqi", "cqb", "cqmin", "cqmax"];

fn parse_declarations(text: &str) -> Declarations {
    let mut found = Declarations::default();
    let mut input = ParserInput::new(text);
    let mut parser = Parser::new(&mut input);
    declarations_in(&mut parser, &mut found, css::MAX_NESTING);
    found
}

/// Read the declarations `input` holds into `found`, and those of the rules
/// nested in it, `depth` levels down at most. A rule nested deeper may set
/// anything.
fn declarations_in(input: &mut Parser<'_, '_>, found: &mut Declarations, depth: usize) {
    while !input.is_exhausted() {
        // One that cannot be read is passed over, as CSS passes over it.
        let _ = input.parse_until_after(Delimiter::Semicolon, |chunk| {
            // A rule nested in the block, `& img { … }` say, has a block of
            // its own.
            let start = chunk.state();
            let mut nested = false;
            while let Ok(token) = chunk.next() {
                if !matches!(token, Token::CurlyBracketBlock) {
                    continue;
                }
                nested = true;
                let mut inner = Declarations::default();
                if depth == 0 {
                    inner.own.push(Declaration {
                        property: Property::Height,
                        value: Value::Unknown,
                        important: false,
                        logical: false,
                    });
                } else {
                    let _ = chunk.parse_nested_block(|block| {
                        declarations_in(block, &mut inner, depth - 1);
                        Ok::<_, ParseError<()>>(())
                    });
                }
                found.nested.extend(inner.own);
                found.nested.extend(inner.nested);
            }
            if !nested {
                chunk.reset(&start);
                declaration(chunk, &mut found.own);
            }
            Ok::<_, ParseError<()>>(())
        });
    }
}

/// Read one declaration into `own`, if it is of a property that sizes or
/// places a box.
fn declaration(input: &mut Parser<'_, '_>, own: &mut Vec<Declaration>) -> Option<()> {
    use Property::*;
    let name = input.expect_ident().ok()?.to_ascii_lowercase();
    input.expect_colon().ok()?;
    let (properties, logical): (&[Property], bool) = match name.as_str() {
        "height" => (&[Height], false),
        "max-height" => (&[MaxHeight], false),
        "width" => (&[Width], false),
        // Across or down the page, as it is written.
        "block-size" | "inline-size" => (&[Height, Width], true),
        "max-block-size" | "max-inline-size" => (&[MaxHeight], true),
        "aspect-ratio" => (&[AspectRatio], false),
        "position" => (&[Position], false),
        "transform" | "-webkit-transform" | "-moz-transform" | "-ms-transform" | "-o-transform"
        | "rotate" | "scale" | "translate" => (&[Transform], false),
        "overflow" => (&[OverflowX, OverflowY], false),
        "overflow-x" => (&[OverflowX], false),
        "overflow-y" => (&[OverflowY], false),
        "overflow-inline" | "overflow-block" => (&[OverflowX, OverflowY], true),
        "contain" => (&[Contain], false),
        _ => return None,
    };
    let start = input.state();
    for &property in properties {
        input.reset(&start);
        let (value, important) = value_of(property, input);
        own.push(Declaration {
            property,
            value,
            important,
            logical,
        });
    }
    Some(())
}

/// What the value `input` holds does to a box for `property`, and whether
/// it is `!important`.
fn value_of(property: Property, input: &mut Parser<'_, '_>) -> (Value, bool) {
    let mut terms = Terms::default();
    let mut important = false;
    while let Ok(token) = input.next() {
        let token = token.clone();
        if matches!(token, Token::Delim('!')) {
            important = input
                .try_parse(|i| i.expect_ident_matching("important"))
                .is_ok();
        } else {
            terms.take(input, &token, true, MAX_VALUE_NESTING);
        }
    }

    let said = |words: &[&str]| {
        terms
            .words
            .iter()
            .any(|word| words.contains(&word.as_str()))
    };
    if said(&["inherit"]) {
        return (Value::Inherit, important);
    }
    if terms.unknown {
        return (Value::Unknown, important);
    }
    let left_alone = |also: &[&str]| {
        terms.words.iter().all(|word| {
            also.contains(&word.as_str())
                || ["initial", "unset", "revert", "revert-layer"].contains(&word.as_str())
        })
    };
    let value = match property {
        Property::Height | Property::MaxHeight => {
            if terms.percent
                || said(&[
                    "stretch",
                    "fill-available",
                    "-webkit-fill-available",
                    "-moz-available",
                ])
            {
                Value::Percent
            } else if terms.fixed {
                Value::Length
            } else if terms.screen {
                Value::Screen
            } else {
                Value::Initial
            }
        }
        Property::Position => {
            if said(&["absolute", "fixed"]) {
                Value::Set
            } else {
                Value::Initial
            }
        }
        Property::OverflowX | Property::OverflowY => {
            if left_alone(&["visible"]) {
                Value::Initial
            } else {
                Value::Set
            }
        }
        Property::Contain => {
            if said(&["size", "strict", "inline-size"]) {
                Value::Set
            } else {
                Value::Initial
            }
        }
        Property::Width | Property::AspectRatio | Property::Transform => {
            if terms.any_number() || !left_alone(&["auto", "none"]) {
                Value::Set
            } else {
                Value::Initial
            }
        }
    };
    (value, important)
}

/// What a value is made of, as far as telling what it does to a box goes.
#[derive(Debug, Default)]
struct Terms {
    /// Its keywords, lowercased, and the names of its functions, with a `(`.
    words: Vec<String>,
    fixed: bool,
    screen: bool,
    percent: bool,
    unknown: bool,
    other: bool,
}

impl Terms {
    fn read(&mut self, input: &mut Parser<'_, '_>, depth: usize) {
        while let Ok(token) = input.next() {
            let token = token.clone();
            self.take(input, &token, false, depth);
        }
    }

    /// Take in `token`, read from `input`, and what is in it if it opens a
    /// block, `depth` levels down at most. A bare number `outside` `calc()`
    /// and the like is a length, `0` or pixels to an engine that takes it
    /// so; inside, it is a factor.
    fn take(&mut self, input: &mut Parser<'_, '_>, token: &Token<'_>, outside: bool, depth: usize) {
        let unit_in =
            |units: &[&str], unit: &str| units.iter().any(|known| unit.eq_ignore_ascii_case(known));
        match token {
            Token::Ident(word) => self.words.push(word.to_ascii_lowercase()),
            Token::Function(name) => {
                let name = name.to_ascii_lowercase();
                if matches!(name.as_str(), "var" | "env" | "attr") {
                    self.unknown = true;
                }
                self.words.push(format!("{name}("));
            }
            Token::Dimension { unit, .. } if unit_in(SCREEN_UNITS, unit) => self.screen = true,
            Token::Dimension { unit, .. } if unit_in(CONTAINER_UNITS, unit) => self.unknown = true,
            Token::Dimension { .. } => self.fixed = true,
            Token::Percentage { .. } => self.percent = true,
            Token::Number { .. } if outside => self.fixed = true,
            Token::Number { .. } => self.other = true,
            _ => {}
        }
        if matches!(
            token,
            Token::Function(_)
                | Token::ParenthesisBlock
                | Token::SquareBracketBlock
                | Token::CurlyBracketBlock
        ) {
            if depth == 0 {
                self.unknown = true;
                return;
            }
            let _ = input.parse_nested_block(|inner| {
                self.read(inner, depth - 1);
                Ok::<_, ParseError<()>>(())
            });
        }
    }

    fn any_number(&self) -> bool {
        self.fixed || self.screen || self.percent || self.other
    }
}

// ---------------------------------------------------------------- sheets

/// A stylesheet's rules, read once, as far as Light Novel mode reads them.
#[derive(Debug, Default)]
pub(crate) struct Sheet {
    rules: Vec<SheetRule>,
    /// The rules whose selectors want an element to have a key, by key: see
    /// [`Complex::key`].
    by_key: HashMap<String, Vec<usize>>,
    /// The rules with a selector that wants none.
    anywhere: Vec<usize>,
    /// What its `@import` rules bring in before its own rules: each url, and
    /// whether the import applies.
    pub(crate) imports: Vec<(String, Tri)>,
}

#[derive(Debug)]
struct SheetRule {
    selectors: Vec<Complex>,
    /// Whether the grouping rules around it hold: `@media`, `@supports` and
    /// the rest.
    applies: Tri,
    declarations: Vec<Declaration>,
}

impl Sheet {
    /// The rules of `css`, as long as what they come to fits in what is
    /// `left` of the pieces a book's stylesheets may come to: `None` past
    /// that.
    pub(crate) fn parse(css: &str, left: &mut usize) -> Option<Sheet> {
        let mut sheet = Sheet::default();
        let rules = css::rules(css, css::MAX_NESTING);
        // An `@import` counts only before every other rule but `@charset`
        // and `@layer` statements.
        let mut importing = true;
        for rule in &rules {
            match &rule.kind {
                RuleKind::At { name, prelude, .. } if name == "import" && importing => {
                    if let Some(import) = import(&css[prelude.clone()]) {
                        sheet.imports.push(import);
                    }
                }
                RuleKind::At { name, children, .. }
                    if name == "charset" || name == "layer" && children.is_empty() => {}
                _ => importing = false,
            }
        }
        sheet.add(css, rules, Tri::Yes, left)?;
        Some(sheet)
    }

    fn add(
        &mut self,
        css: &str,
        rules: Vec<css::Rule>,
        applies: Tri,
        left: &mut usize,
    ) -> Option<()> {
        for rule in rules {
            match rule.kind {
                RuleKind::Style {
                    selectors,
                    declarations,
                } => {
                    // A declaration is a few bytes at least, and a compound
                    // of a selector one, so that the text tells how many
                    // pieces there can be before it is read.
                    let declarations = &css[declarations];
                    if declarations.len() / 6 > *left {
                        return None;
                    }
                    let found = parse_declarations(declarations);
                    let mut cost = found.own.len() + found.nested.len();
                    if !found.own.is_empty() {
                        let selectors = &css[selectors];
                        if selectors.len() > left.saturating_sub(cost) {
                            return None;
                        }
                        let selectors = parse_selectors(selectors);
                        cost += pieces(&selectors);
                        self.push(selectors, applies, found.own);
                    }
                    if !found.nested.is_empty() {
                        self.push(vec![Complex::anything()], applies, found.nested);
                    }
                    *left = left.checked_sub(cost)?;
                }
                RuleKind::At {
                    name,
                    prelude,
                    children,
                } => {
                    let condition = match name.as_str() {
                        "media" => media_applies(&css[prelude]),
                        "starting-style" => Tri::No,
                        _ => Tri::Maybe,
                    };
                    self.add(css, children, applies.and(condition), left)?;
                }
            }
        }
        Some(())
    }

    fn push(&mut self, selectors: Vec<Complex>, applies: Tri, declarations: Vec<Declaration>) {
        if applies == Tri::No {
            return;
        }
        let index = self.rules.len();
        let mut keys: Vec<Option<String>> = selectors.iter().map(Complex::key).collect();
        keys.sort();
        keys.dedup();
        for key in keys {
            match key {
                Some(key) => self.by_key.entry(key).or_default().push(index),
                None => self.anywhere.push(index),
            }
        }
        self.rules.push(SheetRule {
            selectors,
            applies,
            declarations,
        });
    }

    /// The rules that may style an element of these `keys`, in order.
    fn rules_for(&self, keys: &[String]) -> Vec<usize> {
        let mut found = self.anywhere.clone();
        for key in keys {
            found.extend(self.by_key.get(key).into_iter().flatten());
        }
        found.sort_unstable();
        found.dedup();
        found
    }
}

/// How many pieces a book's stylesheets may come to, as Light Novel mode
/// reads them: compounds of selectors and declarations, a few hundred bytes
/// each at most. Real books' come to a few thousand; a stylesheet past this
/// is taken to say anything.
pub(crate) const MAX_PIECES: usize = 1 << 18;

/// How many pieces `list` comes to: its compounds, and those of the lists in
/// their pseudo-classes.
fn pieces(list: &[Complex]) -> usize {
    list.iter()
        .flat_map(|complex| &complex.compounds)
        .map(|compound| {
            1 + compound
                .pseudo_classes
                .iter()
                .map(|class| match class {
                    PseudoClass::Not(list) | PseudoClass::Is(list) | PseudoClass::Where(list) => {
                        pieces(list)
                    }
                    PseudoClass::Nth { of: Some(list), .. } => pieces(list),
                    _ => 0,
                })
                .sum::<usize>()
        })
        .sum()
}

/// The url an `@import` prelude names, and whether the import applies, as
/// its media queries, a `supports()` or a `layer` leave it.
fn import(prelude: &str) -> Option<(String, Tri)> {
    let mut input = ParserInput::new(prelude);
    let mut parser = Parser::new(&mut input);
    let url = match parser.next().ok()?.clone() {
        Token::QuotedString(url) | Token::UnquotedUrl(url) => url.to_string(),
        Token::Function(name) if name.eq_ignore_ascii_case("url") => parser
            .parse_nested_block(|inner| {
                Ok::<_, ParseError<()>>(inner.expect_string().ok().map(|url| url.to_string()))
            })
            .ok()??,
        _ => return None,
    };
    let rest = &prelude[parser.position().byte_index()..];
    let lowered = without_comments(rest).to_ascii_lowercase();
    let applies = if lowered.contains("supports(") || lowered.trim_start().starts_with("layer") {
        Tri::Maybe
    } else {
        media_applies(rest)
    };
    Some((url, applies))
}

// ---------------------------------------------------------------- layout

/// The stylesheets that style one document, in the order the cascade takes
/// them, each with whether it applies.
pub(crate) type Styles<'a> = [(&'a Sheet, Tri)];

/// Where a declaration stands in the cascade: whether it is important,
/// whether it is a `style` attribute's, its selector's specificity, and
/// where it comes, by sheet, rule and place in the rule.
type Rank = (bool, bool, Specificity, usize, usize, usize);

/// One declaration for an element: where it stands in the cascade, what it
/// does, and whether it surely applies.
struct Candidate {
    rank: Rank,
    value: Value,
    sure: bool,
}

/// What an element's box could be sized and placed as: for each property,
/// every value it may take where the book is read.
#[derive(Debug, Clone, Copy, Default)]
struct Sizing {
    values: [u8; PROPERTIES.len()],
}

impl Sizing {
    fn could_be(&self, property: Property, value: Value) -> bool {
        self.values[property as usize] & (1 << value as u8) != 0
    }

    fn could_be_any(&self, property: Property, values: &[Value]) -> bool {
        values.iter().any(|&value| self.could_be(property, value))
    }
}

/// Elements whose `height` attribute is a presentational hint for their
/// height: one, as CSS takes it, at no specificity, before every rule.
const HEIGHT_HINTED: &[&str] = &[
    "table", "td", "th", "tr", "object", "iframe", "video", "canvas", "embed",
];

/// The boxes of one document's elements, as its styles lay them out, worked
/// out as they are asked for.
pub(crate) struct Layout<'a> {
    styles: &'a Styles<'a>,
    matching: Matching<'a>,
    sizings: RefCell<HashMap<xmlNodePtr, Sizing>>,
    heights: RefCell<HashMap<xmlNodePtr, bool>>,
}

impl<'a> Layout<'a> {
    /// The layout `styles` give a document whose images `reshaping` may
    /// reshape, matching selectors while `budget` lasts.
    pub(crate) fn new(
        styles: &'a Styles<'a>,
        reshaping: &'a Reshaping,
        budget: &'a Cell<u64>,
    ) -> Self {
        Layout {
            styles,
            matching: Matching { reshaping, budget },
            sizings: RefCell::new(HashMap::new()),
            heights: RefCell::new(HashMap::new()),
        }
    }

    /// `element` and the elements it is in, from the outermost down, as far
    /// as `known` has not worked them out yet.
    fn unknown_line(element: &Node, known: impl Fn(&Node) -> bool) -> Vec<Node> {
        let mut line = Vec::new();
        let mut node = Some(element.clone());
        while let Some(current) = node {
            if known(&current) {
                break;
            }
            node = parent_element(&current);
            line.push(current);
        }
        line.reverse();
        line
    }

    fn sizing(&self, element: &Node) -> Sizing {
        // Each box can take after the one it is in, which is worked out
        // first.
        let line = Self::unknown_line(element, |node| {
            self.sizings.borrow().contains_key(&node.node_ptr())
        });
        for node in &line {
            let sizing = self.sizing_of(node);
            self.sizings.borrow_mut().insert(node.node_ptr(), sizing);
        }
        self.sizings.borrow()[&element.node_ptr()]
    }

    /// `element`'s sizing, that of the element it is in worked out already.
    fn sizing_of(&self, element: &Node) -> Sizing {
        let mut candidates: [Vec<Candidate>; PROPERTIES.len()] = Default::default();
        let mut add = |declaration: &Declaration, rank: Rank, sure: bool| {
            candidates[declaration.property as usize].push(Candidate {
                rank,
                value: declaration.value,
                sure: sure && !declaration.logical,
            });
        };

        if HEIGHT_HINTED.contains(&local_name(element).as_str()) {
            if let Some(value) = element
                .get_attribute_no_ns("height")
                .and_then(|height| hinted_height(&height))
            {
                let hint = Declaration {
                    property: Property::Height,
                    value,
                    important: false,
                    logical: false,
                };
                add(&hint, (false, false, Specificity::default(), 0, 0, 0), true);
            }
        }

        let keys = keys_of(element);
        for (sheet_index, (sheet, applies)) in self.styles.iter().enumerate() {
            if *applies == Tri::No {
                continue;
            }
            for rule_index in sheet.rules_for(&keys) {
                let rule = &sheet.rules[rule_index];
                let applies = applies.and(rule.applies);
                let (mut sure, mut perhaps) = (None, None);
                for selector in &rule.selectors {
                    let slot = match selector.matches(element, self.matching) {
                        Tri::Yes => &mut sure,
                        Tri::Maybe => &mut perhaps,
                        Tri::No => continue,
                    };
                    *slot = (*slot).max(Some(selector.specificity));
                }
                // A rule that surely applies does at its surest specificity,
                // and may at a higher one.
                let ranks = [
                    sure.map(|specificity| (specificity, applies == Tri::Yes)),
                    perhaps
                        .filter(|&specificity| sure.is_none_or(|sure| specificity > sure))
                        .map(|specificity| (specificity, false)),
                ];
                for (specificity, sure) in ranks.into_iter().flatten() {
                    for (place, declaration) in rule.declarations.iter().enumerate() {
                        let rank = (
                            declaration.important,
                            false,
                            specificity,
                            sheet_index + 1,
                            rule_index,
                            place,
                        );
                        add(declaration, rank, sure);
                    }
                }
            }
        }

        if let Some(style) = element.get_attribute_no_ns("style") {
            for (place, declaration) in parse_declarations(&style).own.iter().enumerate() {
                let rank = (
                    declaration.important,
                    true,
                    Specificity::default(),
                    0,
                    0,
                    place,
                );
                add(declaration, rank, true);
            }
        }

        let parent =
            parent_element(element).map(|parent| self.sizings.borrow()[&parent.node_ptr()]);
        let mut found = Sizing::default();
        for property in PROPERTIES {
            let list = &mut candidates[property as usize];
            list.sort_by_key(|candidate| std::cmp::Reverse(candidate.rank));
            // The cascade's winner, and whatever may win above it.
            let mut values = Vec::new();
            let mut settled = false;
            for candidate in list.iter() {
                values.push(candidate.value);
                if candidate.sure {
                    settled = true;
                    break;
                }
            }
            if !settled {
                values.push(Value::Initial);
            }
            for value in values {
                found.values[property as usize] |= if value == Value::Inherit {
                    parent.map_or(1 << Value::Initial as u8, |parent| {
                        parent.values[property as usize]
                    })
                } else {
                    1 << value as u8
                };
            }
        }
        found
    }

    /// Could a percentage of `element`'s height hold: is the box it is in of
    /// a height set? The screen's is, for the root, and so is the box an
    /// element placed out of the run of the page is placed against.
    fn percent_holds(&self, element: &Node) -> bool {
        let own = self.sizing(element);
        own.could_be_any(Property::Position, &[Value::Set, Value::Unknown])
            || parent_element(element).is_none_or(|parent| self.height_set(&parent))
    }

    /// Could `element`'s height be set, rather than grow with what it holds?
    fn height_set(&self, element: &Node) -> bool {
        let line = Self::unknown_line(element, |node| {
            self.heights.borrow().contains_key(&node.node_ptr())
        });
        for node in &line {
            let own = self.sizing(node);
            // That of the box it is in is worked out already.
            let percent_holds = own.could_be_any(Property::Position, &[Value::Set, Value::Unknown])
                || parent_element(node)
                    .is_none_or(|parent| self.heights.borrow()[&parent.node_ptr()]);
            let set = own.could_be_any(
                Property::Height,
                &[Value::Length, Value::Screen, Value::Unknown],
            ) || own.could_be(Property::Height, Value::Percent) && percent_holds;
            self.heights.borrow_mut().insert(node.node_ptr(), set);
        }
        self.heights.borrow()[&element.node_ptr()]
    }

    /// Could `element`'s box stop short of holding what it is given: a height
    /// or a `max-height` set, a shape, or a size that is not what it holds?
    fn capped(&self, element: &Node) -> bool {
        let own = self.sizing(element);
        self.height_set(element)
            || own.could_be_any(
                Property::MaxHeight,
                &[Value::Length, Value::Screen, Value::Unknown],
            )
            || own.could_be(Property::MaxHeight, Value::Percent) && self.percent_holds(element)
            || own.could_be_any(Property::AspectRatio, &[Value::Set, Value::Unknown])
            || own.could_be_any(Property::Contain, &[Value::Set, Value::Unknown])
    }

    /// Could the box `element` is in, one level up or more, not take its pages
    /// where it took it: a box of a set height or shape, one the book turns,
    /// or lays over the page? The page's own, `<html>` and `<body>`, runs on
    /// onto the pages after; only one that hides what overflows it frames.
    pub(crate) fn frames(&self, element: &Node) -> bool {
        let own = self.sizing(element);
        let set = |property| own.could_be_any(property, &[Value::Set, Value::Unknown]);
        if set(Property::Transform) {
            return true;
        }
        if matches!(local_name(element).as_str(), "html" | "body") {
            return self.capped(element) && (set(Property::OverflowX) || set(Property::OverflowY));
        }
        self.capped(element) || set(Property::Position)
    }

    /// Would `element`, a page of an image, keeping the style it has, take a
    /// size or proportions of its own: a height, or a screen high with a
    /// width, a ratio; or would it be turned, or laid over the others?
    pub(crate) fn frames_itself(&self, element: &Node) -> bool {
        let own = self.sizing(element);
        let set = |property| own.could_be_any(property, &[Value::Set, Value::Unknown]);
        let of_its_own = [Value::Length, Value::Unknown];
        if own.could_be_any(Property::Height, &of_its_own)
            || own.could_be_any(Property::MaxHeight, &of_its_own)
            || set(Property::AspectRatio)
            || set(Property::Position)
            || set(Property::Transform)
            || set(Property::Contain)
        {
            return true;
        }
        let page_high = own.could_be(Property::Height, Value::Screen)
            || own.could_be(Property::MaxHeight, Value::Screen)
            || (own.could_be(Property::Height, Value::Percent)
                || own.could_be(Property::MaxHeight, Value::Percent))
                && self.percent_holds(element);
        page_high && set(Property::Width)
    }
}

/// What a `height` attribute gives as a height: a number of pixels, or a
/// percentage. Anything else is no hint.
fn hinted_height(height: &str) -> Option<Value> {
    let height = height.trim();
    let (number, value) = match height.strip_suffix('%') {
        Some(number) => (number, Value::Percent),
        None => (height.strip_suffix("px").unwrap_or(height), Value::Length),
    };
    number.trim().parse::<f64>().ok().map(|_| value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet(css: &str) -> Sheet {
        Sheet::parse(css, &mut usize::MAX.clone()).unwrap()
    }

    fn document(body: &str) -> crate::html::ContentDocument {
        crate::html::parse_content(
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body>{body}</body></html>"#
            )
            .as_bytes(),
        )
        .unwrap()
    }

    fn element(content: &crate::html::ContentDocument, id: &str) -> Node {
        crate::xml::find_nodes(&content.doc, &format!("//*[@id='{id}']"))
            .unwrap()
            .remove(0)
    }

    fn body(content: &crate::html::ContentDocument) -> Node {
        crate::xml::find_nodes(&content.doc, "//*[local-name()='body']")
            .unwrap()
            .remove(0)
    }

    /// Whether `selector` matches `node`, with `reshaping` as it is.
    fn matches_node(selector: &str, node: &Node, reshaping: &Reshaping) -> Tri {
        let list = parse_selectors(selector);
        let budget = Cell::new(u64::MAX);
        let matching = Matching {
            reshaping,
            budget: &budget,
        };
        any(&list, node, matching)
    }

    fn matches(selector: &str, content: &crate::html::ContentDocument, id: &str) -> Tri {
        matches_node(selector, &element(content, id), &Reshaping::default())
    }

    /// What `css` makes of `node`'s box: does it frame what is in it, and
    /// would it frame itself as a page of an image?
    fn laid_out(css: &str, node: &Node, reshaping: &Reshaping) -> (bool, bool) {
        let sheet = sheet(css);
        let styles = [(&sheet, Tri::Yes)];
        let budget = Cell::new(u64::MAX);
        let layout = Layout::new(&styles, reshaping, &budget);
        (layout.frames(node), layout.frames_itself(node))
    }

    #[test]
    fn media_queries_hold_on_a_screen() {
        for (queries, holds) in [
            ("", Tri::Yes),
            ("all", Tri::Yes),
            ("screen", Tri::Yes),
            ("only screen", Tri::Yes),
            ("SCREEN /* e-ink */", Tri::Yes),
            ("print", Tri::No),
            ("only print and (color)", Tri::No),
            ("not print", Tri::Yes),
            ("not screen", Tri::No),
            ("screen and (min-width: 600px)", Tri::Maybe),
            ("(orientation: portrait)", Tri::Maybe),
            ("amzn-kf8", Tri::Maybe),
            ("print, screen", Tri::Yes),
            ("print, tv", Tri::No),
        ] {
            assert_eq!(media_applies(queries), holds, "{queries}");
        }
    }

    #[test]
    fn selectors_match_as_css_matches_them() {
        let content = document(
            r#"<div id="frame" class="frame wide" lang="fr"><p id="text" class="frame">Text</p><span id="mark"/><img id="image" src="a.png" alt=""/></div><div id="other"><img id="second" class="plate" src="b.png" alt=""/></div>"#,
        );
        let cases = [
            (".frame", "frame", Tri::Yes),
            (".frame.wide", "frame", Tri::Yes),
            (".frame.narrow", "frame", Tri::No),
            ("div.frame", "text", Tri::No),
            ("#frame", "frame", Tri::Yes),
            ("#frame", "text", Tri::No),
            (".FRAME", "frame", Tri::Maybe),
            ("IMG", "image", Tri::Maybe),
            // Combinators.
            (".frame img", "image", Tri::Yes),
            (".frame img", "second", Tri::No),
            ("#other > img", "second", Tri::Yes),
            ("body > img", "image", Tri::No),
            ("p + span", "mark", Tri::Yes),
            ("p ~ img", "image", Tri::Yes),
            ("p + img", "image", Tri::No),
            // Attributes.
            ("img[src]", "image", Tri::Yes),
            ("img[src='a.png']", "image", Tri::Yes),
            ("img[src$='.png']", "second", Tri::Yes),
            ("img[src^='b']", "image", Tri::No),
            ("img[alt]", "image", Tri::Yes),
            ("img[title]", "image", Tri::No),
            // Pseudo-classes.
            ("img:last-child", "image", Tri::Yes),
            ("img:first-child", "image", Tri::No),
            ("div:nth-child(2)", "other", Tri::Yes),
            ("div:nth-of-type(odd)", "frame", Tri::Yes),
            ("img:only-of-type", "second", Tri::Yes),
            ("img:not(.plate)", "image", Tri::Yes),
            ("img:not(.plate)", "second", Tri::No),
            ("img:is(.plate, #image)", "image", Tri::Yes),
            ("div:lang(fr)", "frame", Tri::Yes),
            ("img:lang(en)", "image", Tri::No),
            ("img:hover", "image", Tri::No),
            ("a:hover img", "image", Tri::No),
            ("img:-epub-whatever", "image", Tri::Maybe),
            (":root", "frame", Tri::No),
            ("span:empty", "mark", Tri::Yes),
            // A pseudo-element's box is not the element's.
            ("div::before, div:after", "frame", Tri::No),
            ("div::before, div", "frame", Tri::Yes),
            ("svg|img", "image", Tri::Maybe),
            ("*|img", "image", Tri::Yes),
            // A selector this cannot read may be one a reader can.
            ("#1a, img", "frame", Tri::Maybe),
            ("img >", "frame", Tri::Maybe),
        ];
        for (selector, id, expected) in cases {
            assert_eq!(
                matches(selector, &content, id),
                expected,
                "{selector} on #{id}"
            );
        }
    }

    /// Where an image's pages are added, which siblings an element has is
    /// not known; of the pages, all but the first lose the image's id, and
    /// each names a file of its own.
    #[test]
    fn selectors_match_an_images_pages_as_they_may_be() {
        let content = document(
            r#"<p id="before">Text</p><div id="frame"><img id="image" class="plate" src="a.png" width="10" alt=""/></div>"#,
        );
        let image = element(&content, "image");
        let mut reshaping = Reshaping::default();
        reshaping.pages.insert(image.node_ptr());
        reshaping
            .changing
            .insert(element(&content, "frame").node_ptr());
        let on_page = |selector: &str| matches_node(selector, &image, &reshaping);
        assert_eq!(on_page("img.plate"), Tri::Yes);
        assert_eq!(on_page("div > img"), Tri::Yes);
        assert_eq!(on_page("img:first-child"), Tri::Maybe);
        assert_eq!(on_page("img:last-child"), Tri::Maybe);
        assert_eq!(on_page("img + img"), Tri::Maybe);
        assert_eq!(on_page("#image"), Tri::Maybe);
        assert_eq!(on_page("img[id]"), Tri::Maybe);
        assert_eq!(on_page("img[width]"), Tri::No);
        assert_eq!(on_page("img[src]"), Tri::Yes);
        assert_eq!(on_page("img[src$='.png']"), Tri::Maybe);
        assert_eq!(on_page("img[alt]"), Tri::Yes);
        // What is around the pages stays as it is.
        let frame = element(&content, "frame");
        assert_eq!(matches_node("p + div", &frame, &reshaping), Tri::Yes);
        assert_eq!(matches_node("div:first-child", &frame, &reshaping), Tri::No);
    }

    /// A stylesheet comes to a piece for each compound of a selector and
    /// each declaration it keeps, and one that comes to more than is left
    /// is not read.
    #[test]
    fn a_stylesheet_past_what_is_left_is_not_read() {
        let css: String = (0..1000)
            .map(|i| format!(".a{i} {{ height: 0; width: 0 }} p {{ color: red }}\n"))
            .collect();
        let mut left = 4000;
        assert!(Sheet::parse(&css, &mut left).is_some());
        assert_eq!(left, 1000);
        let mut left = 2000;
        assert!(Sheet::parse(&css, &mut left).is_none());
        assert_eq!(pieces(&parse_selectors("a b, :is(.c, d > e):not(f)")), 7);
    }

    /// An `@import` brings in what its url names, before every other rule,
    /// where its media queries hold.
    #[test]
    fn imports_are_read_with_where_they_apply() {
        let imports = |css: &str| sheet(css).imports;
        let one = |url: &str, applies| vec![(url.to_string(), applies)];
        assert_eq!(imports(r#"@import url("a.css");"#), one("a.css", Tri::Yes));
        assert_eq!(imports("@import url(a.css)"), one("a.css", Tri::Yes));
        assert_eq!(imports(r#"@import "a.css" print;"#), one("a.css", Tri::No));
        assert_eq!(
            imports(r#"@charset "UTF-8"; @import 'a.css' screen and (color);"#),
            one("a.css", Tri::Maybe)
        );
        assert_eq!(
            imports(r#"@import url("a.css") supports(display: grid);"#),
            one("a.css", Tri::Maybe)
        );
        // Not after another rule.
        assert!(imports(r#"p { color: red } @import "a.css";"#).is_empty());
    }

    #[test]
    fn specificity_counts_ids_classes_and_names() {
        let of = |selector: &str| parse_selectors(selector)[0].specificity;
        assert_eq!(of("img"), Specificity(0, 0, 1));
        assert_eq!(of("div.frame img"), Specificity(0, 1, 2));
        assert_eq!(of("#box > p:first-child"), Specificity(1, 1, 1));
        assert_eq!(of("img:not(#a, .b)"), Specificity(1, 0, 1));
        assert_eq!(of("img:where(#a)"), Specificity(0, 0, 1));
        assert_eq!(of("[src].x::before"), Specificity(0, 2, 1));
    }

    #[test]
    fn declarations_say_what_they_do_to_a_box() {
        let one = |text: &str| parse_declarations(text).own[0];
        for (text, value) in [
            ("height: 200px", Value::Length),
            ("height: 0", Value::Length),
            ("height: calc(100% - 2em)", Value::Percent),
            ("height: calc(2 * 3em)", Value::Length),
            ("height: 100vh", Value::Screen),
            ("height: 100%", Value::Percent),
            ("height: stretch", Value::Percent),
            ("height: auto", Value::Initial),
            ("height: fit-content", Value::Initial),
            ("height: var(--page)", Value::Unknown),
            ("height: 50cqh", Value::Unknown),
            ("height: inherit", Value::Inherit),
            ("max-height: none", Value::Initial),
            ("width: 100%", Value::Set),
            ("width: auto", Value::Initial),
            ("aspect-ratio: 5 / 2", Value::Set),
            ("aspect-ratio: auto", Value::Initial),
            ("position: absolute", Value::Set),
            ("position: relative", Value::Initial),
            ("transform: none", Value::Initial),
            ("-webkit-transform: rotate(90deg)", Value::Set),
            ("scale: 2", Value::Set),
            ("overflow: hidden", Value::Set),
            ("overflow: visible auto", Value::Set),
            ("overflow-y: visible", Value::Initial),
            ("contain: strict", Value::Set),
            ("contain: paint", Value::Initial),
        ] {
            assert_eq!(one(text).value, value, "{text}");
        }
        assert!(one("height: 2em !important").important);
        assert!(!one("height: 2em").important);
        // One that cannot be read is passed over, and the next read.
        assert_eq!(
            parse_declarations("height 200px; HEIGHT: 4EM").own[0].value,
            Value::Length
        );
        assert!(parse_declarations("color: red; min-height: 10em")
            .own
            .is_empty());
        // A logical size is a height or a width.
        let logical = parse_declarations("block-size: 3in").own;
        assert_eq!(logical.len(), 2);
        assert!(logical.iter().all(|declaration| declaration.logical));
        // A nested rule's are kept apart, a rule too deep to read taken to
        // set what may be anything.
        let nested = parse_declarations("height: auto; & img { height: 2em } color: red");
        assert_eq!(nested.own.len(), 1);
        assert_eq!(nested.nested[0].value, Value::Length);
        let deep = format!("{}{}", "a { ".repeat(40), "}".repeat(40));
        assert_eq!(parse_declarations(&deep).nested[0].value, Value::Unknown);
        let calc = format!("height: {}1px{}", "calc(".repeat(40), ")".repeat(40));
        assert_eq!(parse_declarations(&calc).own[0].value, Value::Unknown);
    }

    /// The cascade's winner decides: a later rule, a more specific one, an
    /// important one, a style attribute. A rule that only perhaps applies
    /// can frame a box, never free one.
    #[test]
    fn the_cascade_decides_what_frames() {
        let content = document(
            r#"<div id="frame" class="frame" style="height: auto"><img id="image" class="plate" src="a.png" alt=""/></div><div id="loose" class="frame"><img id="free" src="b.png" alt=""/></div>"#,
        );
        let framed =
            |css: &str, id: &str| laid_out(css, &element(&content, id), &Reshaping::default()).0;
        assert!(framed(".frame { height: 200px }", "loose"));
        // Overridden by a later rule, a more specific one, or the style
        // attribute.
        assert!(!framed(
            ".frame { height: 200px } .frame { height: auto }",
            "loose"
        ));
        assert!(!framed(
            "#loose { height: auto } .frame { height: 200px }",
            "loose"
        ));
        assert!(!framed(".frame { height: 200px; height: auto }", "loose"));
        assert!(!framed(".frame { height: 200px }", "frame"));
        // But not by an earlier or less specific one, nor over `!important`.
        assert!(framed(
            "div { height: auto } .frame { height: 200px }",
            "loose"
        ));
        assert!(framed(".frame { height: auto; height: 200px }", "loose"));
        assert!(framed(".frame { height: 200px !important }", "frame"));
        // A rule that only perhaps applies frames, but frees nothing.
        assert!(framed(
            "@media (min-width: 600px) { .frame { height: 200px } }",
            "loose"
        ));
        assert!(framed(
            ".frame { height: 200px } @media (min-width: 600px) { .frame { height: auto } }",
            "loose"
        ));
        assert!(framed(
            ".frame { height: 200px } .frame { block-size: auto }",
            "loose"
        ));
        assert!(framed(".frame { inline-size: 200px }", "loose"));
        assert!(framed(".frame { & img { height: 200px } }", "loose"));
        assert!(framed("#1a, .frame { height: 200px }", "loose"));
        assert!(!framed(
            "@media print { .frame { height: 200px } }",
            "loose"
        ));
        // `inherit` takes what the box it is in has.
        assert!(framed(
            "body { height: 50vh } .frame { height: inherit }",
            "loose"
        ));
        // A table's height attribute counts, under the rules.
        let table = document(
            r#"<table id="table" height="600"><tr><td><img id="image" src="a.png" alt=""/></td></tr></table>"#,
        );
        let node = element(&table, "table");
        assert!(laid_out("", &node, &Reshaping::default()).0);
        assert!(!laid_out("table { height: auto }", &node, &Reshaping::default()).0);
    }

    /// A percentage of the height of a box whose own height grows with what it
    /// holds is no height at all; of one set, or of the screen through every
    /// box between, it is.
    #[test]
    fn a_percentage_frames_only_where_it_holds() {
        let content = document(
            r#"<div id="outer"><div id="frame"><img id="image" src="a.png" alt=""/></div></div>"#,
        );
        let framed =
            |css: &str| laid_out(css, &element(&content, "frame"), &Reshaping::default()).0;
        assert!(!framed("#frame { height: 100% }"));
        assert!(!framed("body { height: 100% } #frame { height: 100% }"));
        assert!(framed("#outer { height: 300px } #frame { height: 50% }"));
        assert!(framed(
            "html, body { height: 100% } #outer { height: 100% } #frame { height: 100% }"
        ));
        assert!(framed(
            "html, body, #outer { height: 100% } #frame { max-height: 80% }"
        ));
        assert!(framed("#frame { position: absolute; height: 100% }"));
        // The page itself frames nothing unless it hides what overflows it.
        let page = |css: &str| laid_out(css, &body(&content), &Reshaping::default()).0;
        assert!(!page("html, body { height: 100% }"));
        assert!(!page("body { height: 100vh }"));
        assert!(page("body { height: 100vh; overflow: hidden }"));
        assert!(page("body { height: 100vh; overflow-x: hidden }"));
        assert!(!page(
            "body { height: 100vh; overflow: hidden } body { overflow: visible }"
        ));
        // An image whose pages are a screen high and as wide as they were
        // made gives them its proportions; one a screen high alone does not.
        let image = element(&content, "image");
        let itself = |css: &str| laid_out(css, &image, &Reshaping::default()).1;
        assert!(!itself("img { height: 100vh }"));
        assert!(itself("img { height: 100vh; width: 100% }"));
        assert!(!itself("img { height: 100%; width: 100% }"));
        assert!(itself(
            "html, body, div { height: 100% } img { height: 100%; width: 100% }"
        ));
        assert!(itself("img { max-height: 30em }"));
    }

    /// The pages of an image, which lose its id and size, are styled as they
    /// will be.
    #[test]
    fn an_images_pages_are_styled_as_they_will_be() {
        let content = document(
            r#"<div id="frame"><img id="image" class="plate" src="a.png" height="300" alt=""/></div>"#,
        );
        let image = element(&content, "image");
        let mut reshaping = Reshaping::default();
        reshaping.pages.insert(image.node_ptr());
        reshaping
            .changing
            .insert(element(&content, "frame").node_ptr());
        let itself = |css: &str| laid_out(css, &image, &reshaping).1;
        assert!(!itself(
            ".plate { height: 300px } img.plate { height: auto }"
        ));
        assert!(itself("#image { height: auto } .plate { height: 300px }"));
        assert!(!itself(
            "img[height] { height: 300px } img { height: auto }"
        ));
        assert!(itself("img:not([height]) { height: 300px }"));
        assert!(itself("img:only-child { height: 300px }"));
    }

    /// A selector that fails only at its far end is tried no more than a few
    /// times at each element, however deep the boxes go; and a book's CSS
    /// can ask for no end of matching, so once the budget is spent, what is
    /// left may match.
    #[test]
    fn matching_takes_few_steps_and_stops_once_its_budget_is_spent() {
        let content = document(&format!(
            "{}<img id=\"image\" src=\"a.png\" alt=\"\"/>{}",
            "<div>".repeat(200),
            "</div>".repeat(200)
        ));
        let image = element(&content, "image");
        let css = format!("p {} img {{ height: 300px }}", ["div"; 30].join(" "));
        let sheet = sheet(&css);
        let styles = [(&sheet, Tri::Yes)];
        let reshaping = Reshaping::default();

        let budget = Cell::new(100_000);
        let layout = Layout::new(&styles, &reshaping, &budget);
        assert!(!layout.frames_itself(&image));
        assert!(budget.get() > 90_000, "{} steps", 100_000 - budget.get());

        let budget = Cell::new(10);
        let layout = Layout::new(&styles, &reshaping, &budget);
        assert!(layout.frames_itself(&image));
        assert_eq!(budget.get(), 0);
    }
}
