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

use std::io::{BufRead, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use image::error::{ImageError, LimitError, LimitErrorKind};
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{
    DynamicImage, GrayImage, ImageDecoder, ImageFormat, ImageReader, Limits, Luma, Rgb, RgbImage,
};
use jpeg_encoder::{ColorType, Encoder as JpegEncoder, SamplingFactor};

use crate::memory::MemoryBudget;
use crate::{Error, Result};

/// The SSD1677's four grey levels: black, dark grey, light grey, white.
pub const SSD1677_LEVELS: &[u8] = &[0, 85, 170, 255];

/// Hard ceiling from the Xteink JPEG spec, applied before the device box.
pub const MAX_IMAGE_DIMENSION: u32 = 1024;

/// The most pixels an image may have to be converted: Pillow's limit, past
/// which it takes an image for a decompression bomb.
pub const MAX_PIXELS: u64 = 178_956_970;

/// What decoding one image may allocate: room for an image of [`MAX_PIXELS`]
/// at sixteen bits a channel with alpha, and for the decoder's own buffers.
const MAX_DECODE_BYTES: u64 = MAX_PIXELS * 8 + 128 * 1024 * 1024;

pub const DEFAULT_DEVICE: &str = "x4";

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
    /// How the format changed, which the report counts images by:
    /// `PNG→JPEG`, say, or `baseline JPEG` for a JPEG written again.
    pub conversion: String,
    /// Light Novel mode rotated or split the image, so its proportions no
    /// longer match the source's, nor any size a document gives for it.
    pub reshaped: bool,
}

/// `bytes`, an image, as a JPEG to show it small: in colour, turned as its
/// EXIF data says, and shrunk to fit within `max_width` x `max_height`. What
/// decoding it takes is held from `budget` until the thumbnail is made.
///
/// Shrunk by averaging blocks of pixels, not resampled: at the size a
/// thumbnail is shown, the two look alike, and averaging takes a third of the
/// time.
pub fn thumbnail(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    budget: &MemoryBudget,
) -> Result<Vec<u8>> {
    let failed = |e: ImageError| Error::Image(e.to_string());
    let needed = Footprint::of(
        ImageReader::new(Cursor::new(bytes)),
        &mut Cursor::new(bytes),
    )
    .map_or(0, |footprint| footprint.thumbnail());
    if needed > budget.total() {
        return Err(Error::Image(format!(
            "making a thumbnail would take {} MB, more than the {} MB set aside",
            needed >> 20,
            budget.total() >> 20
        )));
    }
    let _held = budget.hold(needed);

    // At eight bits a channel, which is all a thumbnail has: the block sums
    // of a deeper image overflow.
    let (decoded, orientation) = decode(bytes).map_err(failed)?;
    let mut flat = flatten_onto_white(decoded);
    flat.apply_orientation(orientation);
    let mut rgb = flat.into_rgb8();
    let (width, height) = fit_within(rgb.width(), rgb.height(), max_width, max_height);
    // Each sum is of 32 bits, so a reduction of more than a few thousand
    // times over goes in steps of sixteen.
    while u64::from(rgb.width() / width) * u64::from(rgb.height() / height) > 1 << 16 {
        let step_width = rgb.width().div_ceil(16).max(width);
        let step_height = rgb.height().div_ceil(16).max(height);
        rgb = image::imageops::thumbnail(&rgb, step_width, step_height);
    }
    if (width, height) != rgb.dimensions() {
        rgb = image::imageops::thumbnail(&rgb, width, height);
    }
    // Halved chroma is plenty for a picture this small.
    encode_baseline_jpeg(&rgb, 80, true)
}

/// Is the file at `path` an image the image step reads: in a format it
/// decodes, with a header that reads? A file that only begins like one, text
/// starting "BM" as a BMP does, is not.
pub fn is_raster_image(path: &Path) -> bool {
    ImageReader::open(path)
        .and_then(ImageReader::with_guessed_format)
        .ok()
        .and_then(|reader| reader.into_decoder().ok())
        .is_some()
}

