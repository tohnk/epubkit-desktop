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
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine;
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

    // A thumbnail, whatever the cover was.
    assert!(cover.starts_with("data:image/jpeg;base64,"), "{cover}");
}

/// A folder dropped on the window gives the books in it, and in the folders
/// in it, in the order a listing of each folder gives them; a book dropped
/// on its own is itself. A dropped folder did nothing, and said nothing. What
/// is hidden is left out: macOS writes a `._` file beside each book on a
/// drive it does not format, an SD card say, and it too ends in `.epub`.
#[test]
fn a_dropped_folder_gives_the_books_in_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Books");
    for file in [
        "b.EPUB",
        "a.epub",
        "notes.txt",
        "._a.epub",
        ".hidden/d.epub",
        "Series/c.epub",
        "Series/Later/e.epub",
    ] {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
    }
    let alone = dir.path().join("alone.epub");
    std::fs::write(&alone, b"").unwrap();
    let text = |path: &Path| path.to_string_lossy().to_string();

    let found = tauri::async_runtime::block_on(commands::find_books(vec![
        text(&root),
        text(&alone),
        text(&dir.path().join("notes.txt")),
        text(&dir.path().join("missing")),
    ]))
    .unwrap();

    let expected: Vec<String> = [
        root.join("a.epub"),
        root.join("b.EPUB"),
        root.join("Series/c.epub"),
        root.join("Series/Later/e.epub"),
        alone,
    ]
    .iter()
    .map(|path| text(path))
    .collect();
    assert_eq!(found, expected);
}

/// A folder linked into a dropped one is looked through as any other: a
/// library kept as links to folders gave none of the books in them. A link
/// back up the tree is not followed round for ever.
#[cfg(unix)]
#[test]
fn a_dropped_folder_gives_the_books_in_folders_linked_into_it() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = dir.path().join("Elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("f.epub"), b"").unwrap();
    let root = dir.path().join("Library");
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join("Linked")).unwrap();
    std::os::unix::fs::symlink(&root, root.join("Up")).unwrap();

    let found = tauri::async_runtime::block_on(commands::find_books(vec![root
        .to_string_lossy()
        .to_string()]))
    .unwrap();

    assert_eq!(
        found,
        [root.join("Linked/f.epub").to_string_lossy().to_string()]
    );
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

/// A GIF whose canvas is no pixels wide, with one pixel drawn on it.
const BLANK_GIF: &[u8] = &[
    b'G', b'I', b'F', b'8', b'9', b'a', 0, 0, 1, 0, 0x80, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0x2C, 0,
    0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 0x01, 0, 0x3B,
];

/// A cover no thumbnail can be made of must not sink the drop either: its
/// book is listed without one, beside the books that were fine.
#[test]
fn a_cover_with_no_pixels_leaves_only_its_own_book_without_one() {
    let dir = tempfile::tempdir().unwrap();
    let blank = dir.path().join("blank.epub");
    let good = dir.path().join("good.epub");
    // What the cover is, is read from its bytes, whatever its name.
    write_epub(
        &blank,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", OPF),
            ("OEBPS/c1.xhtml", CHAPTER),
            ("OEBPS/cover.png", BLANK_GIF),
        ],
    );
    demo_epub(&good);

    let books = inspect(vec![
        blank.to_string_lossy().to_string(),
        good.to_string_lossy().to_string(),
    ]);

    assert_eq!(books.len(), 2);
    assert!(books[0].error.is_none(), "{:?}", books[0].error);
    assert_eq!(books[0].title, "The Long Afternoon");
    assert!(books[0].cover.is_none());
    assert!(books[1].error.is_none(), "{:?}", books[1].error);
    assert!(books[1].cover.is_some());
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

/// One book made as the window makes each, by a run nobody stops.
fn optimize_one(
    job: &commands::Job,
    destination: &Path,
    settings: &Settings,
    progress: impl FnMut(u8, &str),
) -> commands::Outcome {
    commands::optimize_one(
        job,
        destination,
        settings,
        progress,
        &AtomicBool::new(false),
    )
}

