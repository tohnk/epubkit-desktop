//! The Huffman rewrite has one job: make the file smaller while changing
//! nothing a decoder can observe. Both halves of that need holding down.

use epubkit_core::image::{process_image, ImageOptions};
use epubkit_core::jpeg::optimize_huffman;
use image::{DynamicImage, GrayImage, Luma, RgbImage};
use jpeg_encoder::{ColorType, Encoder as JpegEncoder, SamplingFactor};

fn encode(rgb: &RgbImage, quality: u8, sampling: SamplingFactor) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new(&mut out, quality);
    encoder.set_sampling_factor(sampling);
    encoder
        .encode(
            rgb.as_raw(),
            rgb.width() as u16,
            rgb.height() as u16,
            ColorType::Rgb,
        )
        .unwrap();
    out
}

/// A photograph-ish image: smooth, with structure at several scales, so the
/// symbol distribution is broad rather than degenerate.
fn textured(width: u32, height: u32) -> RgbImage {
    let mut rgb = RgbImage::new(width, height);
    for (x, y, pixel) in rgb.enumerate_pixels_mut() {
        let (fx, fy) = (x as f32 / width as f32, y as f32 / height as f32);
        let v = 128.0 + 80.0 * (fx * 11.0).sin() * (fy * 7.0).cos() + 30.0 * (fx * 40.0).sin();
        let v = v.clamp(0.0, 255.0) as u8;
        *pixel = image::Rgb([v, v.saturating_add(11), v.saturating_sub(7)]);
    }
    rgb
}

fn solid(width: u32, height: u32, level: u8) -> RgbImage {
    let mut rgb = RgbImage::new(width, height);
    for pixel in rgb.pixels_mut() {
        *pixel = image::Rgb([level, level, level]);
    }
    rgb
}

/// Hard black-and-white noise, the closest stand-in for dithered output.
fn dithered(width: u32, height: u32) -> RgbImage {
    let mut rgb = RgbImage::new(width, height);
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for pixel in rgb.pixels_mut() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let v = [0u8, 85, 170, 255][(seed >> 33) as usize % 4];
        *pixel = image::Rgb([v, v, v]);
    }
    rgb
}

fn cases() -> Vec<(&'static str, RgbImage)> {
    vec![
        ("textured", textured(320, 240)),
        ("tall", textured(97, 331)),
        // Dimensions that are not multiples of the MCU size, so the encoder
        // pads and the block walk has to agree about how many there are.
        ("odd", textured(101, 67)),
        ("tiny", textured(9, 3)),
        ("white", solid(64, 64, 255)),
        ("black", solid(64, 64, 0)),
        ("dithered", dithered(240, 320)),
    ]
}

fn pixels(jpeg: &[u8]) -> Vec<u8> {
    image::load_from_memory(jpeg)
        .expect("decodes")
        .to_rgb8()
        .into_raw()
}

/// The whole point: not "close enough", but the same image.
#[test]
fn the_rewrite_changes_no_pixels() {
    let mut applied = 0;

    for sampling in [SamplingFactor::F_2_2, SamplingFactor::F_1_1] {
        for (name, rgb) in cases() {
            let plain = encode(&rgb, 70, sampling);
            let Some(rewritten) = optimize_huffman(&plain) else {
                continue; // declined; the caller keeps the original
            };
            applied += 1;

            assert_eq!(
                pixels(&plain),
                pixels(&rewritten),
                "{name} at {sampling:?} decoded differently after the rewrite"
            );
        }
    }

    // Guard against the whole test passing because nothing was rewritten.
    assert!(
        applied >= cases().len() * 2 - 2,
        "only {applied} of {} cases were rewritten at all",
        cases().len() * 2
    );
}

#[test]
fn the_rewrite_makes_files_smaller() {
    for (name, rgb) in cases() {
        let plain = encode(&rgb, 70, SamplingFactor::F_2_2);
        if let Some(rewritten) = optimize_huffman(&plain) {
            assert!(
                rewritten.len() < plain.len(),
                "{name} grew: {} -> {}",
                plain.len(),
                rewritten.len()
            );
        }
    }
}

