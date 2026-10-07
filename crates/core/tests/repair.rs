use epubkit_core::html::{default_backend, HtmlRepair, LibxmlRepair};

fn repair(input: &[u8]) -> (String, bool) {
    let backend = LibxmlRepair::new();
    let out = backend.repair(input).expect("repair should succeed");
    (
        String::from_utf8(out.bytes).expect("utf-8 output"),
        out.recovered,
    )
}

const WELL_FORMED: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Chapter</title></head>
<body><p>Plain prose.</p></body>
</html>
"#;

const MALFORMED: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Ch 1</title></head>
<body>
<h1>Chapter One</h1>
<p>An <b>unclosed bold tag and a bare & ampersand.</p>
<p>Another paragraph.
</body>
</html>
"#;

#[test]
fn well_formed_input_is_not_flagged_as_recovered() {
    let (output, recovered) = repair(WELL_FORMED);
    assert!(!recovered);
    assert!(output.contains("Plain prose."));
    assert!(output.contains("http://www.w3.org/1999/xhtml"));
}

#[test]
fn unclosed_tag_is_recovered() {
    let (output, recovered) = repair(MALFORMED);
    assert!(recovered, "malformed input should report recovery");
    assert!(output.contains("unclosed bold tag"));
    assert!(output.contains("Another paragraph."));
}

/// The single most important property of the recovery path: it must not lose
/// text. Running libxml2's *XML* parser in recovery mode deletes a bare `&`
/// and everything the parser was mid-way through; the HTML parser keeps it.
#[test]
fn bare_ampersand_survives_recovery() {
    let (output, recovered) = repair(MALFORMED);
    assert!(recovered);
    assert!(
        output.contains("bare &amp; ampersand"),
        "the ampersand was dropped:\n{output}"
    );
}

/// Recovery output must itself be well-formed — otherwise the next stage of
/// the pipeline inherits a broken document. Feeding the output back in and
/// getting `recovered == false` proves it parses strictly.
#[test]
fn recovered_output_is_well_formed() {
    let (output, recovered) = repair(MALFORMED);
    assert!(recovered);

    let (_, second_pass_recovered) = repair(output.as_bytes());
    assert!(
        !second_pass_recovered,
        "repair produced markup that does not parse strictly:\n{output}"
    );
}

/// libxml2's XHTML serializer injects a `<meta http-equiv="Content-Type">`
/// into every `<head>`, and its HTML parser synthesizes an HTML 4.0 doctype.
/// Neither belongs in the book.
#[test]
fn no_markup_is_injected_during_recovery() {
    let (output, _) = repair(MALFORMED);
    assert!(
        !output.contains("http-equiv"),
        "a meta tag was injected:\n{output}"
    );
    assert!(
        !output.contains("DOCTYPE"),
        "a doctype was injected:\n{output}"
    );
}

/// The HTML parser demotes the source's XML declaration to a processing
/// instruction, which then serializes alongside the one libxml2 writes. Only
/// one may survive, and it must be first.
#[test]
fn exactly_one_xml_declaration_is_emitted() {
    let (output, _) = repair(MALFORMED);
    // The source's own declaration must not survive in any form: libxml2 2.9
    // keeps it as a processing instruction, 2.14 as a `<!--?xml …?-->`
    // comment.
    assert_eq!(
        output.matches("?xml").count(),
        1,
        "expected a single XML declaration:\n{output}"
    );
    assert!(
        output.starts_with("<?xml "),
        "declaration is not first:\n{output}"
    );
}

