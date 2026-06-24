//! Store round-trip, counter, and journal-versus-primary resolution tests.

use moraine_atom::{DepSpec, SlotOp};
use moraine_common::Interner;
use moraine_vdb::contents::{Contents, Entry, EntryKind};
use moraine_vdb::record::{Depend, DependKind, EnvironmentRef, PackageRecord, Slot};
use moraine_vdb::soname::{Provides, Requires, SonameEntry};
use moraine_vdb::store::{Store, StorePaths};

fn sample_record(interner: &Interner, version: &str) -> PackageRecord {
    let features = moraine_eapi::features_for("8");
    let rdepend_raw = "dev-libs/openssl:0/3=".to_string();
    let rdepend_ast = DepSpec::parse(&rdepend_raw, features, interner).unwrap();

    let depends = moraine_vdb::record::DependSet {
        rdepend: Some(Depend {
            raw: rdepend_raw,
            ast: rdepend_ast,
        }),
        ..Default::default()
    };

    let contents = Contents::from_entries([
        Entry {
            path: "/usr/bin/widget".to_string(),
            kind: EntryKind::Obj {
                md5: "d41d8cd98f00b204e9800998ecf8427e".to_string(),
                mtime: 1_700_000_000,
            },
        },
        Entry {
            path: "/usr/lib/libwidget.so".to_string(),
            kind: EntryKind::Sym {
                target: "libwidget.so.1".to_string(),
                mtime: 1_700_000_001,
            },
        },
    ]);

    PackageRecord {
        category: interner.intern("app-misc"),
        package: interner.intern("widget"),
        version: moraine_version::Version::parse(version).unwrap(),
        eapi: "8".to_string(),
        slot: Slot {
            slot: interner.intern("0"),
            subslot: Some(interner.intern("2")),
        },
        use_flags: vec![interner.intern("ssl"), interner.intern("zlib")],
        iuse: vec!["ssl".to_string(), "+zlib".to_string()],
        depends,
        keywords: vec!["amd64".to_string()],
        license: "GPL-2".to_string(),
        properties: String::new(),
        restrict: "test".to_string(),
        repository: Some(interner.intern("gentoo")),
        defined_phases: vec!["compile".to_string(), "install".to_string()],
        build_time: Some(1_700_000_100),
        build_id: Some(7),
        counter: 0,
        chost: "x86_64-pc-linux-gnu".to_string(),
        provides: Provides {
            entries: vec![SonameEntry {
                bucket: interner.intern("x86_64"),
                soname: interner.intern("libwidget.so.1"),
            }],
        },
        requires: Requires {
            entries: vec![SonameEntry {
                bucket: interner.intern("x86_64"),
                soname: interner.intern("libc.so.6"),
            }],
        },
        contents,
        environment: Some(EnvironmentRef {
            digest: "abc".to_string(),
            blob: vec![1, 2, 3, 4],
        }),
    }
}

#[test]
fn record_round_trips_through_primary_file() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path());

    let mut store = Store::empty(paths.clone());
    let interner = store.interner().clone();
    let record = sample_record(&interner, "1.2.3");
    store.add(record).unwrap();
    store.compact().unwrap();

    let loaded = Store::load(paths).unwrap();
    assert_eq!(loaded.records().len(), 1);
    let rec = &loaded.records()[0];
    let li = loaded.interner();

    assert_eq!(li.resolve(rec.category).as_deref(), Some("app-misc"));
    assert_eq!(li.resolve(rec.package).as_deref(), Some("widget"));
    assert_eq!(rec.version.as_str(), "1.2.3");
    assert_eq!(rec.eapi, "8");
    assert_eq!(li.resolve(rec.slot.slot).as_deref(), Some("0"));
    assert_eq!(li.resolve(rec.slot.subslot.unwrap()).as_deref(), Some("2"));
    assert_eq!(rec.license, "GPL-2");
    assert_eq!(rec.restrict, "test");
    assert_eq!(rec.build_id, Some(7));
    assert_eq!(rec.chost, "x86_64-pc-linux-gnu");

    // Dependency string and AST both preserved.
    let rdep = rec.depends.get(DependKind::RDepend).unwrap();
    assert_eq!(rdep.raw, "dev-libs/openssl:0/3=");

    // Soname linkage preserved.
    let soname = li.intern("libwidget.so.1");
    assert!(rec.provides.provides(soname));
    let libc = li.intern("libc.so.6");
    assert!(rec.requires.sonames().any(|s| s == libc));

    // CONTENTS preserved with implicit parents.
    assert!(rec.contents.owns("/usr/bin/widget"));
    assert!(matches!(rec.contents.owner("/usr"), Some(EntryKind::Dir)));

    // Environment reference preserved.
    let env = rec.environment.as_ref().unwrap();
    assert_eq!(env.blob, vec![1, 2, 3, 4]);
}

