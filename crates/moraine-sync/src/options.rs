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
    /// Skip refresh silently (`false-nowarn`).
    Disabled,
    /// Skip refresh after a security warning that revoked keys cannot be
    /// detected (`false`, `no`, `0`).
    DisabledWarn,
    /// Attempt a Web Key Directory refresh first and fall back to a keyserver
    /// refresh when WKD fails (`true`, `yes`, `wkd`, and unrecognized values).
    WkdThenKeyserver,
    /// Refresh from the keyserver only (`keyserver`).
    Keyserver,
}

impl KeyRefresh {
    /// Parse the `sync-openpgp-key-refresh` family of values, mirroring
    /// `lib/portage/repository/config.py` where `yes` maps to `true`, `no` maps
    /// to `false`, and any unrecognized value defaults to `true`.
    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("false-nowarn") => KeyRefresh::Disabled,
            Some("false") | Some("no") | Some("0") => KeyRefresh::DisabledWarn,
            Some("keyserver") => KeyRefresh::Keyserver,
            _ => KeyRefresh::WkdThenKeyserver,
        }
    }
}

/// Engine-wide default `sync-*` values applied beneath each repository's own.
#[derive(Debug, Clone)]
pub struct SyncDefaults {
    /// The default freshness/connection timeout in seconds.
    pub timeout_secs: u64,
    /// The default initial-connection timeout in seconds bounding the rsync
    /// freshness probe (`PORTAGE_RSYNC_INITIAL_TIMEOUT`).
    pub rsync_initial_timeout_secs: u64,
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
            // Portage hardcodes the rsync transfer `--timeout=180`.
            timeout_secs: 180,
            // Portage defaults `PORTAGE_RSYNC_INITIAL_TIMEOUT` to 15 seconds.
            rsync_initial_timeout_secs: 15,
            retries: 3,
            depth: None,
            key_refresh: KeyRefresh::WkdThenKeyserver,
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
    /// The initial-connection timeout in seconds bounding the rsync freshness
    /// probe (`PORTAGE_RSYNC_INITIAL_TIMEOUT`).
    pub rsync_initial_timeout_secs: u64,
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
    /// rsync `sync-rsync-verify-jobs`: parallel manifest-verification jobs.
    pub rsync_verify_jobs: Option<u32>,
    /// rsync `sync-rsync-verify-max-age`: warn when the Manifest `TIMESTAMP` is
    /// older than this many days (0 disables the check).
    pub rsync_verify_max_age_days: u32,
    /// git `sync-git-verify-commit-signature`: verify the head commit signature.
    pub git_verify_commit_signature: bool,
    /// git `sync-git-verify-max-age-days`: reject a head older than this (0 = off).
    pub git_verify_max_age_days: u32,
    /// git `sync-git-env`: `KEY=VALUE` environment assignments injected into both
    /// the clone and the fetch.
    pub git_env: Vec<(String, String)>,
    /// git `sync-git-clone-env`: `KEY=VALUE` environment assignments injected into
    /// the clone only.
    pub git_clone_env: Vec<(String, String)>,
    /// git `sync-git-pull-env`: `KEY=VALUE` environment assignments injected into
    /// the fetch only.
    pub git_pull_env: Vec<(String, String)>,
    /// git `sync-git-clone-extra-opts`: extra arguments appended to the clone.
    pub git_clone_extra_opts: Vec<String>,
    /// git `sync-git-pull-extra-opts`: extra arguments appended to the fetch.
    pub git_pull_extra_opts: Vec<String>,
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
    /// The refresh retry count from `sync-openpgp-key-refresh-retry-count`.
    pub refresh_retries: u32,
    /// `sync-openpgp-key-refresh-retry-overall-timeout` in seconds, when set.
    pub refresh_retry_overall_timeout: Option<f64>,
    /// `sync-openpgp-key-refresh-retry-delay-mult`: the backoff multiplier.
    pub refresh_retry_delay_mult: f64,
    /// `sync-openpgp-key-refresh-retry-delay-exp-base`: the backoff exponential base.
    pub refresh_retry_delay_exp_base: f64,
    /// `sync-openpgp-key-refresh-retry-delay-max`: the per-delay cap in seconds, when set.
    pub refresh_retry_delay_max: Option<f64>,
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

        // `PORTAGE_RSYNC_INITIAL_TIMEOUT` bounds the freshness probe's initial
        // connection. A missing or unparseable value falls back to the default.
        let rsync_initial_timeout_secs = parse_u64(get("PORTAGE_RSYNC_INITIAL_TIMEOUT").as_deref())
            .unwrap_or(defaults.rsync_initial_timeout_secs);

        // `PORTAGE_RSYNC_RETRIES` overrides `sync-retries` for rsync; a negative
        // value (Portage's default of -1, meaning "try every address") does not
        // parse and falls through to the general retry count.
        let retries = parse_u32(get("PORTAGE_RSYNC_RETRIES").as_deref())
            .or_else(|| parse_u32(get("sync-retries").as_deref()))
            .unwrap_or(defaults.retries);

