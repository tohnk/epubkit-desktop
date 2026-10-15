//! Stopping a run from the terminal.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Lines, Write};
use std::path::Path;
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};

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

/// The command line optimizing a long book in `dir`, into `out` there, with
/// `tmp` there for its temporary directory.
struct Run {
    process: Child,
    lines: Lines<BufReader<ChildStderr>>,
    said: String,
}

impl Run {
    fn start(dir: &Path) -> Self {
        let book = dir.join("long.epub");
        write_long_book(&book, 1500);
        for folder in ["out", "tmp"] {
            std::fs::create_dir_all(dir.join(folder)).unwrap();
        }
        let mut process = Command::new(env!("CARGO_BIN_EXE_epubkit"))
            .arg("optimize")
            .arg(&book)
            .arg("-o")
            .arg(dir.join("out/long.epub"))
            .arg("--no-save")
            .env("HOME", dir)
            .env("XDG_CONFIG_HOME", dir.join(".config"))
            .env("TMPDIR", dir.join("tmp"))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = BufReader::new(process.stderr.take().unwrap()).lines();
        Self {
            process,
            lines,
            said: String::new(),
        }
    }

    /// Read what the run says until it says `text`.
    fn wait_for(&mut self, text: &str) {
        for line in self.lines.by_ref() {
            let line = line.unwrap();
            self.said.push_str(&line);
            self.said.push('\n');
            if line.contains(text) {
                return;
            }
        }
        panic!("the run never said {text:?}:\n{}", self.said);
    }

    /// Interrupt the run, as Ctrl-C does.
    fn interrupt(&self) {
        let sent = Command::new("kill")
            .args(["-INT", &self.process.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
    }

    /// How the run ended, and all it said.
    fn end(self) -> (ExitStatus, String) {
        let Run {
            mut process,
            lines,
            mut said,
        } = self;
        for line in lines {
            said.push_str(&line.unwrap());
            said.push('\n');
        }
        (process.wait().unwrap(), said)
    }
}

/// How many files are in `folder` of `dir`.
fn left(dir: &Path, folder: &str) -> usize {
    std::fs::read_dir(dir.join(folder)).unwrap().count()
}

/// Ctrl-C stops a run at its next step, and the run takes what it was making
/// with it. It ended the program there and then, which left the run's staging
/// file beside where the book was going, and the whole book unpacked in the
/// temporary directory.
#[test]
fn ctrl_c_stops_a_run_and_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let mut run = Run::start(dir.path());

    // Interrupted once the chapters are being repaired.
    run.wait_for("Repairing HTML");
    run.interrupt();
    let (ended, said) = run.end();

    assert_eq!(ended.code(), Some(130), "{ended:?}:\n{said}");
    assert!(said.contains("Stopped: nothing was written."), "{said}");
    assert_eq!(left(dir.path(), "out"), 0, "left in the output folder");
    assert_eq!(
        left(dir.path(), "tmp"),
        0,
        "left in the temporary directory"
    );
}

/// A second Ctrl-C, once the first is taken, ends the program there and then,
/// for a run slow to reach its next step: it leaves the staging file and the
/// unpacked book behind, as Ctrl-C did. Two sent together are taken as one.
#[test]
fn a_second_ctrl_c_ends_the_program_there_and_then() {
    let dir = tempfile::tempdir().unwrap();
    let mut run = Run::start(dir.path());

    run.wait_for("Repairing HTML");
    run.interrupt();
    run.wait_for("Stopping");
    run.interrupt();
    let (ended, said) = run.end();

    assert_eq!(ended.code(), Some(130), "{ended:?}:\n{said}");
    assert!(
        !said.contains("Stopped:"),
        "it stopped at its next step:\n{said}"
    );
    assert_eq!(left(dir.path(), "out"), 1, "the staging file");
    assert_eq!(left(dir.path(), "tmp"), 1, "the unpacked book");
}
