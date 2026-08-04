use std::path::{Path, PathBuf};

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("unsafe path in EPUB archive: {0}")]
    UnsafeArchivePath(String),

    #[error("not a valid EPUB: {0}")]
    InvalidEpub(String),

    #[error("no OPF package document found in EPUB")]
    OpfNotFound,

    #[error("XML parse error: {0}")]
    Xml(String),
}

impl Error {
    pub(crate) fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.as_ref().to_path_buf(),
            source,
        }
    }
}
