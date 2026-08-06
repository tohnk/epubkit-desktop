//! Command-line front-end for `epubkit-core`.
//!
//! Exists primarily so the port can be exercised and diffed against the Python
//! reference implementation without a GUI in the way.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use epubkit_core::html::{default_backend, HtmlRepair};
use epubkit_core::metadata::MetadataEdits;
use epubkit_core::pipeline::{process_epub, ProcessingOptions};
use epubkit_core::settings::Settings;
use epubkit_core::{metadata, package, structure, xml};

#[derive(Parser)]
#[command(name = "epubkit", version, about = "EPUB optimizer for e-ink readers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report structural facts about an EPUB.
    Info { path: PathBuf },
    /// Check that a file is structurally a valid EPUB container.
    Validate { path: PathBuf },
    /// Extract and repackage a book, then validate the result.
    Roundtrip {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Optimize an EPUB for an e-ink reader.
    ///
    /// Saved settings provide the defaults; the flags below override them for
    /// this run, and the result is remembered for the next one.
    Optimize {
        input: PathBuf,
        /// Write here. Defaults to "Author - Title.epub" in the current directory.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Start from a preset: quick, full, or one you saved.
        #[arg(short, long)]
        preset: Option<String>,
        /// Target device: x4 or x3.
        #[arg(short, long)]
        device: Option<String>,
        /// JPEG quality, 20-95.
        #[arg(short, long)]
        quality: Option<u8>,
        /// Rotate and split landscape artwork for vertical reading.
        #[arg(long)]
        light_novel: bool,
        /// Override the book's title.
        #[arg(long)]
        title: Option<String>,
        /// Override the book's author.
        #[arg(long)]
        author: Option<String>,
        /// Keep images in colour.
        #[arg(long)]
        no_grayscale: bool,
        /// Keep embedded fonts.
        #[arg(long)]
        no_font_removal: bool,
        /// Keep unused CSS rules.
        #[arg(long)]
        no_css_cleanup: bool,
        /// Leave text content exactly as written.
        #[arg(long)]
        no_text_cleanup: bool,
        /// Keep store and reader metadata.
        #[arg(long)]
        no_metadata_cleanup: bool,
        /// Process this book without remembering the choices.
        #[arg(long)]
        no_save: bool,
    },
    /// Show or change the saved settings.
    Settings {
        #[command(subcommand)]
        action: SettingsAction,
    },
    /// Parse and reserialize one XHTML file through the repair backend.
    Repair {
        path: PathBuf,
        /// Write here instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SettingsAction {
    /// Print the current settings and where they live.
    Show,
    /// Select a preset: quick, full, or one you saved.
    Use { preset: String },
    /// Save the current options as a named preset.
    Save { name: String },
    /// Delete a saved preset.
    Delete { id: String },
    /// Set the reader this machine is for.
    Device { device: String },
}

fn settings_path() -> Result<PathBuf> {
    Settings::default_path().context("could not locate a configuration directory")
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Info { path } => info(&path),
        Command::Validate { path } => validate(&path),
        Command::Roundtrip { input, output } => roundtrip(&input, &output),
        Command::Optimize {
            input,
            output,
            preset,
            device,
            quality,
            light_novel,
            title,
            author,
            no_grayscale,
            no_font_removal,
            no_css_cleanup,
            no_text_cleanup,
            no_metadata_cleanup,
            no_save,
        } => {
            let path = settings_path()?;
            let mut settings = Settings::load(&path).context("loading settings")?;

            if let Some(preset) = &preset {
                settings.select(preset)?;
            }
            if let Some(device) = &device {
                if epubkit_core::image::device(device).is_none() {
                    anyhow::bail!("unknown device '{device}' (expected 'x4' or 'x3')");
                }
                settings.device = device.clone();
            }

            // Each flag can only turn something off, so saved settings stay the
            // base and an unmentioned option keeps whatever it had.
            let mut customized = false;
            let mut turn_off = |current: &mut bool, flag: bool| {
                if flag && *current {
                    *current = false;
                    customized = true;
                }
            };
            turn_off(&mut settings.options.grayscale, no_grayscale);
            turn_off(&mut settings.options.remove_fonts, no_font_removal);
            turn_off(&mut settings.options.remove_unused_css, no_css_cleanup);
            turn_off(&mut settings.options.text_cleanup, no_text_cleanup);
            turn_off(&mut settings.options.clean_metadata, no_metadata_cleanup);

            if let Some(quality) = quality {
                if settings.options.quality != quality {
                    settings.options.quality = quality;
                    customized = true;
                }
            }
            if light_novel && !settings.options.light_novel_mode {
                settings.options.light_novel_mode = true;
                customized = true;
            }
            if customized {
                settings.mark_customized();
            }

            // Metadata edits are about one book, so they are never persisted.
            let options = settings.options.to_processing_options(
                settings.device_profile(),
                MetadataEdits {
                    title,
                    author,
                    language: None,
                },
            );

            if !no_save {
                settings.save(&path).context("saving settings")?;
            }

            optimize(&input, output.as_deref(), options)
        }
        Command::Settings { action } => settings_command(action),
        Command::Repair { path, output } => repair(&path, output.as_deref()),
    }
}