/// About as much memory as converting the image at `path` for `options`
/// holds at once, reckoned from its header, before the rest of the file is
/// read: the file itself, and [what decoding and converting it
/// hold](Footprint::converting). Just the file for an image whose header
/// cannot be read, which fails as it is decoded, and for one past
/// [`MAX_PIXELS`], refused before anything is allocated for it.
pub fn memory_needed(path: &Path, options: &ImageOptions) -> u64 {
    let file = std::fs::metadata(path).map_or(0, |metadata| metadata.len());
    let footprint = ImageReader::open(path)
        .ok()
        .zip(std::fs::File::open(path).ok())
        .and_then(|(reader, mut header)| Footprint::of(reader, &mut header));
    file.saturating_add(footprint.map_or(0, |footprint| footprint.converting(options)))
}

/// What an image takes in memory as it is decoded and converted, reckoned
/// from its header. Measured for each format at 64 megapixels; what is
/// reckoned here is never less.
struct Footprint {
    /// Its size as stored, before its EXIF orientation turns it.
    width: u32,
    height: u32,
    /// The image decoded, at its own depth.
    decoded: u64,
    /// What its decoder holds beside the decoded image.
    decoding: u64,
    /// The image once flat: at eight bits a channel if it had alpha, at its
    /// own depth if not. A copy of it, turned say, is this big.
    flat: u64,
    /// What the flat image holds, which is more where it was flattened in
    /// the memory the decoded image took.
    flat_held: u64,
    /// Whether it is eight-bit RGB, which a thumbnail needs no copy of.
    flat_rgb8: bool,
    /// What flattening holds beside the decoded image: nothing where the
    /// flat image takes the decoded one's place, the flat image where it is
    /// made anew from a deeper one.
    flattening: u64,
    /// Whether its EXIF orientation turns it a quarter, which takes a turned
    /// copy.
    quarter_turn: bool,
}

/// What a page holds once resampled, as it is made grey, dithered and coded:
/// at most 1024 x 1024 at sixteen bits a channel, its grey and RGB copies,
/// and the JPEG rewritten, which held 17 MB for a page that size.
const PAGE_MEMORY: u64 = 32 << 20;

impl Footprint {
    /// Read from an image's header by `reader`, and from `header`, the same
    /// image, where its format keeps what its decoder holds further in.
    /// `None` for one that cannot be decoded, or is past [`MAX_PIXELS`].
    fn of<R: BufRead + Seek>(
        reader: ImageReader<R>,
        header: &mut (impl Read + Seek),
    ) -> Option<Self> {
        let reader = reader.with_guessed_format().ok()?;
        let format = reader.format()?;
        let mut decoder = reader.into_decoder().ok()?;
        let (width, height) = decoder.dimensions();
        let pixels = u64::from(width) * u64::from(height);
        if pixels > MAX_PIXELS {
            return None;
        }

        let color = decoder.color_type();
        let decoded = decoder.total_bytes();
        let gray = matches!(
            color,
            image::ColorType::L8
                | image::ColorType::La8
                | image::ColorType::L16
                | image::ColorType::La16
        );
        let flat_alpha = pixels * if gray { 1 } else { 3 };
        let (flat, flat_held, flattening) = if !color.has_alpha() {
            (decoded, decoded, 0)
        } else {
            match color.bytes_per_pixel() / color.channel_count() {
                // In the decoded image's memory.
                1 => (flat_alpha, decoded, 0),
                // Made anew from a deeper one.
                2 => (flat_alpha, flat_alpha, flat_alpha),
                // By way of RGBA at eight bits.
                _ => (flat_alpha, pixels * 4, pixels * 4),
            }
        };
        let quarter_turn = matches!(
            decoder.orientation().unwrap_or(Orientation::NoTransforms),
            Orientation::Rotate90
                | Orientation::Rotate270
                | Orientation::Rotate90FlipH
                | Orientation::Rotate270FlipH
        );

        let decoding = match format {
            ImageFormat::Png | ImageFormat::Bmp => 0,
            // The indices of a frame, beside its colours.
            ImageFormat::Gif => pixels,
            // Every coefficient of a progressive scan is kept to its end.
            ImageFormat::Jpeg => jpeg_coefficients(header, width, height).unwrap_or(decoded),
            ImageFormat::WebP => match webp_kind(header) {
                // Luma and quarter-size chroma.
                Some(WebpKind::Lossy) => pixels * 3 / 2,
                // Decoded where it lies; a byte a pixel to spare.
                Some(WebpKind::Lossless) => pixels,
                _ => decoded,
            },
            // The TIFF decoder's own buffer, copied into the image.
            _ => decoded,
        };

        Some(Footprint {
            width,
            height,
            decoded,
            decoding,
            flat,
            flat_held,
            flat_rgb8: color == image::ColorType::Rgb8 || (color.has_alpha() && !gray),
            flattening,
            quarter_turn,
        })
    }

