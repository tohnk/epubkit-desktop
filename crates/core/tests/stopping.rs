//! How soon a run stops once told, timed. A test of its own, so that nothing
//! else runs in the process while it is timed.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use epubkit_core::pipeline::{process_epub, process_epub_stoppable, ProcessingOptions};
use epubkit_core::Error;

const STYLESHEET: &str = r#"@font-face { font-family: "BodyFont"; src: url(../fonts/body.otf); }
body { margin: 0; }
.lead { font-size: 1.1em; }
.never-used-anywhere { color: rebeccapurple; }
"#;

/// A book of `chapters` long chapters, one showing an image, every one
/// styled by a stylesheet with a font and a rule nothing uses: something for
/// each step to do, chapter by chapter.
fn write_long_epub(path: &Path, chapters: usize) {
    let plate = common::png_gradient(240, 160);
    let page = concat!(
        "<p class=\"lead\" data-page=\"1\">It was a  long afternoon &amp; the light was  ",
        "failing, \"slowly\"..... and the water lay still.</p>\n<p></p>\n",
    )
    .repeat(2000);
    let mut manifest = String::new();
    let mut spine = String::new();
    let mut texts = Vec::new();
    for n in 0..chapters {
        manifest.push_str(&format!(
            r#"<item id="ch{n}" href="text/ch{n:03}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="ch{n}"/>"#));
        let image = if n == 0 {
            r#"<img src="../images/plate.png" alt="A plate"/>"#
        } else {
            ""
        };
        texts.push((
            format!("OEBPS/text/ch{n:03}.xhtml"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Chapter {n}</title><link rel="stylesheet" type="text/css" href="../styles/main.css"/></head>
<body><h1>Chapter {n}</h1>{image}{page}</body>
</html>
"#
            ),
        ));
    }
    let opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:long</dc:identifier>
    <dc:title>The Long Afternoon</dc:title>
    <dc:creator>Marguerite Vale</dc:creator>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="plate" href="images/plate.png" media-type="image/png"/>
    <item id="css" href="styles/main.css" media-type="text/css"/>
    <item id="font" href="fonts/body.otf" media-type="font/otf"/>
    {manifest}
  </manifest>
  <spine>{spine}</spine>
</package>
"#
    );
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", common::CONTAINER_XML),
        ("OEBPS/content.opf", opf.as_bytes()),
        ("OEBPS/styles/main.css", STYLESHEET.as_bytes()),
        (
            "OEBPS/fonts/body.otf",
            b"not really a font, but named like one",
        ),
        ("OEBPS/images/plate.png", &plate),
    ];
    entries.extend(
        texts
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_bytes())),
    );
    common::write_epub(path, &entries);
}

/// A run told to stop partway through a step stops at the next document,
/// image or file it would begin, not at the next step: a step over every
/// chapter of a long book took seconds, and a quit from the Dock gives a run
/// ten. Told as each long step begins, it stops in under half the time the
/// step takes left alone.
#[test]
fn a_run_told_to_stop_partway_through_a_step_stops_within_it() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let output = dir.path().join("out.epub");
    write_long_epub(&input, 10);
    let options = ProcessingOptions::default();

    // How long each step takes left alone, once a first run has warmed up
    // what the runs after it find warm.
    let mut began = Vec::new();
    for _ in 0..2 {
        began.clear();
        process_epub(&input, &output, &options, |percent, _| {
            began.push((percent, Instant::now()));
        })
        .unwrap();
    }

    // Every step that takes a twenty-fifth of this book's run or more, found
    // rather than listed, so that a step that comes to take as long on this
    // book is timed as well. Only that long is a stop timed well apart from
    // the deleting it does. A step quick on this book is not timed, however
    // long it takes on another: making a table of contents, say, which a
    // test of its own stops directly.
    let run = began[began.len() - 1].1 - began[0].1;
    let long: Vec<(u8, Duration)> = began
        .windows(2)
        .map(|pair| (pair[0].0, pair[1].1 - pair[0].1))
        .filter(|&(_, takes)| takes * 25 >= run)
        .collect();
    assert!(long.len() >= 6, "{long:?} of {run:?}");

    for (step, takes) in long {
        let stop = AtomicBool::new(false);
        let mut told = None;
        let result = process_epub_stoppable(
            &input,
            &output,
            &options,
            |percent, _| {
                if percent == step {
                    stop.store(true, Ordering::Relaxed);
                    told = Some(Instant::now());
                }
            },
            &stop,
        );
        let stopped_in = told.unwrap().elapsed();

        assert!(matches!(result, Err(Error::Stopped)), "{step}: {result:?}");
        assert!(
            stopped_in * 2 < takes,
            "{step}: stopped in {stopped_in:?}, where the step takes {takes:?}"
        );
    }
}
