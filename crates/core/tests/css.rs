use epubkit_core::css::{
    collect_used_selectors, decode_stylesheet, remove_embedded_fonts,
    remove_embedded_fonts_from_styles, remove_unused_css, UsedSelectors,
};
use epubkit_core::xml;

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

/// CSS names may contain any non-ASCII character. Reading only ASCII split
/// `.kapitelüberschrift` into the class `kapitel` and an element `berschrift`,
/// neither in use, so a rule the book relies on was removed.
#[test]
fn keeps_rules_whose_names_are_not_ascii() {
    let used = used_from(
        r#"<p class="kapitelüberschrift">x</p><p class="überschrift">y</p><div id="kapitel-ä">z</div>"#,
    );
    let css = ".kapitelüberschrift { font-weight: bold; }\n\
               .überschrift { font-size: 1.2em; }\n\
               #kapitel-ä { margin: 0; }\n\
               .ungenutzt-ö { color: red; }\n";

    let (out, removed) = remove_unused_css(css, &used);

    assert_eq!(removed, 1, "{out}");
    assert!(out.contains(".kapitelüberschrift"), "{out}");
    assert!(out.contains(".überschrift"), "{out}");
    assert!(out.contains("#kapitel-ä"), "{out}");
    assert!(!out.contains("ungenutzt"), "{out}");
}

