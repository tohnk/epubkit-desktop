//! The CLI binary serves as its own decode worker. These drive a real worker
//! process over the pipe the same way the pipeline does, so the worker's
//! spawning, the protocol, and the memory cap are all exercised end to end.

use std::path::Path;

use epubkit_core::image::ImageOptions;
use epubkit_core::worker::{self, Converted};
use image::ImageEncoder;

/// Point the worker machinery at the compiled CLI, once for the test binary.
fn use_the_cli_as_worker() {
    worker::enable(env!("CARGO_BIN_EXE_epubkit").into());
}

/// A small PNG written to a file for a worker to read.
fn png_file(dir: &Path) -> std::path::PathBuf {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(
            &[90, 120, 150].repeat(64 * 64),
            64,
            64,
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    let path = dir.join("plate.png");
    std::fs::write(&path, bytes).unwrap();
    path
}

/// With room to spare, a worker reads the file and hands back a JPEG.
#[test]
fn a_worker_converts_an_image_in_its_own_process() {
    use_the_cli_as_worker();
    let dir = tempfile::tempdir().unwrap();
    let path = png_file(dir.path());
    let Converted::Done(outputs) =
        worker::convert(&path, "plate.png", &ImageOptions::default(), 256 << 20).unwrap()
    else {
        panic!("the worker did not convert the image");
    };
    assert_eq!(outputs.len(), 1);
    assert!(
        image::load_from_memory(&outputs[0].bytes).is_ok(),
        "the worker's output is not an image"
    );
}

/// Held to a byte, the worker is stopped the moment it tries to decode, and
/// the image comes back as having overrun — the caller is not brought down
/// with it.
#[test]
fn a_worker_that_overruns_its_cap_is_stopped_not_the_caller() {
    use_the_cli_as_worker();
    let dir = tempfile::tempdir().unwrap();
    let path = png_file(dir.path());
    assert!(
        matches!(
            worker::convert(&path, "plate.png", &ImageOptions::default(), 1).unwrap(),
            Converted::Overran
        ),
        "a one-byte cap did not stop the worker"
    );
    // The thread's worker was replaced; the next image converts as usual.
    assert!(matches!(
        worker::convert(&path, "plate.png", &ImageOptions::default(), 256 << 20).unwrap(),
        Converted::Done(_)
    ));
}

/// Bytes that are no image are reported undecodable, gracefully, and the
/// worker lives on to take the next request.
#[test]
fn a_worker_reports_what_it_cannot_decode() {
    use_the_cli_as_worker();
    let dir = tempfile::tempdir().unwrap();
    let garbage = dir.path().join("x.png");
    std::fs::write(&garbage, b"not an image at all").unwrap();
    assert!(matches!(
        worker::convert(&garbage, "x.png", &ImageOptions::default(), 256 << 20).unwrap(),
        Converted::Undecodable
    ));
    let path = png_file(dir.path());
    assert!(matches!(
        worker::convert(&path, "plate.png", &ImageOptions::default(), 256 << 20).unwrap(),
        Converted::Done(_)
    ));
}

/// A file that cannot be read fails, so the book fails as it did when the
/// parent read the file; the worker stays alive for the next request.
#[test]
fn a_file_that_cannot_be_read_is_an_error() {
    use_the_cli_as_worker();
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("gone.png");
    assert!(
        worker::convert(&missing, "gone.png", &ImageOptions::default(), 256 << 20).is_err(),
        "a missing file should be an error, not a skipped image"
    );
    let path = png_file(dir.path());
    assert!(matches!(
        worker::convert(&path, "plate.png", &ImageOptions::default(), 256 << 20).unwrap(),
        Converted::Done(_)
    ));
}

/// An uncompressed big-endian RGB TIFF, sixteen bits a sample, in one strip.
fn tiff_be_rgb16(width: u32, height: u32) -> Vec<u8> {
    let short = |v: u16| {
        let b = v.to_be_bytes();
        [b[0], b[1], 0, 0]
    };
    let long = |v: u32| v.to_be_bytes();
    let bits_at = 8 + 2 + 10 * 12 + 4;
    let data_at = bits_at + 6;
    let entries: [(u16, u16, u32, [u8; 4]); 10] = [
        (256, 4, 1, long(width)),
        (257, 4, 1, long(height)),
        (258, 3, 3, long(bits_at)),
        (259, 3, 1, short(1)),
        (262, 3, 1, short(2)),
        (273, 4, 1, long(data_at)),
        (277, 3, 1, short(3)),
        (278, 4, 1, long(height)),
        (279, 4, 1, long(width * height * 6)),
        (284, 3, 1, short(1)),
    ];
    let mut tiff = b"MM".to_vec();
    tiff.extend_from_slice(&42u16.to_be_bytes());
    tiff.extend_from_slice(&8u32.to_be_bytes());
    tiff.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    for (tag, kind, count, value) in entries {
        tiff.extend_from_slice(&tag.to_be_bytes());
        tiff.extend_from_slice(&kind.to_be_bytes());
        tiff.extend_from_slice(&count.to_be_bytes());
        tiff.extend_from_slice(&value);
    }
    tiff.extend_from_slice(&0u32.to_be_bytes());
    for _ in 0..3 {
        tiff.extend_from_slice(&16u16.to_be_bytes());
    }
    tiff.resize(tiff.len() + (width * height * 6) as usize, 0x55);
    tiff
}

/// Deep TIFFs convert in a worker held to just what the pipeline reckons for
/// them, the cap it uses in production: a big-endian sixteen-bit one, whose
/// depth was misread as eight bits, and floating-point RGB, reckoned at
/// sixteen bits a sample, each overran that cap and was left unconverted.
#[test]
fn a_deep_tiff_converts_within_the_memory_reckoned_for_it() {
    use std::io::Cursor;
    use_the_cli_as_worker();
    let dir = tempfile::tempdir().unwrap();
    let options = ImageOptions::default();
    let mut float = Vec::new();
    image::DynamicImage::ImageRgb32F(image::Rgb32FImage::from_pixel(
        2000,
        1500,
        image::Rgb([0.3, 0.5, 0.7]),
    ))
    .write_to(&mut Cursor::new(&mut float), image::ImageFormat::Tiff)
    .unwrap();
    for (name, bytes) in [
        ("big-endian.tiff", tiff_be_rgb16(4000, 3000)),
        ("float.tiff", float),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, &bytes).unwrap();
        let cap = epubkit_core::image::memory_needed(&path, &options).unwrap();
        let converted = worker::convert(&path, name, &options, cap).unwrap();
        assert!(
            matches!(converted, Converted::Done(_)),
            "{name} did not convert within the {cap} bytes reckoned for it"
        );
    }
}

