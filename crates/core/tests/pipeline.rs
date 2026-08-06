mod common;

use std::fs;
use std::path::Path;

use epubkit_core::pipeline::{process_epub, ProcessingOptions, ProcessingReport};
use epubkit_core::{metadata, package, structure, xml, Error};

/// A book exercising every step: two chapters (one malformed, with store
/// metadata, an unused stylesheet rule, an embedded font and an image).
fn write_demo_epub(path: &Path) {
    let cover = common::png_gradient(300, 400);
    let plate = common::png_gradient(240, 160);

    common::write_epub(
        path,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("OEBPS/content.opf", DEMO_OPF.as_bytes()),
            ("OEBPS/chapter1.xhtml", MALFORMED_CHAPTER.as_bytes()),
            ("OEBPS/chapter2.xhtml", CLEAN_CHAPTER.as_bytes()),
            ("OEBPS/styles/main.css", DEMO_CSS.as_bytes()),
            (
                "OEBPS/fonts/body.otf",
                b"not really a font, but named like one",
            ),
            ("OEBPS/images/cover.png", &cover),
            ("OEBPS/images/plate.png", &plate),
        ],
    );
}

const DEMO_OPF: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:demo</dc:identifier>
    <dc:title>The Long Afternoon</dc:title>
    <dc:creator>Marguerite Vale</dc:creator>
    <dc:language>en</dc:language>
    <meta name="calibre:timestamp" content="2019-04-02"/>
    <meta name="ibooks:version" content="2.1"/>
    <meta name="cover" content="cover-img"/>
  </metadata>
  <manifest>
    <item id="cover-img" href="images/cover.png" media-type="image/png"/>
    <item id="plate" href="images/plate.png" media-type="image/png"/>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="chapter2.xhtml" media-type="application/xhtml+xml"/>
    <item id="css" href="styles/main.css" media-type="text/css"/>
    <item id="font" href="fonts/body.otf" media-type="font/otf"/>
  </manifest>
  <spine><itemref idref="ch1"/><itemref idref="ch2"/></spine>
</package>
"#;

const MALFORMED_CHAPTER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Chapter One</title></head>
<body>
<h1 class="chapter-title">Chapter One</h1>
<p class="lead" data-page="1" aria-label="opening">It was a  long afternoon &amp; the light was  failing.</p>
<p></p><p></p><p></p>
<p>An <b>unclosed tag and the &#xFB01;rst &#xFB02;ight of stairs.</p>
<img src="images/plate.png" alt="A plate"/>
</body>
</html>
"#;

const CLEAN_CHAPTER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Chapter Two</title></head>
<body><h1>Chapter Two</h1><p>Wait..... Really,,, yes!</p></body>
</html>
"#;

const DEMO_CSS: &str = r#"/* book styles */
@font-face { font-family: "BodyFont"; src: url(../fonts/body.otf); }
body { margin: 0; }
.lead { font-size: 1.1em; }
.never-used-anywhere { color: rebeccapurple; }
"#;

fn run(options: ProcessingOptions) -> (tempfile::TempDir, ProcessingReport) {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let output = dir.path().join("out.epub");
    write_demo_epub(&input);

    let report = process_epub(&input, &output, &options, |_, _| {}).expect("pipeline should run");
    (dir, report)
}

fn entry_names(path: &Path) -> Vec<String> {
    let archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    archive.file_names().map(|s| s.to_string()).collect()
}

#[test]
fn produces_a_valid_epub() {
    let (dir, _) = run(ProcessingOptions::default());
    let output = dir.path().join("out.epub");

    let validation = package::validate_epub(&output).unwrap();
    assert!(validation.is_valid(), "problems: {:?}", validation.problems);
}

#[test]
fn every_step_reports_what_it_did() {
    let (_, report) = run(ProcessingOptions::default());

    assert_eq!(report.images_total, 2);
    assert_eq!(report.images_converted, 2);
    assert!(report.fonts_removed >= 2, "css rule plus font file");
    assert!(report.css_rules_removed >= 1);
    assert!(report.metadata_items_stripped >= 2, "calibre and ibooks");
    assert!(report.blank_elements_removed >= 2);
    assert!(report.attributes_stripped >= 2, "data- and aria-");
    assert!(report.text.total_fixes() > 0);
    assert!(report.documents_recovered >= 1, "chapter one is malformed");
    assert!(!report.toc_status.is_empty());
    assert!(report.original_size > 0 && report.optimized_size > 0);
}

