//! Typed errors for repository discovery, import, and the metadata store.
//!
//! Every fallible operation in this crate returns [`RepoError`]. Library code
//! never prints; callers render these variants as they see fit.

use std::path::PathBuf;

use moraine_common::CommonError;

/// The error type for all `moraine-repo` operations.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    /// A repository configuration could not be discovered or resolved.
    #[error("repository discovery failed: {0}")]
    Discovery(#[from] DiscoveryError),

    /// A metadata import operation failed irrecoverably.
    #[error("metadata import failed: {0}")]
    Import(#[from] ImportError),

    /// A stored metadata file is structurally corrupt.
    #[error("metadata store is corrupt: {reason}")]
    StoreCorruption {
        /// A short description of the corruption.
        reason: String,
    },

    /// A stored metadata file declares an unsupported format version.
    #[error("metadata store format version mismatch: found {found}, expected {expected}")]
    FormatVersionMismatch {
        /// The format version found on disk.
        found: u32,
        /// The format version this build understands.
        expected: u32,
    },

    /// An underlying I/O or primitive operation failed.
    #[error(transparent)]
    Common(#[from] CommonError),
}

/// Reasons repository discovery can fail.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// A `repos.conf` file or fragment could not be read.
    #[error("cannot read repos.conf at `{path}`")]
    Read {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A `repos.conf` file could not be parsed as INI.
    #[error("malformed repos.conf at `{path}` line {line}: {reason}")]
    Ini {
        /// The path of the offending fragment.
        path: PathBuf,
        /// The 1-based line number.
        line: usize,
        /// A short description of the problem.
        reason: &'static str,
    },

    /// A repository section did not supply a usable `location`.
    #[error("repository `{repo}` has no usable location")]
    MissingLocation {
        /// The repository whose section omitted a location.
        repo: String,
    },

    /// The masters graph contains a cycle.
    #[error("masters graph contains a cycle among: {repos}")]
    MastersCycle {
        /// The repositories participating in the cycle, comma-separated.
        repos: String,
    },

    /// A repository referenced a master that was never declared.
    #[error("repository `{repo}` lists unknown master `{master}`")]
    UnknownMaster {
        /// The repository declaring the master.
        repo: String,
        /// The master that could not be resolved.
        master: String,
    },
}

/// Reasons a metadata import can fail irrecoverably.
///
/// Per-entry problems (corrupt cache lines, stale eclasses, EAPI violations) are
/// not fatal: they are collected as [`crate::import::ImportIssue`] values and the
/// import continues. This type covers failures that abort the whole import.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The repository location does not exist or is not a directory.
    #[error("repository location `{path}` is not a directory")]
    NotADirectory {
        /// The offending path.
        path: PathBuf,
    },

    /// Walking the md5-cache tree failed.
    #[error("cannot walk md5-cache at `{path}`")]
    Walk {
        /// The directory being walked.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: walkdir::Error,
    },

    /// Serializing the built store failed.
    #[error("cannot serialize metadata store: {reason}")]
    Serialize {
        /// A short description of the failure.
        reason: String,
    },

    /// An underlying I/O or primitive operation failed.
    #[error(transparent)]
    Common(#[from] CommonError),
}

/// A specialized result type for this crate.
pub type Result<T> = std::result::Result<T, RepoError>;
