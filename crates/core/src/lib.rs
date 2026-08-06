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
pub mod metadata;
pub mod package;
pub mod structure;
pub mod text;
pub mod xml;

pub use error::{Error, Result};
