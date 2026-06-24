//! OpenPGP verification of synced trees.
//!
//! Verification shells out to `gpg` (and uses git's own signature reporting for
//! the git backend) through the injectable [`CommandRunner`], so tests drive the
//! gate without real keys. Keys are loaded from the configured key path into an
//! isolated GnuPG home used only for the repository being verified, and are
//! refreshed according to the repository's `sync-openpgp-*` policy before the
//! check.

use std::path::{Path, PathBuf};

use tracing::instrument;

use crate::command::{CommandRunner, CommandSpec};
use crate::error::SyncError;
use crate::options::{KeyRefresh, SyncOptions};

/// The classification of a git commit signature, mirroring `git log %G?`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitSigStatus {
    /// A good signature from a trusted key (`G` or `U`).
    Good,
    /// A signature whose key is not trusted (`E` for cannot-check, etc.).
    Untrusted,
    /// A bad signature (`B`) or no signature (`N`).
    Bad,
}

impl GitSigStatus {
    /// Classify a single `%G?` status character.
    fn from_code(code: &str) -> Self {
        match code.trim() {
            "G" | "U" => GitSigStatus::Good,
            "E" | "X" | "Y" | "R" => GitSigStatus::Untrusted,
            _ => GitSigStatus::Bad,
        }
    }
}

/// Verifies signatures for synced trees via an injected command runner.
pub struct Verifier<'a, R: CommandRunner> {
    runner: &'a R,
}

impl<'a, R: CommandRunner> Verifier<'a, R> {
    /// Build a verifier over `runner`.
    pub fn new(runner: &'a R) -> Self {
        Self { runner }
    }

    /// Load the configured key into an isolated GnuPG home and refresh it per
    /// the repository's policy. Returns the isolated home directory to use for
    /// the subsequent verification, when a key path is configured.
    #[instrument(skip(self, opts), fields(repo = repo))]
    pub fn prepare_keys(
        &self,
        repo: &str,
        opts: &SyncOptions,
        gnupg_home: &Path,
    ) -> Result<Option<PathBuf>, SyncError> {
        let Some(key_path) = &opts.openpgp_key_path else {
            return Ok(None);
        };

        let import = CommandSpec::new("gpg")
            .arg("--homedir")
            .arg(gnupg_home.to_string_lossy().into_owned())
            .arg("--batch")
            .arg("--import")
            .arg(key_path.to_string_lossy().into_owned());
        let out = self.runner.run(&import)?;
        if !out.success() {
            return Err(SyncError::Verification {
                repo: repo.to_owned(),
                reason: format!("key import failed: {}", out.stderr.trim()),
            });
        }

        self.refresh_keys(repo, opts, gnupg_home)?;
        Ok(Some(gnupg_home.to_path_buf()))
    }

    /// Refresh keys under the configured policy and retry count. When refresh is
    /// disabled this is a no-op; when refresh is required and every attempt
    /// fails verification fails.
    fn refresh_keys(
        &self,
        repo: &str,
        opts: &SyncOptions,
        gnupg_home: &Path,
    ) -> Result<(), SyncError> {
        let refresh_arg = match opts.key_refresh {
            KeyRefresh::Disabled => return Ok(()),
            KeyRefresh::Keyserver => "--refresh-keys",
            KeyRefresh::Wkd => "--locate-external-key",
        };

        let attempts = opts.refresh_retries.max(1);
        let mut last = String::new();
        for _ in 0..attempts {
            let spec = CommandSpec::new("gpg")
                .arg("--homedir")
                .arg(gnupg_home.to_string_lossy().into_owned())
                .arg("--batch")
                .arg(refresh_arg);
            match self.runner.run(&spec) {
                Ok(out) if out.success() => return Ok(()),
                Ok(out) => last = out.stderr.trim().to_owned(),
                Err(e) => last = e.to_string(),
            }
        }
        Err(SyncError::Verification {
            repo: repo.to_owned(),
            reason: format!("key refresh failed: {last}"),
        })
    }

    /// Verify the signed metadata manifest of an rsync staging directory.
    #[instrument(skip(self, gnupg_home), fields(repo = repo))]
    pub fn verify_rsync_tree(
        &self,
        repo: &str,
        staging: &Path,
        gnupg_home: Option<&Path>,
    ) -> Result<(), SyncError> {
        let manifest = staging.join("Manifest");
        let mut spec = CommandSpec::new("gpg");
        if let Some(home) = gnupg_home {
            spec = spec
                .arg("--homedir")
                .arg(home.to_string_lossy().into_owned());
        }
        spec = spec
            .arg("--batch")
            .arg("--verify")
            .arg(manifest.to_string_lossy().into_owned());
        let out = self.runner.run(&spec)?;
        if out.success() {
            Ok(())
        } else {
            Err(SyncError::Verification {
                repo: repo.to_owned(),
                reason: format!("manifest signature rejected: {}", out.stderr.trim()),
            })
        }
    }

    /// Verify the signature on the head commit of a git repository.
    #[instrument(skip(self), fields(repo = repo))]
    pub fn verify_git_head(&self, repo: &str, location: &Path) -> Result<(), SyncError> {
        let spec = CommandSpec::new("git")
            .arg("-C")
            .arg(location.to_string_lossy().into_owned())
            .arg("log")
            .arg("-1")
            .arg("--pretty=%G?")
            .arg("HEAD");
        let out = self.runner.run(&spec)?;
        if !out.success() {
            return Err(SyncError::Verification {
                repo: repo.to_owned(),
                reason: format!("could not read commit signature: {}", out.stderr.trim()),
            });
        }
        match GitSigStatus::from_code(&out.stdout) {
            GitSigStatus::Good => Ok(()),
            GitSigStatus::Untrusted => Err(SyncError::Verification {
                repo: repo.to_owned(),
                reason: "head commit signature is from an untrusted key".to_owned(),
            }),
            GitSigStatus::Bad => Err(SyncError::Verification {
                repo: repo.to_owned(),
                reason: "head commit has a bad or missing signature".to_owned(),
            }),
        }
    }

    /// Verify the detached signature on a webrsync snapshot tarball.
    #[instrument(skip(self, gnupg_home), fields(repo = repo))]
    pub fn verify_snapshot(
        &self,
        repo: &str,
        snapshot: &Path,
        signature: &Path,
        gnupg_home: Option<&Path>,
    ) -> Result<(), SyncError> {
        let mut spec = CommandSpec::new("gpg");
        if let Some(home) = gnupg_home {
            spec = spec
                .arg("--homedir")
                .arg(home.to_string_lossy().into_owned());
        }
        spec = spec
            .arg("--batch")
            .arg("--verify")
            .arg(signature.to_string_lossy().into_owned())
            .arg(snapshot.to_string_lossy().into_owned());
        let out = self.runner.run(&spec)?;
        if out.success() {
            Ok(())
        } else {
            Err(SyncError::Verification {
                repo: repo.to_owned(),
                reason: format!("snapshot signature rejected: {}", out.stderr.trim()),
            })
        }
    }
}