fn info(path: &Path) -> Result<()> {
    let drm = package::has_drm(path).context("checking for DRM")?;
    println!("file:  {}", path.display());
    println!("drm:   {drm}");

    let validation = package::validate_epub(path).context("validating container")?;
    println!("valid: {}", validation.is_valid());
    for problem in &validation.problems {
        println!("  - {problem}");
    }

    if drm {
        println!("opf:   <skipped, file is DRM-protected>");
        return Ok(());
    }

    let work = tempfile::tempdir().context("creating work directory")?;
    package::extract_epub(path, work.path()).context("extracting")?;

    let opf_rel = match package::find_opf_path(work.path()) {
        Ok(opf) => opf,
        Err(e) => {
            println!("opf:   <not found: {e}>");
            return Ok(());
        }
    };
    println!("opf:   {opf_rel}");

    let opf_path = work.path().join(&opf_rel);
    let opf_dir = opf_path.parent().unwrap_or(work.path()).to_path_buf();
    let doc = xml::parse_file(&opf_path).context("parsing the OPF")?;

    let meta = metadata::extract_metadata(&doc).context("reading metadata")?;
    println!();
    println!("title:    {}", or_dash(&meta.title));
    println!("author:   {}", or_dash(&meta.author));
    println!("language: {}", or_dash(&meta.language));
    if !meta.series.is_empty() {
        println!("series:   {} #{}", meta.series, or_dash(&meta.series_index));
    }
    println!("cover:    {}", or_dash(&meta.cover_href));
    println!(
        "filename: {}",
        metadata::format_filename(&meta.title, &meta.author)
    );

    let files = structure::find_content_files(&opf_dir, &doc).context("reading the manifest")?;
    println!();
    println!(
        "content:  {} xhtml, {} css, {} images, {} fonts, {} other",
        files.xhtml.len(),
        files.css.len(),
        files.images.len(),
        files.fonts.len(),
        files.other.len()
    );
    println!("spine:    {} entries", structure::spine_hrefs(&doc)?.len());

    Ok(())
}

fn or_dash(value: &str) -> &str {
    if value.is_empty() {
        "—"
    } else {
        value
    }
}

fn validate(path: &Path) -> Result<()> {
    let validation = package::validate_epub(path).context("validating container")?;
    if validation.is_valid() {
        println!("{}: valid", path.display());
        return Ok(());
    }

    println!("{}: invalid", path.display());
    for problem in &validation.problems {
        println!("  - {problem}");
    }
    std::process::exit(1);
}

fn roundtrip(input: &Path, output: &Path) -> Result<()> {
    let work = tempfile::tempdir().context("creating work directory")?;

    package::extract_epub(input, work.path()).context("extracting")?;
    let removed = package::remove_os_artifacts(work.path()).context("cleaning OS artifacts")?;
    package::package_epub(work.path(), output).context("repackaging")?;

    let before = std::fs::metadata(input).map(|m| m.len()).unwrap_or(0);
    let after = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);

    println!("in:       {} ({before} bytes)", input.display());
    println!("out:      {} ({after} bytes)", output.display());
    println!("artifacts removed: {removed}");

    let validation = package::validate_epub(output).context("validating output")?;
    if validation.is_valid() {
        println!("output:   valid EPUB container");
    } else {
        println!("output:   INVALID");
        for problem in &validation.problems {
            println!("  - {problem}");
        }
        std::process::exit(1);
    }

    Ok(())
}

