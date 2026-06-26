//! The git backend.
//!
//! The git backend clones shallowly by default (`--depth 1`), honoring
//! `sync-depth`/`clone-depth` where a depth of zero requests full history. For
//! an existing repository it fetches and merges, sets `safe.directory` for the
//! location first, and detects change by comparing the head revision before and
//! after the operation. When commit-signature verification is enabled it checks
//! the head commit signature before accepting the merged result. It shells out
//! to `git` through the injectable [`CommandRunner`].

use tracing::instrument;

use crate::backend::{Backend, SyncContext};
use crate::command::{CommandRunner, CommandSpec};
use crate::error::SyncError;
use crate::outcome::{SyncKind, SyncOutcome};
use crate::verify::Verifier;

/// The git backend over an injected command runner.
pub struct GitBackend<R: CommandRunner> {
    runner: R,
}

impl<R: CommandRunner> GitBackend<R> {
    /// Build a git backend over `runner`.
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// The depth arguments for a clone or fetch, honoring `sync-depth`. A depth
    /// of zero requests full history (`--unshallow` semantics expressed as no
    /// depth limit); `None` defaults to shallow depth one.
    fn depth_args(&self, ctx: &SyncContext<'_>) -> Vec<String> {
        match ctx.options.depth {
            Some(0) => Vec::new(),
            Some(n) => vec![format!("--depth={n}")],
            None => vec!["--depth=1".to_owned()],
        }
    }

    /// Mark the repository location as a safe git directory before operating on
    /// it, matching stock behavior for repositories owned by another user.
    fn set_safe_directory(&self, ctx: &SyncContext<'_>) -> Result<(), SyncError> {
        let spec = CommandSpec::new("git")
            .arg("config")
            .arg("--global")
            .arg("--add")
            .arg("safe.directory")
            .arg(ctx.location.to_string_lossy().into_owned());
        let out = self.runner.run(&spec)?;
        if out.success() {
            Ok(())
        } else {
            Err(SyncError::Transport {
                repo: ctx.repo.to_owned(),
                reason: format!("could not set safe.directory: {}", out.stderr.trim()),
            })
        }
    }

    /// Run `git -C <location> <args...>`, erroring on a non-zero exit.
    fn git(&self, ctx: &SyncContext<'_>, args: &[&str]) -> Result<(), SyncError> {
        let spec = CommandSpec::new("git")
            .arg("-C")
            .arg(ctx.location.to_string_lossy().into_owned())
            .args(args.iter().copied());
        let out = self.runner.run(&spec)?;
        if !out.success() {
            return Err(SyncError::Transport {
                repo: ctx.repo.to_owned(),
                reason: format!("git {}: {}", args.join(" "), out.stderr.trim()),
            });
        }
        Ok(())
    }

    /// Update the `origin` remote URL when `sync-uri` differs from the configured
    /// one, so a changed mirror takes effect (non-volatile repositories only).
    fn update_remote_url(&self, ctx: &SyncContext<'_>) -> Result<(), SyncError> {
        let spec = CommandSpec::new("git")
            .arg("-C")
            .arg(ctx.location.to_string_lossy().into_owned())
            .arg("remote")
            .arg("get-url")
            .arg("origin");
        let out = self.runner.run(&spec)?;
        let current = out.stdout.trim();
        if out.success() && current != ctx.options.uri {
            self.git(ctx, &["remote", "set-url", "origin", &ctx.options.uri])?;
        }
        Ok(())
    }

    /// Read the current head revision, or `None` when the repository has no head
    /// yet.
    fn head(&self, ctx: &SyncContext<'_>) -> Result<Option<String>, SyncError> {
        let spec = CommandSpec::new("git")
            .arg("-C")
            .arg(ctx.location.to_string_lossy().into_owned())
            .arg("rev-parse")
            .arg("HEAD");
        let out = self.runner.run(&spec)?;
        if out.success() {
            let rev = out.stdout.trim().to_owned();
            Ok((!rev.is_empty()).then_some(rev))
        } else {
            Ok(None)
        }
    }
}

