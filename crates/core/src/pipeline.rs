//! The whole optimization run, start to finish. Port of `epub_processor.py`.
//!
//! Steps are ordered so each one sees the output of the last: images are
//! converted before references are rewritten to match their new names, CSS is
//! pruned before fonts are stripped from it, and the table of contents is
//! checked after everything that could have invalidated it.
//!
//! Unlike the reference, the OPF package document is parsed once and written
//! once. The Python re-read and re-wrote it at almost every step, which is both
//! slow and a way to lose an edit made earlier in the run.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::html::{self, HtmlRepair};
use crate::image::{self, DeviceProfile, ImageOptions};
use crate::metadata::{self, MetadataEdits};
use crate::text::{TextCleanOptions, TextCleanReport};
use crate::{css, package, structure, xml, Error, Result};

/// Everything the user can turn on or off for a run.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessingOptions {
    pub device: DeviceProfile,
    pub grayscale: bool,
    pub contrast_boost: bool,
    pub contrast_factor: f32,
    pub quality: u8,
    pub eink_quantize: bool,
    pub remove_fonts: bool,
    pub remove_unused_css: bool,
    pub light_novel_mode: bool,
    pub light_novel_rotate_left: bool,
    pub clean_metadata: bool,
    pub text_cleanup: bool,
    pub normalize_quotes: bool,
    pub metadata_edits: MetadataEdits,
}

impl Default for ProcessingOptions {
    fn default() -> Self {
        Self {
            device: image::X4,
            grayscale: true,
            contrast_boost: true,
            contrast_factor: 1.5,
            quality: 70,
            eink_quantize: true,
            remove_fonts: true,
            remove_unused_css: true,
            light_novel_mode: false,
            light_novel_rotate_left: true,
            clean_metadata: true,
            text_cleanup: true,
            normalize_quotes: true,
            metadata_edits: MetadataEdits::default(),
        }
    }
}

impl ProcessingOptions {
    fn image_options(&self) -> ImageOptions {
        ImageOptions {
            grayscale: self.grayscale,
            contrast_boost: self.contrast_boost,
            contrast_factor: self.contrast_factor,
            quality: self.quality,
            eink_quantize: self.eink_quantize,
            light_novel_mode: self.light_novel_mode,
            light_novel_rotate_left: self.light_novel_rotate_left,
            ..ImageOptions::for_device(self.device)
        }
    }
}

/// An account of everything the run changed, for the user and for the UI.
#[derive(Debug, Clone, Default)]
pub struct ProcessingReport {
    pub original_size: u64,
    pub optimized_size: u64,
    pub output_filename: String,

    pub images_converted: usize,
    pub images_total: usize,
    /// e.g. `{"PNG→JPEG": 5}` — how the images were transformed.
    pub image_formats: BTreeMap<String, usize>,
    pub image_details: Vec<String>,

    pub fonts_removed: usize,
    pub css_rules_removed: usize,
    pub svg_covers_fixed: usize,
    pub toc_status: String,
    pub metadata_items_stripped: usize,
    pub blank_elements_removed: usize,
    pub attributes_stripped: usize,
    pub documents_recovered: usize,
    pub text: TextCleanReport,
    pub os_artifacts_removed: usize,
}

impl ProcessingReport {
    pub fn size_reduction_percent(&self) -> f64 {
        if self.original_size == 0 {
            return 0.0;
        }
        (1.0 - self.optimized_size as f64 / self.original_size as f64) * 100.0
    }

    /// One line describing what happened, in the reference's style.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();

        if self.images_converted > 0 {
            let formats: Vec<String> = self
                .image_formats
                .iter()
                .map(|(kind, count)| format!("{count} {kind}"))
                .collect();
            parts.push(format!(
                "Converted {}/{} images ({})",
                self.images_converted,
                self.images_total,
                formats.join(", ")
            ));
        }
        if self.fonts_removed > 0 {
            parts.push(format!("Removed {} embedded fonts", self.fonts_removed));
        }
        if self.css_rules_removed > 0 {
            parts.push(format!(
                "Stripped {} unused CSS rules",
                self.css_rules_removed
            ));
        }
        if self.svg_covers_fixed > 0 {
            parts.push(format!(
                "Fixed {} SVG cover wrappers",
                self.svg_covers_fixed
            ));
        }
        if self.documents_recovered > 0 {
            parts.push(format!(
                "Repaired {} malformed documents",
                self.documents_recovered
            ));
        }
        if !self.toc_status.is_empty() {
            parts.push(format!("TOC: {}", self.toc_status));
        }
        if self.metadata_items_stripped > 0 {
            parts.push(format!(
                "Stripped {} store metadata entries",
                self.metadata_items_stripped
            ));
        }
        if self.blank_elements_removed > 0 {
            parts.push(format!(
                "Cleaned {} empty elements",
                self.blank_elements_removed
            ));
        }
        if self.attributes_stripped > 0 {
            parts.push(format!(
                "Stripped {} unnecessary attributes",
                self.attributes_stripped
            ));
        }
        if self.text.total_fixes() > 0 {
            parts.push(format!("Text cleanup: {}", self.text.summary()));
        }
        if self.os_artifacts_removed > 0 {
            parts.push(format!(
                "Removed {} OS artifacts",
                self.os_artifacts_removed
            ));
        }
        if self.original_size > 0 && self.optimized_size > 0 {
            // Dithering to four levels is high-frequency noise by construction,
            // which is the worst case for a DCT codec — a book of smooth
            // artwork can legitimately come out larger than it went in.
            let change = self.size_reduction_percent();
            let direction = if change < 0.0 {
                "increase"
            } else {
                "reduction"
            };
            parts.push(format!(
                "Size: {} → {} ({:.1}% {direction})",
                format_size(self.original_size),
                format_size(self.optimized_size),
                change.abs()
            ));
        }

