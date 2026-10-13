//! Decoding an image in a process of its own, held to a hard memory cap.
//!
//! A decoder reckons its memory from an image's pixels, but a crafted one can
//! make a decoder allocate far more than its size or its pixels suggest: a
//! lossless WebP names up to 65,536 Huffman groups, each of five tables, and
//! a few-megabyte file can ask a decoder for gigabytes. No limit the `image`
//! crate takes reaches that allocation.
//!
//! So the work that decodes an image — converting it, or making a thumbnail —
//! is done in a worker process, under [`CappedAllocator`], which refuses to
//! let it grow past the memory the pipeline set aside for it. A worker that
//! asks for more is killed by the allocator, and the image is left as it is;
//! the book, and the other images, are not touched.
//!
//! The same binary serves as its own worker: [`serve_if_asked`], called first
//! thing in `main`, turns the process into a worker when it is launched with
//! [`WORKER_ARG`]. A parent that has called [`enable`] runs each decode in a
//! worker; one that has not — a test, or the library used on its own — decodes
//! in its own process, with no cap.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use crate::image::{self, ImageOptions, ProcessedImage};
use crate::memory::MemoryBudget;
use crate::{Error, Result};

/// The argument that tells a copy of the binary to serve as a decode worker
/// rather than run as usual. It is checked before the command line is parsed,
/// and spelt so that no user would pass it by hand.
pub const WORKER_ARG: &str = "--epubkit-decode-worker-9f3c1a";

// --------------------------------------------------------------- the allocator

/// The system allocator, counting what is live and refusing to let it pass a
/// limit [`set_limit`] sets. A binary that decodes images in worker processes
/// installs it as its global allocator; the parent leaves the limit off, so
/// it only counts, while a worker sets the limit around each decode.
pub struct CappedAllocator;

/// What is live now, in bytes.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// The most that may be live. `usize::MAX` is no limit.
static LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);

// SAFETY: every method forwards to the system allocator for a layout of
// non-zero size, as `GlobalAlloc` requires, and only keeps a tally beside it.
// A pointer is returned null exactly when the system allocator returns null or
// the reservation would pass the limit; the tally is kept in step with what is
// actually held, so that a later limit is checked against the truth.
unsafe impl GlobalAlloc for CappedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !reserve(layout.size()) {
            return null_mut();
        }
        let pointer = System.alloc(layout);
        if pointer.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        System.dealloc(pointer, layout);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() && !reserve(new_size - layout.size()) {
            return null_mut();
        }
        let moved = System.realloc(pointer, layout, new_size);
        if moved.is_null() {
            if new_size > layout.size() {
                LIVE.fetch_sub(new_size - layout.size(), Ordering::Relaxed);
            }
        } else if new_size < layout.size() {
            LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
        }
        moved
    }
}

/// Add `size` to what is live, unless that would pass the limit, in which case
/// leave the tally as it was and refuse. A refused allocation returns null,
/// which makes the standard library abort the process — the worker dying is
/// how a decode that would overrun its cap is stopped.
fn reserve(size: usize) -> bool {
    let mut live = LIVE.load(Ordering::Relaxed);
    loop {
        let Some(new) = live.checked_add(size) else {
            return false;
        };
        if new > LIMIT.load(Ordering::Relaxed) {
            return false;
        }
        match LIVE.compare_exchange_weak(live, new, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(now) => live = now,
        }
    }
}

/// Hold everything the process allocates from here to [`clear_limit`] within
/// `cap` bytes more than is live now.
fn set_limit(cap: u64) {
    let live = LIVE.load(Ordering::Relaxed);
    let limit = usize::try_from(cap)
        .ok()
        .and_then(|cap| live.checked_add(cap))
        .unwrap_or(usize::MAX);
    LIMIT.store(limit, Ordering::Relaxed);
}

/// Take the limit off again.
fn clear_limit() {
    LIMIT.store(usize::MAX, Ordering::Relaxed);
}