#[test]
fn the_output_filename_comes_from_the_metadata() {
    let (_, report) = run(ProcessingOptions::default());
    assert_eq!(
        report.output_filename,
        "Marguerite Vale - The Long Afternoon.epub"
    );
}

#[test]
fn images_become_jpegs_and_references_follow() {
    let (dir, _) = run(ProcessingOptions::default());
    let output = dir.path().join("out.epub");

    let names = entry_names(&output);
    assert!(
        names.iter().any(|n| n == "OEBPS/images/cover.jpg"),
        "{names:?}"
    );
    assert!(
        names.iter().any(|n| n == "OEBPS/images/plate.jpg"),
        "{names:?}"
    );
    assert!(
        !names.iter().any(|n| n.ends_with(".png")),
        "the source PNGs should be gone: {names:?}"
    );

    // The chapter's <img src> and the manifest must both point at the new file.
    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();

    let chapter = fs::read_to_string(work.path().join("OEBPS/chapter1.xhtml")).unwrap();
    assert!(chapter.contains("images/plate.jpg"), "{chapter}");
    assert!(!chapter.contains("plate.png"), "{chapter}");

    let opf = xml::parse_file(&work.path().join("OEBPS/content.opf")).unwrap();
    let hrefs: Vec<String> = structure::manifest_items(&opf)
        .unwrap()
        .into_iter()
        .map(|i| i.href)
        .collect();
    assert!(hrefs.contains(&"images/plate.jpg".to_string()), "{hrefs:?}");
}

#[test]
fn fonts_are_gone_from_the_archive_the_css_and_the_manifest() {
    let (dir, _) = run(ProcessingOptions::default());
    let output = dir.path().join("out.epub");

    let names = entry_names(&output);
    assert!(!names.iter().any(|n| n.contains("body.otf")), "{names:?}");

    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();

    let css = fs::read_to_string(work.path().join("OEBPS/styles/main.css")).unwrap();
    assert!(!css.contains("@font-face"), "{css}");

    let opf = xml::parse_file(&work.path().join("OEBPS/content.opf")).unwrap();
    let hrefs: Vec<String> = structure::manifest_items(&opf)
        .unwrap()
        .into_iter()
        .map(|i| i.href)
        .collect();
    assert!(!hrefs.iter().any(|h| h.contains(".otf")), "{hrefs:?}");
}

/// Every chapter in the output must parse strictly — including the one that
/// arrived malformed.
#[test]
fn all_chapters_come_out_well_formed() {
    let (dir, _) = run(ProcessingOptions::default());
    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&dir.path().join("out.epub"), work.path()).unwrap();

    for name in ["OEBPS/chapter1.xhtml", "OEBPS/chapter2.xhtml"] {
        let bytes = fs::read(work.path().join(name)).unwrap();
        xml::parse_strict(&bytes).unwrap_or_else(|e| panic!("{name} is not well-formed: {e}"));
    }
}

#[test]
fn text_cleanup_reaches_the_output() {
    let (dir, _) = run(ProcessingOptions::default());
    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&dir.path().join("out.epub"), work.path()).unwrap();

    let one = fs::read_to_string(work.path().join("OEBPS/chapter1.xhtml")).unwrap();
    assert!(
        one.contains("a long afternoon"),
        "double space survived: {one}"
    );
    assert!(one.contains("first"), "the fi ligature survived: {one}");
    assert!(one.contains("&amp;"), "the ampersand was lost: {one}");

    let two = fs::read_to_string(work.path().join("OEBPS/chapter2.xhtml")).unwrap();
    assert!(two.contains("Wait..."), "{two}");
    assert!(!two.contains("Wait....."), "{two}");
}

