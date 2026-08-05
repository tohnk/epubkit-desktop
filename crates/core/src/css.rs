//! Stylesheet cleanup: dropping rules nothing in the book matches, and
//! removing embedded fonts. The CSS half of `html_cleaner.py`.
//!
//! The reference used `cssutils`; this uses `lightningcss`, a real CSS parser,
//! so `@media` blocks, nested rules and comments survive a round-trip that
//! `cssutils` would flatten or lose.

use std::collections::BTreeSet;

use lightningcss::printer::PrinterOptions;
use lightningcss::rules::CssRule;
use lightningcss::stylesheet::{ParserOptions, StyleSheet};
use lightningcss::traits::ToCss;

use crate::html;
use crate::{xml, Result};

/// Selectors that must never be dropped, whatever the content looks like.
const ALWAYS_KEEP: &[&str] = &["*", "html", "body"];

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
    // the markup can decide, so they stay.
    if selector.contains(':') || selector.contains('[') {
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
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_')
        {
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