// --------------------------------------------------------------- the parent

/// Where the worker binary is, once a parent has turned workers on.
static WORKER_EXE: OnceLock<PathBuf> = OnceLock::new();

/// Decode images in worker processes from now on, each a copy of the binary
/// at `exe`, which must serve as a worker when it is launched with
/// [`WORKER_ARG`] (see [`serve_if_asked`]). Called once, in the parent.
pub fn enable(exe: PathBuf) {
    let _ = WORKER_EXE.set(exe);
}

fn worker_exe() -> Option<&'static PathBuf> {
    WORKER_EXE.get()
}

thread_local! {
    /// The worker this thread sends its decodes to, spawned when first needed
    /// and replaced if it ever dies.
    static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
}

/// What converting an image came to.
pub enum Converted {
    /// Converted to these, a page or two.
    Done(Vec<ProcessedImage>),
    /// Not decoded: not an image after all, or one in a form the decoders do
    /// not read.
    Undecodable,
    /// The worker could not decode it within the memory set aside, and was
    /// stopped, or died before it could answer.
    Overran,
}

/// Convert the image in `bytes`, named `filename`, for `options`, holding the
/// work to `cap` bytes. In a worker where workers are on; in this process, with
/// no cap, where they are not.
pub fn convert(bytes: Vec<u8>, filename: &str, options: &ImageOptions, cap: u64) -> Converted {
    if worker_exe().is_none() {
        return match image::process_image(&bytes, filename, options) {
            Ok(outputs) => Converted::Done(outputs),
            Err(_) => Converted::Undecodable,
        };
    }
    with_worker(|worker| worker.convert(&bytes, filename, options, cap))
        .unwrap_or(Converted::Overran)
}

/// Make a thumbnail of `bytes` within `max_width` x `max_height`, out of
/// `budget`, as [`image::thumbnail`] does, but decoding it in a worker where
/// workers are on, so that an image crafted to make a decoder allocate wildly
/// cannot. One too large for the budget is refused before any worker is asked.
pub fn thumbnail(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    budget: &MemoryBudget,
) -> Result<Vec<u8>> {
    let needed = image::thumbnail_needed(bytes, budget);
    if needed > budget.total() {
        return Err(image::thumbnail_oversize_error(needed, budget.total()));
    }
    let _held = budget.hold(needed);
    if worker_exe().is_none() {
        return image::thumbnail_decode(bytes, max_width, max_height);
    }
    with_worker(|worker| worker.thumbnail(bytes, max_width, max_height, needed))
        .unwrap_or_else(|| Err(Error::Image("the worker could not make a thumbnail".into())))
}

/// Run `work` against this thread's worker, spawning one if there is none, and
/// dropping it if `work` fails so that the next call starts a fresh one.
/// `None` where no worker could be spawned or the one there died.
fn with_worker<T>(work: impl FnOnce(&mut Worker) -> io::Result<T>) -> Option<T> {
    WORKER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = worker_exe().and_then(|exe| Worker::spawn(exe).ok());
        }
        let worker = slot.as_mut()?;
        match work(worker) {
            Ok(value) => Some(value),
            Err(_) => {
                // A dead worker is dropped, which reaps it; the next call
                // spawns another.
                *slot = None;
                None
            }
        }
    })
}

/// A worker process this thread talks to over its pipes.
struct Worker {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

/// What a request asks the worker to do.
const OP_CONVERT: u8 = 1;
const OP_THUMBNAIL: u8 = 2;
/// How a worker answers.
const STATUS_OK: u8 = 0;
const STATUS_UNDECODABLE: u8 = 1;

impl Worker {
    fn spawn(exe: &PathBuf) -> io::Result<Worker> {
        let mut child = Command::new(exe)
            .arg(OsString::from(WORKER_ARG))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = std::io::BufReader::new(child.stdout.take().expect("piped"));
        Ok(Worker {
            child,
            stdin,
            stdout,
        })
    }