        if parts.is_empty() {
            "No changes needed".to_string()
        } else {
            parts.join("; ")
        }
    }
}

/// Optimize one EPUB.
///
/// `progress` is called with a percentage and a description as the run
/// advances, so a UI can show what is happening without polling.
pub fn process_epub<P: FnMut(u8, &str)>(
    input_path: &Path,
    output_path: &Path,
    options: &ProcessingOptions,
    mut progress: P,
) -> Result<ProcessingReport> {
    let mut report = ProcessingReport {
        original_size: fs::metadata(input_path)
            .map_err(|e| Error::io(input_path, e))?
            .len(),
        ..ProcessingReport::default()
    };

    progress(2, "Checking for DRM...");
    if package::has_drm(input_path)? {
        return Err(Error::DrmProtected);
    }

    let work = tempfile::tempdir().map_err(|e| Error::io(input_path, e))?;
    let work_dir = work.path();

    progress(5, "Extracting EPUB...");
    package::extract_epub(input_path, work_dir)?;

    progress(8, "Parsing structure...");
    let opf_relative = package::find_opf_path(work_dir)?;
    let opf_path = work_dir.join(&opf_relative);
    let opf_dir = opf_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| work_dir.to_path_buf());

    // Parsed once; every step below mutates this same document.
    let opf = xml::parse_file(&opf_path)?;

    progress(10, "Reading metadata...");
    if !options.metadata_edits.is_empty() {
        metadata::update_metadata(&opf, &options.metadata_edits)?;
    }

    let content = structure::find_content_files(&opf_dir, &opf)?;

    // --- images (15-60%) -------------------------------------------------
    progress(15, "Processing images...");
    let renames = convert_images(
        &content.images,
        &opf_dir,
        options,
        &mut report,
        &mut progress,
    )?;

    // --- content documents ------------------------------------------------
    // Repair runs before anything else reads a chapter. The reference did this
    // *after* rewriting references, which meant the rewriting step silently
    // repaired the file first and the repair count came out as zero. Going
    // first also means every later step sees a well-formed tree.
    progress(62, "Repairing HTML...");
    let backend = html::LibxmlRepair::new();
    for path in &content.xhtml {
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;

        let repaired = backend.repair(&bytes)?;
        if repaired.recovered {
            report.documents_recovered += 1;
        }

        let (stripped, count) = html::strip_unnecessary_attributes(&repaired.bytes)?;
        report.attributes_stripped += count;

        fs::write(path, stripped).map_err(|e| Error::io(path, e))?;
    }

    progress(66, "Fixing SVG covers...");
    report.svg_covers_fixed = structure::fix_svg_covers(&opf_dir, &opf)?;

    progress(68, "Updating references...");
    let rename_map = structure::build_rename_map(&renames);
    if !rename_map.is_empty() {
        structure::update_opf(&opf, &rename_map)?;
        for path in &content.xhtml {
            if path.is_file() {
                structure::update_xhtml_references(path, &rename_map)?;
            }
        }
        for path in &content.css {
            if path.is_file() {
                structure::update_css_references(path, &rename_map)?;
            }
        }
    }

    if options.remove_unused_css {
        progress(76, "Removing unused CSS...");
        let mut used = css::UsedSelectors::default();
        for path in &content.xhtml {
            if path.is_file() {
                let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
                used.merge(&css::collect_used_selectors(&bytes)?);
            }
        }

        for path in &content.css {
            if !path.is_file() {
                continue;
            }
            let stylesheet = read_text(path)?;
            let (cleaned, removed) = css::remove_unused_css(&stylesheet, &used);
            report.css_rules_removed += removed;
            if removed > 0 {
                fs::write(path, cleaned).map_err(|e| Error::io(path, e))?;
            }
        }
    }

    if options.remove_fonts && !content.fonts.is_empty() {
        progress(80, "Removing embedded fonts...");

        for path in &content.css {
            if !path.is_file() {
                continue;
            }
            let stylesheet = read_text(path)?;
            let (cleaned, removed) = css::remove_embedded_fonts(&stylesheet);
            report.fonts_removed += removed;
            if removed > 0 {
                fs::write(path, cleaned).map_err(|e| Error::io(path, e))?;
            }
        }

        for path in &content.fonts {
            if path.is_file() && fs::remove_file(path).is_ok() {
                report.fonts_removed += 1;
            }
        }

        structure::update_opf_remove_fonts(&opf, &content.fonts)?;
    }

    progress(82, "Normalizing content...");
    for path in &content.xhtml {
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
        let (cleaned, removed) = html::normalize_whitespace(&bytes)?;
        report.blank_elements_removed += removed;
        let with_breaks = html::add_chapter_page_breaks(&cleaned)?;
        fs::write(path, with_breaks).map_err(|e| Error::io(path, e))?;
    }

    if options.text_cleanup {
        progress(85, "Cleaning text content...");
        let text_options = TextCleanOptions {
            normalize_quotes: options.normalize_quotes,
            ..TextCleanOptions::default()
        };

        for path in &content.xhtml {
            if !path.is_file() {
                continue;
            }
            let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
            let (cleaned, file_report) = crate::text::clean_text_content(&bytes, &text_options)?;
            if file_report.total_fixes() > 0 {
                fs::write(path, cleaned).map_err(|e| Error::io(path, e))?;
                report.text.merge(&file_report);
            }
        }
    }

    // --- package document --------------------------------------------------
    if options.clean_metadata {
        progress(87, "Cleaning metadata...");
        report.metadata_items_stripped = metadata::strip_store_metadata(&opf)?;
    }

    progress(90, "Checking TOC...");
    let toc = structure::fix_toc(&opf_dir, &opf)?;
    report.toc_status = toc.describe();

    // Every OPF edit lands in one write, rather than the reference's dozen.
    xml::write_file(&opf, &opf_path, true)?;

    progress(93, "Cleaning up...");
    report.os_artifacts_removed = package::remove_os_artifacts(work_dir)?;

    progress(95, "Repackaging EPUB...");
    package::package_epub(work_dir, output_path)?;

    let final_metadata = metadata::extract_metadata(&opf)?;
    report.output_filename =
        metadata::format_filename(&final_metadata.title, &final_metadata.author);
    report.optimized_size = fs::metadata(output_path)
        .map_err(|e| Error::io(output_path, e))?
        .len();

    progress(100, "Complete");
    Ok(report)
}

