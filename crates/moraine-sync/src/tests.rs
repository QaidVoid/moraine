//! Engine and backend tests against fake command runners and fixtures.
//!
//! No test touches the network or real tooling: every external invocation goes
//! through a scripted [`FakeRunner`], and the metadata refresh goes through a
//! recording fake [`MetadataRefresher`]. The tests assert ordering, auto-sync
//! selection, override precedence, backend argument construction, the freshness
//! probe decision, change detection, verification gating with prior-tree
//! preservation, and the incremental-versus-full refresh selection.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use moraine_repo::{RepoSet, discover};
use tempfile::TempDir;

use crate::backend::{Backend, BackendRegistry, SyncContext};
use crate::backends::{GitBackend, RsyncBackend, WebrsyncBackend};
use crate::command::CommandOutput;
use crate::command::fake::FakeRunner;
use crate::engine::{RepoResult, SyncEngine};
use crate::error::SyncError;
use crate::options::{SyncDefaults, SyncOptions};
use crate::outcome::{SyncKind, SyncOutcome};
use crate::refresh::{MetadataRefresher, RefreshMode, RefreshReport};
use crate::revision::RevisionHistory;

/// A recording fake refresher: records `(repo, force_full)` and returns a fixed
/// mode based on whether full was forced.
#[derive(Default)]
struct FakeRefresher {
    calls: Mutex<Vec<(String, bool)>>,
    force_full_for: Mutex<Vec<String>>,
}

impl FakeRefresher {
    fn new() -> Self {
        Self::default()
    }

    /// Mark `repo` as inconsistent so its incremental refresh escalates to full.
    fn mark_inconsistent(&self, repo: &str) {
        self.force_full_for.lock().unwrap().push(repo.to_owned());
    }

    fn calls(&self) -> Vec<(String, bool)> {
        self.calls.lock().unwrap().clone()
    }
}

impl MetadataRefresher for FakeRefresher {
    fn refresh(&self, repo: &str, force_full: bool) -> Result<RefreshReport, SyncError> {
        let inconsistent = self
            .force_full_for
            .lock()
            .unwrap()
            .iter()
            .any(|r| r == repo);
        let mode = if force_full || inconsistent {
            RefreshMode::Full
        } else {
            RefreshMode::Incremental
        };
        self.calls
            .lock()
            .unwrap()
            .push((repo.to_owned(), mode == RefreshMode::Full));
        Ok(RefreshReport {
            mode,
            entries: 1,
            regenerated: 0,
        })
    }
}

/// Build a minimal repository tree on disk.
fn make_repo(root: &Path, name: &str, layout: &str) -> PathBuf {
    let loc = root.join(name);
    std::fs::create_dir_all(loc.join("profiles")).unwrap();
    std::fs::create_dir_all(loc.join("metadata")).unwrap();
    std::fs::write(loc.join("profiles/repo_name"), format!("{name}\n")).unwrap();
    if !layout.is_empty() {
        std::fs::write(loc.join("metadata/layout.conf"), layout).unwrap();
    }
    loc
}

/// Discover a repo set from a written `repos.conf` body.
fn discover_set(tmp: &Path, body: &str) -> RepoSet {
    let conf = tmp.join("repos.conf");
    std::fs::write(&conf, body).unwrap();
    discover(&conf).unwrap()
}

/// Load the auto-sync/post-sync extras from the `repos.conf` written by
/// [`discover_set`].
fn extras_for(tmp: &Path) -> crate::extras::ExtrasMap {
    crate::extras::ExtrasMap::load(tmp.join("repos.conf")).unwrap()
}

fn ok(stdout: &str) -> Result<CommandOutput, SyncError> {
    Ok(CommandOutput {
        code: Some(0),
        stdout: stdout.to_owned(),
        stderr: String::new(),
    })
}

fn fail(stderr: &str) -> Result<CommandOutput, SyncError> {
    Ok(CommandOutput {
        code: Some(1),
        stdout: String::new(),
        stderr: stderr.to_owned(),
    })
}

// --- Options resolution -----------------------------------------------------

#[test]
fn per_repo_override_takes_precedence_over_default() {
    let tmp = TempDir::new().unwrap();
    let loc = make_repo(tmp.path(), "gentoo", "");
    let set = discover_set(
        tmp.path(),
        &format!(
            "[gentoo]\nlocation = {}\nsync-type = git\nsync-uri = https://x\nsync-depth = 7\n",
            loc.display()
        ),
    );
    let defaults = SyncDefaults {
        depth: Some(1),
        ..SyncDefaults::default()
    };
    let opts = SyncOptions::resolve(set.get("gentoo").unwrap(), &defaults).unwrap();
    assert_eq!(opts.depth, Some(7));
}

#[test]
fn missing_sync_uri_is_config_error() {
    let tmp = TempDir::new().unwrap();
    let loc = make_repo(tmp.path(), "gentoo", "");
    let set = discover_set(
        tmp.path(),
        &format!(
            "[gentoo]\nlocation = {}\nsync-type = rsync\n",
            loc.display()
        ),
    );
    let err =
        SyncOptions::resolve(set.get("gentoo").unwrap(), &SyncDefaults::default()).unwrap_err();
    assert!(matches!(err, SyncError::Config { .. }));
}

// --- Engine ordering and selection ------------------------------------------

#[test]
fn masters_synced_before_dependents() {
    let tmp = TempDir::new().unwrap();
    let master = make_repo(tmp.path(), "gentoo", "");
    let child = make_repo(tmp.path(), "overlay", "");
    let set = discover_set(
        tmp.path(),
        &format!(
            "[gentoo]\nlocation = {}\nsync-type = webrsync\nsync-uri = https://m\n\
             [overlay]\nlocation = {}\nmasters = gentoo\nsync-type = webrsync\nsync-uri = https://c\n",
            master.display(),
            child.display()
        ),
    );
    // Remove tree dirs so backend reports change unconditionally; webrsync helper succeeds.
    let runner = FakeRunner::new().rule(|s| (s.program == "emerge-webrsync").then(|| ok("")));
    let registry = BackendRegistry::new(vec![Box::new(WebrsyncBackend::new(&runner))]);
    let refresher = FakeRefresher::new();
    let staging = tmp.path().join("staging");
    let engine = SyncEngine::new(&set, &registry, &refresher, &runner, &staging);

    let mut history = RevisionHistory::new();
    let report = engine.sync_all(&mut history);
    let order: Vec<&str> = report.results.iter().map(|(n, _)| n.as_str()).collect();
    let im = order.iter().position(|n| *n == "gentoo").unwrap();
    let ic = order.iter().position(|n| *n == "overlay").unwrap();
    assert!(im < ic, "master must precede dependent: {order:?}");
}

#[test]
fn auto_sync_disabled_is_skipped_but_explicit_overrides() {
    let tmp = TempDir::new().unwrap();
    let loc = make_repo(tmp.path(), "extra", "");
    let set = discover_set(
        tmp.path(),
        &format!(
            "[extra]\nlocation = {}\nsync-type = webrsync\nsync-uri = https://x\nauto-sync = no\n",
            loc.display()
        ),
    );
    let runner = FakeRunner::new().rule(|s| (s.program == "emerge-webrsync").then(|| ok("")));
    let registry = BackendRegistry::new(vec![Box::new(WebrsyncBackend::new(&runner))]);
    let refresher = FakeRefresher::new();
    let staging = tmp.path().join("staging");
    let engine = SyncEngine::new(&set, &registry, &refresher, &runner, &staging)
        .with_extras(extras_for(tmp.path()));

    let mut history = RevisionHistory::new();
    let skipped = engine.sync_all(&mut history);
    assert!(matches!(skipped.get("extra"), Some(RepoResult::Skipped)));

    let mut history = RevisionHistory::new();
    let named = engine.sync_named(&["extra".to_owned()], &mut history);
    assert!(named.get("extra").unwrap().is_synced());
}

