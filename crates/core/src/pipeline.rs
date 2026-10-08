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

use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::html::{self, HtmlRepair};
use crate::image::{self, DeviceProfile, ImageOptions};
use crate::metadata::{self, FilenameFormat, FilenameOptions, MetadataEdits};
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
    /// How [`ProcessingReport::output_filename`] is worked out.
    pub filename: FilenameOptions,
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
            filename: FilenameOptions::default(),
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
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessingReport {
    pub original_size: u64,
    pub optimized_size: u64,
    pub output_filename: String,

    /// Source images converted. A split spread counts once.
    pub images_converted: usize,
    pub images_total: usize,
    /// Images the image step could not convert, left as they were.
    pub images_unconverted: usize,
    /// Double-page spreads Light Novel mode split into pages.
    pub spreads_split: usize,
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
    /// Content documents nothing could parse, left exactly as they were.
    pub documents_unreadable: usize,
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
        if self.images_unconverted > 0 {
            let n = self.images_unconverted;
            let (plural, as_it_was) = if n == 1 {
                ("", "it was")
            } else {
                ("s", "they were")
            };
            parts.push(format!(
                "Left {n} image{plural} that could not be converted as {as_it_was}"
            ));
        }
        if self.spreads_split > 0 {
            let plural = if self.spreads_split == 1 { "" } else { "s" };
            parts.push(format!(
                "Split {} double-page spread{plural}",
                self.spreads_split
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
        if self.documents_unreadable > 0 {
            let n = self.documents_unreadable;
            let (plural, as_it_was) = if n == 1 {
                ("", "it was")
            } else {
                ("s", "they were")
            };
            parts.push(format!(
                "Left {n} unreadable document{plural} as {as_it_was}"
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

    // A template that cannot name the book should say so before the run, not
    // after it.
    if options.filename.format == FilenameFormat::Custom {
        metadata::check_template(&options.filename.template)?;
    }

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

    let content = structure::find_content_files(work_dir, &opf_dir, &opf)?;

    // --- images (15-60%) -------------------------------------------------
    progress(15, "Processing images...");
    let converted = convert_images(
        &content.images,
        work_dir,
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
    //
    // A chapter nothing can parse, an empty file say, is left exactly as it
    // was rather than sinking the book; every later step works only on the
    // chapters that did parse.
    progress(62, "Repairing HTML...");
    let backend = html::LibxmlRepair::new();
    let mut chapters: Vec<&Path> = Vec::new();
    for path in &content.xhtml {
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;

        let Ok(repaired) = backend.repair(&bytes) else {
            report.documents_unreadable += 1;
            continue;
        };
        if repaired.recovered {
            report.documents_recovered += 1;
        }

        let (stripped, count) = html::strip_unnecessary_attributes(&repaired.bytes)?;
        report.attributes_stripped += count;

        fs::write(path, stripped).map_err(|e| Error::io(path, e))?;
        chapters.push(path);
    }

    progress(66, "Fixing SVG covers...");
    report.svg_covers_fixed = structure::fix_svg_covers(work_dir, &opf_dir, &opf)?;

    progress(68, "Updating references...");
    let rename_map = structure::build_rename_map(&converted.renames);
    if !rename_map.is_empty() {
        // Indexed once, not for each document.
        let renames = structure::Renames::new(work_dir, &opf_dir, &rename_map);
        structure::update_opf(&opf, &renames)?;
        for &path in &chapters {
            structure::update_xhtml_references(path, &renames)?;
        }
        // An SVG document names images as a chapter does. One that is not
        // well-formed is left as it is.
        for path in svg_documents(&content) {
            structure::update_svg_references(path, &renames).ok();
        }
        for path in &content.css {
            if path.is_file() {
                structure::update_css_references(path, &renames)?;
            }
        }
    }

    // A rotated image or a split spread no longer has the shape its pages
    // describe, and a split one has pages no page shows yet.
    if !converted.reshaped.is_empty() {
        progress(72, "Showing reshaped pages...");
        structure::declare_reshaped_pages(&opf, &converted.reshaped)?;
        let reshaped = structure::ReshapedPages::new(work_dir, &opf_dir, &converted.reshaped);
        for &path in &chapters {
            structure::show_reshaped_pages(path, &reshaped)?;
        }
    }

    if options.remove_unused_css {
        progress(76, "Removing unused CSS...");
        let mut used = css::UsedSelectors::default();
        for &path in &chapters {
            let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
            used.merge(&css::collect_used_selectors(&bytes)?);
        }
        // An SVG document can use a stylesheet's rules as much as a chapter.
        for path in svg_documents(&content) {
            let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
            if let Ok(used_here) = css::collect_used_selectors(&bytes) {
                used.merge(&used_here);
            }
        }

        for path in &content.css {
            if !path.is_file() {
                continue;
            }
            let stylesheet = css::read_stylesheet(path)?;
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
            let stylesheet = css::read_stylesheet(path)?;
            let (cleaned, removed) = css::remove_embedded_fonts(&stylesheet);
            if removed > 0 {
                fs::write(path, cleaned).map_err(|e| Error::io(path, e))?;
            }
        }

        // A chapter may declare a font in a <style> of its own.
        for &path in &chapters {
            let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
            let (cleaned, removed) = css::remove_embedded_fonts_from_styles(&bytes)?;
            if removed > 0 {
                fs::write(path, cleaned).map_err(|e| Error::io(path, e))?;
            }
        }

        // Fonts are counted, not the rules that named them.
        for path in &content.fonts {
            if path.is_file() && fs::remove_file(path).is_ok() {
                report.fonts_removed += 1;
            }
        }

        structure::update_opf_remove_fonts(&opf, &content.fonts)?;
        // encryption.xml lists obfuscated fonts, which are gone now.
        package::forget_missing_encrypted_files(work_dir)?;
    }

    progress(82, "Normalizing content...");
    for &path in &chapters {
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
            language: metadata::extract_metadata(&opf)?.language,
            ..TextCleanOptions::default()
        };

        for &path in &chapters {
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
    let toc = structure::fix_toc(work_dir, &opf_dir, &opf)?;
    report.toc_status = toc.describe();

    // Every OPF edit lands in one write, rather than the reference's dozen.
    xml::write_file(&opf, &opf_path, true)?;

    progress(93, "Cleaning up...");
    report.os_artifacts_removed = package::remove_os_artifacts(work_dir)?;

    progress(95, "Repackaging EPUB...");
    package::package_epub(work_dir, output_path)?;

    let final_metadata = metadata::extract_metadata(&opf)?;
    let original = input_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    report.output_filename =
        metadata::output_filename(&final_metadata, &options.filename, &original)?;
    report.optimized_size = fs::metadata(output_path)
        .map_err(|e| Error::io(output_path, e))?
        .len();

    progress(100, "Complete");
    Ok(report)
}

// ---------------------------------------------------------------- internals

/// The SVG documents in a book that are there to read.
fn svg_documents(content: &structure::ContentFiles) -> impl Iterator<Item = &Path> {
    content
        .svg
        .iter()
        .map(PathBuf::as_path)
        .filter(|path| path.is_file())
}

/// What the image step leaves for the steps after it. Paths are relative to
/// the OPF's directory.
struct ConvertedImages {
    /// Source path → the filename of its (first) output.
    renames: BTreeMap<String, String>,
    /// For each image Light Novel mode rotated or split: its pages in reading
    /// order, keyed by the first.
    reshaped: BTreeMap<String, Vec<String>>,
}

/// Convert every image in the manifest.
fn convert_images<P: FnMut(u8, &str)>(
    images: &[PathBuf],
    root: &Path,
    opf_dir: &Path,
    options: &ProcessingOptions,
    report: &mut ProcessingReport,
    progress: &mut P,
) -> Result<ConvertedImages> {
    const START: f64 = 15.0;
    const SPAN: f64 = 45.0;

    let image_options = options.image_options();
    let mut renames = BTreeMap::new();
    let mut reshaped = BTreeMap::new();
    let mut taken = TakenNames::default();
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
        // What the rename map calls the image: its path from the OPF's
        // directory, which may climb out of it. An image the map could not
        // name would lose its references, so it is left as it is.
        let Some(relative) = structure::relative_path(root, opf_dir, path) else {
            continue;
        };

        let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;

        // A single unreadable image must not sink the whole book.
        let Ok(outputs) = image::process_image(&bytes, &name, &image_options) else {
            report.images_unconverted += 1;
            report
                .image_details
                .push(format!("{name}: skipped (could not be decoded)"));
            continue;
        };

        let parent = path.parent().unwrap_or(opf_dir);

        // Settle every output's name before writing any. One named like its
        // own source replaces it in place, under the source's exact spelling:
        // a case-insensitive filesystem holds `IMG.JPG` and `IMG.jpg` as one
        // file, so "renaming" it would delete the new image along with the
        // old. Any other name has to be free, or writing it would destroy
        // another image — `cover.png` and `cover.jpeg` both want `cover.jpg`.
        let names: Vec<String> = outputs
            .iter()
            .map(|output| {
                if same_name(&output.filename, &name) {
                    name.clone()
                } else {
                    taken.claim(parent, &output.filename)
                }
            })
            .collect();

        for (output, output_name) in outputs.iter().zip(&names) {
            let destination = parent.join(output_name);
            fs::write(&destination, &output.bytes).map_err(|e| Error::io(&destination, e))?;
            report.image_details.push(output.details.clone());
        }

        // Counted once per source, however many pages it became. The leading
        // clause of the details line is the format change, which is what the
        // summary counts.
        report.images_converted += 1;
        if names.len() > 1 {
            report.spreads_split += 1;
        }
        let kind = outputs[0]
            .details
            .split(',')
            .next()
            .unwrap_or("processed")
            .trim()
            .to_string();
        *report.image_formats.entry(kind).or_insert(0) += 1;

        if outputs[0].reshaped {
            let pages: Vec<String> = names
                .iter()
                .map(|page| {
                    Path::new(&relative)
                        .with_file_name(page)
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect();
            reshaped.insert(pages[0].clone(), pages);
        }
        renames.insert(relative, names[0].clone());

        // The source only goes once its replacement is safely written, and
        // never when the replacement took its place.
        if !names.contains(&name) && path.is_file() {
            fs::remove_file(path).ok();
        }
    }

    Ok(ConvertedImages { renames, reshaped })
}

/// Filenames in use, per directory, so that a converted image never lands on
/// another file.
///
/// Compared ignoring case: macOS and Windows hold `Plate.jpg` and `plate.jpg`
/// as one file, and a book should convert the same wherever it is converted.
#[derive(Default)]
struct TakenNames {
    by_directory: HashMap<PathBuf, HashSet<String>>,
}

impl TakenNames {
    /// Claim `wanted` in `directory`, or the first free name after it in the
    /// series `name-2.jpg`, `name-3.jpg`, ….
    fn claim(&mut self, directory: &Path, wanted: &str) -> String {
        let taken = self
            .by_directory
            .entry(structure::normalize_path(directory))
            .or_insert_with(|| {
                fs::read_dir(directory)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|entry| fold_case(&entry.file_name().to_string_lossy()))
                    .collect()
            });

        let (stem, extension) = match wanted.rfind('.') {
            Some(dot) if dot > 0 => wanted.split_at(dot),
            _ => (wanted, ""),
        };
        let name = std::iter::once(wanted.to_string())
            .chain((2..).map(|n| format!("{stem}-{n}{extension}")))
            .find(|candidate| !taken.contains(&fold_case(candidate)))
            .expect("an unused suffix always exists");

        taken.insert(fold_case(&name));
        name
    }
}

fn fold_case(name: &str) -> String {
    name.to_lowercase()
}

fn same_name(a: &str, b: &str) -> bool {
    fold_case(a) == fold_case(b)
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
