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

use image::error::{DecodingError, ImageError, ImageFormatHint, LimitError, LimitErrorKind};
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

/// What reading an image's header may hold beside it: the ICC profile a
/// PNG's decoder unpacks, and the text and EXIF chunks it keeps. A decoder is
/// held to half of this as it is made, as a buffer it grows doubles, and can
/// come to twice what it counts before it stops; it lets go of an ICC
/// profile larger, which nothing here uses. A WebP's decoder is held to no
/// limit at all, so its EXIF is never left to it: see [`webp_orientation`].
/// Nor is a GIF's, which collects its ICC profile and XMP as it is made, so
/// its header is read without it: see [`gif_start`].
pub const HEADER_MEMORY: u64 = 16 << 20;

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

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
/// reading its header may take is held from `budget` while it is read, and
/// what decoding it takes, until the thumbnail is made.
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
    let needed = thumbnail_needed(bytes, budget);
    if needed > budget.total() {
        return Err(thumbnail_oversize_error(needed, budget.total()));
    }
    let _held = budget.hold(needed);
    thumbnail_decode(bytes, max_width, max_height)
}

/// About as much memory as making a thumbnail of the image in `bytes` holds
/// at once, reckoned from its header, which is read holding [`HEADER_MEMORY`]
/// of `budget`. Zero for one whose header does not read, which fails as it is
/// decoded.
pub(crate) fn thumbnail_needed(bytes: &[u8], budget: &MemoryBudget) -> u64 {
    let _reading = budget.hold(HEADER_MEMORY);
    Header::read(Cursor::new(bytes))
        .and_then(|header| Footprint::of(header, bytes.len() as u64))
        .map_or(0, |footprint| footprint.thumbnail())
}

/// The error a thumbnail that would take `needed` bytes, more than the
/// `total` set aside, is refused with.
pub(crate) fn thumbnail_oversize_error(needed: u64, total: u64) -> Error {
    Error::Image(format!(
        "making a thumbnail would take {} MB, more than the {} MB set aside",
        needed >> 20,
        total >> 20
    ))
}