    /// Its width and height as shown, turned as its EXIF data says.
    fn shown(&self) -> (u32, u32) {
        if self.quarter_turn {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    /// The most it holds at once as far as it is flat and turned as shown.
    fn made_flat(&self) -> u64 {
        let decoding = self.decoded + self.decoding;
        let flattening = self.decoded + self.flattening;
        decoding
            .max(flattening)
            .max(self.turning(self.quarter_turn))
    }

    /// What turning the flat image a quarter holds: it, and the turned copy.
    fn turning(&self, turns: bool) -> u64 {
        if turns {
            self.flat_held + self.flat
        } else {
            0
        }
    }

    /// The most converting it for `options` holds at once, beside its file:
    /// decoding it, flattening it, turning it as shown or as Light Novel
    /// mode might, or resampling it, a page at a time, with the page made
    /// after.
    fn converting(&self, options: &ImageOptions) -> u64 {
        let (width, height) = self.shown();
        let may_turn = options.light_novel_mode && width > height;
        let resampling = resample_buffer(width, height, options).max(if may_turn {
            resample_buffer(height, width, options)
        } else {
            0
        });
        self.made_flat()
            .max(self.turning(may_turn))
            .max(self.flat_held + resampling)
            + PAGE_MEMORY
    }

    /// The most making a thumbnail of it holds at once: as far as it is
    /// flat and turned, then its eight-bit RGB copy.
    fn thumbnail(&self) -> u64 {
        let (width, height) = self.shown();
        let rgb8 = if self.flat_rgb8 {
            0
        } else {
            u64::from(width) * u64::from(height) * 3
        };
        self.made_flat().max(self.flat_held + rgb8) + PAGE_MEMORY
    }
}

/// The buffer resampling an image `width` x `height` to the size it is
/// shown at holds: four `f32` for every column of the source at each row of
/// the result. Nothing for one that is not resampled.
fn resample_buffer(width: u32, height: u32, options: &ImageOptions) -> u64 {
    let (clamped_w, clamped_h) =
        fit_within(width, height, MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION);
    let (target_w, target_h) =
        fit_within(clamped_w, clamped_h, options.max_width, options.max_height);
    if (target_w, target_h) == (width, height) {
        0
    } else {
        u64::from(width) * u64::from(target_h) * 16
    }
}

/// What the decoder of the JPEG `header` reads keeps beside the image: for a
/// progressive one, every coefficient, two bytes each, of each component at
/// the size its sampling makes it; for one in one scan, nothing.
fn jpeg_coefficients(header: &mut (impl Read + Seek), width: u32, height: u32) -> Option<u64> {
    let frame = jpeg_frame(header)?;
    if !frame.progressive {
        return Some(0);
    }
    let most =
        |pick: fn(&(u8, u8)) -> u8| frame.sampling.iter().map(pick).max().unwrap_or(1).max(1);
    let (most_h, most_v) = (u64::from(most(|s| s.0)), u64::from(most(|s| s.1)));
    let mcus_x = u64::from(width).div_ceil(8 * most_h);
    let mcus_y = u64::from(height).div_ceil(8 * most_v);
    Some(
        frame
            .sampling
            .iter()
            .map(|&(h, v)| mcus_x * u64::from(h) * mcus_y * u64::from(v) * 64 * 2)
            .sum(),
    )
}

/// A JPEG's frame, as its start-of-frame marker says.
struct JpegFrame {
    progressive: bool,
    /// How each component is sampled, across and down.
    sampling: Vec<(u8, u8)>,
}

/// The frame of the JPEG `reader` holds, found by stepping over the markers
/// before it.
fn jpeg_frame(reader: &mut (impl Read + Seek)) -> Option<JpegFrame> {
    let mut two = [0u8; 2];
    reader.read_exact(&mut two).ok()?;
    if two != [0xFF, 0xD8] {
        return None;
    }
    loop {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte).ok()?;
        if byte[0] != 0xFF {
            return None;
        }
        // Fill bytes before the marker.
        while byte[0] == 0xFF {
            reader.read_exact(&mut byte).ok()?;
        }
        let marker = byte[0];
        match marker {
            0x01 | 0xD0..=0xD8 => continue,
            0xD9 | 0xDA => return None,
            _ => {}
        }
        reader.read_exact(&mut two).ok()?;
        let length = u16::from_be_bytes(two);
        let body = usize::from(length.checked_sub(2)?);
        // Every start of frame but those of a table or reserved marker.
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let mut frame = vec![0; body];
            reader.read_exact(&mut frame).ok()?;
            let count = usize::from(*frame.get(5)?);
            let sampling = (0..count)
                .map(|component| frame.get(7 + 3 * component).map(|hv| (hv >> 4, hv & 15)))
                .collect::<Option<Vec<_>>>()?;
            return Some(JpegFrame {
                progressive: matches!(marker, 0xC2 | 0xC6 | 0xCA | 0xCE),
                sampling,
            });
        }
        reader.seek(SeekFrom::Current(body as i64)).ok()?;
    }
}

