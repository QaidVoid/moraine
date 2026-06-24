//! IPC `has_version`/`best_version` answered from a fixture installed store
//! backed by `moraine-repo`'s query API.
//!
//! This wires a real [`moraine_repo::RepoIndex`] (built from in-memory stored
//! entries) behind the [`moraine_build::VersionQuery`] trait and drives the IPC
//! handler over it, covering the repo-backed query path end to end.

use moraine_build::{IpcHandler, QueryRoot, VersionQuery};
use moraine_repo::store::{LoadedStore, StoredEntry};
use moraine_repo::{RepoIndex, RepoStore};

fn entry(category: &str, package: &str, version: &str) -> StoredEntry {
    StoredEntry {
        category: category.into(),
        package: package.into(),
        version: version.into(),
        repository: "installed".into(),
        eapi: "8".into(),
        slot: "0".into(),
        subslot: None,
        depend: String::new(),
        rdepend: String::new(),
        bdepend: String::new(),
        pdepend: String::new(),
        idepend: String::new(),
        required_use: String::new(),
        src_uri: String::new(),
        license: String::new(),
        keywords: vec![],
        iuse: vec![],
        properties: vec![],
        restrict: vec![],
        defined_phases: vec![],
        inherit: vec![],
        inherited: vec![],
        mtime: String::new(),
        md5: String::new(),
    }
}

/// A `VersionQuery` answered from a `RepoIndex` standing in for the installed
/// store. The root selector is ignored here since the fixture has one store, but
/// the adapter shows the seam where `ROOT`/`ESYSROOT`/`BROOT` would select stores.
struct StoreQuery {
    index: RepoIndex,
}

impl VersionQuery for StoreQuery {
    fn has_version(&self, _root: QueryRoot, atom: &str) -> bool {
        !self.index.match_atom_str(atom).is_empty()
    }

    fn best_version(&self, _root: QueryRoot, atom: &str) -> Option<String> {
        let candidates = self.index.match_atom_str(atom);
        // match_atom_str returns ascending version order; the best is last.
        candidates.last().map(|c| {
            let store = &self.index.repos()[c.repo_order].store;
            let interner = store.interner();
            let cat = interner
                .resolve(c.entry.category)
                .map(|s| s.to_string())
                .unwrap_or_default();
            let pkg = interner
                .resolve(c.entry.package)
                .map(|s| s.to_string())
                .unwrap_or_default();
            format!("{cat}/{pkg}-{}", c.entry.version.as_str())
        })
    }
}

fn handler_fixture() -> StoreQuery {
    let store = LoadedStore::from_entries(vec![
        entry("dev-libs", "foo", "1.0"),
        entry("dev-libs", "foo", "2.0"),
        entry("dev-libs", "bar", "3.1"),
    ])
    .unwrap();
    let index = RepoIndex::new(vec![RepoStore {
        name: "installed".into(),
        store,
    }]);
    StoreQuery { index }
}

#[test]
fn has_version_true_for_installed() {
    let backend = handler_fixture();
    let handler = IpcHandler::new(&backend);
    let r = handler
        .handle_line("has_version host dev-libs/foo")
        .unwrap();
    assert_eq!(r.code, 0);
}

#[test]
fn has_version_false_for_absent() {
    let backend = handler_fixture();
    let handler = IpcHandler::new(&backend);
    let r = handler
        .handle_line("has_version host dev-libs/absent")
        .unwrap();
    assert_eq!(r.code, 1);
}

#[test]
fn best_version_returns_highest() {
    let backend = handler_fixture();
    let handler = IpcHandler::new(&backend);
    let r = handler
        .handle_line("best_version host dev-libs/foo")
        .unwrap();
    assert_eq!(r.code, 0);
    assert_eq!(r.value.as_deref(), Some("dev-libs/foo-2.0"));
}

#[test]
fn best_version_with_version_constraint() {
    let backend = handler_fixture();
    let handler = IpcHandler::new(&backend);
    let r = handler
        .handle_line("best_version host <dev-libs/foo-2.0")
        .unwrap();
    assert_eq!(r.code, 0);
    assert_eq!(r.value.as_deref(), Some("dev-libs/foo-1.0"));
}