/// Make the thumbnail itself, the memory it needs set aside already: see
/// [`thumbnail`]. Decoding it is held to no budget here, so a caller that
/// must bound it, against an image crafted to make a decoder allocate wildly,
/// runs this where it can stop it (see [`crate::worker`]).
pub(crate) fn thumbnail_decode(bytes: &[u8], max_width: u32, max_height: u32) -> Result<Vec<u8>> {
    let failed = |e: ImageError| Error::Image(e.to_string());
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
/// starting "BM" as a BMP does, is not. Only its header is read, as
/// [`Header::read`] reads it.
pub fn is_raster_image(path: &Path) -> bool {
    header_of(path).is_some()
}

/// About as much memory as converting the image at `path` for `options`
/// holds at once, reckoned from its header, before the rest of the file is
/// read: the file itself, and [what decoding and converting it
/// hold](Footprint::converting). Just the file for an image whose header
/// cannot be read, which fails as it is decoded, and for one past
/// [`MAX_PIXELS`], refused before anything is allocated for it.
///
/// Reading the header holds no more than [`HEADER_MEMORY`], which the caller
/// sets aside while it is read.
pub fn memory_needed(path: &Path, options: &ImageOptions) -> u64 {
    let file = std::fs::metadata(path).map_or(0, |metadata| metadata.len());
    let footprint = header_of(path).and_then(|header| Footprint::of(header, file));
    file.saturating_add(footprint.map_or(0, |footprint| footprint.converting(options)))
}

/// The header of the image at `path`: see [`Header::read`].
fn header_of(path: &Path) -> Option<Header> {
    let file = std::fs::File::open(path).ok()?;
    Header::read(std::io::BufReader::new(file))
}

/// What an image's header says, read without the rest of the file, or more
/// than [`HEADER_MEMORY`] held reading it.
struct Header {
    format: ImageFormat,
    /// Its size as stored, before its EXIF orientation turns it.
    width: u32,
    height: u32,
    /// What its decoder decodes it as.
    color: image::ColorType,
    /// How its EXIF data turns it, as its decoder reads it.
    orientation: Orientation,
    /// A JPEG's frame, which tells what its decoder keeps beside the image.
    jpeg: Option<JpegFrame>,
    /// How a WebP is coded, which tells the same.
    webp: Option<WebpKind>,
    /// What a GIF's decoder reads before its image, which tells the same.
    gif: Option<GifStart>,
}

impl Header {
    /// The header of the image `reader` reads, from its start: `None` for an
    /// image in no format the image step decodes, or whose header does not
    /// read. A JPEG's is read here, marker by marker, as its decoder reads the
    /// whole file as it is made, and copies each segment it knows; so is a
    /// GIF's, block by block, as its decoder collects its ICC profile and XMP
    /// as it is made, held to no limit of ours. Any other's decoder reads its
    /// header, held to [`HEADER_MEMORY`].
    fn read<R: BufRead + Seek>(reader: R) -> Option<Header> {
        let reader = ImageReader::new(reader).with_guessed_format().ok()?;
        let format = reader.format()?;
        let mut inner = reader.into_inner();
        if format == ImageFormat::Jpeg {
            let frame = jpeg_frame(&mut inner)?;
            return Some(Header {
                format,
                width: u32::from(frame.width),
                height: u32::from(frame.height),
                // Grey for one component, and RGB for any other number.
                color: if frame.sampling.len() == 1 {
                    image::ColorType::L8
                } else {
                    image::ColorType::Rgb8
                },
                orientation: frame.orientation,
                jpeg: Some(frame),
                webp: None,
                gif: None,
            });
        }
        if format == ImageFormat::Gif {
            let start = gif_start(&mut inner)?;
            return Some(Header {
                format,
                width: u32::from(start.width),
                height: u32::from(start.height),
                // Its decoder decodes every GIF in RGBA, and a GIF has no
                // EXIF to turn it.
                color: image::ColorType::Rgba8,
                orientation: Orientation::NoTransforms,
                jpeg: None,
                webp: None,
                gif: Some(start),
            });
        }
        if format == ImageFormat::Tiff {
            // A TIFF's decoder reads its image file directory as it is made,
            // and a crafted one — many strips, say — makes it hold arrays of
            // whatever counts the directory claims, held to no limit of ours.
            // Its size and depth are read here from the directory instead,
            // without the decoder.
            let (width, height, color, orientation) = tiff_header(&mut inner)?;
            return Some(Header {
                format,
                width,
                height,
                color,
                orientation,
                jpeg: None,
                webp: None,
                gif: None,
            });
        }

        // A WebP's decoder reads the whole of its EXIF to say how it is
        // turned, held to no limit, so that is read here too.
        let (webp, webp_orientation) = if format == ImageFormat::WebP {
            let kind = webp_kind(&mut inner);
            inner.seek(SeekFrom::Start(0)).ok()?;
            let orientation = webp_orientation(&mut inner);
            inner.seek(SeekFrom::Start(0)).ok()?;
            (kind, Some(orientation))
        } else {
            (None, None)
        };
        let mut reader = ImageReader::with_format(inner, format);
        reader.limits(header_limits());
        let mut decoder = reader.into_decoder().ok()?;
        let (width, height) = decoder.dimensions();
        // A tag that cannot be read leaves the image as it is stored, as a
        // reader would.
        let orientation = webp_orientation
            .unwrap_or_else(|| decoder.orientation().unwrap_or(Orientation::NoTransforms));
        Some(Header {
            format,
            width,
            height,
            color: decoder.color_type(),
            orientation,
            jpeg: None,
            webp,
            gif: None,
        })
    }
}

/// Limits that hold a decoder, as it is made, within [`HEADER_MEMORY`]: to
/// half of it, as the buffer a PNG's unpacks its ICC profile into doubles as
/// it grows, past what it counts.
fn header_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_alloc = Some(HEADER_MEMORY / 2);
    limits
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
    /// Reckoned from the `header` of an image whose file is `file` bytes
    /// long. `None` for one past [`MAX_PIXELS`].
    fn of(header: Header, file: u64) -> Option<Self> {
        let Header {
            format,
            width,
            height,
            color,
            orientation,
            jpeg,
            webp,
            gif,
        } = header;
        let pixels = u64::from(width) * u64::from(height);
        let gif_frame = gif.as_ref().and_then(|start| start.frame);
        if pixels > MAX_PIXELS || gif_frame.is_some_and(|frame| frame.pixels() > MAX_PIXELS) {
            return None;
        }

        let decoded = pixels * u64::from(color.bytes_per_pixel());
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
            orientation,
            Orientation::Rotate90
                | Orientation::Rotate270
                | Orientation::Rotate90FlipH
                | Orientation::Rotate270FlipH
        );

        let decoding = match format {
            // The chunks it keeps, and the ICC profile it unpacks.
            ImageFormat::Png => HEADER_MEMORY,
            ImageFormat::Bmp => 0,
            // The indices of a frame, beside its colours; a frame that does not
            // lie across the canvas decoded in RGBA of its own size, to be laid
            // on it; and the ICC profile and XMP collected before it.
            ImageFormat::Gif => {
                let apart = match gif_frame {
                    Some(frame) if !frame.lies_across(width, height) => frame.pixels() * 5,
                    _ => 0,
                };
                pixels + apart + gif.as_ref().map_or(0, |start| start.metadata)
            }
            // Its copy of the file, and of each segment it knows, beside it;
            // and every coefficient of a progressive scan, kept to its end.
            ImageFormat::Jpeg => file * 2 + jpeg.as_ref().map_or(0, jpeg_coefficients),
            ImageFormat::WebP => match webp {
                // Luma and quarter-size chroma; and an alpha plane decoded in
                // RGBA, then kept a byte a pixel, beside them.
                Some(WebpKind::Lossy) if color.has_alpha() => pixels * 13 / 2,
                Some(WebpKind::Lossy) => pixels * 3 / 2,
                // With alpha, decoded where it lies, a byte a pixel to spare;
                // without, decoded in RGBA beside the RGB it comes out as.
                Some(WebpKind::Lossless) if color.has_alpha() => pixels,
                Some(WebpKind::Lossless) => pixels * 4,
                // A canvas in RGBA, and a frame decoded in any of those ways
                // beside it.
                _ => pixels * 15,
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

/// What the decoder of the JPEG whose frame is `frame` keeps beside the
/// image: for a progressive one, every coefficient, two bytes each, of each
/// component at the size its sampling makes it; for one in one scan, nothing.
fn jpeg_coefficients(frame: &JpegFrame) -> u64 {
    if !frame.progressive {
        return 0;
    }
    let most =
        |pick: fn(&(u8, u8)) -> u8| frame.sampling.iter().map(pick).max().unwrap_or(1).max(1);
    let (most_h, most_v) = (u64::from(most(|s| s.0)), u64::from(most(|s| s.1)));
    let mcus_x = u64::from(frame.width).div_ceil(8 * most_h);
    let mcus_y = u64::from(frame.height).div_ceil(8 * most_v);
    frame
        .sampling
        .iter()
        .map(|&(h, v)| mcus_x * u64::from(h) * mcus_y * u64::from(v) * 64 * 2)
        .sum()
}

/// A JPEG's frame, as its start-of-frame marker says, and how its EXIF data
/// turns it.
struct JpegFrame {
    width: u16,
    height: u16,
    progressive: bool,
    /// How each of its components is sampled, across and down.
    sampling: Vec<(u8, u8)>,
    orientation: Orientation,
}

/// The frame of the JPEG `reader` holds, found by stepping over the markers
/// before its scan as its decoder steps over them, and how the last EXIF
/// segment among them turns it, as its decoder takes it. Each segment is
/// passed over unread but those two kinds, which are a few kilobytes at most.
/// `None` where its decoder reads no frame: at the end of the image before a
/// scan, or at a frame or a segment of a kind it does not read.
fn jpeg_frame(reader: &mut (impl Read + Seek)) -> Option<JpegFrame> {
    let mut two = [0u8; 2];
    reader.read_exact(&mut two).ok()?;
    if two != [0xFF, 0xD8] {
        return None;
    }
    let mut frame = None;
    let mut orientation = Orientation::NoTransforms;
    let mut byte = [0u8; 1];
    let mut last = 0;
    loop {
        reader.read_exact(&mut byte).ok()?;
        let mut marker = byte[0];
        // Fill bytes after a marker's 0xFF, or a zero stuffed after it.
        if last == 0xFF {
            while marker == 0xFF || marker == 0 {
                last = marker;
                reader.read_exact(&mut byte).ok()?;
                marker = byte[0];
            }
        }
        // A byte between segments, which its decoder passes over.
        if last != 0xFF {
            last = marker;
            continue;
        }
        last = marker;
        match marker {
            // The scan, which the frame comes before.
            0xDA => break,
            0xD9 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCC..=0xCF | 0xDC => return None,
            _ => {}
        }
        // Any other marker has a length, a lone one too.
        reader.read_exact(&mut two).ok()?;
        let length = u16::from_be_bytes(two);
        let body = usize::from(length.checked_sub(2)?);
        let starts_frame = matches!(marker, 0xC0..=0xC2);
        if !starts_frame && marker != 0xE1 {
            reader.seek(SeekFrom::Current(body as i64)).ok()?;
            continue;
        }
        let mut data = vec![0; body];
        reader.read_exact(&mut data).ok()?;
        if starts_frame {
            let count = usize::from(*data.get(5)?);
            let sampling = (0..count)
                .map(|component| data.get(7 + 3 * component).map(|hv| (hv >> 4, hv & 15)))
                .collect::<Option<Vec<_>>>()?;
            frame = Some(JpegFrame {
                width: u16::from_be_bytes([*data.get(3)?, *data.get(4)?]),
                height: u16::from_be_bytes([*data.get(1)?, *data.get(2)?]),
                progressive: marker == 0xC2,
                sampling,
                orientation: Orientation::NoTransforms,
            });
        } else if data.len() > 6 && data.starts_with(b"Exif\0\0") {
            orientation =
                Orientation::from_exif_chunk(&data[6..]).unwrap_or(Orientation::NoTransforms);
        }
    }
    let mut frame = frame?;
    frame.orientation = orientation;
    Some(frame)
}

/// A GIF's first frame: where on the canvas its decoder lays it, and its
/// size.
#[derive(Debug, Clone, Copy)]
struct GifFrame {
    left: u16,
    top: u16,
    width: u16,
    height: u16,
}

impl GifFrame {
    fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Does it lie across a canvas `width` x `height` from its left edge, as
    /// its decoder decodes a frame where it lies?
    fn lies_across(&self, width: u32, height: u32) -> bool {
        self.left == 0
            && u32::from(self.width) == width
            && u32::from(self.top) + u32::from(self.height) <= height
    }
}

/// What a GIF's decoder collects of an ICC profile, or of XMP, before it
/// fails: its own limit of 50 MB, as the image step's limits are not passed
/// on to it.
const GIF_METADATA_LIMIT: u64 = 50_000_000;

/// What a GIF's decoder reads before its image, as it is made.
struct GifStart {
    /// The canvas.
    width: u16,
    height: u16,
    /// What the ICC profile and XMP it collects can come to.
    metadata: u64,
    /// The first frame, where the GIF starts with one.
    frame: Option<GifFrame>,
}

/// What the GIF `reader` holds starts with, read as its decoder reads it
/// when it is made: the canvas; then, past the colour table and each
/// extension before it, a block of data at a time, the first frame. `None`
/// where the decoder fails to be made: at a signature or an extension it
/// does not know, a graphic control extension of the wrong length, or the
/// file's end.
///
/// The decoder collects the data of an application extension that names
/// itself an ICC profile or XMP, with a byte more for each block of XMP, to
/// [`GIF_METADATA_LIMIT`], past which it fails. It keeps the last of each
/// kind, in a buffer that doubles as it grows, which can come to twice what
/// it collects: what the largest of each can come to is reckoned.
fn gif_start(reader: &mut (impl Read + Seek)) -> Option<GifStart> {
    // The two kinds of application extension the decoder collects.
    const PROFILE: usize = 0;
    const XMP: usize = 1;

    let mut screen = [0u8; 13];
    reader.read_exact(&mut screen).ok()?;
    if &screen[..6] != b"GIF87a" && &screen[..6] != b"GIF89a" {
        return None;
    }
    if screen[10] & 0x80 != 0 {
        let colours = 3 * (2i64 << (screen[10] & 7));
        reader.seek(SeekFrom::Current(colours)).ok()?;
    }
    // The most collected of each kind.
    let mut largest = [0u64; 2];
    let mut byte = [0u8; 1];
    let frame = 'blocks: loop {
        reader.read_exact(&mut byte).ok()?;
        match byte[0] {
            // An extension: its label, then blocks up to an empty one.
            0x21 => {
                reader.read_exact(&mut byte).ok()?;
                let label = byte[0];
                // Plain text, graphic control, comment or application.
                if !matches!(label, 0x01 | 0xF9 | 0xFE | 0xFF) {
                    return None;
                }
                // The kind the first block names an application extension,
                // if it is one the decoder collects, and what it has
                // collected of it: each block, after its length for XMP.
                let mut kind = None;
                let mut collected = 0u64;
                // The length of the block before.
                let mut last = None;
                loop {
                    reader.read_exact(&mut byte).ok()?;
                    let length = byte[0];
                    if length == 0 {
                        break;
                    }
                    if last.is_none() && label == 0xFF && length == 11 {
                        let mut name = [0u8; 11];
                        reader.read_exact(&mut name).ok()?;
                        kind = match &name {
                            b"ICCRGBG1012" => Some(PROFILE),
                            b"XMP DataXMP" => Some(XMP),
                            _ => None,
                        };
                    } else {
                        if let Some(kind) = kind {
                            collected += u64::from(length) + u64::from(kind == XMP);
                            if collected > GIF_METADATA_LIMIT {
                                largest[kind] = GIF_METADATA_LIMIT;
                                break 'blocks None;
                            }
                            largest[kind] = largest[kind].max(collected);
                        }
                        reader.seek(SeekFrom::Current(i64::from(length))).ok()?;
                    }
                    last = Some(length);
                }
                // A graphic control extension's last block is of four bytes.
                if label == 0xF9 && last != Some(4) {
                    return None;
                }
            }
            // Anything else ends what the decoder reads as it is made, once
            // it has the byte after it: a frame's descriptor, say.
            introducer => {
                let mut descriptor = [0u8; 8];
                reader.read_exact(&mut descriptor[..1]).ok()?;
                if introducer != 0x2C || reader.read_exact(&mut descriptor[1..]).is_err() {
                    break None;
                }
                let word = |at: usize| u16::from_le_bytes([descriptor[at], descriptor[at + 1]]);
                break Some(GifFrame {
                    left: word(0),
                    top: word(2),
                    width: word(4),
                    height: word(6),
                });
            }
        }
    };
    Some(GifStart {
        width: u16::from_le_bytes([screen[6], screen[7]]),
        height: u16::from_le_bytes([screen[8], screen[9]]),
        metadata: 2 * (largest[PROFILE] + largest[XMP]),
        frame,
    })
}

/// The size, depth and turn of the TIFF `reader`, read from its first image
/// file directory as its decoder reads it when it is made, but without the
/// decoder, so none of the arrays a crafted directory could claim are held.
/// `None` for one this does not read: another byte order, BigTIFF, or a
/// directory that does not hold the width and height.
///
/// The depth is read from the samples a pixel has and the bits a sample has,
/// and taken as the widest a sample is, so the colour is never reckoned
/// smaller than the decoder decodes it to. The turn is only whether it is a
/// quarter, which is all the footprint asks.
fn tiff_header(
    reader: &mut (impl Read + Seek),
) -> Option<(u32, u32, image::ColorType, Orientation)> {
    let mut head = [0u8; 8];
    reader.read_exact(&mut head).ok()?;
    let big_endian = match &head[..2] {
        b"II" => false,
        b"MM" => true,
        _ => return None,
    };
    let u16_at = |b: &[u8]| {
        if big_endian {
            u16::from_be_bytes([b[0], b[1]])
        } else {
            u16::from_le_bytes([b[0], b[1]])
        }
    };
    let u32_at = |b: &[u8]| {
        if big_endian {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        }
    };
    // 42 marks a classic TIFF; 43 is BigTIFF, which this does not read.
    if u16_at(&head[2..4]) != 42 {
        return None;
    }
    reader
        .seek(SeekFrom::Start(u64::from(u32_at(&head[4..8]))))
        .ok()?;
    let mut count = [0u8; 2];
    reader.read_exact(&mut count).ok()?;

    // What a tag whose value is a single number holds, from the entry's value
    // field, by its type: a SHORT or a LONG.
    let number = |entry: &[u8; 12]| -> Option<u32> {
        match u16_at(&entry[2..4]) {
            3 => Some(u32::from(u16_at(&entry[8..10]))),
            4 => Some(u32_at(&entry[8..12])),
            _ => None,
        }
    };

    let (mut width, mut height) = (None, None);
    let mut samples = 1u32;
    let mut bits_entry = None;
    let mut turned = false;
    for _ in 0..u16_at(&count) {
        let mut entry = [0u8; 12];
        reader.read_exact(&mut entry).ok()?;
        match u16_at(&entry[0..2]) {
            256 => width = number(&entry),
            257 => height = number(&entry),
            277 => samples = number(&entry).unwrap_or(1),
            258 => bits_entry = Some(entry),
            // A quarter turn, 90 or 270 degrees, is EXIF orientation 5 to 8.
            274 => turned = matches!(number(&entry), Some(5..=8)),
            _ => {}
        }
    }
    let width = width?;
    let height = height?;
    // The widest a sample is, from the BitsPerSample entry. Its count is of
    // 32 bits, as every entry's is; its values, SHORTs or LONGs, are in the
    // value field if they fit, or at the offset it holds, read no further than
    // the samples a pixel has, so a crafted count holds nothing. With no entry
    // a sample is TIFF's default of one bit; with one that does not read, it
    // is taken at the widest the decoder takes, never the narrowest.
    let bits = match bits_entry {
        None => 1,
        Some(entry) => (|| {
            let size = match u16_at(&entry[2..4]) {
                3 => 2,
                4 => 4,
                _ => return None,
            };
            let n = (u32_at(&entry[4..8]) as usize).min(samples.clamp(1, 16) as usize);
            let values = if n * size <= 4 {
                entry[8..12].to_vec()
            } else {
                reader
                    .seek(SeekFrom::Start(u64::from(u32_at(&entry[8..12]))))
                    .ok()?;
                let mut values = vec![0u8; n * size];
                reader.read_exact(&mut values).ok()?;
                values
            };
            (0..n)
                .filter_map(|i| {
                    let value = values.get(size * i..size * (i + 1))?;
                    Some(if size == 2 {
                        u32::from(u16_at(value))
                    } else {
                        u32_at(value)
                    })
                })
                .max()
        })()
        .unwrap_or(32),
    };

    // The colour the decoder would decode it to, never narrower. `image`
    // decodes a TIFF of eight or sixteen bits a sample at that depth, and one
    // of 32 only as floating-point RGB or RGBA; a pixel of more than four
    // samples is taken at four.
    let color = match (samples.clamp(1, 4), bits) {
        (1, ..=8) => image::ColorType::L8,
        (1, 9..=16) => image::ColorType::L16,
        (2, ..=8) => image::ColorType::La8,
        (2, 9..=16) => image::ColorType::La16,
        (3, ..=8) => image::ColorType::Rgb8,
        (3, 9..=16) => image::ColorType::Rgb16,
        (4, ..=8) => image::ColorType::Rgba8,
        (4, 9..=16) => image::ColorType::Rgba16,
        // Grey of more than sixteen bits the decoder refuses; it is reckoned
        // as wide as what it does take.
        (1..=3, _) => image::ColorType::Rgb32F,
        _ => image::ColorType::Rgba32F,
    };
    let orientation = if turned {
        Orientation::Rotate90
    } else {
        Orientation::NoTransforms
    };
    Some((width, height, color, orientation))
}

/// How a WebP is coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebpKind {
    Lossy,
    Lossless,
    Animated,
}

/// Where the EXIF of the WebP `reader` holds lies, as its decoder finds it:
/// the first EXIF chunk of an extended WebP. A simple one has none.
///
/// The decoder walks the chunks after the extended header while one starts
/// before where it takes the container to end: as far on from the header's
/// end as the container's size, less its first twelve bytes. That runs past
/// the container's true end by the header's own size, ten bytes as a rule,
/// and EXIF past that is not found.
fn webp_exif(reader: &mut (impl Read + Seek)) -> Option<std::ops::Range<u64>> {
    let mut riff = [0u8; 12];
    reader.read_exact(&mut riff).ok()?;
    if &riff[..4] != b"RIFF" || &riff[8..] != b"WEBP" {
        return None;
    }
    let riff_size = u32::from_le_bytes([riff[4], riff[5], riff[6], riff[7]]);
    let (name, _, padded) = riff_chunk(reader)?;
    if &name != b"VP8X" {
        return None;
    }
    let mut position = 20 + padded;
    let end = position + u64::from(riff_size).saturating_sub(12);
    while position < end {
        reader.seek(SeekFrom::Start(position)).ok()?;
        let (name, size, padded) = riff_chunk(reader)?;
        let start = position + 8;
        if &name == b"EXIF" {
            return Some(start..start + size);
        }
        position = start + padded;
    }
    None
}

/// The name of the RIFF chunk `reader` is at, its size, and that size padded
/// to an even one as the WebP decoder pads it, never past the largest a size
/// can be.
fn riff_chunk(reader: &mut impl Read) -> Option<([u8; 4], u64, u64)> {
    let mut header = [0u8; 8];
    reader.read_exact(&mut header).ok()?;
    let size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let name = [header[0], header[1], header[2], header[3]];
    Some((
        name,
        u64::from(size),
        u64::from(size.saturating_add(size & 1)),
    ))
}

/// How much of a WebP's EXIF is read to tell how it turns the image: the
/// entry says so in the first few hundred bytes.
const WEBP_EXIF_READ: u64 = 64 << 10;

/// How the EXIF of the WebP `reader` holds turns it, as its decoder reads
/// it, from no more than [`WEBP_EXIF_READ`] of it. One whose EXIF is longer,
/// and does not say within that, is taken to be turned a quarter, which
/// reckons the most memory.
fn webp_orientation(reader: &mut (impl Read + Seek)) -> Orientation {
    let Some(range) = webp_exif(reader) else {
        return Orientation::NoTransforms;
    };
    let length = range.end - range.start;
    let mut exif = Vec::new();
    let read = reader.seek(SeekFrom::Start(range.start)).is_ok()
        && reader.take(WEBP_EXIF_READ).read_to_end(&mut exif).is_ok();
    match Orientation::from_exif_chunk(&exif) {
        Some(orientation) => orientation,
        None if read && length > WEBP_EXIF_READ => Orientation::Rotate90,
        None => Orientation::NoTransforms,
    }
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
///
/// An image of no pixels across or down, which a GIF's canvas can be, shows
/// nothing, and is refused before anything is sized from it.
///
/// Its decoder is held to [`HEADER_MEMORY`] as it is made, as [`Footprint`]
/// reckons it: what a PNG's keeps of its chunks. A JPEG's copies the file,
/// which is reckoned too.
fn decode(bytes: &[u8]) -> image::ImageResult<(DynamicImage, Orientation)> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let format = reader.format();
    // A GIF's decoder collects its ICC profile and XMP as it is made, so one
    // too large to decode is refused before then. Its first frame is decoded
    // at its own size where it does not lie across the canvas, and is held to
    // the same limit.
    if format == Some(ImageFormat::Gif) {
        let too_large = gif_start(&mut Cursor::new(bytes)).is_some_and(|start| {
            u64::from(start.width) * u64::from(start.height) > MAX_PIXELS
                || start.frame.is_some_and(|frame| frame.pixels() > MAX_PIXELS)
        });
        if too_large {
            return Err(ImageError::Limits(LimitError::from_kind(
                LimitErrorKind::DimensionError,
            )));
        }
    }
    reader.limits(header_limits());
    let mut decoder = reader.into_decoder()?;
    // A tag that cannot be read leaves the image as it is stored, as a
    // reader would. A WebP's decoder would copy the whole of its EXIF, which
    // is read here where it lies.
    let orientation = if format == Some(ImageFormat::WebP) {
        webp_exif(&mut Cursor::new(bytes))
            .and_then(|range| {
                bytes.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
            })
            .and_then(Orientation::from_exif_chunk)
            .unwrap_or(Orientation::NoTransforms)
    } else {
        decoder.orientation().unwrap_or(Orientation::NoTransforms)
    };

    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 {
        return Err(ImageError::Decoding(DecodingError::new(
            format.map_or(ImageFormatHint::Unknown, ImageFormatHint::Exact),
            "the image has no pixels",
        )));
    }
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
            let frame = jpeg_frame(&mut Cursor::new(&jpeg)).unwrap();
            assert_eq!(
                jpeg_coefficients(&frame),
                expected,
                "{progressive} {sampling:?}"
            );
        }
    }

    /// A JPEG's header is read up to its scan, past comments and segments it
    /// does not need: its size, its components, and the turn its EXIF says.
    #[test]
    fn a_jpegs_header_is_read_past_what_it_does_not_need() {
        let rgb = vec![128u8; 40 * 30 * 3];
        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg, 80)
            .encode(&rgb, 40, 30, ColorType::Rgb)
            .unwrap();
        // EXIF in TIFF's little-endian form: one entry, an orientation of 6.
        let mut exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0".to_vec();
        exif.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0]);
        let segment = |marker: u8, body: &[u8]| {
            let mut segment = vec![0xFF, marker];
            segment.extend_from_slice(&(body.len() as u16 + 2).to_be_bytes());
            segment.extend_from_slice(body);
            segment
        };
        let mut file = jpeg[..2].to_vec();
        file.extend(segment(0xFE, &[b'c'; 1000]));
        // Bytes between segments, and a fill byte before a marker, which its
        // decoder passes over too.
        file.extend_from_slice(b"stray\xFF");
        file.extend(segment(0xE1, &exif));
        file.extend(segment(0xE2, &[0; 500]));
        file.extend_from_slice(&jpeg[2..]);

        let header = Header::read(Cursor::new(&file)).unwrap();
        assert_eq!((header.width, header.height), (40, 30));
        assert_eq!(header.color, image::ColorType::Rgb8);
        assert_eq!(header.orientation, Orientation::Rotate90);
        let decoder = ImageReader::new(Cursor::new(&file))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        assert_eq!(decoder.dimensions(), (40, 30));
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
