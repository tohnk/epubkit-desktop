//! UTF-8 text in malformed chapters: upstream's issue #1, where German came
//! back from the recovery parser as "Ã¤".
//!
//! Every chapter here is malformed on purpose (an unclosed `<br>`), because a
//! well-formed one never reaches the HTML parser that did the damage. Given no
//! encoding, that parser guesses, and the guess depends on the libxml2
//! release: 2.9 reads undeclared bytes as UTF-8, 2.14 as ISO-8859-1. These
//! tests must pass against both.

mod common;

use std::fs;
use std::io::Read;

use epubkit_core::css::collect_used_selectors;
use epubkit_core::html::{
    normalize_whitespace, strip_unnecessary_attributes, HtmlRepair, LibxmlRepair,
};
use epubkit_core::pipeline::{process_epub, ProcessingOptions};
use epubkit_core::text::{clean_text_content, TextCleanOptions};

const GERMAN: &str = "Während ihre Schwester die Lehre als Verkäuferin abgebrochen hatte";
const QUOTED: &str = "»Wie geht’s Mutter übrigens?«";

/// UTF-8, with no charset declared anywhere.
fn undeclared() -> Vec<u8> {
    format!("<!DOCTYPE html><html><body><p>{GERMAN}</p><p>{QUOTED}<br></p></body></html>")
        .into_bytes()
}

/// UTF-8 under a `<meta>` still claiming ISO-8859-1, as books converted from
/// old HTML often are.
fn stale_latin1_declaration() -> Vec<u8> {
    format!(
        r#"<html><head><meta http-equiv="Content-Type" content="text/html; charset=iso-8859-1"/></head><body><p>{GERMAN}<br></p></body></html>"#
    )
    .into_bytes()
}

/// Undeclared UTF-8 that gives every cleanup pass something to change, so
/// each one reserializes what it parsed rather than handing back its input.
fn busy_chapter() -> Vec<u8> {
    format!(
        r#"<html><body><p class="kapitelüberschrift" data-page="1">{GERMAN}</p><p></p><p></p><p>{QUOTED}<br></p></body></html>"#
    )
    .into_bytes()
}

fn latin1(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).expect("fixture should be Latin-1"))
        .collect()
}

fn repaired(input: &[u8]) -> String {
    let out = LibxmlRepair::new()
        .repair(input)
        .expect("repair should succeed");
    assert!(out.recovered, "the fixture should need recovering");
    String::from_utf8(out.bytes).expect("utf-8 output")
}

/// UTF-8 read as Latin-1 turns each multi-byte character into a run starting
/// with one of these: "ä" becomes "Ã¤", "»" becomes "Â»", and "’" becomes
/// "â" followed by two C1 controls.
fn assert_not_mangled(text: &str) {
    for marker in ["Ã", "Â", "\u{80}"] {
        assert!(
            !text.contains(marker),
            "UTF-8 was read as Latin-1 ({marker:?}):\n{text}"
        );
    }
}

#[test]
fn undeclared_utf8_survives_recovery() {
    let output = repaired(&undeclared());

    assert!(output.contains(GERMAN), "{output}");
    assert!(output.contains(QUOTED), "{output}");
    assert_not_mangled(&output);
}

#[test]
fn a_stale_latin1_declaration_does_not_mangle_utf8() {
    let output = repaired(&stale_latin1_declaration());

    assert!(output.contains(GERMAN), "{output}");
    assert_not_mangled(&output);
}

/// Whatever holds the parser to UTF-8 must not end up in the book, however
/// little the chapter holds. A byte order mark given nothing to precede is
/// read by libxml2 2.9 as text: an invisible U+FEFF in an empty paragraph.
#[test]
fn nothing_is_added_to_the_text() {
    for chapter in ["", "   \n", "<p>x", "<p>ä", "<p>ä<br></p>"] {
        let Ok(out) = LibxmlRepair::new().repair(chapter.as_bytes()) else {
            continue; // refusing a chapter is not adding to it
        };
        let text = String::from_utf8(out.bytes).unwrap();
        assert!(
            !text.contains('\u{feff}'),
            "{chapter:?} came back as {text:?}"
        );
    }
}

/// The other direction: bytes that are not UTF-8 are not forced to be, so a
/// chapter that really is Latin-1 still decodes, whether it declares itself
/// or not.
#[test]
fn latin1_bytes_are_still_read_as_latin1() {
    let declared = format!(
        r#"<html><head><meta http-equiv="Content-Type" content="text/html; charset=iso-8859-1"/></head><body><p>{GERMAN}<br></p></body></html>"#
    );
    let undeclared = format!("<html><body><p>{GERMAN}<br></p></body></html>");

    for chapter in [declared, undeclared] {
        let output = repaired(&latin1(&chapter));
        assert!(output.contains(GERMAN), "{output}");
    }
}

// Every pass that reads a chapter shares the repair step's parse, so each must
// keep UTF-8 intact on its own, not only after repair has run.

#[test]
fn attribute_stripping_keeps_utf8() {
    let (output, stripped) = strip_unnecessary_attributes(&busy_chapter()).unwrap();
    let output = String::from_utf8(output).unwrap();

    assert_eq!(stripped, 1);
    assert!(output.contains(GERMAN), "{output}");
    assert_not_mangled(&output);
}

#[test]
fn whitespace_normalization_keeps_utf8() {
    let (output, removed) = normalize_whitespace(&busy_chapter()).unwrap();
    let output = String::from_utf8(output).unwrap();

    assert_eq!(removed, 1);
    assert!(output.contains(GERMAN), "{output}");
    assert_not_mangled(&output);
}

#[test]
fn text_cleanup_keeps_utf8() {
    // Mojibake repair would quietly undo some of the damage this looks for.
    let options = TextCleanOptions {
        fix_encoding: false,
        ..TextCleanOptions::default()
    };
    let (output, _) = clean_text_content(&busy_chapter(), &options).unwrap();
    let output = String::from_utf8(output).unwrap();

    assert!(output.contains("Verkäuferin"), "{output}");
    assert!(output.contains("übrigens"), "{output}");
    assert_not_mangled(&output);
}

/// A misread class name no longer matches its stylesheet, so the unused-CSS
/// pass would strip rules the book uses.
#[test]
fn selector_collection_keeps_utf8_class_names() {
    let used = collect_used_selectors(&busy_chapter()).unwrap();
    assert!(
        used.classes.contains("kapitelüberschrift"),
        "{:?}",
        used.classes
    );
}

#[test]
fn a_malformed_utf8_chapter_survives_the_whole_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let output = dir.path().join("out.epub");

    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("OEBPS/content.opf", common::CONTENT_OPF),
            ("OEBPS/chapter1.xhtml", &undeclared()),
        ],
    );

    let options = ProcessingOptions {
        text_cleanup: false,
        ..ProcessingOptions::default()
    };
    let report = process_epub(&input, &output, &options, |_, _| {}).unwrap();
    assert_eq!(report.documents_recovered, 1);

    let mut archive = zip::ZipArchive::new(fs::File::open(&output).unwrap()).unwrap();
    let mut chapter = String::new();
    archive
        .by_name("OEBPS/chapter1.xhtml")
        .unwrap()
        .read_to_string(&mut chapter)
        .unwrap();

    assert!(chapter.contains(GERMAN), "{chapter}");
    assert!(chapter.contains(QUOTED), "{chapter}");
    assert_not_mangled(&chapter);
}
