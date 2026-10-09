mod common;

use std::collections::BTreeMap;
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
    assert_eq!(report.fonts_removed, 1, "one font file");
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

/// A font removed is removed from everywhere that names it: a stylesheet, a
/// chapter's own `<style>`, and `encryption.xml`, which listed it as
/// obfuscated. And it counts once, not once per place.
#[test]
fn nothing_is_left_naming_a_removed_font() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let chapter = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title>
<style type="text/css">@font-face { font-family: Inline; src: url(fonts/body.otf) } p { margin: 0 }</style>
</head><body><p>Text.</p></body></html>
"#;
    let encryption =
        common::encryption_xml("http://www.idpf.org/2008/embedding", "OEBPS/fonts/body.otf");
    demo_epub_with(
        &input,
        &[
            ("OEBPS/chapter2.xhtml", chapter.as_bytes()),
            ("META-INF/encryption.xml", &encryption),
        ],
    );

    let output = dir.path().join("out.epub");
    let report = process_epub(&input, &output, &ProcessingOptions::default(), |_, _| {}).unwrap();

    assert_eq!(report.fonts_removed, 1);
    let names = entry_names(&output);
    assert!(!names.iter().any(|n| n.ends_with(".otf")), "{names:?}");
    assert!(
        !names.iter().any(|n| n == "META-INF/encryption.xml"),
        "{names:?}"
    );

    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();
    for file in ["OEBPS/styles/main.css", "OEBPS/chapter2.xhtml"] {
        let text = fs::read_to_string(work.path().join(file)).unwrap();
        assert!(!text.contains("@font-face"), "{file}:\n{text}");
    }
    let chapter = fs::read_to_string(work.path().join("OEBPS/chapter2.xhtml")).unwrap();
    assert!(chapter.contains("p { margin: 0 }"), "{chapter}");
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

// ------------------------------------------------------ image name collisions

/// A square of one grey, so a test can tell which image ended up where.
fn solid(format: image::ImageFormat, grey: u8) -> Vec<u8> {
    let square = image::GrayImage::from_pixel(64, 64, image::Luma([grey]));
    let mut out = Vec::new();
    image::DynamicImage::ImageLuma8(square)
        .write_to(&mut std::io::Cursor::new(&mut out), format)
        .expect("encode fixture image");
    out
}

/// Optimize a book whose manifest lists `images` in the order given and whose
/// one chapter, in a subdirectory of its own, shows each of them. Returns the
/// unpacked output.
fn convert_images_book(images: &[(&str, Vec<u8>)]) -> tempfile::TempDir {
    let body: String = images
        .iter()
        .map(|(href, _)| format!(r#"<p><img src="../{href}" alt=""/></p>"#))
        .collect();
    optimize_book(images, &body, &ProcessingOptions::default())
}

/// Optimize a book whose manifest lists `images` and whose one chapter, in a
/// subdirectory of its own, has `body`. Returns the unpacked output.
fn optimize_book(
    images: &[(&str, Vec<u8>)],
    body: &str,
    options: &ProcessingOptions,
) -> tempfile::TempDir {
    optimize_book_with_report(images, body, options).0
}

/// [`optimize_book`], and what the run reported.
fn optimize_book_with_report(
    images: &[(&str, Vec<u8>)],
    body: &str,
    options: &ProcessingOptions,
) -> (tempfile::TempDir, ProcessingReport) {
    let mut manifest = String::new();
    for (index, (href, _)) in images.iter().enumerate() {
        let media_type = if href.to_ascii_lowercase().ends_with(".png") {
            "image/png"
        } else {
            "image/jpeg"
        };
        manifest.push_str(&format!(
            r#"<item id="img{index}" href="{href}" media-type="{media_type}"/>"#
        ));
    }

    let opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:images</dc:identifier>
    <dc:title>Images</dc:title>
  </metadata>
  <manifest>
    <item id="ch1" href="text/chapter1.xhtml" media-type="application/xhtml+xml"/>
    {manifest}
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>
"#
    );
    let chapter = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title></head><body>{body}</body></html>
"#
    );

    let mut entries: Vec<(String, Vec<u8>)> = vec![
        ("mimetype".into(), b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml".into(),
            common::CONTAINER_XML.to_vec(),
        ),
        ("OEBPS/content.opf".into(), opf.into_bytes()),
        ("OEBPS/text/chapter1.xhtml".into(), chapter.into_bytes()),
    ];
    for (href, bytes) in images {
        entries.push((in_archive("OEBPS", href), bytes.clone()));
    }
    let entries: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let output = dir.path().join("out.epub");
    common::write_epub(&input, &entries);
    let report = process_epub(&input, &output, options, |_, _| {}).unwrap();

    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();
    (work, report)
}

/// Where `href`, from the archive directory `base`, leads in the archive,
/// resolved as a reader resolves it: a leading `/` from the root, `..` and `.`
/// as written, whether or not the directories they pass through exist.
fn in_archive(base: &str, href: &str) -> String {
    let mut parts: Vec<&str> = if href.starts_with('/') {
        Vec::new()
    } else {
        base.split('/').collect()
    };
    for part in href.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// The chapter's image references, in order.
fn chapter_sources(work: &Path) -> Vec<String> {
    let chapter = fs::read_to_string(work.join("OEBPS/text/chapter1.xhtml")).unwrap();
    chapter
        .split(r#"src=""#)
        .skip(1)
        .map(|rest| rest[..rest.find('"').unwrap()].to_string())
        .collect()
}

/// The grey of each image the chapter shows, followed through its own
/// references, to the nearest of black, mid-grey and white.
fn shown_greys(work: &Path) -> Vec<u8> {
    chapter_sources(work)
        .iter()
        .map(|src| {
            let path = work.join(in_archive("OEBPS/text", src));
            let image = image::open(&path)
                .unwrap_or_else(|e| panic!("{src} does not lead to an image: {e}"))
                .to_luma8();
            let mean =
                image.pixels().map(|p| u64::from(p[0])).sum::<u64>() / image.pixels().len() as u64;
            [0u8, 128, 255]
                .into_iter()
                .min_by_key(|level| (i64::from(*level) - mean as i64).abs())
                .unwrap()
        })
        .collect()
}

/// Every image in the manifest is in the archive, under an href of its own,
/// and every image in the archive is in the manifest.
fn assert_manifest_matches_archive(work: &Path) {
    let opf = xml::parse_file(&work.join("OEBPS/content.opf")).unwrap();
    let hrefs: Vec<String> = structure::manifest_items(&opf)
        .unwrap()
        .into_iter()
        .filter(|item| item.media_type.starts_with("image/"))
        .map(|item| item.decoded_href())
        .collect();

    for href in &hrefs {
        assert!(
            work.join(in_archive("OEBPS", href)).is_file(),
            "{href} is missing"
        );
    }
    let declared: std::collections::BTreeSet<std::path::PathBuf> = hrefs
        .iter()
        .map(|href| work.join(in_archive("OEBPS", href)))
        .collect();
    assert_eq!(
        declared.len(),
        hrefs.len(),
        "two items share a file: {hrefs:?}"
    );

    for file in image_files(work) {
        assert!(
            declared.contains(&file),
            "{} is packaged but not in the manifest",
            file.display()
        );
    }
}

fn image_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(image_files(&path));
        } else if image::ImageFormat::from_path(&path).is_ok() {
            found.push(path);
        }
    }
    found
}

/// Upstream's issue #11: `image.png` and `image.jpeg` both became `image.jpg`,
/// and whichever was converted second replaced the first.
#[test]
fn images_that_would_share_a_name_are_both_kept() {
    let work = convert_images_book(&[
        ("images/image.png", solid(image::ImageFormat::Png, 0)),
        ("images/image.jpeg", solid(image::ImageFormat::Jpeg, 255)),
    ]);

    assert_eq!(
        shown_greys(work.path()),
        [0, 255],
        "{:?}",
        chapter_sources(work.path())
    );
    assert_manifest_matches_archive(work.path());
}

/// A converted image must not land on another image that already has the name
/// it wants.
#[test]
fn a_conversion_never_overwrites_an_existing_image() {
    let work = convert_images_book(&[
        ("images/plate.png", solid(image::ImageFormat::Png, 0)),
        ("images/plate.jpg", solid(image::ImageFormat::Jpeg, 255)),
    ]);

    assert_eq!(
        shown_greys(work.path()),
        [0, 255],
        "{:?}",
        chapter_sources(work.path())
    );
    assert_manifest_matches_archive(work.path());
}

