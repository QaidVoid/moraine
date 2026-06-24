//! The webrsync backend.
//!
//! The webrsync backend wraps the external `emerge-webrsync` helper, which
//! downloads a signed snapshot of the repository and verifies its detached GPG
//! signature before unpacking it into the repository location. A successful sync
//! always reports the tree as changed, matching stock behavior. It shells out to
//! `emerge-webrsync` through the injectable [`CommandRunner`].

use tracing::instrument;

use crate::backend::{Backend, SyncContext};
use crate::command::{CommandRunner, CommandSpec};
use crate::error::SyncError;
use crate::outcome::{SyncKind, SyncOutcome};

/// The webrsync backend over an injected command runner.
pub struct WebrsyncBackend<R: CommandRunner> {
    runner: R,
}

impl<R: CommandRunner> WebrsyncBackend<R> {
    /// Build a webrsync backend over `runner`.
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// Invoke `emerge-webrsync` for the repository. The helper retrieves the
    /// signed snapshot, verifies its signature, and unpacks it into place.
    #[instrument(skip(self, ctx), fields(repo = ctx.repo))]
    fn run_helper(&self, ctx: &SyncContext<'_>) -> Result<SyncOutcome, SyncError> {
        let spec = CommandSpec::new("emerge-webrsync")
            .arg("--repo")
            .arg(ctx.repo.to_owned());
        let out = self.runner.run(&spec)?;
        if !out.success() {
            // A signature rejection in the helper is reported as a verification
            // failure so the engine preserves the prior tree.
            let reason = out.stderr.trim().to_owned();
            if reason.to_lowercase().contains("signature") || reason.to_lowercase().contains("gpg")
            {
                return Err(SyncError::Verification {
                    repo: ctx.repo.to_owned(),
                    reason,
                });
            }
            return Err(SyncError::Transport {
                repo: ctx.repo.to_owned(),
                reason: format!("emerge-webrsync failed: {reason}"),
            });
        }
        Ok(SyncOutcome::changed(SyncKind::Initial, None))
    }
}

impl<R: CommandRunner> Backend for WebrsyncBackend<R> {
    fn sync_type(&self) -> &str {
        "webrsync"
    }

    fn exists(&self, ctx: &SyncContext<'_>) -> bool {
        ctx.location.is_dir()
    }

    fn fetch(&self, ctx: &SyncContext<'_>) -> Result<SyncOutcome, SyncError> {
        self.run_helper(ctx)
    }

    fn update(&self, ctx: &SyncContext<'_>) -> Result<SyncOutcome, SyncError> {
        self.run_helper(ctx).map(|o| SyncOutcome {
            kind: SyncKind::Update,
            ..o
        })
    }

    fn retrieve_head(&self, _ctx: &SyncContext<'_>) -> Result<Option<String>, SyncError> {
        // The snapshot helper does not report a head revision.
        Ok(None)
    }
}
