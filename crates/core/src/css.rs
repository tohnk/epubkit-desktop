//! Stylesheet cleanup: dropping rules nothing in the book matches, and
//! removing embedded fonts. The CSS half of `html_cleaner.py`.
//!
//! The reference used `cssutils`; this uses `lightningcss`, a real CSS parser,
//! so `@media` blocks, nested rules and comments survive a round-trip that
//! `cssutils` would flatten or lose.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};
use lightningcss::printer::PrinterOptions;
use lightningcss::rules::CssRule;
use lightningcss::stylesheet::{ParserOptions, StyleSheet};
use lightningcss::traits::ToCss;

use crate::html;
use crate::{xml, Error, Result};

/// Selectors that must never be dropped, whatever the content looks like.
const ALWAYS_KEEP: &[&str] = &["*", "html", "body"];

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
pub fn remove_unused_css(css_text: &str, used: &UsedSelectors) -> (String, usize) {
    let Ok(mut stylesheet) = StyleSheet::parse(css_text, ParserOptions::default()) else {
        // Unparseable CSS is left exactly as found rather than mangled.
        return (css_text.to_string(), 0);
    };

    let mut removed = 0;
    stylesheet.rules.0.retain(|rule| match rule {
        CssRule::Style(style) => {
            let keep = match style.selectors.to_css_string(PrinterOptions::default()) {
                Ok(selector_text) => selector_matches_used(&selector_text, used),
                // If a selector will not serialize, keep the rule.
                Err(_) => true,
            };
            if !keep {
                removed += 1;
            }
            keep
        }
        _ => true,
    });

    match stylesheet.to_css(PrinterOptions::default()) {
        Ok(result) => (result.code, removed),
        Err(_) => (css_text.to_string(), 0),
    }
}

/// Remove `@font-face` rules. Returns the cleaned stylesheet and how many went.
pub fn remove_embedded_fonts(css_text: &str) -> (String, usize) {
    let Ok(mut stylesheet) = StyleSheet::parse(css_text, ParserOptions::default()) else {
        return (css_text.to_string(), 0);
    };

    let mut removed = 0;
    stylesheet.rules.0.retain(|rule| {
        let is_font_face = matches!(rule, CssRule::FontFace(_));
        if is_font_face {
            removed += 1;
        }
        !is_font_face
    });

    if removed == 0 {
        return (css_text.to_string(), 0);
    }

    match stylesheet.to_css(PrinterOptions::default()) {
        Ok(result) => (result.code, removed),
        Err(_) => (css_text.to_string(), 0),
    }
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
    // scan would misread: `.\31 st` is the class `1st`.
    if selector.contains(':') || selector.contains('[') || selector.contains('\\') {
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

/// Every byte of a multi-byte UTF-8 character is non-ASCII, so testing bytes
/// keeps such a character whole and the slices on character boundaries.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || !byte.is_ascii()
}
