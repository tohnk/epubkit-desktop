//! The IPC surface, exercised without a window.
//!
//! `#[tauri::command]` leaves the underlying function callable, so everything
//! except the one command needing an `AppHandle` can be tested directly — and
//! that one does its work through `optimize_one`, which can be too. That
//! matters because a screenshot only proves the page loaded — it says nothing
//! about whether the data crossing the boundary is right.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use epubkit_core::metadata::{FilenameFormat, FilenameOptions, TEMPLATE_FIELDS};
use epubkit_core::settings::Settings;
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

/// Run the command to completion, as the window's async runtime would.
fn inspect(paths: Vec<String>) -> Vec<commands::BookInfo> {
    tauri::async_runtime::block_on(commands::inspect_books(paths)).expect("the command should run")
}

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

    let books = inspect(vec![path.to_string_lossy().to_string()]);

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

    let cover = inspect(vec![path.to_string_lossy().to_string()])
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

    let books = inspect(vec![
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

    let book = inspect(vec![path.to_string_lossy().to_string()]).swap_remove(0);
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

    let book = inspect(vec![path.to_string_lossy().to_string()]).swap_remove(0);
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

// ------------------------------------------------------------------- naming

fn job(path: &Path) -> commands::Job {
    commands::Job {
        path: path.to_string_lossy().to_string(),
        title: None,
        author: None,
    }
}

fn naming(format: FilenameFormat) -> Settings {
    Settings {
        filename: FilenameOptions {
            format,
            ..FilenameOptions::default()
        },
        ..Settings::default()
    }
}

/// The window names its books as chosen, not only the CLI.
#[test]
fn a_book_is_written_under_the_chosen_name() {
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("book.epub");
    demo_epub(&book);
    let out = tempfile::tempdir().unwrap();

    let outcome = commands::optimize_one(
        &job(&book),
        out.path(),
        &naming(FilenameFormat::TitleAuthor),
        0,
        |_, _| {},
    );

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    let output = PathBuf::from(outcome.output.expect("a book was written"));
    assert_eq!(
        output.file_name().unwrap(),
        "The Long Afternoon - Marguerite Vale.epub"
    );
    // And nothing but the book is left behind.
    let left: Vec<_> = std::fs::read_dir(out.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(left, [output.file_name().unwrap()]);
}

/// Keeping the original name, in the folder the book came from, must not
/// replace the book.
#[test]
fn keeping_the_original_name_never_replaces_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("book.epub");
    demo_epub(&book);
    let before = std::fs::read(&book).unwrap();

    let outcome = commands::optimize_one(
        &job(&book),
        dir.path(),
        &naming(FilenameFormat::Original),
        0,
        |_, _| {},
    );

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    let output = PathBuf::from(outcome.output.expect("a book was written"));
    assert_eq!(output.file_name().unwrap(), "book (2).epub");
    assert_eq!(std::fs::read(&book).unwrap(), before);
}

/// The page asks before a run whether a template will do, and shows the
/// answer as it is typed.
#[test]
fn a_filename_template_is_checked_before_a_run() {
    assert_eq!(
        commands::check_filename_template("{author} - {title}".into()).unwrap(),
        "Marguerite Vale - The Long Afternoon.epub"
    );

    let error = commands::check_filename_template("{publisher}".into()).unwrap_err();
    assert!(error.contains("{publisher}"), "{error}");
}

// ------------------------------------------------------- the page's contract

fn page() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/index.html"))
        .expect("the page should be readable")
}

/// The values of every `name="…"` attribute on the page, in order.
fn attribute_values<'a>(html: &'a str, name: &str) -> Vec<&'a str> {
    let marker = format!("{name}=\"");
    html.match_indices(&marker)
        .map(|(at, _)| {
            let rest = &html[at + marker.len()..];
            &rest[..rest.find('"').expect("unterminated attribute")]
        })
        .collect()
}

