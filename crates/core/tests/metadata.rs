use epubkit_core::metadata::{
    check_template, extract_metadata, format_filename, output_filename, strip_store_metadata,
    unused_path, update_metadata, FilenameFormat, FilenameOptions, Metadata, MetadataEdits,
};
use epubkit_core::xml;

fn opf(body: &str) -> libxml::tree::Document {
    xml::parse_strict(body.as_bytes()).expect("fixture should parse")
}

const STANDARD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:identifier id="bookid">urn:uuid:test</dc:identifier>
    <dc:title>The Book Title</dc:title>
    <dc:creator>Jane Author</dc:creator>
    <dc:language>en</dc:language>
    <meta name="calibre:series" content="A Series"/>
    <meta name="calibre:series_index" content="3"/>
  </metadata>
  <manifest>
    <item id="cov" href="images/cover.jpg" media-type="image/jpeg" properties="cover-image"/>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>
"#;

#[test]
fn reads_core_fields() {
    let metadata = extract_metadata(&opf(STANDARD)).unwrap();
    assert_eq!(metadata.title, "The Book Title");
    assert_eq!(metadata.author, "Jane Author");
    assert_eq!(metadata.language, "en");
}

#[test]
fn reads_calibre_series() {
    let metadata = extract_metadata(&opf(STANDARD)).unwrap();
    assert_eq!(metadata.series, "A Series");
    assert_eq!(metadata.series_index, "3");
}

#[test]
fn reads_epub3_collection_series() {
    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>T</dc:title>
    <meta property="belongs-to-collection">Collected Works</meta>
    <meta property="group-position">2</meta>
  </metadata>
  <manifest/><spine/>
</package>
"#);

    let metadata = extract_metadata(&doc).unwrap();
    assert_eq!(metadata.series, "Collected Works");
    assert_eq!(metadata.series_index, "2");
}

/// Plenty of real EPUBs omit the Dublin Core namespace declaration entirely.
/// The reference implementation had a chain of fallbacks for this; so must we.
#[test]
fn reads_fields_without_namespace_declarations() {
    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package version="2.0">
  <metadata>
    <title>Bare Title</title>
    <creator>Bare Author</creator>
    <language>fr</language>
  </metadata>
  <manifest/><spine/>
</package>
"#);

    let metadata = extract_metadata(&doc).unwrap();
    assert_eq!(metadata.title, "Bare Title");
    assert_eq!(metadata.author, "Bare Author");
    assert_eq!(metadata.language, "fr");
}

#[test]
fn finds_cover_via_epub3_properties() {
    let metadata = extract_metadata(&opf(STANDARD)).unwrap();
    assert_eq!(metadata.cover_id, "cov");
    assert_eq!(metadata.cover_href, "images/cover.jpg");
}

#[test]
fn finds_cover_via_epub2_meta() {
    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>T</dc:title>
    <meta name="cover" content="my-cover"/>
  </metadata>
  <manifest>
    <item id="my-cover" href="art/front.png" media-type="image/png"/>
  </manifest>
  <spine/>
</package>
"#);

    let metadata = extract_metadata(&doc).unwrap();
    assert_eq!(metadata.cover_id, "my-cover");
    assert_eq!(metadata.cover_href, "art/front.png");
}

#[test]
fn finds_cover_by_id_as_a_last_resort() {
    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest>
    <item id="ch1" href="c1.xhtml" media-type="application/xhtml+xml"/>
    <item id="the-cover-image" href="cover.jpeg" media-type="image/jpeg"/>
  </manifest>
  <spine/>
</package>
"#);

    let metadata = extract_metadata(&doc).unwrap();
    assert_eq!(metadata.cover_id, "the-cover-image");
    assert_eq!(metadata.cover_href, "cover.jpeg");
}

#[test]
fn no_cover_is_reported_as_empty() {
    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest><item id="ch1" href="c1.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine/>
</package>
"#);

    let metadata = extract_metadata(&doc).unwrap();
    assert!(metadata.cover_id.is_empty());
    assert!(metadata.cover_href.is_empty());
}

#[test]
fn edits_overwrite_existing_fields() {
    let doc = opf(STANDARD);
    let edits = MetadataEdits {
        title: Some("Renamed".into()),
        author: Some("New Author".into()),
        ..MetadataEdits::default()
    };

    update_metadata(&doc, &edits).unwrap();

    let metadata = extract_metadata(&doc).unwrap();
    assert_eq!(metadata.title, "Renamed");
    assert_eq!(metadata.author, "New Author");
    assert_eq!(metadata.language, "en", "untouched fields must survive");
}