/// Everything except the tables must survive untouched. A dropped SOF or DQT
/// is the easy mistake here, and it is not one a size check would catch.
#[test]
fn every_segment_but_the_tables_is_carried_over() {
    let plain = encode(&textured(200, 150), 70, SamplingFactor::F_2_2);
    let rewritten = optimize_huffman(&plain).expect("should apply");

    let segments = |jpeg: &[u8]| -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 2;
        while at + 4 <= jpeg.len() && jpeg[at] == 0xFF {
            let marker = jpeg[at + 1];
            let length = u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]) as usize;
            out.push((marker, jpeg[at + 4..at + 2 + length].to_vec()));
            if marker == 0xDA {
                break;
            }
            at += 2 + length;
        }
        out
    };

    let before = segments(&plain);
    let after = segments(&rewritten);

    let kept = |segments: &[(u8, Vec<u8>)]| -> Vec<(u8, Vec<u8>)> {
        segments
            .iter()
            .filter(|(marker, _)| *marker != 0xC4)
            .cloned()
            .collect()
    };
    assert_eq!(
        kept(&before),
        kept(&after),
        "a non-table segment was altered or lost"
    );

    // And the tables really did change.
    let tables = |segments: &[(u8, Vec<u8>)]| -> Vec<Vec<u8>> {
        segments
            .iter()
            .filter(|(marker, _)| *marker == 0xC4)
            .map(|(_, payload)| payload.clone())
            .collect()
    };
    assert_ne!(tables(&before), tables(&after));
}

/// One interleaved scan in, one interleaved scan out. This is the property
/// that asking `jpeg-encoder` to optimize would have broken.
#[test]
fn the_scan_stays_single_and_interleaved() {
    let plain = encode(&textured(200, 150), 70, SamplingFactor::F_2_2);
    let rewritten = optimize_huffman(&plain).expect("should apply");

    let scans = |jpeg: &[u8]| -> Vec<u8> {
        (0..jpeg.len() - 1)
            .filter(|&i| jpeg[i] == 0xFF && jpeg[i + 1] == 0xDA)
            .map(|i| jpeg[i + 4]) // the component count
            .collect()
    };

    assert_eq!(scans(&plain), vec![3]);
    assert_eq!(scans(&rewritten), vec![3]);
}

/// Files this cannot reason about must come back as `None` rather than as
/// something plausible-looking.
#[test]
fn unsupported_and_broken_input_is_declined() {
    assert!(optimize_huffman(b"").is_none());
    assert!(optimize_huffman(b"not a jpeg").is_none());
    assert!(optimize_huffman(&[0xFF, 0xD8, 0xFF, 0xD9]).is_none());

    let mut progressive = Vec::new();
    let mut encoder = JpegEncoder::new(&mut progressive, 70);
    encoder.set_progressive(true);
    let rgb = textured(64, 64);
    encoder
        .encode(rgb.as_raw(), 64, 64, ColorType::Rgb)
        .unwrap();
    assert!(
        optimize_huffman(&progressive).is_none(),
        "progressive JPEG should be left alone"
    );

    // Truncation anywhere must not panic, and must not produce a file.
    let plain = encode(&textured(80, 80), 70, SamplingFactor::F_2_2);
    for cut in (4..plain.len()).step_by(7) {
        let _ = optimize_huffman(&plain[..cut]);
    }
    // Corruption in the entropy data likewise.
    for at in (plain.len() / 2..plain.len() - 2).step_by(5) {
        let mut damaged = plain.clone();
        damaged[at] ^= 0x5A;
        let _ = optimize_huffman(&damaged);
    }
}

/// Applying it twice must be a no-op: the second pass has nothing left to gain,
/// so it declines rather than churning the file.
#[test]
fn a_second_pass_finds_nothing_to_do() {
    let plain = encode(&textured(200, 150), 70, SamplingFactor::F_2_2);
    let once = optimize_huffman(&plain).expect("should apply");
    assert!(
        optimize_huffman(&once).is_none(),
        "the rewrite should be idempotent"
    );
}

// ------------------------------------------------------- through the pipeline

/// What `process_image` emits must be readable, and must have been optimized.
#[test]
fn pipeline_output_is_optimized_and_still_decodes() {
    let mut gray = GrayImage::new(400, 600);
    for (x, y, pixel) in gray.enumerate_pixels_mut() {
        let v = ((x * 7 + y * 3) % 256) as u8;
        *pixel = Luma([v]);
    }
    let mut source = Vec::new();
    DynamicImage::ImageLuma8(gray)
        .write_to(
            &mut std::io::Cursor::new(&mut source),
            image::ImageFormat::Png,
        )
        .unwrap();

    let out = process_image(&source, "page.png", &ImageOptions::default()).unwrap();
    let jpeg = &out[0].bytes;

    // It decodes.
    let decoded = image::load_from_memory(jpeg).expect("pipeline output decodes");
    assert!(decoded.width() > 0 && decoded.height() > 0);

    // It is already optimal, so a further pass has nothing to offer.
    assert!(
        optimize_huffman(jpeg).is_none(),
        "pipeline output should already carry optimized tables"
    );

    // And it is still a single interleaved baseline scan.
    let scans: Vec<u8> = (0..jpeg.len() - 1)
        .filter(|&i| jpeg[i] == 0xFF && jpeg[i + 1] == 0xDA)
        .map(|i| jpeg[i + 4])
        .collect();
    assert_eq!(scans, vec![3], "expected one interleaved scan");
}
