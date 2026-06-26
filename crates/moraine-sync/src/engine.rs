//! The sync engine.
//!
//! The engine orders repositories so every master is synced before its
//! dependents (with `priority` breaking ties, via `moraine-repo` discovery),
//! skips `auto-sync=false` repositories unless they are named explicitly,
//! resolves each repository's effective `sync-*` options, dispatches to the
//! backend chosen by `sync-type`, and gathers a per-repository typed result. An
//! unknown or unimplemented `sync-type`, a transport failure, or a verification
//! failure is isolated to that repository and does not abort the others. On a
//! successful, changed sync the engine records the head revision and triggers the
//! post-sync metadata refresh and repository-level post-sync actions.

use std::path::{Path, PathBuf};

use moraine_repo::{RepoConfig, RepoSet};
use tracing::{info_span, instrument};

use crate::backend::{BackendRegistry, SyncContext};
use crate::command::{CommandRunner, CommandSpec};
use crate::error::SyncError;
use crate::extras::ExtrasMap;
use crate::options::{SyncDefaults, SyncOptions};
use crate::outcome::SyncOutcome;
use crate::refresh::{MetadataRefresher, RefreshReport};
use crate::revision::RevisionHistory;

/// What the engine did with one repository.
#[derive(Debug)]
pub enum RepoResult {
    /// The repository was skipped because `auto-sync` was disabled and it was
    /// not named explicitly.
    Skipped,
    /// The repository synced successfully.
    Synced {
        /// The backend outcome.
        outcome: SyncOutcome,
        /// The post-sync refresh report, present only when the tree changed.
        refresh: Option<RefreshReport>,
    },
    /// The repository failed to sync, verify, or refresh.
    Failed(SyncError),
}

impl RepoResult {
    /// Whether this result is a success.
    pub fn is_synced(&self) -> bool {
        matches!(self, RepoResult::Synced { .. })
    }
}

/// The per-repository results of an engine run, in processing order.
#[derive(Debug, Default)]
pub struct SyncReport {
    /// `(repository name, result)` pairs in the order processed.
    pub results: Vec<(String, RepoResult)>,
}

impl SyncReport {
    /// The result for `repo`, when present.
    pub fn get(&self, repo: &str) -> Option<&RepoResult> {
        self.results
            .iter()
            .find(|(name, _)| name == repo)
            .map(|(_, r)| r)
    }
}

/// Drives synchronization of a set of repositories.
pub struct SyncEngine<'a, 'b, R: CommandRunner, M: MetadataRefresher> {
    repo_set: &'a RepoSet,
    registry: &'a BackendRegistry<'b>,
    refresher: &'a M,
    runner: &'a R,
    defaults: SyncDefaults,
    staging_root: PathBuf,
    extras: ExtrasMap,
}

impl<'a, 'b, R: CommandRunner, M: MetadataRefresher> SyncEngine<'a, 'b, R, M> {
    /// Build an engine over the discovered repositories, a backend registry, a
    /// metadata refresher, and a command runner for post-sync actions. Staging
    /// directories are created under `staging_root`.
    pub fn new(
        repo_set: &'a RepoSet,
        registry: &'a BackendRegistry<'b>,
        refresher: &'a M,
        runner: &'a R,
        staging_root: impl AsRef<Path>,
    ) -> Self {
        Self {
            repo_set,
            registry,
            refresher,
            runner,
            defaults: SyncDefaults::default(),
            staging_root: staging_root.as_ref().to_path_buf(),
            extras: ExtrasMap::new(),
        }
    }

    /// Override the engine-wide `sync-*` defaults.
    pub fn with_defaults(mut self, defaults: SyncDefaults) -> Self {
        self.defaults = defaults;
        self
    }

    /// Supply the `auto-sync` and `post-sync` settings parsed from the raw
    /// `repos.conf`, which the discovery model does not retain.
    pub fn with_extras(mut self, extras: ExtrasMap) -> Self {
        self.extras = extras;
        self
    }

    /// Synchronize every `auto-sync` repository in dependency order.
    #[instrument(skip(self))]
    pub fn sync_all(&self, history: &mut RevisionHistory) -> SyncReport {
        self.run(None, history)
    }

    /// Synchronize only the named repositories, in dependency order, regardless
    /// of their `auto-sync` value.
    #[instrument(skip(self, names))]
    pub fn sync_named(&self, names: &[String], history: &mut RevisionHistory) -> SyncReport {
        self.run(Some(names), history)
    }

