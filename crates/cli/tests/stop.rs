//! Stopping a run from the terminal.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// A book of `chapters` short chapters, for a run to spend a while on.
fn write_long_book(path: &Path, chapters: usize) {
    let mut manifest = String::new();
    let mut spine = String::new();
    for n in 0..chapters {
        manifest.push_str(&format!(
            r#"<item id="c{n}" href="c{n}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
    }
    let opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:stop</dc:identifier><dc:title>A Long Book</dc:title>
  </metadata>
  <manifest>{manifest}</manifest>
  <spine>{spine}</spine>
</package>
"#
    );
    let container = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;
    let chapter = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>A chapter</title></head><body>
<h1>A chapter</h1><p>It was a long afternoon , and the light lay  across the floor.</p>
<p>She said nothing ; the clock said less.</p><p></p><p></p><p>And then it was evening.</p>
</body></html>
"#;

    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", stored).unwrap();
    zip.write_all(container.as_bytes()).unwrap();
    zip.start_file("OEBPS/content.opf", stored).unwrap();
    zip.write_all(opf.as_bytes()).unwrap();
    for n in 0..chapters {
        zip.start_file(format!("OEBPS/c{n}.xhtml"), stored).unwrap();
        zip.write_all(chapter.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

/// Ctrl-C stops a run at its next step, and the run takes what it was making
/// with it. It ended the program there and then, which left the run's staging
/// file beside where the book was going, and the whole book unpacked in the
/// temporary directory.
#[test]
fn ctrl_c_stops_a_run_and_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("long.epub");
    write_long_book(&book, 1500);
    let out = dir.path().join("out");
    let temporary = dir.path().join("tmp");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::create_dir_all(&temporary).unwrap();

    let mut run = Command::new(env!("CARGO_BIN_EXE_epubkit"))
        .arg("optimize")
        .arg(&book)
        .arg("-o")
        .arg(out.join("long.epub"))
        .arg("--no-save")
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".config"))
        .env("TMPDIR", &temporary)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Interrupted as Ctrl-C would, once the chapters are being repaired.
    let mut said = String::new();
    let mut lines = BufReader::new(run.stderr.take().unwrap()).lines();
    for line in lines.by_ref() {
        let line = line.unwrap();
        said.push_str(&line);
        said.push('\n');
        if line.contains("Repairing HTML") {
            break;
        }
    }
    let interrupted = Command::new("kill")
        .args(["-INT", &run.id().to_string()])
        .status()
        .unwrap();
    assert!(interrupted.success());
    for line in lines {
        said.push_str(&line.unwrap());
        said.push('\n');
    }
    let ended = run.wait().unwrap();

    assert_eq!(ended.code(), Some(130), "{ended:?}:\n{said}");
    assert!(said.contains("Stopped"), "{said}");
    let left = |dir: &Path| std::fs::read_dir(dir).unwrap().count();
    assert_eq!(left(&out), 0, "left in the output folder");
    assert_eq!(left(&temporary), 0, "left in the temporary directory");
}
