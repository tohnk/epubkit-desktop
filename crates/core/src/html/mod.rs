//! XHTML repair.
//!
//! EPUB content documents are nominally XHTML but in practice are often
//! malformed — unclosed tags, stray ampersands, mismatched nesting. The
//! original Python leaned on lxml's recovery parser for this. Repair sits
//! behind a trait so the backend can be swapped (a pure-Rust implementation,
//! say) without touching the pipeline that calls it.

mod libxml_repair;

pub use libxml_repair::{parse_content, serialize_content, LibxmlRepair};

use libxml::tree::Document;

use crate::Result;

/// A parsed EPUB content document, and whether it needed recovering.
///
/// Carrying the flag matters for serialization: a recovered document has had
/// its XML declaration stripped along with the HTML parser's other artifacts,
/// so one has to be written back.
pub struct ContentDocument {
    pub doc: Document,
    pub recovered: bool,
}

/// The result of repairing one XHTML document.
#[derive(Debug, Clone)]
pub struct Repaired {
    /// Serialized XHTML.
    pub bytes: Vec<u8>,
    /// `true` when the input failed a strict XML parse and had to be recovered.
    /// Useful for reporting which files in a book were actually broken.
    pub recovered: bool,
}

/// A backend that can parse possibly-malformed XHTML and serialize it back.
pub trait HtmlRepair: Send + Sync {
    /// Short backend name, for diagnostics.
    fn name(&self) -> &'static str;

    /// Parse `input` and serialize it back as XHTML.
    ///
    /// Implementations must round-trip well-formed input unchanged in meaning,
    /// and must not reflow or re-indent text content — whitespace inside an
    /// EPUB's markup is frequently significant.
    fn repair(&self, input: &[u8]) -> Result<Repaired>;
}

/// The default backend.
pub fn default_backend() -> impl HtmlRepair {
    LibxmlRepair::new()
}
