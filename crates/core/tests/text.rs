use epubkit_core::text::{clean_text_content, TextCleanOptions, TextCleanReport};

fn wrap(body: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body>{body}</body></html>
"#
    )
    .into_bytes()
}

fn clean(body: &str) -> (String, TextCleanReport) {
    let (bytes, report) = clean_text_content(&wrap(body), &TextCleanOptions::default()).unwrap();
    (String::from_utf8(bytes).expect("utf-8 output"), report)
}

#[test]
fn collapses_runs_of_spaces() {
    let (out, report) = clean("<p>Two  spaces   here.</p>");
    assert!(out.contains("<p>Two spaces here.</p>"), "{out}");
    assert_eq!(report.double_spaces_fixed, 2);
}

#[test]
fn removes_space_before_punctuation() {
    let (out, _) = clean("<p>Really ? Yes , indeed .</p>");
    assert!(out.contains("<p>Really? Yes, indeed.</p>"), "{out}");
}

#[test]
fn expands_ocr_ligatures() {
    let (out, report) = clean("<p>The \u{fb01}rst \u{fb02}ight was di\u{fb03}cult.</p>");
    assert!(out.contains("The first flight was difficult."), "{out}");
    assert_eq!(report.ocr_ligatures_fixed, 3);
}

#[test]
fn folds_smart_quotes_and_dashes() {
    let (out, report) = clean(
        "<p>\u{201c}Quoted\u{201d} \u{2018}single\u{2019} em\u{2014}dash en\u{2013}dash\u{2026}</p>",
    );
    assert!(
        out.contains("\"Quoted\" 'single' em--dash en-dash..."),
        "{out}"
    );
    assert_eq!(report.smart_quotes_normalized, 7);
}

#[test]
fn repairs_mojibake() {
    let (out, report) = clean("<p>caf\u{00c3}\u{00a9} na\u{00c3}\u{00af}ve</p>");
    assert!(out.contains("caf\u{00e9}"), "{out}");
    assert_eq!(report.encoding_issues_fixed, 1);
}

#[test]
fn fixes_punctuation() {
    let (out, report) = clean("<p>Wait..... Really,,, yes!!!!!!</p>");
    assert!(out.contains("Wait... Really, yes!!!"), "{out}");
    assert!(report.punctuation_fixed >= 3);
}

#[test]
fn adds_a_missing_space_after_a_sentence() {
    let (out, _) = clean("<p>One sentence.Another sentence.</p>");
    assert!(out.contains("One sentence. Another sentence."), "{out}");
}

/// The single most dangerous thing this module could do is mangle an
/// ampersand. Text node content is stored unescaped and escaped again on
/// serialization, so cleaned text goes back in verbatim — escaping it first
/// would turn every `&` in the book into `&amp;amp;`.
#[test]
fn literal_ampersands_survive() {
    let (out, _) = clean("<p>Marks  &amp; Spencer  &amp; Co.</p>");
    assert!(
        out.contains("Marks &amp; Spencer &amp; Co."),
        "the ampersand was lost or double-escaped:\n{out}"
    );
    assert!(!out.contains("&amp;amp;"), "double-escaped:\n{out}");
}

#[test]
fn angle_brackets_in_text_stay_escaped() {
    let (out, _) = clean("<p>Use  &lt;tag&gt; here</p>");
    assert!(out.contains("&lt;tag&gt;"), "{out}");
    assert!(!out.contains("&amp;lt;"), "double-escaped:\n{out}");
}

#[test]
fn code_and_pre_content_is_left_alone() {
    let (out, _) = clean("<pre>keep    these     spaces</pre><code>a  b</code><p>fix  this</p>");
    assert!(out.contains("keep    these     spaces"), "{out}");
    assert!(out.contains("<code>a  b</code>"), "{out}");
    assert!(out.contains("<p>fix this</p>"), "{out}");
}

#[test]
fn script_and_style_content_is_left_alone() {
    let (out, _) = clean("<script>var a  =  1;</script><style>p  {  color:  red  }</style>");
    assert!(out.contains("var a  =  1;"), "{out}");
    assert!(out.contains("p  {  color:  red  }"), "{out}");
}

