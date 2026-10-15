//! The command line's options, checked before anything is done with them.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Every file under `dir`.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// Where the settings file is under a home of `home`: on macOS, and on Linux.
fn settings_files(home: &Path) -> [PathBuf; 2] {
    [
        home.join("Library/Application Support/epubkit/settings.toml"),
        home.join(".config/epubkit/settings.toml"),
    ]
}

/// A book of one chapter, for the command line to optimize.
fn write_book(path: &Path) {
    use std::io::Write;
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:options</dc:identifier>
    <dc:title>A Book</dc:title>
  </metadata>
  <manifest><item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="ch1"/></spine>
</package>
"#;
    let chapter = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title></head><body><p>Text.</p></body></html>
"#;
    let container = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, text) in [
        ("mimetype", "application/epub+zip"),
        ("META-INF/container.xml", container),
        ("OEBPS/content.opf", opf),
        ("OEBPS/chapter1.xhtml", chapter),
    ] {
        zip.start_file(name, stored).unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

/// A filename template the window saved half typed, as it saves the field at
/// every keystroke, does not stop a run that names its book with `-o`, which
/// takes no name from it; where it would name the book, the run stops before
/// anything is saved. It stopped every run, `-o` or not, and after the run's
/// flags had been saved.
#[test]
fn a_broken_saved_template_stops_only_a_run_it_would_name() {
    let home = tempfile::tempdir().unwrap();
    let saved = "[options]\nquality = 70\n\n[filename]\nformat = \"custom\"\ntemplate = \"{tit\"\n";
    for file in settings_files(home.path()) {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, saved).unwrap();
    }
    write_book(&home.path().join("in.epub"));
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_epubkit"))
            .args(args)
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .output()
            .unwrap()
    };

    let named = run(&["optimize", "in.epub", "-o", "out.epub"]);
    let said = String::from_utf8_lossy(&named.stderr);
    assert!(named.status.success(), "with -o: {said}");
    assert!(home.path().join("out.epub").exists());

    let unnamed = run(&["optimize", "in.epub", "--quality", "80"]);
    let said = String::from_utf8_lossy(&unnamed.stderr);
    assert!(!unnamed.status.success(), "without -o: {said}");
    assert!(said.contains("template"), "{said}");
    for file in settings_files(home.path()) {
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("quality = 70"), "{}: {text}", file.display());
    }
}

/// Each option the command line sets, it sets either way, and the run after
/// it, the window's too, starts from what it set. The flags only turned
/// options off, Light Novel mode only on, and contrast boost not at all: one
/// `--no-grayscale` took grayscale from every run after it, with no flag to
/// give it back but a preset, which reset everything else.
#[test]
fn every_option_can_be_turned_on_and_off() {
    let home = tempfile::tempdir().unwrap();
    write_book(&home.path().join("in.epub"));
    let run = |flags: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_epubkit"))
            .args(["optimize", "in.epub", "-o", "out.epub"])
            .args(flags)
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&output.stderr).to_string();
        assert!(output.status.success(), "{flags:?}: {said}");
        settings_files(home.path())
            .iter()
            .find_map(|file| std::fs::read_to_string(file).ok())
            .expect("the run saved its settings")
    };

    for (option, on, off) in [
        ("grayscale", "--grayscale", "--no-grayscale"),
        ("contrast_boost", "--contrast-boost", "--no-contrast-boost"),
        ("remove_fonts", "--font-removal", "--no-font-removal"),
        ("remove_unused_css", "--css-cleanup", "--no-css-cleanup"),
        ("text_cleanup", "--text-cleanup", "--no-text-cleanup"),
        (
            "clean_metadata",
            "--metadata-cleanup",
            "--no-metadata-cleanup",
        ),
        ("light_novel_mode", "--light-novel", "--no-light-novel"),
    ] {
        for (flags, value) in [
            (vec![off], false),
            (vec![], false),
            (vec![on], true),
            (vec![], true),
            (vec![on, off], false),
            (vec![off, on], true),
        ] {
            let saved = run(&flags);
            assert!(
                saved.contains(&format!("\n{option} = {value}\n")),
                "{flags:?} should leave {option} {value}:\n{saved}"
            );
        }
    }
}

/// JPEG quality is 20 to 95, as the CLI's help says and the app's slider
/// allows. Any byte was taken, and remembered for every run after it:
/// `--quality 9`, for 90, made each book from then on at the worst quality the
/// encoder has, with nothing said.
#[test]
fn a_quality_outside_its_range_is_refused_and_not_remembered() {
    let home = tempfile::tempdir().unwrap();
    for quality in ["0", "9", "19", "96", "100"] {
        let output = Command::new(env!("CARGO_BIN_EXE_epubkit"))
            .args(["optimize", "missing.epub", "--quality", quality])
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .output()
            .unwrap();
        assert!(!output.status.success(), "--quality {quality}");
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(said.contains("20..=95"), "--quality {quality}: {said}");
    }
    assert_eq!(files_under(home.path()), Vec::<PathBuf>::new());
}
