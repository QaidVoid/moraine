//! Shared error building blocks for Moraine library crates.
//!
//! Library crates expose typed errors and never print. [`CommonError`] covers
//! the failures produced by the primitives in this crate, and [`IoResultExt`]
//! attaches the offending path to an [`std::io::Error`] so callers can render a
//! useful message.

use std::path::{Path, PathBuf};

/// Errors produced by the shared primitives in [`crate`].
#[derive(Debug, thiserror::Error)]
pub enum CommonError {
    /// An I/O operation failed, annotated with the path it concerned.
    #[error("I/O error at `{path}`")]
    Io {
        /// The filesystem path the failed operation concerned.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Attach a filesystem path to an [`std::io::Result`], turning it into a
/// [`Result`] over [`CommonError`].
pub trait IoResultExt<T> {
    /// Convert an I/O error into [`CommonError::Io`] carrying `path`.
    fn with_path(self, path: impl AsRef<Path>) -> Result<T, CommonError>;
}

impl<T> IoResultExt<T> for std::io::Result<T> {
    fn with_path(self, path: impl AsRef<Path>) -> Result<T, CommonError> {
        self.map_err(|source| CommonError::Io {
            path: path.as_ref().to_path_buf(),
            source,
        })
    }
}