#[test]
fn slot_operator_binding_survives_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path());

    let mut store = Store::empty(paths.clone());
    let interner = store.interner().clone();
    let record = sample_record(&interner, "1.0");
    store.add(record).unwrap();
    store.compact().unwrap();

    let loaded = Store::load(paths).unwrap();
    let rec = &loaded.records()[0];
    let li = loaded.interner();

    let bindings = loaded.slot_operator_bindings(rec);
    assert_eq!(bindings.len(), 1);
    let b = bindings[0];
    assert_eq!(li.resolve(b.slot.unwrap()).as_deref(), Some("0"));
    assert_eq!(li.resolve(b.subslot.unwrap()).as_deref(), Some("3"));

    // The atom kept the := operator, not collapsed to plain slot or :*.
    let rdep = rec.depends.get(DependKind::RDepend).unwrap();
    let atom = rdep.ast.atoms()[0];
    assert_eq!(atom.slot_op(), Some(SlotOp::Equal));
}

#[test]
fn counter_increases_on_every_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path());

    let mut store = Store::empty(paths);
    let interner = store.interner().clone();

    store.add(sample_record(&interner, "1.0")).unwrap();
    let first = store.counter();
    store.add(sample_record(&interner, "2.0")).unwrap();
    let second = store.counter();

    assert!(second > first);
    assert_ne!(store.records()[0].counter, store.records()[1].counter);
}

#[test]
fn journal_highest_counter_wins() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path());

    // Write a primary file with version 1.0 at a low counter.
    let mut store = Store::empty(paths.clone());
    let interner = store.interner().clone();
    store.add(sample_record(&interner, "1.0")).unwrap();
    store.compact().unwrap();

    // Append a journal record replacing the same package with a newer license.
    let mut store = Store::load(paths.clone()).unwrap();
    let li = store.interner().clone();
    let mut replacement = sample_record(&li, "1.0");
    replacement.license = "MIT".to_string();
    store.add(replacement).unwrap();

    // Reload: journal record (higher counter) must win.
    let loaded = Store::load(paths).unwrap();
    assert_eq!(loaded.records().len(), 1);
    assert_eq!(loaded.records()[0].license, "MIT");
}

#[test]
fn journal_remove_takes_effect_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path());

    let mut store = Store::empty(paths.clone());
    let interner = store.interner().clone();
    store.add(sample_record(&interner, "1.0")).unwrap();
    store.compact().unwrap();

    let mut store = Store::load(paths.clone()).unwrap();
    let li = store.interner().clone();
    let cat = li.intern("app-misc");
    let pkg = li.intern("widget");
    assert!(store.remove(cat, pkg, "1.0").unwrap());

    let loaded = Store::load(paths).unwrap();
    assert!(loaded.records().is_empty());
}

#[test]
fn partial_trailing_journal_record_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path());

    let mut store = Store::empty(paths.clone());
    let interner = store.interner().clone();
    store.add(sample_record(&interner, "1.0")).unwrap();
    store.compact().unwrap();

    let mut store = Store::load(paths.clone()).unwrap();
    let li = store.interner().clone();
    let mut second = sample_record(&li, "2.0");
    second.license = "MIT".to_string();
    store.add(second).unwrap();

    // Corrupt the journal by truncating its trailing bytes.
    let mut bytes = std::fs::read(&paths.journal).unwrap();
    bytes.truncate(bytes.len() - 3);
    std::fs::write(&paths.journal, &bytes).unwrap();

    // The intact primary record survives; the partial journal frame is dropped.
    let loaded = Store::load(paths).unwrap();
    assert_eq!(loaded.records().len(), 1);
    assert_eq!(loaded.records()[0].version.as_str(), "1.0");
}

#[test]
fn single_add_does_not_rewrite_primary() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path());

    let mut store = Store::empty(paths.clone());
    let interner = store.interner().clone();
    store.add(sample_record(&interner, "1.0")).unwrap();
    store.compact().unwrap();

    let primary_before = std::fs::metadata(&paths.primary).unwrap().len();

    let mut store = Store::load(paths.clone()).unwrap();
    let li = store.interner().clone();
    store.add(sample_record(&li, "2.0")).unwrap();

    // Primary file untouched; the change went to the journal.
    let primary_after = std::fs::metadata(&paths.primary).unwrap().len();
    assert_eq!(primary_before, primary_after);
    assert!(std::fs::metadata(&paths.journal).unwrap().len() > 0);
}

#[test]
fn match_atom_honors_version_and_slot() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path());

    let mut store = Store::empty(paths);
    let interner = store.interner().clone();
    store.add(sample_record(&interner, "1.0")).unwrap();
    store.add(sample_record(&interner, "2.0")).unwrap();

    let features = moraine_eapi::features_for("8");
    let li = store.interner();

    let atom = moraine_atom::Atom::parse(">=app-misc/widget-2.0", features, li).unwrap();
    let matches = store.match_atom(&atom);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].version.as_str(), "2.0");

    let slot_atom = moraine_atom::Atom::parse("app-misc/widget:0", features, li).unwrap();
    assert_eq!(store.match_atom(&slot_atom).len(), 2);

    let wrong_slot = moraine_atom::Atom::parse("app-misc/widget:9", features, li).unwrap();
    assert_eq!(store.match_atom(&wrong_slot).len(), 0);
}
