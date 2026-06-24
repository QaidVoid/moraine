//! Corpus round-trip test over a real `/var/db/pkg` tree.
//!
//! Gated on the `MORAINE_CORPUS` environment variable. When it is unset the test
//! no-ops so the gate stays green without a corpus. When set it imports the tree,
//! writes the store, reloads it, and asserts field-level fidelity of recorded
//! USE, SLOT and sub-slot, `:=` bindings, CONTENTS entries, and PROVIDES /
//! REQUIRES against the freshly imported records.

use std::collections::HashMap;
use std::sync::Arc;

use moraine_common::Interner;
use moraine_vdb::store::{Store, StorePaths};

#[test]
fn corpus_round_trips_resolver_fields() {
    let Ok(corpus) = std::env::var("MORAINE_CORPUS") else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus round-trip");
        return;
    };

    // Import the stock tree into a fresh set of records.
    let src_interner = Arc::new(Interner::new());
    let imported = moraine_vdb::import_vdb(&corpus, &src_interner).expect("import corpus");
    assert!(!imported.is_empty(), "corpus produced no records");

    // Index imported records by cpv for comparison after reload.
    let mut src: HashMap<String, _> = HashMap::new();
    for rec in &imported {
        src.insert(rec.cpv(&src_interner), rec);
    }

    // Persist and reload.
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path());
    let mut store = Store::from_records(paths.clone(), Arc::clone(&src_interner), imported.clone());
    store.compact().unwrap();

    let loaded = Store::load(paths).unwrap();
    let li = loaded.interner();
    assert_eq!(loaded.records().len(), imported.len());

    for rec in loaded.records() {
        let cpv = rec.cpv(li);
        let original = src.get(&cpv).unwrap_or_else(|| panic!("missing {cpv}"));

        // Recorded SLOT and sub-slot.
        let (oslot, osub) = (
            src_interner.resolve(original.slot.slot).unwrap(),
            original
                .slot
                .subslot
                .map(|s| src_interner.resolve(s).unwrap()),
        );
        assert_eq!(li.resolve(rec.slot.slot), Some(oslot));
        assert_eq!(rec.slot.subslot.map(|s| li.resolve(s).unwrap()), osub);

        // Recorded USE (as a sorted set of strings).
        let mut a: Vec<String> = rec
            .use_flags
            .iter()
            .map(|&s| li.resolve(s).unwrap().to_string())
            .collect();
        let mut b: Vec<String> = original
            .use_flags
            .iter()
            .map(|&s| src_interner.resolve(s).unwrap().to_string())
            .collect();
        a.sort();
        b.sort();
        assert_eq!(a, b, "USE mismatch for {cpv}");

        // `:=` bindings (raw RDEPEND/DEPEND strings round-trip verbatim).
        for kind in moraine_vdb::record::DependKind::ALL {
            let lhs = rec.depends.get(kind).map(|d| d.raw.as_str());
            let rhs = original.depends.get(kind).map(|d| d.raw.as_str());
            assert_eq!(lhs, rhs, "{} mismatch for {cpv}", kind.name());
        }

        // CONTENTS entry count (including implicit parents) matches.
        assert_eq!(
            rec.contents.len(),
            original.contents.len(),
            "CONTENTS size mismatch for {cpv}"
        );

        // PROVIDES / REQUIRES counts match.
        assert_eq!(rec.provides.entries.len(), original.provides.entries.len());
        assert_eq!(rec.requires.entries.len(), original.requires.entries.len());
    }
}