    /// The shared driver. `explicit` is `Some` when the caller named
    /// repositories; those are synced regardless of `auto-sync`.
    fn run(&self, explicit: Option<&[String]>, history: &mut RevisionHistory) -> SyncReport {
        let mut report = SyncReport::default();
        for cfg in self.repo_set.ordered() {
            let named = explicit.map(|names| names.iter().any(|n| n == &cfg.name));
            // When an explicit selection is given, only process named repos.
            if let Some(false) = named {
                continue;
            }
            let explicitly_named = named == Some(true);
            let result = self.process_repo(cfg, explicitly_named, history);
            report.results.push((cfg.name.clone(), result));
        }
        report
    }

    /// Process one repository: resolve options, apply auto-sync selection,
    /// dispatch to the backend, then run post-sync work on a changed success.
    fn process_repo(
        &self,
        cfg: &RepoConfig,
        explicitly_named: bool,
        history: &mut RevisionHistory,
    ) -> RepoResult {
        let span = info_span!("sync_repo", repo = %cfg.name);
        let _enter = span.enter();

        let mut options = match SyncOptions::resolve(cfg, &self.defaults) {
            Ok(o) => o,
            Err(e) => return RepoResult::Failed(e),
        };

        // Apply `auto-sync`/`post-sync` from the raw repos.conf extras.
        let extras = self.extras.get(&cfg.name);
        if let Some(auto) = extras.auto_sync {
            options.auto_sync = auto;
        }
        if let Some(post) = extras.post_sync {
            options.post_sync = Some(post);
        }
        options.volatile = extras.volatile;

        if !options.auto_sync && !explicitly_named {
            return RepoResult::Skipped;
        }

        let backend = match self.registry.resolve(&cfg.name, &options.sync_type) {
            Ok(b) => b,
            Err(e) => return RepoResult::Failed(e),
        };

        let staging = self.staging_root.join(&cfg.name);
        if let Err(e) = self.prepare_staging(&cfg.name, &staging) {
            return RepoResult::Failed(e);
        }

        let ctx = SyncContext {
            repo: &cfg.name,
            location: &cfg.location,
            staging: &staging,
            options: &options,
        };

        let outcome = match backend.sync(&ctx) {
            Ok(o) => o,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                return RepoResult::Failed(e);
            }
        };
        let _ = std::fs::remove_dir_all(&staging);

        // Record revision history: use the outcome head, else query the backend.
        // A volatile (user-managed) repository records no revision history.
        let head = match &outcome.head {
            Some(h) => Some(h.clone()),
            None => backend.retrieve_head(&ctx).ok().flatten(),
        };
        if !options.volatile {
            history.record(&cfg.name, head.as_deref());
        }

        let refresh = if outcome.changed {
            match self.refresher.refresh(&cfg.name, false) {
                Ok(r) => Some(r),
                Err(e) => return RepoResult::Failed(e),
            }
        } else {
            None
        };

        // Repository-level post-sync actions run after the refresh. A failure is
        // reported without discarding the synced tree or refreshed metadata.
        if let Some(action) = &options.post_sync
            && let Err(e) = self.run_post_sync_action(&cfg.name, action)
        {
            return RepoResult::Failed(e);
        }

        RepoResult::Synced { outcome, refresh }
    }

    /// Create a clean staging directory for the repository on the same staging
    /// root filesystem.
    fn prepare_staging(&self, repo: &str, staging: &Path) -> Result<(), SyncError> {
        let _ = std::fs::remove_dir_all(staging);
        std::fs::create_dir_all(staging).map_err(|source| SyncError::Io {
            path: staging.to_path_buf(),
            reason: format!("could not create staging for `{repo}`: {source}"),
        })
    }

    /// Run a repository-level post-sync action.
    fn run_post_sync_action(&self, repo: &str, action: &[String]) -> Result<(), SyncError> {
        let Some((program, args)) = action.split_first() else {
            return Ok(());
        };
        let spec = CommandSpec::new(program.clone()).args(args.iter().cloned());
        let out = self.runner.run(&spec)?;
        if out.success() {
            Ok(())
        } else {
            Err(SyncError::PostSyncAction {
                repo: repo.to_owned(),
                reason: out.stderr.trim().to_owned(),
            })
        }
    }
}
