//! What reading and converting an image holds, counted by an allocator that
//! keeps a tally. Tests here run one at a time, so the tally is theirs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use epubkit_core::image::{
    is_raster_image, memory_needed, process_image, thumbnail, ImageOptions, HEADER_MEMORY,
};
use epubkit_core::memory::MemoryBudget;
use image::ImageEncoder;

/// The system's allocator, counting what is live and the most that was.
struct Tally;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let live = LIVE.fetch_add(by, Ordering::SeqCst) + by;
    PEAK.fetch_max(live, Ordering::SeqCst);
}

unsafe impl GlobalAlloc for Tally {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc(layout);
        if !pointer.is_null() {
            grew(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        System.dealloc(pointer, layout);
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let moved = System.realloc(pointer, layout, size);
        if !moved.is_null() {
            if size > layout.size() {
                grew(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, Ordering::SeqCst);
            }
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Tally = Tally;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// What `work` returns, and the most it held at once beyond what was held
/// before it.
fn peak_of<T>(work: impl FnOnce() -> T) -> (T, usize) {
    let before = LIVE.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    let done = work();
    (done, PEAK.load(Ordering::SeqCst) - before)
}

/// A JPEG of 16 x 16 pixels with `extra` bytes of comments after its start,
/// which a decoder passes over.
fn jpeg_with_comments(extra: usize) -> Vec<u8> {
    let mut jpeg = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        16,
        16,
        image::Rgb([90, 120, 150]),
    ))
    .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
    .unwrap();

    let mut out = jpeg[..2].to_vec();
    let mut left = extra;
    while left > 0 {
        let body = left.min(65533);
        out.extend_from_slice(&[0xFF, 0xFE]);
        out.extend_from_slice(&(body as u16 + 2).to_be_bytes());
        out.resize(out.len() + body, b'c');
        left -= body;
    }
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// A PNG of one pixel whose ICC profile is `size` bytes, which pack small.
fn png_with_profile(size: usize) -> Vec<u8> {
    let mut png = Vec::new();
    let mut encoder = image::codecs::png::PngEncoder::new(&mut png);
    encoder.set_icc_profile(vec![0; size]).unwrap();
    encoder
        .write_image(&[200, 100, 50], 1, 1, image::ExtendedColorType::Rgb8)
        .unwrap();
    png
}

/// Reckoning what converting an image takes, or whether it is one, reads its
/// header, and no more. A JPEG's decoder copied the whole file as it was made,
/// comments it passes over and all, and a PNG's unpacked its ICC profile, all
/// before anything was set aside for either.
#[test]
fn reading_a_header_holds_little_of_the_file() {
    let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let jpeg = dir.path().join("comments.jpg");
    std::fs::write(&jpeg, jpeg_with_comments(8 << 20)).unwrap();
    let png = dir.path().join("profile.png");
    std::fs::write(&png, png_with_profile(64 << 20)).unwrap();
    let options = ImageOptions::default();

    for (path, most) in [(&jpeg, 1 << 20), (&png, HEADER_MEMORY as usize)] {
        let (needed, peak) = peak_of(|| memory_needed(path, &options));
        assert!(needed > std::fs::metadata(path).unwrap().len());
        assert!(peak <= most, "{}: reckoning held {peak}", path.display());
        let (raster, peak) = peak_of(|| is_raster_image(path));
        assert!(raster);
        assert!(peak <= most, "{}: telling held {peak}", path.display());
    }
}

/// Converting an image holds no more than was set aside for it: the file, and
/// what decoding and converting it hold, a JPEG decoder's copy of the file
/// and what a PNG decoder keeps of its chunks among it.
#[test]
fn converting_an_image_holds_no_more_than_was_set_aside() {
    let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    for (name, bytes) in [
        ("comments.jpg", jpeg_with_comments(48 << 20)),
        ("profile.png", png_with_profile(64 << 20)),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, &bytes).unwrap();
        drop(bytes);
        for light_novel_mode in [false, true] {
            let options = ImageOptions {
                light_novel_mode,
                ..ImageOptions::default()
            };
            let needed = memory_needed(&path, &options);
            let file = std::fs::read(&path).unwrap();
            let (converted, peak) = peak_of(|| process_image(&file, name, &options));
            assert!(converted.is_ok(), "{name}");
            let held = (file.len() + peak) as u64;
            assert!(held <= needed, "{name}: {held} held, {needed} set aside");
        }
    }
}

/// `body` as a RIFF chunk named `fourcc`, padded to an even length.
fn chunk(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut chunk = fourcc.to_vec();
    chunk.extend_from_slice(&(body.len() as u32).to_le_bytes());
    chunk.extend_from_slice(body);
    if body.len() % 2 == 1 {
        chunk.push(0);
    }
    chunk
}

/// An extended WebP of 2 x 1 pixels whose EXIF, `size` bytes of it, says in
/// its first entry to turn it a quarter.
fn webp_with_exif(size: usize) -> Vec<u8> {
    let mut simple = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut simple)
        .write_image(
            &[200, 100, 50, 10, 20, 30],
            2,
            1,
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    // TIFF's little-endian form: one entry, an orientation of 6.
    let mut exif = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0".to_vec();
    exif.resize(size, 0);
    // EXIF present; a canvas of 2 x 1.
    let header = [0x08, 0, 0, 0, 1, 0, 0, 0, 0, 0];

    let mut body = b"WEBP".to_vec();
    body.extend(chunk(b"VP8X", &header));
    // The image's own chunk, after RIFF, its size and WEBP.
    body.extend_from_slice(&simple[12..]);
    body.extend(chunk(b"EXIF", &exif));
    let mut riff = b"RIFF".to_vec();
    riff.extend_from_slice(&(body.len() as u32).to_le_bytes());
    riff.extend(body);
    riff
}

/// A WebP's decoder reads its whole EXIF chunk to say how the image is
/// turned, held to no limit. A WebP of two pixels with 48 MiB of EXIF held
/// 48 MiB to be reckoned, and converting it held 96 MiB where 80 were set
/// aside. Its EXIF is read now no further than it has to be, and it comes out
/// turned all the same.
#[test]
fn a_webps_exif_is_read_no_further_than_its_turn() {
    let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("exif.webp");
    std::fs::write(&path, webp_with_exif(48 << 20)).unwrap();
    let options = ImageOptions::default();

    let (needed, peak) = peak_of(|| memory_needed(&path, &options));
    assert!(peak <= HEADER_MEMORY as usize, "reckoning held {peak}");
    let (raster, peak) = peak_of(|| is_raster_image(&path));
    assert!(raster);
    assert!(peak <= HEADER_MEMORY as usize, "telling held {peak}");

    let file = std::fs::read(&path).unwrap();
    let (converted, peak) = peak_of(|| process_image(&file, "exif.webp", &options));
    let held = (file.len() + peak) as u64;
    assert!(held <= needed, "{held} held, {needed} set aside");
    let page = image::load_from_memory(&converted.unwrap()[0].bytes).unwrap();
    assert_eq!((page.width(), page.height()), (1, 2));
}

/// A GIF whose canvas is `canvas` and whose one frame, of one colour, is
/// `frame`, each width by height: an encoder's GIF of the frame's size, its
/// canvas then made smaller.
fn gif_with_frame(canvas: (u16, u16), frame: (u16, u16)) -> Vec<u8> {
    let mut gif = Vec::new();
    let pixels = usize::from(frame.0) * usize::from(frame.1);
    image::codecs::gif::GifEncoder::new(&mut gif)
        .encode(
            &[90, 120, 150, 255].repeat(pixels),
            u32::from(frame.0),
            u32::from(frame.1),
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    gif[6..8].copy_from_slice(&canvas.0.to_le_bytes());
    gif[8..10].copy_from_slice(&canvas.1.to_le_bytes());
    gif
}

/// A GIF's decoder decodes a frame that does not lie across its canvas in
/// RGBA of the frame's own size. One of 4000 x 3000 on a canvas of one pixel
/// was reckoned at the canvas's size: 32 MiB set aside, 57 MiB held, and a
/// thumbnail of it made past a budget too small for it.
#[test]
fn a_gif_frame_larger_than_its_canvas_is_reckoned_at_its_own_size() {
    let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("frame.gif");
    std::fs::write(&path, gif_with_frame((1, 1), (4000, 3000))).unwrap();
    let options = ImageOptions::default();

    let needed = memory_needed(&path, &options);
    let file = std::fs::read(&path).unwrap();
    let (converted, peak) = peak_of(|| process_image(&file, "frame.gif", &options));
    assert!(converted.is_ok());
    let held = (file.len() + peak) as u64;
    assert!(held <= needed, "{held} held, {needed} set aside");

    let budget = MemoryBudget::new(40 << 20);
    let (made, peak) = peak_of(|| thumbnail(&file, 480, 720, &budget));
    assert!(
        made.is_err() || peak <= 40 << 20,
        "a thumbnail held {peak} of 40 MiB"
    );
}

/// A lossless WebP without alpha is decoded in RGBA beside the RGB it comes
/// out as, where a byte a pixel was reckoned for it: one of 2000 x 10000
/// held more than was set aside.
#[test]
fn an_opaque_lossless_webp_is_reckoned_with_its_rgba() {
    let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opaque.webp");
    let (width, height) = (2000u32, 10000u32);
    let mut webp = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
        .write_image(
            &[90, 120, 150].repeat((width * height) as usize),
            width,
            height,
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    std::fs::write(&path, webp).unwrap();
    let options = ImageOptions::default();

    let needed = memory_needed(&path, &options);
    let file = std::fs::read(&path).unwrap();
    let (converted, peak) = peak_of(|| process_image(&file, "opaque.webp", &options));
    assert!(converted.is_ok());
    let held = (file.len() + peak) as u64;
    assert!(held <= needed, "{held} held, {needed} set aside");
}
