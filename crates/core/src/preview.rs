//! A book's metadata and cover, read straight from its archive.
//!
//! Listing a book needs its container, its package document and its cover,
//! so nothing else is unpacked: a book of hundreds of megabytes lists as
//! quickly as a small one.
//!
//! Every path here comes from the book, so it is resolved inside the archive
//! and never on the filesystem. An absolute href, or one climbing out with
//! `..`, simply names nothing.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use percent_encoding::percent_decode_str;
use zip::ZipArchive;

use crate::metadata::{self, Metadata};
use crate::{package, structure, xml, Error, Result};

const CONTAINER_ENTRY: &str = "META-INF/container.xml";

/// Package documents run to kilobytes; one claiming more is not to be trusted
/// with memory.
const MAX_PACKAGE_BYTES: u64 = 16 * 1024 * 1024;

/// What a book's entry in a list shows.
#[derive(Debug, Clone)]
pub struct Preview {
    pub metadata: Metadata,
    pub cover: Option<Cover>,
}

/// A cover image, as the book stores it.
#[derive(Debug, Clone)]
pub struct Cover {
    pub bytes: Vec<u8>,
    /// As the manifest declares it, which may be anything at all.
    pub media_type: String,
    /// Where in the archive it was found.
    pub path: String,
}

/// Read a book's metadata and cover without unpacking it. A cover larger than
/// `max_cover_bytes` is left out: this is for a thumbnail, not the artwork.
pub fn read_preview(epub_path: &Path, max_cover_bytes: u64) -> Result<Preview> {
    let file = File::open(epub_path).map_err(|e| Error::io(epub_path, e))?;
    let mut archive = ZipArchive::new(file)?;

    let opf_path = find_opf(&mut archive)?;
    let opf_bytes =
        read_entry(&mut archive, &opf_path, MAX_PACKAGE_BYTES)?.ok_or(Error::OpfNotFound)?;
    let opf = xml::parse_strict(&opf_bytes)?;
    let metadata = metadata::extract_metadata(&opf)?;

    let mut cover = None;
    if !metadata.cover_href.is_empty() {
        let opf_dir = opf_path.rsplit_once('/').map_or("", |(dir, _)| dir);
        if let Some(path) = resolve(opf_dir, &metadata.cover_href) {
            if let Some(bytes) = read_entry(&mut archive, &path, max_cover_bytes)? {
                let media_type = structure::manifest_items(&opf)?
                    .into_iter()
                    .find(|item| item.id == metadata.cover_id)
                    .map(|item| item.media_type)
                    .unwrap_or_default();
                cover = Some(Cover {
                    bytes,
                    media_type,
                    path,
                });
            }
        }
    }

    Ok(Preview { metadata, cover })
}

/// The package document's path in the archive: where `container.xml` says, or
/// failing that the first `.opf` there is.
fn find_opf(archive: &mut ZipArchive<File>) -> Result<String> {
    if let Some(container) = read_entry(archive, CONTAINER_ENTRY, MAX_PACKAGE_BYTES)? {
        if let Some(path) = package::opf_path_in_container(&container)? {
            return Ok(path);
        }
    }

    let mut candidates: Vec<&str> = archive
        .file_names()
        .filter(|name| name.ends_with(".opf"))
        .collect();
    candidates.sort_unstable();
    candidates
        .first()
        .map(|name| name.to_string())
        .ok_or(Error::OpfNotFound)
}

/// An entry's bytes; `None` if there is no such entry or it is larger than
/// `limit`. The size an entry declares can lie, so no more than the limit is
/// ever read.
fn read_entry(archive: &mut ZipArchive<File>, name: &str, limit: u64) -> Result<Option<Vec<u8>>> {
    let entry = match archive.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if entry.size() > limit {
        return Ok(None);
    }

    let mut bytes = Vec::new();
    entry
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| Error::Zip(e.into()))?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// The archive entry `href` names, written in a document in `dir`; `None`
/// for anything that would lie outside the archive.
fn resolve(dir: &str, href: &str) -> Option<String> {
    let path = href.split(['#', '?']).next().unwrap_or_default();
    let decoded = percent_decode_str(path).decode_utf8().ok()?;

    let mut parts: Vec<&str> = if decoded.starts_with('/') {
        Vec::new()
    } else {
        dir.split('/').filter(|part| !part.is_empty()).collect()
    };
    for part in decoded.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }

    (!parts.is_empty()).then(|| parts.join("/"))
}
