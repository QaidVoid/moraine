//! Corpus comparison tests, gated on the `MORAINE_CORPUS` environment variable.
//!
//! `MORAINE_CORPUS` points at a captured system root (`EROOT`); the repository
//! configuration is read from its `etc/portage/repos.conf` (a file or a
//! `repos.conf` directory). When the variable is unset or that path is absent
//! the tests are no-ops so they pass in environments without a real Gentoo tree.
//!
//! When `portageq` is also on `PATH` and `MORAINE_CORPUS_PORTAGE=1` is set, the
//! resolved repository order is compared against Portage's `get_repos` output.

use std::path::PathBuf;
use std::process::Command;

use moraine_repo::{build_index, discover, import_repo};

/// The corpus `repos.conf` path, when a corpus is configured and it exists.
fn corpus_repos_conf() -> Option<PathBuf> {
    let root = std::env::var_os("MORAINE_CORPUS").filter(|v| !v.is_empty())?;
    let repos_conf = PathBuf::from(root).join("etc/portage/repos.conf");
    repos_conf.exists().then_some(repos_conf)
}

#[test]
fn repo_order_resolves_on_corpus() {
    let Some(repos_conf) = corpus_repos_conf() else {
        eprintln!("MORAINE_CORPUS unset or no etc/portage/repos.conf; skipping");
        return;
    };
    let set = discover(&repos_conf).expect("discover repos.conf");
    assert!(
        !set.is_empty(),
        "corpus must define at least one repository"
    );

    // Every master must precede its inheritors in the resolved order.
    let order = set.order();
    for cfg in set.ordered() {
        let self_pos = order.iter().position(|n| n == &cfg.name).unwrap();
        for master in &cfg.masters {
            let master_pos = order.iter().position(|n| n == master).unwrap();
            assert!(
                master_pos < self_pos,
                "master {master} must precede {}",
                cfg.name
            );
        }
    }

    // Optionally compare against Portage's resolved repository order.
    if std::env::var_os("MORAINE_CORPUS_PORTAGE").is_some()
        && let Some(portage_order) = portage_repo_order()
    {
        // Portage lists repos lowest-priority-first in `get_repos`; our order is
        // masters-first/highest-priority-first. Compare as sets to confirm the
        // same repositories were discovered, and confirm relative master order.
        let ours: std::collections::BTreeSet<&str> = order.iter().map(String::as_str).collect();
        let theirs: std::collections::BTreeSet<&str> =
            portage_order.iter().map(String::as_str).collect();
        assert_eq!(ours, theirs, "discovered repository set must match Portage");
    }
}

#[test]
fn build_and_query_on_corpus() {
    let Some(repos_conf) = corpus_repos_conf() else {
        eprintln!("MORAINE_CORPUS unset or no etc/portage/repos.conf; skipping");
        return;
    };
    // Build the store into a temporary directory rather than polluting the
    // corpus tree.
    let store = tempfile::tempdir().expect("temp store dir");
    let index = build_index(&repos_conf, store.path()).expect("build index on corpus");
    assert!(!index.repos().is_empty());

    // A broadly present package should resolve to at least one candidate.
    let cands = index.match_atom_str("sys-apps/portage");
    assert!(
        !cands.is_empty(),
        "expected at least one sys-apps/portage candidate on the corpus"
    );
    // Every candidate carries a repository tag and pre-parsed metadata.
    for c in &cands {
        assert!(!c.repo.is_empty());
        let _ = c.entry.rdepend.atoms();
    }
}

#[test]
fn pms_cache_import_reads_correct_auxdbkey_order() {
    let Some(repos_conf) = corpus_repos_conf() else {
        eprintln!("MORAINE_CORPUS unset or no etc/portage/repos.conf; skipping");
        return;
    };
    let set = discover(&repos_conf).expect("discover repos.conf");
    // A repository shipping a pms `metadata/cache` and no `metadata/md5-cache`
    // selects the positional flat_list reader. Real corpora usually ship
    // md5-cache, so skip cleanly when none is present.
    let Some(cfg) = set
        .ordered()
        .find(|c| c.pms_cache_dir().is_dir() && !c.md5_cache_dir().is_dir())
    else {
        eprintln!("no cache-formats=pms repository on the corpus; skipping");
        return;
    };
    let report =
        import_repo(&set, &cfg.name, &std::collections::HashMap::new()).expect("import pms repo");
    assert!(
        !report.entries.is_empty(),
        "pms repo must import at least one entry"
    );
    for e in &report.entries {
        // EAPI read from the corrected slot (index 15) is a small numeric level,
        // not a dependency string mis-read from an adjacent slot.
        assert!(
            e.eapi.parse::<u8>().is_ok(),
            "EAPI must be numeric, got {:?} for {}/{}-{}",
            e.eapi,
            e.category,
            e.package,
            e.version
        );
    }
}

/// Query Portage for its resolved repository names, if `portageq` is available.
fn portage_repo_order() -> Option<Vec<String>> {
    let output = Command::new("portageq")
        .arg("get_repos")
        .arg("/")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.split_whitespace().map(str::to_owned).collect())
}
