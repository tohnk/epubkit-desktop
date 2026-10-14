//! EPUB container handling: extraction, repackaging, validation, DRM
//! detection. Port of `epub_packager.py`.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::{structure, xml};
use crate::{Error, Result};

pub const MIMETYPE: &str = "application/epub+zip";

const MIMETYPE_ENTRY: &str = "mimetype";
/// More than a `mimetype` entry ever needs to say what it is.
const MAX_MIMETYPE_BYTES: u64 = 1024;
const CONTAINER_ENTRY: &str = "META-INF/container.xml";
const ENCRYPTION_ENTRY: &str = "META-INF/encryption.xml";

/// Files dropped by desktop operating systems that have no business in an EPUB.
pub const OS_ARTIFACTS: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini", "._.DS_Store"];
/// Directories likewise.
pub const OS_ARTIFACT_DIRS: &[&str] = &["__MACOSX", ".git", ".svn"];

const FONT_EXTENSIONS: &[&str] = &["ttf", "otf", "woff", "woff2"];

const NS_CONTAINER: &str = "urn:oasis:names:tc:opendocument:xmlns:container";

/// The font obfuscation algorithms `encryption.xml` names alongside real
/// encryption: the IDPF's, and Adobe's older one.
const OBFUSCATION_ALGORITHMS: &[&str] = &[
    "http://www.idpf.org/2008/embedding",
    "http://ns.adobe.com/pdf/enc#RC",
];

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
    let mut zip = ZipWriter::new(BufWriter::new(out));

    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    // 1. mimetype.
    let mimetype_path = source_dir.join(MIMETYPE_ENTRY);
    // No more of it than a media type takes.
    let mut mimetype = String::new();
    let read = File::open(&mimetype_path)
        .and_then(|file| file.take(MAX_MIMETYPE_BYTES).read_to_string(&mut mimetype));
    let mimetype = match read {
        Ok(_) => mimetype.trim().to_string(),
        Err(_) => MIMETYPE.to_string(),
    };
    zip.start_file(MIMETYPE_ENTRY, stored)?;
    zip.write_all(mimetype.as_bytes())
        .map_err(|e| Error::io(output_path, e))?;

    // 2. container.xml.
    let container_path = source_dir.join("META-INF").join("container.xml");
    if container_path.is_file() {
        zip.start_file(CONTAINER_ENTRY, deflated)?;
        copy_into(&mut zip, &container_path, output_path)?;
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
        copy_into(&mut zip, &path, output_path)?;
    }

    // The buffer would flush itself when dropped, but lose any error doing so.
    zip.finish()?.flush().map_err(|e| Error::io(output_path, e))
}

/// Copy the file at `path` into the entry `zip` has started, a piece at a
/// time: a book's largest file need not fit in memory to be packed.
fn copy_into(zip: &mut ZipWriter<BufWriter<File>>, path: &Path, output_path: &Path) -> Result<()> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
    io::copy(&mut file, zip).map_err(|e| Error::io(output_path, e))?;
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
        Ok(entry) => {
            if entry.compression() != CompressionMethod::Stored {
                validation
                    .problems
                    .push("mimetype entry is compressed (should be stored)".into());
            }
            let mut content = String::new();
            entry
                .take(MAX_MIMETYPE_BYTES)
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
/// `META-INF/encryption.xml` alone does not mean DRM: font obfuscation, the
/// IDPF's scheme and Adobe's, is declared in the same file. What counts is
/// what each entry does, and to what — a font obfuscated leaves the book
/// processable; anything else is real DRM.
///
/// The file is parsed rather than searched, so it reads the same in any
/// encoding XML allows, UTF-16 as much as UTF-8. Metadata that cannot be read
/// could be hiding anything, so it is taken for DRM rather than handing the
/// pipeline a book it cannot read.
pub fn has_drm(epub_path: &Path) -> Result<bool> {
    let bytes = match read_optional_entry(epub_path, ENCRYPTION_ENTRY) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(false),
        // Too large to read is as good as unreadable.
        Err(Error::DocumentTooLarge { .. }) => return Ok(true),
        Err(error) => return Err(error),
    };

    // An empty file declares nothing encrypted.
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(false);
    }

    let Ok(encrypted) = encrypted_resources(&bytes) else {
        return Ok(true);
    };

    Ok(encrypted.iter().any(|(algorithm, uri)| {
        !(OBFUSCATION_ALGORITHMS.contains(&algorithm.as_str()) && is_font_uri(uri))
    }))
}

