use epubkit_core::html::{
    add_chapter_page_breaks, normalize_whitespace, strip_unnecessary_attributes,
};

fn wrap(body: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body>{body}</body></html>
"#
    )
    .into_bytes()
}

fn strip(body: &str) -> (String, usize) {
    let (bytes, count) = strip_unnecessary_attributes(&wrap(body)).unwrap();
    (String::from_utf8(bytes).unwrap(), count)
}

fn collapse(body: &str) -> (String, usize) {
    let (bytes, count) = normalize_whitespace(&wrap(body)).unwrap();
    (String::from_utf8(bytes).unwrap(), count)
}

#[test]
fn strips_data_and_aria_attributes() {
    let (out, removed) = strip(r#"<p data-page="3" aria-label="para" data-foo="x">Text</p>"#);
    assert_eq!(removed, 3);
    assert!(!out.contains("data-page"), "{out}");
    assert!(!out.contains("aria-label"), "{out}");
    assert!(out.contains("Text"), "{out}");
}

#[test]
fn strips_interaction_attributes() {
    let (out, removed) = strip(r#"<div role="doc-chapter" tabindex="0" hidden="hidden">X</div>"#);
    assert_eq!(removed, 3);
    assert!(!out.contains("role="), "{out}");
    assert!(!out.contains("tabindex"), "{out}");
    assert!(!out.contains("hidden"), "{out}");
}

#[test]
fn keeps_attributes_that_affect_rendering() {
    let (out, removed) = strip(
        r#"<p class="c" id="i" style="color:red" lang="en" title="t">a</p><img src="x.jpg" alt="A" width="10" height="20"/><td colspan="2" rowspan="3">c</td>"#,
    );
    assert_eq!(removed, 0, "nothing here should have been stripped: {out}");
    for attribute in [
        "class=", "id=", "style=", "lang=", "title=", "src=", "alt=", "width=", "height=",
        "colspan=", "rowspan=",
    ] {
        assert!(out.contains(attribute), "{attribute} was dropped:\n{out}");
    }
}

#[test]
fn keeps_links_intact() {
    let (out, removed) = strip(r#"<a href="chapter2.xhtml" rel="next">Next</a>"#);
    assert_eq!(removed, 0);
    assert!(out.contains(r#"href="chapter2.xhtml""#), "{out}");
    assert!(out.contains(r#"rel="next""#), "{out}");
}

#[test]
fn a_document_with_nothing_to_strip_is_returned_unchanged() {
    let input = wrap("<p>Plain.</p>");
    let (bytes, removed) = strip_unnecessary_attributes(&input).unwrap();
    assert_eq!(removed, 0);
    assert_eq!(bytes, input, "the file should not have been rewritten");
}

#[test]
fn collapses_runs_of_empty_paragraphs() {
    let (out, removed) = collapse("<p>Real</p><p></p><p></p><p></p><p>More</p>");
    assert_eq!(removed, 2, "one of the three empties should remain");
    assert!(out.contains("Real"), "{out}");
    assert!(out.contains("More"), "{out}");
}

#[test]
fn a_single_empty_paragraph_is_left_alone() {
    let input = wrap("<p>Real</p><p></p><p>More</p>");
    let (bytes, removed) = normalize_whitespace(&input).unwrap();
    assert_eq!(removed, 0);
    assert_eq!(bytes, input);
}

#[test]
fn empty_divs_collapse_too() {
    let (_, removed) = collapse("<div></div><div></div><div></div>");
    assert_eq!(removed, 2);
}

/// A paragraph holding a line break or an image is not empty, however little
/// text it has.
#[test]
fn elements_with_children_are_not_empty() {
    let (_, removed) = collapse(r#"<p><br/></p><p><img src="x.jpg" alt=""/></p><p><br/></p>"#);
    assert_eq!(removed, 0);
}

#[test]
fn prose_between_empty_paragraphs_survives() {
    let (out, removed) = collapse("<p></p><p></p><p>Keep this sentence.</p><p></p><p></p>");
    assert_eq!(removed, 2);
    assert!(out.contains("Keep this sentence."), "{out}");
}

#[test]
fn whitespace_only_paragraphs_count_as_empty() {
    let (_, removed) = collapse("<p>   </p><p>\n\t</p><p> </p>");
    assert_eq!(removed, 2);
}

#[test]
fn adds_a_page_break_rule() {
    let out = String::from_utf8(add_chapter_page_breaks(&wrap("<h1>Ch</h1>")).unwrap()).unwrap();
    assert!(out.contains("page-break-before"), "{out}");
    assert!(out.contains("h1, h2"), "{out}");
    assert!(out.contains(r#"type="text/css""#), "{out}");
    epubkit_core::xml::parse_strict(out.as_bytes()).expect("output should parse");
}

#[test]
fn an_existing_page_break_rule_is_not_duplicated() {
    let input = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><style type="text/css">h1 { page-break-before: always; }</style></head>
<body><h1>Ch</h1></body></html>
"#;

    let out = add_chapter_page_breaks(input).unwrap();
    assert_eq!(out, input.to_vec(), "the document should be untouched");
    assert_eq!(
        String::from_utf8(out)
            .unwrap()
            .matches("page-break-before")
            .count(),
        1
    );
}

#[test]
fn a_document_without_a_head_is_left_alone() {
    let input = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body><h1>Ch</h1></body></html>
"#;

    assert_eq!(add_chapter_page_breaks(input).unwrap(), input.to_vec());
}