// ---------------------------------------------------------------- internals

/// Convert every image in the manifest, returning old path → new filename for
/// the reference-rewriting step.
fn convert_images<P: FnMut(u8, &str)>(
    images: &[PathBuf],
    opf_dir: &Path,
    options: &ProcessingOptions,
    report: &mut ProcessingReport,
    progress: &mut P,
) -> Result<BTreeMap<String, String>> {
    const START: f64 = 15.0;
    const SPAN: f64 = 45.0;

    let image_options = options.image_options();
    let mut renames = BTreeMap::new();
    report.images_total = images.len();

    for (index, path) in images.iter().enumerate() {
        let percent = START + SPAN * (index as f64 / images.len().max(1) as f64);
        progress(
            percent as u8,
            &format!("Processing image {}/{}...", index + 1, images.len()),
        );

        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if !image::should_process(&name) {
            continue;
        }

        let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;

        // A single unreadable image must not sink the whole book.
        let Ok(outputs) = image::process_image(&bytes, &name, &image_options) else {
            report
                .image_details
                .push(format!("{name}: skipped (could not be decoded)"));
            continue;
        };

        let parent = path.parent().unwrap_or(opf_dir);
        let mut renamed = false;

        for output in &outputs {
            let destination = parent.join(&output.filename);
            fs::write(&destination, &output.bytes).map_err(|e| Error::io(&destination, e))?;

            report.images_converted += 1;
            report.image_details.push(output.details.clone());

            // The leading clause of the details line is the format change,
            // which is what the summary counts.
            let kind = output
                .details
                .split(',')
                .next()
                .unwrap_or("processed")
                .trim()
                .to_string();
            *report.image_formats.entry(kind).or_insert(0) += 1;

            if output.filename != name {
                renamed = true;
            }
        }

        if let Ok(relative) = path.strip_prefix(opf_dir) {
            if let Some(first) = outputs.first() {
                renames.insert(
                    relative.to_string_lossy().replace('\\', "/"),
                    first.filename.clone(),
                );
            }
        }

        // The source only goes once its replacement is safely written.
        if renamed && path.is_file() {
            fs::remove_file(path).ok();
        }
    }

    Ok(renames)
}

fn read_text(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    for unit in UNITS {
        if size < 1024.0 {
            return format!("{size:.1} {unit}");
        }
        size /= 1024.0;
    }
    format!("{size:.1} TB")
}
