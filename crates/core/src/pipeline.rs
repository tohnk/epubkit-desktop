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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Condvar, Mutex, PoisonError};

use crate::memory::MemoryBudget;

use crate::html::{self, HtmlRepair};
use crate::image::{self, DeviceProfile, ImageOptions};
use crate::metadata::{self, FilenameFormat, FilenameOptions, MetadataEdits};
use crate::text::{TextCleanOptions, TextCleanReport};
use crate::{css, package, structure, worker, xml, Error, Result};

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
    /// The images the image step tried: every one the book holds that its
    /// manifest declares or a document shows, but for SVG.
    pub images_total: usize,
    /// Images the image step could not convert, left as they were.
    pub images_unconverted: usize,
    /// Images the book shows that its manifest left out, declared once
    /// converted.
    pub images_declared: usize,
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
    /// Documents too large to read whole, left exactly as they were.
    pub documents_too_large: usize,
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
        if self.images_declared > 0 {
            let n = self.images_declared;
            let plural = if n == 1 { "" } else { "s" };
            parts.push(format!("Declared {n} image{plural} the manifest left out"));
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
        if self.documents_too_large > 0 {
            let n = self.documents_too_large;
            let (plural, as_it_was) = if n == 1 {
                ("", "it was")
            } else {
                ("s", "they were")
            };
            parts.push(format!(
                "Left {n} document{plural} too large to process as {as_it_was}"
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
    // What the book is named from, read now, before cleaning the metadata
    // takes anything out of it.
    let naming = metadata::extract_metadata(&opf)?;

    let content = structure::find_content_files(work_dir, &opf_dir, &opf)?;

    // --- content documents ------------------------------------------------
    // Repair runs before anything else reads a chapter. The reference did this
    // *after* rewriting references, which meant the rewriting step silently
    // repaired the file first and the repair count came out as zero. Going
    // first also means every later step sees a well-formed tree, the image
    // step's look at how images are shown included.
    //
    // A chapter nothing can parse, an empty file say, is left exactly as it
    // was rather than sinking the book; every later step works only on the
    // chapters that did parse.
    progress(12, "Repairing HTML...");
    let backend = html::LibxmlRepair::new();
    let mut chapters: Vec<&Path> = Vec::new();
    // Whether the run leaves a document exactly as it is that it has not
    // read, so that what it names and uses cannot be known: one too large to
    // read whole, or one nothing can parse that is not blank.
    let mut left_unread = !content.too_large.is_empty();
    report.documents_too_large = content.too_large.len();
    for path in &content.xhtml {
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;

        let Ok(repaired) = backend.repair(&bytes) else {
            report.documents_unreadable += 1;
            left_unread |= !is_blank(&bytes);
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

    // A stylesheet a chapter or SVG document reaches but the manifest leaves
    // out is followed all the same: its references are rewritten with the
    // rest, and one too large to read leaves every image as it is, as any
    // document left unread does.
    let reachable = {
        let mut documents: Vec<&Path> = chapters.clone();
        documents.extend(svg_documents(&content));
        structure::reachable_stylesheets(work_dir, &opf_dir, &documents)?
    };
    left_unread |= reachable.too_large;

    // --- images (15-60%) -------------------------------------------------
    // Light Novel mode reshapes an image only where it is shown as a page of
    // its own, so first it looks at how each is shown.
    let keep_shape = if options.light_novel_mode {
        structure::fixed_shape_images(work_dir, &opf_dir, &opf, &chapters, &content)?
    } else {
        HashSet::new()
    };
    progress(15, "Processing images...");
    // Every image the manifest declares, whatever its name or media type, but
    // for SVG, which is drawn and stays as it is; then those the book shows
    // that the manifest leaves out.
    let undeclared = structure::undeclared_images(work_dir, &opf_dir, &opf, &chapters, &content)?;
    let mut rasters: Vec<PathBuf> = content
        .images
        .iter()
        .filter(|path| !content.svg.contains(path))
        .chain(&undeclared)
        .cloned()
        .collect();
    // Which images a document left as it is names cannot be known without
    // reading it as a reader does, escapes and all, and an image it names
    // must keep its name, so the reference there still leads to it. So where
    // there is one, every image keeps its name, and stays as it is.
    let mut kept: Vec<PathBuf> = if left_unread {
        let rasters = std::mem::take(&mut rasters);
        rasters.into_iter().filter(|path| path.is_file()).collect()
    } else {
        Vec::new()
    };
    let converted = convert_images(
        &rasters,
        work_dir,
        &opf_dir,
        options,
        &keep_shape,
        &mut report,
        &mut progress,
    )?;
    report.images_total += kept.len();
    report.images_unconverted += kept.len();
    kept.sort();
    for path in &kept {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        report.image_details.push(format!(
            "{name}: kept as it is (a document left as it is may name it)"
        ));
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
        // Every stylesheet the manifest lists, and every one a document
        // reaches that it leaves out, each rewritten once.
        let mut stylesheets: Vec<&Path> = content.css.iter().map(PathBuf::as_path).collect();
        stylesheets.extend(
            reachable
                .readable
                .iter()
                .filter(|path| !content.css.contains(path))
                .map(PathBuf::as_path),
        );
        for path in stylesheets {
            if path.is_file() {
                structure::update_css_references(path, &renames)?;
            }
        }
    }
    // A renamed image's entry is declared a JPEG as it is pointed at its new
    // file. One that kept its name is a JPEG now all the same.
    structure::declare_jpegs(&opf, work_dir, &opf_dir, &converted.in_place)?;
    // One the manifest left out is declared, as the JPEG it became.
    let declared: Vec<String> = undeclared
        .iter()
        .filter_map(|path| {
            let relative = structure::relative_path(work_dir, &opf_dir, path)?;
            let name = converted.renames.get(&relative)?;
            Some(
                Path::new(&relative)
                    .with_file_name(name)
                    .to_string_lossy()
                    .replace('\\', "/"),
            )
        })
        .collect();
    report.images_declared = structure::declare_images(&opf, &declared)?;

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

    // What a document left unread uses cannot be known, so no rule is known
    // to be unused.
    if options.remove_unused_css && !left_unread {
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

    let original = input_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    report.output_filename = metadata::output_filename(&naming, &options.filename, &original)?;
    report.optimized_size = fs::metadata(output_path)
        .map_err(|e| Error::io(output_path, e))?
        .len();

    progress(100, "Complete");
    Ok(report)
}

// ---------------------------------------------------------------- internals

/// Whether a document, as `bytes`, is blank: nothing but whitespace, in UTF-8
/// or UTF-16, after a byte order mark if it has one. A blank one names no
/// image.
fn is_blank(bytes: &[u8]) -> bool {
    let text = [&b"\xEF\xBB\xBF"[..], b"\xFF\xFE", b"\xFE\xFF"]
        .iter()
        .find_map(|mark| bytes.strip_prefix(*mark))
        .unwrap_or(bytes);
    text.iter()
        .all(|&byte| byte.is_ascii_whitespace() || byte == 0)
}

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
#[derive(Default)]
struct ConvertedImages {
    /// Source path → the filename of its (first) output.
    renames: BTreeMap<String, String>,
    /// For each image Light Novel mode rotated or split: its pages in reading
    /// order, keyed by the first.
    reshaped: BTreeMap<String, Vec<String>>,
    /// Sources replaced in place by an image of their own name, which no
    /// rename points the manifest at.
    in_place: Vec<String>,
}

/// Convert `images`, whatever they are named: what each is, is read from its
/// bytes. Those in `keep_shape`, by path, are converted as they are shaped,
/// whatever Light Novel mode would make of them. One that cannot be is left
/// as it is, and counted.
///
/// Images are converted on as many threads as the machine runs at once. What
/// is then done with each, naming, writing, deleting its source, reporting,
/// is done in manifest order, so that a book converts the same however its
/// threads ran.
fn convert_images<P: FnMut(u8, &str)>(
    images: &[PathBuf],
    root: &Path,
    opf_dir: &Path,
    options: &ProcessingOptions,
    keep_shape: &HashSet<PathBuf>,
    report: &mut ProcessingReport,
    progress: &mut P,
) -> Result<ConvertedImages> {
    const START: f64 = 15.0;
    const SPAN: f64 = 45.0;

    let image_options = options.image_options();
    let as_shaped = ImageOptions {
        light_novel_mode: false,
        ..image_options.clone()
    };

    let jobs: Vec<ImageJob> = images
        .iter()
        .filter_map(|path| {
            if !path.is_file() {
                return None;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            // What the rename map calls the image: its path from the OPF's
            // directory, which may climb out of it. An image the map could
            // not name would lose its references, so it is left as it is.
            let relative = structure::relative_path(root, opf_dir, path)?;
            let options = if keep_shape.contains(path) {
                &as_shaped
            } else {
                &image_options
            };
            Some(ImageJob {
                path,
                name,
                relative,
                options,
            })
        })
        .collect();
    report.images_total = jobs.len();

    let mut converted = ConvertedImages::default();
    let mut taken = TakenNames::default();

    in_order_in_parallel(
        &jobs,
        |job| -> Result<Converted> {
            // Set aside before the file is read, so a thread waiting its turn
            // holds nothing; and an image that would need more than all
            // there is to set aside is left as it is. Reading its header to
            // know holds a little too, which is set aside while it is read.
            let needed = {
                let _reading = IMAGE_MEMORY.hold(image::HEADER_MEMORY);
                image::memory_needed(job.path, job.options)
            };
            // One that will not be decoded is not tried.
            let Some(needed) = needed else {
                return Ok(Converted::Undecodable);
            };
            if needed > IMAGE_MEMORY.total() {
                return Ok(Converted::TooLarge(needed));
            }
            let _held = IMAGE_MEMORY.hold(needed);
            // Decoded in a worker held to `needed` bytes, where workers are on,
            // so an image crafted to make a decoder allocate wildly cannot. The
            // worker reads the file itself, so the parent keeps no second copy.
            // A single image that will not decode, or overruns the worker, must
            // not sink the whole book; a file that cannot be read still does.
            Ok(
                match worker::convert(job.path, &job.name, job.options, needed)? {
                    worker::Converted::Done(outputs) => Converted::Done(outputs),
                    worker::Converted::Undecodable => Converted::Undecodable,
                    worker::Converted::Overran => Converted::Overran(needed),
                },
            )
        },
        |finished| {
            let percent = START + SPAN * (finished as f64 / jobs.len() as f64);
            progress(
                percent as u8,
                &format!("Processing image {finished}/{}...", jobs.len()),
            );
        },
        |job, outcome| {
            let skipped = match outcome? {
                Converted::Done(outputs) => {
                    return converted.take(job, outputs, &mut taken, opf_dir, report)
                }
                Converted::Undecodable => "could not be decoded".to_string(),
                Converted::TooLarge(needed) => format!(
                    "converting it would take {} MB, more than the {} MB set aside",
                    needed >> 20,
                    IMAGE_MEMORY.total() >> 20
                ),
                Converted::Overran(needed) => format!(
                    "could not be decoded within the {} MB set aside for it",
                    needed >> 20
                ),
            };
            report.images_unconverted += 1;
            report
                .image_details
                .push(format!("{}: skipped ({skipped})", job.name));
            Ok(())
        },
    )?;

    Ok(converted)
}

/// What became of one image the image step tried.
enum Converted {
    /// Converted to these, a page or two.
    Done(Vec<image::ProcessedImage>),
    /// Not decoded: not an image after all, or one in a form the decoders do
    /// not read.
    Undecodable,
    /// Not tried: converting it would need this much memory, more than there
    /// is to set aside.
    TooLarge(u64),
    /// The worker could not decode it within this much memory, set aside for
    /// it, and was stopped, or died before it could answer.
    Overran(u64),
}

/// One image the image step tries to convert.
struct ImageJob<'a> {
    path: &'a Path,
    name: String,
    /// Its path from the OPF's directory.
    relative: String,
    options: &'a ImageOptions,
}

impl ConvertedImages {
    /// Write what `job`'s image converted to, and note it.
    fn take(
        &mut self,
        job: &ImageJob,
        outputs: Vec<image::ProcessedImage>,
        taken: &mut TakenNames,
        opf_dir: &Path,
        report: &mut ProcessingReport,
    ) -> Result<()> {
        let (path, name, relative) = (job.path, &job.name, &job.relative);
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
                if same_name(&output.filename, name) {
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

        // Counted once per source, however many pages it became.
        report.images_converted += 1;
        if names.len() > 1 {
            report.spreads_split += 1;
        }
        *report
            .image_formats
            .entry(outputs[0].conversion.clone())
            .or_insert(0) += 1;

        if outputs[0].reshaped {
            let pages: Vec<String> = names
                .iter()
                .map(|page| {
                    Path::new(relative)
                        .with_file_name(page)
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect();
            self.reshaped.insert(pages[0].clone(), pages);
        }
        if names[0] == *name {
            self.in_place.push(relative.clone());
        }
        self.renames.insert(relative.clone(), names[0].clone());

        // The source only goes once its replacement is safely written, and
        // never when the replacement took its place.
        if !names.contains(name) && path.is_file() {
            fs::remove_file(path).ok();
        }
        Ok(())
    }
}

/// Memory the image step's threads may hold at once for the images they are
/// converting, as [`image::memory_needed`] reckons it, in every run in the
/// process together. Enough for the four or so a machine converts at once,
/// unless they are very large, when they wait their turn; an image that
/// would need more than all of it is not converted.
static IMAGE_MEMORY: MemoryBudget = MemoryBudget::new(1 << 30);

/// Run `work` on each of `jobs` on as many threads as the machine runs at
/// once, telling `finished` how many are done each time one is, and hand
/// each result to `take` in the order of `jobs`, as soon as it and every one
/// before it are done. Stops at the first error `take` returns; threads
/// finish only the jobs they have begun.
///
/// A result waits in memory until every one before it is taken, so the
/// threads run at most twice their number of jobs ahead of what has been
/// taken: one slow job early on does not leave the rest of the book
/// converted and waiting behind it.
fn in_order_in_parallel<J: Sync, T: Send>(
    jobs: &[J],
    work: impl Fn(&J) -> T + Sync,
    mut finished: impl FnMut(usize),
    mut take: impl FnMut(&J, T) -> Result<()>,
) -> Result<()> {
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(jobs.len());
    if threads <= 1 {
        for (index, job) in jobs.iter().enumerate() {
            let result = work(job);
            finished(index + 1);
            take(job, result)?;
        }
        return Ok(());
    }

    let ahead = threads * 2;
    let next = AtomicUsize::new(0);
    let order = Order::default();
    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        for _ in 0..threads {
            let sender = sender.clone();
            let (next, work, order) = (&next, &work, &order);
            scope.spawn(move || {
                // One thread's panic must not leave the others waiting.
                let _stop = order.stop_on_drop(true);
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else {
                        break;
                    };
                    if !order.wait_until_taken(index.saturating_sub(ahead - 1)) {
                        break;
                    }
                    if sender.send((index, work(job))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);

        // However this ends, no thread is left waiting on what will now never
        // be taken.
        let _stop = order.stop_on_drop(false);
        let mut ready = BTreeMap::new();
        let mut taken = 0;
        for (count, (index, result)) in receiver.iter().enumerate() {
            finished(count + 1);
            ready.insert(index, result);
            while let Some(result) = ready.remove(&taken) {
                take(&jobs[taken], result)?;
                taken += 1;
                order.taken(taken);
            }
        }
        Ok(())
    })
}

/// How many results have been taken in order, for threads waiting to run no
/// further ahead.
#[derive(Default)]
struct Order {
    state: Mutex<OrderState>,
    changed: Condvar,
}

#[derive(Default)]
struct OrderState {
    taken: usize,
    stopped: bool,
}

impl Order {
    fn lock(&self) -> std::sync::MutexGuard<'_, OrderState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait until `count` results have been taken. False if the work stopped
    /// first.
    fn wait_until_taken(&self, count: usize) -> bool {
        let mut state = self.lock();
        while state.taken < count && !state.stopped {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        !state.stopped
    }

    fn taken(&self, count: usize) {
        self.lock().taken = count;
        self.changed.notify_all();
    }

    fn stop(&self) {
        self.lock().stopped = true;
        self.changed.notify_all();
    }

    /// Something that stops the work when it is dropped: while unwinding
    /// only, if `only_on_panic`.
    fn stop_on_drop(&self, only_on_panic: bool) -> StopOnDrop<'_> {
        StopOnDrop {
            order: self,
            only_on_panic,
        }
    }
}

struct StopOnDrop<'a> {
    order: &'a Order,
    only_on_panic: bool,
}

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        if !self.only_on_panic || std::thread::panicking() {
            self.order.stop();
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Results are taken in order, so a slow job holds up every one after it,
    /// and those wait in memory. With nothing to stop them, the other threads
    /// went on to convert the whole rest of a book behind it.
    #[test]
    fn work_runs_only_so_far_ahead_of_what_is_taken() {
        let jobs: Vec<usize> = (0..64).collect();
        let done = AtomicUsize::new(0);
        let taken = AtomicUsize::new(0);
        let most_waiting = AtomicUsize::new(0);

        in_order_in_parallel(
            &jobs,
            |&job| {
                if job == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
                let waiting =
                    done.fetch_add(1, Ordering::SeqCst) + 1 - taken.load(Ordering::SeqCst);
                most_waiting.fetch_max(waiting, Ordering::SeqCst);
            },
            |_| {},
            |_, ()| {
                taken.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap();

        let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let most_waiting = most_waiting.load(Ordering::SeqCst);
        assert!(most_waiting <= 2 * threads, "{most_waiting} results waited");
        assert_eq!(taken.load(Ordering::SeqCst), 64);
    }
}