/// The window names its books as chosen, not only the CLI.
#[test]
fn a_book_is_written_under_the_chosen_name() {
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("book.epub");
    demo_epub(&book);
    let out = tempfile::tempdir().unwrap();

    let outcome = optimize_one(
        &job(&book),
        out.path(),
        &naming(FilenameFormat::TitleAuthor),
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

    let outcome = optimize_one(
        &job(&book),
        dir.path(),
        &naming(FilenameFormat::Original),
        |_, _| {},
    );

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    let output = PathBuf::from(outcome.output.expect("a book was written"));
    assert_eq!(output.file_name().unwrap(), "book (2).epub");
    assert_eq!(std::fs::read(&book).unwrap(), before);
}

/// The demo book under another title, to tell two books' outputs apart.
fn titled_epub(path: &Path, title: &str) {
    let opf = String::from_utf8(OPF.to_vec())
        .unwrap()
        .replace("The Long Afternoon", title);
    write_epub(
        path,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/c1.xhtml", CHAPTER),
            ("OEBPS/cover.png", PNG),
        ],
    );
}

fn title_of(epub: &str) -> String {
    epubkit_core::preview::read_preview(Path::new(epub), 0)
        .unwrap()
        .metadata
        .title
}

/// A run that fails takes nothing with it but what it made itself.
#[test]
fn a_failed_run_removes_nothing_it_did_not_make() {
    let out = tempfile::tempdir().unwrap();
    let bystander = out.path().join(".epubkit-0.part");
    std::fs::write(&bystander, b"someone else's file").unwrap();

    let outcome = optimize_one(
        &job(&out.path().join("missing.epub")),
        out.path(),
        &naming(FilenameFormat::AuthorTitle),
        |_, _| {},
    );

    assert!(outcome.error.is_some());
    assert!(
        std::fs::read(&bystander).ok().as_deref() == Some(b"someone else's file".as_slice()),
        "a file the run did not make was removed or changed"
    );
}

/// A run told to stop stops the book it is making, which is not written, and
/// begins no other; each says it was stopped. Nothing could stop a run but
/// closing the window, which left the book's staging file in the folder the
/// books were going to.
#[test]
fn a_stopped_run_writes_nothing_more_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.epub");
    let second = dir.path().join("second.epub");
    titled_epub(&first, "First");
    titled_epub(&second, "Second");
    let jobs = [job(&first), job(&second)];
    let out = tempfile::tempdir().unwrap();
    let settings = naming(FilenameFormat::Title);

    let stop = AtomicBool::new(false);
    let mut finished = Vec::new();
    let outcomes = commands::optimize_all(
        &jobs,
        out.path(),
        &settings,
        &stop,
        |index, percent, _| {
            if index == 0 && percent >= 15 {
                stop.store(true, Ordering::Relaxed);
            }
        },
        |outcome| finished.push(outcome.path.clone()),
    );

    assert_eq!(finished, [jobs[0].path.clone(), jobs[1].path.clone()]);
    for outcome in &outcomes {
        assert!(outcome.stopped, "{outcome:?}");
        assert!(outcome.output.is_none() && outcome.error.is_some());
    }
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);

    // Not stopped, every book is made.
    let outcomes = commands::optimize_all(
        &jobs,
        out.path(),
        &settings,
        &AtomicBool::new(false),
        |_, _, _| {},
        |_| {},
    );
    let made: Vec<_> = outcomes
        .iter()
        .map(|outcome| title_of(outcome.output.as_ref().unwrap()))
        .collect();
    assert_eq!(made, ["First", "Second"]);
    assert!(outcomes.iter().all(|outcome| !outcome.stopped));
}