impl<R: CommandRunner> Backend for GitBackend<R> {
    fn sync_type(&self) -> &str {
        "git"
    }

    fn exists(&self, ctx: &SyncContext<'_>) -> bool {
        ctx.location.join(".git").exists()
    }

    #[instrument(skip(self, ctx), fields(repo = ctx.repo))]
    fn fetch(&self, ctx: &SyncContext<'_>) -> Result<SyncOutcome, SyncError> {
        let mut spec = CommandSpec::new("git")
            .arg("clone")
            .args(self.depth_args(ctx));
        spec = spec
            .arg(ctx.options.uri.clone())
            .arg(ctx.location.to_string_lossy().into_owned());
        let out = self.runner.run(&spec)?;
        if !out.success() {
            return Err(SyncError::Transport {
                repo: ctx.repo.to_owned(),
                reason: format!("clone failed: {}", out.stderr.trim()),
            });
        }
        self.set_safe_directory(ctx)?;
        if ctx.options.git_verify_commit_signature {
            Verifier::new(&self.runner).verify_git_head(ctx.repo, ctx.location)?;
        }
        let head = self.head(ctx)?;
        Ok(SyncOutcome::changed(SyncKind::Initial, head))
    }

    #[instrument(skip(self, ctx), fields(repo = ctx.repo))]
    fn update(&self, ctx: &SyncContext<'_>) -> Result<SyncOutcome, SyncError> {
        self.set_safe_directory(ctx)?;
        let before = self.head(ctx)?;

        // A non-volatile repository is clobbered back to a clean state before the
        // fetch so a shallow clone with orphaned files keeps syncing (bug 887025).
        // A volatile (user-managed) repository is never clobbered.
        if !ctx.options.volatile {
            self.update_remote_url(ctx)?;
            self.git(ctx, &["clean", "--force", "-d", "-x"])?;
            self.git(ctx, &["reset", "--hard"])?;
        }

        let mut fetch = CommandSpec::new("git")
            .arg("-C")
            .arg(ctx.location.to_string_lossy().into_owned())
            .arg("fetch");
        fetch = fetch.args(self.depth_args(ctx)).arg("origin");
        let out = self.runner.run(&fetch)?;
        if !out.success() {
            return Err(SyncError::Transport {
                repo: ctx.repo.to_owned(),
                reason: format!("fetch failed: {}", out.stderr.trim()),
            });
        }

        let merge = CommandSpec::new("git")
            .arg("-C")
            .arg(ctx.location.to_string_lossy().into_owned())
            .arg("merge")
            .arg("--ff-only")
            .arg("FETCH_HEAD");
        let out = self.runner.run(&merge)?;
        if !out.success() {
            // A non-volatile repo recovers a non-fast-forwardable tree by resetting
            // hard to the fetched head, matching GitSync.update.
            if ctx.options.volatile {
                return Err(SyncError::Transport {
                    repo: ctx.repo.to_owned(),
                    reason: format!("merge failed: {}", out.stderr.trim()),
                });
            }
            self.git(ctx, &["reset", "--hard", "FETCH_HEAD"])?;
        }

        // Prune a shallow repository's objects after the fetch.
        if matches!(ctx.options.depth, None | Some(1..)) && !ctx.options.volatile {
            let _ = self.git(ctx, &["gc", "--auto"]);
        }

        if ctx.options.git_verify_commit_signature {
            Verifier::new(&self.runner).verify_git_head(ctx.repo, ctx.location)?;
        }

        let after = self.head(ctx)?;
        let changed = before != after;
        Ok(SyncOutcome {
            kind: SyncKind::Update,
            changed,
            head: after,
        })
    }

    fn retrieve_head(&self, ctx: &SyncContext<'_>) -> Result<Option<String>, SyncError> {
        self.head(ctx)
    }
}
