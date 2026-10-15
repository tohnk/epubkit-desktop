//! The IPC surface.
//!
//! Commands are deliberately thin. Anything that decides behaviour — what a
//! preset means, what the pipeline does, how a filename is derived — lives in
//! `epubkit-core` so the CLI and the window cannot drift apart.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use epubkit_core::memory::MemoryBudget;
use epubkit_core::metadata::{self, MetadataEdits};
use epubkit_core::pipeline::{process_epub_stoppable, ProcessingReport};
use epubkit_core::settings::Settings;
use epubkit_core::{image, package, preview, worker, Error};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Runtime};

/// Commands report failure as a string; the page has no use for a typed error.
type Response<T> = Result<T, String>;

fn to_message(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn settings_path() -> Response<PathBuf> {
    Settings::default_path().ok_or_else(|| "could not locate a configuration directory".to_string())
}

// ------------------------------------------------------------------ settings

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub id: String,
    pub label: String,
    pub width: u32,
    pub height: u32,
    pub gray_levels: usize,
}

#[tauri::command]
pub fn devices() -> Vec<DeviceInfo> {
    image::DEVICES
        .iter()
        .map(|device| DeviceInfo {
            id: device.id.to_string(),
            label: device.label.to_string(),
            width: device.width,
            height: device.height,
            gray_levels: device.gray_levels.len(),
        })
        .collect()
}

#[tauri::command]
pub fn load_settings() -> Response<Settings> {
    Settings::load(&settings_path()?).map_err(to_message)
}

#[tauri::command]
pub fn save_settings(settings: Settings) -> Response<()> {
    settings.save(&settings_path()?).map_err(to_message)
}

/// Apply a preset and persist the result, returning the new state.
///
/// The page does not compute what a preset means — it asks for one by name and
/// renders whatever comes back.
#[tauri::command]
pub fn select_preset(id: String) -> Response<Settings> {
    let path = settings_path()?;
    let mut settings = Settings::load(&path).map_err(to_message)?;

    settings.select(&id).map_err(to_message)?;
    settings.save(&path).map_err(to_message)?;

    Ok(settings)
}

#[tauri::command]
pub fn save_preset(name: String, settings: Settings) -> Response<Settings> {
    let path = settings_path()?;
    let mut settings = settings;

    settings.save_preset(&name).map_err(to_message)?;
    settings.save(&path).map_err(to_message)?;

    Ok(settings)
}

#[tauri::command]
pub fn delete_preset(id: String) -> Response<Settings> {
    let path = settings_path()?;
    let mut settings = Settings::load(&path).map_err(to_message)?;

    settings.delete_preset(&id).map_err(to_message)?;
    settings.save(&path).map_err(to_message)?;

    Ok(settings)
}

/// What a custom filename template makes of an example book, or what is wrong
/// with it — so a mistake shows while it is typed, not when every book fails.
#[tauri::command]
pub fn check_filename_template(template: String) -> Response<String> {
    metadata::check_template(&template).map_err(to_message)
}

// -------------------------------------------------------------------- books

/// What the file list shows for one book before anything is done to it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookInfo {
    pub path: String,
    pub filename: String,
    pub size: u64,
    pub title: String,
    pub author: String,
    pub series: String,
    /// The book's own cover, as a data URL, for the preview thumbnail.
    pub cover: Option<String>,
    /// Set when the book cannot be processed; the rest of the fields are then
    /// best-effort.
    pub error: Option<String>,
}

impl BookInfo {
    fn failed(path: &Path, error: impl std::fmt::Display) -> Self {
        Self {
            path: path.to_string_lossy().to_string(),
            filename: file_name(path),
            size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            title: String::new(),
            author: String::new(),
            series: String::new(),
            cover: None,
            error: Some(error.to_string()),
        }
    }
}