#[test]
fn unknown_sync_type_is_isolated() {
    let tmp = TempDir::new().unwrap();
    let a = make_repo(tmp.path(), "good", "");
    let b = make_repo(tmp.path(), "weird", "");
    let set = discover_set(
        tmp.path(),
        &format!(
            "[good]\nlocation = {}\nsync-type = webrsync\nsync-uri = https://a\n\
             [weird]\nlocation = {}\nsync-type = bogus\nsync-uri = https://b\n",
            a.display(),
            b.display()
        ),
    );
    let runner = FakeRunner::new().rule(|s| (s.program == "emerge-webrsync").then(|| ok("")));
    let registry = BackendRegistry::new(vec![Box::new(WebrsyncBackend::new(&runner))]);
    let refresher = FakeRefresher::new();
    let staging = tmp.path().join("staging");
    let engine = SyncEngine::new(&set, &registry, &refresher, &runner, &staging);
    let mut history = RevisionHistory::new();
    let report = engine.sync_all(&mut history);
    assert!(report.get("good").unwrap().is_synced());
    assert!(matches!(
        report.get("weird"),
        Some(RepoResult::Failed(SyncError::UnknownBackend { .. }))
    ));
}

#[test]
fn unimplemented_sync_type_is_isolated() {
    let tmp = TempDir::new().unwrap();
    let b = make_repo(tmp.path(), "old", "");
    let set = discover_set(
        tmp.path(),
        &format!(
            "[old]\nlocation = {}\nsync-type = cvs\nsync-uri = :pserver:x\n",
            b.display()
        ),
    );
    let runner = FakeRunner::new();
    let registry = BackendRegistry::new(vec![Box::new(WebrsyncBackend::new(&runner))]);
    let refresher = FakeRefresher::new();
    let staging = tmp.path().join("staging");
    let engine = SyncEngine::new(&set, &registry, &refresher, &runner, &staging);
    let mut history = RevisionHistory::new();
    let report = engine.sync_all(&mut history);
    assert!(matches!(
        report.get("old"),
        Some(RepoResult::Failed(SyncError::UnimplementedBackend { .. }))
    ));
}

// --- Refresh selection ------------------------------------------------------

#[test]
fn changed_tree_triggers_incremental_refresh() {
    let tmp = TempDir::new().unwrap();
    let loc = make_repo(tmp.path(), "g", "");
    let set = discover_set(
        tmp.path(),
        &format!(
            "[g]\nlocation = {}\nsync-type = webrsync\nsync-uri = https://x\n",
            loc.display()
        ),
    );
    let runner = FakeRunner::new().rule(|s| (s.program == "emerge-webrsync").then(|| ok("")));
    let registry = BackendRegistry::new(vec![Box::new(WebrsyncBackend::new(&runner))]);
    let refresher = FakeRefresher::new();
    let staging = tmp.path().join("staging");
    let engine = SyncEngine::new(&set, &registry, &refresher, &runner, &staging);
    let mut history = RevisionHistory::new();
    engine.sync_all(&mut history);
    assert_eq!(refresher.calls(), vec![("g".to_owned(), false)]);
}

#[test]
fn unchanged_tree_skips_refresh() {
    // rsync update with matching timestamp reports no change.
    let tmp = TempDir::new().unwrap();
    let loc = make_repo(tmp.path(), "g", "");
    std::fs::write(
        loc.join("metadata/timestamp.chk"),
        "Sun, 21 Jun 2026 05:45:00 +0000\n",
    )
    .unwrap();
    let set = discover_set(
        tmp.path(),
        &format!(
            "[g]\nlocation = {}\nsync-type = rsync\nsync-uri = rsync://x\n",
            loc.display()
        ),
    );
    // Probe writes the server timestamp into staging/timestamp.chk equal to local.
    let staging = tmp.path().join("staging");
    let staging_for_rule = staging.clone();
    let runner = FakeRunner::new().rule(move |s| {
        if s.program == "rsync" && s.args.iter().any(|a| a.contains("timestamp.chk")) {
            std::fs::create_dir_all(staging_for_rule.join("g")).ok();
            std::fs::write(
                staging_for_rule.join("g/timestamp.chk"),
                "Sun, 21 Jun 2026 05:45:00 +0000\n",
            )
            .ok();
            Some(ok(""))
        } else {
            None
        }
    });
    let registry = BackendRegistry::new(vec![Box::new(RsyncBackend::new(&runner))]);
    let refresher = FakeRefresher::new();
    let engine = SyncEngine::new(&set, &registry, &refresher, &runner, &staging);
    let mut history = RevisionHistory::new();
    let report = engine.sync_all(&mut history);
    assert!(report.get("g").unwrap().is_synced());
    assert!(
        refresher.calls().is_empty(),
        "unchanged tree must not refresh"
    );
}

#[test]
fn inconsistent_store_falls_back_to_full_refresh() {
    let tmp = TempDir::new().unwrap();
    let loc = make_repo(tmp.path(), "g", "");
    let set = discover_set(
        tmp.path(),
        &format!(
            "[g]\nlocation = {}\nsync-type = webrsync\nsync-uri = https://x\n",
            loc.display()
        ),
    );
    let runner = FakeRunner::new().rule(|s| (s.program == "emerge-webrsync").then(|| ok("")));
    let registry = BackendRegistry::new(vec![Box::new(WebrsyncBackend::new(&runner))]);
    let refresher = FakeRefresher::new();
    refresher.mark_inconsistent("g");
    let staging = tmp.path().join("staging");
    let engine = SyncEngine::new(&set, &registry, &refresher, &runner, &staging);
    let mut history = RevisionHistory::new();
    engine.sync_all(&mut history);
    assert_eq!(refresher.calls(), vec![("g".to_owned(), true)]);
}

// --- rsync backend ----------------------------------------------------------

#[test]
fn rsync_server_out_of_date_preserves_tree() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    std::fs::create_dir_all(loc.join("metadata")).unwrap();
    std::fs::write(
        loc.join("metadata/timestamp.chk"),
        "Mon, 22 Jun 2026 05:45:00 +0000\n",
    )
    .unwrap();
    std::fs::write(loc.join("marker"), "keep").unwrap();
    let staging = tmp.path().join("staging/g");
    std::fs::create_dir_all(&staging).unwrap();

    let staging_for_rule = staging.clone();
    let runner = FakeRunner::new().rule(move |s| {
        if s.args.iter().any(|a| a.contains("timestamp.chk")) {
            std::fs::write(
                staging_for_rule.join("timestamp.chk"),
                "Sun, 21 Jun 2026 05:45:00 +0000\n",
            )
            .ok();
            Some(ok(""))
        } else {
            None
        }
    });
    let backend = RsyncBackend::new(&runner);
    let opts = SyncOptions {
        sync_type: "rsync".into(),
        uri: "rsync://x".into(),
        auto_sync: true,
        timeout_secs: 5,
        rsync_initial_timeout_secs: 15,
        retries: 1,
        depth: None,
        rsync_extra_opts: vec![],
        rsync_opts_override: None,
        rsync_vcs_ignore: false,
        verify_metamanifest: false,
        rsync_verify_jobs: None,
        rsync_verify_max_age_days: 0,
        git_verify_commit_signature: false,
        git_verify_max_age_days: 0,
        git_env: vec![],
        git_clone_env: vec![],
        git_pull_env: vec![],
        git_clone_extra_opts: vec![],
        git_pull_extra_opts: vec![],
        webrsync_verify_signature: false,
        webrsync_keep_snapshots: false,
        openpgp_key_path: None,
        openpgp_keyserver: None,
        key_refresh: crate::options::KeyRefresh::Disabled,
        refresh_retries: 0,
        refresh_retry_overall_timeout: None,
        refresh_retry_delay_mult: 1.0,
        refresh_retry_delay_exp_base: 2.0,
        refresh_retry_delay_max: None,
        post_sync: None,
        volatile: false,
    };
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    let err = backend.update(&ctx).unwrap_err();
    assert!(matches!(err, SyncError::ServerOutOfDate { .. }));
    // Prior tree untouched.
    assert!(loc.join("marker").exists());
}

