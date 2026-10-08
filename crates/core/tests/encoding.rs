//! UTF-8 text in malformed chapters: upstream's issue #1, where German came
//! back from the recovery parser as "Ã¤". And chapters that are not UTF-8 at
//! all, which the same parser misreads in other ways.
//!
//! Every chapter here is malformed on purpose (an unclosed `<br>`), because a
//! well-formed one never reaches the HTML parser that did the damage. Given no
//! encoding, that parser guesses, and the guess depends on the libxml2
//! release: 2.9 reads undeclared bytes as UTF-8, 2.14 as ISO-8859-1. These
//! tests must pass against both.

mod common;

use std::fs;
use std::io::Read;

use encoding_rs::{Encoding, SHIFT_JIS, WINDOWS_1251, WINDOWS_1252};
use epubkit_core::css::collect_used_selectors;
use epubkit_core::html::{
    normalize_whitespace, strip_unnecessary_attributes, HtmlRepair, LibxmlRepair,
};
use epubkit_core::pipeline::{process_epub, ProcessingOptions};
use epubkit_core::text::{clean_text_content, TextCleanOptions};

const GERMAN: &str = "Während ihre Schwester die Lehre als Verkäuferin abgebrochen hatte";
const QUOTED: &str = "»Wie geht’s Mutter übrigens?«";
const JAPANESE: &str = "吾輩は猫である。";
const RUSSIAN: &str = "Война и мир";
/// Word's punctuation, which windows-1252 has where Latin-1 has controls.
const PUNCTUATION: &str = "“Quoted” … it’s – done";

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