/// Locate the OPF package document within an extracted EPUB, relative to the
/// EPUB root. Reads `META-INF/container.xml`, falling back to a search for any
/// `.opf` file.
pub fn find_opf_path(epub_dir: &Path) -> Result<String> {
    let container_path = epub_dir.join("META-INF").join("container.xml");

    // One too large to read is no use either.
    if container_path.is_file() && !crate::too_large_to_read(&container_path) {
        let bytes = fs::read(&container_path).map_err(|e| Error::io(&container_path, e))?;
        // A path out of the book is no use, and the pipeline would rewrite
        // whatever it named. Neither is one to nothing in it. The search
        // below finds the package that is there.
        let inside = opf_path_in_container(&bytes)?
            .and_then(|path| structure::resolve_href(epub_dir, epub_dir, &path))
            .filter(|path| path.is_file());
        if let Some(path) = inside {
            return archive_name(epub_dir, &path);
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

/// The package document `container.xml` points at, if it parses and does.
pub(crate) fn opf_path_in_container(container_xml: &[u8]) -> Result<Option<String>> {
    let Ok(doc) = xml::parse_strict(container_xml) else {
        return Ok(None);
    };

    // Namespaced form first, then a namespace-agnostic fallback for the EPUBs
    // that omit or misdeclare it.
    for xpath in ["//c:rootfile", "//*[local-name()='rootfile']"] {
        let values = xml::attribute_values(&doc, xpath, "full-path", &[("c", NS_CONTAINER)])?;
        if let Some(path) = values.into_iter().find(|v| !v.is_empty()) {
            return Ok(Some(path));
        }
    }
    Ok(None)
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

/// An entry's bytes, `None` if there is no such entry. One larger than a
/// document may be is not read: the size it declares can lie, so no more
/// than that is ever read.
fn read_optional_entry(epub_path: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let file = File::open(epub_path).map_err(|e| Error::io(epub_path, e))?;
    let mut archive = ZipArchive::new(file)?;

    let result = match archive.by_name(name) {
        Ok(entry) => {
            let mut bytes = Vec::new();
            entry
                .take(crate::MAX_DOCUMENT_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| Error::io(epub_path, e))?;
            if bytes.len() as u64 > crate::MAX_DOCUMENT_BYTES {
                return Err(Error::DocumentTooLarge {
                    path: PathBuf::from(name),
                    size: bytes.len() as u64,
                });
            }
            Ok(Some(bytes))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(e.into()),
    };
    result
}

/// Drop what `META-INF/encryption.xml` says about files no longer in the
/// unpacked book at `epub_dir`, such as the obfuscated fonts removed with
/// the rest, and the file itself once it says nothing. A file that cannot be
/// read is left as it is.
pub fn forget_missing_encrypted_files(epub_dir: &Path) -> Result<()> {
    let path = epub_dir.join(ENCRYPTION_ENTRY);
    if !path.is_file() {
        return Ok(());
    }
    let Ok(doc) = xml::parse_file(&path) else {
        return Ok(());
    };

    let reference = format!(
        "./{}/{}",
        xml::local("CipherData"),
        xml::local("CipherReference")
    );
    let mut kept = 0;
    let mut forgotten = 0;
    for mut data in xml::find_nodes(&doc, &format!("//{}", xml::local("EncryptedData")))? {
        let uri = xml::find_nodes_under(&doc, &data, &reference)?
            .first()
            .and_then(|node| node.get_attribute("URI"))
            .unwrap_or_default();
        let decoded = percent_decode_str(&uri).decode_utf8_lossy();
        let missing = !uri.is_empty()
            && structure::resolve_href(epub_dir, epub_dir, &decoded)
                .is_some_and(|file| !file.exists());
        if missing {
            data.unlink();
            forgotten += 1;
        } else {
            kept += 1;
        }
    }

    match (forgotten, kept) {
        (0, _) => Ok(()),
        (_, 0) => fs::remove_file(&path).map_err(|e| Error::io(&path, e)),
        _ => xml::write_file(&doc, &path, false),
    }
}

/// The algorithm of each `EncryptedData` in `encryption.xml`, and the file it
/// applies to, either empty if it names none.
///
/// Elements are matched by local name, so that one in a namespace other than
/// XML Encryption's still counts.
fn encrypted_resources(encryption_xml: &[u8]) -> Result<Vec<(String, String)>> {
    let doc = xml::parse_strict(encryption_xml)?;
    let method = format!("./{}", xml::local("EncryptionMethod"));
    let reference = format!(
        "./{}/{}",
        xml::local("CipherData"),
        xml::local("CipherReference")
    );

    let mut encrypted = Vec::new();
    for data in xml::find_nodes(&doc, &format!("//{}", xml::local("EncryptedData")))? {
        let attribute = |xpath: &str, name: &str| -> Result<String> {
            Ok(xml::find_nodes_under(&doc, &data, xpath)?
                .first()
                .and_then(|node| node.get_attribute(name))
                .unwrap_or_default())
        };
        encrypted.push((
            attribute(&method, "Algorithm")?,
            attribute(&reference, "URI")?,
        ));
    }

    Ok(encrypted)
}