#[test]
fn rsync_transfer_includes_standard_excludes_and_extra_opts() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    let staging = tmp.path().join("staging/g");
    std::fs::create_dir_all(&staging).unwrap();
    let runner = FakeRunner::new().rule(|s| {
        if s.program == "rsync" && s.args.iter().any(|a| a == "--recursive") {
            Some(ok(""))
        } else {
            None
        }
    });
    let backend = RsyncBackend::new(&runner);
    let opts = SyncOptions {
        sync_type: "rsync".into(),
        uri: "rsync://mirror/gentoo".into(),
        auto_sync: true,
        timeout_secs: 30,
        rsync_initial_timeout_secs: 15,
        retries: 1,
        depth: None,
        rsync_extra_opts: vec!["--bwlimit=1000".into()],
        rsync_opts_override: None,
        rsync_vcs_ignore: false,
        verify_metamanifest: false,
        rsync_verify_jobs: None,
        rsync_verify_max_age_days: 0,
        git_verify_commit_signature: false,
        git_verify_max_age_days: 0,
        git_env: vec![],
        git_clone_env: vec![],
        git_pull_env: vec![],
        git_clone_extra_opts: vec![],
        git_pull_extra_opts: vec![],
        webrsync_verify_signature: false,
        webrsync_keep_snapshots: false,
        openpgp_key_path: None,
        openpgp_keyserver: None,
        key_refresh: crate::options::KeyRefresh::Disabled,
        refresh_retries: 0,
        refresh_retry_overall_timeout: None,
        refresh_retry_delay_mult: 1.0,
        refresh_retry_delay_exp_base: 2.0,
        refresh_retry_delay_max: None,
        post_sync: None,
        volatile: false,
    };
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    let out = backend.fetch(&ctx).unwrap();
    assert_eq!(out.kind, SyncKind::Initial);
    let call = runner
        .calls()
        .into_iter()
        .find(|c| c.args.iter().any(|a| a == "--recursive"))
        .unwrap();
    assert!(call.args.iter().any(|a| a == "--exclude=/distfiles"));
    assert!(call.args.iter().any(|a| a == "--bwlimit=1000"));
    // Portage does not exclude `/.git`; the VCS case is handled by check_vcs.
    assert!(!call.args.iter().any(|a| a == "--exclude=/.git"));
}

#[test]
fn rsync_verification_failure_preserves_prior_tree() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    std::fs::create_dir_all(loc.join("metadata")).unwrap();
    std::fs::write(
        loc.join("metadata/timestamp.chk"),
        "Sun, 21 Jun 2026 05:45:00 +0000\n",
    )
    .unwrap();
    std::fs::write(loc.join("marker"), "keep").unwrap();
    let staging = tmp.path().join("staging/g");
    std::fs::create_dir_all(&staging).unwrap();

    let staging_for_rule = staging.clone();
    let runner = FakeRunner::new()
        .rule(move |s| {
            if s.program == "rsync" && s.args.iter().any(|a| a.contains("timestamp.chk")) {
                std::fs::write(
                    staging_for_rule.join("timestamp.chk"),
                    "Mon, 22 Jun 2026 05:45:00 +0000\n",
                )
                .ok();
                Some(ok(""))
            } else {
                None
            }
        })
        .rule(|s| {
            (s.program == "rsync" && s.args.iter().any(|a| a == "--recursive")).then(|| ok(""))
        })
        .rule(|s| (s.program == "gpg").then(|| fail("BAD signature")));
    let backend = RsyncBackend::new(&runner);
    let opts = SyncOptions {
        sync_type: "rsync".into(),
        uri: "rsync://x".into(),
        auto_sync: true,
        timeout_secs: 5,
        rsync_initial_timeout_secs: 15,
        retries: 1,
        depth: None,
        rsync_extra_opts: vec![],
        rsync_opts_override: None,
        rsync_vcs_ignore: false,
        verify_metamanifest: true,
        rsync_verify_jobs: None,
        rsync_verify_max_age_days: 0,
        git_verify_commit_signature: true,
        git_verify_max_age_days: 0,
        git_env: vec![],
        git_clone_env: vec![],
        git_pull_env: vec![],
        git_clone_extra_opts: vec![],
        git_pull_extra_opts: vec![],
        webrsync_verify_signature: true,
        webrsync_keep_snapshots: false,
        openpgp_key_path: None,
        openpgp_keyserver: None,
        key_refresh: crate::options::KeyRefresh::Disabled,
        refresh_retries: 0,
        refresh_retry_overall_timeout: None,
        refresh_retry_delay_mult: 1.0,
        refresh_retry_delay_exp_base: 2.0,
        refresh_retry_delay_max: None,
        post_sync: None,
        volatile: false,
    };
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    let err = backend.update(&ctx).unwrap_err();
    assert!(matches!(err, SyncError::Verification { .. }));
    assert!(
        loc.join("marker").exists(),
        "prior tree must survive failed verification"
    );
}

#[test]
fn metamanifest_verify_detects_tampered_file() {
    use crate::verify::Verifier;
    let tmp = TempDir::new().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("foo"), b"abc").unwrap();
    let digest = moraine_common::hash::sha256(b"abc");
    std::fs::write(
        staging.join("Manifest"),
        format!("DATA foo 3 SHA256 {digest}\nTIMESTAMP 2026-06-21T05:38:02Z\n"),
    )
    .unwrap();

    let runner = FakeRunner::new().rule(|s| (s.program == "gpg").then(|| ok("")));
    let mut opts = rsync_verify_opts();
    opts.verify_metamanifest = true;
    let verifier = Verifier::new(&runner);
    let gnupg = tmp.path().join("gnupg");

    // A clean tree with a matching signature and hash verifies.
    verifier
        .verify_rsync_tree("g", &staging, &opts, &gnupg)
        .expect("clean tree must verify");

    // Tampering a listed file fails even though the signature is accepted.
    std::fs::write(staging.join("foo"), b"xyz").unwrap();
    let err = verifier
        .verify_rsync_tree("g", &staging, &opts, &gnupg)
        .unwrap_err();
    assert!(matches!(err, SyncError::Verification { .. }));

    // A file listed in the Manifest but missing from the tree also fails.
    std::fs::remove_file(staging.join("foo")).unwrap();
    let err = verifier
        .verify_rsync_tree("g", &staging, &opts, &gnupg)
        .unwrap_err();
    assert!(matches!(err, SyncError::Verification { .. }));
}

#[test]
fn rsync_transfer_retries_on_transport_failure() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("repo");
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let attempts = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let runner = FakeRunner::new().rule(move |s| {
        if s.program == "rsync" && s.args.iter().any(|a| a == "--recursive") {
            // Fail the first attempt, succeed on the retry.
            let n = counter.fetch_add(1, Ordering::SeqCst);
            Some(if n == 0 {
                fail("connection refused")
            } else {
                ok("")
            })
        } else {
            None
        }
    });
    let backend = RsyncBackend::new(&runner);
    let mut opts = rsync_verify_opts();
    opts.verify_metamanifest = false;
    opts.retries = 3;
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    let outcome = backend.fetch(&ctx).expect("retry must recover");
    assert!(outcome.changed);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "one failure then one success"
    );
}

fn rsync_verify_opts() -> SyncOptions {
    SyncOptions {
        sync_type: "rsync".into(),
        uri: "rsync://x".into(),
        auto_sync: true,
        timeout_secs: 5,
        rsync_initial_timeout_secs: 15,
        retries: 1,
        depth: None,
        rsync_extra_opts: vec![],
        rsync_opts_override: None,
        rsync_vcs_ignore: false,
        verify_metamanifest: true,
        rsync_verify_jobs: None,
        rsync_verify_max_age_days: 0,
        git_verify_commit_signature: false,
        git_verify_max_age_days: 0,
        git_env: vec![],
        git_clone_env: vec![],
        git_pull_env: vec![],
        git_clone_extra_opts: vec![],
        git_pull_extra_opts: vec![],
        webrsync_verify_signature: false,
        webrsync_keep_snapshots: false,
        openpgp_key_path: None,
        openpgp_keyserver: None,
        key_refresh: crate::options::KeyRefresh::Disabled,
        refresh_retries: 0,
        refresh_retry_overall_timeout: None,
        refresh_retry_delay_mult: 1.0,
        refresh_retry_delay_exp_base: 2.0,
        refresh_retry_delay_max: None,
        post_sync: None,
        volatile: false,
    }
}

// --- git backend ------------------------------------------------------------