fn repair(path: &Path, output: Option<&Path>) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;

    let backend = default_backend();
    let repaired = backend
        .repair(&bytes)
        .with_context(|| format!("repairing {} with {}", path.display(), backend.name()))?;

    match output {
        Some(dest) => {
            std::fs::write(dest, &repaired.bytes)
                .with_context(|| format!("writing {}", dest.display()))?;
            eprintln!(
                "{} -> {} ({}, {} bytes)",
                path.display(),
                dest.display(),
                if repaired.recovered {
                    "recovered"
                } else {
                    "well-formed"
                },
                repaired.bytes.len()
            );
        }
        None => {
            eprintln!(
                "{}: {} ({} bytes)",
                path.display(),
                if repaired.recovered {
                    "recovered"
                } else {
                    "well-formed"
                },
                repaired.bytes.len()
            );
            print!("{}", String::from_utf8_lossy(&repaired.bytes));
        }
    }

    Ok(())
}

fn optimize(input: &Path, output: Option<&Path>, options: ProcessingOptions) -> Result<()> {
    // The output name comes from the book's metadata, which is not known until
    // the run finishes — so write to a temporary file and move it into place.
    let staging = tempfile::Builder::new()
        .suffix(".epub")
        .tempfile()
        .context("creating a staging file")?;

    let mut last_percent = u8::MAX;
    let report = process_epub(input, staging.path(), &options, |percent, message| {
        if percent != last_percent {
            eprintln!("[{percent:>3}%] {message}");
            last_percent = percent;
        }
    })
    .with_context(|| format!("optimizing {}", input.display()))?;

    let destination = match output {
        Some(path) => path.to_path_buf(),
        None => PathBuf::from(&report.output_filename),
    };

    // `persist` fails across filesystems, so fall back to a copy.
    if let Err(error) = staging.persist(&destination) {
        std::fs::copy(error.file.path(), &destination)
            .with_context(|| format!("writing {}", destination.display()))?;
    }

    println!();
    println!("{}", destination.display());
    println!("{}", report.summary());

    Ok(())
}

fn settings_command(action: SettingsAction) -> Result<()> {
    let path = settings_path()?;
    let mut settings = Settings::load(&path).context("loading settings")?;

    match action {
        SettingsAction::Show => {
            println!("file:    {}", path.display());
            println!("device:  {}", settings.device);
            println!("preset:  {}", settings.active_label());
            println!();
            let o = &settings.options;
            println!("  grayscale        {}", o.grayscale);
            println!("  contrast boost   {}", o.contrast_boost);
            println!("  quality          {}", o.quality);
            println!("  remove fonts     {}", o.remove_fonts);
            println!("  clean css        {}", o.remove_unused_css);
            println!("  clean metadata   {}", o.clean_metadata);
            println!("  text cleanup     {}", o.text_cleanup);
            println!("  light novel      {}", o.light_novel_mode);

            if !settings.presets.is_empty() {
                println!();
                println!("saved presets:");
                for preset in &settings.presets {
                    println!("  {:<20} {}", preset.id, preset.name);
                }
            }
            return Ok(());
        }
        SettingsAction::Use { preset } => {
            settings.select(&preset)?;
            println!("selected {}", settings.active_label());
        }
        SettingsAction::Save { name } => {
            let id = settings.save_preset(&name)?;
            println!("saved current options as '{id}'");
        }
        SettingsAction::Delete { id } => {
            settings.delete_preset(&id)?;
            println!("deleted '{id}'");
        }
        SettingsAction::Device { device } => {
            if epubkit_core::image::device(&device).is_none() {
                anyhow::bail!("unknown device '{device}' (expected 'x4' or 'x3')");
            }
            settings.device = device;
            println!("device set to {}", settings.device);
        }
    }

    settings.save(&path).context("saving settings")
}
