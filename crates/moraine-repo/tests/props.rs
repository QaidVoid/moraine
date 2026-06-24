//! Property test: imported dependency ASTs re-serialize to the same atoms and
//! grouping as the source dependency string.

use moraine_atom::{Atom, DepSpec};
use moraine_common::Interner;
use moraine_eapi::features_for_level;
use moraine_repo::{LoadedStore, StoredEntry};
use proptest::prelude::*;

/// Render every atom in a DepSpec to its canonical string, in depth-first order.
fn rendered_atoms(spec: &DepSpec, interner: &Interner) -> Vec<String> {
    spec.atoms().iter().map(|a| a.render(interner)).collect()
}

/// A strategy producing a single valid atom string under EAPI 8.
fn atom_strategy() -> impl Strategy<Value = String> {
    let cat = prop::sample::select(vec!["dev-libs", "dev-lang", "sys-apps", "app-misc"]);
    let pkg = prop::sample::select(vec!["openssl", "python", "portage", "zlib", "foo"]);
    let op = prop::sample::select(vec!["", ">=", "<=", "~", "="]);
    let ver = prop::sample::select(vec!["1.0", "2.3.4", "3.0-r1"]);
    let slot = prop::sample::select(vec!["", ":0", ":2/2.1"]);
    (op, cat, pkg, ver, slot).prop_map(|(op, cat, pkg, ver, slot)| {
        if op.is_empty() {
            format!("{cat}/{pkg}{slot}")
        } else {
            format!("{op}{cat}/{pkg}-{ver}{slot}")
        }
    })
}

/// A strategy producing a dependency string from one to four atoms, optionally
/// wrapped in `|| ( ... )` and conditional groups.
fn dep_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(atom_strategy(), 1..4).prop_map(|atoms| {
        if atoms.len() >= 2 {
            let (head, tail) = atoms.split_at(1);
            format!("{} || ( {} )", head[0], tail.join(" "))
        } else {
            atoms.join(" ")
        }
    })
}

fn entry_with_rdepend(rdepend: &str) -> StoredEntry {
    StoredEntry {
        category: "dev-libs".to_owned(),
        package: "probe".to_owned(),
        version: "1.0".to_owned(),
        repository: "gentoo".to_owned(),
        eapi: "8".to_owned(),
        slot: "0".to_owned(),
        subslot: None,
        depend: String::new(),
        rdepend: rdepend.to_owned(),
        bdepend: String::new(),
        pdepend: String::new(),
        idepend: String::new(),
        required_use: String::new(),
        keywords: vec![],
        iuse: vec![],
        properties: vec![],
        restrict: vec![],
        defined_phases: vec![],
        inherit: vec![],
        mtime: "1".to_owned(),
        md5: "x".to_owned(),
    }
}

proptest! {
    #[test]
    fn imported_ast_reserializes_to_same_atoms(dep in dep_strategy()) {
        // The reference parse, against a throwaway interner.
        let reference = Interner::new();
        let parsed = DepSpec::parse(&dep, features_for_level(8), &reference).unwrap();
        let expected = rendered_atoms(&parsed, &reference);

        // Round-trip through the store: build entries, load, parse ASTs.
        let store = LoadedStore::from_entries(vec![entry_with_rdepend(&dep)]).unwrap();
        let entry = &store.entries()[0];
        let actual = rendered_atoms(&entry.rdepend, store.interner());

        prop_assert_eq!(expected, actual);
    }

    #[test]
    fn single_atom_roundtrips(atom in atom_strategy()) {
        let i1 = Interner::new();
        let a1 = Atom::parse(&atom, features_for_level(8), &i1).unwrap();
        let store = LoadedStore::from_entries(vec![entry_with_rdepend(&atom)]).unwrap();
        let entry = &store.entries()[0];
        let rendered: Vec<String> = entry
            .rdepend
            .atoms()
            .iter()
            .map(|a| a.render(store.interner()))
            .collect();
        prop_assert_eq!(rendered, vec![a1.render(&i1)]);
    }
}
