use epubkit_core::css::{
    collect_used_selectors, remove_embedded_fonts, remove_unused_css, UsedSelectors,
};

fn used_from(body: &str) -> UsedSelectors {
    let xhtml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body>{body}</body></html>
"#
    );
    collect_used_selectors(xhtml.as_bytes()).unwrap()
}

#[test]
fn collects_elements_classes_and_ids() {
    let used = used_from(r#"<p class="lead intro" id="first">Text <em>x</em></p>"#);

    assert!(used.elements.contains("p"));
    assert!(used.elements.contains("em"));
    assert!(used.elements.contains("body"));
    assert!(used.classes.contains("lead"));
    assert!(used.classes.contains("intro"));
    assert!(used.ids.contains("first"));
}

#[test]
fn usage_merges_across_documents() {
    let mut all = used_from(r#"<p class="a">x</p>"#);
    all.merge(&used_from(r#"<div class="b" id="d">y</div>"#));

    assert!(all.classes.contains("a"));
    assert!(all.classes.contains("b"));
    assert!(all.ids.contains("d"));
    assert!(all.elements.contains("div"));
}

#[test]
fn drops_rules_nothing_matches() {
    let used = used_from(r#"<p class="lead">x</p>"#);
    let css = ".lead { color: red; }\n.orphan { color: blue; }\n";

    let (out, removed) = remove_unused_css(css, &used);

    assert_eq!(removed, 1);
    assert!(out.contains(".lead"), "{out}");
    assert!(!out.contains(".orphan"), "{out}");
}

#[test]
fn keeps_rules_for_elements_in_use() {
    let used = used_from("<p>x</p>");
    let (out, removed) = remove_unused_css("p { margin: 0; }\ntable { border: 0; }\n", &used);

    assert_eq!(removed, 1);
    assert!(out.contains('p'), "{out}");
    assert!(!out.contains("table"), "{out}");
}

#[test]
fn keeps_structural_selectors_whatever_the_content() {
    let used = used_from("<p>x</p>");
    let (out, removed) = remove_unused_css(
        "* { box-sizing: border-box; }\nhtml { font-size: 100%; }\nbody { margin: 0; }\n",
        &used,
    );

    assert_eq!(removed, 0, "{out}");
}

/// A static scan of the markup cannot tell whether a pseudo-class or attribute
/// selector will match, so those rules stay.
#[test]
fn keeps_pseudo_and_attribute_selectors() {
    let used = used_from("<p>x</p>");
    let (out, removed) = remove_unused_css(
        "a:hover { color: red; }\np::first-line { font-weight: bold; }\n[hidden] { display: none; }\n",
        &used,
    );

    assert_eq!(removed, 0, "{out}");
}

#[test]
fn keeps_a_rule_when_any_selector_in_the_group_is_used() {
    let used = used_from(r#"<p class="lead">x</p>"#);
    let (out, removed) = remove_unused_css(".orphan, .lead { color: red; }\n", &used);

    assert_eq!(removed, 0, "{out}");
    assert!(out.contains(".lead"), "{out}");
}

#[test]
fn keeps_rules_matching_ids_in_use() {
    let used = used_from(r#"<div id="toc">x</div>"#);
    let (out, removed) = remove_unused_css("#toc { padding: 0; }\n#gone { padding: 0; }\n", &used);

    assert_eq!(removed, 1);
    assert!(out.contains("#toc"), "{out}");
}

#[test]
fn descendant_selectors_are_kept_when_any_part_is_used() {
    let used = used_from(r#"<div class="chapter"><p>x</p></div>"#);
    let (out, removed) = remove_unused_css(".chapter p { text-indent: 1em; }\n", &used);

    assert_eq!(removed, 0, "{out}");
}

/// Only top-level rules are considered, matching the reference. Anything
/// inside an `@media` block is left alone rather than being filtered against
/// markup that may not represent the conditions the block applies to.
#[test]
fn media_block_contents_are_left_alone() {
    let used = used_from("<p>x</p>");
    let (out, removed) = remove_unused_css("@media print { .never-used { color: red; } }\n", &used);

    assert_eq!(removed, 0);
    assert!(out.contains("never-used"), "{out}");
}

#[test]
fn unparseable_css_is_returned_untouched() {
    let used = used_from("<p>x</p>");
    let broken = "@@@ this is not css {{{ ";
    let (out, removed) = remove_unused_css(broken, &used);

    assert_eq!(removed, 0);
    assert_eq!(out, broken);
}

#[test]
fn removes_font_face_rules() {
    let css = r#"@font-face { font-family: "Custom"; src: url(font.otf); }
p { margin: 0; }
@font-face { font-family: "Other"; src: url(other.woff); }
"#;

    let (out, removed) = remove_embedded_fonts(css);

    assert_eq!(removed, 2);
    assert!(!out.contains("@font-face"), "{out}");
    assert!(out.contains('p'), "the ordinary rule should survive: {out}");
}

#[test]
fn a_stylesheet_without_fonts_is_returned_unchanged() {
    let css = "p { margin: 0; }\n";
    let (out, removed) = remove_embedded_fonts(css);

    assert_eq!(removed, 0);
    assert_eq!(out, css, "no rewrite when there is nothing to remove");
}

/// Comments and at-rules have to survive the round-trip; cssutils was prone to
/// dropping them.
#[test]
fn comments_and_imports_survive() {
    let used = used_from(r#"<p class="lead">x</p>"#);
    let css = "/* chapter styles */\n@import url(base.css);\n.lead { color: red; }\n";

    let (out, _) = remove_unused_css(css, &used);

    assert!(out.contains("@import"), "{out}");
    assert!(out.contains(".lead"), "{out}");
}