/// `text` in a legacy encoding, which has to be able to spell all of it.
fn encoded(encoding: &'static Encoding, text: &str) -> Vec<u8> {
    let (bytes, _, unmappable) = encoding.encode(text);
    assert!(!unmappable, "{} cannot spell {text:?}", encoding.name());
    bytes.into_owned()
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

/// libxml2's HTML parser ignores an encoding named in an XML declaration: 2.9
/// reads on as Latin-1 and 2.14 as UTF-8, so German came back from 2.14 as
/// "W�hrend", and Japanese or Russian from either as nonsense.
#[test]
fn an_encoding_named_in_the_xml_declaration_is_obeyed() {
    let cases = [
        ("iso-8859-1", WINDOWS_1252, GERMAN),
        ("Shift_JIS", SHIFT_JIS, JAPANESE),
        ("windows-1251", WINDOWS_1251, RUSSIAN),
    ];

    for (label, encoding, text) in cases {
        let chapter = format!(
            r#"<?xml version="1.0" encoding="{label}"?><html><body><p>{text}<br></p></body></html>"#
        );
        let output = repaired(&encoded(encoding, &chapter));

        assert!(output.contains(text), "{label}: {output}");
        // And the chapter now says what it is.
        assert!(
            output.starts_with(r#"<?xml version="1.0" encoding="utf-8"?>"#),
            "{label}: {output}"
        );
        assert_eq!(output.matches("?xml").count(), 1, "{label}: {output}");
    }
}

/// Bytes that are not the UTF-8 they claim to be are legacy text under a
/// declaration pasted over it.
#[test]
fn a_chapter_that_is_not_the_utf8_it_claims_is_read_as_windows_1252() {
    let chapter = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><html><body><p>{GERMAN}<br></p></body></html>"#
    );
    let output = repaired(&latin1(&chapter));
    assert!(output.contains(GERMAN), "{output}");
}

/// Word's curly quotes, dashes and ellipses sit where Latin-1 has invisible
/// control characters. Browsers read such text as windows-1252, even when it
/// claims to be ISO-8859-1, and so does this.
#[test]
fn windows_1252_punctuation_comes_through() {
    let declared = format!(
        r#"<html><head><meta http-equiv="Content-Type" content="text/html; charset=iso-8859-1"/></head><body><p>{PUNCTUATION}<br></p></body></html>"#
    );
    let undeclared = format!("<html><body><p>{PUNCTUATION}<br></p></body></html>");

    for chapter in [declared, undeclared] {
        let output = repaired(&encoded(WINDOWS_1252, &chapter));
        assert!(output.contains(PUNCTUATION), "{output}");
    }
}

/// A `<meta>` charset was always obeyed, and still is.
#[test]
fn an_encoding_named_in_a_meta_element_is_still_obeyed() {
    let cases = [
        (r#"<meta charset="Shift_JIS"/>"#, SHIFT_JIS, JAPANESE),
        (
            r#"<meta http-equiv="Content-Type" content="text/html; charset=windows-1251"/>"#,
            WINDOWS_1251,
            RUSSIAN,
        ),
    ];

    for (meta, encoding, text) in cases {
        let chapter = format!("<html><head>{meta}</head><body><p>{text}<br></p></body></html>");
        let output = repaired(&encoded(encoding, &chapter));
        assert!(output.contains(text), "{meta}: {output}");
    }
}

/// A byte order mark settles the encoding before anything the chapter says.
#[test]
fn a_utf16_chapter_is_read_by_its_byte_order_mark() {
    let chapter = format!("<html><body><p>{GERMAN}<br></p></body></html>");
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(chapter.encode_utf16().flat_map(u16::to_le_bytes));

    let output = repaired(&bytes);
    assert!(output.contains(GERMAN), "{output}");
}

/// A name read as ASCII cannot be of an encoding that is not ASCII-compatible,
/// and a name nobody knows names nothing; either way the chapter is read as
/// windows-1252 rather than as nonsense.
#[test]
fn a_declaration_that_cannot_be_right_is_passed_over() {
    for label in ["UTF-16", "iso-2022-kr", "x-no-such-thing"] {
        let chapter = format!(
            r#"<html><head><meta charset="{label}"/></head><body><p>{GERMAN}<br></p></body></html>"#
        );
        let output = repaired(&latin1(&chapter));
        assert!(output.contains(GERMAN), "{label}: {output}");
    }
}

/// Repair a chapter that may or may not need recovering.
fn read(input: &[u8]) -> String {
    let out = LibxmlRepair::new()
        .repair(input)
        .expect("repair should succeed");
    String::from_utf8(out.bytes).expect("utf-8 output")
}

/// One stray byte that is not UTF-8, pasted into a UTF-8 chapter, is just that
/// byte; the rest is still UTF-8. Reading the whole chapter as windows-1252
/// because of it turned every Cyrillic letter into two Latin ones.
#[test]
fn a_stray_byte_leaves_the_rest_of_a_utf8_chapter_alone() {
    let mut chapter = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><meta charset="utf-8"/><title>{RUSSIAN}</title></head><body><p>{RUSSIAN}, "#
    )
    .into_bytes();
    chapter.extend(b"it\x92s here</p></body></html>");

    let output = read(&chapter);

    assert_eq!(output.matches(RUSSIAN).count(), 2, "{output}");
    assert!(output.contains("it\u{2019}s here"), "{output}");
}

/// A well-formed chapter is read as one with a markup error in it is.
/// Declared ISO-8859-1, which browsers take to mean windows-1252, its curly
/// quotes and dashes are not invisible control characters. Declared
/// ISO-8859-1 over bytes that are UTF-8, its accents are not mangled.
#[test]
fn a_well_formed_chapter_is_decoded_as_a_malformed_one_is() {
    let chapter = |text: &str| {
        format!(
            r#"<?xml version="1.0" encoding="iso-8859-1"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body><p>{text}</p></body></html>"#
        )
    };

    let output = read(&encoded(WINDOWS_1252, &chapter(PUNCTUATION)));
    assert!(output.contains(PUNCTUATION), "{output}");
    assert!(
        !output.chars().any(|c| ('\u{80}'..='\u{9f}').contains(&c)),
        "{output}"
    );

    let output = read(chapter(GERMAN).as_bytes());
    assert!(output.contains(GERMAN), "{output}");
    assert_not_mangled(&output);
}

/// Once a chapter is UTF-8, nothing in it says otherwise.
#[test]
fn a_chapter_says_it_is_the_utf8_it_now_is() {
    let chapter = format!(
        r#"<html><head><meta http-equiv="Content-Type" content="text/html; charset=windows-1251"/></head><body><p>{RUSSIAN}<br></p></body></html>"#
    );
    let output = repaired(&encoded(WINDOWS_1251, &chapter));

    assert!(output.contains(RUSSIAN), "{output}");
    assert!(output.contains("charset=utf-8"), "{output}");
    assert!(!output.contains("1251"), "{output}");
}

/// XML may be UTF-16 without a byte order mark; its declaration's first bytes
/// say so.
#[test]
fn a_utf16_chapter_without_a_byte_order_mark_is_read_as_utf16() {
    let chapter = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body><p>{GERMAN}</p></body></html>"#
    );
    let bytes: Vec<u8> = chapter.encode_utf16().flat_map(u16::to_le_bytes).collect();

    let output = read(&bytes);
    assert!(output.contains(GERMAN), "{output}");
}