/// How a WebP is coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebpKind {
    Lossy,
    Lossless,
    Animated,
}

/// How the WebP `reader` holds is coded, from its chunks up to the first
/// image.
fn webp_kind(reader: &mut (impl Read + Seek)) -> Option<WebpKind> {
    let mut riff = [0u8; 12];
    reader.read_exact(&mut riff).ok()?;
    if &riff[..4] != b"RIFF" || &riff[8..] != b"WEBP" {
        return None;
    }
    loop {
        let mut chunk = [0u8; 8];
        reader.read_exact(&mut chunk).ok()?;
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        match &chunk[..4] {
            b"VP8 " => return Some(WebpKind::Lossy),
            b"VP8L" => return Some(WebpKind::Lossless),
            b"ANIM" | b"ANMF" => return Some(WebpKind::Animated),
            // Chunks are padded to an even size.
            _ => reader
                .seek(SeekFrom::Current(i64::from(size) + i64::from(size & 1)))
                .ok()?,
        };
    }
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
    // Named for what the image is, whatever its file is called.
    let conversion = match image::guess_format(bytes) {
        Ok(ImageFormat::Jpeg) => "baseline JPEG".to_string(),
        Ok(format) => format!("{}→JPEG", format_name(format)),
        Err(_) => "image→JPEG".to_string(),
    };

    let (decoded, orientation) =
        decode(bytes).map_err(|e| Error::Image(format!("{filename}: {e}")))?;

    // Alpha has to go before anything else; a transparent region would
    // otherwise quantize to whatever the undefined colour channel held.
    let mut image = flatten_onto_white(decoded);
    image.apply_orientation(orientation);

    // Light Novel mode turns landscape art: rotated, or split in two. Each
    // page is read from the image where it lies.
    let reshape = if options.light_novel_mode {
        reshape_for_vertical_reading(image.width(), image.height(), options)
    } else {
        Reshape::Whole
    };
    if reshape == Reshape::Turn {
        // Pillow's `rotate(90)` turns counter-clockwise, which is this crate's
        // `rotate270`.
        image = if options.light_novel_rotate_left {
            image.rotate270()
        } else {
            image.rotate90()
        };
    }
    let (width, height) = (image.width(), image.height());
    let pages = match reshape {
        Reshape::Split { mid } => vec![(mid, 0, width - mid, height), (0, 0, mid, height)],
        _ => vec![(0, 0, width, height)],
    };
    let reshaped = reshape != Reshape::Whole;

    let page_count = pages.len();
    let mut results = Vec::with_capacity(page_count);

    for (index, (x, y, before_w, before_h)) in pages.into_iter().enumerate() {
        let mut details = Vec::new();

        if conversion != "baseline JPEG" {
            details.push(conversion.clone());
        }

        // The spec ceiling first, then the device's own box.
        let (clamped_w, clamped_h) =
            fit_within(before_w, before_h, MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION);
        let (target_w, target_h) =
            fit_within(clamped_w, clamped_h, options.max_width, options.max_height);

        let page = if (target_w, target_h) != (before_w, before_h) {
            details.push(format!(
                "resized {before_w}x{before_h}→{target_w}x{target_h}"
            ));
            resize_region(&image, (x, y, before_w, before_h), target_w, target_h)
        } else {
            image.crop_imm(x, y, before_w, before_h)
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

        // Last, so that the format change stays the first thing said.
        if page_count > 1 {
            details.push(format!("split part {}/{page_count}", index + 1));
        } else if reshaped {
            details.push("rotated".to_string());
        }

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
            conversion: conversion.clone(),
            reshaped,
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

/// Decode an image as it is stored, and the way its EXIF orientation says to
/// turn it to show it as a reader does. The tag does not survive conversion,
/// so the pixels have to, but they are turned once flat, when they are no
/// deeper than they have to be.
fn decode(bytes: &[u8]) -> image::ImageResult<(DynamicImage, Orientation)> {
    let mut decoder = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .into_decoder()?;
    // A tag that cannot be read leaves the image as it is stored, as a
    // reader would.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);

    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(ImageError::Limits(LimitError::from_kind(
            LimitErrorKind::DimensionError,
        )));
    }
    // The decoded image comes out of the budget before the decoder runs, as
    // `ImageReader::decode` would have it, and the rest is the decoder's.
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;

    Ok((DynamicImage::from_decoder(decoder)?, orientation))
}

/// What the report calls a format: by its usual extension, `PNG`, `GIF`,
/// `WEBP`, `BMP` or `TIFF`.
fn format_name(format: ImageFormat) -> String {
    format
        .extensions_str()
        .first()
        .map_or_else(|| format!("{format:?}"), |extension| extension.to_string())
        .to_ascii_uppercase()
}

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
    match img {
        DynamicImage::ImageRgba8(rgba) => DynamicImage::ImageRgb8(rgb_over_white(rgba)),
        DynamicImage::ImageLumaA8(gray) => DynamicImage::ImageLuma8(gray_over_white(gray)),
        // Eight bits a channel, as any other conversion of a deep image here
        // makes them, before the alpha goes.
        DynamicImage::ImageRgba16(rgba) => {
            let (width, height) = rgba.dimensions();
            let raw = rgba
                .pixels()
                .flat_map(|pixel| {
                    let alpha = eight_bits(pixel[3]);
                    [0, 1, 2].map(|channel| over_white(eight_bits(pixel[channel]), alpha))
                })
                .collect();
            DynamicImage::ImageRgb8(
                RgbImage::from_raw(width, height, raw).expect("3 bytes a pixel"),
            )
        }
        DynamicImage::ImageLumaA16(gray) => {
            let (width, height) = gray.dimensions();
            let raw = gray
                .pixels()
                .map(|pixel| over_white(eight_bits(pixel[0]), eight_bits(pixel[1])))
                .collect();
            DynamicImage::ImageLuma8(
                GrayImage::from_raw(width, height, raw).expect("a byte a pixel"),
            )
        }
        img if !img.color().has_alpha() => img,
        img => DynamicImage::ImageRgb8(rgb_over_white(img.into_rgba8())),
    }
}

/// `rgba` laid over white, in the memory it is in: each pixel's three bytes
/// go where the four of it and those before it were.
fn rgb_over_white(rgba: image::RgbaImage) -> RgbImage {
    let (width, height) = rgba.dimensions();
    let pixels = width as usize * height as usize;
    let mut raw = rgba.into_raw();
    for pixel in 0..pixels {
        let [red, green, blue, alpha] = [0, 1, 2, 3].map(|channel| raw[4 * pixel + channel]);
        raw[3 * pixel] = over_white(red, alpha);
        raw[3 * pixel + 1] = over_white(green, alpha);
        raw[3 * pixel + 2] = over_white(blue, alpha);
    }
    raw.truncate(3 * pixels);
    RgbImage::from_raw(width, height, raw).expect("3 bytes a pixel")
}

/// [`rgb_over_white`] for a grey image, a byte a pixel.
fn gray_over_white(gray: image::GrayAlphaImage) -> GrayImage {
    let (width, height) = gray.dimensions();
    let pixels = width as usize * height as usize;
    let mut raw = gray.into_raw();
    for pixel in 0..pixels {
        raw[pixel] = over_white(raw[2 * pixel], raw[2 * pixel + 1]);
    }
    raw.truncate(pixels);
    GrayImage::from_raw(width, height, raw).expect("a byte a pixel")
}

/// A channel of `alpha` laid over white, as Pillow rounds it.
fn over_white(channel: u8, alpha: u8) -> u8 {
    let (channel, alpha) = (u32::from(channel), u32::from(alpha));
    ((channel * alpha + 255 * (255 - alpha) + 127) / 255).min(255) as u8
}

/// A sixteen-bit sample at eight bits, rounded as this crate's conversions
/// round it.
fn eight_bits(sample: u16) -> u8 {
    ((u32::from(sample) + 128) / 257) as u8
}

/// Light Novel mode: make landscape artwork readable on a portrait panel.
///
/// A double-page spread is split rather than shrunk to illegibility. The right
/// half comes first, matching the reading order of the books this is for.
///
/// Art is reshaped only if that shows it at least [`RESHAPE_GAIN`] times as
/// big. The panel never enlarges an image, so one it already shows whole, an
/// ornament or a small figure, gains nothing, nor does one nearly square. And
/// an image more than [`MAX_SPREAD_ASPECT`] times as wide as it is tall is a
/// rule or a banner rather than a page or a spread of two, and stays whole.
fn reshape_for_vertical_reading(width: u32, height: u32, options: &ImageOptions) -> Reshape {
    if width <= height {
        return Reshape::Whole;
    }

    let aspect = width as f64 / height as f64;
    if aspect > MAX_SPREAD_ASPECT {
        return Reshape::Whole;
    }
    let shown = shown_scale(width, height, options);

    const SPREAD_ASPECT: f64 = 1.8;
    if aspect > SPREAD_ASPECT {
        let mid = width / 2;
        if shown_scale(width - mid, height, options) < shown * RESHAPE_GAIN {
            return Reshape::Whole;
        }
        return Reshape::Split { mid };
    }

    if shown_scale(height, width, options) < shown * RESHAPE_GAIN {
        return Reshape::Whole;
    }
    Reshape::Turn
}

/// How Light Novel mode reshapes an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reshape {
    /// Not at all.
    Whole,
    /// Into two pages, either side of the column `mid`, the right half first.
    Split { mid: u32 },
    /// Turned a quarter, to stand on the panel.
    Turn,
}

