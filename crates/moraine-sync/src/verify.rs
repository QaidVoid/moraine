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

    /// Verify the signed metamanifest of an rsync staging directory: load the
    /// configured key into an isolated GnuPG home, check the top-level Manifest
    /// signature, then recursively verify every listed file's size and hashes.
    /// A valid signature over a tree with tampered or missing files still fails.
    #[instrument(skip(self, opts, gnupg_home), fields(repo = repo))]
    pub fn verify_rsync_tree(
        &self,
        repo: &str,
        staging: &Path,
        opts: &SyncOptions,
        gnupg_home: &Path,
    ) -> Result<(), SyncError> {
        let home = self.prepare_keys(repo, opts, gnupg_home)?;
        self.verify_manifest_signature(repo, &staging.join("Manifest"), home.as_deref())?;
        self.verify_manifest_tree(repo, staging, opts)?;
        Ok(())
    }

    /// Check the OpenPGP signature on a single Manifest file.
    fn verify_manifest_signature(
        &self,
        repo: &str,
        manifest: &Path,
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

    /// Recursively verify the Manifest tree rooted at `staging`. Every entry's
    /// size and hashes are checked against the file on disk, nested `MANIFEST`
    /// entries are descended into (decompressing a `.gz` listing first), and a
    /// `TIMESTAMP` older than `sync-rsync-verify-max-age` days warns.
    fn verify_manifest_tree(
        &self,
        repo: &str,
        staging: &Path,
        opts: &SyncOptions,
    ) -> Result<(), SyncError> {
        let root_text =
            std::fs::read_to_string(staging.join("Manifest")).map_err(|source| SyncError::Io {
                path: staging.join("Manifest"),
                reason: source.to_string(),
            })?;
        let root = crate::manifest::parse(&root_text);

        if opts.rsync_verify_max_age_days > 0
            && let Some(ts) = &root.timestamp
            && let Some(age) = manifest_age_days(ts)
            && age > i64::from(opts.rsync_verify_max_age_days)
        {
            tracing::warn!(
                repo = repo,
                age_days = age,
                "Manifest timestamp is older than sync-rsync-verify-max-age"
            );
        }

        self.verify_manifest_entries(repo, staging, &root)
    }

    /// Verify every entry of one parsed Manifest against `dir`, descending into
    /// nested `MANIFEST` entries.
    fn verify_manifest_entries(
        &self,
        repo: &str,
        dir: &Path,
        manifest: &crate::manifest::Manifest,
    ) -> Result<(), SyncError> {
        for entry in &manifest.entries {
            let path = dir.join(&entry.path);
            let bytes = std::fs::read(&path).map_err(|_| SyncError::Verification {
                repo: repo.to_owned(),
                reason: format!("manifest lists missing file: {}", entry.path),
            })?;
            verify_entry_digest(repo, entry, &bytes)?;

            if entry.kind == "MANIFEST" {
                let text = self.load_nested_manifest(repo, &path, &bytes)?;
                let nested = crate::manifest::parse(&text);
                let subdir = path.parent().unwrap_or(dir);
                self.verify_manifest_entries(repo, subdir, &nested)?;
            }
        }
        Ok(())
    }

    /// Read a nested Manifest's text, decompressing a `.gz` listing through the
    /// system `gzip` tool (already-read bytes are reused for a plain listing).
    fn load_nested_manifest(
        &self,
        repo: &str,
        path: &Path,
        bytes: &[u8],
    ) -> Result<String, SyncError> {
        if path.extension().is_some_and(|e| e == "gz") {
            let spec = CommandSpec::new("gzip")
                .arg("-dc")
                .arg(path.to_string_lossy().into_owned());
            let out = self.runner.run(&spec)?;
            if !out.success() {
                return Err(SyncError::Verification {
                    repo: repo.to_owned(),
                    reason: format!("could not decompress {}", path.display()),
                });
            }
            Ok(out.stdout)
        } else {
            Ok(String::from_utf8_lossy(bytes).into_owned())
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

/// Verify a Manifest entry's size and every hash this implementation supports
/// against the file's bytes. Unknown hash algorithms are skipped rather than
/// rejected, so a future hash does not break verification of the rest.
fn verify_entry_digest(
    repo: &str,
    entry: &crate::manifest::ManifestEntry,
    bytes: &[u8],
) -> Result<(), SyncError> {
    use moraine_common::hash;

    if bytes.len() as u64 != entry.size {
        return Err(SyncError::Verification {
            repo: repo.to_owned(),
            reason: format!(
                "{} size mismatch: expected {}, got {}",
                entry.path,
                entry.size,
                bytes.len()
            ),
        });
    }
    for (name, expected) in &entry.hashes {
        let actual = match name.as_str() {
            "BLAKE2B" => hash::blake2b(bytes),
            "SHA512" => hash::sha512(bytes),
            "SHA256" => hash::sha256(bytes),
            "SHA1" => hash::sha1(bytes),
            "MD5" => hash::md5(bytes),
            _ => continue,
        };
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(SyncError::Verification {
                repo: repo.to_owned(),
                reason: format!("{} {name} hash mismatch", entry.path),
            });
        }
    }
    Ok(())
}

/// Compute the age in days of an ISO 8601 UTC timestamp such as
/// `2026-06-21T05:38:02Z`, relative to now. Returns `None` on a malformed value.
fn manifest_age_days(timestamp: &str) -> Option<i64> {
    let epoch = parse_iso8601_utc(timestamp)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((now - epoch) / 86_400)
}

/// Parse an ISO 8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) to Unix seconds.
fn parse_iso8601_utc(ts: &str) -> Option<i64> {
    let ts = ts.trim().trim_end_matches('Z');
    let (date, time) = ts.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t.next().unwrap_or("0").parse().ok()?;

    // Days since the Unix epoch using a civil-to-days conversion (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso8601_epoch() {
        // 2026-06-21T05:38:02Z == 1782020282 seconds since the epoch.
        assert_eq!(
            parse_iso8601_utc("2026-06-21T05:38:02Z"),
            Some(1_782_020_282)
        );
        assert_eq!(parse_iso8601_utc("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn entry_digest_checks_size_and_hash() {
        let bytes = b"hello";
        let entry = crate::manifest::ManifestEntry {
            kind: "DATA".to_owned(),
            path: "f".to_owned(),
            size: 5,
            hashes: vec![("SHA256".to_owned(), moraine_common::hash::sha256(bytes))],
        };
        assert!(verify_entry_digest("r", &entry, bytes).is_ok());

        let mut bad = entry.clone();
        bad.size = 4;
        assert!(verify_entry_digest("r", &bad, bytes).is_err());

        let mut bad_hash = entry.clone();
        bad_hash.hashes = vec![("SHA256".to_owned(), "deadbeef".to_owned())];
        assert!(verify_entry_digest("r", &bad_hash, bytes).is_err());
    }
}
