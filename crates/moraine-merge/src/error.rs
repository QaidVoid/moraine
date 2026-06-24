//! Typed errors for the merge engine.
//!
//! Every fallible operation in this crate returns [`MergeError`]. The crate
//! never prints; callers render these values as they see fit.

use std::path::PathBuf;

use moraine_common::CommonError;
use moraine_vdb::VdbError;

/// Errors produced while merging, unmerging, or recording installed state.
#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    /// An underlying shared-primitive failure (atomic write, mmap).
    #[error(transparent)]
    Common(#[from] CommonError),

    /// An installed-store failure surfaced from `moraine-vdb`.
    #[error(transparent)]
    Vdb(#[from] VdbError),

    /// A plain I/O failure annotated with the path it concerned.
    #[error("I/O error at `{path}`")]
    Io {
        /// The path the failed operation concerned.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A merge was refused because collision protection found conflicting paths.
    ///
    /// The live root is left untouched: collision checks run before any mutation.
    #[error("collision protection aborted the merge: {} conflicting path(s)", paths.len())]
    Collision {
        /// The conflicting target paths, relative to the install root.
        paths: Vec<String>,
    },

    /// The lock guarding the installed store could not be acquired.
    #[error("could not acquire the installed-store lock at `{path}`")]
    Lock {
        /// The lock file path.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The image directory named by an operation does not exist.
    #[error("image directory `{path}` does not exist")]
    MissingImage {
        /// The missing image directory.
        path: PathBuf,
    },

    /// A package version string could not be parsed when recording state.
    #[error("invalid version `{version}` for `{package}`")]
    Version {
        /// The version string that failed to parse.
        version: String,
        /// The `category/package` it belonged to.
        package: String,
    },

    /// The preserved-libs registry on disk failed to decode.
    #[error("preserved-libs registry at `{path}` is corrupt")]
    Registry {
        /// The registry file path.
        path: PathBuf,
    },
}

/// Attach a filesystem path to an [`std::io::Result`], turning it into a
/// [`Result`] over [`MergeError`].
pub(crate) trait IoResultExt<T> {
    /// Convert an I/O error into [`MergeError::Io`] carrying `path`.
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T, MergeError>;
}

impl<T> IoResultExt<T> for std::io::Result<T> {
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T, MergeError> {
        self.map_err(|source| MergeError::Io {
            path: path.into(),
            source,
        })
    }
}
