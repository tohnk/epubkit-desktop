//! Text-level cleanup inside XHTML content: whitespace, OCR ligature
//! artifacts, smart quotes, mojibake, punctuation and Unicode normalization.
//! Port of `text_cleaner.py`.
//!
//! Only text is touched; markup structure is left exactly as found.

use std::sync::LazyLock;

use libxml::tree::{Node, NodeType};
use regex::{Captures, Regex};
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::html;
use crate::Result;

/// Elements whose text is meant to be read literally, so must not be
/// "corrected".
const SKIP_TAGS: &[&str] = &["script", "style", "pre", "code", "kbd", "samp"];

/// Ligature codepoints that OCR and older typesetting leave in the text. They
/// render as boxes on a device with a limited font.
const OCR_LIGATURES: &[(char, &str)] = &[
    ('\u{fb00}', "ff"),
    ('\u{fb01}', "fi"),
    ('\u{fb02}', "fl"),
    ('\u{fb03}', "ffi"),
    ('\u{fb04}', "ffl"),
];

/// Typographic characters folded to ASCII equivalents.
const SMART_QUOTES: &[(char, &str)] = &[
    ('\u{2018}', "'"),
    ('\u{2019}', "'"),
    ('\u{201c}', "\""),
    ('\u{201d}', "\""),
    ('\u{2014}', "--"),
    ('\u{2013}', "-"),
    ('\u{2026}', "..."),
    ('\u{00a0}', " "),
    ('\u{201a}', ","),
];

/// UTF-8 bytes that were decoded as Latin-1 somewhere upstream, and the
/// characters they were meant to be.
const MOJIBAKE: &[(&str, &str)] = &[
    ("\u{00c3}\u{00a9}", "\u{00e9}"),
    ("\u{00c3}\u{00a8}", "\u{00e8}"),
    ("\u{00c3}\u{00ab}", "\u{00eb}"),
    ("\u{00c3}\u{00a0}", "\u{00e0}"),
    ("\u{00c3}\u{00bc}", "\u{00fc}"),
    ("\u{00c3}\u{00b1}", "\u{00f1}"),
    ("\u{00c3}\u{00a7}", "\u{00e7}"),
    ("\u{00c3}\u{00b6}", "\u{00f6}"),
    ("\u{00c3}\u{00a4}", "\u{00e4}"),
    ("\u{00c2}\u{00a3}", "\u{00a3}"),
    ("\u{00c2}\u{00bb}", "\u{00bb}"),
    ("\u{00c2}\u{00ab}", "\u{00ab}"),
    ("\u{00c2}\u{00b0}", "\u{00b0}"),
];

static RUNS_OF_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]{2,}").unwrap());
static SPACE_BEFORE_PUNCTUATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+([.,;:!?])").unwrap());
static LONG_ELLIPSIS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.{4,}").unwrap());
static MISSING_SENTENCE_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([.!?])([A-Z])").unwrap());
static REPEATED_COMMAS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r",{2,}").unwrap());
static EXCESSIVE_TERMINATORS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([!?]){4,}").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextCleanOptions {
    pub fix_whitespace: bool,
    pub fix_ocr: bool,
    pub normalize_quotes: bool,
    pub fix_encoding: bool,
    pub fix_punctuation: bool,
    pub normalize_unicode: bool,
}

