//! End-to-end incremental reimport test: unchanged entries are reused and only
//! changed entries are re-read from the source cache.

use std::fs;
use std::path::Path;

use moraine_repo::{discover, import_repo, previous_index, store};
use tempfile::TempDir;

fn write_cache(loc: &Path, cat: &str, pv: &str, body: &str) {
    let dir = loc.join("metadata/md5-cache").join(cat);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(pv), body).unwrap();
}

#[test]
fn unchanged_reused_and_changed_reparsed() {
    let tmp = TempDir::new().unwrap();
    let loc = tmp.path().join("gentoo");
    fs::create_dir_all(loc.join("profiles")).unwrap();
    fs::write(loc.join("profiles/repo_name"), "gentoo\n").unwrap();

    // Two entries: `stable` and `changing`.
    write_cache(
        &loc,
        "dev-libs",
        "stable-1.0",
        "EAPI=8\nSLOT=0\nRDEPEND=dev-libs/a\n_mtime_=100\n_md5_=stablehash\n",
    );
    write_cache(
        &loc,
        "dev-libs",
        "changing-1.0",
        "EAPI=8\nSLOT=0\nRDEPEND=dev-libs/old\n_mtime_=100\n_md5_=oldhash\n",
    );

    let conf = tmp.path().join("repos.conf");
    fs::write(&conf, format!("[gentoo]\nlocation = {}\n", loc.display())).unwrap();
    let store_path = tmp.path().join("gentoo.mrepo");

    // Cold import and persist.
    let set = discover(&conf).unwrap();
    let first = import_repo(&set, "gentoo", &std::collections::HashMap::new()).unwrap();
    assert_eq!(first.entries.len(), 2);
    store::write_store(&store_path, first.entries).unwrap();

    // Mutate `changing` (new mtime/md5 and new RDEPEND); leave `stable` alone.
    write_cache(
        &loc,
        "dev-libs",
        "changing-1.0",
        "EAPI=8\nSLOT=0\nRDEPEND=dev-libs/new\n_mtime_=200\n_md5_=newhash\n",
    );

    // Reimport incrementally from the persisted entries.
    let prev_entries = store::read_entries(&store_path).unwrap();
    let prev = previous_index(&prev_entries);
    let second = import_repo(&set, "gentoo", &prev).unwrap();

    let stable = second
        .entries
        .iter()
        .find(|e| e.package == "stable")
        .unwrap();
    let changing = second
        .entries
        .iter()
        .find(|e| e.package == "changing")
        .unwrap();

    // Unchanged entry reused (same mtime/md5 carried through).
    assert_eq!(stable.mtime, "100");
    assert_eq!(stable.md5, "stablehash");
    assert_eq!(stable.rdepend, "dev-libs/a");

    // Changed entry re-read with new content.
    assert_eq!(changing.mtime, "200");
    assert_eq!(changing.md5, "newhash");
    assert_eq!(changing.rdepend, "dev-libs/new");
}