    fn convert(
        &mut self,
        bytes: &[u8],
        filename: &str,
        options: &ImageOptions,
        cap: u64,
    ) -> io::Result<Converted> {
        let options =
            toml::to_string(options).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.stdin.write_all(&[OP_CONVERT])?;
        self.stdin.write_all(&cap.to_le_bytes())?;
        write_block(&mut self.stdin, filename.as_bytes())?;
        write_block(&mut self.stdin, options.as_bytes())?;
        write_block(&mut self.stdin, bytes)?;
        self.stdin.flush()?;

        match read_status(&mut self.stdout)? {
            STATUS_OK => {
                let count = read_u32(&mut self.stdout)? as usize;
                let mut outputs = Vec::with_capacity(count.min(1024));
                for _ in 0..count {
                    outputs.push(read_processed_image(&mut self.stdout)?);
                }
                Ok(Converted::Done(outputs))
            }
            _ => Ok(Converted::Undecodable),
        }
    }

    fn thumbnail(
        &mut self,
        bytes: &[u8],
        max_width: u32,
        max_height: u32,
        cap: u64,
    ) -> io::Result<Result<Vec<u8>>> {
        self.stdin.write_all(&[OP_THUMBNAIL])?;
        self.stdin.write_all(&cap.to_le_bytes())?;
        self.stdin.write_all(&max_width.to_le_bytes())?;
        self.stdin.write_all(&max_height.to_le_bytes())?;
        write_block(&mut self.stdin, bytes)?;
        self.stdin.flush()?;

        match read_status(&mut self.stdout)? {
            STATUS_OK => Ok(Ok(read_block(&mut self.stdout)?)),
            _ => {
                let message = String::from_utf8_lossy(&read_block(&mut self.stdout)?).into_owned();
                Ok(Err(Error::Image(message)))
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Closing stdin lets a live worker exit; one that is already gone is
        // reaped. Either way, nothing is left running.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --------------------------------------------------------------- the worker

/// If this process was launched to serve as a decode worker, serve requests
/// until its input closes, then exit without returning. Called first thing in
/// `main`, before the program does anything else.
pub fn serve_if_asked() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(std::ffi::OsStr::new(WORKER_ARG)) {
        std::process::exit(serve());
    }
}

/// Serve one request after another until the input closes cleanly (`0`) or a
/// request cannot be read or answered (`1`). A decode that would overrun its
/// cap does not return here: the allocator stops the process first.
fn serve() -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = io::BufWriter::new(stdout.lock());
    loop {
        match serve_one(&mut input, &mut output) {
            Ok(true) => {}
            Ok(false) => return 0,
            Err(_) => return 1,
        }
    }
}

/// Read and answer one request. `Ok(false)` at a clean end of input.
fn serve_one(input: &mut impl Read, output: &mut impl Write) -> io::Result<bool> {
    let mut op = [0u8; 1];
    if input.read(&mut op)? == 0 {
        return Ok(false);
    }
    let cap = read_u64(input)?;
    match op[0] {
        OP_CONVERT => {
            let filename = read_block(input)?;
            let options = read_block(input)?;
            let bytes = read_block(input)?;
            let filename = String::from_utf8_lossy(&filename).into_owned();
            let options: ImageOptions = toml::from_str(&String::from_utf8_lossy(&options))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            set_limit(cap);
            let decoded = image::process_image(&bytes, &filename, &options);
            clear_limit();

            match decoded {
                Ok(outputs) => {
                    output.write_all(&[STATUS_OK])?;
                    write_u32(output, outputs.len() as u32)?;
                    for image in &outputs {
                        write_processed_image(output, image)?;
                    }
                }
                Err(_) => output.write_all(&[STATUS_UNDECODABLE])?,
            }
        }
        OP_THUMBNAIL => {
            let max_width = read_u32(input)?;
            let max_height = read_u32(input)?;
            let bytes = read_block(input)?;

            set_limit(cap);
            let made = image::thumbnail_decode(&bytes, max_width, max_height);
            clear_limit();

            match made {
                Ok(jpeg) => {
                    output.write_all(&[STATUS_OK])?;
                    write_block(output, &jpeg)?;
                }
                Err(error) => {
                    output.write_all(&[STATUS_UNDECODABLE])?;
                    write_block(output, error.to_string().as_bytes())?;
                }
            }
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown request",
            ))
        }
    }
    output.flush()?;
    Ok(true)
}

// --------------------------------------------------------------- framing

fn write_u32(output: &mut impl Write, value: u32) -> io::Result<()> {
    output.write_all(&value.to_le_bytes())
}

fn write_u64(output: &mut impl Write, value: u64) -> io::Result<()> {
    output.write_all(&value.to_le_bytes())
}

fn write_block(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    write_u64(output, bytes.len() as u64)?;
    output.write_all(bytes)
}

fn write_processed_image(output: &mut impl Write, image: &ProcessedImage) -> io::Result<()> {
    write_block(output, image.filename.as_bytes())?;
    write_u64(output, image.original_size as u64)?;
    write_u64(output, image.new_size as u64)?;
    write_block(output, image.details.as_bytes())?;
    write_block(output, image.conversion.as_bytes())?;
    output.write_all(&[u8::from(image.reshaped)])?;
    write_block(output, &image.bytes)
}

fn read_status(input: &mut impl Read) -> io::Result<u8> {
    let mut status = [0u8; 1];
    input.read_exact(&mut status)?;
    Ok(status[0])
}

fn read_u32(input: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(input: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    input.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_block(input: &mut impl Read) -> io::Result<Vec<u8>> {
    let length = read_u64(input)?;
    let length =
        usize::try_from(length).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    let mut bytes = vec![0u8; length];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_processed_image(input: &mut impl Read) -> io::Result<ProcessedImage> {
    let filename = String::from_utf8_lossy(&read_block(input)?).into_owned();
    let original_size = read_u64(input)? as usize;
    let new_size = read_u64(input)? as usize;
    let details = String::from_utf8_lossy(&read_block(input)?).into_owned();
    let conversion = String::from_utf8_lossy(&read_block(input)?).into_owned();
    let mut reshaped = [0u8; 1];
    input.read_exact(&mut reshaped)?;
    let bytes = read_block(input)?;
    Ok(ProcessedImage {
        bytes,
        filename,
        original_size,
        new_size,
        details,
        conversion,
        reshaped: reshaped[0] != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A processed image survives being written to the pipe and read back,
    /// field for field.
    #[test]
    fn a_processed_image_round_trips_through_the_pipe() {
        let image = ProcessedImage {
            bytes: vec![1, 2, 3, 0, 255, 128],
            filename: "a plate.jpg".into(),
            original_size: 4096,
            new_size: 321,
            details: "kept a quarter turn".into(),
            conversion: "PNG→JPEG".into(),
            reshaped: true,
        };
        let mut pipe = Vec::new();
        write_processed_image(&mut pipe, &image).unwrap();
        let read = read_processed_image(&mut Cursor::new(&pipe)).unwrap();

        assert_eq!(read.bytes, image.bytes);
        assert_eq!(read.filename, image.filename);
        assert_eq!(read.original_size, image.original_size);
        assert_eq!(read.new_size, image.new_size);
        assert_eq!(read.details, image.details);
        assert_eq!(read.conversion, image.conversion);
        assert_eq!(read.reshaped, image.reshaped);
    }

    /// A block of bytes, empty or not, round-trips whole.
    #[test]
    fn a_block_round_trips_through_the_pipe() {
        for block in [Vec::new(), vec![0u8; 1000], b"WEBP".to_vec()] {
            let mut pipe = Vec::new();
            write_block(&mut pipe, &block).unwrap();
            assert_eq!(read_block(&mut Cursor::new(&pipe)).unwrap(), block);
        }
    }
}
