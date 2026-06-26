//! Resolution of a repository's effective `sync-*` options.
//!
//! Stock Portage layers default `sync-*` values under each repository's own
//! `repos.conf` keys. [`SyncDefaults`] holds the engine-wide defaults and
//! [`SyncOptions`] is the per-repository resolution that prefers the
//! repository's own value over the default, matching `moraine-config`
//! precedence.

use std::path::PathBuf;

use moraine_repo::RepoConfig;

use crate::error::SyncError;

/// Whether OpenPGP key refresh is attempted before verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRefresh {
    /// Refresh keys via a keyserver before verifying.
    Keyserver,
    /// Refresh keys via Web Key Directory before verifying.
    Wkd,
    /// Do not refresh; verify against the keys as currently loaded.
    Disabled,
}

impl KeyRefresh {
    /// Parse the `sync-openpgp-key-refresh` family of values. Unknown values
    /// default to keyserver refresh, matching stock behavior.
    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("no") | Some("false") | Some("0") => KeyRefresh::Disabled,
            Some("wkd") => KeyRefresh::Wkd,
            _ => KeyRefresh::Keyserver,
        }
    }
}

/// Engine-wide default `sync-*` values applied beneath each repository's own.
#[derive(Debug, Clone)]
pub struct SyncDefaults {
    /// The default freshness/connection timeout in seconds.
    pub timeout_secs: u64,
    /// The default number of transport retries.
    pub retries: u32,
    /// The default git fetch depth (`None` for the backend default of one).
    pub depth: Option<u32>,
    /// The default key-refresh policy.
    pub key_refresh: KeyRefresh,
    /// The number of refresh retries under the configured policy.
    pub refresh_retries: u32,
    /// Whether signature verification is required by default.
    pub verify: bool,
}

impl Default for SyncDefaults {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            retries: 3,
            depth: None,
            key_refresh: KeyRefresh::Keyserver,
            refresh_retries: 1,
            verify: false,
        }
    }
}

/// A repository's effective `sync-*` options after applying overrides.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// The repository's `sync-type`.
    pub sync_type: String,
    /// The repository's `sync-uri`, required by every transport.
    pub uri: String,
    /// Whether `auto-sync` is enabled for this repository.
    pub auto_sync: bool,
    /// The connection/freshness timeout in seconds.
    pub timeout_secs: u64,
    /// The transport retry count.
    pub retries: u32,
    /// The git fetch depth; `Some(0)` requests full history.
    pub depth: Option<u32>,
    /// Extra rsync options from `sync-rsync-extra-opts`.
    pub rsync_extra_opts: Vec<String>,
    /// A full `PORTAGE_RSYNC_OPTS` override replacing the default option set, when
    /// set. Required options and excludes are re-injected by the backend.
    pub rsync_opts_override: Option<Vec<String>>,
    /// `sync-rsync-vcs-ignore`: sync into a VCS-controlled target without aborting.
    pub rsync_vcs_ignore: bool,
    /// rsync `sync-rsync-verify-metamanifest`: verify the metamanifest tree.
    pub verify_metamanifest: bool,
    /// git `sync-git-verify-commit-signature`: verify the head commit signature.
    pub git_verify_commit_signature: bool,
    /// git `sync-git-verify-max-age-days`: reject a head older than this (0 = off).
    pub git_verify_max_age_days: u32,
    /// webrsync `sync-webrsync-verify-signature`: verify the snapshot signature.
    pub webrsync_verify_signature: bool,
    /// webrsync `sync-webrsync-keep-snapshots`: keep downloaded snapshots (`-k`).
    pub webrsync_keep_snapshots: bool,
    /// The OpenPGP key path from `sync-openpgp-key-path`, when set.
    pub openpgp_key_path: Option<PathBuf>,
    /// The OpenPGP keyserver from `sync-openpgp-keyserver`, when set.
    pub openpgp_keyserver: Option<String>,
    /// The key-refresh policy.
    pub key_refresh: KeyRefresh,
    /// The refresh retry count.
    pub refresh_retries: u32,
    /// The repository-level post-sync command from `post-sync`, when set.
    pub post_sync: Option<Vec<String>>,
    /// Whether the repository is `volatile` (user-managed): its revision history
    /// is not recorded and the git backend never clobbers it.
    pub volatile: bool,
}

