use epubkit_core::metadata::{
    extract_metadata, format_filename, strip_store_metadata, update_metadata, MetadataEdits,
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