#[test]
fn git_initial_clone_is_shallow_by_default() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    let staging = tmp.path().join("staging/g");
    let runner = FakeRunner::new()
        .rule(|s| (s.program == "git" && s.args.iter().any(|a| a == "clone")).then(|| ok("")))
        .rule(|s| (s.program == "git" && s.args.iter().any(|a| a == "config")).then(|| ok("")))
        .rule(|s| {
            (s.program == "git" && s.args.iter().any(|a| a == "rev-parse")).then(|| ok("abc123"))
        });
    let backend = GitBackend::new(&runner);
    let opts = git_opts(None);
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    let out = backend.fetch(&ctx).unwrap();
    assert_eq!(out.head.as_deref(), Some("abc123"));
    let clone = runner
        .calls()
        .into_iter()
        .find(|c| c.args.iter().any(|a| a == "clone"))
        .unwrap();
    assert!(clone.args.iter().any(|a| a == "--depth=1"));
}

#[test]
fn git_depth_zero_requests_full_history() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    let staging = tmp.path().join("staging/g");
    let runner = FakeRunner::new()
        .rule(|s| (s.program == "git" && s.args.iter().any(|a| a == "clone")).then(|| ok("")))
        .rule(|s| (s.program == "git" && s.args.iter().any(|a| a == "config")).then(|| ok("")))
        .rule(|s| (s.program == "git" && s.args.iter().any(|a| a == "rev-parse")).then(|| ok("h")));
    let backend = GitBackend::new(&runner);
    let opts = git_opts(Some(0));
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    backend.fetch(&ctx).unwrap();
    let clone = runner
        .calls()
        .into_iter()
        .find(|c| c.args.iter().any(|a| a == "clone"))
        .unwrap();
    assert!(!clone.args.iter().any(|a| a.starts_with("--depth")));
}

#[test]
fn git_change_detected_only_when_head_moves() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    std::fs::create_dir_all(loc.join(".git")).unwrap();
    let staging = tmp.path().join("staging/g");

    // rev-parse returns "old" before, "old" after: no change.
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let runner = FakeRunner::new()
        .rule(|s| (s.program == "git" && s.args.iter().any(|a| a == "config")).then(|| ok("")))
        .rule(|s| (s.program == "git" && s.args.iter().any(|a| a == "fetch")).then(|| ok("")))
        .rule(|s| (s.program == "git" && s.args.iter().any(|a| a == "merge")).then(|| ok("")))
        .rule(move |s| {
            if s.program == "git" && s.args.iter().any(|a| a == "rev-parse") {
                let _ = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(ok("same"))
            } else {
                None
            }
        })
        // The non-volatile clobber path also runs remote/clean/reset/gc.
        .rule(|s| (s.program == "git").then(|| ok("")));
    let backend = GitBackend::new(&runner);
    let opts = git_opts(None);
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    let out = backend.update(&ctx).unwrap();
    assert!(!out.changed, "head did not move, so no change");
}

#[test]
fn git_volatile_repo_is_not_clobbered() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    std::fs::create_dir_all(loc.join(".git")).unwrap();
    let staging = tmp.path().join("staging/g");
    let runner = FakeRunner::new().rule(|s| (s.program == "git").then(|| ok("h")));
    let backend = GitBackend::new(&runner);
    let mut opts = git_opts(None);
    opts.volatile = true;
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    backend.update(&ctx).unwrap();
    // A volatile repo must never run the destructive clobber commands.
    for call in runner.calls() {
        assert!(!call.args.iter().any(|a| a == "clean"));
        assert!(!call.args.iter().any(|a| a == "reset"));
        assert!(!call.args.iter().any(|a| a == "gc"));
    }
}

#[test]
fn postsync_hooks_run_with_argv_and_change_gate() {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = TempDir::new().unwrap();
    let loc = make_repo(tmp.path(), "gentoo", "");
    let set = discover_set(
        tmp.path(),
        &format!(
            "[gentoo]\nlocation = {}\nsync-type = webrsync\nsync-uri = https://m\n",
            loc.display()
        ),
    );
    // A repo.postsync.d hook (executable).
    let hooks = tmp.path().join("config/etc/portage/repo.postsync.d");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("10-notify");
    std::fs::write(&hook, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let runner = FakeRunner::new()
        .rule(|s| (s.program == "emerge-webrsync").then(|| ok("")))
        .rule(|s| s.program.ends_with("10-notify").then(|| ok("")));
    let registry = BackendRegistry::new(vec![Box::new(WebrsyncBackend::new(&runner))]);
    let refresher = FakeRefresher::new();
    let staging = tmp.path().join("staging");
    let engine = SyncEngine::new(&set, &registry, &refresher, &runner, &staging)
        .with_config_root(tmp.path().join("config"));

    let mut history = RevisionHistory::new();
    let report = engine.sync_all(&mut history);
    assert!(report.get("gentoo").unwrap().is_synced());
    let hook_call = runner
        .calls()
        .into_iter()
        .find(|c| c.program.ends_with("10-notify"))
        .expect("hook must run");
    // argv is [reponame, sync_uri, repo_location].
    assert_eq!(hook_call.args[0], "gentoo");
    assert_eq!(hook_call.args[1], "https://m");
    assert_eq!(hook_call.args[2], loc.to_string_lossy());
}

fn git_opts(depth: Option<u32>) -> SyncOptions {
    SyncOptions {
        sync_type: "git".into(),
        uri: "https://example/repo.git".into(),
        auto_sync: true,
        timeout_secs: 30,
        rsync_initial_timeout_secs: 15,
        retries: 1,
        depth,
        rsync_extra_opts: vec![],
        rsync_opts_override: None,
        rsync_vcs_ignore: false,
        verify_metamanifest: false,
        rsync_verify_jobs: None,
        rsync_verify_max_age_days: 0,
        git_verify_commit_signature: false,
        git_verify_max_age_days: 0,
        git_env: vec![],
        git_clone_env: vec![],
        git_pull_env: vec![],
        git_clone_extra_opts: vec![],
        git_pull_extra_opts: vec![],
        webrsync_verify_signature: false,
        webrsync_keep_snapshots: false,
        openpgp_key_path: None,
        openpgp_keyserver: None,
        key_refresh: crate::options::KeyRefresh::Disabled,
        refresh_retries: 0,
        refresh_retry_overall_timeout: None,
        refresh_retry_delay_mult: 1.0,
        refresh_retry_delay_exp_base: 2.0,
        refresh_retry_delay_max: None,
        post_sync: None,
        volatile: false,
    }
}

// --- webrsync backend -------------------------------------------------------

#[test]
fn webrsync_signature_rejection_is_verification_error() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    let staging = tmp.path().join("staging/g");
    let key = tmp.path().join("release.gpg");
    std::fs::write(&key, "KEY").unwrap();
    let runner = FakeRunner::new()
        .rule(|s| (s.program == "emerge-webrsync").then(|| fail("gpg: BAD signature")));
    let backend = WebrsyncBackend::new(&runner);
    let opts = SyncOptions {
        sync_type: "webrsync".into(),
        uri: "https://x".into(),
        auto_sync: true,
        timeout_secs: 30,
        rsync_initial_timeout_secs: 15,
        retries: 1,
        depth: None,
        rsync_extra_opts: vec![],
        rsync_opts_override: None,
        rsync_vcs_ignore: false,
        verify_metamanifest: true,
        rsync_verify_jobs: None,
        rsync_verify_max_age_days: 0,
        git_verify_commit_signature: true,
        git_verify_max_age_days: 0,
        git_env: vec![],
        git_clone_env: vec![],
        git_pull_env: vec![],
        git_clone_extra_opts: vec![],
        git_pull_extra_opts: vec![],
        webrsync_verify_signature: true,
        webrsync_keep_snapshots: false,
        openpgp_key_path: Some(key),
        openpgp_keyserver: None,
        key_refresh: crate::options::KeyRefresh::Disabled,
        refresh_retries: 0,
        refresh_retry_overall_timeout: None,
        refresh_retry_delay_mult: 1.0,
        refresh_retry_delay_exp_base: 2.0,
        refresh_retry_delay_max: None,
        post_sync: None,
        volatile: false,
    };
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    let err = backend.fetch(&ctx).unwrap_err();
    assert!(matches!(err, SyncError::Verification { .. }));
}

