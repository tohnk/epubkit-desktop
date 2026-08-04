//! Command-line front-end for `epubkit-core`.
//!
//! Exists primarily so the port can be exercised and diffed against the Python
//! reference implementation without a GUI in the way.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use epubkit_core::html::{default_backend, HtmlRepair};
use epubkit_core::package;

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
    /// Parse and reserialize one XHTML file through the repair backend.
    Repair {
        path: PathBuf,
        /// Write here instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Info { path } => info(&path),
        Command::Validate { path } => validate(&path),
        Command::Roundtrip { input, output } => roundtrip(&input, &output),
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
    match package::find_opf_path(work.path()) {
        Ok(opf) => println!("opf:   {opf}"),
        Err(e) => println!("opf:   <not found: {e}>"),
    }

    Ok(())
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