/// A book whose run panics fails on its own, as one whose run errs does. The
/// panic went up through the window's run of every book dropped with it:
/// those after it were never made, and the page was told only that the run
/// had failed, not which of the books before it had been written.
#[test]
fn a_book_whose_run_panics_fails_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("book.epub");
    demo_epub(&book);
    let out = tempfile::tempdir().unwrap();
    let settings = naming(FilenameFormat::AuthorTitle);

    let outcome = optimize_one(&job(&book), out.path(), &settings, |percent, _| {
        assert!(percent < 50, "a bug halfway through the book");
    });

    assert!(outcome.output.is_none());
    let error = outcome.error.expect("the panic is the book's error");
    assert!(error.contains("a bug halfway through the book"), "{error}");
    // Its staging file went with it.
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);

    // And the next book is made as ever.
    let next = optimize_one(&job(&book), out.path(), &settings, |_, _| {});
    assert!(next.error.is_none(), "{:?}", next.error);
}

/// A book is written to a file of its own making, not through a link that
/// happens to sit where it might have been written.
#[cfg(unix)]
#[test]
fn a_book_is_not_written_through_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("book.epub");
    demo_epub(&book);
    let elsewhere = tempfile::tempdir().unwrap();
    let precious = elsewhere.path().join("precious.txt");
    std::fs::write(&precious, b"precious").unwrap();
    let out = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&precious, out.path().join(".epubkit-0.part")).unwrap();

    let outcome = optimize_one(
        &job(&book),
        out.path(),
        &naming(FilenameFormat::AuthorTitle),
        |_, _| {},
    );

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(std::fs::read(&precious).unwrap(), b"precious");
}

/// Two runs into one folder, the second finishing inside the first, each
/// end up with their own book under their own name.
#[test]
fn runs_side_by_side_keep_their_own_books() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.epub");
    let second = dir.path().join("second.epub");
    demo_epub(&first);
    titled_epub(&second, "Another Book");
    let out = tempfile::tempdir().unwrap();
    let settings = naming(FilenameFormat::TitleAuthor);

    let mut inner = None;
    let outer = optimize_one(&job(&first), out.path(), &settings, |percent, _| {
        if percent == 100 && inner.is_none() {
            inner = Some(optimize_one(
                &job(&second),
                out.path(),
                &settings,
                |_, _| {},
            ));
        }
    });
    let inner = inner.expect("the second run happened");

    for (outcome, title) in [(&outer, "The Long Afternoon"), (&inner, "Another Book")] {
        assert!(outcome.error.is_none(), "{title}: {:?}", outcome.error);
        let written = outcome.output.as_ref().unwrap();
        assert!(written.contains(title), "{title} was written as {written}");
        assert_eq!(title_of(written), title);
    }
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

/// Every command the page's script invokes is one the app registers. One it
/// does not is refused only when the page calls it, and a button bound to it
/// does nothing.
#[test]
fn the_page_invokes_only_commands_the_app_registers() {
    let script = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app.js"))
        .expect("the page's script should be readable");
    let app = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("the app's source should be readable");

    let invoked: std::collections::BTreeSet<&str> = script
        .match_indices("invoke('")
        .map(|(at, marker)| {
            let rest = &script[at + marker.len()..];
            &rest[..rest.find('\'').expect("unterminated command name")]
        })
        .collect();
    for name in ["load_settings", "optimize_books", "stop_run"] {
        assert!(invoked.contains(name), "{name} not found in {invoked:?}");
    }
    for name in invoked {
        assert!(
            app.contains(&format!("commands::{name},")),
            "the page invokes '{name}', which the app does not register"
        );
    }
}