/// An XML declaration's encoding can be anything at all, a `<meta>` included.
/// Renaming the declaration's and then the `<meta>`'s, one inside the other,
/// panicked.
#[test]
fn a_meta_inside_the_xml_declaration_is_no_meta() {
    let chapter = br#"<?xml version="1.0" encoding="<meta charset=latin1"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body><p>Text</p></body></html>"#;

    let output = read(chapter);
    assert!(output.contains("<p>Text</p>"), "{output}");
    assert!(!output.contains("latin1"), "{output}");
}

/// Only the declarations a chapter makes are renamed. A `<meta>` written out
/// in a CDATA section, a comment or an attribute is the chapter's text, and so
/// is "charset=" in a `<meta>` that says something else.
#[test]
fn what_only_looks_like_a_declaration_is_left_alone() {
    let kept = [
        r#"<pre><![CDATA[<meta charset="iso-8859-1">]]></pre>"#,
        r#"<!-- <meta charset="iso-8859-1"> -->"#,
        r#"<p title="&lt;meta charset=iso-8859-1&gt;">T</p>"#,
    ];
    let head_kept = r#"<meta name="description" content="Notes on charset=latin1 in old HTML"/>"#;
    let chapter = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title>{head_kept}<meta charset="iso-8859-1"/></head><body>{}</body></html>"#,
        kept.concat()
    );

    let output = read(chapter.as_bytes());
    for text in kept.iter().chain([&head_kept]) {
        assert!(output.contains(text), "{text}: {output}");
    }
    assert!(output.contains(r#"<meta charset="utf-8"/>"#), "{output}");

    // The same in a chapter that needs recovering, which the HTML parser reads.
    let malformed = format!(
        r#"<html><head><title>T</title><meta charset="iso-8859-1"></head><body><!-- <meta charset="iso-8859-1"> --><p>{GERMAN}<br></p></body></html>"#
    );
    let output = repaired(malformed.as_bytes());
    assert!(
        output.contains(r#"<!-- <meta charset="iso-8859-1"> -->"#),
        "{output}"
    );
    assert!(output.contains(r#"<meta charset="utf-8"/>"#), "{output}");
    assert!(output.contains(GERMAN), "{output}");
}

/// ISO-2022-JP is written in seven bits, so its bytes are valid UTF-8 too.
/// What gives it away is the escapes it switches character sets with: read as
/// UTF-8, Japanese came out as ASCII gibberish.
#[test]
fn a_chapter_in_iso_2022_jp_is_read_as_declared() {
    let declarations = [
        (r#"<?xml version="1.0" encoding="ISO-2022-JP"?>"#, ""),
        ("", r#"<meta charset="iso-2022-jp"/>"#),
        (
            "",
            r#"<meta http-equiv="Content-Type" content="text/html; charset=ISO-2022-JP"/>"#,
        ),
    ];
    for (declaration, meta) in declarations {
        let chapter = format!(
            r#"{declaration}<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title>{meta}</head><body><p>{JAPANESE}</p></body></html>"#
        );
        let bytes = encoded(encoding_rs::ISO_2022_JP, &chapter);
        assert!(bytes.is_ascii(), "the fixture should be seven-bit");

        let output = read(&bytes);
        assert!(output.contains(JAPANESE), "{declaration}{meta}: {output}");
        assert!(!output.contains("2022"), "{declaration}{meta}: {output}");
    }
}

/// A chapter re-encoded as UTF-8 that still declares ISO-2022-JP has no escapes
/// in it, and is the UTF-8 it now is.
#[test]
fn a_stale_iso_2022_jp_declaration_does_not_mangle_utf8() {
    let chapter = format!(
        r#"<?xml version="1.0" encoding="ISO-2022-JP"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body><p>{JAPANESE}</p></body></html>"#
    );

    let output = read(chapter.as_bytes());
    assert!(output.contains(JAPANESE), "{output}");
}

/// A `<meta>` after a CDATA section that never closes is passed over in time
/// that grows with the chapter. Looking for the end of each such section
/// again from every `<meta>` after it took time growing with the square of
/// it: 280 KB took twenty seconds.
#[test]
fn unclosed_cdata_sections_are_passed_over_once() {
    let chapter = format!(
        "<html><head><title>T</title></head><body><p>{GERMAN}</p>{}</body></html>",
        "<![CDATA[<meta charset=\"utf-8\">".repeat(12_000)
    );

    let output = common::finishes_within(std::time::Duration::from_secs(20), move || {
        read(chapter.as_bytes())
    });
    assert!(output.contains(GERMAN), "{}", &output[..200]);
}

/// A `<meta>` charset is found as a browser finds it. A `<!--` in a script, a
/// stylesheet or a quoted attribute is text there, not the start of a
/// comment; what a real comment or CDATA section holds is passed over; and a
/// `<meta>` whose `content` mentions a charset without declaring one says
/// nothing. Taken for a comment that never ended, a script's `"<!--"` hid the
/// `<meta>` after it, and a windows-1251 chapter came out in Latin letters.
#[test]
fn only_a_real_meta_charset_decides_the_encoding() {
    let before_meta = [
        r#"<script>var marker = "<!--";</script>"#,
        r#"<script>var marker = "<![CDATA[";</script>"#,
        r#"<style>p::before { content: "<!--" }</style>"#,
        r#"<link rel="next" title="<!--" href="next.html"/>"#,
        r#"<!-- <meta charset="iso-8859-1"> -->"#,
        r#"<![CDATA[<meta charset="iso-8859-1">]]>"#,
        r#"<meta name="description" content="Notes on charset=iso-8859-1"/>"#,
    ];
    for markup in before_meta {
        let chapter = format!(
            r#"<html><head>{markup}<meta charset="windows-1251"><title>Title</title></head><body><p>{RUSSIAN}</p></body></html>"#
        );
        let output = read(&encoded(WINDOWS_1251, &chapter));
        assert!(output.contains(RUSSIAN), "{markup}: {output}");
    }
}

/// What a script or stylesheet's CDATA section holds, and what a DOCTYPE's
/// internal subset holds, is no markup of the chapter's: a `</script>` and a
/// `<meta>` written there are text. Looked for in the text, the `<meta>` after
/// a CDATA section's `"</script><meta charset='iso-8859-1'>"` was found first,
/// and a `<script>` in a DOCTYPE's comment swallowed the real one. The
/// DOCTYPEs, which hold a `]` or a `>` that does not end them, are tried
/// well-formed, which the XML parser reads, and with a markup error, which the
/// HTML parser recovers. The CDATA sections are tried well-formed only: the
/// HTML parser that recovers a malformed chapter ends a script at its first
/// `</script>`, in a CDATA section or not, and the `<meta>` is found as it
/// reads the chapter.
#[test]
fn markup_written_in_cdata_or_a_doctype_does_not_decide_the_encoding() {
    let head = [
        r#"<script><![CDATA[var html = "</script><meta charset='iso-8859-1'>";]]></script>"#,
        r#"<style><![CDATA[p::before { content: "</style><meta charset='iso-8859-1'>" }]]></style>"#,
    ];
    let doctypes = [
        "<!DOCTYPE html [<!-- > <script> -->]>",
        r#"<!DOCTYPE html [<!ENTITY example " > <script>">]>"#,
        r#"<!DOCTYPE html [<!-- ]> <script> --><!ENTITY bracket "]> <script>">]>"#,
    ];
    let chapter = |doctype: &str, head: &str, body_extra: &str| {
        format!(
            r#"{doctype}<html><head>{head}<meta charset="windows-1251"/><title>Title</title></head><body><p>{RUSSIAN}</p>{body_extra}</body></html>"#
        )
    };

    for markup in head {
        let output = read(&encoded(WINDOWS_1251, &chapter("", markup, "")));
        assert!(output.contains(RUSSIAN), "{markup}: {output}");
    }
    for doctype in doctypes {
        for body_extra in ["", "<br>"] {
            let output = read(&encoded(WINDOWS_1251, &chapter(doctype, "", body_extra)));
            assert!(output.contains(RUSSIAN), "{doctype}{body_extra}: {output}");
        }
    }
}

/// A `<![CDATA[` in a script, a stylesheet, a title, a text area or a quoted
/// attribute value is text there, to the HTML parser that recovers the
/// chapter as to a browser, and starts no section for a later `]]>` to end.
/// Taken for one, the section ran from there to the end of a real one further
/// on, the `<meta>` between them was not read, and a windows-1251 chapter came
/// out in Latin letters. A real section in the chapter's text is still text,
/// `<meta>` and all, and so is one that ends inside a title, whose markup
/// libxml2 2.9 reads.
#[test]
fn a_cdata_marker_in_a_script_or_an_attribute_starts_no_section() {
    let markers = [
        r#"<script>var marker = "<![CDATA[";</script>"#,
        r#"<style>p::before { content: "<![CDATA[" }</style>"#,
        r#"<link rel="next" title="<![CDATA[" href="next.html"/>"#,
        "<title>Literal <![CDATA[ syntax</title>",
        "<textarea>Literal <![CDATA[ syntax</textarea>",
        r#"<title><![CDATA[<meta charset="iso-8859-1">]]></title>"#,
    ];
    // A real section after the `<meta>`, as an SVG caption might be, and one
    // before it with a `<meta>` of its own.
    let sections = [
        ("", r#"<svg><desc><![CDATA[Caption]]></desc></svg>"#),
        (r#"<![CDATA[<meta charset="iso-8859-1">]]>"#, ""),
    ];
    for marker in markers {
        for (before, after) in sections {
            let chapter = format!(
                r#"<html><head>{marker}{before}<meta charset="windows-1251"><title>Title</title></head><body><p>{RUSSIAN}</p>{after}</body></html>"#
            );
            let output = read(&encoded(WINDOWS_1251, &chapter));
            assert!(
                output.contains(RUSSIAN),
                "{marker}{before}…{after}: {output}"
            );
        }
    }
}

/// Only a `<meta>`'s own `charset`, `http-equiv` and `content` say anything,
/// not attributes of another vocabulary with the same local names. Read by
/// local name alone, an `x:charset` stood in for the real `charset`, and a
/// windows-1251 chapter came out in Latin letters.
#[test]
fn a_meta_attribute_in_another_namespace_says_nothing() {
    let metas = [
        r#"<meta charset="windows-1251" x:charset="iso-8859-1"/>"#,
        r#"<meta x:charset="iso-8859-1"/><meta charset="windows-1251"/>"#,
        r#"<meta http-equiv="Content-Type" content="text/html; charset=windows-1251" x:content="text/html; charset=iso-8859-1"/>"#,
        r#"<meta x:http-equiv="Content-Type" content="text/html; charset=iso-8859-1"/><meta charset="windows-1251"/>"#,
    ];
    for meta in metas {
        for markup_error in ["", "<br>"] {
            let chapter = format!(
                r#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:x="urn:example"><head>{meta}<title>Title</title></head><body><p>{RUSSIAN}</p>{markup_error}</body></html>"#
            );
            let output = read(&encoded(WINDOWS_1251, &chapter));
            assert!(output.contains(RUSSIAN), "{meta}{markup_error}: {output}");
        }
    }
}

/// Finding the `<meta>` reads each byte before it once, however many tags,
/// comments and scripts stand there, and a CDATA section that never ends is
/// looked for to its end once.
#[test]
fn finding_a_meta_charset_reads_the_chapter_once() {
    let chapters = [
        format!(
            r#"<html><head>{}<meta charset="windows-1251"><title>Title</title></head><body><p>{RUSSIAN}</p></body></html>"#,
            r#"<!--x--><i title="a" lang=b></i><script>"</a>"</script>"#.repeat(20_000)
        ),
        format!(
            r#"<html><head><meta charset="windows-1251"><title>Title</title></head><body><p>{RUSSIAN}</p><p>{}</p></body></html>"#,
            "<![CDATA[x ".repeat(100_000)
        ),
    ];
    for chapter in chapters {
        let bytes = encoded(WINDOWS_1251, &chapter);

        let output =
            common::finishes_within(std::time::Duration::from_secs(20), move || read(&bytes));
        assert!(output.contains(RUSSIAN), "{}", &output[..400]);
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