/// An escaped name is beyond what this scan can read — `.\31 st` is the class
/// `1st` — so, like pseudo-classes, it stays rather than being misread.
#[test]
fn keeps_rules_with_escaped_names() {
    let used = used_from(r#"<p class="1st">x</p><p class="w-1/2">y</p>"#);
    let (out, removed) = remove_unused_css(
        ".\\31 st { color: red; }\n.w-1\\/2 { width: 50%; }\n",
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

    assert!(out.contains("/* chapter styles */"), "{out}");
    assert!(out.contains("@import"), "{out}");
    assert!(out.contains(".lead"), "{out}");
}

/// What stays is left exactly as the book wrote it. Reprinted by a CSS
/// library, media queries and colours came back in syntax older reading
/// engines do not read, `(width <= 600px)` and `#0000`. Comments and the
/// `@charset` went, and a minified sheet came back laid out at length.
#[test]
fn what_is_kept_is_kept_exactly_as_written() {
    let used = used_from(r#"<p class="lead">x</p><p class="end">y</p>"#);
    let before = "@charset \"UTF-8\";\n\
                  /*! licence */\n\
                  @import url(\"print.css\") screen and (max-width: 500px);\n\
                  @media screen and (max-width: 600px) { .lead { color: transparent } }\n\
                  .lead{color:rgba(0,0,0,0.6);font-family:\"Default Sans\";quotes:\"\\201C\" \"\\201D\"}\n";
    let after = ".end{border-bottom:1px solid transparent}\n";
    let css = format!("{before}.orphan {{ color: hsla(0,0%,50%,.5) }}\n{after}");

    let (out, removed) = remove_unused_css(&css, &used);

    assert_eq!(removed, 1);
    assert_eq!(out, format!("{before}{after}"));
}

/// A font declared inside an `@media` block, or in a stylesheet with an old
/// browser hack in it, is as much a font as any. Its file is deleted with the
/// rest, so its rule has to go too.
#[test]
fn every_font_face_rule_goes_wherever_it_is() {
    let css = ".a { *zoom: 1; color: red }\n\
               @font-face { font-family: Top; src: url(top.ttf) }\n\
               @media screen {\n  @font-face { font-family: Nested; src: url(nested.ttf) }\n  p { margin: 0 }\n}\n\
               @supports (display: grid) { @media print { @font-face { font-family: Deep; src: url(deep.ttf) } } }\n";

    let (out, removed) = remove_embedded_fonts(css);

    assert_eq!(removed, 3, "{out}");
    assert!(!out.contains("@font-face"), "{out}");
    assert!(out.starts_with(".a { *zoom: 1; color: red }\n"), "{out}");
    assert!(out.contains("p { margin: 0 }"), "{out}");
}

/// A stylesheet nested thousands deep, which no book needs but any book can
/// contain, is read without running out of stack, even on a thread with as
/// little as the desktop app's workers.
#[test]
fn deeply_nested_css_does_not_exhaust_the_stack() {
    let depth = 20_000;
    let css = format!(
        "{}.inner {{ color: red }}{}\n.orphan {{ color: blue }}\n@font-face {{ font-family: F; src: url(f.ttf) }}\n",
        "@media screen { ".repeat(depth),
        " }".repeat(depth)
    );

    let ((unused, removed_rules), (fonts, removed_fonts)) = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            (
                remove_unused_css(&css, &UsedSelectors::default()),
                remove_embedded_fonts(&css),
            )
        })
        .unwrap()
        .join()
        .expect("reading the stylesheet should not overflow the stack");

    assert_eq!(removed_rules, 1);
    assert!(!unused.contains("orphan"));
    assert!(unused.contains(".inner"));
    assert_eq!(removed_fonts, 1);
    assert!(!fonts.contains("@font-face"));
}

/// A `<style>` element's text and CDATA sections are one stylesheet, as a
/// reading engine reads them. Cleaned one at a time, a rule that started in
/// one and ended in the next was cut in half, and the common
/// `/*<![CDATA[*/ … /*]]>*/` wrapping hid every rule inside it.
#[test]
fn a_style_elements_text_and_cdata_are_one_stylesheet() {
    let chapter = |style: &str| {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title><style type="text/css">{style}</style></head><body><p>x</p></body></html>
"#
        )
    };
    let cleaned = |style: &str| {
        let (out, removed) = remove_embedded_fonts_from_styles(chapter(style).as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        xml::parse_strict(out.as_bytes()).expect("the chapter should stay well-formed");
        (out, removed)
    };

    let (out, removed) = cleaned(
        "@font-face { font-family: Test; <![CDATA[src: url(test.ttf);]]> } p { color: red; }",
    );
    assert_eq!(removed, 1, "{out}");
    assert!(!out.contains("font-face"), "{out}");
    assert!(!out.contains("test.ttf"), "{out}");
    assert!(
        out.contains(r#"<style type="text/css">p { color: red; }</style>"#),
        "{out}"
    );

    let (out, removed) = cleaned(
        "\n/*<![CDATA[*/\n@font-face { font-family: Wrapped; src: url(w.ttf) }\np { margin: 0 }\n/*]]>*/\n",
    );
    assert_eq!(removed, 1, "{out}");
    assert!(
        out.contains(
            "<style type=\"text/css\">\n/*<![CDATA[*/\np { margin: 0 }\n/*]]>*/\n</style>"
        ),
        "{out}"
    );
}

// ------------------------------------------------------------- encodings

#[test]
fn a_stylesheet_that_is_not_utf8_is_read_as_windows_1252() {
    // Latin-1 "©", and windows-1252's "€", which Latin-1 lacks.
    let css = decode_stylesheet(b"/* \xa9 Verlag, 5 \x80 */\np { margin: 0; }\n");
    assert_eq!(css, "/* \u{a9} Verlag, 5 \u{20ac} */\np { margin: 0; }\n");
}

/// An `@charset` naming a legacy encoding is honoured, and rewritten to name
/// UTF-8, which is what the text will be saved as.
#[test]
fn a_declared_encoding_is_honoured_and_redeclared() {
    let css = decode_stylesheet(b"@charset \"iso-8859-1\";\n/* \xa9 */\n");
    assert_eq!(css, "@charset \"UTF-8\";\n/* \u{a9} */\n");

    // Shift_JIS, for the Japanese books Light Novel mode is for.
    let css = decode_stylesheet(
        b"@charset \"Shift_JIS\";\np::before { content: \"\x93\xfa\x96{\x8c\xea\"; }\n",
    );
    assert_eq!(
        css,
        "@charset \"UTF-8\";\np::before { content: \"日本語\"; }\n"
    );
}

#[test]
fn utf8_is_read_as_it_is() {
    let plain = "@charset \"utf-8\";\n/* © */\np { margin: 0; }\n";
    assert_eq!(decode_stylesheet(plain.as_bytes()), plain);

    // A byte order mark is dropped; a file saved as UTF-8 has no need of one.
    let marked = [b"\xef\xbb\xbf".as_slice(), b"/* \xc2\xa9 */"].concat();
    assert_eq!(decode_stylesheet(&marked), "/* \u{a9} */");
}

/// A declaration the bytes contradict is not believed: these bytes are not
/// UTF-8, and an ASCII `@charset` cannot be in UTF-16.
#[test]
fn a_declaration_the_bytes_contradict_is_not_believed() {
    let css = decode_stylesheet(b"@charset \"UTF-8\";\n/* \xa9 */\n");
    assert_eq!(css, "@charset \"UTF-8\";\n/* \u{a9} */\n");

    let css = decode_stylesheet(b"@charset \"utf-16\";\np { margin: 0; }\n");
    assert_eq!(css, "@charset \"UTF-8\";\np { margin: 0; }\n");
}
