//! Image conversion for e-ink panels: resize, grayscale, contrast, 4-level
//! quantization with error diffusion, and baseline JPEG output. Port of
//! `image_processor.py`.
//!
//! # Reproducing Pillow
//!
//! The reference implementation leans on several Pillow operations whose exact
//! behaviour is not obvious from their names. Where the choice is a decision
//! rather than an accident, it is reproduced exactly and pinned by a test
//! against Pillow's own output:
//!
//! - **Grayscale** uses ITU-R BT.601 luma in Pillow's fixed-point form, not the
//!   Rec. 709 coefficients most Rust imaging crates default to. The two differ
//!   by 10 grey levels on average and 33 at worst — enough to move a pixel
//!   across a quantization threshold on a 4-level panel.
//! - **Contrast** blends against a solid image filled with the source's own
//!   mean luma, not against mid-grey. The obvious `(v - 128) * f + 128` is a
//!   different operation on any image that is not mid-grey on average.
//! - **Autocontrast** clips a percentage off each end of the histogram by a
//!   specific procedure, then rescales between the surviving endpoints.
//!
//! Two things are deliberately *not* bit-exact, because chasing them would buy
//! nothing visible: Lanczos resampling (same algorithm, `f32` coefficients
//! rather than Pillow's fixed-point) and error diffusion (classic
//! Floyd–Steinberg on the grey channel, where Pillow diffuses against a palette
//! in RGB). Both land within a level or so per pixel, inside noise the dither
//! introduces by design.

use std::io::Cursor;
use std::path::Path;

use image::imageops::FilterType;
use image::{DynamicImage, GrayImage, ImageReader, Luma, Rgb, RgbImage};
use jpeg_encoder::{ColorType, Encoder as JpegEncoder, SamplingFactor};

use crate::{Error, Result};

/// The SSD1677's four grey levels: black, dark grey, light grey, white.
pub const SSD1677_LEVELS: &[u8] = &[0, 85, 170, 255];

/// Hard ceiling from the Xteink JPEG spec, applied before the device box.
pub const MAX_IMAGE_DIMENSION: u32 = 1024;

pub const DEFAULT_DEVICE: &str = "x4";

/// Extensions the image step will attempt.
const SUPPORTED_EXTENSIONS: &[&str] = &["png", "gif", "webp", "bmp", "jpeg", "jpg", "tif", "tiff"];

/// A reader's panel, in display orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceProfile {
    pub id: &'static str,
    pub label: &'static str,
    pub width: u32,
    pub height: u32,
    pub gray_levels: &'static [u8],
}

pub const X4: DeviceProfile = DeviceProfile {
    id: "x4",
    label: "Xteink X4",
    width: 480,
    height: 800,
    gray_levels: SSD1677_LEVELS,
};

pub const X3: DeviceProfile = DeviceProfile {
    id: "x3",
    label: "Xteink X3",
    width: 528,
    height: 792,
    gray_levels: SSD1677_LEVELS,
};

pub const DEVICES: &[DeviceProfile] = &[X4, X3];