        let depth = parse_u32(get("sync-depth").as_deref())
            .or_else(|| parse_u32(get("clone-depth").as_deref()))
            .map(Some)
            .unwrap_or(defaults.depth);

        let rsync_extra_opts = get("sync-rsync-extra-opts")
            .map(|s| shlex_tokens(&s))
            .unwrap_or_default();
        let rsync_opts_override = get("PORTAGE_RSYNC_OPTS")
            .filter(|s| !s.is_empty())
            .map(|s| shlex_tokens(&s));
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
        let rsync_verify_jobs = parse_u32(get("sync-rsync-verify-jobs").as_deref());
        let rsync_verify_max_age_days =
            parse_u32(get("sync-rsync-verify-max-age").as_deref()).unwrap_or(0);
        let git_verify_commit_signature = bool_key("sync-git-verify-commit-signature", false);
        let git_verify_max_age_days =
            parse_u32(get("sync-git-verify-max-age-days").as_deref()).unwrap_or(0);

        // Each `sync-git-*-env` value is shlex-tokenized into `KEY=VALUE`
        // assignments and the extra-opts values into argument lists.
        let git_env = get("sync-git-env")
            .map(|s| shlex_env(&s))
            .unwrap_or_default();
        let git_clone_env = get("sync-git-clone-env")
            .map(|s| shlex_env(&s))
            .unwrap_or_default();
        let git_pull_env = get("sync-git-pull-env")
            .map(|s| shlex_env(&s))
            .unwrap_or_default();
        let git_clone_extra_opts = get("sync-git-clone-extra-opts")
            .map(|s| shlex_tokens(&s))
            .unwrap_or_default();
        let git_pull_extra_opts = get("sync-git-pull-extra-opts")
            .map(|s| shlex_tokens(&s))
            .unwrap_or_default();
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
        // The retry tuning defaults mirror Portage's `_key_refresh_retry_decorator`
        // fallbacks: no overall timeout, multiplier 1, base 2, and no per-delay cap.
        let refresh_retry_overall_timeout =
            parse_f64(get("sync-openpgp-key-refresh-retry-overall-timeout").as_deref());
        let refresh_retry_delay_mult =
            parse_f64(get("sync-openpgp-key-refresh-retry-delay-mult").as_deref()).unwrap_or(1.0);
        let refresh_retry_delay_exp_base =
            parse_f64(get("sync-openpgp-key-refresh-retry-delay-exp-base").as_deref())
                .unwrap_or(2.0);
        let refresh_retry_delay_max =
            parse_f64(get("sync-openpgp-key-refresh-retry-delay-max").as_deref());

        // `post-sync` is supplied by the engine from the extras map.
        let post_sync = None;

        Ok(Self {
            sync_type,
            uri,
            auto_sync,
            timeout_secs,
            rsync_initial_timeout_secs,
            retries,
            depth,
            rsync_extra_opts,
            rsync_opts_override,
            rsync_vcs_ignore,
            verify_metamanifest,
            rsync_verify_jobs,
            rsync_verify_max_age_days,
            git_verify_commit_signature,
            git_verify_max_age_days,
            git_env,
            git_clone_env,
            git_pull_env,
            git_clone_extra_opts,
            git_pull_extra_opts,
            webrsync_verify_signature,
            webrsync_keep_snapshots,
            openpgp_key_path,
            openpgp_keyserver,
            key_refresh,
            refresh_retries,
            refresh_retry_overall_timeout,
            refresh_retry_delay_mult,
            refresh_retry_delay_exp_base,
            refresh_retry_delay_max,
            post_sync,
            volatile: false,
        })
    }
}

/// Tokenize a value with shell quoting rules, mirroring Portage's `shlex.split`.
/// An unbalanced-quote value that cannot be tokenized yields no tokens.
fn shlex_tokens(value: &str) -> Vec<String> {
    shlex::split(value).unwrap_or_default()
}

/// Tokenize a value with shell quoting rules and split each token into a
/// `KEY=VALUE` assignment on its first `=`, keeping only tokens with a non-empty
/// key, mirroring Portage's `assignment.partition("=")` env parsing.
fn shlex_env(value: &str) -> Vec<(String, String)> {
    shlex_tokens(value)
        .into_iter()
        .filter_map(|token| {
            let (key, val) = match token.split_once('=') {
                Some((k, v)) => (k.to_owned(), v.to_owned()),
                None => (token, String::new()),
            };
            (!key.is_empty()).then_some((key, val))
        })
        .collect()
}

fn parse_u64(value: Option<&str>) -> Option<u64> {
    value.and_then(|v| v.trim().parse().ok())
}

fn parse_u32(value: Option<&str>) -> Option<u32> {
    value.and_then(|v| v.trim().parse().ok())
}