impl Default for TextCleanOptions {
    fn default() -> Self {
        Self {
            fix_whitespace: true,
            fix_ocr: true,
            normalize_quotes: true,
            fix_encoding: true,
            fix_punctuation: true,
            normalize_unicode: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextCleanReport {
    pub double_spaces_fixed: usize,
    pub ocr_ligatures_fixed: usize,
    pub smart_quotes_normalized: usize,
    pub encoding_issues_fixed: usize,
    pub unicode_normalized: usize,
    pub punctuation_fixed: usize,
}

impl TextCleanReport {
    pub fn total_fixes(&self) -> usize {
        self.double_spaces_fixed
            + self.ocr_ligatures_fixed
            + self.smart_quotes_normalized
            + self.encoding_issues_fixed
            + self.unicode_normalized
            + self.punctuation_fixed
    }

    /// Fold another file's counts into this one.
    pub fn merge(&mut self, other: &TextCleanReport) {
        self.double_spaces_fixed += other.double_spaces_fixed;
        self.ocr_ligatures_fixed += other.ocr_ligatures_fixed;
        self.smart_quotes_normalized += other.smart_quotes_normalized;
        self.encoding_issues_fixed += other.encoding_issues_fixed;
        self.unicode_normalized += other.unicode_normalized;
        self.punctuation_fixed += other.punctuation_fixed;
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (count, label) in [
            (self.double_spaces_fixed, "extra spaces"),
            (self.ocr_ligatures_fixed, "OCR artifacts"),
            (self.smart_quotes_normalized, "quotes normalized"),
            (self.encoding_issues_fixed, "encoding fixes"),
            (self.punctuation_fixed, "punctuation fixes"),
            (self.unicode_normalized, "unicode fixes"),
        ] {
            if count > 0 {
                parts.push(format!("{count} {label}"));
            }
        }

        if parts.is_empty() {
            "no text issues found".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Clean every text node in an XHTML document, leaving markup untouched.
///
/// Where this differs from the reference: lxml stores the text *after* an
/// element as that element's `tail`, so skipping `<code>` also skipped the
/// prose following it. Walking real text nodes means only the text genuinely
/// inside a skipped element is left alone.
pub fn clean_text_content(
    xhtml_bytes: &[u8],
    options: &TextCleanOptions,
) -> Result<(Vec<u8>, TextCleanReport)> {
    let content = html::parse_content(xhtml_bytes)?;
    let mut report = TextCleanReport::default();

    for mut node in crate::xml::find_nodes(&content.doc, "//text()")? {
        if node.get_type() != Some(NodeType::TextNode) || is_inside_skipped_element(&node) {
            continue;
        }

        let original = node.get_content();
        if original.is_empty() {
            continue;
        }

        let cleaned = clean_string(&original, options, &mut report);
        if cleaned != original {
            // Text node content is stored unescaped and escaped again on
            // serialization, so `cleaned` goes in exactly as it reads. Escaping
            // it here would double-escape every ampersand in the book.
            node.set_content(&cleaned).ok();
        }
    }

    Ok((html::serialize_content(&content), report))
}

/// Apply the enabled fixes to one string, counting what changed.
pub fn clean_string(
    text: &str,
    options: &TextCleanOptions,
    report: &mut TextCleanReport,
) -> String {
    let mut text = text.to_string();

    if options.fix_whitespace {
        text = replace_counted(&RUNS_OF_SPACES, &text, " ", &mut report.double_spaces_fixed);
        // The reference counts this one under whitespace rather than
        // punctuation; keeping that split makes the two reports comparable.
        text = replace_counted_with(
            &SPACE_BEFORE_PUNCTUATION,
            &text,
            |caps: &Captures| caps[1].to_string(),
            &mut report.double_spaces_fixed,
        );
    }

    if options.fix_ocr {
        for (from, to) in OCR_LIGATURES {
            let count = text.matches(*from).count();
            if count > 0 {
                text = text.replace(*from, to);
                report.ocr_ligatures_fixed += count;
            }
        }

        if options.normalize_quotes {
            for (from, to) in SMART_QUOTES {
                let count = text.matches(*from).count();
                if count > 0 {
                    text = text.replace(*from, to);
                    report.smart_quotes_normalized += count;
                }
            }
        }
    }

    if options.fix_encoding {
        for (from, to) in MOJIBAKE {
            let count = text.matches(from).count();
            if count > 0 {
                text = text.replace(from, to);
                report.encoding_issues_fixed += count;
            }
        }
    }

    if options.fix_punctuation {
        text = replace_counted(&LONG_ELLIPSIS, &text, "...", &mut report.punctuation_fixed);
        text = replace_counted_with(
            &MISSING_SENTENCE_SPACE,
            &text,
            |caps: &Captures| format!("{} {}", &caps[1], &caps[2]),
            &mut report.punctuation_fixed,
        );
        text = replace_counted(&REPEATED_COMMAS, &text, ",", &mut report.punctuation_fixed);
        text = replace_counted_with(
            &EXCESSIVE_TERMINATORS,
            &text,
            |caps: &Captures| caps[1].repeat(3),
            &mut report.punctuation_fixed,
        );
    }

    if options.normalize_unicode {
        let normalized: String = text.nfc().collect();
        if normalized != text {
            report.unicode_normalized += 1;
            text = normalized;
        }
    }

    text
}

// ---------------------------------------------------------------- internals

fn replace_counted(pattern: &Regex, text: &str, replacement: &str, count: &mut usize) -> String {
    *count += pattern.find_iter(text).count();
    pattern.replace_all(text, replacement).into_owned()
}

fn replace_counted_with<F>(pattern: &Regex, text: &str, build: F, count: &mut usize) -> String
where
    F: Fn(&Captures) -> String,
{
    *count += pattern.find_iter(text).count();
    pattern.replace_all(text, build).into_owned()
}

fn is_inside_skipped_element(node: &Node) -> bool {
    let mut current = node.get_parent();
    while let Some(element) = current {
        let name = element
            .get_name()
            .rsplit(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if SKIP_TAGS.contains(&name.as_str()) {
            return true;
        }
        current = element.get_parent();
    }
    false
}