pub fn device(id: &str) -> Option<DeviceProfile> {
    DEVICES.iter().copied().find(|d| d.id == id)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageOptions {
    pub max_width: u32,
    pub max_height: u32,
    pub gray_levels: Vec<u8>,
    pub grayscale: bool,
    pub contrast_boost: bool,
    /// Higher than a photo editor's default, for a low-bit-depth display.
    pub contrast_factor: f32,
    pub eink_quantize: bool,
    pub quality: u8,
    pub light_novel_mode: bool,
    pub light_novel_rotate_left: bool,
}

impl Default for ImageOptions {
    fn default() -> Self {
        Self::for_device(X4)
    }
}

impl ImageOptions {
    pub fn for_device(profile: DeviceProfile) -> Self {
        Self {
            max_width: profile.width,
            max_height: profile.height,
            gray_levels: profile.gray_levels.to_vec(),
            grayscale: true,
            contrast_boost: true,
            contrast_factor: 1.5,
            eink_quantize: true,
            quality: 70,
            light_novel_mode: false,
            light_novel_rotate_left: true,
        }
    }
}

/// One output image. Light Novel mode can turn a double-page spread into two.
#[derive(Debug, Clone)]
pub struct ProcessedImage {
    pub bytes: Vec<u8>,
    pub filename: String,
    pub original_size: usize,
    pub new_size: usize,
    /// Human-readable account of what was done, for the processing report.
    pub details: String,
}

/// Is this a file the image step should try to open?
pub fn should_process(filename: &str) -> bool {
    Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| SUPPORTED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Convert one image for the device.
pub fn process_image(
    bytes: &[u8],
    filename: &str,
    options: &ImageOptions,
) -> Result<Vec<ProcessedImage>> {
    let original_size = bytes.len();
    let stem = Path::new(filename)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "image".to_string());
    let was_jpeg = matches!(
        Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("jpg") | Some("jpeg")
    );

    let decoded = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::Image(format!("{filename}: {e}")))?
        .decode()
        .map_err(|e| Error::Image(format!("{filename}: {e}")))?;

    // Alpha has to go before anything else; a transparent region would
    // otherwise quantize to whatever the undefined colour channel held.
    let flattened = flatten_onto_white(decoded);

    let pages = if options.light_novel_mode {
        split_for_vertical_reading(flattened, options.light_novel_rotate_left)
    } else {
        vec![flattened]
    };

    let page_count = pages.len();
    let mut results = Vec::with_capacity(page_count);

    for (index, page) in pages.into_iter().enumerate() {
        let mut details = Vec::new();

        if page_count > 1 {
            details.push(format!("split part {}/{page_count}", index + 1));
        }
        if !was_jpeg {
            let from = Path::new(filename)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("image")
                .to_ascii_uppercase();
            details.push(format!("{from}→JPEG"));
        }

        let (before_w, before_h) = (page.width(), page.height());

        // The spec ceiling first, then the device's own box.
        let (clamped_w, clamped_h) =
            fit_within(before_w, before_h, MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION);
        let (target_w, target_h) =
            fit_within(clamped_w, clamped_h, options.max_width, options.max_height);

        let page = if (target_w, target_h) != (before_w, before_h) {
            details.push(format!(
                "resized {before_w}x{before_h}→{target_w}x{target_h}"
            ));
            page.resize_exact(target_w, target_h, FilterType::Lanczos3)
        } else {
            page
        };

        let rgb = if options.grayscale {
            let mut gray = to_gray_601(&page);

            if options.contrast_boost {
                // Stretching the histogram first gives the quantizer a full
                // range to map onto; without it a flat scan lands on two levels.
                if options.eink_quantize {
                    autocontrast(&mut gray, 1);
                }
                adjust_contrast(&mut gray, options.contrast_factor);
            }

            if options.eink_quantize {
                floyd_steinberg(&mut gray, &options.gray_levels);
                details.push(match options.gray_levels.len() {
                    2 => "B/W dithered".to_string(),
                    n => format!("{n}-level grayscale"),
                });
            } else {
                details.push("grayscale".to_string());
            }

            if options.contrast_boost {
                details.push(format!("contrast {}x", options.contrast_factor));
            }

            gray_to_rgb(&gray)
        } else {
            let mut rgb = page.to_rgb8();
            if options.contrast_boost {
                adjust_contrast_rgb(&mut rgb, options.contrast_factor);
                details.push(format!("contrast {}x", options.contrast_factor));
            }
            rgb
        };

        let encoded = encode_baseline_jpeg(&rgb, options.quality, options.grayscale)?;

        results.push(ProcessedImage {
            filename: if page_count > 1 {
                format!("{stem}_part{}.jpg", index + 1)
            } else {
                format!("{stem}.jpg")
            },
            // Only the first output carries the source's size, so a split
            // spread does not count its input twice.
            original_size: if index == 0 { original_size } else { 0 },
            new_size: encoded.len(),
            details: if details.is_empty() {
                "baseline JPEG".to_string()
            } else {
                details.join(", ")
            },
            bytes: encoded,
        });
    }

    Ok(results)
}

// ------------------------------------------------------- pixel operations

/// ITU-R BT.601 luma in the fixed-point form Pillow's `convert("L")` uses.
///
/// The constants are 0.299, 0.587 and 0.114 scaled by 2^16, with a rounding
/// bias before the shift. Verified against Pillow across 160,608 samples.
#[inline]
pub fn luma_601(r: u8, g: u8, b: u8) -> u8 {
    let l = r as u32 * 19595 + g as u32 * 38470 + b as u32 * 7471 + 32768;
    (l >> 16) as u8
}

/// Convert to grey using BT.601, matching the reference implementation rather
/// than the Rec. 709 coefficients this crate's `to_luma8` would apply.
pub fn to_gray_601(img: &DynamicImage) -> GrayImage {
    let rgb = img.to_rgb8();
    let mut gray = GrayImage::new(rgb.width(), rgb.height());

    for (target, source) in gray.pixels_mut().zip(rgb.pixels()) {
        *target = Luma([luma_601(source[0], source[1], source[2])]);
    }

    gray
}

