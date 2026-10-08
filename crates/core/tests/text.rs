mod common;

use std::time::Duration;

use epubkit_core::text::{clean_string, clean_text_content, TextCleanOptions, TextCleanReport};

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

fn clean_with(body: &str, options: &TextCleanOptions) -> (String, TextCleanReport) {
    let (bytes, report) = clean_text_content(&wrap(body), options).unwrap();
    (String::from_utf8(bytes).expect("utf-8 output"), report)
}

const KEEP_QUOTES: TextCleanOptions = TextCleanOptions {
    fix_whitespace: true,
    fix_ocr: true,
    normalize_quotes: false,
    fix_encoding: true,
    fix_punctuation: true,
    normalize_unicode: true,
    language: String::new(),
};

/// Typographic punctuation is three bytes in UTF-8, so read as Latin-1 it
/// becomes "â" and two invisible C1 controls. Upstream's `test_encoding.py`.
#[test]
fn repairs_utf8_punctuation_read_as_latin1() {
    let (out, report) = clean_with(
        "<p>geht\u{e2}\u{80}\u{99}s and \u{e2}\u{80}\u{9c}quoted\u{e2}\u{80}\u{9d}</p>",
        &KEEP_QUOTES,
    );
    assert!(
        out.contains("geht\u{2019}s and \u{201c}quoted\u{201d}"),
        "{out}"
    );
    assert_eq!(report.encoding_issues_fixed, 3);

    let (out, report) = clean_with(
        "<p>\u{e2}\u{80}\u{98}a\u{e2}\u{80}\u{99} b\u{e2}\u{80}\u{94}c d\u{e2}\u{80}\u{93}e f\u{e2}\u{80}\u{a6}</p>",
        &KEEP_QUOTES,
    );
    assert!(
        out.contains("\u{2018}a\u{2019} b\u{2014}c d\u{2013}e f\u{2026}"),
        "{out}"
    );
    assert_eq!(report.encoding_issues_fixed, 5);
}

/// Upstream's `test_encoding.py`, plus the acute vowels and capital umlauts
/// it added alongside.
#[test]
fn repairs_letters_read_as_latin1() {
    let (out, report) = clean(
        "<p>Verk\u{c3}\u{a4}uferin Kopfh\u{c3}\u{b6}rer Stra\u{c3}\u{9f}e \
         \u{c3}\u{84}rger \u{c3}\u{96}l \u{c3}\u{9c}ber \
         m\u{c3}\u{a1}s s\u{c3}\u{ad} cami\u{c3}\u{b3}n \u{c3}\u{ba}ltimo</p>",
    );
    assert!(
        out.contains("Verkäuferin Kopfhörer Straße Ärger Öl Über más sí camión último"),
        "{out}"
    );
    assert_eq!(report.encoding_issues_fixed, 10);
}

/// Repair comes before the other passes, so what it restores is treated like
/// the rest of the text: a repaired quote is straightened along with every
/// intact one rather than surviving as the only curly quote in the book.
#[test]
fn repaired_punctuation_is_normalized_like_the_rest() {
    let (out, report) = clean("<p>It\u{e2}\u{80}\u{99}s Anna\u{2019}s</p>");
    assert!(out.contains("It's Anna's"), "{out}");
    assert_eq!(report.encoding_issues_fixed, 1);
    assert_eq!(report.smart_quotes_normalized, 2);
}

/// "à" is the bytes C3 A0, and A0 read as Latin-1 is a no-break space, which
/// quote normalization turns into a plain one. Run in the other order, the
/// pattern could never match.
#[test]
fn repairs_an_a_grave_whose_second_byte_is_a_no_break_space() {
    let (out, report) = clean("<p>voil\u{c3}\u{a0} tout</p>");
    assert!(out.contains("voilà tout"), "{out}");
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

/// Space before a mark that does not end a word is not a stray space: it is
/// a calibre, a file extension or a smiley. Gluing it on made "his.45".
#[test]
fn a_space_before_a_mark_that_starts_something_stays() {
    let text = "He drew his .45 and fired. The .NET runtime, a .com site, ok :)";
    let (out, report) = clean(&format!("<p>{text}</p>"));
    assert!(out.contains(text), "{out}");
    assert_eq!(report.total_fixes(), 0, "{report:?}");
}

/// French sets a space before `; : ! ?`, and a no-break space before any of
/// them is someone's deliberate choice in any language.
#[test]
fn french_spacing_and_no_break_spaces_before_punctuation_stay() {
    let french = "C\u{2019}est vrai ? Oui ! Voici : rien ; enfin.";
    let (bytes, _) = clean_text_content(
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xml:lang="fr"><body><p>{french}</p></body></html>
"#
        )
        .as_bytes(),
        &KEEP_QUOTES,
    )
    .unwrap();
    let out = String::from_utf8(bytes).unwrap();
    assert!(out.contains(french), "{out}");

    let (out, _) = clean("<p>Vrai\u{a0}? Yes\u{202f}!</p>");
    assert!(out.contains("Vrai\u{a0}? Yes\u{202f}!"), "{out}");
}