/// The books among what was dropped on the window: each EPUB itself, and
/// those in each folder and the folders in it, linked in or not, folder by
/// folder in the order a listing gives them; a link back up the tree is not
/// followed. What is hidden is left out, a folder or a file: on a drive it
/// does not format, an SD card say, macOS writes a `._` file beside each
/// book, and it too ends in `.epub`.
///
/// Runs on a blocking worker: a library takes a while to walk.
#[tauri::command]
pub async fn find_books(paths: Vec<String>) -> Response<Found> {
    tauri::async_runtime::spawn_blocking(move || {
        let is_epub = |path: &Path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("epub"))
        };
        let hidden = |entry: &walkdir::DirEntry| {
            entry.depth() > 0 && entry.file_name().to_string_lossy().starts_with('.')
        };

        let mut found = Found::default();
        for path in paths.iter().map(Path::new) {
            if path.is_dir() {
                let entries = walkdir::WalkDir::new(path)
                    .follow_links(true)
                    .sort_by(|a, b| {
                        let name = |entry: &walkdir::DirEntry| {
                            entry.file_name().to_string_lossy().to_lowercase()
                        };
                        name(a).cmp(&name(b))
                    })
                    .into_iter()
                    .filter_entry(|entry| !hidden(entry));
                for entry in entries {
                    match entry {
                        Ok(entry) => {
                            if is_epub(entry.path()) && entry.path().is_file() {
                                found.books.push(entry.path().to_string_lossy().to_string());
                            }
                        }
                        // A link back up the tree is no folder to look in.
                        Err(error) if error.loop_ancestor().is_some() => {}
                        Err(error) => found
                            .unreadable
                            .push(error.path().unwrap_or(path).to_string_lossy().to_string()),
                    }
                }
            } else if is_epub(path) && path.is_file() {
                found.books.push(path.to_string_lossy().to_string());
            }
        }
        found
    })
    .await
    .map_err(to_message)
}

/// What was dropped on the window holds.
#[derive(Serialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    /// The books, in the order they were found.
    pub books: Vec<String>,
    /// The folders that could not be looked in. Left out with nothing said,
    /// a drop of only such a folder read as one with no books in it.
    pub unreadable: Vec<String>,
}

/// Read metadata and a cover thumbnail for each dropped book.
///
/// Runs on a blocking worker: a command without `async` runs on the main
/// thread, and the window would hang for as long as the books took to read.
#[tauri::command]
pub async fn inspect_books(paths: Vec<String>) -> Response<Vec<BookInfo>> {
    // Making each cover's thumbnail is most of the time a drop takes, so the
    // books are read a few at a time.
    tauri::async_runtime::spawn_blocking(move || {
        map_in_parallel(&paths, |path| {
            let path = Path::new(path);
            unless_it_panics(|| inspect_one(path))
                .unwrap_or_else(|message| BookInfo::failed(path, message))
        })
    })
    .await
    .map_err(to_message)
}

fn inspect_one(path: &Path) -> BookInfo {
    // This is for a thumbnail, not the artwork.
    const MAX_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;

    if !path.is_file() {
        return BookInfo::failed(path, "not a file");
    }

    match package::has_drm(path) {
        Ok(true) => return BookInfo::failed(path, Error::DrmProtected),
        Err(error) => return BookInfo::failed(path, error),
        Ok(false) => {}
    }

    // Only the package document and the cover are read; the rest of the book
    // stays packed.
    let preview = match preview::read_preview(path, MAX_PREVIEW_BYTES) {
        Ok(preview) => preview,
        Err(error) => return BookInfo::failed(path, error),
    };

    BookInfo {
        path: path.to_string_lossy().to_string(),
        filename: file_name(path),
        size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        title: preview.metadata.title,
        author: preview.metadata.author,
        series: preview.metadata.series,
        cover: preview.cover.as_ref().and_then(cover_data_url),
        error: None,
    }
}

/// `work` done for each of `items`, on as many threads as the machine runs at
/// once, the results in the order of `items`.
fn map_in_parallel<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(items.len());
    if threads <= 1 {
        return items.iter().map(work).collect();
    }

    let next = AtomicUsize::new(0);
    let mut results: Vec<Option<R>> = items.iter().map(|_| None).collect();
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(index) else {
                            break done;
                        };
                        done.push((index, work(item)));
                    }
                })
            })
            .collect();
        for worker in workers {
            let done = worker
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            for (index, result) in done {
                results[index] = Some(result);
            }
        }
    });
    results
        .into_iter()
        .map(|result| result.expect("every item was worked on"))
        .collect()
}

/// A book to process, with any per-book metadata edits the user typed.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub path: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Progress {
    path: String,
    index: usize,
    total: usize,
    percent: u8,
    message: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub path: String,
    pub output: Option<String>,
    pub summary: String,
    pub report: Option<ProcessingReport>,
    pub error: Option<String>,
    /// Not made because the run was stopped, whether partway through the
    /// book or before it was begun. `error` says which.
    pub stopped: bool,
}

