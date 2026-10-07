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
const SKIP_TAGS: &[&str] = &[
    "script", "style", "pre", "code", "kbd", "samp", "tt", "var", "math",
];

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
///
/// German and others open quotes low, with „ and ‚, which fold to quotes like
/// the rest; the reference made ‚ a comma and left „ alone. A no-break space
/// is not folded: in a paragraph of its own it is a visible scene break,
/// where a plain space collapses to nothing.
const SMART_QUOTES: &[(char, &str)] = &[
    ('\u{2018}', "'"),
    ('\u{2019}', "'"),
    ('\u{201a}', "'"),
    ('\u{201c}', "\""),
    ('\u{201d}', "\""),
    ('\u{201e}', "\""),
    ('\u{2014}', "--"),
    ('\u{2013}', "-"),
    ('\u{2026}', "..."),
];

/// Characters left as they are when doubled: Japanese and Chinese write their
/// ellipsis and dash that way.
const KEPT_WHEN_DOUBLED: &[char] = &['\u{2026}', '\u{2014}'];

/// UTF-8 bytes that were decoded as Latin-1 somewhere upstream, and the
/// characters they were meant to be.
///
/// Longest first, so that no pattern can claim the start of a longer one.
/// Typographic punctuation is three bytes in UTF-8 and comes back as "â"
/// followed by two invisible C1 controls.
const MOJIBAKE: &[(&str, &str)] = &[
    ("\u{00e2}\u{0080}\u{0099}", "\u{2019}"),
    ("\u{00e2}\u{0080}\u{0098}", "\u{2018}"),
    ("\u{00e2}\u{0080}\u{009c}", "\u{201c}"),
    ("\u{00e2}\u{0080}\u{009d}", "\u{201d}"),
    ("\u{00e2}\u{0080}\u{0094}", "\u{2014}"),
    ("\u{00e2}\u{0080}\u{0093}", "\u{2013}"),
    ("\u{00e2}\u{0080}\u{00a6}", "\u{2026}"),
    ("\u{00c3}\u{00a9}", "\u{00e9}"),
    ("\u{00c3}\u{00a8}", "\u{00e8}"),
    ("\u{00c3}\u{00ab}", "\u{00eb}"),
    ("\u{00c3}\u{00bc}", "\u{00fc}"),
    ("\u{00c3}\u{00b1}", "\u{00f1}"),
    ("\u{00c3}\u{00a7}", "\u{00e7}"),
    ("\u{00c3}\u{00b6}", "\u{00f6}"),
    ("\u{00c3}\u{00a4}", "\u{00e4}"),
    ("\u{00c3}\u{009f}", "\u{00df}"),
    ("\u{00c3}\u{00a1}", "\u{00e1}"),
    ("\u{00c3}\u{00b3}", "\u{00f3}"),
    ("\u{00c3}\u{00ba}", "\u{00fa}"),
    ("\u{00c3}\u{0084}", "\u{00c4}"),
    ("\u{00c3}\u{0096}", "\u{00d6}"),
    ("\u{00c3}\u{009c}", "\u{00dc}"),
    ("\u{00c2}\u{00a3}", "\u{00a3}"),
    ("\u{00c2}\u{00bb}", "\u{00bb}"),
    ("\u{00c2}\u{00ab}", "\u{00ab}"),
    ("\u{00c2}\u{00b0}", "\u{00b0}"),
];

/// "à" and "í" read as Latin-1: "Ã" followed by what their second bytes are
/// in Latin-1, a no-break space and a soft hyphen. After a capital, though,
/// "Ã" is the real Portuguese or Vietnamese letter, as in "MAÇÃ VERDE".
static AMBIGUOUS_MOJIBAKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[^\p{Lu}])\x{c3}([\x{a0}\x{ad}])").unwrap());

/// Spaces between words, not the indentation at the start of a line.
static RUNS_OF_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\S)[ \t]{2,}").unwrap());

