//! The IPC surface.
//!
//! Commands are deliberately thin. Anything that decides behaviour — what a
//! preset means, what the pipeline does, how a filename is derived — lives in
//! `epubkit-core` so the CLI and the window cannot drift apart.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine;
use epubkit_core::memory::MemoryBudget;
use epubkit_core::metadata::{self, MetadataEdits};
use epubkit_core::pipeline::{process_epub, ProcessingReport};
use epubkit_core::settings::Settings;
use epubkit_core::{image, package, preview, Error};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

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

/// Read metadata and a cover thumbnail for each dropped book.
///
/// Runs on a blocking worker: a command without `async` runs on the main
/// thread, and the window would hang for as long as the books took to read.
#[tauri::command]
pub async fn inspect_books(paths: Vec<String>) -> Response<Vec<BookInfo>> {
    // Making each cover's thumbnail is most of the time a drop takes, so the
    // books are read a few at a time.
    tauri::async_runtime::spawn_blocking(move || {
        map_in_parallel(&paths, |path| inspect_one(Path::new(path)))
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

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub path: String,
    pub output: Option<String>,
    pub summary: String,
    pub report: Option<ProcessingReport>,
    pub error: Option<String>,
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
        let destination = PathBuf::from(destination);
        let total = jobs.len();
        let mut outcomes = Vec::with_capacity(total);

        for (index, job) in jobs.iter().enumerate() {
            let emit = |percent: u8, message: &str| {
                let _ = app.emit(
                    "progress",
                    Progress {
                        path: job.path.clone(),
                        index,
                        total,
                        percent,
                        message: message.to_string(),
                    },
                );
            };

            let outcome = optimize_one(job, &destination, &settings, emit);

            let _ = app.emit("finished", outcome.clone());
            outcomes.push(outcome);
        }

        Ok(outcomes)
    })
    .await
    .map_err(to_message)?
}

/// Optimize one book into `destination`, as [`optimize_books`] does for each.
///
/// Apart so it can be tested without a window.
pub fn optimize_one(
    job: &Job,
    destination: &Path,
    settings: &Settings,
    progress: impl FnMut(u8, &str),
) -> Outcome {
    let input = PathBuf::from(&job.path);
    let options = settings.processing_options(MetadataEdits {
        title: job.title.clone().filter(|value| !value.trim().is_empty()),
        author: job.author.clone().filter(|value| !value.trim().is_empty()),
        language: None,
    });

    let failed = |error: String| Outcome {
        path: job.path.clone(),
        output: None,
        summary: String::new(),
        report: None,
        error: Some(error),
    };

    // The output name comes from the book's metadata, which is not known until
    // the run finishes — so write beside the destination and move the book
    // into place once the report says what to call it. The staging file is
    // this run's own, and goes with it unless the book is published.
    let staging = match metadata::staging_file(destination) {
        Ok(staging) => staging,
        Err(error) => return failed(error.to_string()),
    };

    let report = match process_epub(&input, staging.path(), &options, progress) {
        Ok(report) => report,
        Err(error) => return failed(error.to_string()),
    };

    let final_path = match metadata::publish(staging, &destination.join(&report.output_filename)) {
        Ok(path) => path,
        Err(error) => return failed(error.to_string()),
    };

    Outcome {
        path: job.path.clone(),
        output: Some(final_path.to_string_lossy().to_string()),
        summary: report.summary(),
        report: Some(report),
        error: None,
    }
}

// ------------------------------------------------------------------ helpers

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

    let thumbnail =
        image::thumbnail(&cover.bytes, THUMBNAIL.0, THUMBNAIL.1, &PREVIEW_MEMORY).ok()?;
    Some(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(thumbnail)
    ))
}
