//! Repository synchronization for the Moraine package manager.
//!
//! This crate is the `portage.sync` equivalent of the rewrite. It orders the
//! repositories discovered by [`moraine_repo`] so every master is synced before
//! its dependents (with `priority` breaking ties), skips `auto-sync=false`
//! repositories unless they are named explicitly, resolves each repository's
//! effective `sync-*` options, and dispatches each to a backend chosen by its
//! `sync-type`. Each backend returns a typed [`outcome::SyncOutcome`] with a
//! `changed` flag and the new head revision when known, replacing the stock
//! `(exitcode, bool)` tuple.
//!
//! The crate ships three backends: [`backends::RsyncBackend`] (the default, with
//! a `metadata/timestamp.chk` freshness probe and stage-then-commit),
//! [`backends::GitBackend`] (shallow clone, fetch-and-merge, change by HEAD
//! movement), and [`backends::WebrsyncBackend`] (the external `emerge-webrsync`
//! helper). The `cvs`, `svn`, and `mercurial` backends are recorded as lower
//! priority and are left unimplemented; the engine treats them as unknown.
//!
//! Verification gates the committed tree: the rsync backend verifies the staged
//! tree's signed manifest before commit, the git backend verifies the head
//! commit signature, and the webrsync backend relies on the snapshot's detached
//! signature, all via [`verify`]. On a successful, changed sync the engine
//! records the head revision in a bounded [`revision::RevisionHistory`] and runs
//! the post-sync metadata [`refresh`] through the `moraine-repo` incremental
//! importer, with a full-refresh fallback.
//!
//! There is no async runtime. Concurrency for independent transfers comes from
//! the bounded worker pool in [`pool`]. All external tools are invoked through
//! the injectable [`command::CommandRunner`] so the backends can be tested
//! against recorded command behavior rather than a live network.

pub mod backend;
pub mod backends;
pub mod command;
pub mod engine;
pub mod error;
pub mod extras;
pub mod manifest;
pub mod options;
pub mod outcome;
pub mod pool;
pub mod refresh;
pub mod revision;
pub mod verify;

#[cfg(test)]
mod tests;

pub use backend::{Backend, BackendRegistry, SyncContext, UNIMPLEMENTED_BACKENDS};
pub use backends::{GitBackend, RsyncBackend, WebrsyncBackend};
pub use command::{CommandOutput, CommandRunner, CommandSpec, SystemRunner};
pub use engine::{RepoResult, SyncEngine, SyncReport};
pub use error::{Result, SyncError};
pub use extras::{ExtrasMap, RepoExtras};
pub use options::{KeyRefresh, SyncDefaults, SyncOptions};
pub use outcome::{SyncKind, SyncOutcome};
pub use refresh::{MetadataRefresher, RefreshMode, RefreshReport, RepoRefresher};
pub use revision::{HISTORY_LIMIT, RevisionHistory};
pub use verify::{GitSigStatus, Verifier};

/// Build a [`BackendRegistry`] with the three primary backends, each driven by a
/// clone of `runner`.
pub fn default_registry<R>(runner: R) -> BackendRegistry<'static>
where
    R: CommandRunner + Clone + 'static,
{
    BackendRegistry::new(vec![
        Box::new(RsyncBackend::new(runner.clone())),
        Box::new(GitBackend::new(runner.clone())),
        Box::new(WebrsyncBackend::new(runner)),
    ])
}