impl SyncOptions {
    /// Resolve a repository's effective options from its `repos.conf` keys and
    /// the engine defaults. Fails when the repository declares no `sync-type` or
    /// no `sync-uri`.
    pub fn resolve(cfg: &RepoConfig, defaults: &SyncDefaults) -> Result<Self, SyncError> {
        let get = |key: &str| cfg.sync.get(key).map(|s| s.trim().to_owned());

        let sync_type =
            get("sync-type")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| SyncError::Config {
                    repo: cfg.name.clone(),
                    reason: "missing sync-type".to_owned(),
                })?;

        let uri = get("sync-uri")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| SyncError::Config {
                repo: cfg.name.clone(),
                reason: "missing sync-uri".to_owned(),
            })?;

        // `auto-sync` and `post-sync` are not retained by the discovery model,
        // so they default here and are overridden by the engine from the raw
        // `repos.conf` via the extras map.
        let auto_sync = true;

        let timeout_secs = parse_u64(get("sync-rsync-timeout-secs").as_deref())
            .or_else(|| parse_u64(get("sync-timeout").as_deref()))
            .unwrap_or(defaults.timeout_secs);

        let retries = parse_u32(get("sync-retries").as_deref()).unwrap_or(defaults.retries);

        let depth = parse_u32(get("sync-depth").as_deref())
            .or_else(|| parse_u32(get("clone-depth").as_deref()))
            .map(Some)
            .unwrap_or(defaults.depth);

        let rsync_extra_opts = get("sync-rsync-extra-opts")
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        let rsync_opts_override = get("PORTAGE_RSYNC_OPTS")
            .filter(|s| !s.is_empty())
            .map(|s| s.split_whitespace().map(str::to_owned).collect());
        let rsync_vcs_ignore = matches!(
            get("sync-rsync-vcs-ignore").as_deref(),
            Some("yes") | Some("true") | Some("1")
        );

        // Each backend's verify key is parsed independently so enabling one does
        // not silently force another.
        let bool_key = |key: &str, default: bool| match get(key).as_deref() {
            Some("yes") | Some("true") | Some("1") => true,
            Some("no") | Some("false") | Some("0") => false,
            _ => default,
        };
        let verify_metamanifest = match get("sync-rsync-verify-metamanifest")
            .or_else(|| get("sync-verify"))
            .as_deref()
        {
            Some("yes") | Some("true") | Some("1") => true,
            Some("no") | Some("false") | Some("0") => false,
            _ => defaults.verify,
        };
        let git_verify_commit_signature = bool_key("sync-git-verify-commit-signature", false);
        let git_verify_max_age_days =
            parse_u32(get("sync-git-verify-max-age-days").as_deref()).unwrap_or(0);
        let webrsync_verify_signature = bool_key("sync-webrsync-verify-signature", false);
        let webrsync_keep_snapshots = bool_key("sync-webrsync-keep-snapshots", false);

        let openpgp_key_path = get("sync-openpgp-key-path")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let openpgp_keyserver = get("sync-openpgp-keyserver").filter(|s| !s.is_empty());

        let key_refresh = if cfg.sync.contains_key("sync-openpgp-key-refresh") {
            KeyRefresh::parse(get("sync-openpgp-key-refresh").as_deref())
        } else {
            defaults.key_refresh
        };

        let refresh_retries = parse_u32(get("sync-openpgp-key-refresh-retry-count").as_deref())
            .unwrap_or(defaults.refresh_retries);

        // `post-sync` is supplied by the engine from the extras map.
        let post_sync = None;

        Ok(Self {
            sync_type,
            uri,
            auto_sync,
            timeout_secs,
            retries,
            depth,
            rsync_extra_opts,
            rsync_opts_override,
            rsync_vcs_ignore,
            verify_metamanifest,
            git_verify_commit_signature,
            git_verify_max_age_days,
            webrsync_verify_signature,
            webrsync_keep_snapshots,
            openpgp_key_path,
            openpgp_keyserver,
            key_refresh,
            refresh_retries,
            post_sync,
            volatile: false,
        })
    }
}

fn parse_u64(value: Option<&str>) -> Option<u64> {
    value.and_then(|v| v.trim().parse().ok())
}

fn parse_u32(value: Option<&str>) -> Option<u32> {
    value.and_then(|v| v.trim().parse().ok())
}