fn parse_f64(value: Option<&str>) -> Option<f64> {
    value.and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use moraine_repo::RepoConfig;

    use super::{KeyRefresh, SyncDefaults, SyncOptions};

    /// Build a minimal `RepoConfig` with the given `sync-*` keys.
    fn cfg_with(pairs: &[(&str, &str)]) -> RepoConfig {
        let mut sync = BTreeMap::new();
        sync.insert("sync-type".to_owned(), "rsync".to_owned());
        sync.insert("sync-uri".to_owned(), "rsync://x".to_owned());
        for (k, v) in pairs {
            sync.insert((*k).to_owned(), (*v).to_owned());
        }
        RepoConfig {
            name: "g".to_owned(),
            location: PathBuf::from("/x"),
            masters: vec![],
            priority: 0,
            aliases: vec![],
            eclass_overrides: vec![],
            cache_formats: vec![],
            profile_formats: vec![],
            manifest_hashes: vec![],
            manifest_required_hashes: vec![],
            thin_manifests: false,
            sign_manifests: false,
            use_manifests: true,
            eapis_banned: vec![],
            eapis_deprecated: vec![],
            default_eapi: "0".to_owned(),
            sync,
        }
    }

    #[test]
    fn rsync_extra_opts_tokenized_by_shell_rules() {
        let cfg = cfg_with(&[("sync-rsync-extra-opts", "--rsh=\"ssh -p 2222\"")]);
        let opts = SyncOptions::resolve(&cfg, &SyncDefaults::default()).unwrap();
        assert_eq!(opts.rsync_extra_opts, vec!["--rsh=ssh -p 2222".to_owned()]);
    }

    #[test]
    fn rsync_opts_override_tokenized_by_shell_rules() {
        let cfg = cfg_with(&[("PORTAGE_RSYNC_OPTS", "--archive --rsh='ssh -p 2222'")]);
        let opts = SyncOptions::resolve(&cfg, &SyncDefaults::default()).unwrap();
        assert_eq!(
            opts.rsync_opts_override,
            Some(vec!["--archive".to_owned(), "--rsh=ssh -p 2222".to_owned()])
        );
    }

    #[test]
    fn initial_timeout_defaults_to_fifteen_and_parses_override() {
        let cfg = cfg_with(&[]);
        let opts = SyncOptions::resolve(&cfg, &SyncDefaults::default()).unwrap();
        assert_eq!(opts.rsync_initial_timeout_secs, 15);

        let cfg = cfg_with(&[("PORTAGE_RSYNC_INITIAL_TIMEOUT", "30")]);
        let opts = SyncOptions::resolve(&cfg, &SyncDefaults::default()).unwrap();
        assert_eq!(opts.rsync_initial_timeout_secs, 30);
    }

    #[test]
    fn git_env_parsed_as_key_value_assignments() {
        let cfg = cfg_with(&[(
            "sync-git-env",
            "GIT_SSH_COMMAND='ssh -i /home/u/.ssh/overlay_key'",
        )]);
        let opts = SyncOptions::resolve(&cfg, &SyncDefaults::default()).unwrap();
        assert_eq!(
            opts.git_env,
            vec![(
                "GIT_SSH_COMMAND".to_owned(),
                "ssh -i /home/u/.ssh/overlay_key".to_owned()
            )]
        );
    }

    #[test]
    fn git_clone_extra_opts_tokenized() {
        let cfg = cfg_with(&[("sync-git-clone-extra-opts", "--filter=blob:none")]);
        let opts = SyncOptions::resolve(&cfg, &SyncDefaults::default()).unwrap();
        assert_eq!(
            opts.git_clone_extra_opts,
            vec!["--filter=blob:none".to_owned()]
        );
    }

    #[test]
    fn key_refresh_parse_maps_every_portage_value() {
        assert_eq!(
            KeyRefresh::parse(Some("false-nowarn")),
            KeyRefresh::Disabled
        );
        assert_eq!(KeyRefresh::parse(Some("false")), KeyRefresh::DisabledWarn);
        assert_eq!(KeyRefresh::parse(Some("no")), KeyRefresh::DisabledWarn);
        assert_eq!(KeyRefresh::parse(Some("0")), KeyRefresh::DisabledWarn);
        assert_eq!(
            KeyRefresh::parse(Some("true")),
            KeyRefresh::WkdThenKeyserver
        );
        assert_eq!(KeyRefresh::parse(Some("yes")), KeyRefresh::WkdThenKeyserver);
        assert_eq!(KeyRefresh::parse(Some("wkd")), KeyRefresh::WkdThenKeyserver);
        assert_eq!(KeyRefresh::parse(Some("keyserver")), KeyRefresh::Keyserver);
        assert_eq!(
            KeyRefresh::parse(Some("bogus")),
            KeyRefresh::WkdThenKeyserver
        );
    }
}