#[test]
fn edits_create_missing_fields() {
    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Only A Title</dc:title>
  </metadata>
  <manifest/><spine/>
</package>
"#);

    update_metadata(
        &doc,
        &MetadataEdits {
            author: Some("Added Author".into()),
            ..MetadataEdits::default()
        },
    )
    .unwrap();

    assert_eq!(extract_metadata(&doc).unwrap().author, "Added Author");
}

#[test]
fn empty_edits_change_nothing() {
    let doc = opf(STANDARD);
    let before = extract_metadata(&doc).unwrap();
    update_metadata(&doc, &MetadataEdits::default()).unwrap();
    assert_eq!(extract_metadata(&doc).unwrap(), before);
}

#[test]
fn store_metadata_is_stripped_but_real_metadata_survives() {
    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Keep Me</dc:title>
    <dc:creator>Keep Me Too</dc:creator>
    <meta name="calibre:timestamp" content="2020-01-01"/>
    <meta name="calibre:title_sort" content="Keep Me"/>
    <meta name="ibooks:version" content="1.0"/>
    <meta name="amazon:asin" content="B00X"/>
    <meta name="cover" content="cov"/>
    <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
  </metadata>
  <manifest><item id="cov" href="c.jpg" media-type="image/jpeg"/></manifest>
  <spine/>
</package>
"#);

    let removed = strip_store_metadata(&doc).unwrap();
    assert_eq!(removed, 4, "calibre x2, ibooks, amazon");

    let metadata = extract_metadata(&doc).unwrap();
    assert_eq!(metadata.title, "Keep Me");
    assert_eq!(metadata.author, "Keep Me Too");
    // The cover pointer is not store cruft and must be left alone.
    assert_eq!(metadata.cover_id, "cov");
}

#[test]
fn filenames_combine_author_and_title() {
    assert_eq!(
        format_filename("The Title", "The Author"),
        "The Author - The Title.epub"
    );
}

#[test]
fn filenames_degrade_when_fields_are_missing() {
    assert_eq!(format_filename("Only Title", ""), "Only Title.epub");
    assert_eq!(format_filename("", "Only Author"), "Only Author.epub");
    assert_eq!(format_filename("", ""), "optimized.epub");
    assert_eq!(format_filename("  ", "  "), "optimized.epub");
}

#[test]
fn filenames_are_sanitized() {
    // Slash and backslash become dashes, a colon becomes " -", asterisk,
    // question mark and angle brackets vanish, a double quote becomes an
    // apostrophe, and a pipe becomes a dash.
    assert_eq!(
        format_filename("A/B\\C:D*E?F\"G<H>I|J", ""),
        "A-B-C -DEF'GHI-J.epub"
    );
}

#[test]
fn filenames_collapse_runs_of_spaces_and_dashes() {
    assert_eq!(
        format_filename("Spaced    Out", "Dash--Dash"),
        "Dash-Dash - Spaced Out.epub"
    );
}

/// Control characters are deleted rather than replaced with a space — a tab
/// between two words closes up, matching the reference implementation, which
/// strips `[\x00-\x1f\x7f]` before collapsing whitespace.
#[test]
fn filenames_drop_control_characters() {
    let name = format_filename("Tab\there", "Null\u{0}byte");
    assert!(!name.contains('\t'));
    assert!(!name.contains('\u{0}'));
    assert_eq!(name, "Nullbyte - Tabhere.epub");
}

/// Truncation counts characters, not bytes — slicing a multi-byte codepoint
/// would panic.
#[test]
fn long_multibyte_titles_do_not_panic() {
    let long_title = "é".repeat(500);
    let name = format_filename(&long_title, "");
    assert!(name.ends_with(".epub"));
    assert!(name.chars().count() <= 205);
}

// ------------------------------------------------- filename formats (PR #5)

fn book(title: &str, author: &str) -> Metadata {
    Metadata {
        title: title.into(),
        author: author.into(),
        ..Metadata::default()
    }
}

fn named(metadata: &Metadata, format: FilenameFormat, template: &str, original: &str) -> String {
    let options = FilenameOptions {
        format,
        template: template.into(),
    };
    output_filename(metadata, &options, original).unwrap()
}

/// Upstream's `test_metadata_handler.py`, case for case.
#[test]
fn every_preset_names_the_book_its_own_way() {
    let the_book = book("The Book", "A. Writer");
    assert_eq!(
        named(&the_book, FilenameFormat::Original, "", "upload.epub"),
        "upload.epub"
    );
    assert_eq!(
        named(&the_book, FilenameFormat::TitleAuthor, "", ""),
        "The Book - A. Writer.epub"
    );
    assert_eq!(
        named(&the_book, FilenameFormat::AuthorTitle, "", ""),
        "A. Writer - The Book.epub"
    );
    assert_eq!(
        named(&the_book, FilenameFormat::Title, "", ""),
        "The Book.epub"
    );
}

