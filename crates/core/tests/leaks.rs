//! What a run frees once it is done, measured as the desktop app runs: book
//! after book in one process. A test of its own, so that nothing else runs in
//! the process while its memory is read.

mod common;

use epubkit_core::pipeline::{process_epub, ProcessingOptions};

/// This process's resident memory, in KB, as `ps` reads it.
fn resident_kb() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// What a run takes out of a book's documents is freed. A node libxml2
/// unlinks it keeps until the process ends unless it is freed, so the app,
/// which runs book after book in one process, kept every empty paragraph,
/// unused manifest entry and store tag of every book it had run: a book
/// losing 60,000 empty paragraphs grew it by 23 MB each time, to 308 MB by
/// the twelfth.
#[test]
fn what_a_run_takes_out_of_a_book_is_freed() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let mut chapter = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title></head><body><p>Start.</p>"#,
    );
    for _ in 0..10_000 {
        chapter.push_str("<p class=\"gap\"> </p>\n");
    }
    chapter.push_str("<p>End.</p></body></html>\n");
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:leaks</dc:identifier><dc:title>Gaps</dc:title>
  </metadata>
  <manifest>
    <item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="c2.xhtml" media-type="application/xhtml+xml"/>
    <item id="c3" href="c3.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="c1"/><itemref idref="c2"/><itemref idref="c3"/></spine>
</package>"#;
    common::write_epub(
        &input,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", common::CONTAINER_XML),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/c1.xhtml", chapter.as_bytes()),
            ("OEBPS/c2.xhtml", chapter.as_bytes()),
            ("OEBPS/c3.xhtml", chapter.as_bytes()),
        ],
    );

    let options = ProcessingOptions::default();
    let output = dir.path().join("out.epub");
    let run = || {
        let report = process_epub(&input, &output, &options, |_, _| {}).unwrap();
        assert_eq!(report.blank_elements_removed, 3 * 9_999);
    };
    // The first run settles the allocator's pools.
    run();
    let settled = resident_kb();
    for _ in 0..5 {
        run();
    }
    let grown = resident_kb().saturating_sub(settled);
    assert!(grown < 8 * 1024, "grew by {grown} KB over five runs");
}
