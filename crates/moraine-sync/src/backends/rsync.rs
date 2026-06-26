//! The rsync backend.
//!
//! The rsync backend is the default. Before transferring the tree it probes
//! `metadata/timestamp.chk` with a bounded connection timeout and compares it to
//! the local copy: equal timestamps mean no change and no transfer, an older
//! server timestamp is a server-out-of-date condition, and a newer one triggers
//! a transfer into a staging directory that is committed into place only after
//! the transfer (and, when enabled, verification) succeeds. The backend shells
//! out to `rsync` through the injectable [`CommandRunner`].

use std::path::Path;

use tracing::instrument;

use crate::backend::{Backend, SyncContext};
use crate::command::{CommandRunner, CommandSpec};
use crate::error::SyncError;
use crate::outcome::{SyncKind, SyncOutcome};
use crate::verify::Verifier;

/// The standard rsync excludes stock Portage applies to a tree transfer.
const STANDARD_EXCLUDES: &[&str] = &[
    "--exclude=/distfiles",
    "--exclude=/local",
    "--exclude=/packages",
    "--exclude=/.git",
];

/// The freshness decision derived from comparing server and local timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The server and local timestamps match; the tree is current.
    Current,
    /// The server timestamp is newer; a transfer is required.
    Newer,
    /// The server timestamp is older than the local copy.
    Older,
}

/// Compare a server timestamp to the local timestamp.
pub fn classify_freshness(server: i64, local: Option<i64>) -> Freshness {
    match local {
        Some(local) if server == local => Freshness::Current,
        Some(local) if server < local => Freshness::Older,
        _ => Freshness::Newer,
    }
}

/// The rsync backend over an injected command runner.
pub struct RsyncBackend<R: CommandRunner> {
    runner: R,
}

impl<R: CommandRunner> RsyncBackend<R> {
    /// Build an rsync backend over `runner`.
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// Build the timestamp-probe command: transfer only `metadata/timestamp.chk`
    /// into the staging directory with a bounded connection timeout.
    fn probe_command(&self, ctx: &SyncContext<'_>) -> CommandSpec {
        let src = format!(
            "{}/metadata/timestamp.chk",
            ctx.options.uri.trim_end_matches('/')
        );
        let dst = ctx
            .staging
            .join("timestamp.chk")
            .to_string_lossy()
            .into_owned();
        CommandSpec::new("rsync")
            .arg(format!("--timeout={}", ctx.options.timeout_secs))
            .arg(format!("--contimeout={}", ctx.options.timeout_secs))
            .arg(src)
            .arg(dst)
    }

    /// Build the tree-transfer command into the staging directory.
    fn transfer_command(&self, ctx: &SyncContext<'_>) -> CommandSpec {
        let src = format!("{}/", ctx.options.uri.trim_end_matches('/'));
        let dst = format!("{}/", ctx.staging.to_string_lossy());
        CommandSpec::new("rsync")
            .arg("--recursive")
            .arg("--links")
            .arg("--perms")
            .arg("--times")
            .arg("--compress")
            .arg("--delete")
            .arg(format!("--timeout={}", ctx.options.timeout_secs))
            .args(STANDARD_EXCLUDES.iter().copied())
            .args(ctx.options.rsync_extra_opts.iter().cloned())
            .arg(src)
            .arg(dst)
    }

    /// Probe the server timestamp and decide freshness.
    #[instrument(skip(self, ctx), fields(repo = ctx.repo))]
    fn probe(&self, ctx: &SyncContext<'_>) -> Result<Freshness, SyncError> {
        let probe = self.probe_command(ctx);
        let out = self.runner.run(&probe)?;
        if !out.success() {
            return Err(SyncError::Transport {
                repo: ctx.repo.to_owned(),
                reason: format!("timestamp probe failed: {}", out.stderr.trim()),
            });
        }
        let server = read_timestamp(&ctx.staging.join("timestamp.chk")).ok_or_else(|| {
            SyncError::Transport {
                repo: ctx.repo.to_owned(),
                reason: "server timestamp.chk could not be read".to_owned(),
            }
        })?;
        let local = read_timestamp(&ctx.location.join("metadata/timestamp.chk"));
        Ok(classify_freshness(server, local))
    }