#[test]
fn webrsync_command_has_no_repo_and_default_no_pgp_verify() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    let staging = tmp.path().join("staging/g");

    // Verify off (default): the command takes --no-pgp-verify and never --repo.
    let runner = FakeRunner::new().rule(|s| (s.program == "emerge-webrsync").then(|| ok("")));
    let mut opts = webrsync_opts();
    {
        let backend = WebrsyncBackend::new(&runner);
        let ctx = SyncContext {
            repo: "g",
            location: &loc,
            staging: &staging,
            options: &opts,
        };
        backend.fetch(&ctx).unwrap();
    }
    let call = &runner.calls()[0];
    assert!(call.args.iter().any(|a| a == "--no-pgp-verify"));
    assert!(!call.args.iter().any(|a| a == "--repo"));

    // Verify on: the GPG environment is exported and --no-pgp-verify is dropped.
    let key = tmp.path().join("release.gpg");
    std::fs::write(&key, "KEY").unwrap();
    opts.webrsync_verify_signature = true;
    opts.openpgp_key_path = Some(key);
    opts.openpgp_keyserver = Some("hkps://keys.gentoo.org".into());
    opts.webrsync_keep_snapshots = true;
    let runner2 = FakeRunner::new().rule(|s| (s.program == "emerge-webrsync").then(|| ok("")));
    {
        let backend = WebrsyncBackend::new(&runner2);
        let ctx = SyncContext {
            repo: "g",
            location: &loc,
            staging: &staging,
            options: &opts,
        };
        backend.fetch(&ctx).unwrap();
    }
    let call = &runner2.calls()[0];
    assert!(!call.args.iter().any(|a| a == "--no-pgp-verify"));
    assert!(call.args.iter().any(|a| a == "-k"));
    assert!(
        call.env
            .iter()
            .any(|(k, v)| k == "PORTAGE_SYNC_WEBRSYNC_GPG" && v == "1")
    );
    assert!(call.env.iter().any(|(k, _)| k == "PORTAGE_GPG_KEY"));
    assert!(call.env.iter().any(|(k, _)| k == "PORTAGE_GPG_KEY_SERVER"));
}

/// Default webrsync options with verification off.
fn webrsync_opts() -> SyncOptions {
    SyncOptions {
        sync_type: "webrsync".into(),
        uri: "https://x".into(),
        auto_sync: true,
        timeout_secs: 30,
        rsync_initial_timeout_secs: 15,
        retries: 1,
        depth: None,
        rsync_extra_opts: vec![],
        rsync_opts_override: None,
        rsync_vcs_ignore: false,
        verify_metamanifest: false,
        rsync_verify_jobs: None,
        rsync_verify_max_age_days: 0,
        git_verify_commit_signature: false,
        git_verify_max_age_days: 0,
        git_env: vec![],
        git_clone_env: vec![],
        git_pull_env: vec![],
        git_clone_extra_opts: vec![],
        git_pull_extra_opts: vec![],
        webrsync_verify_signature: false,
        webrsync_keep_snapshots: false,
        openpgp_key_path: None,
        openpgp_keyserver: None,
        key_refresh: crate::options::KeyRefresh::Disabled,
        refresh_retries: 0,
        refresh_retry_overall_timeout: None,
        refresh_retry_delay_mult: 1.0,
        refresh_retry_delay_exp_base: 2.0,
        refresh_retry_delay_max: None,
        post_sync: None,
        volatile: false,
    }
}

// --- verification key handling ----------------------------------------------

#[test]
fn key_loaded_from_configured_path_with_refresh_disabled() {
    use crate::verify::Verifier;
    let tmp = TempDir::new().unwrap();
    let key = tmp.path().join("key.gpg");
    std::fs::write(&key, "KEY").unwrap();
    let home = tmp.path().join("gnupg");
    std::fs::create_dir_all(&home).unwrap();

    let runner = FakeRunner::new()
        .rule(|s| (s.program == "gpg" && s.args.iter().any(|a| a == "--import")).then(|| ok("")));
    let verifier = Verifier::new(&runner);
    let mut opts = git_opts(None);
    opts.openpgp_key_path = Some(key.clone());
    opts.key_refresh = crate::options::KeyRefresh::Disabled;

    let result = verifier.prepare_keys("g", &opts, &home).unwrap();
    assert_eq!(result.as_deref(), Some(home.as_path()));
    // The import ran; no refresh command was issued because refresh is disabled.
    let calls = runner.calls();
    assert!(calls.iter().any(|c| c.args.iter().any(|a| a == "--import")));
    assert!(
        !calls
            .iter()
            .any(|c| c.args.iter().any(|a| a == "--refresh-keys")),
        "refresh disabled must not run a refresh"
    );
}

#[test]
fn key_refresh_attempted_under_keyserver_policy() {
    use crate::verify::Verifier;
    let tmp = TempDir::new().unwrap();
    let key = tmp.path().join("key.gpg");
    std::fs::write(&key, "KEY").unwrap();
    let home = tmp.path().join("gnupg");

    let runner = FakeRunner::new()
        .rule(|s| (s.program == "gpg" && s.args.iter().any(|a| a == "--import")).then(|| ok("")))
        .rule(|s| {
            (s.program == "gpg" && s.args.iter().any(|a| a == "--refresh-keys")).then(|| ok(""))
        });
    let verifier = Verifier::new(&runner);
    let mut opts = git_opts(None);
    opts.openpgp_key_path = Some(key);
    opts.key_refresh = crate::options::KeyRefresh::Keyserver;
    opts.openpgp_keyserver = Some("hkps://keys.gentoo.org".into());
    opts.refresh_retries = 2;

    verifier.prepare_keys("g", &opts, &home).unwrap();
    // The configured keyserver is forwarded to gpg via `--keyserver`.
    let refresh = runner
        .calls()
        .into_iter()
        .find(|c| c.args.iter().any(|a| a == "--refresh-keys"))
        .expect("keyserver policy must run a refresh");
    let pos = refresh
        .args
        .iter()
        .position(|a| a == "--keyserver")
        .unwrap();
    assert_eq!(refresh.args[pos + 1], "hkps://keys.gentoo.org");
    // WKD is not attempted under the keyserver-only policy.
    assert!(
        !runner
            .calls()
            .iter()
            .any(|c| c.args.iter().any(|a| a == "--locate-external-key"))
    );
}

#[test]
fn wkd_then_keyserver_falls_back_when_wkd_fails() {
    use crate::verify::Verifier;
    let tmp = TempDir::new().unwrap();
    let key = tmp.path().join("key.gpg");
    std::fs::write(&key, "KEY").unwrap();
    let home = tmp.path().join("gnupg");

    let runner = FakeRunner::new()
        .rule(|s| (s.program == "gpg" && s.args.iter().any(|a| a == "--import")).then(|| ok("")))
        .rule(|s| {
            (s.program == "gpg" && s.args.iter().any(|a| a == "--locate-external-key"))
                .then(|| fail("no WKD entry"))
        })
        .rule(|s| {
            (s.program == "gpg" && s.args.iter().any(|a| a == "--refresh-keys")).then(|| ok(""))
        });
    let verifier = Verifier::new(&runner);
    let mut opts = git_opts(None);
    opts.openpgp_key_path = Some(key);
    opts.key_refresh = crate::options::KeyRefresh::WkdThenKeyserver;

    verifier.prepare_keys("g", &opts, &home).unwrap();
    let calls = runner.calls();
    // WKD is attempted first, then the keyserver refresh runs as the fallback.
    let wkd = calls
        .iter()
        .position(|c| c.args.iter().any(|a| a == "--locate-external-key"))
        .expect("WKD must be attempted first");
    let keyserver = calls
        .iter()
        .position(|c| c.args.iter().any(|a| a == "--refresh-keys"))
        .expect("keyserver fallback must run after WKD fails");
    assert!(wkd < keyserver);
}

