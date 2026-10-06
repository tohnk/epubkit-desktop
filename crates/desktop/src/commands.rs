//! The IPC surface.
//!
//! Commands are deliberately thin. Anything that decides behaviour — what a
//! preset means, what the pipeline does, how a filename is derived — lives in
//! `epubkit-core` so the CLI and the window cannot drift apart.

use std::path::{Path, PathBuf};

use base64::Engine;
use epubkit_core::metadata::MetadataEdits;
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
    tauri::async_runtime::spawn_blocking(move || {
        paths
            .iter()
            .map(|path| inspect_one(Path::new(path)))
            .collect()
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
        let device = settings.device_profile();
        let total = jobs.len();
        let mut outcomes = Vec::with_capacity(total);

        for (index, job) in jobs.iter().enumerate() {
            let input = PathBuf::from(&job.path);
            let options = settings.options.to_processing_options(
                device,
                MetadataEdits {
                    title: job.title.clone().filter(|value| !value.trim().is_empty()),
                    author: job.author.clone().filter(|value| !value.trim().is_empty()),
                    language: None,
                },
            );

            // The output name comes from the book's metadata, which is not
            // known until the run finishes — so write beside the destination
            // and rename once the report says what to call it.
            let staging = destination.join(format!(".epubkit-{index}.part"));

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

            let outcome = match process_epub(&input, &staging, &options, emit) {
                Ok(report) => {
                    let final_path = unique_path(&destination.join(&report.output_filename));
                    match std::fs::rename(&staging, &final_path) {
                        Ok(()) => Outcome {
                            path: job.path.clone(),
                            output: Some(final_path.to_string_lossy().to_string()),
                            summary: report.summary(),
                            report: Some(report),
                            error: None,
                        },
                        Err(error) => Outcome {
                            path: job.path.clone(),
                            output: None,
                            summary: String::new(),
                            report: None,
                            error: Some(format!(
                                "could not write {}: {error}",
                                final_path.display()
                            )),
                        },
                    }
                }
                Err(error) => {
                    let _ = std::fs::remove_file(&staging);
                    Outcome {
                        path: job.path.clone(),
                        output: None,
                        summary: String::new(),
                        report: None,
                        error: Some(error.to_string()),
                    }
                }
            };

            let _ = app.emit("finished", outcome.clone());
            outcomes.push(outcome);
        }

        Ok(outcomes)
    })
    .await
    .map_err(to_message)?
}

// ------------------------------------------------------------------ helpers

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Never silently overwrite a book that is already there.
fn unique_path(preferred: &Path) -> PathBuf {
    if !preferred.exists() {
        return preferred.to_path_buf();
    }

    let stem = preferred
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "optimized".to_string());
    let parent = preferred.parent().unwrap_or(Path::new("."));

    (2..)
        .map(|n| parent.join(format!("{stem} ({n}).epub")))
        .find(|candidate| !candidate.exists())
        .expect("an unused suffix always exists")
}

/// Encode a cover for display.
///
/// The type written into the data URL is always one of a fixed few, never the
/// book's own string: the page puts the URL in an `<img src>`, and a media
/// type is whatever the book's author typed.
fn cover_data_url(cover: &preview::Cover) -> Option<String> {
    let extension = Path::new(&cover.path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    let mime = match cover.media_type.to_ascii_lowercase().as_str() {
        "image/png" => "image/png",
        "image/gif" => "image/gif",
        "image/webp" => "image/webp",
        "image/jpeg" | "image/jpg" => "image/jpeg",
        "image/svg+xml" => return None, // not a raster preview
        _ => match extension.as_deref() {
            Some("png") => "image/png",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("svg") => return None,
            _ => "image/jpeg",
        },
    };

    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&cover.bytes)
    ))
}