/// The part of `image` at `x`, `y`, `region_width` wide and `region_height`
/// high, resized to `width` x `height` as `resize_exact` resizes a copy of
/// it, but read where it lies.
fn resize_region(
    image: &DynamicImage,
    (x, y, region_width, region_height): (u32, u32, u32, u32),
    width: u32,
    height: u32,
) -> DynamicImage {
    use image::imageops::{crop_imm, resize};
    let filter = FilterType::Lanczos3;
    let (w, h) = (region_width, region_height);
    match image {
        DynamicImage::ImageLuma8(buffer) => DynamicImage::ImageLuma8(resize(
            &*crop_imm(buffer, x, y, w, h),
            width,
            height,
            filter,
        )),
        DynamicImage::ImageRgb8(buffer) => DynamicImage::ImageRgb8(resize(
            &*crop_imm(buffer, x, y, w, h),
            width,
            height,
            filter,
        )),
        DynamicImage::ImageLuma16(buffer) => DynamicImage::ImageLuma16(resize(
            &*crop_imm(buffer, x, y, w, h),
            width,
            height,
            filter,
        )),
        DynamicImage::ImageRgb16(buffer) => DynamicImage::ImageRgb16(resize(
            &*crop_imm(buffer, x, y, w, h),
            width,
            height,
            filter,
        )),
        other => other
            .crop_imm(x, y, w, h)
            .resize_exact(width, height, filter),
    }
}