    /// Transfer the tree into staging, verify when required, then commit.
    #[instrument(skip(self, ctx), fields(repo = ctx.repo))]
    fn transfer_and_commit(
        &self,
        ctx: &SyncContext<'_>,
        kind: SyncKind,
    ) -> Result<SyncOutcome, SyncError> {
        let transfer = self.transfer_command(ctx);
        let out = self.runner.run(&transfer)?;
        if !out.success() {
            return Err(SyncError::Transport {
                repo: ctx.repo.to_owned(),
                reason: format!("tree transfer failed: {}", out.stderr.trim()),
            });
        }

        if ctx.options.verify_metamanifest {
            let verifier = Verifier::new(&self.runner);
            verifier.verify_rsync_tree(ctx.repo, ctx.staging, None)?;
        }

        commit_staging(ctx.repo, ctx.staging, ctx.location)?;
        let head =
            read_timestamp(&ctx.location.join("metadata/timestamp.chk")).map(|ts| ts.to_string());
        Ok(SyncOutcome::changed(kind, head))
    }
}

impl<R: CommandRunner> Backend for RsyncBackend<R> {
    fn sync_type(&self) -> &str {
        "rsync"
    }

    fn exists(&self, ctx: &SyncContext<'_>) -> bool {
        ctx.location.join("metadata/timestamp.chk").exists() || ctx.location.is_dir()
    }

    fn fetch(&self, ctx: &SyncContext<'_>) -> Result<SyncOutcome, SyncError> {
        // No local tree to probe against; transfer unconditionally.
        self.transfer_and_commit(ctx, SyncKind::Initial)
    }

    fn update(&self, ctx: &SyncContext<'_>) -> Result<SyncOutcome, SyncError> {
        match self.probe(ctx)? {
            Freshness::Current => Ok(SyncOutcome::unchanged(SyncKind::Update)),
            Freshness::Older => Err(SyncError::ServerOutOfDate {
                repo: ctx.repo.to_owned(),
            }),
            Freshness::Newer => self.transfer_and_commit(ctx, SyncKind::Update),
        }
    }

    fn retrieve_head(&self, ctx: &SyncContext<'_>) -> Result<Option<String>, SyncError> {
        Ok(read_timestamp(&ctx.location.join("metadata/timestamp.chk")).map(|ts| ts.to_string()))
    }
}

/// Read the integer timestamp from a `timestamp.chk` file, ignoring a trailing
/// human-readable suffix.
fn read_timestamp(path: &Path) -> Option<i64> {
    let content = std::fs::read_to_string(path).ok()?;
    content
        .split_whitespace()
        .next()
        .and_then(|t| t.parse::<i64>().ok())
        .or_else(|| content.trim().parse::<i64>().ok())
}

/// Commit a staged tree into place by replacing the live location atomically on
/// the same filesystem.
fn commit_staging(repo: &str, staging: &Path, location: &Path) -> Result<(), SyncError> {
    if let Some(parent) = location.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SyncError::Io {
            path: parent.to_path_buf(),
            reason: source.to_string(),
        })?;
    }
    // Remove the timestamp probe artifact so it does not leak into the tree.
    let _ = std::fs::remove_file(staging.join("timestamp.chk"));

    if location.exists() {
        let backup = location.with_extension("moraine-old");
        let _ = std::fs::remove_dir_all(&backup);
        std::fs::rename(location, &backup).map_err(|source| SyncError::Io {
            path: location.to_path_buf(),
            reason: format!("could not move prior tree aside: {source}"),
        })?;
        match std::fs::rename(staging, location) {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(&backup);
                Ok(())
            }
            Err(source) => {
                // Restore the prior tree to preserve a known-good state.
                let _ = std::fs::rename(&backup, location);
                Err(SyncError::Io {
                    path: location.to_path_buf(),
                    reason: format!("could not commit staged tree for `{repo}`: {source}"),
                })
            }
        }
    } else {
        std::fs::rename(staging, location).map_err(|source| SyncError::Io {
            path: location.to_path_buf(),
            reason: format!("could not commit staged tree for `{repo}`: {source}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_current_when_equal() {
        assert_eq!(classify_freshness(100, Some(100)), Freshness::Current);
    }

    #[test]
    fn freshness_older_when_server_behind() {
        assert_eq!(classify_freshness(90, Some(100)), Freshness::Older);
    }

    #[test]
    fn freshness_newer_when_server_ahead_or_no_local() {
        assert_eq!(classify_freshness(110, Some(100)), Freshness::Newer);
        assert_eq!(classify_freshness(110, None), Freshness::Newer);
    }
}
