//! The git backend.
//!
//! The git backend clones shallowly by default (`--depth 1`), honoring
//! `sync-depth`/`clone-depth` where a depth of zero requests full history. For
//! an existing repository it fetches and merges, sets `safe.directory` for the
//! location first, and detects change by comparing the head revision before and
//! after the operation. The fetched ref is verified (commit signature and head
//! age, each independently when its key is set) before it is merged into the live
//! tree, so an untrusted or stale commit never lands on disk. It shells out to
//! `git` through the injectable [`CommandRunner`].

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

    /// The depth arguments for a fetch. When no depth is configured for a
    /// volatile repository, the backend probes `git rev-parse
    /// --is-shallow-repository` and omits `--depth` when the repository is not
    /// already shallow, so a user-managed full clone is not truncated, mirroring
    /// `GitSync.update`. All other cases follow [`Self::depth_args`].
    fn fetch_depth_args(&self, ctx: &SyncContext<'_>) -> Result<Vec<String>, SyncError> {
        if ctx.options.depth.is_none() && ctx.options.volatile {
            let spec = CommandSpec::new("git")
                .arg("-C")
                .arg(ctx.location.to_string_lossy().into_owned())
                .arg("rev-parse")
                .arg("--is-shallow-repository");
            let out = self.runner.run(&spec)?;
            if out.success() && out.stdout.trim() == "false" {
                return Ok(Vec::new());
            }
        }
        Ok(self.depth_args(ctx))
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

    /// Verify a fetched revision before it advances the live tree: check the
    /// commit signature when `sync-git-verify-commit-signature` is set, and the
    /// head age when `sync-git-verify-max-age-days` is set. Either check runs
    /// independently of the other, matching Portage's `verify_head`, so a stale or
    /// untrusted head is rejected before the merge.
    fn verify_revision(&self, ctx: &SyncContext<'_>, rev: &str) -> Result<(), SyncError> {
        if ctx.options.git_verify_max_age_days > 0 {
            self.verify_max_age(ctx, rev)?;
        }
        if ctx.options.git_verify_commit_signature {
            Verifier::new(&self.runner).verify_git_head(ctx.repo, ctx.location, rev)?;
        }
        Ok(())
    }

    /// Reject a fetched revision whose `metadata/timestamp.chk` is older than
    /// `sync-git-verify-max-age-days`, reading it from the revision itself via
    /// `git show <rev>:metadata/timestamp.chk`. A repository without the file is
    /// not aged out, matching Portage's lenient handling of a missing timestamp.
    fn verify_max_age(&self, ctx: &SyncContext<'_>, rev: &str) -> Result<(), SyncError> {
        let max_days = ctx.options.git_verify_max_age_days as i64;
        let spec = CommandSpec::new("git")
            .arg("-C")
            .arg(ctx.location.to_string_lossy().into_owned())
            .arg("show")
            .arg(format!("{rev}:metadata/timestamp.chk"));
        let out = self.runner.run(&spec)?;
        if !out.success() {
            // No timestamp file in the tree: nothing to age out.
            return Ok(());
        }
        let Some(head_ts) = out
            .stdout
            .lines()
            .next()
            .and_then(crate::timestamp::parse_timestamp_format)
        else {
            return Ok(());
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(head_ts);
        if now - head_ts > max_days * 86_400 {
            return Err(SyncError::Verification {
                repo: ctx.repo.to_owned(),
                reason: format!("fetched head is older than {max_days} days"),
            });
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
            .args(self.depth_args(ctx))
            .args(ctx.options.git_clone_extra_opts.iter().cloned());
        spec = spec
            .arg(ctx.options.uri.clone())
            .arg(ctx.location.to_string_lossy().into_owned());
        // `sync-git-env` plus `sync-git-clone-env` are injected into the clone
        // environment, mirroring `GitSync.new`.
        for (key, value) in ctx.options.git_env.iter().chain(&ctx.options.git_clone_env) {
            spec = spec.env(key.clone(), value.clone());
        }
        let out = self.runner.run(&spec)?;
        if !out.success() {
            return Err(SyncError::Transport {
                repo: ctx.repo.to_owned(),
                reason: format!("clone failed: {}", out.stderr.trim()),
            });
        }
        self.set_safe_directory(ctx)?;
        // Gate the freshly cloned tree on the same signature and max-age checks.
        // A clone checks out immediately, so on failure the cloned tree is removed
        // rather than left on disk.
        if let Err(e) = self.verify_revision(ctx, "HEAD") {
            let _ = std::fs::remove_dir_all(ctx.location);
            return Err(e);
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
        fetch = fetch
            .args(self.fetch_depth_args(ctx)?)
            .args(ctx.options.git_pull_extra_opts.iter().cloned())
            .arg("origin");
        // `sync-git-env` plus `sync-git-pull-env` are injected into the fetch
        // environment, mirroring `GitSync.update`.
        for (key, value) in ctx.options.git_env.iter().chain(&ctx.options.git_pull_env) {
            fetch = fetch.env(key.clone(), value.clone());
        }
        let out = self.runner.run(&fetch)?;
        if !out.success() {
            return Err(SyncError::Transport {
                repo: ctx.repo.to_owned(),
                reason: format!("fetch failed: {}", out.stderr.trim()),
            });
        }

        // Verify the fetched ref before it is merged into the live tree, so an
        // untrusted or stale commit never lands on disk. The clobber above reset
        // the tree to the previously verified head, which stays in place on abort.
        self.verify_revision(ctx, "FETCH_HEAD")?;

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
