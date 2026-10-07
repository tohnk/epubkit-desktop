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
    process_epub(&input, &output, options, |_, _| {}).unwrap();

    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();
    work
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

/// An SVG document in the book names its images as a chapter does, and has to
/// follow them when they are converted.
#[test]
fn an_svg_document_follows_the_images_it_draws() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="bookid">urn:uuid:svg</dc:identifier><dc:title>SVG</dc:title></metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="map" href="map.svg" media-type="image/svg+xml"/>
    <item id="plate" href="images/plate.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="ch1"/><itemref idref="map"/></spine>
</package>
"#;
    let svg = br#"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 64 64"><image width="64" height="64" xlink:href="images/plate.png"/><text x="5" y="60">Map</text></svg>
"#;
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/chapter1.xhtml", CLEAN_CHAPTER.as_bytes()),
            ("OEBPS/map.svg", svg),
            ("OEBPS/images/plate.png", &solid(image::ImageFormat::Png, 0)),
        ],
    );

    let output = dir.path().join("out.epub");
    process_epub(&input, &output, &ProcessingOptions::default(), |_, _| {}).unwrap();
    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();

    let svg = fs::read_to_string(work.path().join("OEBPS/map.svg")).unwrap();
    assert!(svg.contains(r#"xlink:href="images/plate.jpg""#), "{svg}");
    assert!(work.path().join("OEBPS/images/plate.jpg").is_file());
}

/// An SVG document can use a stylesheet's rules as much as a chapter can, so
/// what it uses counts when deciding which rules nothing uses.
#[test]
fn rules_only_an_svg_document_uses_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="bookid">urn:uuid:svgcss</dc:identifier><dc:title>SVG CSS</dc:title></metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="page" href="page2.svg" media-type="image/svg+xml"/>
    <item id="css" href="style.css" media-type="text/css"/>
  </manifest>
  <spine><itemref idref="ch1"/><itemref idref="page"/></spine>
</package>
"#;
    let svg = br#"<?xml version="1.0" encoding="UTF-8"?>
<?xml-stylesheet type="text/css" href="style.css"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><text class="balloon" x="10" y="50">Hello</text></svg>
"#;
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/chapter1.xhtml", CLEAN_CHAPTER.as_bytes()),
            ("OEBPS/page2.svg", svg),
            (
                "OEBPS/style.css",
                b".balloon { font-size: 40px; fill: #333 }\n.unused { color: red }\n",
            ),
        ],
    );

    let output = dir.path().join("out.epub");
    let report = process_epub(&input, &output, &ProcessingOptions::default(), |_, _| {}).unwrap();
    let work = tempfile::tempdir().unwrap();
    package::extract_epub(&output, work.path()).unwrap();

    let css = fs::read_to_string(work.path().join("OEBPS/style.css")).unwrap();
    assert!(
        css.contains(".balloon { font-size: 40px; fill: #333 }"),
        "{css}"
    );
    assert!(!css.contains(".unused"), "{css}");
    assert_eq!(report.css_rules_removed, 1);
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