/// macOS and Windows hold `IMG.JPG` and `IMG.jpg` as one file, so converting
/// `IMG.JPG` under the lowercase name and then deleting the source deleted the
/// output with it. Replaced in place, it keeps its own name.
#[test]
fn an_uppercase_jpg_is_replaced_under_its_own_name() {
    let work =
        convert_images_book(&[("images/IMG_0001.JPG", solid(image::ImageFormat::Jpeg, 128))]);

    assert_eq!(chapter_sources(work.path()), ["../images/IMG_0001.JPG"]);
    assert_eq!(shown_greys(work.path()), [128]);
    assert_manifest_matches_archive(work.path());
}

/// Images that share a filename in different directories can now be renamed
/// differently, so a reference has to be followed by its path, not matched by
/// its filename.
#[test]
fn same_named_images_in_different_directories_keep_their_own_references() {
    let work = convert_images_book(&[
        ("a/pic.png", solid(image::ImageFormat::Png, 0)),
        ("b/pic.png", solid(image::ImageFormat::Png, 255)),
        ("b/pic.jpg", solid(image::ImageFormat::Jpeg, 128)),
    ]);

    assert_eq!(
        shown_greys(work.path()),
        [0, 255, 128],
        "{:?}",
        chapter_sources(work.path())
    );
    assert_manifest_matches_archive(work.path());
}

/// An image beside the package's folder rather than in it, reached with `..`
/// or from the root with `/`, is as much the book's as any other, and its
/// references follow it when it is converted.
#[test]
fn an_image_outside_the_package_folder_is_converted_and_followed() {
    let work = optimize_book(
        &[
            ("../Images/cover.png", solid(image::ImageFormat::Png, 0)),
            ("/Images/plate.png", solid(image::ImageFormat::Png, 255)),
        ],
        r#"<p><img src="../../Images/cover.png" alt=""/></p><p><img src="../../Images/plate.png" alt=""/></p>"#,
        &ProcessingOptions::default(),
    );

    assert_eq!(
        chapter_sources(work.path()),
        ["../../Images/cover.jpg", "../../Images/plate.jpg"]
    );
    assert_eq!(shown_greys(work.path()), [0, 255]);
    assert_manifest_matches_archive(work.path());
}

/// An href with `.` or `..` in it names the same file as one without. It keeps
/// naming it once the file is renamed, even when the filename alone would not
/// say which of two renamed images was meant.
#[test]
fn an_href_spelled_with_dot_segments_follows_its_image() {
    let work = convert_images_book(&[
        ("images/cover.jpeg", solid(image::ImageFormat::Jpeg, 255)),
        ("images/a/../cover.png", solid(image::ImageFormat::Png, 0)),
        ("other/./cover.png", solid(image::ImageFormat::Png, 128)),
    ]);

    assert_eq!(
        shown_greys(work.path()),
        [255, 0, 128],
        "{:?}",
        chapter_sources(work.path())
    );
    assert_manifest_matches_archive(work.path());
}

/// An image named through an entity that stands for a quote, in a chapter that
/// is recovered, follows its image: filled into the `src` as written, the
/// quote ended it, and the chapter named a file that was never there.
#[test]
fn an_image_named_through_an_entity_with_a_quote_in_it_is_followed() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="bookid">urn:uuid:quoted</dc:identifier><dc:title>Quoted</dc:title></metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="cover" href="a&quot;b.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>
"#;
    let chapter = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html [<!ENTITY image "a&#34;b.png">]>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body><p>Text<br></p><img src="&image;" alt="cover"/></body></html>
"#;
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/chapter1.xhtml", chapter.as_bytes()),
            ("OEBPS/a\"b.png", &solid(image::ImageFormat::Png, 0)),
        ],
    );

    let output = dir.path().join("out.epub");
    let options = ProcessingOptions {
        text_cleanup: false,
        ..ProcessingOptions::default()
    };
    process_epub(&input, &output, &options, |_, _| {}).unwrap();
    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();

    let chapter = fs::read_to_string(work.path().join("OEBPS/chapter1.xhtml")).unwrap();
    let doc = epubkit_core::xml::parse_strict(chapter.as_bytes()).unwrap();
    let images = epubkit_core::xml::find_nodes(&doc, "//*[local-name()='img']").unwrap();
    let source = images[0].get_attribute("src").unwrap();
    assert_eq!(source, "a\"b.jpg", "{chapter}");
    assert!(
        work.path().join("OEBPS").join(&source).is_file(),
        "{source} is not in the book: {chapter}"
    );
}

/// Every image a chapter's `<style>` names is still there after the images are
/// converted, whatever entities the style holds: one beside a url, one in it,
/// one standing for another site, whose url is that site's, one that is the
/// whole style, or one that stands for nothing.
#[test]
fn a_style_with_entities_in_it_names_only_images_that_are_there() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="bookid">urn:uuid:entities</dc:identifier><dc:title>Entities</dc:title></metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="cover" href="cover.png" media-type="image/png"/>
    <item id="plate" href="images/plate.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>
"#;
    let chapter = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html [<!ENTITY family "serif"><!ENTITY dir "images/"><!ENTITY cdn "https://cdn.example/"><!ENTITY image "cover.png"><!ENTITY rule ".d { background: url(&image;) }"><!ENTITY empty "">]>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title><style type="text/css">.a { font-family: &family;; background: url(cover.png) }</style><style type="text/css">.b { background: url(&dir;plate.png) }</style><style type="text/css">.c { background: url(&cdn;cover.png) }</style><style type="text/css">&rule;</style><style type="text/css">.e { background: url(&empty;cover.png) }</style></head>
<body><p class="a b c d e">Body</p><p><img src="cover.png" alt=""/></p></body></html>
"#;
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/chapter1.xhtml", chapter.as_bytes()),
            ("OEBPS/cover.png", &solid(image::ImageFormat::Png, 0)),
            (
                "OEBPS/images/plate.png",
                &solid(image::ImageFormat::Png, 90),
            ),
        ],
    );

    let output = dir.path().join("out.epub");
    let options = ProcessingOptions {
        text_cleanup: false,
        ..ProcessingOptions::default()
    };
    process_epub(&input, &output, &options, |_, _| {}).unwrap();
    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();

    let chapter = fs::read_to_string(work.path().join("OEBPS/chapter1.xhtml")).unwrap();
    assert!(
        chapter.contains(".a { font-family: &family;; background: url(cover.jpg) }"),
        "{chapter}"
    );
    assert!(
        chapter.contains(".b { background: url(images/plate.jpg) }"),
        "{chapter}"
    );
    assert!(
        chapter.contains(".c { background: url(&cdn;cover.png) }"),
        "{chapter}"
    );
    assert!(
        chapter.contains(r#"<style type="text/css">.d { background: url(cover.jpg) }</style>"#),
        "{chapter}"
    );
    assert!(
        chapter.contains(".e { background: url(cover.jpg) }"),
        "{chapter}"
    );
    for image in ["OEBPS/cover.jpg", "OEBPS/images/plate.jpg"] {
        assert!(work.path().join(image).is_file(), "{image} is missing");
    }
    for gone in ["OEBPS/cover.png", "OEBPS/images/plate.png"] {
        assert!(!work.path().join(gone).exists(), "{gone} is still there");
    }
}

/// An SVG document in the book names its images as a chapter does, and has to
/// follow them when they are converted. It is an SVG document by its media
/// type, whatever its name.
#[test]
fn an_svg_document_follows_the_images_it_draws() {
    for name in ["map.svg", "map"] {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.epub");
        let opf = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="bookid">urn:uuid:svg</dc:identifier><dc:title>SVG</dc:title></metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="map" href="{name}" media-type="image/svg+xml"/>
    <item id="plate" href="images/plate.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="ch1"/><itemref idref="map"/></spine>
</package>
"#
        );
        let svg = br#"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 64 64"><image width="64" height="64" xlink:href="images/plate.png"/><text x="5" y="60">Map</text></svg>
"#;
        let svg_path = format!("OEBPS/{name}");
        common::write_epub(
            &input,
            &[
                ("mimetype", b"application/epub+zip"),
                ("META-INF/container.xml", common::CONTAINER_XML),
                ("OEBPS/content.opf", opf.as_bytes()),
                ("OEBPS/chapter1.xhtml", CLEAN_CHAPTER.as_bytes()),
                (&svg_path, svg),
                ("OEBPS/images/plate.png", &solid(image::ImageFormat::Png, 0)),
            ],
        );

        let output = dir.path().join("out.epub");
        process_epub(&input, &output, &ProcessingOptions::default(), |_, _| {}).unwrap();
        let work = tempfile::tempdir().unwrap();
        package::extract_epub(&output, work.path()).unwrap();

        let svg = fs::read_to_string(work.path().join(&svg_path)).unwrap();
        assert!(
            svg.contains(r#"xlink:href="images/plate.jpg""#),
            "{name}: {svg}"
        );
        assert!(work.path().join("OEBPS/images/plate.jpg").is_file());
    }
}