/// Stretch the histogram so the darkest surviving pixel becomes black and the
/// lightest becomes white, after discarding `cutoff` percent from each end.
///
/// Reproduces `PIL.ImageOps.autocontrast`, including its integer clipping walk.
pub fn autocontrast(gray: &mut GrayImage, cutoff: u32) {
    let mut histogram = [0u64; 256];
    for pixel in gray.pixels() {
        histogram[pixel[0] as usize] += 1;
    }

    let total: u64 = histogram.iter().sum();
    if total == 0 {
        return;
    }

    if cutoff > 0 {
        let mut cut = total * cutoff as u64 / 100;
        for bin in histogram.iter_mut() {
            if cut == 0 {
                break;
            }
            if cut > *bin {
                cut -= *bin;
                *bin = 0;
            } else {
                *bin -= cut;
                cut = 0;
            }
        }

        let mut cut = total * cutoff as u64 / 100;
        for bin in histogram.iter_mut().rev() {
            if cut == 0 {
                break;
            }
            if cut > *bin {
                cut -= *bin;
                *bin = 0;
            } else {
                *bin -= cut;
                cut = 0;
            }
        }
    }

    let low = histogram.iter().position(|&count| count > 0);
    let high = histogram.iter().rposition(|&count| count > 0);

    let (Some(low), Some(high)) = (low, high) else {
        return;
    };
    if high <= low {
        // A single occupied bin has no range to stretch.
        return;
    }

    let scale = 255.0 / (high - low) as f64;
    let offset = -(low as f64) * scale;

    let mut lut = [0u8; 256];
    for (value, entry) in lut.iter_mut().enumerate() {
        // Pillow truncates toward zero here; negatives clamp to 0 either way.
        *entry = ((value as f64 * scale + offset) as i32).clamp(0, 255) as u8;
    }

    for pixel in gray.pixels_mut() {
        *pixel = Luma([lut[pixel[0] as usize]]);
    }
}

/// Scale contrast about the image's own mean luma.
///
/// Reproduces `PIL.ImageEnhance.Contrast`, which blends the image against a
/// solid fill of its mean rather than against mid-grey.
pub fn adjust_contrast(gray: &mut GrayImage, factor: f32) {
    let total: u64 = gray.pixels().map(|p| p[0] as u64).sum();
    let count = gray.pixels().len() as f64;
    if count == 0.0 {
        return;
    }

    let mean = mean_level(total, count);

    for pixel in gray.pixels_mut() {
        *pixel = Luma([blend_toward(pixel[0], mean, factor)]);
    }
}

/// The colour equivalent, pivoting about the mean of the image's *luma* —
/// which is what Pillow does even for an RGB image.
pub fn adjust_contrast_rgb(rgb: &mut RgbImage, factor: f32) {
    let total: u64 = rgb
        .pixels()
        .map(|p| luma_601(p[0], p[1], p[2]) as u64)
        .sum();
    let count = rgb.pixels().len() as f64;
    if count == 0.0 {
        return;
    }

    let mean = mean_level(total, count);

    for pixel in rgb.pixels_mut() {
        *pixel = Rgb([
            blend_toward(pixel[0], mean, factor),
            blend_toward(pixel[1], mean, factor),
            blend_toward(pixel[2], mean, factor),
        ]);
    }
}

/// Quantize to the device's grey levels, diffusing the error into neighbouring
/// pixels so gradients survive as texture rather than banding.
///
/// Classic Floyd–Steinberg, raster order, 7/16 right and 3/16, 5/16, 1/16 into
/// the row below.
pub fn floyd_steinberg(gray: &mut GrayImage, levels: &[u8]) {
    if levels.is_empty() {
        return;
    }

    let (width, height) = gray.dimensions();
    if width == 0 || height == 0 {
        return;
    }

    // Error is carried in f32 alongside the image so it can go out of range
    // before being folded back in.
    let mut error = vec![0f32; (width * height) as usize];

    for y in 0..height {
        for x in 0..width {
            let index = (y * width + x) as usize;
            let wanted = gray.get_pixel(x, y)[0] as f32 + error[index];
            let chosen = nearest_level(wanted, levels);
            gray.put_pixel(x, y, Luma([chosen]));

            let residual = wanted - chosen as f32;
            if residual == 0.0 {
                continue;
            }

            let mut spread = |dx: i64, dy: i64, share: f32| {
                let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                if nx < 0 || ny < 0 || nx >= width as i64 || ny >= height as i64 {
                    return;
                }
                error[(ny as u32 * width + nx as u32) as usize] += residual * share;
            };

            spread(1, 0, 7.0 / 16.0);
            spread(-1, 1, 3.0 / 16.0);
            spread(0, 1, 5.0 / 16.0);
            spread(1, 1, 1.0 / 16.0);
        }
    }
}

// ---------------------------------------------------------------- internals