/// A readable file larger than the cap is an overrun, not a file that cannot
/// be read: reading it allocates fallibly, so the cap's refusal comes back as
/// an error rather than stopping the worker, and must not fail the book.
#[test]
fn a_file_larger_than_the_cap_is_an_overrun_not_an_error() {
    use_the_cli_as_worker();
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = std::fs::read(png_file(dir.path())).unwrap();
    bytes.resize(4 << 20, 0);
    let path = dir.path().join("padded.png");
    std::fs::write(&path, bytes).unwrap();

    let converted = worker::convert(&path, "padded.png", &ImageOptions::default(), 1 << 20);
    assert!(
        matches!(converted, Ok(Converted::Overran)),
        "a refused read came back as {:?}",
        converted
            .as_ref()
            .map(|c| matches!(c, Converted::Overran))
            .map_err(|e| e.to_string())
    );
    // The worker lives on, or is replaced, for the next request.
    let path = png_file(dir.path());
    assert!(matches!(
        worker::convert(&path, "plate.png", &ImageOptions::default(), 256 << 20).unwrap(),
        Converted::Done(_)
    ));
}

/// A sequential JPEG with each component in a scan of its own, which the
/// JPEG decoder decodes wrongly however its components are sampled, is
/// refused in a worker as it is in this process: reported undecodable, so
/// the book leaves it as it is, and the worker goes on to the next image.
#[test]
fn a_worker_refuses_a_jpeg_in_separate_scans() {
    use_the_cli_as_worker();
    let dir = tempfile::tempdir().unwrap();
    let options = ImageOptions::default();
    let rgb = image::RgbImage::from_fn(96, 64, |x, y| {
        if (16..80).contains(&x) && (12..44).contains(&y) {
            image::Rgb([(x * 3) as u8, 90, (y * 5) as u8])
        } else {
            image::Rgb([255, 255, 255])
        }
    });
    let mut jpeg = Vec::new();
    // Optimized tables make `jpeg-encoder` write each component in a scan of
    // its own.
    let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, 90);
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
    encoder.set_optimized_huffman_tables(true);
    encoder
        .encode(rgb.as_raw(), 96, 64, jpeg_encoder::ColorType::Rgb)
        .unwrap();
    let path = dir.path().join("separate.jpg");
    std::fs::write(&path, &jpeg).unwrap();

    assert!(matches!(
        worker::convert(&path, "separate.jpg", &options, 256 << 20).unwrap(),
        Converted::Undecodable
    ));
    let path = png_file(dir.path());
    assert!(matches!(
        worker::convert(&path, "plate.png", &options, 256 << 20).unwrap(),
        Converted::Done(_)
    ));
}