/// An SVG document can use a stylesheet's rules as much as a chapter can, so
/// what it uses counts when deciding which rules nothing uses, whatever the
/// document is called.
#[test]
fn rules_only_an_svg_document_uses_are_kept() {
    for name in ["page2.svg", "page2"] {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.epub");
        let opf = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="bookid">urn:uuid:svgcss</dc:identifier><dc:title>SVG CSS</dc:title></metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="page" href="{name}" media-type="image/svg+xml"/>
    <item id="css" href="style.css" media-type="text/css"/>
  </manifest>
  <spine><itemref idref="ch1"/><itemref idref="page"/></spine>
</package>
"#
        );
        let svg = br#"<?xml version="1.0" encoding="UTF-8"?>
<?xml-stylesheet type="text/css" href="style.css"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><text class="balloon" x="10" y="50">Hello</text></svg>
"#;
        let svg_path = format!("OEBPS/{name}");
        common::write_epub(
            &input,
            &[
                ("mimetype", b"application/epub+zip"),
                ("META-INF/container.xml", common::CONTAINER_XML),
                ("OEBPS/content.opf", opf.as_bytes()),
                ("OEBPS/chapter1.xhtml", CLEAN_CHAPTER.as_bytes()),
                (&svg_path, svg),
                (
                    "OEBPS/style.css",
                    b".balloon { font-size: 40px; fill: #333 }\n.unused { color: red }\n",
                ),
            ],
        );

        let output = dir.path().join("out.epub");
        let report =
            process_epub(&input, &output, &ProcessingOptions::default(), |_, _| {}).unwrap();
        let work = tempfile::tempdir().unwrap();
        package::extract_epub(&output, work.path()).unwrap();

        let css = fs::read_to_string(work.path().join("OEBPS/style.css")).unwrap();
        assert!(
            css.contains(".balloon { font-size: 40px; fill: #333 }"),
            "{name}: {css}"
        );
        assert!(!css.contains(".unused"), "{name}: {css}");
        assert_eq!(report.css_rules_removed, 1, "{name}");
    }
}

