mod common;

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use epubkit_core::package::{self, MIMETYPE};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

fn entry_names(path: &Path) -> Vec<String> {
    let archive = ZipArchive::new(File::open(path).unwrap()).unwrap();
    archive.file_names().map(|s| s.to_string()).collect()
}

#[test]
fn roundtrip_preserves_content_and_validity() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let work = dir.path().join("work");
    let output = dir.path().join("out.epub");

    common::write_minimal_epub(&input);

    package::extract_epub(&input, &work).unwrap();
    package::package_epub(&work, &output).unwrap();

    let validation = package::validate_epub(&output).unwrap();
    assert!(validation.is_valid(), "problems: {:?}", validation.problems);

    let mut names = entry_names(&output);
    names.sort();
    assert_eq!(
        names,
        vec![
            "META-INF/container.xml",
            "OEBPS/chapter1.xhtml",
            "OEBPS/content.opf",
            "mimetype",
        ]
    );

    // Content survives the round-trip byte for byte.
    let mut archive = ZipArchive::new(File::open(&output).unwrap()).unwrap();
    let mut chapter = Vec::new();
    std::io::Read::read_to_end(
        &mut archive.by_name("OEBPS/chapter1.xhtml").unwrap(),
        &mut chapter,
    )
    .unwrap();
    assert_eq!(chapter, common::CHAPTER_XHTML);
}

/// epubcheck requires the mimetype entry to be first, stored, and carry no
/// extra field — which puts its content at a fixed offset of 38 bytes
/// (30-byte local header + the 8-byte name). This is the assertion that
/// catches a zip backend silently adding an extended-timestamp extra field.
#[test]
fn mimetype_is_first_stored_and_unpadded() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let work = dir.path().join("work");
    let output = dir.path().join("out.epub");

    common::write_minimal_epub(&input);
    package::extract_epub(&input, &work).unwrap();
    package::package_epub(&work, &output).unwrap();

    let bytes = fs::read(&output).unwrap();
    assert_eq!(&bytes[0..4], b"PK\x03\x04", "not a local file header");
    assert_eq!(
        &bytes[30..38],
        b"mimetype",
        "mimetype is not the first entry"
    );
    assert_eq!(
        &bytes[38..38 + MIMETYPE.len()],
        MIMETYPE.as_bytes(),
        "mimetype content is not at offset 38 — an extra field crept in"
    );
}

#[test]
fn packaging_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let work = dir.path().join("work");

    common::write_minimal_epub(&input);
    package::extract_epub(&input, &work).unwrap();

    let first = dir.path().join("a.epub");
    let second = dir.path().join("b.epub");
    package::package_epub(&work, &first).unwrap();
    package::package_epub(&work, &second).unwrap();

    assert_eq!(entry_names(&first), entry_names(&second));
}

#[test]
fn os_artifacts_are_removed() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let work = dir.path().join("work");
    let output = dir.path().join("out.epub");

    common::write_epub_with_artifacts(&input);
    package::extract_epub(&input, &work).unwrap();

    let removed = package::remove_os_artifacts(&work).unwrap();
    assert_eq!(removed, 3, "expected .DS_Store, Thumbs.db and __MACOSX");

    package::package_epub(&work, &output).unwrap();
    let names = entry_names(&output);
    assert!(!names.iter().any(|n| n.contains(".DS_Store")));
    assert!(!names.iter().any(|n| n.contains("Thumbs.db")));
    assert!(!names.iter().any(|n| n.contains("__MACOSX")));
    assert!(names.iter().any(|n| n == "OEBPS/chapter1.xhtml"));
}

/// Even if `remove_os_artifacts` is never called, packaging must not carry
/// artifacts into the output.
#[test]
fn packaging_skips_artifacts_without_explicit_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let work = dir.path().join("work");
    let output = dir.path().join("out.epub");

    common::write_epub_with_artifacts(&input);
    package::extract_epub(&input, &work).unwrap();
    package::package_epub(&work, &output).unwrap();

    let names = entry_names(&output);
    assert!(!names.iter().any(|n| n.contains(".DS_Store")));
    assert!(!names.iter().any(|n| n.contains("__MACOSX")));
}

#[test]
fn zip_slip_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let malicious = dir.path().join("evil.epub");
    let work = dir.path().join("work");

    // Build the archive by hand: the fixture helper would be a fine place to
    // hide this, but the traversal payload should be visible in the test.
    let file = File::create(&malicious).unwrap();
    let mut zip = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("../../escaped.txt", stored).unwrap();
    zip.write_all(b"pwned").unwrap();
    zip.finish().unwrap();

    let result = package::extract_epub(&malicious, &work);
    assert!(result.is_err(), "traversal entry was accepted");
    assert!(!dir.path().join("escaped.txt").exists());
    assert!(!dir.path().parent().unwrap().join("escaped.txt").exists());
}

#[test]
fn finds_opf_via_container() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let work = dir.path().join("work");

    common::write_minimal_epub(&input);
    package::extract_epub(&input, &work).unwrap();

    assert_eq!(package::find_opf_path(&work).unwrap(), "OEBPS/content.opf");
}

#[test]
fn finds_opf_by_search_when_container_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    fs::create_dir_all(work.join("OEBPS")).unwrap();
    fs::write(work.join("OEBPS/content.opf"), common::CONTENT_OPF).unwrap();

    assert_eq!(package::find_opf_path(&work).unwrap(), "OEBPS/content.opf");
}

#[test]
fn validation_reports_a_compressed_mimetype() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.epub");

    // Every entry deflated, including mimetype.
    let file = File::create(&bad).unwrap();
    let mut zip = ZipWriter::new(file);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("mimetype", deflated).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", deflated).unwrap();
    zip.write_all(common::CONTAINER_XML).unwrap();
    zip.finish().unwrap();

    let validation = package::validate_epub(&bad).unwrap();
    assert!(!validation.is_valid());
    assert!(
        validation.problems.iter().any(|p| p.contains("compressed")),
        "problems: {:?}",
        validation.problems
    );
}

#[test]
fn no_encryption_means_no_drm() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    common::write_minimal_epub(&input);

    assert!(!package::has_drm(&input).unwrap());
}

#[test]
fn font_obfuscation_is_not_drm() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");

    let encryption =
        common::encryption_xml("http://www.idpf.org/2008/embedding", "OEBPS/fonts/body.otf");
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("META-INF/encryption.xml", &encryption),
            ("OEBPS/content.opf", common::CONTENT_OPF),
        ],
    );

    assert!(
        !package::has_drm(&input).unwrap(),
        "font obfuscation was misreported as DRM"
    );
}

#[test]
fn encrypted_content_is_drm() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");

    // Same obfuscation algorithm marker, but a content document is encrypted.
    let encryption =
        common::encryption_xml("http://www.idpf.org/2008/embedding", "OEBPS/chapter1.xhtml");
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("META-INF/encryption.xml", &encryption),
            ("OEBPS/content.opf", common::CONTENT_OPF),
        ],
    );

    assert!(package::has_drm(&input).unwrap());
}

#[test]
fn unrecognized_encryption_is_drm() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");

    // XML Encryption with no obfuscation marker at all.
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

    assert!(package::has_drm(&input).unwrap());
}