/// How much bigger Light Novel mode has to show art to reshape it.
const RESHAPE_GAIN: f64 = 1.15;

/// The widest a page or a spread of two pages is, for its height: two pages
/// side by side, each at most a little wider than tall.
const MAX_SPREAD_ASPECT: f64 = 2.6;

/// The scale the device shows an image `width` x `height` at: fitted to its
/// box, never enlarged.
fn shown_scale(width: u32, height: u32, options: &ImageOptions) -> f64 {
    let box_width = options.max_width.min(MAX_IMAGE_DIMENSION) as f64;
    let box_height = options.max_height.min(MAX_IMAGE_DIMENSION) as f64;
    (box_width / width as f64)
        .min(box_height / height as f64)
        .min(1.0)
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
/// cost nothing and save 15-20%. So is a grey image kept in colour, whose
/// chroma is just as flat. Encoding either as a single-component grayscale
/// JPEG would be smaller still, but that changes the file's structure and
/// wants testing on real hardware first.
///
/// Huffman tables are optimized for each image, matching the reference's
/// `optimize=True`, but by rewriting the finished file rather than by asking
/// the encoder for it: `jpeg-encoder`'s own optimization also splits the single
/// interleaved scan into three, which several decoders mishandle. See
/// [`crate::jpeg`]. The rewrite is lossless and leaves the file's structure
/// alone; if anything about it fails, the unoptimized file is kept.
fn encode_baseline_jpeg(rgb: &RgbImage, quality: u8, halve_chroma: bool) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new(&mut out, quality);

    let halve_chroma = halve_chroma || rgb.pixels().all(|p| p[0] == p[1] && p[1] == p[2]);
    encoder.set_sampling_factor(if halve_chroma {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A progressive JPEG's coefficients are two bytes each, for each
    /// component at the size its sampling makes it, whole MCUs of it: at
    /// 4:2:0, luma at full size and two chromas at a quarter, three bytes a
    /// pixel. One in a single scan keeps none.
    #[test]
    fn a_progressive_jpeg_keeps_two_bytes_a_coefficient() {
        let (width, height) = (160u16, 96u16);
        let rgb = vec![128u8; usize::from(width) * usize::from(height) * 3];
        for (progressive, sampling, expected) in [
            (false, jpeg_encoder::SamplingFactor::F_2_2, 0),
            (true, jpeg_encoder::SamplingFactor::F_2_2, 160 * 96 * 3),
            (true, jpeg_encoder::SamplingFactor::F_1_1, 160 * 96 * 6),
        ] {
            let mut jpeg = Vec::new();
            let mut encoder = JpegEncoder::new(&mut jpeg, 80);
            encoder.set_progressive(progressive);
            encoder.set_sampling_factor(sampling);
            encoder.encode(&rgb, width, height, ColorType::Rgb).unwrap();
            let coefficients =
                jpeg_coefficients(&mut Cursor::new(&jpeg), u32::from(width), u32::from(height));
            assert_eq!(coefficients, Some(expected), "{progressive} {sampling:?}");
        }
    }

    /// The kind of a WebP is read from its chunks, an extended one's too.
    #[test]
    fn a_webp_is_known_by_its_chunks() {
        let chunk = |fourcc: &[u8; 4], body: &[u8]| {
            let mut chunk = fourcc.to_vec();
            chunk.extend_from_slice(&(body.len() as u32).to_le_bytes());
            chunk.extend_from_slice(body);
            if body.len() % 2 == 1 {
                chunk.push(0);
            }
            chunk
        };
        let riff = |chunks: Vec<Vec<u8>>| {
            let body: Vec<u8> = chunks.concat();
            let mut riff = b"RIFF".to_vec();
            riff.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
            riff.extend_from_slice(b"WEBP");
            riff.extend_from_slice(&body);
            riff
        };
        let cases = [
            (riff(vec![chunk(b"VP8 ", &[0; 10])]), WebpKind::Lossy),
            (riff(vec![chunk(b"VP8L", &[0; 5])]), WebpKind::Lossless),
            (
                riff(vec![
                    chunk(b"VP8X", &[0; 10]),
                    chunk(b"ALPH", &[0; 3]),
                    chunk(b"VP8 ", &[0; 4]),
                ]),
                WebpKind::Lossy,
            ),
            (
                riff(vec![chunk(b"VP8X", &[0; 10]), chunk(b"ANIM", &[0; 6])]),
                WebpKind::Animated,
            ),
        ];
        for (webp, kind) in cases {
            assert_eq!(webp_kind(&mut Cursor::new(&webp)), Some(kind));
        }
    }
}