/// Text nested deeper inside a skipped element must also be spared.
#[test]
fn nested_content_inside_a_skipped_element_is_left_alone() {
    let (out, _) = clean("<pre><span>deep    spaces</span></pre>");
    assert!(out.contains("deep    spaces"), "{out}");
}

/// lxml stores text following an element as that element's `tail`, so the
/// reference skipped prose after `<code>` along with the code itself. Walking
/// real text nodes means only what is genuinely inside the element is spared.
#[test]
fn prose_after_a_skipped_element_is_still_cleaned() {
    let (out, _) = clean("<p><code>x  y</code> and  then  more</p>");
    assert!(out.contains("<code>x  y</code>"), "code untouched: {out}");
    assert!(out.contains(" and then more"), "tail not cleaned: {out}");
}

#[test]
fn markup_and_attributes_are_preserved() {
    let (out, _) = clean(
        r#"<p class="first" id="p1">Text  with <em>emphasis</em> and <a href="x.html">a  link</a>.</p>"#,
    );
    assert!(out.contains(r#"class="first""#), "{out}");
    assert!(out.contains(r#"<a href="x.html">a link</a>"#), "{out}");
    assert!(out.contains("<em>emphasis</em>"), "{out}");
}

#[test]
fn clean_text_is_left_exactly_as_it_was() {
    let (out, report) = clean("<p>Already clean prose.</p>");
    assert!(out.contains("<p>Already clean prose.</p>"), "{out}");
    assert_eq!(report.total_fixes(), 0);
    assert_eq!(report.summary(), "no text issues found");
}

#[test]
fn options_disable_individual_passes() {
    let options = TextCleanOptions {
        fix_whitespace: false,
        normalize_quotes: false,
        ..TextCleanOptions::default()
    };

    let (bytes, report) =
        clean_text_content(&wrap("<p>Two  spaces \u{201c}quoted\u{201d}</p>"), &options).unwrap();
    let out = String::from_utf8(bytes).unwrap();

    assert!(out.contains("Two  spaces"), "whitespace pass ran: {out}");
    assert!(out.contains('\u{201c}'), "quote pass ran: {out}");
    assert_eq!(report.double_spaces_fixed, 0);
    assert_eq!(report.smart_quotes_normalized, 0);
}

#[test]
fn ligatures_can_be_expanded_without_touching_quotes() {
    let options = TextCleanOptions {
        normalize_quotes: false,
        ..TextCleanOptions::default()
    };

    let (bytes, report) =
        clean_text_content(&wrap("<p>\u{fb01}rst \u{201c}quoted\u{201d}</p>"), &options).unwrap();
    let out = String::from_utf8(bytes).unwrap();

    assert!(out.contains("first"), "{out}");
    assert!(out.contains('\u{201c}'), "{out}");
    assert_eq!(report.ocr_ligatures_fixed, 1);
    assert_eq!(report.smart_quotes_normalized, 0);
}

#[test]
fn reports_merge_across_files() {
    let mut total = TextCleanReport::default();
    let (_, first) = clean("<p>Two  spaces</p>");
    let (_, second) = clean("<p>Three   spaces</p>");

    total.merge(&first);
    total.merge(&second);

    assert_eq!(total.double_spaces_fixed, 2);
    assert_eq!(total.total_fixes(), 2);
    assert!(
        total.summary().contains("2 extra spaces"),
        "{}",
        total.summary()
    );
}

#[test]
fn output_is_well_formed() {
    let (out, _) = clean("<p>Text  with  &amp; ampersand and \u{201c}quotes\u{201d}</p>");
    epubkit_core::xml::parse_strict(out.as_bytes()).expect("cleaned output should parse");
}

/// Malformed input still has to come back cleaned rather than rejected.
#[test]
fn malformed_input_is_recovered_and_cleaned() {
    let broken = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body>
<p>Unclosed  <b>bold with  spaces</p>
</body></html>
"#;

    let (bytes, report) = clean_text_content(broken, &TextCleanOptions::default()).unwrap();
    let out = String::from_utf8(bytes).unwrap();

    assert!(out.contains("bold with spaces"), "{out}");
    assert!(report.double_spaces_fixed >= 2);
    epubkit_core::xml::parse_strict(out.as_bytes()).expect("output should parse");
}