impl Outcome {
    fn failed(job: &Job, error: impl std::fmt::Display) -> Self {
        Self {
            path: job.path.clone(),
            output: None,
            summary: String::new(),
            report: None,
            error: Some(error.to_string()),
            stopped: false,
        }
    }

    fn stopped(job: &Job, why: impl std::fmt::Display) -> Self {
        Self {
            stopped: true,
            ..Self::failed(job, why)
        }
    }
}

/// Whether books are being optimized, and what has been asked of the run.
pub struct RunState {
    /// Set to stop the run under way at its next step.
    stop: AtomicBool,
    /// Set while books are being optimized.
    running: AtomicBool,
    /// Set when the window was closed during a run, to close once it stops.
    close_when_stopped: AtomicBool,
}

impl RunState {
    pub const fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            running: AtomicBool::new(false),
            close_when_stopped: AtomicBool::new(false),
        }
    }

    /// A run begins, forgetting any stop told to the last one. Gives the flag
    /// that stops it.
    pub fn begin(&self) -> &AtomicBool {
        self.stop.store(false, Ordering::SeqCst);
        self.running.store(true, Ordering::SeqCst);
        &self.stop
    }

    /// Stop the run under way.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Whether the window, asked to close, must wait. During a run it stops
    /// the run, and closes once that has stopped, so the run takes the files
    /// it was making with it, as it cannot if the program simply ends. Asked
    /// again meanwhile, it closes there and then.
    pub fn stop_before_closing(&self) -> bool {
        if self.running.load(Ordering::SeqCst)
            && !self.close_when_stopped.swap(true, Ordering::SeqCst)
        {
            self.stop();
            return true;
        }
        false
    }

    /// Stop the run under way, if there is one, and wait up to `patience` for
    /// it to have stopped, and taken the files it was making with it. Gives
    /// whether it has.
    pub fn stop_and_wait(&self, patience: Duration) -> bool {
        if !self.running.load(Ordering::SeqCst) {
            return true;
        }
        self.stop();
        let given_up = Instant::now() + patience;
        while self.running.load(Ordering::SeqCst) {
            if Instant::now() >= given_up {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// The run is over: whether the window is to close now, as it was asked
    /// to during the run. The run is marked over first, so that the close
    /// this asks for is not taken for one during a run, and put off again.
    pub fn end(&self) -> bool {
        self.running.store(false, Ordering::SeqCst);
        self.close_when_stopped.swap(false, Ordering::SeqCst)
    }
}

impl Default for RunState {
    fn default() -> Self {
        Self::new()
    }
}

/// The window's run.
static RUN: RunState = RunState::new();

/// Stop the run under way: the book being made is not written, and no other
/// is begun.
#[tauri::command]
pub fn stop_run() {
    RUN.stop();
}

/// Whether the window, asked to close, must wait for the run to stop: see
/// [`RunState::stop_before_closing`]. When it must, the page is told, so it
/// says the window will go once the run has stopped; it said nothing, and a
/// second close, taken for impatience, ended the program there and then.
pub fn stop_before_closing<R: Runtime>(app: &AppHandle<R>) -> bool {
    if RUN.stop_before_closing() {
        let _ = app.emit("closing", ());
        return true;
    }
    false
}

/// For an app ending with no say in it, quit from the Dock say: stop the run
/// under way, and wait for it, a while, to take its files with it.
pub fn stop_before_exiting() {
    RUN.stop_and_wait(Duration::from_secs(10));
}

/// Optimize a list of books into `destination`, streaming progress as it goes.
///
/// Runs on a blocking worker so the window stays responsive; each book emits
/// `progress` events and the whole run resolves with one outcome per book. A
/// book that fails does not stop the rest — the outcome carries the error.
#[tauri::command]
pub async fn optimize_books(
    app: AppHandle,
    jobs: Vec<Job>,
    destination: String,
    settings: Settings,
) -> Response<Vec<Outcome>> {
    tauri::async_runtime::spawn_blocking(move || {
        let stop = RUN.begin();
        let total = jobs.len();
        let outcomes = optimize_all(
            &jobs,
            Path::new(&destination),
            &settings,
            stop,
            |index, percent, message| {
                let _ = app.emit(
                    "progress",
                    Progress {
                        path: jobs[index].path.clone(),
                        index,
                        total,
                        percent,
                        message: message.to_string(),
                    },
                );
            },
            |outcome| {
                let _ = app.emit("finished", outcome.clone());
            },
        );
        if RUN.end() {
            app.exit(0);
        }
        Ok(outcomes)
    })
    .await
    .map_err(to_message)?
}

/// Optimize each of `jobs` into `destination`, as [`optimize_books`] does,
/// telling `progress` how the book at each index goes and `finished` how
/// each went. A book that fails does not stop the rest. Once `stop` is set,
/// the book being made is not written and no other is begun, and each says
/// it was stopped.
///
/// Apart so it can be tested without a window.
pub fn optimize_all(
    jobs: &[Job],
    destination: &Path,
    settings: &Settings,
    stop: &AtomicBool,
    mut progress: impl FnMut(usize, u8, &str),
    mut finished: impl FnMut(&Outcome),
) -> Vec<Outcome> {
    let mut outcomes = Vec::with_capacity(jobs.len());
    for (index, job) in jobs.iter().enumerate() {
        let outcome = if stop.load(Ordering::SeqCst) {
            Outcome::stopped(job, "not begun: the run was stopped")
        } else {
            let progress = |percent: u8, message: &str| progress(index, percent, message);
            optimize_one(job, destination, settings, progress, stop)
        };
        finished(&outcome);
        outcomes.push(outcome);
    }
    outcomes
}

/// Optimize one book into `destination`, as [`optimize_all`] does each,
/// stopping once `stop` is set.
pub fn optimize_one(
    job: &Job,
    destination: &Path,
    settings: &Settings,
    progress: impl FnMut(u8, &str),
    stop: &AtomicBool,
) -> Outcome {
    let input = PathBuf::from(&job.path);
    let options = settings.processing_options(MetadataEdits {
        title: job.title.clone().filter(|value| !value.trim().is_empty()),
        author: job.author.clone().filter(|value| !value.trim().is_empty()),
        language: None,
    });

    // The output name comes from the book's metadata, which is not known until
    // the run finishes — so write beside the destination and move the book
    // into place once the report says what to call it. The staging file is
    // this run's own, and goes with it unless the book is published.
    let staging = match metadata::staging_file(destination) {
        Ok(staging) => staging,
        Err(error) => return Outcome::failed(job, error),
    };

    let run = unless_it_panics(|| {
        process_epub_stoppable(&input, staging.path(), &options, progress, stop)
    });
    let report = match run {
        Ok(Ok(report)) => report,
        Ok(Err(Error::Stopped)) => return Outcome::stopped(job, Error::Stopped),
        Ok(Err(error)) => return Outcome::failed(job, error),
        Err(panic) => return Outcome::failed(job, panic),
    };

    let final_path = match metadata::publish(staging, &destination.join(&report.output_filename)) {
        Ok(path) => path,
        Err(error) => return Outcome::failed(job, error),
    };

    Outcome {
        path: job.path.clone(),
        output: Some(final_path.to_string_lossy().to_string()),
        summary: report.summary(),
        report: Some(report),
        error: None,
        stopped: false,
    }
}

// ------------------------------------------------------------------ helpers

/// What `work` gives, or if it panics, what went wrong. A panic is a bug, but
/// one book's: it went up through every book read or run with it, and the
/// window learned of none of them.
fn unless_it_panics<R>(work: impl FnOnce() -> R) -> Result<R, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).map_err(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|message| message.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        format!("internal error: {message}")
    })
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Encode a cover for display.
///
/// The list shows a cover at a few dozen pixels, so it goes as a thumbnail:
/// sent whole, every cover in a batch of books was held by the page as a
/// data URL and again decoded at full size. A cover that cannot be made into
/// one, too large say, or in a format not read here, is not sent at all,
/// for the page would only be asked to decode what was refused here.
fn cover_data_url(cover: &preview::Cover) -> Option<String> {
    // Plenty for the list's 52 x 72 at any screen density, and about the
    // width of the band a narrow window shows a cover in.
    const THUMBNAIL: (u32, u32) = (480, 720);
    // Shared by every book being read, however many are dropped at once.
    static PREVIEW_MEMORY: MemoryBudget = MemoryBudget::new(512 << 20);

    // The cover is the book's to make of what it likes. Decoding it in a
    // worker held to a memory cap leaves one that is crafted to make a decoder
    // allocate wildly, or that simply will not decode, as this one book
    // without a cover, not every book dropped with it, and not the app.
    let thumbnail =
        worker::thumbnail(&cover.bytes, THUMBNAIL.0, THUMBNAIL.1, &PREVIEW_MEMORY).ok()?;
    Some(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(thumbnail)
    ))
}