/// Clean a chapter whose root has `root_attributes`, in a book in
/// `book_language`.
fn clean_in(root_attributes: &str, body: &str, book_language: &str) -> String {
    let chapter = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" {root_attributes}><head><title>T</title></head>{body}</html>
"#
    );
    let options = TextCleanOptions {
        language: book_language.to_string(),
        ..KEEP_QUOTES
    };
    let (bytes, _) = clean_text_content(chapter.as_bytes(), &options).unwrap();
    String::from_utf8(bytes).unwrap()
}

/// Text is in the language of the nearest element that gives one, as a
/// browser reads it: a French passage in an English book keeps its spacing,
/// and an English one in a French book does not.
#[test]
fn the_nearest_language_decides_french_spacing() {
    let out = clean_in(
        r#"xml:lang="en" lang="en""#,
        r#"<body><p>Hello ! <span xml:lang="fr">Bonjour ! Comment allez-vous ?</span> Bye !</p><p lang="fr">Oui ! <em lang="en">Yes !</em> Non !</p></body>"#,
        "en",
    );
    for kept in ["Bonjour ! Comment allez-vous ?", "Oui ! ", " Non !"] {
        assert!(out.contains(kept), "{kept}: {out}");
    }
    for fixed in ["Hello! ", " Bye!", "Yes!"] {
        assert!(out.contains(fixed), "{fixed}: {out}");
    }

    let out = clean_in(
        "",
        r#"<body><p>Oui ! <span xml:lang="en-GB">Yes !</span></p></body>"#,
        "fr",
    );
    assert!(out.contains("Oui ! "), "{out}");
    assert!(out.contains("Yes!"), "{out}");
}

/// A body that gives a language other than its root's is in that one.
#[test]
fn a_french_body_under_an_english_root_is_french() {
    let out = clean_in(
        r#"lang="en""#,
        r#"<body xml:lang="fr"><p>Oui ! Non ?</p></body>"#,
        "en",
    );
    assert!(out.contains("Oui ! Non ?"), "{out}");
}

/// `xml:lang` is what XHTML reads where an element gives both.
#[test]
fn xml_lang_outranks_lang() {
    let out = clean_in(
        "",
        r#"<body><p lang="en" xml:lang="fr">Oui !</p><p xml:lang="en" lang="fr">Yes !</p></body>"#,
        "",
    );
    assert!(out.contains("Oui !"), "{out}");
    assert!(out.contains("Yes!"), "{out}");
}

/// A paragraph holding a no-break space is a visible blank line, a scene
/// break; a plain space in its place collapses to nothing. And a no-break
/// space keeps "10 km" or verse indentation together.
#[test]
fn no_break_spaces_are_kept() {
    let (out, _) = clean("<p>\u{a0}</p><p>10\u{a0}km</p><p>\u{a0}\u{a0}\u{a0}Indented</p>");
    assert!(out.contains("<p>\u{a0}</p>"), "{out}");
    assert!(out.contains("10\u{a0}km"), "{out}");
    assert!(out.contains("\u{a0}\u{a0}\u{a0}Indented"), "{out}");
}

/// German opens quotes low: „ and ‚. Folded with the rest, they become
/// straight quotes too, not a comma.
#[test]
fn low_quotes_are_folded_as_quotes() {
    let (out, _) = clean("<p>\u{201e}Komm\u{201c}, sagte sie. \u{201a}Nein\u{2018}</p>");
    assert!(out.contains("\"Komm\", sagte sie. 'Nein'"), "{out}");
}

/// A full stop before a capital is not always a missing space: initials,
/// abbreviations, numbered clauses, file names and web addresses have them
/// too. Only one word ending and another starting is a run-on sentence.
#[test]
fn initials_abbreviations_and_dotted_names_keep_their_dots() {
    let text = "U.S.A., J.R.R. Tolkien, 10 A.M., section 1.E.8, README.TXT, www.Example.Com";
    let (out, _) = clean(&format!("<p>{text}</p>"));
    assert!(out.contains(text), "{out}");

    let (out, _) = clean("<p>It ended.Then it began.</p>");
    assert!(out.contains("It ended. Then it began."), "{out}");
}

