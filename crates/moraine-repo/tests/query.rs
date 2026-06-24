//! End-to-end query API tests over a built index.

use std::fs;
use std::path::Path;

use moraine_repo::build_index;
use tempfile::TempDir;

/// Build a repository tree with md5-cache entries and return its `repos.conf`.
fn make_tree(root: &Path, repo: &str, entries: &[(&str, &str, &str)]) {
    let loc = root.join(repo);
    fs::create_dir_all(loc.join("profiles")).unwrap();
    fs::write(loc.join("profiles/repo_name"), format!("{repo}\n")).unwrap();
    for (cat, pv, body) in entries {
        let dir = loc.join("metadata/md5-cache").join(cat);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(pv), body).unwrap();
    }
}

fn cache_body(eapi: &str, slot: &str, rdepend: &str) -> String {
    format!("EAPI={eapi}\nSLOT={slot}\nRDEPEND={rdepend}\nKEYWORDS=amd64\n_mtime_=1\n_md5_=x\n")
}

#[test]
fn version_constrained_match() {
    let tmp = TempDir::new().unwrap();
    make_tree(
        tmp.path(),
        "gentoo",
        &[
            ("dev-libs", "openssl-1.0", &cache_body("8", "0", "")),
            ("dev-libs", "openssl-3.0", &cache_body("8", "0", "")),
            ("dev-libs", "openssl-3.2", &cache_body("8", "0", "")),
        ],
    );
    let conf = tmp.path().join("repos.conf");
    fs::write(
        &conf,
        format!(
            "[gentoo]\nlocation = {}\n",
            tmp.path().join("gentoo").display()
        ),
    )
    .unwrap();

    let index = build_index(&conf, tmp.path().join("store")).unwrap();
    let cands = index.match_atom_str(">=dev-libs/openssl-3.0");
    let mut versions: Vec<&str> = cands.iter().map(|c| c.entry.version.as_str()).collect();
    versions.sort();
    assert_eq!(versions, vec!["3.0", "3.2"]);
    // Each candidate carries its repository.
    assert!(cands.iter().all(|c| c.repo == "gentoo"));
}

#[test]
fn slot_constrained_match() {
    let tmp = TempDir::new().unwrap();
    make_tree(
        tmp.path(),
        "gentoo",
        &[
            ("dev-lang", "python-3.11", &cache_body("8", "3.11", "")),
            ("dev-lang", "python-3.12", &cache_body("8", "3.12", "")),
        ],
    );
    let conf = tmp.path().join("repos.conf");
    fs::write(
        &conf,
        format!(
            "[gentoo]\nlocation = {}\n",
            tmp.path().join("gentoo").display()
        ),
    )
    .unwrap();

    let index = build_index(&conf, tmp.path().join("store")).unwrap();
    let cands = index.match_atom_str("dev-lang/python:3.12");
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].entry.version.as_str(), "3.12");
}

#[test]
fn multi_repo_ordering_and_tagging() {
    let tmp = TempDir::new().unwrap();
    make_tree(
        tmp.path(),
        "gentoo",
        &[("dev-libs", "foo-1.0", &cache_body("8", "0", ""))],
    );
    make_tree(
        tmp.path(),
        "overlay",
        &[("dev-libs", "foo-1.0", &cache_body("8", "0", ""))],
    );
    let conf = tmp.path().join("repos.conf");
    fs::write(
        &conf,
        format!(
            "[gentoo]\nlocation = {}\n[overlay]\nlocation = {}\nmasters = gentoo\n",
            tmp.path().join("gentoo").display(),
            tmp.path().join("overlay").display()
        ),
    )
    .unwrap();

    let index = build_index(&conf, tmp.path().join("store")).unwrap();
    let cands = index.match_atom_str("dev-libs/foo");
    assert_eq!(cands.len(), 2);
    // gentoo is the master, so it is searched first.
    assert_eq!(cands[0].repo, "gentoo");
    assert_eq!(cands[1].repo, "overlay");
    assert!(cands[0].repo_order < cands[1].repo_order);
}

#[test]
fn metadata_fetch_returns_pre_parsed_asts() {
    let tmp = TempDir::new().unwrap();
    make_tree(
        tmp.path(),
        "gentoo",
        &[(
            "dev-libs",
            "bar-1.0",
            &cache_body("8", "0", "|| ( dev-libs/a dev-libs/b )"),
        )],
    );
    let conf = tmp.path().join("repos.conf");
    fs::write(
        &conf,
        format!(
            "[gentoo]\nlocation = {}\n",
            tmp.path().join("gentoo").display()
        ),
    )
    .unwrap();

    let index = build_index(&conf, tmp.path().join("store")).unwrap();
    let cands = index.match_atom_str("dev-libs/bar");
    assert_eq!(cands.len(), 1);
    // The dependency is already a parsed AST with two atoms.
    assert_eq!(cands[0].entry.rdepend.atoms().len(), 2);
}

#[test]
fn concurrent_reads_are_safe() {
    let tmp = TempDir::new().unwrap();
    make_tree(
        tmp.path(),
        "gentoo",
        &[
            ("dev-libs", "x-1.0", &cache_body("8", "0", "")),
            ("dev-libs", "x-2.0", &cache_body("8", "0", "")),
        ],
    );
    let conf = tmp.path().join("repos.conf");
    fs::write(
        &conf,
        format!(
            "[gentoo]\nlocation = {}\n",
            tmp.path().join("gentoo").display()
        ),
    )
    .unwrap();

    let index = build_index(&conf, tmp.path().join("store")).unwrap();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                let cands = index.match_atom_str(">=dev-libs/x-1.0");
                assert_eq!(cands.len(), 2);
            });
        }
    });
}
