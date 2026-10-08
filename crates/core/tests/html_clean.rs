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
    let (out, removed) = strip(r#"<div role="doc-chapter" tabindex="0" accesskey="c">X</div>"#);
    assert_eq!(removed, 3);
    assert!(!out.contains("role="), "{out}");
    assert!(!out.contains("tabindex"), "{out}");
    assert!(!out.contains("accesskey"), "{out}");
}

/// Writing direction and whether something is shown at all are rendering, not
/// interaction. Without `dir`, an Arabic or Hebrew book runs left to right;
/// without `hidden`, a navigation document shows its landmarks list.
#[test]
fn direction_and_visibility_are_kept() {
    let (out, removed) = strip(
        r#"<p dir="rtl">x</p><nav hidden="">y</nav><div inert="">z</div><div popover="">w</div>"#,
    );
    assert_eq!(removed, 0, "{out}");
    for attribute in ["dir=", "hidden=", "inert=", "popover="] {
        assert!(out.contains(attribute), "{attribute} was dropped:\n{out}");
    }
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

/// Under an XHTML 1.1 doctype, which is never loaded, `&bull;` and `&mdash;`
/// stay entity references, and a paragraph made of them has no text as such.
/// A scene-break ornament is content all the same. A paragraph holding only
/// a no-break space is still spacing.
#[test]
fn paragraphs_made_of_entities_are_not_empty() {
    let input = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body>
<p>&nbsp;</p><p class="center">&bull;&nbsp;&bull;&nbsp;&bull;</p><p>&nbsp;</p><p>&nbsp;</p>
<p>The next scene.</p><p>&mdash;</p><p>&mdash;</p>
</body></html>
"#;

    let (bytes, removed) = normalize_whitespace(input).unwrap();
    let out = String::from_utf8(bytes).unwrap();

    assert_eq!(removed, 1, "{out}");
    assert!(out.contains("&bull;&nbsp;&bull;&nbsp;&bull;"), "{out}");
    assert_eq!(out.matches("&mdash;").count(), 2, "{out}");
}

/// An empty element with an id is a link target: a page marker the page list
/// points at, or the anchor a table of contents entry names.
#[test]
fn an_empty_element_with_an_id_is_kept() {
    let (out, removed) = collapse(
        r#"<div xmlns:epub="http://www.idpf.org/2007/ops" epub:type="pagebreak" id="page4"></div><div xmlns:epub="http://www.idpf.org/2007/ops" epub:type="pagebreak" id="page5"></div><div class="spacer"></div><div id="chapter-2"></div><h1>Two</h1>"#,
    );

    assert_eq!(removed, 0, "{out}");
    for id in ["page4", "page5", "chapter-2"] {
        assert!(out.contains(&format!(r#"id="{id}""#)), "{id} went:\n{out}");
    }
}

#[test]
fn adds_a_page_break_rule() {
    let out = String::from_utf8(add_chapter_page_breaks(&wrap("<h1>Ch</h1>")).unwrap()).unwrap();
    assert!(out.contains("page-break-before"), "{out}");
    assert!(out.contains("h1, h2"), "{out}");
    assert!(out.contains(r#"type="text/css""#), "{out}");
    epubkit_core::xml::parse_strict(out.as_bytes()).expect("output should parse");
}

/// The book's own stylesheets come after the added rule, so where they say
/// something else about a heading, at the same specificity, they win.
#[test]
fn the_page_break_rule_gives_way_to_the_books_own_styles() {
    let input = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title><link rel="stylesheet" type="text/css" href="style.css"/><style type="text/css">p { margin: 0; }</style></head><body><h1>Ch</h1></body></html>
"#;

    let out = String::from_utf8(add_chapter_page_breaks(input).unwrap()).unwrap();

    let rule = out.find("page-break-before").expect("the rule was added");
    assert!(rule < out.find("<link").unwrap(), "{out}");
    assert!(rule < out.find("p { margin").unwrap(), "{out}");
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
