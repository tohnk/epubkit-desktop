//! Shared fixture helpers. Synthesizes EPUBs rather than checking binaries
//! into the repo, so every structural property under test is explicit here.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

pub const CONTAINER_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;

pub const CONTENT_OPF: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:test</dc:identifier>
    <dc:title>Test Book</dc:title>
    <dc:creator>A Writer</dc:creator>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
  </spine>
</package>
"#;

pub const CHAPTER_XHTML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Chapter 1</title></head>
  <body><p>Well-formed text.</p></body>
</html>
"#;

/// Write a zip whose first entry is stored and whose remaining entries are
/// deflated — i.e. the layout a valid EPUB has.
pub fn write_epub(path: &Path, entries: &[(&str, &[u8])]) {
    let file = File::create(path).expect("create fixture");
    let mut zip = ZipWriter::new(file);

    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    for (i, (name, bytes)) in entries.iter().enumerate() {
        let options = if i == 0 { stored } else { deflated };
        zip.start_file(*name, options).expect("start entry");
        zip.write_all(bytes).expect("write entry");
    }

    zip.finish().expect("finish fixture");
}

/// A minimal, structurally valid EPUB.
pub fn write_minimal_epub(path: &Path) {
    write_epub(
        path,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER_XML),
            ("OEBPS/content.opf", CONTENT_OPF),
            ("OEBPS/chapter1.xhtml", CHAPTER_XHTML),
        ],
    );
}

/// The same book, plus the debris a macOS or Windows round-trip leaves behind.
pub fn write_epub_with_artifacts(path: &Path) {
    write_epub(
        path,
        &[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER_XML),
            ("OEBPS/content.opf", CONTENT_OPF),
            ("OEBPS/chapter1.xhtml", CHAPTER_XHTML),
            ("OEBPS/.DS_Store", b"\x00\x01junk"),
            ("Thumbs.db", b"junk"),
            ("__MACOSX/._chapter1.xhtml", b"junk"),
        ],
    );
}

/// Build an `encryption.xml` declaring `uri` as encrypted, using `algorithm`
/// as the encryption method.
pub fn encryption_xml(algorithm: &str, uri: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <EncryptedData xmlns="http://www.w3.org/2001/04/xmlenc#">
    <EncryptionMethod Algorithm="{algorithm}"/>
    <CipherData><CipherReference URI="{uri}"/></CipherData>
  </EncryptedData>
</encryption>
"#
    )
    .into_bytes()
}
