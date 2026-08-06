//! The IPC surface, exercised without a window.
//!
//! `#[tauri::command]` leaves the underlying function callable, so everything
//! except the one command needing an `AppHandle` can be tested directly. That
//! matters because a screenshot only proves the page loaded — it says nothing
//! about whether the data crossing the boundary is right.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use epubkit_desktop::commands;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

fn write_epub(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zip = ZipWriter::new(File::create(path).unwrap());
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    for (index, (name, bytes)) in entries.iter().enumerate() {
        zip.start_file(*name, if index == 0 { stored } else { deflated })
            .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

const CONTAINER: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>
"#;

const OPF: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:demo</dc:identifier>
    <dc:title>The Long Afternoon</dc:title>
    <dc:creator>Marguerite Vale</dc:creator>
    <meta name="calibre:series" content="Afternoons"/>
    <meta name="cover" content="cover"/>
  </metadata>
  <manifest>
    <item id="cover" href="cover.png" media-type="image/png"/>
    <item id="ch1" href="c1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>
"#;

const CHAPTER: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body><p>Text.</p></body></html>
"#;

/// A one-pixel PNG, enough to exercise the cover preview.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn demo_epub(path: &Path) {
    write_epub(
        path,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", OPF),
            ("OEBPS/c1.xhtml", CHAPTER),
            ("OEBPS/cover.png", PNG),
        ],
    );
}

#[test]
fn the_device_list_matches_the_core() {
    let devices = commands::devices();

    assert_eq!(devices.len(), epubkit_core::image::DEVICES.len());
    let x4 = devices.iter().find(|d| d.id == "x4").expect("x4 missing");
    assert_eq!((x4.width, x4.height), (480, 800));
    assert_eq!(x4.gray_levels, 4);
}

#[test]
fn inspecting_a_book_returns_what_the_list_shows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.epub");
    demo_epub(&path);

    let books = commands::inspect_books(vec![path.to_string_lossy().to_string()]);

    assert_eq!(books.len(), 1);
    let book = &books[0];
    assert!(book.error.is_none(), "{:?}", book.error);
    assert_eq!(book.title, "The Long Afternoon");
    assert_eq!(book.author, "Marguerite Vale");
    assert_eq!(book.series, "Afternoons");
    assert_eq!(book.filename, "book.epub");
    assert!(book.size > 0);
}

#[test]
fn a_cover_comes_back_as_a_data_url_the_page_can_render() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.epub");
    demo_epub(&path);

    let cover = commands::inspect_books(vec![path.to_string_lossy().to_string()])
        .swap_remove(0)
        .cover
        .expect("the book has a cover");

    assert!(cover.starts_with("data:image/png;base64,"), "{cover}");
}

/// A bad file must not sink the whole drop — it comes back as one failed entry
/// alongside the books that were fine.
#[test]
fn a_broken_file_fails_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.epub");
    let bad = dir.path().join("bad.epub");
    demo_epub(&good);
    std::fs::write(&bad, b"this is not an epub").unwrap();

    let books = commands::inspect_books(vec![
        good.to_string_lossy().to_string(),
        bad.to_string_lossy().to_string(),
        dir.path()
            .join("missing.epub")
            .to_string_lossy()
            .to_string(),
    ]);

    assert_eq!(books.len(), 3);
    assert!(books[0].error.is_none());
    assert!(
        books[1].error.is_some(),
        "a non-EPUB should report an error"
    );
    assert!(
        books[2].error.is_some(),
        "a missing file should report an error"
    );
    // Even a failed entry keeps enough to render a row.
    assert_eq!(books[1].filename, "bad.epub");
}

#[test]
fn a_drm_protected_book_says_so_before_anything_is_processed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drm.epub");

    let encryption = br#"<?xml version="1.0" encoding="UTF-8"?>
<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <EncryptedData xmlns="http://www.w3.org/2001/04/xmlenc#">
    <EncryptionMethod Algorithm="http://www.w3.org/2001/04/xmlenc#aes256-cbc"/>
    <CipherData><CipherReference URI="OEBPS/c1.xhtml"/></CipherData>
  </EncryptedData>
</encryption>
"#;
    write_epub(
        &path,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("META-INF/encryption.xml", encryption),
            ("OEBPS/content.opf", OPF),
        ],
    );

    let book = commands::inspect_books(vec![path.to_string_lossy().to_string()]).swap_remove(0);
    let message = book.error.expect("DRM should be reported");
    assert!(message.contains("DRM"), "{message}");
}

/// The page renders whatever the core hands back, so the payload has to carry
/// the fields the page reads — under the names it reads them by.
#[test]
fn what_crosses_the_boundary_is_shaped_the_way_the_page_expects() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.epub");
    demo_epub(&path);

    let book = commands::inspect_books(vec![path.to_string_lossy().to_string()]).swap_remove(0);
    let json = serde_json::to_value(&book).unwrap();

    for field in [
        "path", "filename", "size", "title", "author", "series", "cover", "error",
    ] {
        assert!(
            json.get(field).is_some(),
            "payload is missing '{field}': {json}"
        );
    }

    let devices = serde_json::to_value(commands::devices()).unwrap();
    let first = &devices[0];
    for field in ["id", "label", "width", "height", "grayLevels"] {
        assert!(
            first.get(field).is_some(),
            "device payload is missing '{field}'"
        );
    }
}
