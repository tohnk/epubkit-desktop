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