/// A page of colour noise at 1024 x 1024, the most a page may be, converts in
/// a worker held to just what is reckoned for it. Making its Huffman tables
/// optimal held ten times its JPEG, which overran that cap at quality 70 as
/// at 100, and the image was left unconverted.
#[test]
fn a_page_of_colour_noise_converts_within_the_memory_reckoned_for_it() {
    use std::io::Cursor;
    use_the_cli_as_worker();
    let dir = tempfile::tempdir().unwrap();
    // Each pixel one of the eight corners of the RGB cube, at random.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let noise = image::RgbImage::from_fn(1024, 1024, |_, _| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let corner = state >> 40;
        image::Rgb([0, 1, 2].map(|bit| if corner >> bit & 1 == 0 { 0 } else { 255 }))
    });
    let mut bmp = Vec::new();
    image::DynamicImage::ImageRgb8(noise)
        .write_to(&mut Cursor::new(&mut bmp), image::ImageFormat::Bmp)
        .unwrap();
    let path = dir.path().join("noise.bmp");
    std::fs::write(&path, &bmp).unwrap();

    for quality in [70, 100] {
        let options = ImageOptions {
            max_width: 1024,
            max_height: 1024,
            grayscale: false,
            eink_quantize: false,
            contrast_boost: false,
            quality,
            ..ImageOptions::default()
        };
        let cap = epubkit_core::image::memory_needed(&path, &options).unwrap();
        let converted = worker::convert(&path, "noise.bmp", &options, cap).unwrap();
        assert!(
            matches!(converted, Converted::Done(_)),
            "quality {quality}: did not convert within the {cap} bytes reckoned for it"
        );
    }
}

/// A book's manifest calls these images, but none of them will decode: a file
/// that begins like a JPEG and is not one, a PNG whose header claims more
/// pixels than any image may have, and text named as a PNG.
fn images_that_will_not_decode() -> Vec<(&'static str, Vec<u8>)> {
    let mut not_a_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
    not_a_jpeg.extend((0..4000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8));
    // A PNG's signature and header claiming 20,000 x 20,000, and nothing more.
    let mut too_many_pixels = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    too_many_pixels.extend_from_slice(&20_000u32.to_be_bytes());
    too_many_pixels.extend_from_slice(&20_000u32.to_be_bytes());
    too_many_pixels.extend_from_slice(&[8, 2, 0, 0, 0]);
    let crc = crc32(&too_many_pixels[12..]);
    too_many_pixels.extend_from_slice(&crc.to_be_bytes());
    vec![
        ("not-a-jpeg.jpg", not_a_jpeg),
        ("too-many-pixels.png", too_many_pixels),
        ("text.png", b"not an image".to_vec()),
    ]
}

/// The CRC a PNG chunk ends with.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// An EPUB whose one chapter shows `images`, each in its manifest as the
/// image its name says it is.
fn book_with_images(path: &Path, images: &[(&str, Vec<u8>)]) {
    use std::io::Write;
    let manifest: String = images
        .iter()
        .enumerate()
        .map(|(index, (name, _))| {
            let media_type = if name.ends_with(".png") {
                "image/png"
            } else {
                "image/jpeg"
            };
            format!(r#"<item id="img{index}" href="{name}" media-type="{media_type}"/>"#)
        })
        .collect();
    let body: String = images
        .iter()
        .map(|(name, _)| format!(r#"<p><img src="{name}" alt=""/></p>"#))
        .collect();
    let opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:broken-images</dc:identifier>
    <dc:title>Broken images</dc:title>
  </metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
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
    let container = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut entries: Vec<(String, Vec<u8>)> = vec![
        ("mimetype".into(), b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml".into(),
            container.as_bytes().to_vec(),
        ),
        ("OEBPS/content.opf".into(), opf.into_bytes()),
        ("OEBPS/chapter1.xhtml".into(), chapter.into_bytes()),
    ];
    for (name, bytes) in images {
        entries.push((format!("OEBPS/{name}"), bytes.clone()));
    }
    for (name, bytes) in entries {
        zip.start_file(name, stored).unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap();
}

/// An image that will not decode is reported so, where workers are on. Nothing
/// could be reckoned for it, so the worker was held to its file alone, and was
/// stopped before it could say so: each was reported as not decoding within
/// the 0 MB set aside for it.
#[test]
fn an_image_that_will_not_decode_is_reported_so_not_as_an_overrun() {
    use_the_cli_as_worker();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.epub");
    let output = dir.path().join("out.epub");
    let images = images_that_will_not_decode();
    book_with_images(&input, &images);

    let options = epubkit_core::pipeline::ProcessingOptions::default();
    let report =
        epubkit_core::pipeline::process_epub(&input, &output, &options, |_, _| {}).unwrap();
    assert_eq!(report.images_unconverted, images.len());
    for (name, _) in &images {
        let said = format!("{name}: skipped (could not be decoded)");
        assert!(
            report.image_details.contains(&said),
            "{name}: {:?}",
            report.image_details
        );
    }
}

/// A cover that will not decode is refused as it is, rather than by the
/// worker asked to make its thumbnail being stopped, under a cap of nothing.
#[test]
fn a_cover_that_will_not_decode_is_refused_without_stopping_a_worker() {
    use_the_cli_as_worker();
    let budget = epubkit_core::memory::MemoryBudget::new(1 << 30);
    for (name, bytes) in images_that_will_not_decode() {
        let refused = worker::thumbnail(&bytes, 120, 160, &budget)
            .err()
            .map(|error| error.to_string());
        assert_eq!(
            refused.as_deref(),
            Some("image error: the image could not be decoded"),
            "{name}"
        );
    }
}