/// An image is known by its media type and its bytes, not its name. A PNG
/// the manifest declares with no extension, or with one the image step did
/// not know, was left as it was, and nothing said so; so were JPEGs named
/// `.jpe` and `.jfif`. A PNG named as a JPEG is counted as the PNG it is.
#[test]
fn an_image_is_converted_whatever_its_name() {
    let png = solid(image::ImageFormat::Png, 0);
    let jpeg = solid(image::ImageFormat::Jpeg, 255);
    let files = [
        ("images/spread", "image/png", png.clone()),
        ("images/plate.bin", "image/png", png.clone()),
        ("images/photo.jpe", "image/jpeg", jpeg.clone()),
        ("images/scan.jfif", "image/jpeg", jpeg),
        ("images/misnamed.jpg", "image/jpeg", png),
    ];
    let body: String = files
        .iter()
        .map(|(href, ..)| format!(r#"<p><img src="../{href}" alt=""/></p>"#))
        .collect();
    let (work, report) = optimize_files(&files, "", "", &body, &ProcessingOptions::default());

    let converted =
        ["spread", "plate", "photo", "scan", "misnamed"].map(|stem| format!("images/{stem}.jpg"));
    assert_eq!(
        chapter_sources(work.path()),
        converted
            .iter()
            .map(|href| format!("../{href}"))
            .collect::<Vec<_>>()
    );
    let opf = fs::read_to_string(work.path().join("OEBPS/content.opf")).unwrap();
    for href in &converted {
        let bytes = fs::read(work.path().join("OEBPS").join(href)).unwrap();
        assert_eq!(
            image::guess_format(&bytes).unwrap(),
            image::ImageFormat::Jpeg,
            "{href}"
        );
        assert!(
            opf.contains(&format!(r#"href="{href}" media-type="image/jpeg""#)),
            "{href}: {opf}"
        );
    }
    for (href, ..) in &files[..4] {
        assert!(
            !work.path().join("OEBPS").join(href).exists(),
            "{href} stayed"
        );
    }
    assert_manifest_matches_archive(work.path());

    let summary = report.summary();
    assert!(
        summary.contains("Converted 5/5 images (3 PNG→JPEG, 2 baseline JPEG)"),
        "{summary}"
    );
}

/// An image is an image whatever media type the manifest gives it. One
/// declared `application/octet-stream`, or with no media type at all, was
/// left as it was. A file that only begins like one, text that starts "BM"
/// as a BMP does, is not one, and is left alone without a word.
#[test]
fn an_image_declared_as_something_else_is_converted() {
    let png = solid(image::ImageFormat::Png, 0);
    let notes = b"BMW notes, not a bitmap at all".to_vec();
    let files = [
        ("images/plate.png", "application/octet-stream", png.clone()),
        ("images/scan.png", "", png),
        ("images/notes.txt", "text/plain", notes.clone()),
    ];
    let body = r#"<p><img src="../images/plate.png" alt=""/></p><p><img src="../images/scan.png" alt=""/></p><p><a href="../images/notes.txt">Notes</a></p>"#;
    let (work, report) = optimize_files(&files, "", "", body, &ProcessingOptions::default());

    assert_eq!(
        chapter_sources(work.path()),
        ["../images/plate.jpg", "../images/scan.jpg"]
    );
    let opf = fs::read_to_string(work.path().join("OEBPS/content.opf")).unwrap();
    for href in ["images/plate.jpg", "images/scan.jpg"] {
        assert!(
            opf.contains(&format!(r#"href="{href}" media-type="image/jpeg""#)),
            "{href}: {opf}"
        );
    }
    assert!(
        opf.contains(r#"href="images/notes.txt" media-type="text/plain""#),
        "{opf}"
    );
    assert_eq!(
        fs::read(work.path().join("OEBPS/images/notes.txt")).unwrap(),
        notes
    );
    assert_manifest_matches_archive(work.path());
    assert_eq!(
        (
            report.images_converted,
            report.images_unconverted,
            report.images_total
        ),
        (2, 0, 2)
    );
}

/// An image a chapter or a stylesheet shows is part of the book as it is
/// read, though the manifest leaves it out. It was left as it was. Now it is
/// converted, its references follow, and it is declared. An image nothing
/// names, and a file named that is not an image, are left alone.
#[test]
fn an_image_the_manifest_leaves_out_is_converted_and_declared() {
    let png = solid(image::ImageFormat::Png, 0);
    let css = b"body { background-image: url(../images/paper.png); }\n".to_vec();
    let files = [
        ("styles/main.css", "text/css", css),
        ("images/missing.png", UNDECLARED, png.clone()),
        ("images/paper.png", UNDECLARED, png.clone()),
        ("images/stray.png", UNDECLARED, png.clone()),
        ("images/notes.txt", UNDECLARED, b"notes".to_vec()),
    ];
    let body = r#"<p><img src="../images/missing.png" alt=""/></p><p><a href="../images/notes.txt">Notes</a></p>"#;
    let (work, report) = optimize_files(
        &files,
        "",
        r#"<link rel="stylesheet" type="text/css" href="../styles/main.css"/>"#,
        body,
        &ProcessingOptions::default(),
    );

    assert_eq!(chapter_sources(work.path()), ["../images/missing.jpg"]);
    let css = fs::read_to_string(work.path().join("OEBPS/styles/main.css")).unwrap();
    assert!(css.contains("url(../images/paper.jpg)"), "{css}");

    let opf = fs::read_to_string(work.path().join("OEBPS/content.opf")).unwrap();
    for href in ["images/missing.jpg", "images/paper.jpg"] {
        assert!(
            opf.contains(&format!(r#"href="{href}" media-type="image/jpeg""#)),
            "{href}: {opf}"
        );
        assert!(work.path().join("OEBPS").join(href).is_file(), "{href}");
    }
    for left in ["images/stray.png", "images/notes.txt"] {
        assert!(work.path().join("OEBPS").join(left).is_file(), "{left}");
        assert!(!opf.contains(left), "{left}: {opf}");
    }
    assert!(!work.path().join("OEBPS/images/missing.png").exists());

    assert_eq!(report.images_converted, 2);
    let summary = report.summary();
    assert!(
        summary.contains("Declared 2 images the manifest left out"),
        "{summary}"
    );
}

/// Split in Light Novel mode, a spread the manifest left out has both its
/// pages declared, the first as the image it was, the second beside it.
#[test]
fn a_spread_the_manifest_leaves_out_has_both_pages_declared() {
    let files = [("images/spread.png", UNDECLARED, spread())];
    let (work, report) = optimize_files(
        &files,
        "",
        "",
        r#"<div><img src="../images/spread.png" alt=""/></div>"#,
        &light_novel(),
    );

    assert_eq!(
        chapter_sources(work.path()),
        ["../images/spread_part1.jpg", "../images/spread_part2.jpg"]
    );
    assert_manifest_matches_archive(work.path());
    assert_eq!((report.images_declared, report.spreads_split), (1, 1));
}

/// A document too large to read whole, a chapter of more than 32 MiB here,
/// is left exactly as it is, and counted: parsing one takes some fifteen
/// times its size. So is every image it might name, which would otherwise
/// be converted and renamed out from under it, a name written with a
/// percent-escape included; one only the rest of the book names is
/// converted as usual.
#[test]
fn a_document_too_large_to_read_is_left_as_it_is_with_the_images_it_names() {
    let filler =
        "<p>The long afternoon light was failing, and she said it would be well enough.</p>\n";
    let big = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Big</title></head><body>
<p><img src="../images/my%20plate.png" alt=""/></p>
{}</body></html>
"#,
        filler.repeat(32 * 1024 * 1024 / filler.len() + 1)
    );
    let png = solid(image::ImageFormat::Png, 0);
    let files = [
        ("images/my plate.png", "image/png", png.clone()),
        ("images/other.png", "image/png", png),
        (
            "text/big.xhtml",
            "application/xhtml+xml",
            big.clone().into_bytes(),
        ),
    ];
    let body = r#"<p><img src="../images/my%20plate.png" alt=""/></p><p><img src="../images/other.png" alt=""/></p>"#;
    let (work, report) = optimize_files(&files, "", "", body, &ProcessingOptions::default());

    assert!(
        fs::read(work.path().join("OEBPS/text/big.xhtml")).unwrap() == big.as_bytes(),
        "the large chapter changed"
    );
    assert!(work.path().join("OEBPS/images/my plate.png").is_file());
    assert_eq!(
        chapter_sources(work.path()),
        ["../images/my%20plate.png", "../images/other.jpg"]
    );
    assert_eq!(report.documents_too_large, 1);
    assert_eq!((report.images_converted, report.images_unconverted), (1, 1));
    let summary = report.summary();
    assert!(
        summary.contains("Left 1 document too large to process as it was"),
        "{summary}"
    );
}

/// A package document too large to read whole leaves nothing to go on, and
/// the book is refused, saying why, rather than read into memory.
#[test]
fn a_package_document_too_large_to_read_refuses_the_book() {
    let padding = "<!-- padding -->".repeat(32 * 1024 * 1024 / 16 + 1);
    let opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="bookid">urn:uuid:big</dc:identifier><dc:title>Big</dc:title></metadata>
  <manifest><item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="ch1"/></spine>
{padding}
</package>
"#
    );
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/chapter1.xhtml", common::CHAPTER_XHTML),
        ],
    );

    let error = process_epub(
        &input,
        &dir.path().join("out.epub"),
        &ProcessingOptions::default(),
        |_, _| {},
    )
    .expect_err("a package document of 32 MiB is read");
    assert!(error.to_string().contains("too large"), "{error}");
}

/// An image no decoder reads, an icon here, is left as it was, and said to
/// be: the image step skipped it by its name, and the summary was silent. An
/// SVG is drawn, not converted, and counts as neither.
#[test]
fn an_image_no_decoder_reads_is_counted_and_an_svg_is_not() {
    let svg = br#"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10"/></svg>
"#;
    let icon = b"\x00\x00\x01\x00 not really an icon".to_vec();
    let files = [
        (
            "images/plate.png",
            "image/png",
            solid(image::ImageFormat::Png, 0),
        ),
        ("images/mark.ico", "image/x-icon", icon.clone()),
        ("images/map.svg", "image/svg+xml", svg.to_vec()),
    ];
    let body = r#"<p><img src="../images/plate.png" alt=""/><img src="../images/mark.ico" alt=""/><img src="../images/map.svg" alt=""/></p>"#;
    let (work, report) = optimize_files(&files, "", "", body, &ProcessingOptions::default());

    assert_eq!(
        chapter_sources(work.path()),
        [
            "../images/plate.jpg",
            "../images/mark.ico",
            "../images/map.svg"
        ]
    );
    assert_eq!(
        fs::read(work.path().join("OEBPS/images/mark.ico")).unwrap(),
        icon
    );
    assert_eq!(
        fs::read(work.path().join("OEBPS/images/map.svg")).unwrap(),
        svg
    );

    assert_eq!(
        (
            report.images_converted,
            report.images_unconverted,
            report.images_total
        ),
        (1, 1, 2)
    );
    let summary = report.summary();
    assert!(
        summary.contains("Converted 1/2 images (1 PNG→JPEG)"),
        "{summary}"
    );
    assert!(
        summary.contains("Left 1 image that could not be converted as it was"),
        "{summary}"
    );
}

// ---------------------------------------------------------- Light Novel mode

/// A double-page spread: black on the left, white on the right.
fn spread() -> Vec<u8> {
    let picture = image::GrayImage::from_fn(1000, 400, |x, _| {
        image::Luma([if x < 500 { 0 } else { 255 }])
    });
    let mut out = Vec::new();
    image::DynamicImage::ImageLuma8(picture)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .expect("encode fixture image");
    out
}

fn light_novel() -> ProcessingOptions {
    ProcessingOptions {
        light_novel_mode: true,
        ..ProcessingOptions::default()
    }
}

fn read_chapter(work: &Path) -> String {
    fs::read_to_string(work.join("OEBPS/text/chapter1.xhtml")).unwrap()
}

/// Light Novel mode splits a spread into two pages, the right half first for
/// right-to-left reading. Both pages have to reach the manifest and the
/// chapter, or half the picture is lost.
#[test]
fn both_halves_of_a_split_spread_are_shown_right_half_first() {
    let work = optimize_book(
        &[("images/spread.png", spread())],
        r#"<p><img src="../images/spread.png" alt="A spread" class="plate" width="1000" height="400"/></p>"#,
        &light_novel(),
    );

    assert_eq!(
        chapter_sources(work.path()),
        ["../images/spread_part1.jpg", "../images/spread_part2.jpg"]
    );
    assert_eq!(shown_greys(work.path()), [255, 0]);
    assert_manifest_matches_archive(work.path());

    // The second page is shown like the first, but the spread's own size
    // describes neither half.
    let chapter = read_chapter(work.path());
    assert_eq!(chapter.matches(r#"class="plate""#).count(), 2, "{chapter}");
    assert!(!chapter.contains("width="), "{chapter}");
    assert!(!chapter.contains("height="), "{chapter}");
}

/// Full-page illustrations are often wrapped in an SVG sized to the picture.
/// A wrapper sized for the spread would squash each half into it, so it gives
/// way to one plain image per page.
#[test]
fn an_svg_wrapped_spread_becomes_one_image_per_page() {
    let work = optimize_book(
        &[("images/spread.png", spread())],
        r#"<div><svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" version="1.1" width="100%" height="100%" viewBox="0 0 1000 400"><image width="1000" height="400" xlink:href="../images/spread.png"/></svg></div>"#,
        &light_novel(),
    );

    let chapter = read_chapter(work.path());
    assert!(!chapter.contains("<svg"), "{chapter}");
    assert_eq!(
        chapter_sources(work.path()),
        ["../images/spread_part1.jpg", "../images/spread_part2.jpg"]
    );
    assert_eq!(shown_greys(work.path()), [255, 0]);
    assert_manifest_matches_archive(work.path());
}

/// A spread outside the package's folder, or reached by an href with `..` in
/// it, is split and shown like any other.
#[test]
fn a_spread_anywhere_in_the_book_is_split_and_shown() {
    for (href, src) in [
        ("../Images/spread.png", "../../Images/spread.png"),
        ("images/a/../spread.png", "../images/a/../spread.png"),
    ] {
        let work = optimize_book(
            &[(href, spread())],
            &format!(r#"<p><img src="{src}" alt="A spread"/></p>"#),
            &light_novel(),
        );

        let pages: Vec<String> = chapter_sources(work.path());
        assert_eq!(pages.len(), 2, "{href}: {pages:?}");
        assert!(pages[0].ends_with("spread_part1.jpg"), "{href}: {pages:?}");
        assert!(pages[1].ends_with("spread_part2.jpg"), "{href}: {pages:?}");
        assert_eq!(shown_greys(work.path()), [255, 0], "{href}");
        assert_manifest_matches_archive(work.path());
    }
}

#[test]
fn the_summary_counts_a_split_spread_once() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("OEBPS/content.opf", DEMO_OPF.as_bytes()),
            ("OEBPS/chapter1.xhtml", CLEAN_CHAPTER.as_bytes()),
            ("OEBPS/chapter2.xhtml", CLEAN_CHAPTER.as_bytes()),
            ("OEBPS/styles/main.css", DEMO_CSS.as_bytes()),
            ("OEBPS/fonts/body.otf", b"not really a font"),
            ("OEBPS/images/cover.png", &common::png_gradient(300, 400)),
            ("OEBPS/images/plate.png", &spread()),
        ],
    );

    let report = process_epub(
        &input,
        &dir.path().join("out.epub"),
        &light_novel(),
        |_, _| {},
    )
    .unwrap();

    assert_eq!(report.images_total, 2);
    assert_eq!(report.images_converted, 2);
    assert_eq!(report.spreads_split, 1);
    let summary = report.summary();
    assert!(summary.contains("Converted 2/2 images"), "{summary}");
    assert!(summary.contains("Split 1 double-page spread"), "{summary}");
}

// ------------------------------------------------- files that cannot be read

/// The demo book, with some of its files replaced.
fn demo_epub_with(path: &Path, replacements: &[(&str, &[u8])]) {
    let cover = common::png_gradient(300, 400);
    let plate = common::png_gradient(240, 160);
    let mut entries: Vec<(&str, &[u8])> = vec![
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
    ];
    for &(name, bytes) in replacements {
        match entries.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = bytes,
            None => entries.push((name, bytes)),
        }
    }
    common::write_epub(path, &entries);
}

/// A stylesheet saved in a legacy encoding is read in it, and written back as
/// UTF-8 that says so, rather than failing the book.
#[test]
fn a_stylesheet_in_a_legacy_encoding_does_not_sink_the_book() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let output = dir.path().join("out.epub");
    demo_epub_with(
        &input,
        &[(
            "OEBPS/styles/main.css",
            b"@charset \"iso-8859-1\";\n/* \xa9 Verlag */\n.lead { background: url(../images/plate.png); }\n",
        )],
    );

    process_epub(&input, &output, &ProcessingOptions::default(), |_, _| {})
        .expect("one stylesheet must not sink the book");

    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();
    let css = String::from_utf8(fs::read(work.path().join("OEBPS/styles/main.css")).unwrap())
        .expect("a rewritten stylesheet is UTF-8");
    assert!(css.contains("\u{a9} Verlag"), "{css}");
    assert!(css.contains("plate.jpg"), "{css}");
    assert!(
        !css.to_ascii_lowercase().contains("iso-8859-1"),
        "the declaration must match the bytes: {css}"
    );
}

/// A chapter nothing can parse, such as an empty file, is left as it was
/// rather than failing the whole book, and the report says so.
#[test]
fn an_unreadable_chapter_is_left_alone_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let output = dir.path().join("out.epub");
    demo_epub_with(&input, &[("OEBPS/chapter2.xhtml", b"")]);

    let report = process_epub(&input, &output, &ProcessingOptions::default(), |_, _| {})
        .expect("one chapter must not sink the book");

    assert_eq!(report.documents_unreadable, 1);
    assert!(
        report.summary().contains("1 unreadable document"),
        "{}",
        report.summary()
    );

    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();
    assert!(fs::read(work.path().join("OEBPS/chapter2.xhtml"))
        .unwrap()
        .is_empty());
    let one = fs::read_to_string(work.path().join("OEBPS/chapter1.xhtml")).unwrap();
    assert!(
        one.contains("images/plate.jpg"),
        "the rest of the book is done: {one}"
    );
}

// ------------------------------------------------------------ output names

#[test]
fn the_output_is_named_as_chosen() {
    let options = ProcessingOptions {
        filename: metadata::FilenameOptions {
            format: metadata::FilenameFormat::Custom,
            template: "{title} ({original})".into(),
        },
        ..ProcessingOptions::default()
    };
    let (_, report) = run(options);
    assert_eq!(report.output_filename, "The Long Afternoon (in).epub");
}

/// A template that cannot name the book fails before the work, not after.
#[test]
fn a_bad_template_is_refused_before_anything_is_done() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let output = dir.path().join("out.epub");
    write_demo_epub(&input);

    let options = ProcessingOptions {
        filename: metadata::FilenameOptions {
            format: metadata::FilenameFormat::Custom,
            template: "{publisher}".into(),
        },
        ..ProcessingOptions::default()
    };
    let mut steps = 0;
    let error = process_epub(&input, &output, &options, |_, _| steps += 1).unwrap_err();

    assert!(error.to_string().contains("{publisher}"), "{error}");
    assert_eq!(steps, 0, "no step should have started");
    assert!(!output.exists());
}

// ------------------------------------------------- paths that leave the book

/// The book is untrusted, and its manifest hrefs end up in paths the pipeline
/// reads, rewrites and deletes. An absolute href must not hand it files that
/// are not the book's: a font it would delete, a chapter it would repair over,
/// a table of contents it would regenerate over, an image it would convert
/// and then delete.
#[test]
fn manifest_hrefs_cannot_reach_files_outside_the_book() {
    let outside = tempfile::tempdir().unwrap();
    let plant = |name: &str, bytes: &[u8]| {
        let path = outside.path().join(name);
        fs::write(&path, bytes).unwrap();
        (path.to_string_lossy().to_string(), bytes.to_vec())
    };
    let planted = [
        plant("victim.otf", b"not a font, and not the book's"),
        plant("victim.xhtml", b"<p>someone else's page<br></p>"),
        plant("victim.ncx", b"someone else's notes"),
        plant("victim.png", &solid(image::ImageFormat::Png, 128)),
    ];
    let [(font, _), (page, _), (toc, _), (picture, _)] = &planted;

    let opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:outside</dc:identifier>
    <dc:title>Outside</dc:title>
  </metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="font" href="{font}" media-type="font/otf"/>
    <item id="page" href="{page}" media-type="application/xhtml+xml"/>
    <item id="toc" href="{toc}" media-type="application/x-dtbncx+xml"/>
    <item id="picture" href="{picture}" media-type="image/png"/>
  </manifest>
  <spine toc="toc"><itemref idref="ch1"/><itemref idref="page"/></spine>
</package>
"#
    );

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/chapter1.xhtml", common::CHAPTER_XHTML),
        ],
    );
    process_epub(
        &input,
        &dir.path().join("out.epub"),
        &ProcessingOptions::default(),
        |_, _| {},
    )
    .unwrap();

    for (path, bytes) in &planted {
        let now = fs::read(path).ok();
        assert!(now.as_ref() == Some(bytes), "{path} was changed or removed");
    }
    let left: Vec<_> = fs::read_dir(outside.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        left.len(),
        planted.len(),
        "files appeared beside them: {left:?}"
    );
}

