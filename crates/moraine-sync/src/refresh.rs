//! Post-sync metadata refresh.
//!
//! On a successful, changed sync the engine refreshes the greenfield metadata
//! store for the repository. The refresh delegates to the `moraine-repo`
//! incremental importer, which reuses entries whose `_mtime_` and `_md5_` match
//! the source cache and re-parses only the changed ebuilds. When the existing
//! store cannot be read, or the caller forces it, the refresh falls back to a
//! full reimport with no prior entries.
//!
//! The refresh is expressed through the [`MetadataRefresher`] trait so the engine
//! can be tested against a fake that records which repositories were refreshed
//! and whether the incremental or full path ran. [`RepoRefresher`] is the
//! production implementation backed by `moraine-repo`.

use std::path::{Path, PathBuf};

use moraine_repo::{RepoSet, import_repo, previous_index, store};
use tracing::instrument;

use crate::error::SyncError;

/// The mode a refresh used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// Reused prior entries and re-parsed only changed ebuilds.
    Incremental,
    /// Reimported the whole repository with no prior entries.
    Full,
}

/// The outcome of a metadata refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshReport {
    /// Whether the refresh ran incrementally or as a full reimport.
    pub mode: RefreshMode,
    /// The number of entries in the refreshed store.
    pub entries: usize,
}

/// Refreshes the greenfield metadata store for one repository.
pub trait MetadataRefresher: Send + Sync {
    /// Refresh `repo`'s metadata. When `force_full` is set the refresh must use
    /// the full-reimport path rather than incremental reuse.
    fn refresh(&self, repo: &str, force_full: bool) -> Result<RefreshReport, SyncError>;
}

/// The production [`MetadataRefresher`] backed by `moraine-repo`.
pub struct RepoRefresher<'a> {
    repo_set: &'a RepoSet,
    store_dir: PathBuf,
}

impl<'a> RepoRefresher<'a> {
    /// Build a refresher writing per-repository store files under `store_dir`.
    pub fn new(repo_set: &'a RepoSet, store_dir: impl AsRef<Path>) -> Self {
        Self {
            repo_set,
            store_dir: store_dir.as_ref().to_path_buf(),
        }
    }

    /// The store file path for `repo`.
    fn store_path(&self, repo: &str) -> PathBuf {
        self.store_dir.join(format!("{repo}.mrepo"))
    }
}

impl MetadataRefresher for RepoRefresher<'_> {
    #[instrument(skip(self), fields(repo = repo))]
    fn refresh(&self, repo: &str, force_full: bool) -> Result<RefreshReport, SyncError> {
        std::fs::create_dir_all(&self.store_dir).map_err(|source| SyncError::Io {
            path: self.store_dir.clone(),
            reason: source.to_string(),
        })?;

        let store_path = self.store_path(repo);

        // Seed incremental reuse from the existing store. When the store cannot
        // be read its recorded state is inconsistent with the repository, so we
        // fall back to a full reimport with no prior entries.
        let (previous, mode) = if force_full {
            (std::collections::HashMap::new(), RefreshMode::Full)
        } else {
            match store::read_entries(&store_path) {
                Ok(entries) => (previous_index(&entries), RefreshMode::Incremental),
                Err(_) => (std::collections::HashMap::new(), RefreshMode::Full),
            }
        };

        let report =
            import_repo(self.repo_set, repo, &previous).map_err(|source| SyncError::Refresh {
                repo: repo.to_owned(),
                reason: source.to_string(),
            })?;

        let entries = report.entries.len();
        store::write_store(&store_path, report.entries).map_err(|source| SyncError::Refresh {
            repo: repo.to_owned(),
            reason: source.to_string(),
        })?;

        Ok(RefreshReport { mode, entries })
    }
}