/// The events the app emits are the events the page listens for, but for the
/// window's own, `tauri://`. Renamed on one side only, a run's progress bars
/// would stand still and no result would show, and nothing would say why.
#[test]
fn the_page_listens_for_the_events_the_app_emits() {
    let script = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app.js"))
        .expect("the page's script should be readable");
    let commands = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/commands.rs"))
        .expect("the app's commands should be readable");

    // The quoted name after each `call`, however the call is laid out.
    let names = |text: &str, call: &str, quote: char| -> std::collections::BTreeSet<String> {
        text.match_indices(call)
            .filter_map(|(at, _)| {
                let rest = text[at + call.len()..].trim_start();
                let name = rest.strip_prefix(quote)?;
                Some(name[..name.find(quote)?].to_string())
            })
            .collect()
    };
    let listened: std::collections::BTreeSet<String> = names(&script, "listen(", '\'')
        .into_iter()
        .filter(|name| !name.starts_with("tauri://"))
        .collect();
    let emitted = names(&commands, ".emit(", '"');

    assert!(listened.contains("progress"), "{listened:?}");
    assert_eq!(listened, emitted);
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

/// An uncompressed 24-bit BMP of `width` x `height`, a gradient.
fn bmp(width: u32, height: u32) -> Vec<u8> {
    let row = (width * 3).div_ceil(4) * 4;
    let size = 54 + row * height;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 4]); // uncompressed
    out.extend_from_slice(&(row * height).to_le_bytes());
    out.extend_from_slice(&[0; 16]);
    for y in 0..height {
        let start = out.len();
        for x in 0..width {
            out.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 128]);
        }
        out.resize(start + row as usize, 0);
    }
    out
}

/// The width and height a baseline JPEG's frame header gives.
fn jpeg_size(jpeg: &[u8]) -> (u32, u32) {
    let frame = jpeg
        .windows(2)
        .position(|marker| marker == [0xFF, 0xC0])
        .expect("a baseline JPEG");
    let height = u16::from_be_bytes([jpeg[frame + 5], jpeg[frame + 6]]);
    let width = u16::from_be_bytes([jpeg[frame + 7], jpeg[frame + 8]]);
    (width.into(), height.into())
}

/// The list shows a cover at a few dozen pixels. Sent whole, every dropped
/// book's cover, up to 8 MiB each, was held by the page as a data URL and
/// again decoded at full size.
#[test]
fn a_cover_comes_back_as_a_thumbnail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.epub");
    let opf = String::from_utf8(OPF.to_vec()).unwrap().replace(
        r#"href="cover.png" media-type="image/png""#,
        r#"href="cover.bmp" media-type="image/bmp""#,
    );
    let cover = bmp(1200, 1800);
    write_epub(
        &path,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/c1.xhtml", CHAPTER),
            ("OEBPS/cover.bmp", &cover),
        ],
    );

    let url = inspect(vec![path.to_string_lossy().to_string()])
        .swap_remove(0)
        .cover
        .expect("the book has a cover");

    let (kind, data) = url.split_once(";base64,").expect("a data URL");
    assert_eq!(kind, "data:image/jpeg");
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(data)
        .expect("base64");
    let (width, height) = jpeg_size(&jpeg);
    assert!(width <= 480 && height <= 720, "{width}x{height}");
    assert!(jpeg.len() < 200_000, "{} bytes", jpeg.len());
}

/// A PNG that says it is `width` x `height` RGBA and holds no pixels.
fn png_claiming(width: u32, height: u32) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = png.len();
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let crc = crc32(&png[start..]);
        png.extend_from_slice(&crc.to_be_bytes());
    }

    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &header);
    chunk(
        &mut png,
        b"IDAT",
        &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
    );
    chunk(&mut png, b"IEND", &[]);
    png
}

/// A cover too large to be made a thumbnail of is too large for the page
/// too. It was sent whole all the same, a 14000 x 14000 one in a 600 KB
/// PNG, for the page to decode at full size.
#[test]
fn a_cover_too_large_to_shrink_is_not_sent_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.epub");
    let cover = png_claiming(14_000, 14_000);
    write_epub(
        &path,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", OPF),
            ("OEBPS/c1.xhtml", CHAPTER),
            ("OEBPS/cover.png", &cover),
        ],
    );

    let book = inspect(vec![path.to_string_lossy().to_string()]).swap_remove(0);

    assert!(book.error.is_none(), "{:?}", book.error);
    assert!(
        book.cover.is_none(),
        "sent: {:?}",
        book.cover.map(|url| url.len())
    );
}