#[test]
fn keyserver_refresh_honors_retry_count_then_fails() {
    use crate::verify::Verifier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let tmp = TempDir::new().unwrap();
    let key = tmp.path().join("key.gpg");
    std::fs::write(&key, "KEY").unwrap();
    let home = tmp.path().join("gnupg");

    let refreshes = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = refreshes.clone();
    let runner = FakeRunner::new()
        .rule(|s| (s.program == "gpg" && s.args.iter().any(|a| a == "--import")).then(|| ok("")))
        .rule(move |s| {
            if s.program == "gpg" && s.args.iter().any(|a| a == "--refresh-keys") {
                counter.fetch_add(1, Ordering::SeqCst);
                Some(fail("keyserver unreachable"))
            } else {
                None
            }
        });
    let verifier = Verifier::new(&runner);
    let mut opts = git_opts(None);
    opts.openpgp_key_path = Some(key);
    opts.key_refresh = crate::options::KeyRefresh::Keyserver;
    opts.refresh_retries = 3;
    // A zero multiplier exercises the retry count without any real sleep.
    opts.refresh_retry_delay_mult = 0.0;

    let err = verifier.prepare_keys("g", &opts, &home).unwrap_err();
    assert!(matches!(err, SyncError::Verification { .. }));
    assert_eq!(
        refreshes.load(Ordering::SeqCst),
        3,
        "the configured retry count must be honored"
    );
}

#[test]
fn webrsync_verify_without_key_path_fails_fast() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    let staging = tmp.path().join("staging/g");
    let runner = FakeRunner::new().rule(|s| (s.program == "emerge-webrsync").then(|| ok("")));
    let backend = WebrsyncBackend::new(&runner);
    let mut opts = webrsync_opts();
    opts.webrsync_verify_signature = true;
    opts.openpgp_key_path = None;
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    let err = backend.fetch(&ctx).unwrap_err();
    assert!(matches!(err, SyncError::Verification { .. }));
    assert!(
        runner.calls().is_empty(),
        "no helper call when the key path is unset"
    );
}

#[test]
fn webrsync_verify_with_missing_key_file_fails_fast() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    let staging = tmp.path().join("staging/g");
    let runner = FakeRunner::new().rule(|s| (s.program == "emerge-webrsync").then(|| ok("")));
    let backend = WebrsyncBackend::new(&runner);
    let mut opts = webrsync_opts();
    opts.webrsync_verify_signature = true;
    opts.openpgp_key_path = Some(tmp.path().join("absent.gpg"));
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    let err = backend.fetch(&ctx).unwrap_err();
    assert!(matches!(err, SyncError::Verification { .. }));
    assert!(
        runner.calls().is_empty(),
        "no helper call when the key file is missing"
    );
}

#[test]
fn webrsync_verify_with_present_key_invokes_helper() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    let staging = tmp.path().join("staging/g");
    let key = tmp.path().join("release.gpg");
    std::fs::write(&key, "KEY").unwrap();
    let runner = FakeRunner::new().rule(|s| (s.program == "emerge-webrsync").then(|| ok("")));
    let backend = WebrsyncBackend::new(&runner);
    let mut opts = webrsync_opts();
    opts.webrsync_verify_signature = true;
    opts.openpgp_key_path = Some(key);
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    backend.fetch(&ctx).unwrap();
    let call = &runner.calls()[0];
    assert_eq!(call.program, "emerge-webrsync");
    assert!(
        call.env
            .iter()
            .any(|(k, v)| k == "PORTAGE_SYNC_WEBRSYNC_GPG" && v == "1")
    );
    assert!(call.env.iter().any(|(k, _)| k == "PORTAGE_GPG_KEY"));
}

// --- sync-transport-backends additions --------------------------------------

#[test]
fn rsync_empty_server_timestamp_drives_full_transfer() {
    // A mirror mid-regeneration serves an empty timestamp.chk: the probe succeeds
    // but the file is unparseable, so the server timestamp is zero and a full
    // transfer runs rather than a transport failure.
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    std::fs::create_dir_all(loc.join("metadata")).unwrap();
    std::fs::write(
        loc.join("metadata/timestamp.chk"),
        "Mon, 22 Jun 2026 05:45:00 +0000\n",
    )
    .unwrap();
    let staging = tmp.path().join("staging/g");
    std::fs::create_dir_all(&staging).unwrap();

    let staging_for_rule = staging.clone();
    let runner = FakeRunner::new()
        .rule(move |s| {
            if s.program == "rsync" && s.args.iter().any(|a| a.contains("timestamp.chk")) {
                std::fs::write(staging_for_rule.join("timestamp.chk"), "").ok();
                Some(ok(""))
            } else {
                None
            }
        })
        .rule(|s| {
            (s.program == "rsync" && s.args.iter().any(|a| a == "--recursive")).then(|| ok(""))
        });
    let backend = RsyncBackend::new(&runner);
    let mut opts = rsync_verify_opts();
    opts.verify_metamanifest = false;
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    let outcome = backend
        .update(&ctx)
        .expect("an empty server timestamp must drive a transfer, not fail");
    assert!(
        outcome.changed,
        "an empty server timestamp.chk forces a full transfer"
    );
}

#[test]
fn rsync_override_timeout_not_duplicated() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    let staging = tmp.path().join("staging/g");
    std::fs::create_dir_all(&staging).unwrap();
    let runner = FakeRunner::new().rule(|s| {
        (s.program == "rsync" && s.args.iter().any(|a| a == "--recursive")).then(|| ok(""))
    });
    let backend = RsyncBackend::new(&runner);
    let mut opts = rsync_verify_opts();
    opts.verify_metamanifest = false;
    opts.uri = "rsync://mirror/gentoo".into();
    opts.rsync_opts_override = Some(vec![
        "--recursive".into(),
        "--times".into(),
        "--timeout=900".into(),
    ]);
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    backend.fetch(&ctx).unwrap();
    let call = runner
        .calls()
        .into_iter()
        .find(|c| c.args.iter().any(|a| a == "--recursive"))
        .unwrap();
    let timeouts = call
        .args
        .iter()
        .filter(|a| a.starts_with("--timeout="))
        .count();
    assert_eq!(
        timeouts, 1,
        "a user-supplied --timeout must not be duplicated"
    );
    assert!(call.args.iter().any(|a| a == "--timeout=900"));
}

#[test]
fn rsync_gentoo_portage_override_reinjects_compress_and_timeout() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    let staging = tmp.path().join("staging/g");
    std::fs::create_dir_all(&staging).unwrap();
    let runner = FakeRunner::new().rule(|s| {
        (s.program == "rsync" && s.args.iter().any(|a| a == "--recursive")).then(|| ok(""))
    });
    let backend = RsyncBackend::new(&runner);
    let mut opts = rsync_verify_opts();
    opts.verify_metamanifest = false;
    opts.uri = "rsync://rsync.gentoo.org/gentoo-portage".into();
    opts.rsync_opts_override = Some(vec!["--recursive".into(), "--times".into()]);
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    backend.fetch(&ctx).unwrap();
    let call = runner
        .calls()
        .into_iter()
        .find(|c| c.args.iter().any(|a| a == "--recursive"))
        .unwrap();
    assert!(call.args.iter().any(|a| a == "--compress"));
    assert!(call.args.iter().any(|a| a == "--whole-file"));
    assert!(call.args.iter().any(|a| a.starts_with("--timeout=")));
}

#[test]
fn git_volatile_non_shallow_fetches_full_history() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    std::fs::create_dir_all(loc.join(".git")).unwrap();
    let staging = tmp.path().join("staging/g");
    let runner = FakeRunner::new()
        .rule(|s| {
            (s.program == "git" && s.args.iter().any(|a| a == "--is-shallow-repository"))
                .then(|| ok("false\n"))
        })
        .rule(|s| (s.program == "git").then(|| ok("h")));
    let backend = GitBackend::new(&runner);
    let mut opts = git_opts(None);
    opts.volatile = true;
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    backend.update(&ctx).unwrap();
    let fetch = runner
        .calls()
        .into_iter()
        .find(|c| c.args.iter().any(|a| a == "fetch"))
        .unwrap();
    assert!(
        !fetch.args.iter().any(|a| a.starts_with("--depth")),
        "a volatile non-shallow repo fetches the full history"
    );
}