/// Nor can container.xml send the pipeline to a package document outside the
/// book, which it would parse and then rewrite. The package inside is used.
#[test]
fn the_container_cannot_point_at_a_package_outside_the_book() {
    let outside = tempfile::tempdir().unwrap();
    let decoy = outside.path().join("victim.opf");
    fs::write(&decoy, common::CONTENT_OPF).unwrap();

    let container = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="{}" media-type="application/oebps-package+xml"/></rootfiles>
</container>
"#,
        decoy.display()
    );

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", container.as_bytes()),
            ("OEBPS/content.opf", common::CONTENT_OPF),
            ("OEBPS/chapter1.xhtml", common::CHAPTER_XHTML),
        ],
    );
    let report = process_epub(
        &input,
        &dir.path().join("out.epub"),
        &ProcessingOptions::default(),
        |_, _| {},
    )
    .unwrap();

    assert!(
        fs::read(&decoy).unwrap() == common::CONTENT_OPF,
        "the package outside the book was rewritten"
    );
    assert_eq!(report.output_filename, "A Writer - Test Book.epub");
}

/// An image that cannot be converted stays as it was, and the summary says
/// so, rather than leaving it to be worked out from the count of converted
/// images.
#[test]
fn an_image_that_cannot_be_converted_is_counted_in_the_summary() {
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:unconverted</dc:identifier>
    <dc:title>Unconverted</dc:title>
  </metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="good" href="good.png" media-type="image/png"/>
    <item id="bad" href="bad.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>
"#;
    let chapter = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title></head>
<body><p><img src="good.png" alt=""/></p><p><img src="bad.png" alt=""/></p></body></html>
"#;
    let good = common::png_gradient(60, 80);

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let output = dir.path().join("out.epub");
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/chapter1.xhtml", chapter),
            ("OEBPS/good.png", &good),
            ("OEBPS/bad.png", b"not an image at all"),
        ],
    );
    let report = process_epub(&input, &output, &ProcessingOptions::default(), |_, _| {}).unwrap();

    assert_eq!(report.images_converted, 1);
    let summary = report.summary();
    assert!(
        summary.contains("Left 1 image that could not be converted as it was"),
        "{summary}"
    );
    assert!(entry_names(&output).contains(&"OEBPS/bad.png".to_string()));
}