#[test]
fn namespace_is_preserved_through_recovery() {
    let (output, _) = repair(MALFORMED);
    assert!(
        output.contains(r#"xmlns="http://www.w3.org/1999/xhtml""#),
        "the XHTML namespace was lost:\n{output}"
    );
}

#[test]
fn mismatched_nesting_is_recovered() {
    let broken = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<body><p><em>crossed</p></em></body>
</html>
"#;

    let (output, recovered) = repair(broken);
    assert!(recovered);
    assert!(output.contains("crossed"));
}

/// Repair must not reflow prose. libxml2's formatting option would insert
/// indentation into mixed content and visibly corrupt the text, so the
/// serializer runs with formatting off — this pins that down.
#[test]
fn text_content_is_not_reflowed() {
    let input = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<body><p>One <em>two</em> three <strong>four</strong> five.</p></body>
</html>
"#;

    let (output, recovered) = repair(input);
    assert!(!recovered);
    assert!(
        output.contains("<p>One <em>two</em> three <strong>four</strong> five.</p>"),
        "inline spacing was altered:\n{output}"
    );
}

#[test]
fn entities_are_not_expanded_into_a_bomb() {
    // A "billion laughs" payload. The parser must not expand these into
    // gigabytes of text; it should either refuse the document or leave the
    // references alone.
    let bomb = br#"<?xml version="1.0"?>
<!DOCTYPE lolz [
 <!ENTITY lol "lol">
 <!ENTITY lol1 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
 <!ENTITY lol2 "&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;">
 <!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">
 <!ENTITY lol4 "&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;">
 <!ENTITY lol5 "&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;">
 <!ENTITY lol6 "&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;">
]>
<html xmlns="http://www.w3.org/1999/xhtml"><body><p>&lol6;</p></body></html>
"#;

    if let Ok(out) = LibxmlRepair::new().repair(bomb) {
        assert!(
            out.bytes.len() < 100_000,
            "entities were expanded: {} bytes",
            out.bytes.len()
        );
    }
    // Refusing the document outright is an equally acceptable outcome.
}

/// Void elements must stay closed. This is the one place the reference
/// implementation actually produces markup that is not well-formed XHTML: it
/// serializes with `method='html'`, which writes `<br>`, `<img>` and `<hr>`
/// unclosed. XML serialization keeps them self-closing.
#[test]
fn void_elements_stay_closed_through_recovery() {
    let broken = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Void</title></head>
<body>
<p>Before<br/>after an <b>unclosed bold</p>
<img src="pic.jpg" alt="a"/>
<hr/>
</body>
</html>
"#;

    let (output, recovered) = repair(broken);
    assert!(recovered);
    assert!(output.contains("<br/>"), "br was unclosed:\n{output}");
    assert!(output.contains("<hr/>"), "hr was unclosed:\n{output}");
    assert!(
        output.contains(r#"<img src="pic.jpg" alt="a"/>"#),
        "img was unclosed:\n{output}"
    );

    // And the whole thing still parses strictly.
    let (_, second_pass_recovered) = repair(output.as_bytes());
    assert!(
        !second_pass_recovered,
        "output is not well-formed:\n{output}"
    );
}

#[test]
fn default_backend_is_libxml2() {
    assert_eq!(default_backend().name(), "libxml2");
}

/// An empty or blank file holds nothing to recover. Depending on the libxml2
/// release, recovering one either fails or yields a document with no root
/// element, which would serialize to a bare declaration: not XHTML at all.
/// Both are refused, the same on every release.
#[test]
fn a_file_with_nothing_in_it_is_refused() {
    for input in ["", "   \n", "\u{feff}"] {
        assert!(
            LibxmlRepair::new().repair(input.as_bytes()).is_err(),
            "{input:?} was not refused"
        );
    }
}

/// libxml2 has a serializer of its own for XHTML 1.0, chosen by the doctype
/// alone. It injects a `<meta http-equiv>`, copies each `<a name>` into an
/// `id` that duplicates the heading's, and mirrors `lang` into `xml:lang`. A
/// well-formed chapter comes back as it was.
#[test]
fn an_xhtml_1_0_chapter_is_not_rewritten_as_xhtml_1_0_would_be_served() {
    let input = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd">
<html xmlns="http://www.w3.org/1999/xhtml" lang="en"><head><title>T</title></head><body><h2 id="chap01"><a name="chap01"></a>One</h2><p>Text.<br/>More.</p></body></html>
"#;

    let repaired = LibxmlRepair::new().repair(input).unwrap();
    let out = String::from_utf8(repaired.bytes).unwrap();

    assert!(!repaired.recovered);
    assert!(!out.contains("http-equiv"), "{out}");
    assert_eq!(out.matches(r#"id="chap01""#).count(), 1, "{out}");
    assert!(!out.contains("xml:lang"), "{out}");
    assert!(out.contains("<br/>"), "{out}");
}

/// A strict re-read of what repair produced: every chapter it hands on has to
/// be well-formed XHTML, or each later pass recovers it all over again.
fn assert_well_formed(out: &str) {
    epubkit_core::xml::parse_strict(out.as_bytes())
        .unwrap_or_else(|e| panic!("not well-formed ({e}):\n{out}"));
}

/// A NUL cost libxml2 2.9 everything after it, and other control characters
/// went missing between words or into attributes raw. They are spaces now,
/// under every release.
#[test]
fn control_characters_cost_nothing_around_them() {
    let (out, _) = repair(
        b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>One.</p>\0<p>Two.</p>\
          <p class=\"a\x0bb\">Line\x0bone\x0cline\x07two\x1bdone.</p><p>Last.<br></p></body></html>",
    );

    for text in ["One.", "Two.", "Line one line two done.", "Last."] {
        assert!(out.contains(text), "{text:?} is missing:\n{out}");
    }
    assert_well_formed(&out);
}

/// A byte order mark that went through windows-1252 and back, "ï»¿", or a
/// stray U+FEFF after the declaration, sits before the root element. The
/// HTML parser took it for body text, opened an implied `<html><body>` and
/// dropped the real `<html>` and `<head>` with their namespace and language.
#[test]
fn a_stray_byte_order_mark_does_not_cost_the_chapter_its_head() {
    for start in ["\u{ef}\u{bb}\u{bf}", "\u{feff}\u{feff}", ""] {
        for between in ["", "\u{feff}", "\n\u{feff}\n"] {
            let chapter = format!(
                "{start}<?xml version=\"1.0\" encoding=\"utf-8\"?>{between}<html xmlns=\"http://www.w3.org/1999/xhtml\" xml:lang=\"de\"><head><title>Titel</title><link rel=\"stylesheet\" type=\"text/css\" href=\"s.css\"/></head><body><p>Text.<br></p></body></html>"
            );
            let (out, _) = repair(chapter.as_bytes());

            assert!(
                out.contains(r#"<html xmlns="http://www.w3.org/1999/xhtml" xml:lang="de"><head>"#),
                "{start:?} {between:?}:\n{out}"
            );
            assert!(
                !out.contains('\u{feff}') && !out.contains('\u{ef}'),
                "{out}"
            );
            assert_well_formed(&out);
        }
    }
}

/// libxml2's HTML parser cannot read a DOCTYPE's internal subset, and stops it
/// at the first `>`; the rest of the declarations became text. The entities
/// such a subset declares are filled in.
#[test]
fn an_internal_subset_is_read_before_recovery() {
    let (out, recovered) = repair(
        br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html [ <!ENTITY author "Jane Doe"> <!ENTITY copy '&#169;'> ]>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body><p>By &author;, &copy; 2001 &amp; ever since.<br></p></body></html>
"#,
    );

    assert!(recovered);
    assert!(
        out.contains("By Jane Doe, \u{a9} 2001 &amp; ever since."),
        "{out}"
    );
    assert!(!out.contains("ENTITY"), "{out}");
    assert_well_formed(&out);
}

fn nested(depth: usize, closed: bool) -> String {
    let mut chapter = String::from(
        r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body>"#,
    );
    for n in 1..=depth {
        chapter.push_str(&format!(r#"<div class="para">Paragraph {n}."#));
    }
    if closed {
        chapter.push_str(&"</div>".repeat(depth));
    }
    chapter.push_str("<p>THE END</p></body></html>");
    chapter
}

/// libxml2 stops at 256 levels of nesting and hands back what it has. 300
/// `<div>`s nobody closed came back as 255, the rest of the chapter gone, and
/// a well-formed chapter nested that deep lost all its text; both counted as
/// repaired.
#[test]
fn deep_nesting_is_not_cut_short() {
    for closed in [false, true] {
        let (out, _) = repair(nested(300, closed).as_bytes());

        assert!(out.contains("Paragraph 300."), "closed: {closed}\n{out}");
        assert!(out.contains("THE END"), "closed: {closed}\n{out}");
        // Read back past the strict parser's own 256 levels.
        let deep = libxml::parser::ParserOptions {
            huge: true,
            ..libxml::parser::ParserOptions::default()
        };
        libxml::parser::Parser::default()
            .parse_string_with_options(&out, deep)
            .unwrap_or_else(|e| panic!("not well-formed ({e}):\n{out}"));
    }
}

/// Past even the raised limit, recovery would lose the rest of the chapter.
/// It refuses instead, which leaves the chapter as it was; it never hands
/// back part of one. (libxml2 2.9 recovers this depth whole; 2.14 stops at
/// 2048 levels and has to refuse.)
#[test]
fn recovery_never_hands_back_part_of_a_chapter() {
    if let Ok(out) = LibxmlRepair::new().repair(nested(5000, false).as_bytes()) {
        let out = String::from_utf8(out.bytes).unwrap();
        assert!(out.contains("Paragraph 5000."), "truncated");
        assert!(out.contains("THE END"), "truncated");
    }
}