#[test]
fn a_template_fills_in_metadata_and_the_original_name() {
    let the_book = Metadata {
        year: "2026".into(),
        ..book("The Book", "A/Writer")
    };
    assert_eq!(
        named(
            &the_book,
            FilenameFormat::Custom,
            "{year} - {title} - {author} [{original}]",
            "source.epub"
        ),
        "2026 - The Book - A-Writer [source].epub"
    );

    let in_a_series = Metadata {
        series: "Saga".into(),
        series_index: "2".into(),
        language: "en".into(),
        ..book("The Book", "A. Writer")
    };
    assert_eq!(
        named(
            &in_a_series,
            FilenameFormat::Custom,
            "{series} {series_index} - {title} ({language})",
            ""
        ),
        "Saga 2 - The Book (en).epub"
    );
}

#[test]
fn missing_metadata_and_unsafe_names_fall_back_safely() {
    assert_eq!(
        named(&book("", ""), FilenameFormat::TitleAuthor, "", ""),
        "optimized.epub"
    );
    assert_eq!(
        named(&book("", ""), FilenameFormat::Original, "", "../../.epub"),
        "optimized.epub"
    );
    // Only the original file's name counts, never its directory.
    assert_eq!(
        named(
            &book("", ""),
            FilenameFormat::Original,
            "",
            "/books/Mine.EPUB"
        ),
        "Mine.epub"
    );
}

#[test]
fn a_template_cannot_name_fields_it_does_not_know() {
    let error = check_template("{title} {publisher} {}")
        .unwrap_err()
        .to_string();
    assert!(error.contains("Unknown filename template field"), "{error}");
    assert!(
        error.contains("{publisher}") && error.contains("{}"),
        "{error}"
    );
}

#[test]
fn a_template_cannot_format_its_fields() {
    for template in ["{title:>20}", "{title!r}"] {
        let error = check_template(template).unwrap_err().to_string();
        assert!(
            error.contains("do not support formatting options"),
            "{error}"
        );
    }
}

#[test]
fn a_template_must_say_something_and_close_its_braces() {
    for template in ["", "   ", "{title", "title}", &"x".repeat(201)] {
        assert!(
            check_template(template).is_err(),
            "{template:?} was accepted"
        );
    }
}

#[test]
fn doubled_braces_are_literal_and_the_extension_is_not_doubled() {
    assert_eq!(
        check_template("{{{title}}}").unwrap(),
        "{The Long Afternoon}.epub"
    );
    assert_eq!(
        check_template("{original}.epub").unwrap(),
        "long-afternoon.epub"
    );
}

#[test]
fn the_year_comes_from_the_publication_date() {
    for (date, year) in [
        ("2026-07-31", "2026"),
        ("1999", "1999"),
        ("2019-04-02T00:00:00+00:00", "2019"),
        ("unknown", ""),
    ] {
        let doc = opf(&format!(
            r#"<package xmlns="http://www.idpf.org/2007/opf"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Book</dc:title><dc:date>{date}</dc:date></metadata></package>"#
        ));
        assert_eq!(extract_metadata(&doc).unwrap().year, year, "{date}");
    }
}

/// A finished book never replaces a file — least of all the one it came from,
/// which is exactly where `Original` points.
#[test]
fn an_output_never_replaces_a_file_already_there() {
    let dir = tempfile::tempdir().unwrap();
    let wanted = dir.path().join("Vale - Afternoon.epub");
    assert_eq!(unused_path(&wanted), wanted);

    std::fs::write(&wanted, b"the original").unwrap();
    let second = dir.path().join("Vale - Afternoon (2).epub");
    assert_eq!(unused_path(&wanted), second);

    std::fs::write(&second, b"an earlier run").unwrap();
    assert_eq!(
        unused_path(&wanted),
        dir.path().join("Vale - Afternoon (3).epub")
    );
}

/// Copying onto a dangling link would write wherever it points.
#[cfg(unix)]
#[test]
fn a_dangling_link_counts_as_taken() {
    let dir = tempfile::tempdir().unwrap();
    let wanted = dir.path().join("Afternoon.epub");
    std::os::unix::fs::symlink(dir.path().join("nowhere.epub"), &wanted).unwrap();

    assert_eq!(unused_path(&wanted), dir.path().join("Afternoon (2).epub"));
}