/// Every `data-option` the page binds to must exist in the serialized options.
///
/// This is the test that was missing. `OptionSet` serializes snake_case (which
/// keeps `settings.toml` hand-editable) while the page was written asking for
/// camelCase, so every checkbox silently read `undefined` — showing unchecked
/// whatever the setting was, and writing a key the core discards. Nothing
/// failed; the options just quietly did nothing.
///
/// Reading the real HTML rather than a copied list is the point: the two files
/// cannot drift apart without this noticing.
#[test]
fn the_page_binds_to_option_keys_that_exist() {
    let html = page();
    let bound = attribute_values(&html, "data-option");

    assert!(
        bound.len() >= 7,
        "expected the page to bind every option, found {bound:?}"
    );

    let options = serde_json::to_value(epubkit_core::settings::OptionSet::full()).unwrap();
    for key in &bound {
        assert!(
            options.get(key).is_some(),
            "the page binds '{key}', which is not a field of OptionSet.\n\
             Serialized keys are: {:?}",
            options.as_object().unwrap().keys().collect::<Vec<_>>()
        );
    }

    // And the reverse: an option the core gained but the page never exposes.
    for key in options.as_object().unwrap().keys() {
        // Quality has its own slider rather than a checkbox.
        if key == "quality" {
            continue;
        }
        assert!(
            bound.contains(&key.as_str()),
            "OptionSet has '{key}' but the page never binds it"
        );
    }
}

/// The page offers every way of naming a book, under the names the settings
/// carry — it writes the one clicked straight into `settings.filename`.
#[test]
fn the_page_offers_every_filename_format() {
    let html = page();
    let offered = attribute_values(&html, "data-filename-format");

    let names: Vec<&str> = FilenameFormat::ALL.iter().map(|f| f.name()).collect();
    assert_eq!(offered, names);

    for format in FilenameFormat::ALL {
        assert_eq!(serde_json::to_value(format).unwrap(), format.name());
    }
}

/// The fields the page lists for a template are the ones the core fills in.
#[test]
fn the_page_lists_every_template_field() {
    let html = page();
    let marker = "id=\"filename-fields\">";
    let at = html
        .find(marker)
        .expect("the page lists the template fields")
        + marker.len();
    let note = &html[at..at + html[at..].find('<').expect("unterminated note")];

    let listed: Vec<&str> = note
        .split('{')
        .skip(1)
        .map(|rest| &rest[..rest.find('}').expect("unterminated field")])
        .collect();
    assert_eq!(listed, TEMPLATE_FIELDS);
}

/// The settings payload the page reads has to carry these under these names.
#[test]
fn the_settings_payload_is_shaped_the_way_the_page_expects() {
    let json = serde_json::to_value(epubkit_core::settings::Settings::default()).unwrap();

    for field in ["device", "options", "active", "presets", "filename"] {
        assert!(json.get(field).is_some(), "settings is missing '{field}'");
    }
    for field in ["format", "template"] {
        assert!(
            json["filename"].get(field).is_some(),
            "settings.filename is missing '{field}'"
        );
    }

    let mut settings = epubkit_core::settings::Settings::default();
    settings.save_preset("Example").unwrap();
    let json = serde_json::to_value(&settings).unwrap();

    let preset = &json["presets"][0];
    for field in ["id", "name", "options"] {
        assert!(preset.get(field).is_some(), "a preset is missing '{field}'");
    }
}

/// The cover's href is a URL relative to the package document, so a space in
/// its name arrives as `%20`.
#[test]
fn a_cover_whose_name_needs_escaping_still_shows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.epub");
    let opf = String::from_utf8(OPF.to_vec())
        .unwrap()
        .replace(r#"href="cover.png""#, r#"href="cover%20art.png""#);
    write_epub(
        &path,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/c1.xhtml", CHAPTER),
            ("OEBPS/cover art.png", PNG),
        ],
    );

    let book = inspect(vec![path.to_string_lossy().to_string()]).swap_remove(0);
    assert!(book.cover.is_some(), "{:?}", book.error);
}

/// The cover's href comes from the book. It must never lead the app to read a
/// file of the user's into the page.
#[test]
fn a_cover_href_cannot_reach_a_file_outside_the_book() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.png");
    std::fs::write(&secret, PNG).unwrap();
    let path = dir.path().join("book.epub");

    let opf = String::from_utf8(OPF.to_vec()).unwrap().replace(
        r#"href="cover.png""#,
        &format!(r#"href="{}""#, secret.display()),
    );
    write_epub(
        &path,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/c1.xhtml", CHAPTER),
        ],
    );

    let book = inspect(vec![path.to_string_lossy().to_string()]).swap_remove(0);
    assert!(book.error.is_none(), "{:?}", book.error);
    assert!(book.cover.is_none(), "the cover came from outside the book");
}
