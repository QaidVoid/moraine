//! Typed errors for the binary-package layer.
//!
//! Every failure mode in this crate surfaces as a variant of one of these
//! enums. The crate never prints; callers render these errors as they see fit.

use std::path::PathBuf;

/// Errors produced while reading or writing a binary-package container.
#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    /// An I/O operation failed, annotated with the path it concerned.
    #[error("I/O error at `{path}`")]
    Io {
        /// The filesystem path the failed operation concerned.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// An I/O operation failed without a meaningful path.
    #[error("I/O error")]
    IoBare(#[source] std::io::Error),

    /// The container could not be recognized as any supported format.
    #[error("unrecognized binary package format")]
    UnknownFormat,

    /// A greenfield container header or section was malformed.
    #[error("malformed greenfield container: {0}")]
    MalformedGreenfield(String),

    /// A serialized structure could not be decoded.
    #[error("decode error: {0}")]
    Decode(String),

    /// A structure could not be encoded.
    #[error("encode error: {0}")]
    Encode(String),

    /// An xpak blob was malformed.
    #[error("malformed xpak blob: {0}")]
    MalformedXpak(String),

    /// A GPKG container was malformed.
    #[error("malformed gpkg container: {0}")]
    MalformedGpkg(String),

    /// A required member was absent from the container.
    #[error("missing container member `{0}`")]
    MissingMember(String),

    /// A compression codec was named that this crate does not support.
    #[error("unsupported compression codec `{0}`")]
    UnsupportedCompression(String),

    /// A section checksum did not match the recorded manifest value.
    #[error("integrity check failed for section `{section}`: expected {expected}, got {actual}")]
    IntegrityMismatch {
        /// The section whose checksum mismatched.
        section: String,
        /// The checksum recorded in the manifest.
        expected: String,
        /// The checksum recomputed from the section bytes.
        actual: String,
    },

    /// A signature was required or present but failed verification.
    #[error("signature verification failed: {0}")]
    Signature(String),

    /// A wrapped error from the shared primitives crate.
    #[error(transparent)]
    Common(#[from] moraine_common::CommonError),
}

/// Errors produced while parsing or emitting a binhost `Packages` index.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// The index declared a version newer than this crate supports.
    #[error("unsupported index version {found} (supported up to {supported})")]
    UnsupportedVersion {
        /// The version declared in the index header.
        found: u32,
        /// The newest version this crate understands.
        supported: u32,
    },

    /// A required key was absent from a stanza or the header.
    #[error("missing required index key `{0}`")]
    MissingKey(String),

    /// An I/O operation failed, annotated with the path it concerned.
    #[error("I/O error at `{path}`")]
    Io {
        /// The filesystem path the failed operation concerned.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A wrapped error from the shared primitives crate.
    #[error(transparent)]
    Common(#[from] moraine_common::CommonError),
}

/// Errors produced while fetching a binary package from a remote binhost.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The configured fetch command could not be launched.
    #[error("failed to launch fetch command `{command}`")]
    Launch {
        /// The command that could not be launched.
        command: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The fetch command exited with a non-zero status.
    #[error("fetch command failed with status {status}: {stderr}")]
    Command {
        /// The exit status code, or -1 when terminated by a signal.
        status: i32,
        /// The captured standard error output.
        stderr: String,
    },

    /// The fetched artifact failed Manifest or signature verification.
    #[error("verification of fetched artifact failed: {0}")]
    Verification(#[source] ContainerError),

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
/// [`Result`] over [`ContainerError`].
pub(crate) trait IoResultExt<T> {
    /// Convert an I/O error into [`ContainerError::Io`] carrying `path`.
    fn with_path(self, path: impl AsRef<std::path::Path>) -> Result<T, ContainerError>;
}

impl<T> IoResultExt<T> for std::io::Result<T> {
    fn with_path(self, path: impl AsRef<std::path::Path>) -> Result<T, ContainerError> {
        self.map_err(|source| ContainerError::Io {
            path: path.as_ref().to_path_buf(),
            source,
        })
    }
}
