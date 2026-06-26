//! The rsync backend.
//!
//! The rsync backend is the default. Before transferring the tree it probes
//! `metadata/timestamp.chk` with a bounded connection timeout and compares it to
//! the local copy: equal timestamps mean no change and no transfer, an older
//! server timestamp is a server-out-of-date condition, and a newer one triggers
//! a transfer into a staging directory that is committed into place only after
//! the transfer (and, when enabled, verification) succeeds. The backend shells
//! out to `rsync` through the injectable [`CommandRunner`].

use std::net::{IpAddr, ToSocketAddrs};
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
        let src = format!("{}/metadata/timestamp.chk", rsync_source(&ctx.options.uri));
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

    /// Build the tree-transfer command into the staging directory. The default
    /// option set mirrors Portage's `_set_rsync_defaults`; a `PORTAGE_RSYNC_OPTS`
    /// override replaces the defaults, with the required options re-injected. An
    /// `addr` override substitutes a resolved mirror address for the host.
    fn transfer_command(&self, ctx: &SyncContext<'_>, addr: Option<IpAddr>) -> CommandSpec {
        let src = format!("{}/", source_with_addr(&ctx.options.uri, addr));
        let dst = format!("{}/", ctx.staging.to_string_lossy());

        let mut opts: Vec<String> = match &ctx.options.rsync_opts_override {
            Some(over) => {
                // `_validate_rsync_opts`: keep the user options, re-inject the
                // required ones and the excludes that must always be present.
                let mut o = over.clone();
                for req in ["--recursive", "--times"] {
                    if !o.iter().any(|a| a == req) {
                        o.push(req.to_string());
                    }
                }
                o
            }
            None => [
                "--recursive",
                "--links",
                "--safe-links",
                "--perms",
                "--times",
                "--omit-dir-times",
                "--compress",
                "--force",
                "--whole-file",
                "--delete",
                "--stats",
                "--human-readable",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        };
        opts.push(format!("--timeout={}", ctx.options.timeout_secs));
        for exclude in STANDARD_EXCLUDES {
            if !opts.iter().any(|a| a == exclude) {
                opts.push((*exclude).to_string());
            }
        }
        opts.extend(ctx.options.rsync_extra_opts.iter().cloned());

        CommandSpec::new("rsync").args(opts).arg(src).arg(dst)
    }

    /// Abort the sync when the target is a VCS-controlled checkout and
    /// `sync-rsync-vcs-ignore` is not set, rather than silently letting rsync
    /// overwrite a `.git`/`.svn`/... tree.
    fn check_vcs(&self, ctx: &SyncContext<'_>) -> Result<(), SyncError> {
        if ctx.options.rsync_vcs_ignore {
            return Ok(());
        }
        const VCS_DIRS: &[&str] = &[".git", ".svn", ".hg", ".bzr", "CVS"];
        for vcs in VCS_DIRS {
            if ctx.location.join(vcs).exists() {
                return Err(SyncError::Config {
                    repo: ctx.repo.to_owned(),
                    reason: format!(
                        "target is {vcs}-controlled; set sync-rsync-vcs-ignore to override"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Run the tree transfer, retrying across resolved mirror addresses on a
    /// transport failure. The attempt budget is the larger of the configured
    /// retry count (`sync-retries`/`PORTAGE_RSYNC_RETRIES`) and the number of
    /// resolved addresses, so every mirror IP is tried at least once.
    fn transfer_with_failover(&self, ctx: &SyncContext<'_>) -> Result<(), SyncError> {
        let addrs = resolve_addresses(&ctx.options.uri);
        let attempts = (ctx.options.retries as usize).max(addrs.len()).max(1);
        let mut last = String::new();
        for attempt in 0..attempts {
            // Cycle through the resolved addresses; fall back to the hostname
            // (no override) when resolution returned nothing.
            let addr = addrs.get(attempt % addrs.len().max(1)).copied();
            let transfer = self.transfer_command(ctx, addr);
            match self.runner.run(&transfer) {
                Ok(out) if out.success() => return Ok(()),
                Ok(out) => last = out.stderr.trim().to_owned(),
                Err(e) => last = e.to_string(),
            }
        }
        Err(SyncError::Transport {
            repo: ctx.repo.to_owned(),
            reason: format!("tree transfer failed after {attempts} attempt(s): {last}"),
        })
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
        self.check_vcs(ctx)?;
        self.transfer_with_failover(ctx)?;

        if ctx.options.verify_metamanifest {
            let verifier = Verifier::new(&self.runner);
            // Keep the isolated GnuPG home outside the staged tree so it is not
            // committed into the live location.
            let gnupg_home = ctx
                .staging
                .parent()
                .unwrap_or(ctx.staging)
                .join(format!(".gnupg-{}", ctx.repo));
            verifier.verify_rsync_tree(ctx.repo, ctx.staging, ctx.options, &gnupg_home)?;
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
/// Translate a `sync-uri` into an rsync source root: an `rsync://` URI is kept,
/// a `file://` URI is stripped to its local path, and an `ssh://host/path` URI is
/// rewritten to rsync's `host:/path` form. Any other value is returned trimmed.
fn rsync_source(uri: &str) -> String {
    let uri = uri.trim_end_matches('/');
    if let Some(path) = uri.strip_prefix("file://") {
        return path.to_string();
    }
    if let Some(rest) = uri.strip_prefix("ssh://") {
        // `host[:port]/path` -> `host:/path` (rsync over ssh).
        if let Some((host, path)) = rest.split_once('/') {
            let host = host.split(':').next().unwrap_or(host);
            return format!("{host}:/{path}");
        }
        return rest.to_string();
    }
    uri.to_string()
}

/// The rsync daemon port, used to resolve mirror addresses for failover.
const RSYNC_PORT: u16 = 873;

/// Resolve the mirror host's addresses for failover. The list is lightly
/// rotated by the process id so a multi-homed mirror is not always contacted on
/// the same address first (a cheap stand-in for Portage's address shuffle).
/// Returns an empty list for a URI with no resolvable host (such as `file://`).
fn resolve_addresses(uri: &str) -> Vec<IpAddr> {
    let Some(host) = uri_host(uri) else {
        return Vec::new();
    };
    let mut addrs: Vec<IpAddr> = (host.as_str(), RSYNC_PORT)
        .to_socket_addrs()
        .map(|it| it.map(|s| s.ip()).collect())
        .unwrap_or_default();
    addrs.dedup();
    if !addrs.is_empty() {
        let shift = (std::process::id() as usize) % addrs.len();
        addrs.rotate_left(shift);
    }
    addrs
}

/// Extract the host component of an rsync source from a `sync-uri`, when it has
/// one. `file://` paths and bare local paths have no host.
fn uri_host(uri: &str) -> Option<String> {
    let uri = uri.trim_end_matches('/');
    if let Some(rest) = uri
        .strip_prefix("rsync://")
        .or_else(|| uri.strip_prefix("ssh://"))
    {
        let authority = rest.split('/').next().unwrap_or(rest);
        let host = authority.rsplit('@').next().unwrap_or(authority);
        return Some(host.split(':').next().unwrap_or(host).to_owned());
    }
    if uri.starts_with("file://") || uri.starts_with('/') {
        return None;
    }
    // `host:/path` daemon shorthand.
    uri.split_once(":/")
        .map(|(host, _)| host.rsplit('@').next().unwrap_or(host).to_owned())
}

/// Build an rsync source, substituting a resolved address for the host when one
/// is given. An IPv6 literal is bracketed as rsync requires.
fn source_with_addr(uri: &str, addr: Option<IpAddr>) -> String {
    let base = rsync_source(uri);
    let Some(addr) = addr else {
        return base;
    };
    let host = if addr.is_ipv6() {
        format!("[{addr}]")
    } else {
        addr.to_string()
    };
    if let Some(rest) = base.strip_prefix("rsync://")
        && let Some((_, path)) = rest.split_once('/')
    {
        return format!("rsync://{host}/{path}");
    }
    if let Some((h, path)) = base.split_once(":/")
        && !h.contains('/')
    {
        return format!("{host}:/{path}");
    }
    base
}

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
    fn rsync_source_handles_file_and_ssh_schemes() {
        assert_eq!(
            rsync_source("rsync://host/gentoo-portage/"),
            "rsync://host/gentoo-portage"
        );
        assert_eq!(rsync_source("file:///srv/mirror/"), "/srv/mirror");
        assert_eq!(
            rsync_source("ssh://user@host/srv/repo"),
            "user@host:/srv/repo"
        );
        assert_eq!(rsync_source("ssh://host:2222/srv/repo"), "host:/srv/repo");
    }

    #[test]
    fn uri_host_extracts_mirror_host() {
        assert_eq!(
            uri_host("rsync://mirror.example/gentoo/"),
            Some("mirror.example".into())
        );
        assert_eq!(uri_host("user@host:/srv/repo"), Some("host".into()));
        assert_eq!(uri_host("ssh://user@host:22/srv"), Some("host".into()));
        assert_eq!(uri_host("file:///srv/mirror"), None);
        assert_eq!(uri_host("/srv/mirror"), None);
    }

    #[test]
    fn source_with_addr_substitutes_host() {
        let v4: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(
            source_with_addr("rsync://mirror/gentoo", Some(v4)),
            "rsync://1.2.3.4/gentoo"
        );
        let v6: IpAddr = "::1".parse().unwrap();
        assert_eq!(
            source_with_addr("rsync://mirror/gentoo", Some(v6)),
            "rsync://[::1]/gentoo"
        );
        assert_eq!(
            source_with_addr("rsync://mirror/gentoo", None),
            "rsync://mirror/gentoo"
        );
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