/// Plain spaces before a mark that ends a word. A mark followed by more of the
/// word is something else: a calibre (".45"), an extension (".NET", ".com") or
/// a smiley (":)"). A no-break space before one is deliberate.
static SPACE_BEFORE_PUNCTUATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[ \t]+([.,;:!?])(\s|$|["'\x{201d}\x{2019}\x{bb}])"#).unwrap());

/// The same in French, which sets a space before `; : ! ?` by rule.
static SPACE_BEFORE_PUNCTUATION_FRENCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[ \t]+([.,])(\s|$|["'\x{201d}\x{2019}\x{bb}])"#).unwrap());

static LONG_ELLIPSIS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.{4,}").unwrap());

/// A word, and the run-on sentence a lowercase word ending in a full stop and
/// a capitalised one starting straight after it make.
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\S+").unwrap());
static MISSING_SENTENCE_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\p{Ll}{2}[.!?])(\p{Lu}\p{Ll})").unwrap());

/// Commas repeated after a word. Two before one open a quote typed on a
/// typewriter, as in German ",,Hallo''".
static REPEATED_COMMAS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\S),{2,}(\s|$)").unwrap());

static EXCESSIVE_TERMINATORS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[!?]{4,}").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextCleanOptions {
    pub fix_whitespace: bool,
    pub fix_ocr: bool,
    pub normalize_quotes: bool,
    pub fix_encoding: bool,
    pub fix_punctuation: bool,
    pub normalize_unicode: bool,
    /// The book's language, from `dc:language`, for a chapter that does not
    /// say what its own is. Empty when unknown.
    pub language: String,
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
            language: String::new(),
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

    // A chapter that says what language it is in speaks for itself.
    let html = crate::xml::local("html");
    let body = crate::xml::local("body");
    let declared = crate::xml::find_nodes(&content.doc, &format!("/{html} | /{html}/{body}"))?
        .into_iter()
        .find_map(|element| {
            element
                .get_attribute("lang")
                .filter(|lang| !lang.is_empty())
        });
    let options = &TextCleanOptions {
        language: declared.unwrap_or_else(|| options.language.clone()),
        ..options.clone()
    };

    for mut node in crate::xml::find_nodes(&content.doc, "//text()")? {
        if node.get_type() != Some(NodeType::TextNode) || is_inside_skipped_element(&node) {
            continue;
        }

        // Layout between elements: nothing to clean, and nothing to count.
        let original = node.get_content();
        if original.trim().is_empty() {
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

    // Encoding repair comes first, unlike in the reference. Recovering what the
    // text actually says is the precondition for every other fix: a repaired
    // quote should be normalized along with the intact ones, and the later
    // passes can break a pattern before it is found — "à" read as Latin-1
    // ends in a no-break space, which quote normalization makes a plain one.
    if options.fix_encoding {
        for (from, to) in MOJIBAKE {
            let count = text.matches(from).count();
            if count > 0 {
                text = text.replace(from, to);
                report.encoding_issues_fixed += count;
            }
        }
        text = replace_counted_with(
            &AMBIGUOUS_MOJIBAKE,
            &text,
            |caps: &Captures| {
                let letter = if &caps[2] == "\u{a0}" { 'à' } else { 'í' };
                format!("{}{letter}", &caps[1])
            },
            &mut report.encoding_issues_fixed,
        );
    }

    if options.fix_whitespace {
        text = replace_counted(
            &RUNS_OF_SPACES,
            &text,
            "$1 ",
            &mut report.double_spaces_fixed,
        );
        // The reference counts this one under whitespace rather than
        // punctuation; keeping that split makes the two reports comparable.
        let french = options.language.to_ascii_lowercase().starts_with("fr");
        let before_punctuation = if french {
            &SPACE_BEFORE_PUNCTUATION_FRENCH
        } else {
            &SPACE_BEFORE_PUNCTUATION
        };
        text = replace_counted(
            before_punctuation,
            &text,
            "$1$2",
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
            text = fold_quotes(&text, &mut report.smart_quotes_normalized);
        }
    }

    if options.fix_punctuation {
        text = replace_counted(&LONG_ELLIPSIS, &text, "...", &mut report.punctuation_fixed);
        text = add_missing_sentence_spaces(&text, &mut report.punctuation_fixed);
        text = replace_counted(
            &REPEATED_COMMAS,
            &text,
            "$1,$2",
            &mut report.punctuation_fixed,
        );
        text = replace_counted_with(
            &EXCESSIVE_TERMINATORS,
            &text,
            |caps: &Captures| match (caps[0].contains('?'), caps[0].contains('!')) {
                (true, true) => "?!".to_string(),
                (true, false) => "???".to_string(),
                _ => "!!!".to_string(),
            },
            &mut report.punctuation_fixed,
        );
    }

    if options.normalize_unicode {
        let normalized = nfc_keeping_compatibility_ideographs(&text);
        if normalized != text {
            report.unicode_normalized += 1;
            text = normalized;
        }
    }

    text
}

/// Fold typographic quotes, dashes and ellipses to ASCII, leaving the ones
/// Japanese and Chinese write doubled.
fn fold_quotes(text: &str, count: &mut usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut folded = String::with_capacity(text.len());

    for (at, &c) in chars.iter().enumerate() {
        let doubled = KEPT_WHEN_DOUBLED.contains(&c)
            && (at.checked_sub(1).map(|before| chars[before]) == Some(c)
                || chars.get(at + 1) == Some(&c));
        match SMART_QUOTES.iter().find(|(from, _)| *from == c) {
            Some((_, to)) if !doubled => {
                folded.push_str(to);
                *count += 1;
            }
            _ => folded.push(c),
        }
    }

    folded
}

/// Put a space into "ended.Then", but not into initials, abbreviations,
/// numbered clauses, file names or web addresses: only into a word with a
/// single stop in it, between a lowercase word and a capitalised one.
fn add_missing_sentence_spaces(text: &str, count: &mut usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut kept_from = 0;

    for word in WORD.find_iter(text) {
        let core = word.as_str().trim_end_matches(|c: char| {
            c.is_ascii_punctuation() || "\u{201d}\u{2019}\u{bb}".contains(c)
        });
        let stops = core.matches(['.', '!', '?']).count();
        if stops != 1 || core.contains(['/', '@']) {
            continue;
        }
        let Some(found) = MISSING_SENTENCE_SPACE.captures(word.as_str()) else {
            continue;
        };

        let split = word.start() + found.get(1).expect("always captured").end();
        out.push_str(&text[kept_from..split]);
        out.push(' ');
        kept_from = split;
        *count += 1;
    }

    out.push_str(&text[kept_from..]);
    out
}

/// NFC, except for CJK compatibility ideographs: NFC maps each to the unified
/// ideograph it stands for, a different glyph that names rely on.
fn nfc_keeping_compatibility_ideographs(text: &str) -> String {
    let compatibility = |c: char| matches!(c, '\u{f900}'..='\u{faff}' | '\u{2f800}'..='\u{2fa1f}');
    if !text.chars().any(compatibility) {
        return text.nfc().collect();
    }

    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for c in text.chars() {
        if compatibility(c) {
            out.extend(run.nfc());
            run.clear();
            out.push(c);
        } else {
            run.push(c);
        }
    }
    out.extend(run.nfc());
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Patterns are replaced in table order, so one that began a longer
    /// pattern further down would eat the start of it first.
    #[test]
    fn mojibake_patterns_run_longest_first() {
        let lengths: Vec<usize> = MOJIBAKE
            .iter()
            .map(|(from, _)| from.chars().count())
            .collect();
        assert!(
            lengths.windows(2).all(|pair| pair[0] >= pair[1]),
            "{lengths:?}"
        );
    }
}
