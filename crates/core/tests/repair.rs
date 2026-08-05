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
    assert_eq!(
        output.matches("<?xml").count(),
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
