//! Shared fixture helpers. Synthesizes EPUBs rather than checking binaries
//! into the repo, so every structural property under test is explicit here.

#![allow(dead_code)] // compiled into each test binary; each uses a subset

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

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

/// A PNG gradient, for exercising the image pipeline.
pub fn png_gradient(width: u32, height: u32) -> Vec<u8> {
    let mut rgb = image::RgbImage::new(width, height);
    for (x, y, pixel) in rgb.enumerate_pixels_mut() {
        *pixel = image::Rgb([
            ((x * 255) / width.max(1)) as u8,
            ((y * 255) / height.max(1)) as u8,
            128,
        ]);
    }

    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(rgb)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .expect("encode fixture png");
    out
}

/// A PNG whose header claims an image `width` x `height` of `bit_depth`
/// bits a channel in PNG colour type `color_type`, 6 for RGBA say, with
/// almost nothing in it: what reckoning it from its header sees, not what
/// decoding it finds.
#[allow(dead_code)]
pub fn png_claiming(width: u32, height: u32, bit_depth: u8, color_type: u8) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = png.len();
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let crc = crc32(&png[start..]);
        png.extend_from_slice(&crc.to_be_bytes());
    }

    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[bit_depth, color_type, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &header);
    chunk(
        &mut png,
        b"IDAT",
        &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
    );
    chunk(&mut png, b"IEND", &[]);
    png
}

/// Run `work`, failing if it takes longer than `limit`: a check on input that
/// once took time growing with its square. Work that is too slow goes on in
/// the background, so a regression fails the test rather than hanging it.
pub fn finishes_within<T: Send + 'static>(
    limit: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || sender.send(work()).ok());
    match receiver.recv_timeout(limit) {
        Ok(done) => done,
        Err(RecvTimeoutError::Timeout) => panic!("took longer than {limit:?}"),
        Err(RecvTimeoutError::Disconnected) => panic!("the work panicked"),
    }
}
