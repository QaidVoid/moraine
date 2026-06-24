//! Corpus comparison tests, gated on the `MORAINE_CORPUS` environment variable.
//!
//! `MORAINE_CORPUS` must point at a directory containing a `repos.conf` (file or
//! directory) describing the repositories to import. When the variable is unset
//! the tests are no-ops so they pass in environments without a real Gentoo tree.
//!
//! When `portageq` is also on `PATH` and `MORAINE_CORPUS_PORTAGE=1` is set, the
//! resolved repository order is compared against Portage's `get_repos` output.

use std::path::PathBuf;
use std::process::Command;

use moraine_repo::{build_index, discover};

fn corpus_dir() -> Option<PathBuf> {
    std::env::var_os("MORAINE_CORPUS").map(PathBuf::from)
}

#[test]
fn repo_order_resolves_on_corpus() {
    let Some(corpus) = corpus_dir() else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus test");
        return;
    };
    let repos_conf = corpus.join("repos.conf");
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
    let Some(corpus) = corpus_dir() else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus test");
        return;
    };
    let repos_conf = corpus.join("repos.conf");
    let store_dir = corpus.join(".moraine-store");
    let index = build_index(&repos_conf, &store_dir).expect("build index on corpus");
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
