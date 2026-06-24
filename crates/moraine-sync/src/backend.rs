//! The backend trait and registry.
//!
//! A backend mirrors the small stock sync contract without its event-loop
//! machinery: an existence check that decides between an initial fetch and an
//! update, the fetch and update operations themselves, head retrieval for the
//! revision history, and tree verification that gates the commit. Every
//! operation returns a typed [`SyncOutcome`] or a typed [`SyncError`] rather
//! than the stock `(exitcode, bool)` tuple.

use std::path::Path;

use crate::error::SyncError;
use crate::options::SyncOptions;
use crate::outcome::SyncOutcome;

/// The context a backend needs to synchronize one repository.
pub struct SyncContext<'a> {
    /// The repository name, used in spans and errors.
    pub repo: &'a str,
    /// The on-disk repository location.
    pub location: &'a Path,
    /// A staging directory on the same filesystem for stage-then-commit.
    pub staging: &'a Path,
    /// The resolved `sync-*` options for this repository.
    pub options: &'a SyncOptions,
}

/// A synchronization backend for one `sync-type`.
pub trait Backend: Send + Sync {
    /// The `sync-type` value this backend implements.
    fn sync_type(&self) -> &str;

    /// Whether the repository already exists on disk, which decides between an
    /// initial fetch and an update.
    fn exists(&self, ctx: &SyncContext<'_>) -> bool;

    /// Perform an initial fetch into a non-existent repository location.
    fn fetch(&self, ctx: &SyncContext<'_>) -> Result<SyncOutcome, SyncError>;

    /// Update an existing repository in place.
    fn update(&self, ctx: &SyncContext<'_>) -> Result<SyncOutcome, SyncError>;

    /// Synchronize the repository, dispatching to [`Backend::fetch`] or
    /// [`Backend::update`] by existence. The default implementation matches the
    /// stock `NewBase.sync` dispatch.
    fn sync(&self, ctx: &SyncContext<'_>) -> Result<SyncOutcome, SyncError> {
        if self.exists(ctx) {
            self.update(ctx)
        } else {
            self.fetch(ctx)
        }
    }

    /// Retrieve the current head revision of the synced tree, when known.
    fn retrieve_head(&self, ctx: &SyncContext<'_>) -> Result<Option<String>, SyncError>;
}

/// The set of lower-priority backends recorded as out of scope for this change.
pub const UNIMPLEMENTED_BACKENDS: &[&str] = &["cvs", "svn", "mercurial"];

/// Resolves a `sync-type` value to a backend implementation.
///
/// The `'b` lifetime lets the registry hold backends that borrow a command
/// runner, which the engine and tests rely on.
pub struct BackendRegistry<'b> {
    backends: Vec<Box<dyn Backend + 'b>>,
}

impl<'b> BackendRegistry<'b> {
    /// Build a registry from the given backends.
    pub fn new(backends: Vec<Box<dyn Backend + 'b>>) -> Self {
        Self { backends }
    }

    /// Look up the backend for `sync_type`, distinguishing an unimplemented
    /// lower-priority backend from a wholly unknown one.
    pub fn resolve<'a>(
        &'a self,
        repo: &str,
        sync_type: &str,
    ) -> Result<&'a dyn Backend, SyncError> {
        if let Some(backend) = self.backends.iter().find(|b| b.sync_type() == sync_type) {
            return Ok(backend.as_ref());
        }
        if UNIMPLEMENTED_BACKENDS.contains(&sync_type) {
            return Err(SyncError::UnimplementedBackend {
                repo: repo.to_owned(),
                sync_type: sync_type.to_owned(),
            });
        }
        Err(SyncError::UnknownBackend {
            repo: repo.to_owned(),
            sync_type: sync_type.to_owned(),
        })
    }
}