fn mean_level(total: u64, count: f64) -> u8 {
    // Pillow rounds the mean half-up before building its solid fill.
    ((total as f64 / count) + 0.5) as u8
}

fn blend_toward(value: u8, mean: u8, factor: f32) -> u8 {
    let blended = mean as f32 + factor * (value as f32 - mean as f32);
    // Pillow truncates; out-of-range values clamp.
    (blended as i32).clamp(0, 255) as u8
}

fn nearest_level(value: f32, levels: &[u8]) -> u8 {
    let mut best = levels[0];
    let mut best_distance = f32::MAX;

    for &level in levels {
        let distance = (value - level as f32).abs();
        if distance < best_distance {
            best_distance = distance;
            best = level;
        }
    }

    best
}

/// Composite any transparency onto white. E-ink has no alpha, and an
/// unflattened image would quantize its transparent regions to noise.
fn flatten_onto_white(img: DynamicImage) -> DynamicImage {
    if !img.color().has_alpha() {
        return img;
    }

    let rgba = img.to_rgba8();
    let mut out = RgbImage::new(rgba.width(), rgba.height());

    for (target, source) in out.pixels_mut().zip(rgba.pixels()) {
        let alpha = source[3] as u32;
        let over = |channel: u8| {
            (((channel as u32 * alpha) + 255 * (255 - alpha) + 127) / 255).min(255) as u8
        };
        *target = Rgb([over(source[0]), over(source[1]), over(source[2])]);
    }

    DynamicImage::ImageRgb8(out)
}

/// Light Novel mode: make landscape artwork readable on a portrait panel.
///
/// A double-page spread is split rather than shrunk to illegibility. The right
/// half comes first, matching the reading order of the books this is for.
fn split_for_vertical_reading(img: DynamicImage, rotate_left: bool) -> Vec<DynamicImage> {
    let (width, height) = (img.width(), img.height());
    if width <= height {
        return vec![img];
    }

    const SPREAD_ASPECT: f32 = 1.8;
    if width as f32 / height as f32 > SPREAD_ASPECT {
        let mid = width / 2;
        return vec![
            img.crop_imm(mid, 0, width - mid, height),
            img.crop_imm(0, 0, mid, height),
        ];
    }

    // Pillow's `rotate(90)` turns counter-clockwise, which is this crate's
    // `rotate270`.
    vec![if rotate_left {
        img.rotate270()
    } else {
        img.rotate90()
    }]
}

/// Fit within a box, preserving aspect ratio and never enlarging.
fn fit_within(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width <= max_width && height <= max_height {
        return (width, height);
    }

    let scale = (max_width as f64 / width as f64).min(max_height as f64 / height as f64);
    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

fn gray_to_rgb(gray: &GrayImage) -> RgbImage {
    let mut rgb = RgbImage::new(gray.width(), gray.height());
    for (target, source) in rgb.pixels_mut().zip(gray.pixels()) {
        *target = Rgb([source[0], source[0], source[0]]);
    }
    rgb
}

/// Encode baseline JPEG. Progressive JPEG breaks many e-ink readers, so it is
/// never emitted.
///
/// Grayscale output is written as RGB with 4:2:0 subsampling, matching the
/// reference: the three channels are identical, so the halved chroma planes
/// cost nothing and save 15-20%. Encoding it as a single-component grayscale
/// JPEG would be smaller still, but that changes the file's structure and
/// wants testing on real hardware first.
///
/// Huffman tables are optimized for each image, matching the reference's
/// `optimize=True`, but by rewriting the finished file rather than by asking
/// the encoder for it: `jpeg-encoder`'s own optimization also splits the single
/// interleaved scan into three, which several decoders mishandle. See
/// [`crate::jpeg`]. The rewrite is lossless and leaves the file's structure
/// alone; if anything about it fails, the unoptimized file is kept.
fn encode_baseline_jpeg(rgb: &RgbImage, quality: u8, grayscale: bool) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new(&mut out, quality);

    encoder.set_sampling_factor(if grayscale {
        SamplingFactor::F_2_2
    } else {
        SamplingFactor::F_1_1
    });

    let width = u16::try_from(rgb.width())
        .map_err(|_| Error::Image(format!("image too wide to encode: {}", rgb.width())))?;
    let height = u16::try_from(rgb.height())
        .map_err(|_| Error::Image(format!("image too tall to encode: {}", rgb.height())))?;

    encoder
        .encode(rgb.as_raw(), width, height, ColorType::Rgb)
        .map_err(|e| Error::Image(format!("JPEG encoding failed: {e}")))?;

    Ok(crate::jpeg::optimize_huffman(&out).unwrap_or(out))
}