/// The summary counts images by how their format changed. It counted them by
/// the first thing said about each, which for a JPEG is how it was resized,
/// so a book of JPEGs listed one entry per size.
#[test]
fn the_summary_counts_images_by_format_not_by_size() {
    let images = [
        ("images/a.jpg", solid(image::ImageFormat::Jpeg, 40)),
        ("images/b.jpg", jpeg_of(1200, 1600)),
        ("images/c.jpg", jpeg_of(900, 1200)),
        ("images/d.png", solid(image::ImageFormat::Png, 200)),
    ];
    let body = "<p>Text.</p>";

    let (_, report) = optimize_book_with_report(&images, body, &ProcessingOptions::default());

    let summary = report.summary();
    assert!(
        summary.contains("Converted 4/4 images (1 PNG→JPEG, 3 baseline JPEG)"),
        "{summary}"
    );
}

fn jpeg_of(width: u32, height: u32) -> Vec<u8> {
    let image =
        image::GrayImage::from_fn(width, height, |x, y| image::Luma([((x ^ y) & 0xFF) as u8]));
    let mut out = Vec::new();
    image::DynamicImage::ImageLuma8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut out),
            image::ImageFormat::Jpeg,
        )
        .expect("encode fixture image");
    out
}

/// A converted image is a JPEG whatever it was before, and its manifest entry
/// has to say so, under its old name too: a PNG named `plate.jpg` and
/// declared `image/png` came out a JPEG still declared a PNG.
#[test]
fn an_image_converted_under_its_own_name_is_declared_a_jpeg() {
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:in-place</dc:identifier>
    <dc:title>In place</dc:title>
  </metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="plate" href="plate.jpg" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>
"#;
    let chapter = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title></head>
<body><p><img src="plate.jpg" alt=""/></p></body></html>
"#;
    let png = solid(image::ImageFormat::Png, 120);

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let output = dir.path().join("out.epub");
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/chapter1.xhtml", chapter),
            ("OEBPS/plate.jpg", &png),
        ],
    );
    process_epub(&input, &output, &ProcessingOptions::default(), |_, _| {}).unwrap();

    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();
    let plate = fs::read(work.path().join("OEBPS/plate.jpg")).unwrap();
    assert_eq!(&plate[..2], &[0xFF, 0xD8], "converted to JPEG");
    let opf = fs::read_to_string(work.path().join("OEBPS/content.opf")).unwrap();
    assert!(
        opf.contains(r#"<item id="plate" href="plate.jpg" media-type="image/jpeg"/>"#),
        "{opf}"
    );
}

// ------------------------------------------ Light Novel mode: shapes kept

/// Optimize, in Light Novel mode, a book whose one chapter,
/// `OEBPS/text/chapter1.xhtml`, has `head` and `body`, with `files` beside it,
/// each a path in `OEBPS`, a media type and its bytes, and `metadata` in its
/// package. Returns the unpacked output.
fn light_novel_book(
    files: &[(&str, &str, Vec<u8>)],
    metadata: &str,
    head: &str,
    body: &str,
) -> tempfile::TempDir {
    optimize_files(files, metadata, head, body, &light_novel()).0
}

/// What [`optimize_files`] takes for the media type of a file it is to leave
/// out of the manifest, though the book holds it.
const UNDECLARED: &str = "(not in the manifest)";

/// [`light_novel_book`] with `options`, and what the run reported. A file of
/// media type [`UNDECLARED`] is in the book but not in its manifest.
fn optimize_files(
    files: &[(&str, &str, Vec<u8>)],
    metadata: &str,
    head: &str,
    body: &str,
    options: &ProcessingOptions,
) -> (tempfile::TempDir, ProcessingReport) {
    let manifest: String = files
        .iter()
        .enumerate()
        .filter(|(_, (_, media_type, _))| *media_type != UNDECLARED)
        .map(|(index, (href, media_type, _))| {
            format!(r#"<item id="file{index}" href="{href}" media-type="{media_type}"/>"#)
        })
        .collect();
    let opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:shapes</dc:identifier>
    <dc:title>Shapes</dc:title>
    {metadata}
  </metadata>
  <manifest>
    <item id="ch1" href="text/chapter1.xhtml" media-type="application/xhtml+xml"/>
    {manifest}
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>
"#
    );
    let chapter = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:xlink="http://www.w3.org/1999/xlink"><head><title>One</title>{head}</head><body>{body}</body></html>
"#
    );

    let mut entries: Vec<(String, Vec<u8>)> = vec![
        ("mimetype".into(), b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml".into(),
            common::CONTAINER_XML.to_vec(),
        ),
        ("OEBPS/content.opf".into(), opf.into_bytes()),
        ("OEBPS/text/chapter1.xhtml".into(), chapter.into_bytes()),
    ];
    for (href, _, bytes) in files {
        entries.push((format!("OEBPS/{href}"), bytes.clone()));
    }
    let entries: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let output = dir.path().join("out.epub");
    common::write_epub(&input, &entries);
    let report = process_epub(&input, &output, options, |_, _| {}).unwrap();

    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();
    (work, report)
}