#[test]
fn store_metadata_goes_but_the_book_keeps_its_own() {
    let (dir, _) = run(ProcessingOptions::default());
    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&dir.path().join("out.epub"), work.path()).unwrap();

    let opf_bytes = fs::read(work.path().join("OEBPS/content.opf")).unwrap();
    let opf_text = String::from_utf8_lossy(&opf_bytes);
    assert!(!opf_text.contains("calibre:"), "{opf_text}");
    assert!(!opf_text.contains("ibooks:"), "{opf_text}");

    let opf = xml::parse_strict(&opf_bytes).unwrap();
    let meta = metadata::extract_metadata(&opf).unwrap();
    assert_eq!(meta.title, "The Long Afternoon");
    assert_eq!(meta.author, "Marguerite Vale");
}

#[test]
fn metadata_edits_are_applied_and_name_the_output() {
    let options = ProcessingOptions {
        metadata_edits: metadata::MetadataEdits {
            title: Some("A Different Title".into()),
            author: Some("Someone Else".into()),
            language: None,
        },
        ..ProcessingOptions::default()
    };

    let (dir, report) = run(options);
    assert_eq!(
        report.output_filename,
        "Someone Else - A Different Title.epub"
    );

    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&dir.path().join("out.epub"), work.path()).unwrap();
    let opf = xml::parse_file(&work.path().join("OEBPS/content.opf")).unwrap();
    let meta = metadata::extract_metadata(&opf).unwrap();
    assert_eq!(meta.title, "A Different Title");
}

#[test]
fn turning_steps_off_leaves_them_undone() {
    let options = ProcessingOptions {
        remove_fonts: false,
        remove_unused_css: false,
        text_cleanup: false,
        clean_metadata: false,
        ..ProcessingOptions::default()
    };

    let (dir, report) = run(options);

    assert_eq!(report.fonts_removed, 0);
    assert_eq!(report.css_rules_removed, 0);
    assert_eq!(report.metadata_items_stripped, 0);
    assert_eq!(report.text.total_fixes(), 0);

    let names = entry_names(&dir.path().join("out.epub"));
    assert!(
        names.iter().any(|n| n.contains("body.otf")),
        "the font should have survived: {names:?}"
    );
}

#[test]
fn progress_runs_from_start_to_finish_without_going_backwards() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let output = dir.path().join("out.epub");
    write_demo_epub(&input);

    let mut seen: Vec<u8> = Vec::new();
    process_epub(
        &input,
        &output,
        &ProcessingOptions::default(),
        |percent, message| {
            assert!(
                !message.is_empty(),
                "every step should say what it is doing"
            );
            seen.push(percent);
        },
    )
    .unwrap();

    assert!(
        seen.windows(2).all(|w| w[0] <= w[1]),
        "progress went backwards: {seen:?}"
    );
    assert_eq!(seen.last(), Some(&100), "the run should finish at 100%");
    assert!(seen.len() > 10, "too few progress reports: {seen:?}");
}

#[test]
fn a_drm_protected_book_is_refused_with_a_useful_message() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("drm.epub");
    let output = dir.path().join("out.epub");

    let encryption = common::encryption_xml(
        "http://www.w3.org/2001/04/xmlenc#aes256-cbc",
        "OEBPS/chapter1.xhtml",
    );
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("META-INF/encryption.xml", &encryption),
            ("OEBPS/content.opf", common::CONTENT_OPF),
        ],
    );

    let error = process_epub(&input, &output, &ProcessingOptions::default(), |_, _| {})
        .expect_err("a DRM-protected book should be refused");

    assert!(matches!(error, Error::DrmProtected));
    assert!(
        error.to_string().contains("DRM"),
        "the message should say what is wrong: {error}"
    );
    assert!(!output.exists(), "nothing should have been written");
}

#[test]
fn the_summary_reads_as_prose() {
    let (_, report) = run(ProcessingOptions::default());
    let summary = report.summary();

    assert!(summary.contains("Converted 2/2 images"), "{summary}");
    assert!(summary.contains("Size:"), "{summary}");
}

/// Dithering to four levels is high-frequency noise by construction, which is
/// the worst case for a DCT codec. A book of smooth artwork can legitimately
/// come out larger, and the report has to say so rather than print a negative
/// reduction.
#[test]
fn a_size_increase_is_described_as_an_increase() {
    let report = ProcessingReport {
        original_size: 1000,
        optimized_size: 3000,
        ..ProcessingReport::default()
    };

    let summary = report.summary();
    assert!(summary.contains("increase"), "{summary}");
    assert!(!summary.contains('-'), "no negative percentages: {summary}");
}
