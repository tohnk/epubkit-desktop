//! The app decodes every image, converting a book's and making the book
//! list's cover thumbnails, in a copy of its own binary launched as a worker
//! and held to a memory cap. That copy must serve before the app would start:
//! if it went on to start the app instead, no worker would ever answer, and
//! every image would be left as it is. These drive the app's own binary.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use epubkit_core::image::ImageOptions;
use epubkit_core::worker::{self, Converted, WORKER_ARG};

const APP: &str = env!("CARGO_BIN_EXE_epubkit-desktop");

/// A 64 x 48 BMP of a gradient, a format simple enough to write by hand.
fn bmp() -> Vec<u8> {
    let (width, height) = (64u32, 48u32);
    let mut bmp = b"BM".to_vec();
    bmp.extend_from_slice(&(54 + width * height * 3).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&width.to_le_bytes());
    bmp.extend_from_slice(&height.to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&24u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    for y in 0..height {
        for x in 0..width {
            bmp.extend_from_slice(&[(x * 4) as u8, (y * 5) as u8, 128]);
        }
    }
    bmp
}

/// Launched as a worker with nothing to do, the app's binary exits at once,
/// without starting the app; one that does not is stopped, rather than left
/// to hang the test. Then, through copies of it as the app runs them, an
/// image converts and a cover thumbnail is made.
#[test]
fn the_app_serves_as_its_own_worker() {
    let mut idle = Command::new(APP)
        .arg(WORKER_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let exited = loop {
        if let Some(status) = idle.try_wait().unwrap() {
            break Some(status);
        }
        if started.elapsed() > Duration::from_secs(20) {
            idle.kill().ok();
            idle.wait().ok();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let exited = exited.expect("launched as a worker, the app went on to start");
    assert!(exited.success(), "the idle worker exited with {exited}");

    worker::enable(APP.into());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plate.bmp");
    let bytes = bmp();
    std::fs::write(&path, &bytes).unwrap();
    let options = ImageOptions::default();
    let cap = epubkit_core::image::memory_needed(&path, &options).unwrap();
    let Converted::Done(outputs) = worker::convert(&path, "plate.bmp", &options, cap).unwrap()
    else {
        panic!("the image did not convert in a worker");
    };
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].bytes[..2], [0xFF, 0xD8], "not a JPEG");

    let budget = epubkit_core::memory::MemoryBudget::new(1 << 30);
    let thumbnail = worker::thumbnail(&bytes, 120, 160, &budget).unwrap();
    assert_eq!(thumbnail[..2], [0xFF, 0xD8], "not a JPEG");
}