/// "Ã" followed by a no-break space or a soft hyphen is "à" or "í" read as
/// Latin-1 inside a word, but after a capital it is a real Portuguese or
/// Vietnamese "Ã".
#[test]
fn a_real_a_tilde_among_capitals_is_left_alone() {
    let text =
        "A MA\u{c7}\u{c3}\u{a0}VERDE, \u{110}\u{c3}\u{a0}\u{110}\u{1ebe}N, IRM\u{c3}\u{ad}ZINHA";
    let (out, report) = clean(&format!("<p>{text}</p>"));
    assert!(out.contains(text), "{out}");
    assert_eq!(report.encoding_issues_fixed, 0);
}

/// Japanese and Chinese write their ellipsis and dash doubled, and a
/// compatibility ideograph is a different glyph that names rely on.
#[test]
fn cjk_punctuation_and_ideographs_are_kept() {
    let text = "\u{5f85}\u{3063}\u{3066}\u{2026}\u{2026}\u{305d}\u{3046}\u{2014}\u{2014}\u{5b9f}\u{306f}\u{3001}\u{fa10}\u{672c}\u{3055}\u{3093}";
    let (out, _) = clean(&format!("<p>{text}</p>"));
    assert!(out.contains(text), "{out}");
}

/// A chapter laid out with indentation has nothing wrong with its spaces.
#[test]
fn indentation_is_not_counted_as_extra_spaces() {
    let (_, report) =
        clean("\n    <p>One.</p>\n    <p>Two.</p>\n    <div>\n        <p>Three.</p>\n    </div>\n");
    assert_eq!(report.total_fixes(), 0, "{report:?}");
}

/// Runs of marks are shortened, not changed: `?!?!` was `!!!`. Two commas
/// before a word open a quote typed on a typewriter.
#[test]
fn punctuation_is_shortened_without_being_changed() {
    let (out, _) = clean("<p>What?!?!?! Wow!!!!! Er sagte ,,Hallo'' und ging,,, weiter.</p>");
    assert!(
        out.contains("What?! Wow!!! Er sagte ,,Hallo'' und ging, weiter."),
        "{out}"
    );
}

/// Typewriter text, variables and mathematics are as literal as code.
#[test]
fn typewriter_variable_and_math_text_is_left_alone() {
    let body = r#"<p><tt>ls  -la ,then</tt> <var>x ,y</var></p><math xmlns="http://www.w3.org/1998/Math/MathML"><mi>a</mi><mo> ,</mo><annotation encoding="TeX">a  ,b</annotation></math>"#;
    let (out, _) = clean(body);
    for literal in ["ls  -la ,then", "x ,y", "<mo> ,</mo>", "a  ,b"] {
        assert!(out.contains(literal), "{literal:?}:\n{out}");
    }
}

/// Old HTML wrote Word's punctuation as `&#146;`, `&#150;` and `&#133;`, the
/// bytes it has in windows-1252, and HTML parsers read them that way. XML
/// reads them as the invisible control characters at those code points.
#[test]
fn control_characters_where_punctuation_belongs_are_read_as_windows_1252() {
    let (out, report) = clean_with(
        "<p>Don&#146;t &#150; wait&#133; &#147;now&#148;</p>",
        &KEEP_QUOTES,
    );
    assert!(
        out.contains("Don\u{2019}t \u{2013} wait\u{2026} \u{201c}now\u{201d}"),
        "{out}"
    );
    assert_eq!(report.encoding_issues_fixed, 5);
}

/// A word the run-on pattern finds in again and again is looked at once. It
/// was read through again for every find, so 36 KB of "word.Word" took ten
/// seconds, four times as long for twice the text. The words after it are
/// still looked at.
#[test]
fn a_long_run_on_word_is_read_once() {
    let word = "word.Word".repeat(50_000);
    let text = format!("{word} ended.Then");

    let (out, report) = common::finishes_within(Duration::from_secs(20), move || {
        let mut report = TextCleanReport::default();
        let out = clean_string(&text, &TextCleanOptions::default(), &mut report);
        (out, report)
    });

    assert!(
        out == format!("{word} ended. Then"),
        "{}",
        &out[out.len() - 40..]
    );
    assert_eq!(report.punctuation_fixed, 1);
}
