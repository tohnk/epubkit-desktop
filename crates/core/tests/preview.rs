mod common;

use std::path::Path;

use epubkit_core::preview::read_preview;

const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n not a whole image, but a cover all the same";

fn opf(cover_href: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:preview</dc:identifier>
    <dc:title>The Long Afternoon</dc:title>
    <dc:creator>Marguerite Vale</dc:creator>
    <meta name="calibre:series" content="Afternoons"/>
    <meta name="cover" content="cover"/>
  </metadata>
  <manifest>
    <item id="cover" href="{cover_href}" media-type="image/png"/>
    <item id="ch1" href="text/c1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>
"#
    )
}

fn book(path: &Path, cover_href: &str, entries: &[(&str, &[u8])]) {
    let opf = opf(cover_href);
    let mut all: Vec<(&str, &[u8])> = vec![
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", common::CONTAINER_XML),
        ("OEBPS/content.opf", opf.as_bytes()),
        ("OEBPS/text/c1.xhtml", common::CHAPTER_XHTML),
    ];
    all.extend_from_slice(entries);
    common::write_epub(path, &all);
}

#[test]
fn reads_metadata_and_cover_from_the_archive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.epub");
    book(
        &path,
        "images/cover.png",
        &[("OEBPS/images/cover.png", PNG_BYTES)],
    );

    let preview = read_preview(&path, u64::MAX).unwrap();

    assert_eq!(preview.metadata.title, "The Long Afternoon");
    assert_eq!(preview.metadata.author, "Marguerite Vale");
    assert_eq!(preview.metadata.series, "Afternoons");
    let cover = preview.cover.expect("the book has a cover");
    assert_eq!(cover.bytes, PNG_BYTES);
    assert_eq!(cover.media_type, "image/png");
    assert_eq!(cover.path, "OEBPS/images/cover.png");
}

/// An href is a URL: a space in the name arrives as `%20`, and the path is
/// relative to the package document, `..` and all.
#[test]
fn a_cover_href_is_read_as_a_url_relative_to_the_package() {
    let dir = tempfile::tempdir().unwrap();

    let encoded = dir.path().join("encoded.epub");
    book(
        &encoded,
        "images/cover%20art.png",
        &[("OEBPS/images/cover art.png", PNG_BYTES)],
    );
    assert!(read_preview(&encoded, u64::MAX).unwrap().cover.is_some());

    let climbing = dir.path().join("climbing.epub");
    book(
        &climbing,
        "../Images/cover.png",
        &[("Images/cover.png", PNG_BYTES)],
    );
    let cover = read_preview(&climbing, u64::MAX).unwrap().cover;
    assert_eq!(cover.map(|c| c.path).as_deref(), Some("Images/cover.png"));
}

/// The href comes from the book, so it must not reach the filesystem: not
/// as an absolute path, and not by climbing out of the archive.
#[test]
fn a_cover_href_cannot_reach_outside_the_archive() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.png");
    std::fs::write(&secret, PNG_BYTES).unwrap();
    let path = dir.path().join("book.epub");

    for href in [
        secret.to_string_lossy().to_string(),
        "../../secret.png".to_string(),
        "../../../../../../../../../..".to_string() + &secret.to_string_lossy(),
    ] {
        book(&path, &href, &[]);
        let preview = read_preview(&path, u64::MAX).unwrap();
        assert!(preview.cover.is_none(), "{href} reached a file");
    }
}

#[test]
fn a_cover_over_the_limit_is_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.epub");
    book(&path, "cover.png", &[("OEBPS/cover.png", PNG_BYTES)]);

    let preview = read_preview(&path, PNG_BYTES.len() as u64 - 1).unwrap();
    assert!(preview.cover.is_none());
    assert_eq!(
        preview.metadata.title, "The Long Afternoon",
        "the rest still reads"
    );
}

/// Without a usable container, the package document is found by looking.
#[test]
fn a_book_without_a_container_still_previews() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.epub");
    let opf = opf("cover.png");
    common::write_epub(
        &path,
        &[
            ("mimetype", b"application/epub+zip"),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/cover.png", PNG_BYTES),
        ],
    );

    let preview = read_preview(&path, u64::MAX).unwrap();
    assert_eq!(preview.metadata.title, "The Long Afternoon");
    assert!(preview.cover.is_some());
}

#[test]
fn something_that_is_not_an_epub_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not.epub");
    std::fs::write(&path, b"plain text").unwrap();

    assert!(read_preview(&path, u64::MAX).is_err());
}
