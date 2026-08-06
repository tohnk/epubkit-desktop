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
use epubkit_core::{image, metadata, package, xml, Error};
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
#[tauri::command]
pub fn inspect_books(paths: Vec<String>) -> Vec<BookInfo> {
    paths
        .iter()
        .map(|path| inspect_one(Path::new(path)))
        .collect()
}

fn inspect_one(path: &Path) -> BookInfo {
    if !path.is_file() {
        return BookInfo::failed(path, "not a file");
    }

    match package::has_drm(path) {
        Ok(true) => return BookInfo::failed(path, Error::DrmProtected),
        Err(error) => return BookInfo::failed(path, error),
        Ok(false) => {}
    }

    let work = match tempdir() {
        Ok(dir) => dir,
        Err(error) => return BookInfo::failed(path, error),
    };

    if let Err(error) = package::extract_epub(path, work.path()) {
        return BookInfo::failed(path, error);
    }

    let opf_path = match package::find_opf_path(work.path()) {
        Ok(relative) => work.path().join(relative),
        Err(error) => return BookInfo::failed(path, error),
    };

    let opf = match xml::parse_file(&opf_path) {
        Ok(doc) => doc,
        Err(error) => return BookInfo::failed(path, error),
    };

    let meta = match metadata::extract_metadata(&opf) {
        Ok(meta) => meta,
        Err(error) => return BookInfo::failed(path, error),
    };

    let cover = (!meta.cover_href.is_empty())
        .then(|| {
            let opf_dir = opf_path.parent().unwrap_or(work.path());
            cover_data_url(&opf_dir.join(&meta.cover_href))
        })
        .flatten();

    BookInfo {
        path: path.to_string_lossy().to_string(),
        filename: file_name(path),
        size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        title: meta.title,
        author: meta.author,
        series: meta.series,
        cover,
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

fn tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir()
}

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

/// Encode a cover for display. Oversized covers are skipped rather than
/// pushed through IPC — this is a thumbnail, not the artwork.
fn cover_data_url(path: &Path) -> Option<String> {
    const MAX_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;

    if std::fs::metadata(path).ok()?.len() > MAX_PREVIEW_BYTES {
        return None;
    }

    let bytes = std::fs::read(path).ok()?;
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => return None, // not a raster preview
        _ => "image/jpeg",
    };

    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}
