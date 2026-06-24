//! The typed error surface for repository synchronization.

use std::path::PathBuf;

use thiserror::Error;

/// An error raised while synchronizing one or more repositories.
///
/// The variants separate the failure domains the engine and backends care
/// about: configuration of a repository's `sync-*` options, transport while
/// fetching a tree, verification of a synced tree's signatures, and the
/// post-sync metadata refresh.
#[derive(Debug, Error)]
pub enum SyncError {
    /// A repository's `sync-*` configuration was missing or invalid.
    #[error("configuration error for repository `{repo}`: {reason}")]
    Config {
        /// The repository whose configuration was rejected.
        repo: String,
        /// A short description of the configuration problem.
        reason: String,
    },

    /// A repository declared a `sync-type` that names no implemented backend.
    #[error("repository `{repo}` declares unknown sync-type `{sync_type}`")]
    UnknownBackend {
        /// The repository whose `sync-type` could not be resolved.
        repo: String,
        /// The unrecognized `sync-type` value.
        sync_type: String,
    },

    /// A repository declared a `sync-type` for a backend that is recorded as
    /// lower priority and is not implemented in this crate.
    #[error("repository `{repo}` uses unimplemented sync-type `{sync_type}`")]
    UnimplementedBackend {
        /// The repository whose backend is unimplemented.
        repo: String,
        /// The `sync-type` value naming the unimplemented backend.
        sync_type: String,
    },

    /// A transport operation (an external fetch command) failed.
    #[error("transport failure for repository `{repo}`: {reason}")]
    Transport {
        /// The repository whose transfer failed.
        repo: String,
        /// A short description of the transport failure.
        reason: String,
    },

    /// The server reported a tree older than the local copy.
    #[error("server tree for repository `{repo}` is older than the local copy")]
    ServerOutOfDate {
        /// The repository whose server copy was stale.
        repo: String,
    },

    /// Signature or manifest verification of a synced tree failed.
    #[error("verification failure for repository `{repo}`: {reason}")]
    Verification {
        /// The repository whose verification failed.
        repo: String,
        /// A short description of the verification failure.
        reason: String,
    },

    /// A post-sync metadata refresh failed.
    #[error("metadata refresh failed for repository `{repo}`: {reason}")]
    Refresh {
        /// The repository whose refresh failed.
        repo: String,
        /// A short description of the refresh failure.
        reason: String,
    },

    /// A repository-level post-sync action failed.
    #[error("post-sync action failed for repository `{repo}`: {reason}")]
    PostSyncAction {
        /// The repository whose action failed.
        repo: String,
        /// A short description of the action failure.
        reason: String,
    },

    /// An external command could not be spawned or its output could not be read.
    #[error("failed to run `{program}`: {reason}")]
    Command {
        /// The external program that failed to run.
        program: String,
        /// A short description of the spawn or IO failure.
        reason: String,
    },

    /// A filesystem operation failed at a known path.
    #[error("I/O error at `{path}`: {reason}")]
    Io {
        /// The path the failed operation concerned.
        path: PathBuf,
        /// A short description of the IO failure.
        reason: String,
    },
}

/// A convenience result type for the crate's fallible operations.
pub type Result<T> = std::result::Result<T, SyncError>;
