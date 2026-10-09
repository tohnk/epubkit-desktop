//! Core EPUB optimization pipeline for e-ink readers.
//!
//! This is a Rust port of the original Python implementation. Module names
//! deliberately mirror the Python ones so the two can be diffed step for step
//! while the port is being validated:
//!
//! | Python              | Rust           |
//! |---------------------|----------------|
//! | `epub_packager.py`  | [`package`]    |
//! | `html_cleaner.py`   | [`html`]       |
//! | (lxml usage)        | [`xml`]        |

pub mod css;
pub mod error;
pub mod html;
pub mod image;
pub mod jpeg;
mod layout;
pub mod memory;
pub mod metadata;
pub mod package;
pub mod pipeline;
pub mod preview;
pub mod settings;
pub mod structure;
pub mod text;
pub mod xml;

pub use error::{Error, Result};

/// The largest document, a chapter, an SVG document, a stylesheet, a table of
/// contents or the package document, that is read whole. Parsing one takes
/// some fifteen times its size; one larger is left as it is.
pub const MAX_DOCUMENT_BYTES: u64 = 32 << 20;

/// Is the file at `path` larger than a document may be to be read whole?
pub(crate) fn too_large_to_read(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_DOCUMENT_BYTES)
}
