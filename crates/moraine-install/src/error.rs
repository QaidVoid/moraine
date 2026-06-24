//! Error types for the install orchestrator.

use std::path::PathBuf;

use thiserror::Error;

/// The result type used throughout the install orchestrator.
pub type Result<T> = std::result::Result<T, InstallError>;

/// A failure during a write transaction.
#[derive(Debug, Error)]
pub enum InstallError {
    /// Another transaction holds the lock.
    #[error("another transaction holds the install lock at {path}")]
    Locked {
        /// The lock file path.
        path: PathBuf,
    },

    /// A filesystem operation failed.
    #[error("io error at {path}: {source}")]
    Io {
        /// The path the operation concerned.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },

    /// Realizing a task into an installable image failed.
    #[error("failed to prepare {cpv}: {reason}")]
    Realize {
        /// The package the failure concerned.
        cpv: String,
        /// A human-readable reason.
        reason: String,
    },

    /// Applying an operation through the merge engine failed.
    #[error("failed to apply {cpv}: {source}")]
    Merge {
        /// The package the failure concerned.
        cpv: String,
        /// The merge-engine error.
        source: moraine_merge::MergeError,
    },

    /// The resume journal could not be read or written.
    #[error("journal error at {path}: {reason}")]
    Journal {
        /// The journal path.
        path: PathBuf,
        /// A human-readable reason.
        reason: String,
    },
}

impl InstallError {
    /// Build an [`InstallError::Io`] from a path and source.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        InstallError::Io {
            path: path.into(),
            source,
        }
    }
}