#[test]
fn git_env_applied_to_clone_and_fetch() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    std::fs::create_dir_all(loc.join(".git")).unwrap();
    let staging = tmp.path().join("staging/g");
    let runner = FakeRunner::new().rule(|s| (s.program == "git").then(|| ok("h")));
    let backend = GitBackend::new(&runner);
    let mut opts = git_opts(None);
    opts.git_env = vec![(
        "GIT_SSH_COMMAND".into(),
        "ssh -i /home/u/.ssh/overlay_key".into(),
    )];
    opts.git_clone_extra_opts = vec!["--filter=blob:none".into()];
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    backend.fetch(&ctx).unwrap();
    backend.update(&ctx).unwrap();
    let clone = runner
        .calls()
        .into_iter()
        .find(|c| c.args.iter().any(|a| a == "clone"))
        .unwrap();
    assert!(
        clone.args.iter().any(|a| a == "--filter=blob:none"),
        "sync-git-clone-extra-opts is appended to the clone"
    );
    assert!(
        clone
            .env
            .iter()
            .any(|(k, v)| k == "GIT_SSH_COMMAND" && v == "ssh -i /home/u/.ssh/overlay_key")
    );
    let fetch = runner
        .calls()
        .into_iter()
        .find(|c| c.args.iter().any(|a| a == "fetch"))
        .unwrap();
    assert!(
        fetch.env.iter().any(|(k, _)| k == "GIT_SSH_COMMAND"),
        "sync-git-env is applied to the fetch too"
    );
}

#[test]
fn master_cascade_runs_unchanged_dependent_hooks() {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = TempDir::new().unwrap();
    let master = make_repo(tmp.path(), "gentoo", "");
    let child = make_repo(tmp.path(), "overlay", "");
    // The dependent's local timestamp matches the probed server timestamp, so it
    // syncs unchanged.
    std::fs::write(
        child.join("metadata/timestamp.chk"),
        "Sun, 21 Jun 2026 05:45:00 +0000\n",
    )
    .unwrap();
    let set = discover_set(
        tmp.path(),
        &format!(
            "[gentoo]\nlocation = {}\nsync-type = webrsync\nsync-uri = https://m\n\
             [overlay]\nlocation = {}\nmasters = gentoo\nsync-type = rsync\nsync-uri = rsync://c\n",
            master.display(),
            child.display()
        ),
    );

    let hooks = tmp.path().join("config/etc/portage/repo.postsync.d");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("10-notify");
    std::fs::write(&hook, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let runner = FakeRunner::new()
        .rule(|s| (s.program == "emerge-webrsync").then(|| ok("")))
        .rule(|s| {
            if s.program == "rsync" && s.args.iter().any(|a| a.contains("timestamp.chk")) {
                if let Some(dst) = s.args.last() {
                    let p = Path::new(dst);
                    if let Some(parent) = p.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    std::fs::write(p, "Sun, 21 Jun 2026 05:45:00 +0000\n").ok();
                }
                Some(ok(""))
            } else {
                None
            }
        })
        .rule(|s| s.program.ends_with("10-notify").then(|| ok("")));
    let registry = BackendRegistry::new(vec![
        Box::new(WebrsyncBackend::new(&runner)),
        Box::new(RsyncBackend::new(&runner)),
    ]);
    let refresher = FakeRefresher::new();
    let staging = tmp.path().join("staging");
    // `sync-hooks-only-on-change` is on by default, so only the master cascade can
    // run the unchanged dependent's hooks.
    let engine = SyncEngine::new(&set, &registry, &refresher, &runner, &staging)
        .with_config_root(tmp.path().join("config"));

    let mut history = RevisionHistory::new();
    let report = engine.sync_all(&mut history);
    // The dependent synced unchanged.
    match report.get("overlay").unwrap() {
        RepoResult::Synced { outcome, .. } => assert!(!outcome.changed),
        other => panic!("expected unchanged overlay, got {other:?}"),
    }
    // The cascade still ran the dependent's hook.
    let ran_for_overlay = runner
        .calls()
        .into_iter()
        .filter(|c| c.program.ends_with("10-notify"))
        .any(|c| c.args.first().map(|a| a == "overlay").unwrap_or(false));
    assert!(
        ran_for_overlay,
        "a changed master must cascade its unchanged dependent's hooks"
    );
}

// --- corpus-gated live test -------------------------------------------------

#[test]
fn live_git_sync_against_corpus() {
    let Ok(corpus) = std::env::var("MORAINE_CORPUS") else {
        return; // No-op without a corpus.
    };
    let corpus = PathBuf::from(corpus);
    let conf = corpus.join("repos.conf");
    if !conf.exists() {
        return;
    }
    let set = discover(&conf).expect("discover corpus repos.conf");
    assert!(!set.is_empty());
}

// --- post-sync action -------------------------------------------------------

#[test]
fn failed_post_sync_action_reported_without_rollback() {
    let tmp = TempDir::new().unwrap();
    let loc = make_repo(tmp.path(), "g", "");
    let set = discover_set(
        tmp.path(),
        &format!(
            "[g]\nlocation = {}\nsync-type = webrsync\nsync-uri = https://x\npost-sync = false\n",
            loc.display()
        ),
    );
    let runner = FakeRunner::new()
        .rule(|s| (s.program == "emerge-webrsync").then(|| ok("")))
        .rule(|s| (s.program == "false").then(|| fail("boom")));
    let registry = BackendRegistry::new(vec![Box::new(WebrsyncBackend::new(&runner))]);
    let refresher = FakeRefresher::new();
    let staging = tmp.path().join("staging");
    let engine = SyncEngine::new(&set, &registry, &refresher, &runner, &staging)
        .with_extras(extras_for(tmp.path()));
    let mut history = RevisionHistory::new();
    let report = engine.sync_all(&mut history);
    assert!(matches!(
        report.get("g"),
        Some(RepoResult::Failed(SyncError::PostSyncAction { .. }))
    ));
    // Refresh still ran (metadata left in place).
    assert_eq!(refresher.calls(), vec![("g".to_owned(), false)]);
}

// --- sync-cli-wiring additions ----------------------------------------------

#[test]
fn default_rsync_timeout_is_180() {
    // Portage hardcodes the rsync transfer `--timeout=180`.
    assert_eq!(SyncDefaults::default().timeout_secs, 180);
}

#[test]
fn rsync_transfer_carries_timeout_and_no_git_exclude() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    let staging = tmp.path().join("staging/g");
    std::fs::create_dir_all(&staging).unwrap();
    let runner = FakeRunner::new().rule(|s| {
        (s.program == "rsync" && s.args.iter().any(|a| a == "--recursive")).then(|| ok(""))
    });
    let backend = RsyncBackend::new(&runner);
    let mut opts = rsync_verify_opts();
    opts.verify_metamanifest = false;
    opts.timeout_secs = 180;
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    backend.fetch(&ctx).unwrap();
    let call = runner
        .calls()
        .into_iter()
        .find(|c| c.args.iter().any(|a| a == "--recursive"))
        .unwrap();
    assert!(call.args.iter().any(|a| a == "--timeout=180"));
    assert!(!call.args.iter().any(|a| a == "--exclude=/.git"));
}

#[test]
fn rsync_opts_override_has_no_git_exclude() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    let staging = tmp.path().join("staging/g");
    std::fs::create_dir_all(&staging).unwrap();
    let runner = FakeRunner::new().rule(|s| {
        (s.program == "rsync" && s.args.iter().any(|a| a == "--archive")).then(|| ok(""))
    });
    let backend = RsyncBackend::new(&runner);
    let mut opts = rsync_verify_opts();
    opts.verify_metamanifest = false;
    opts.rsync_opts_override = Some(vec!["--archive".into()]);
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    backend.fetch(&ctx).unwrap();
    let call = runner
        .calls()
        .into_iter()
        .find(|c| c.args.iter().any(|a| a == "--archive"))
        .unwrap();
    assert!(!call.args.iter().any(|a| a == "--exclude=/.git"));
}

#[test]
fn freshness_orders_by_epoch() {
    use crate::backends::rsync::{Freshness, classify_freshness};
    assert_eq!(classify_freshness(100, Some(100)), Freshness::Current);
    assert_eq!(classify_freshness(100, Some(200)), Freshness::Older);
    assert_eq!(classify_freshness(200, Some(100)), Freshness::Newer);
    assert_eq!(classify_freshness(100, None), Freshness::Newer);
}