/// The converted image at `path` in the unpacked book is one image, in the
/// shape it came in: wider than tall, with no pages split off it.
fn assert_shape_kept(work: &Path, path: &str) {
    let parts: Vec<String> = image_files(work)
        .iter()
        .map(|file| file.to_string_lossy().to_string())
        .filter(|file| file.contains("_part"))
        .collect();
    assert!(parts.is_empty(), "split: {parts:?}");

    let image = image::open(work.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"));
    assert!(
        image.width() > image.height() * 2,
        "{path} was turned: {}x{}",
        image.width(),
        image.height()
    );
}

/// An SVG document draws its image in a box the image's own shape, and is
/// shown as it is: a split image's other page was declared but shown
/// nowhere, so half the picture was lost.
#[test]
fn light_novel_mode_keeps_the_shape_of_an_image_an_svg_document_draws() {
    let map = br#"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 1000 400"><image width="1000" height="400" xlink:href="map.png"/><text x="600" y="200">North</text></svg>
"#;
    let work = light_novel_book(
        &[
            ("images/map.png", "image/png", spread()),
            ("images/map.svg", "image/svg+xml", map.to_vec()),
        ],
        "",
        "",
        r#"<p><img src="../images/map.svg" alt="A map"/></p>"#,
    );

    assert_shape_kept(work.path(), "OEBPS/images/map.jpg");
    let svg = fs::read_to_string(work.path().join("OEBPS/images/map.svg")).unwrap();
    assert!(svg.contains(r#"xlink:href="map.jpg""#), "{svg}");
}

/// A background fills its box however it is shaped, and only the first page
/// of a split one was ever named.
#[test]
fn light_novel_mode_keeps_the_shape_of_a_background() {
    let css = b"body { background-image: url(../images/paper.png); }\n";
    let work = light_novel_book(
        &[
            ("images/paper.png", "image/png", spread()),
            ("styles/main.css", "text/css", css.to_vec()),
        ],
        "",
        r#"<link rel="stylesheet" type="text/css" href="../styles/main.css"/>"#,
        "<p>Text.</p>",
    );

    assert_shape_kept(work.path(), "OEBPS/images/paper.jpg");
    let css = fs::read_to_string(work.path().join("OEBPS/styles/main.css")).unwrap();
    assert!(css.contains("url(../images/paper.jpg)"), "{css}");
}

/// An SVG that draws more than its image, a label here, places what it draws
/// on the image as it is shaped. A reshaped image no longer lies under them.
#[test]
fn light_novel_mode_keeps_the_shape_of_an_image_an_illustration_draws() {
    let work = light_novel_book(
        &[("images/plate.png", "image/png", spread())],
        "",
        "",
        r#"<div><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 400"><image width="1000" height="400" xlink:href="../images/plate.png"/><text x="700" y="200">A label</text></svg></div>"#,
    );

    assert_shape_kept(work.path(), "OEBPS/images/plate.jpg");
    let chapter = read_chapter(work.path());
    assert!(chapter.contains("A label"), "{chapter}");
    assert_eq!(chapter.matches("plate.jpg").count(), 1, "{chapter}");
}

/// An image in a line of text is part of the line. Split, its halves were
/// shown one after the other in it; turned, it stood on end.
#[test]
fn light_novel_mode_keeps_the_shape_of_an_image_in_text() {
    let work = light_novel_book(
        &[("images/mark.png", "image/png", spread())],
        "",
        "",
        r#"<p>A word <img src="../images/mark.png" alt="mark"/> in a line.</p>"#,
    );

    assert_shape_kept(work.path(), "OEBPS/images/mark.jpg");
    assert_eq!(chapter_sources(work.path()), ["../images/mark.jpg"]);
}

/// A heading's image is its title, not a page of art: split, "Chapter One"
/// read "One Chapter", right half first.
#[test]
fn light_novel_mode_keeps_the_shape_of_a_heading_image() {
    let work = light_novel_book(
        &[("images/title.png", "image/png", spread())],
        "",
        "",
        r#"<h1><img src="../images/title.png" alt="Chapter One"/></h1><p>Text.</p>"#,
    );

    assert_shape_kept(work.path(), "OEBPS/images/title.jpg");
    assert_eq!(chapter_sources(work.path()), ["../images/title.jpg"]);
}

/// The cover is what a reader shows for the book. Turned, it lay on its side
/// there; split, it was half of itself.
#[test]
fn light_novel_mode_keeps_the_shape_of_the_cover() {
    let work = light_novel_book(
        &[("images/cover.png", "image/png", spread())],
        r#"<meta name="cover" content="file0"/>"#,
        "",
        r#"<div><img src="../images/cover.png" alt="Cover"/></div>"#,
    );

    assert_shape_kept(work.path(), "OEBPS/images/cover.jpg");
}

/// A spread shown on its own as a page of art is still split.
#[test]
fn light_novel_mode_still_splits_a_spread_shown_on_its_own() {
    let work = light_novel_book(
        &[("images/spread.png", "image/png", spread())],
        "",
        "",
        r#"<p>Text before.</p><div class="plate"><img src="../images/spread.png" alt="A spread"/></div><p>Text after.</p>"#,
    );

    assert_eq!(
        chapter_sources(work.path()),
        ["../images/spread_part1.jpg", "../images/spread_part2.jpg"]
    );
}

/// Optimize, in Light Novel mode, a book showing `images/plate.png` by
/// `body`, with `css` in a stylesheet the chapter links and `style` in a
/// `<style>` element of its own. Returns the unpacked output.
fn styled_plate(css: &str, style: &str, body: &str) -> tempfile::TempDir {
    light_novel_book(
        &[
            ("images/plate.png", "image/png", spread()),
            ("styles/main.css", "text/css", css.as_bytes().to_vec()),
        ],
        "",
        &format!(
            r#"<link rel="stylesheet" type="text/css" href="../styles/main.css"/><style type="text/css">{style}</style>"#
        ),
        body,
    )
}

/// An image in a box the book sizes for it would not fit it reshaped: a
/// split image's second page was cut off below the frame, or ran over what
/// came after it, and a turned one stood on end in a box made for it lying
/// down. Nor would an image the book turns itself, or lays over another
/// thing: its pages were turned again, or laid over each other.
#[test]
fn light_novel_mode_keeps_the_shape_of_an_image_in_a_frame() {
    let cases = [
        // A frame by style attributes, and the same by a stylesheet.
        (
            "",
            "",
            r#"<p>Text.</p><div style="width:500px;height:200px;overflow:hidden"><img src="../images/plate.png" alt="" style="width:100%;height:100%"/></div><p>After.</p>"#,
        ),
        (
            ".frame { width: 500px; height: 200px; overflow: hidden; } .frame img { width: 100%; height: 100%; }",
            "",
            r#"<p>Text.</p><div class="frame"><img src="../images/plate.png" alt=""/></div><p>After.</p>"#,
        ),
        // A height capped in another unit, by an id, in a media query of
        // the chapter's own style.
        (
            "",
            "@media screen { #box { max-height: 12em } }",
            r#"<div id="box"><p><img src="../images/plate.png" alt=""/></p></div>"#,
        ),
        // A box a screen high holds one page, and a box of a set shape one
        // shape.
        (
            "div.page { height: 100vh }",
            "",
            r#"<div class="page"><img src="../images/plate.png" alt=""/></div>"#,
        ),
        (
            "figure { aspect-ratio: 5 / 2 }",
            "",
            r#"<figure><img src="../images/plate.png" alt=""/></figure>"#,
        ),
        // A frame around an SVG that shows the image.
        (
            "",
            "",
            r#"<div style="height: 200px"><svg xmlns="http://www.w3.org/2000/svg" width="100%" height="100%" viewBox="0 0 1000 400"><image width="1000" height="400" xlink:href="../images/plate.png"/></svg></div>"#,
        ),
        // The image's own height, which each page would take.
        (
            "img.plate { height: 200px }",
            "",
            r#"<div><img class="plate" src="../images/plate.png" alt=""/></div>"#,
        ),
        // An image the book turns, and one it lays over the page.
        (
            "",
            "",
            r#"<div><img src="../images/plate.png" alt="" style="transform: rotate(90deg)"/></div>"#,
        ),
        (
            ".over { position: absolute; top: 0; left: 0 }",
            "",
            r#"<div><img class="over" src="../images/plate.png" alt=""/></div>"#,
        ),
        // A height that is a share of the page's, through every box from the
        // page down.
        (
            "html, body { height: 100% } .frame { height: 100% }",
            "",
            r#"<div class="frame"><img src="../images/plate.png" alt=""/></div>"#,
        ),
        (
            "html, body, .plates { height: 100% } .frame { max-height: 50% }",
            "",
            r#"<div class="plates"><div class="frame"><p><img src="../images/plate.png" alt=""/></p></div></div>"#,
        ),
        // The page's own box, a screen high, hiding what overflows it.
        (
            "body { height: 100vh; overflow: hidden }",
            "",
            r#"<div><img src="../images/plate.png" alt=""/></div>"#,
        ),
        // A height the image's pages after the first take, which lose the
        // id that undoes it for the first.
        (
            "img.plate { height: 200px } #plate { height: auto }",
            "",
            r#"<div><img id="plate" class="plate" src="../images/plate.png" alt=""/></div>"#,
        ),
        // A frame on some screens, and one only some screens undo.
        (
            "@media (orientation: landscape) { .frame { height: 200px } }",
            "",
            r#"<div class="frame"><img src="../images/plate.png" alt=""/></div>"#,
        ),
        (
            ".frame { height: 200px } @media (min-width: 2000px) { .frame { height: auto } }",
            "",
            r#"<div class="frame"><img src="../images/plate.png" alt=""/></div>"#,
        ),
    ];

    for (css, style, body) in cases {
        let work = styled_plate(css, style, body);
        assert_eq!(
            chapter_sources(work.path()),
            ["../images/plate.jpg"],
            "css: {css:?}, style: {style:?}, body: {body}"
        );
        assert_shape_kept(work.path(), "OEBPS/images/plate.jpg");
    }
}

/// Only what a reshaped image would not fit keeps it whole: a box fitted to
/// the page, one that grows to hold both pages, and rules for other things
/// leave a spread to be split.
#[test]
fn light_novel_mode_still_splits_a_spread_in_a_box_that_grows() {
    let cases = [
        // Fitted to the page, as light novels' plates are.
        (
            ".plate { height: 100%; text-align: center } .plate img { max-width: 100%; max-height: 100% }",
            r#"<div class="plate"><img src="../images/plate.png" alt=""/></div>"#,
        ),
        // A screen high itself, which each page then is.
        (
            "img { height: 95vh }",
            r#"<div><img src="../images/plate.png" alt=""/></div>"#,
        ),
        // A set width, or a least height, which a box grows past.
        (
            "div.wide { width: 500px; min-height: 10em }",
            r#"<div class="wide"><img src="../images/plate.png" alt=""/></div>"#,
        ),
        // Rules for something else: another class, a box drawn before the
        // image's, and a box of the right class but another element.
        (
            ".other { height: 200px } div.plate:before { content: ''; height: 2em } p.plate { height: 200px }",
            r#"<div class="plate"><img src="../images/plate.png" alt=""/></div>"#,
        ),
        // A box of the right class elsewhere: inside another, after another.
        (
            ".gallery .plate { height: 200px } h1 + .plate { height: 200px }",
            r#"<p>Text.</p><div class="plate"><img src="../images/plate.png" alt=""/></div>"#,
        ),
        // A frame the cascade undoes: by a later rule, a more specific one,
        // and the box's own style.
        (
            ".plate { height: 200px } .plate { height: auto }",
            r#"<div class="plate"><img src="../images/plate.png" alt=""/></div>"#,
        ),
        (
            "div#plates { height: auto } .plate { height: 200px }",
            r#"<div id="plates" class="plate"><img src="../images/plate.png" alt=""/></div>"#,
        ),
        (
            ".plate { height: 200px }",
            r#"<div class="plate" style="height: auto"><img src="../images/plate.png" alt=""/></div>"#,
        ),
        // A share of a page whose boxes grow: the page's own run on onto the
        // pages after, and a box between them and the frame grows.
        (
            "html, body { height: 100% }",
            r#"<div><img src="../images/plate.png" alt=""/></div>"#,
        ),
        (
            "body { height: 100vh }",
            r#"<div><img src="../images/plate.png" alt=""/></div>"#,
        ),
        (
            "html, body { height: 100% } .plate { height: 100% }",
            r#"<div><div class="plate"><img src="../images/plate.png" alt=""/></div></div>"#,
        ),
        // A frame for print only.
        (
            "@media print { .plate { height: 200px } }",
            r#"<div class="plate"><img src="../images/plate.png" alt=""/></div>"#,
        ),
    ];

    for (css, body) in cases {
        let work = styled_plate(css, "", body);
        assert_eq!(
            chapter_sources(work.path()),
            ["../images/plate_part1.jpg", "../images/plate_part2.jpg"],
            "css: {css:?}, body: {body}"
        );
    }
}

/// What styles a chapter is what it links, and what that imports, as a
/// reader reads them: a frame in a stylesheet it does not link, or links or
/// imports for print, frames nothing there, and one it imports does.
#[test]
fn light_novel_mode_reads_the_stylesheets_a_chapter_links() {
    let frame = ".plate { height: 200px }";
    let body = r#"<div class="plate"><img src="../images/plate.png" alt=""/></div>"#;
    let link = r#"<link rel="stylesheet" type="text/css" href="../styles/main.css"/>"#;
    let sources = |stylesheets: &[(&str, &str)], head: &str| {
        let mut files = vec![("images/plate.png", "image/png", spread())];
        for (href, css) in stylesheets {
            files.push((href, "text/css", css.as_bytes().to_vec()));
        }
        chapter_sources(light_novel_book(&files, "", head, body).path())
    };
    let split = ["../images/plate_part1.jpg", "../images/plate_part2.jpg"];
    let whole = ["../images/plate.jpg"];

    assert_eq!(sources(&[("styles/frames.css", frame)], ""), split);
    assert_eq!(
        sources(
            &[("styles/main.css", ""), ("styles/frames.css", frame)],
            link
        ),
        split
    );
    assert_eq!(
        sources(
            &[("styles/main.css", frame)],
            r#"<link rel="stylesheet" type="text/css" media="print" href="../styles/main.css"/>"#
        ),
        split
    );
    assert_eq!(
        sources(
            &[
                ("styles/main.css", "@import url(\"frames.css\") print;"),
                ("styles/frames.css", frame)
            ],
            link
        ),
        split
    );

    assert_eq!(
        sources(
            &[
                ("styles/main.css", "@import url(\"frames.css\");"),
                ("styles/frames.css", frame)
            ],
            link
        ),
        whole
    );
    assert_eq!(
        sources(
            &[("styles/frames.css", frame)],
            r#"<style type="text/css">@import "../styles/frames.css";</style>"#
        ),
        whole
    );
}

/// A stylesheet too large to read could frame anything, and the images of a
/// chapter it styles keep their shape.
#[test]
fn light_novel_mode_keeps_the_shape_of_an_image_a_stylesheet_too_large_to_read_styles() {
    let rule = ".other { color: black }\n";
    let large = rule.repeat(32 * 1024 * 1024 / rule.len() + 1);
    let work = light_novel_book(
        &[
            ("images/plate.png", "image/png", spread()),
            ("styles/main.css", "text/css", large.into_bytes()),
        ],
        "",
        r#"<link rel="stylesheet" type="text/css" href="../styles/main.css"/>"#,
        r#"<div><img src="../images/plate.png" alt=""/></div>"#,
    );
    assert_eq!(chapter_sources(work.path()), ["../images/plate.jpg"]);
    assert_shape_kept(work.path(), "OEBPS/images/plate.jpg");
}

// --------------------------------------------- images converted in parallel

/// Images are converted on several threads, and finish in whatever order
/// they finish, a large one last. Which takes which name is settled in the
/// manifest's order all the same, so a book converts the same every time.
#[test]
fn images_take_their_names_in_manifest_order_whatever_finishes_first() {
    let large = image::GrayImage::from_pixel(1200, 1800, image::Luma([0]));
    let mut large_png = Vec::new();
    image::DynamicImage::ImageLuma8(large)
        .write_to(
            &mut std::io::Cursor::new(&mut large_png),
            image::ImageFormat::Png,
        )
        .unwrap();
    let images = [
        ("images/plate.png", large_png),
        ("images/plate.gif", solid(image::ImageFormat::Png, 128)),
        ("images/plate.bmp", solid(image::ImageFormat::Png, 255)),
        ("images/plate.jpeg", solid(image::ImageFormat::Jpeg, 128)),
        ("images/plate.webp", solid(image::ImageFormat::Png, 0)),
    ];

    let mut books = Vec::new();
    for _ in 0..3 {
        let work = convert_images_book(&images);
        assert_eq!(
            chapter_sources(work.path()),
            [
                "../images/plate.jpg",
                "../images/plate-2.jpg",
                "../images/plate-3.jpg",
                "../images/plate-4.jpg",
                "../images/plate-5.jpg",
            ]
        );
        assert_eq!(shown_greys(work.path()), [0, 128, 255, 128, 0]);
        assert_manifest_matches_archive(work.path());
        books.push(
            image_files(work.path())
                .iter()
                .map(|file| {
                    let name = file.strip_prefix(work.path()).unwrap().to_path_buf();
                    (name, fs::read(file).unwrap())
                })
                .collect::<BTreeMap<_, _>>(),
        );
    }
    assert!(
        books.windows(2).all(|pair| pair[0] == pair[1]),
        "converted differently"
    );
}

/// A TIFF header claiming 4294967295 x 4294967295 pixels of one grey sample.
fn impossible_tiff() -> Vec<u8> {
    let mut tiff = b"II*\0\x08\0\0\0".to_vec();
    let entries: &[(u16, u16, u32)] = &[
        (256, 4, u32::MAX), // width
        (257, 4, u32::MAX), // height
        (258, 3, 8),        // bits per sample
        (259, 3, 1),        // uncompressed
        (262, 3, 1),        // black is zero
        (273, 4, 0),        // strip offsets
        (277, 3, 1),        // samples per pixel
        (278, 4, u32::MAX), // rows per strip
        (279, 4, 0),        // strip byte counts
    ];
    tiff.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for &(tag, kind, value) in entries {
        tiff.extend_from_slice(&tag.to_le_bytes());
        tiff.extend_from_slice(&kind.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        if kind == 3 {
            tiff.extend_from_slice(&(value as u16).to_le_bytes());
            tiff.extend_from_slice(&[0, 0]);
        } else {
            tiff.extend_from_slice(&value.to_le_bytes());
        }
    }
    tiff.extend_from_slice(&[0; 4]);
    tiff
}

/// An image whose conversion would need more memory than the image step sets
/// aside for every image together, a 12000 x 12000 PNG of sixteen bits a
/// channel with alpha here, is left as it is, and said to be, before any of
/// it is decoded. One the whole budget was too little for converted all the
/// same, with nothing else beside it, to well past the budget: this one
/// would take 1.7 GB.
#[test]
fn an_image_too_large_for_the_memory_set_aside_is_left_as_it_is() {
    let images = [
        ("images/vast.png", common::png_claiming(12000, 12000, 16, 6)),
        ("images/plate.png", solid(image::ImageFormat::Png, 0)),
    ];
    let body = r#"<p><img src="../images/vast.png" alt=""/></p><p><img src="../images/plate.png" alt=""/></p>"#;

    let (work, report) = optimize_book_with_report(&images, body, &ProcessingOptions::default());

    assert_eq!((report.images_converted, report.images_unconverted), (1, 1));
    assert!(work.path().join("OEBPS/images/vast.png").is_file());
    let detail = report
        .image_details
        .iter()
        .find(|detail| detail.starts_with("vast.png"))
        .expect("a word on vast.png");
    assert!(
        detail.contains("more than the 1024 MB set aside"),
        "{detail}"
    );
}

/// An image whose header claims more pixels than any could hold is one that
/// cannot be converted, like any other. Reckoning the memory it would need
/// overflowed, which in a debug build stopped the whole book.
#[test]
fn an_image_claiming_impossible_dimensions_is_skipped() {
    let images = [
        ("images/huge.tif", impossible_tiff()),
        ("images/plate.png", solid(image::ImageFormat::Png, 0)),
    ];
    let body = r#"<p><img src="../images/huge.tif" alt=""/></p><p><img src="../images/plate.png" alt=""/></p>"#;

    let (work, report) = optimize_book_with_report(&images, body, &ProcessingOptions::default());

    assert_eq!(report.images_converted, 1);
    assert_eq!(report.images_unconverted, 1);
    assert!(work.path().join("OEBPS/images/huge.tif").is_file());
}
