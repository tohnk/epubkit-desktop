//! EPUB container handling: extraction, repackaging, validation, DRM
//! detection. Port of `epub_packager.py`.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::xml;
use crate::{Error, Result};

pub const MIMETYPE: &str = "application/epub+zip";

const MIMETYPE_ENTRY: &str = "mimetype";
const CONTAINER_ENTRY: &str = "META-INF/container.xml";
const ENCRYPTION_ENTRY: &str = "META-INF/encryption.xml";

/// Files dropped by desktop operating systems that have no business in an EPUB.
pub const OS_ARTIFACTS: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini", "._.DS_Store"];
/// Directories likewise.
pub const OS_ARTIFACT_DIRS: &[&str] = &["__MACOSX", ".git", ".svn"];

const FONT_EXTENSIONS: &[&str] = &["ttf", "otf", "woff", "woff2"];

const NS_CONTAINER: &str = "urn:oasis:names:tc:opendocument:xmlns:container";
const NS_XMLENC: &str = "http://www.w3.org/2001/04/xmlenc#";

// Substring markers, matched against raw `encryption.xml` text. See `has_drm`.
const MARKER_XMLENC: &str = "http://www.w3.org/2001/04/xmlenc";
const MARKER_IDPF_EMBEDDING: &str = "http://www.idpf.org/2008/embedding";
const MARKER_ADOBE_PDF_ENC: &str = "http://ns.adobe.com/pdf/enc";
const MARKER_ADOBE_ADEPT: &str = "http://ns.adobe.com/adept";

/// Extract an EPUB into `dest_dir`.
///
/// Entry paths are validated before anything is written: an archive cannot
/// escape `dest_dir` via absolute paths or `..` components (zip-slip).
pub fn extract_epub(epub_path: &Path, dest_dir: &Path) -> Result<()> {
    let file = File::open(epub_path).map_err(|e| Error::io(epub_path, e))?;
    let mut archive = ZipArchive::new(file)?;

    fs::create_dir_all(dest_dir).map_err(|e| Error::io(dest_dir, e))?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let raw_name = entry.name().to_string();

        // `enclosed_name` returns None for absolute paths and for anything
        // that would traverse outside the destination directory.
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| Error::UnsafeArchivePath(raw_name.clone()))?;
        let target = dest_dir.join(relative);

        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|e| Error::io(&target, e))?;
            continue;
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        let mut out = File::create(&target).map_err(|e| Error::io(&target, e))?;
        io::copy(&mut entry, &mut out).map_err(|e| Error::io(&target, e))?;
    }

    Ok(())
}