#[test]
fn global_postsync_runs_once_after_all_repos() {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = TempDir::new().unwrap();
    let a = make_repo(tmp.path(), "a", "");
    let b = make_repo(tmp.path(), "b", "");
    let set = discover_set(
        tmp.path(),
        &format!(
            "[a]\nlocation = {}\nsync-type = webrsync\nsync-uri = https://a\n\
             [b]\nlocation = {}\nsync-type = webrsync\nsync-uri = https://b\n",
            a.display(),
            b.display()
        ),
    );
    // Only the global postsync.d directory, no per-repo hooks.
    let hooks = tmp.path().join("config/etc/portage/postsync.d");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("99-global");
    std::fs::write(&hook, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let runner = FakeRunner::new()
        .rule(|s| (s.program == "emerge-webrsync").then(|| ok("")))
        .rule(|s| s.program.ends_with("99-global").then(|| ok("")));
    let registry = BackendRegistry::new(vec![Box::new(WebrsyncBackend::new(&runner))]);
    let refresher = FakeRefresher::new();
    let staging = tmp.path().join("staging");
    let engine = SyncEngine::new(&set, &registry, &refresher, &runner, &staging)
        .with_config_root(tmp.path().join("config"));

    let mut history = RevisionHistory::new();
    engine.sync_all(&mut history);
    let global_calls = runner
        .calls()
        .into_iter()
        .filter(|c| c.program.ends_with("99-global"))
        .count();
    assert_eq!(
        global_calls, 1,
        "the global postsync.d hook runs exactly once"
    );
}

/// Drive a git update with a rule that answers `git show <rev>:metadata/timestamp.chk`
/// with `head_ts`, the signature query `--pretty=%G?` with `sig`, and every other
/// git command with success. Returns the update result and the recorded calls.
fn git_update_with(
    opts: SyncOptions,
    head_ts: &'static str,
    sig: &'static str,
) -> (
    Result<SyncOutcome, SyncError>,
    Vec<crate::command::CommandSpec>,
) {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("g");
    std::fs::create_dir_all(loc.join(".git")).unwrap();
    let staging = tmp.path().join("staging/g");
    let runner = FakeRunner::new()
        .rule(move |s| {
            (s.program == "git" && s.args.iter().any(|a| a.contains("timestamp.chk")))
                .then(|| ok(head_ts))
        })
        .rule(move |s| {
            (s.program == "git" && s.args.iter().any(|a| a == "--pretty=%G?")).then(|| ok(sig))
        })
        .rule(|s| (s.program == "git").then(|| ok("h")));
    let backend = GitBackend::new(&runner);
    let ctx = SyncContext {
        repo: "g",
        location: &loc,
        staging: &staging,
        options: &opts,
    };
    let result = backend.update(&ctx);
    (result, runner.calls())
}

/// Whether any recorded call passed `arg`.
fn calls_have_arg(calls: &[crate::command::CommandSpec], arg: &str) -> bool {
    calls.iter().any(|c| c.args.iter().any(|a| a == arg))
}

/// Whether any recorded call passed an argument containing `needle`.
fn calls_have_arg_containing(calls: &[crate::command::CommandSpec], needle: &str) -> bool {
    calls
        .iter()
        .any(|c| c.args.iter().any(|a| a.contains(needle)))
}

#[test]
fn git_stale_head_rejected_by_max_age_alone() {
    let mut opts = git_opts(None);
    opts.git_verify_commit_signature = false;
    opts.git_verify_max_age_days = 1;
    let (result, calls) = git_update_with(opts, "Fri, 01 Jan 1971 00:00:00 +0000\n", "G");
    assert!(matches!(result, Err(SyncError::Verification { .. })));
    // Verification runs before the merge, so the live tree is never advanced.
    assert!(!calls_have_arg(&calls, "merge"));
}

#[test]
fn git_fresh_head_accepted_under_max_age() {
    let mut opts = git_opts(None);
    opts.git_verify_commit_signature = false;
    // A very large window accepts any plausible head date without a clock dependency.
    opts.git_verify_max_age_days = 3_650_000;
    let (result, _) = git_update_with(opts, "Sun, 21 Jun 2026 05:45:00 +0000\n", "G");
    assert!(result.is_ok());
}

#[test]
fn git_max_age_unset_skips_the_check() {
    let mut opts = git_opts(None);
    opts.git_verify_commit_signature = false;
    opts.git_verify_max_age_days = 0;
    // An ancient head is accepted because the max-age check does not run.
    let (result, calls) = git_update_with(opts, "Fri, 01 Jan 1971 00:00:00 +0000\n", "G");
    assert!(result.is_ok());
    assert!(!calls_have_arg_containing(&calls, "timestamp.chk"));
}

#[test]
fn git_bad_signature_rejected_before_merge() {
    let mut opts = git_opts(None);
    opts.git_verify_commit_signature = true;
    opts.git_verify_max_age_days = 0;
    // `N` is a missing signature.
    let (result, calls) = git_update_with(opts, "Sun, 21 Jun 2026 05:45:00 +0000\n", "N");
    assert!(matches!(result, Err(SyncError::Verification { .. })));
    assert!(!calls_have_arg(&calls, "merge"));
}

#[test]
fn live_rsync_timestamp_fast_path_and_auto_sync_skip() {
    // Opt-in (gated on MORAINE_CORPUS) end-to-end test against the real `rsync`
    // binary and a local `file://` source whose `metadata/timestamp.chk` holds a
    // real TIMESTAMP_FORMAT date. It asserts the no-change fast path on a second
    // run and that an `auto-sync = no` repository is skipped.
    if std::env::var_os("MORAINE_CORPUS").is_none() {
        return;
    }
    if which_rsync().is_none() {
        eprintln!("rsync not available; skipping live rsync test");
        return;
    }

    let tmp = TempDir::new().unwrap();
    // The upstream source tree with a real timestamp.chk date string.
    let source = tmp.path().join("source");
    std::fs::create_dir_all(source.join("metadata")).unwrap();
    std::fs::write(
        source.join("metadata/timestamp.chk"),
        "Sun, 21 Jun 2026 05:45:00 +0000\n",
    )
    .unwrap();
    std::fs::write(source.join("profiles_repo_name"), "g\n").unwrap();

    let local = tmp.path().join("local");
    let skipped_local = tmp.path().join("skipped");
    let body = format!(
        "[g]\nlocation = {}\nsync-type = rsync\nsync-uri = file://{}\n\
         [h]\nlocation = {}\nsync-type = rsync\nsync-uri = file://{}\nauto-sync = no\n",
        local.display(),
        source.display(),
        skipped_local.display(),
        source.display(),
    );
    let set = discover_set(tmp.path(), &body);
    let extras = extras_for(tmp.path());

    let runner = crate::command::SystemRunner;
    let registry = BackendRegistry::new(vec![Box::new(RsyncBackend::new(&runner))]);
    let refresher = FakeRefresher::new();
    let staging = tmp.path().join("staging");
    let engine = SyncEngine::new(&set, &registry, &refresher, &runner, &staging)
        .with_extras(extras)
        .with_metadata_transfer(false);

    let mut history = RevisionHistory::new();
    let first = engine.sync_all(&mut history);
    assert!(first.get("g").unwrap().is_synced(), "{first:?}");
    // `auto-sync = no` is honored: the second repository is skipped.
    assert!(matches!(first.get("h"), Some(RepoResult::Skipped)));
    assert!(local.join("metadata/timestamp.chk").exists());

    // Second run: the server and local timestamp.chk parse to equal epochs, so the
    // probe classifies the tree as current and transfers nothing.
    let second = engine.sync_all(&mut history);
    match second.get("g").unwrap() {
        RepoResult::Synced { outcome, .. } => {
            assert!(
                !outcome.changed,
                "second sync must hit the no-change fast path"
            );
        }
        other => panic!("expected a synced (unchanged) result, got {other:?}"),
    }
}

/// Whether a usable `rsync` binary is on PATH.
fn which_rsync() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join("rsync"))
        .find(|p| p.exists())
}
