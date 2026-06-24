//! Typed errors for the installed store.
//!
//! Every fallible operation in this crate returns [`VdbError`]. The crate never
//! prints; callers render these values as they see fit.

use std::path::PathBuf;

use moraine_common::CommonError;

/// Errors produced while loading, importing, or querying the installed store.
#[derive(Debug, thiserror::Error)]
pub enum VdbError {
    /// An underlying filesystem or mmap operation failed.
    #[error(transparent)]
    Common(#[from] CommonError),

    /// A plain I/O failure annotated with the path it concerned.
    #[error("I/O error at `{path}`")]
    Io {
        /// The path the failed operation concerned.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The primary store file could not be decoded.
    #[error("failed to decode store file `{path}`")]
    DecodeStore {
        /// The store file path.
        path: PathBuf,
        /// The underlying decode error.
        #[source]
        source: rmp_serde::decode::Error,
    },

    /// A store record could not be encoded for writing.
    #[error("failed to encode store")]
    EncodeStore {
        /// The underlying encode error.
        #[source]
        source: rmp_serde::encode::Error,
    },

    /// The store file carried an unsupported format version.
    #[error("unsupported store format version {found}, expected {expected}")]
    UnsupportedVersion {
        /// The version found on disk.
        found: u32,
        /// The version this build understands.
        expected: u32,
    },

    /// A token index in a record pointed outside the intern table.
    #[error("token index {index} out of range (table holds {len} tokens)")]
    TokenOutOfRange {
        /// The offending index.
        index: u32,
        /// The number of tokens available.
        len: usize,
    },

    /// A `*DEPEND` string recorded for a package failed to parse.
    #[error("failed to parse {field} for `{package}`: {reason}")]
    DepParse {
        /// The dependency field name (for example `RDEPEND`).
        field: &'static str,
        /// The `category/package-version` the field belonged to.
        package: String,
        /// A short description of the failure.
        reason: String,
    },

    /// A version string recorded for a package failed to parse.
    #[error("failed to parse version `{version}` for `{package}`")]
    VersionParse {
        /// The version string that failed.
        version: String,
        /// The owning `category/package` directory name.
        package: String,
    },

    /// An importer encountered a malformed package directory name.
    #[error("malformed package directory name `{name}`")]
    BadPackageDir {
        /// The directory name that could not be split into name and version.
        name: String,
    },

    /// A required field was present nowhere for an imported package.
    #[error(
        "required field `{field}` missing for `{package}` (not a one-line file, not in environment)"
    )]
    MissingField {
        /// The missing field name.
        field: &'static str,
        /// The `category/package-version` the field belonged to.
        package: String,
    },

    /// A `NEEDED.ELF.2` line did not have the expected shape.
    #[error("malformed NEEDED.ELF.2 line in `{package}`: `{line}`")]
    BadNeeded {
        /// The owning package.
        package: String,
        /// The offending line.
        line: String,
    },

    /// A `CONTENTS` line did not have a recognized form.
    #[error("malformed CONTENTS line in `{package}`: `{line}`")]
    BadContents {
        /// The owning package.
        package: String,
        /// The offending line.
        line: String,
    },

    /// Decompressing a saved `environment.bz2` failed.
    #[error("failed to read environment for `{package}`")]
    Environment {
        /// The owning package.
        package: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Attach a filesystem path to an [`std::io::Result`], turning it into a
/// [`Result`] over [`VdbError`].
pub(crate) trait IoResultExt<T> {
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T, VdbError>;
}

impl<T> IoResultExt<T> for std::io::Result<T> {
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T, VdbError> {
        self.map_err(|source| VdbError::Io {
            path: path.into(),
            source,
        })
    }
}