/// Rebuild an EPUB from an extracted directory.
///
/// The ordering rules are what make the output a *valid* EPUB rather than
/// merely a zip of the right files:
///
/// 1. `mimetype` first, stored uncompressed and with no extra field, so its
///    content begins at a fixed offset in the archive.
/// 2. `META-INF/container.xml` next, by convention.
/// 3. Everything else, deflated, in sorted order for reproducible output.
pub fn package_epub(source_dir: &Path, output_path: &Path) -> Result<()> {
    let out = File::create(output_path).map_err(|e| Error::io(output_path, e))?;
    let mut zip = ZipWriter::new(out);

    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    // 1. mimetype.
    let mimetype_path = source_dir.join(MIMETYPE_ENTRY);
    let mimetype = fs::read_to_string(&mimetype_path)
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|_| MIMETYPE.to_string());
    zip.start_file(MIMETYPE_ENTRY, stored)?;
    zip.write_all(mimetype.as_bytes())
        .map_err(|e| Error::io(output_path, e))?;

    // 2. container.xml.
    let container_path = source_dir.join("META-INF").join("container.xml");
    if container_path.is_file() {
        zip.start_file(CONTAINER_ENTRY, deflated)?;
        let bytes = fs::read(&container_path).map_err(|e| Error::io(&container_path, e))?;
        zip.write_all(&bytes)
            .map_err(|e| Error::io(output_path, e))?;
    }

    // 3. Everything else. Collected and sorted so the same input directory
    //    always produces the same archive byte-for-byte.
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    for entry in WalkDir::new(source_dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !is_artifact_dir(e.path()))
    {
        let entry = entry.map_err(|e| Error::Xml(e.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if is_artifact_file(path) {
            continue;
        }

        let name = archive_name(source_dir, path)?;
        if name == MIMETYPE_ENTRY || name == CONTAINER_ENTRY {
            continue; // already written
        }
        entries.push((name, path.to_path_buf()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, path) in entries {
        zip.start_file(name, deflated)?;
        let bytes = fs::read(&path).map_err(|e| Error::io(&path, e))?;
        zip.write_all(&bytes)
            .map_err(|e| Error::io(output_path, e))?;
    }

    zip.finish()?;
    Ok(())
}

/// Delete OS artifact files and directories from an extracted EPUB.
/// Returns the number of entries removed.
pub fn remove_os_artifacts(directory: &Path) -> Result<usize> {
    let mut removed = 0;

    // Collect first: deleting during the walk would invalidate it.
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for entry in WalkDir::new(directory).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if entry.file_type().is_dir() {
            if is_artifact_dir(path) {
                dirs.push(path.to_path_buf());
            }
        } else if is_artifact_file(path) {
            files.push(path.to_path_buf());
        }
    }

    for path in files {
        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    for path in dirs {
        if path.exists() && fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }

    Ok(removed)
}

/// The outcome of a structural check on an EPUB file.
#[derive(Debug, Clone, Default)]
pub struct Validation {
    pub problems: Vec<String>,
}

impl Validation {
    pub fn is_valid(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Check the container-level structure of an EPUB.
///
/// Unlike the Python original, which returned on the first problem, this
/// collects every problem it finds — more useful when diagnosing a file.
pub fn validate_epub(epub_path: &Path) -> Result<Validation> {
    let mut validation = Validation::default();

    let file = File::open(epub_path).map_err(|e| Error::io(epub_path, e))?;
    let mut archive = ZipArchive::new(file)?;

    if archive.is_empty() {
        validation.problems.push("archive is empty".into());
        return Ok(validation);
    }

    let first_name = archive.by_index(0)?.name().to_string();
    if first_name != MIMETYPE_ENTRY {
        validation.problems.push(format!(
            "mimetype is not the first entry (found {first_name})"
        ));
    }

    match archive.by_name(MIMETYPE_ENTRY) {
        Ok(mut entry) => {
            if entry.compression() != CompressionMethod::Stored {
                validation
                    .problems
                    .push("mimetype entry is compressed (should be stored)".into());
            }
            let mut content = String::new();
            entry
                .read_to_string(&mut content)
                .map_err(|e| Error::io(epub_path, e))?;
            if content.trim() != MIMETYPE {
                validation
                    .problems
                    .push(format!("invalid mimetype: {}", content.trim()));
            }
        }
        Err(_) => validation.problems.push("missing mimetype entry".into()),
    }

    if archive.by_name(CONTAINER_ENTRY).is_err() {
        validation
            .problems
            .push(format!("missing {CONTAINER_ENTRY}"));
    }

    Ok(validation)
}

/// Detect DRM.
///
/// `META-INF/encryption.xml` alone does not mean DRM: the IDPF font
/// obfuscation scheme (and Adobe's variant) live in the same file. The
/// distinction is *what* is encrypted — if only fonts are, it is obfuscation
/// and the book is processable; anything else is real DRM.
pub fn has_drm(epub_path: &Path) -> Result<bool> {
    let Some(bytes) = read_optional_entry(epub_path, ENCRYPTION_ENTRY)? else {
        return Ok(false);
    };
    let text = String::from_utf8_lossy(&bytes);

    // No XML Encryption at all.
    if !text.contains(MARKER_XMLENC) {
        return Ok(false);
    }

    // Without an obfuscation marker, encrypted content is just encrypted.
    let obfuscation_marker =
        text.contains(MARKER_IDPF_EMBEDDING) || text.contains(MARKER_ADOBE_PDF_ENC);
    if !obfuscation_marker {
        return Ok(true);
    }

    if !(text.contains(MARKER_ADOBE_ADEPT) || text.contains("EncryptedData")) {
        return Ok(false);
    }

    // Inspect what is actually encrypted.
    match encrypted_uris(&bytes) {
        Ok(uris) => Ok(uris.iter().any(|uri| !is_font_uri(uri))),
        // Unparseable encryption metadata: assume the worst rather than
        // handing the pipeline a book it cannot read.
        Err(_) => Ok(true),
    }
}

/// Locate the OPF package document within an extracted EPUB, relative to the
/// EPUB root. Reads `META-INF/container.xml`, falling back to a search for any
/// `.opf` file.
pub fn find_opf_path(epub_dir: &Path) -> Result<String> {
    let container_path = epub_dir.join("META-INF").join("container.xml");

    if container_path.is_file() {
        let bytes = fs::read(&container_path).map_err(|e| Error::io(&container_path, e))?;
        if let Ok(doc) = xml::parse_strict(&bytes) {
            // Namespaced form first, then a namespace-agnostic fallback for
            // the EPUBs that omit or misdeclare it.
            for xpath in ["//c:rootfile", "//*[local-name()='rootfile']"] {
                let values =
                    xml::attribute_values(&doc, xpath, "full-path", &[("c", NS_CONTAINER)])?;
                if let Some(path) = values.into_iter().find(|v| !v.is_empty()) {
                    return Ok(path);
                }
            }
        }
    }

    for entry in WalkDir::new(epub_dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if entry.file_type().is_file() && path.extension().is_some_and(|ext| ext == "opf") {
            return archive_name(epub_dir, path);
        }
    }

    Err(Error::OpfNotFound)
}

// ---------------------------------------------------------------- internals

fn archive_name(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root).map_err(|_| {
        Error::InvalidEpub(format!("{} is outside {}", path.display(), root.display()))
    })?;

    // Zip entries always use forward slashes, regardless of host platform.
    let mut name = String::new();
    for (i, component) in relative.components().enumerate() {
        if i > 0 {
            name.push('/');
        }
        name.push_str(&component.as_os_str().to_string_lossy());
    }
    Ok(name)
}

fn is_artifact_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| OS_ARTIFACTS.contains(&name))
}

fn is_artifact_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| OS_ARTIFACT_DIRS.contains(&name))
}

fn is_font_uri(uri: &str) -> bool {
    Path::new(uri)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| FONT_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

fn read_optional_entry(epub_path: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let file = File::open(epub_path).map_err(|e| Error::io(epub_path, e))?;
    let mut archive = ZipArchive::new(file)?;

    let result = match archive.by_name(name) {
        Ok(mut entry) => {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|e| Error::io(epub_path, e))?;
            Ok(Some(bytes))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(e.into()),
    };
    result
}

fn encrypted_uris(encryption_xml: &[u8]) -> Result<Vec<String>> {
    let doc = xml::parse_strict(encryption_xml)?;
    xml::attribute_values(
        &doc,
        "//enc:EncryptedData//enc:CipherReference",
        "URI",
        &[("enc", NS_XMLENC)],
    )
}
