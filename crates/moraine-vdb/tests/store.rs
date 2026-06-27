//! Store round-trip, counter, and journal-versus-primary resolution tests.

use moraine_atom::{DepSpec, SlotOp};
use moraine_common::Interner;
use moraine_vdb::contents::{Contents, Entry, EntryKind};
use moraine_vdb::record::{Depend, DependKind, EnvironmentRef, PackageRecord, Slot};
use moraine_vdb::soname::{Provides, Requires, SonameEntry};
use moraine_vdb::store::{Store, StorePaths};

/// Add `record` stamped with the next counter value, mirroring how the merge
/// engine stamps a record exactly once before handing it to the store.
fn add_stamped(store: &mut Store, mut record: PackageRecord) {
    record.counter = store.counter() + 1;
    store.add(record).unwrap();
}

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
        description: "A sample widget".to_string(),
        homepage: "https://example.org".to_string(),
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
        inherited: vec!["eutils".to_string(), "toolchain-funcs".to_string()],
        features: vec!["userfetch".to_string()],
        size: Some(4096),
        needed: vec!["x86_64;/usr/lib/libwidget.so.1;libwidget.so.1;;libc.so.6".to_string()],
        toolchain: moraine_vdb::record::Toolchain {
            cflags: "-O2 -pipe".to_string(),
            ..Default::default()
        },
        dbdir_mtime: 0,
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

    // New aux fields preserved across the wire format.
    assert_eq!(rec.inherited, vec!["eutils", "toolchain-funcs"]);
    assert_eq!(rec.features, vec!["userfetch"]);
    assert_eq!(rec.size, Some(4096));
    assert_eq!(
        rec.needed,
        vec!["x86_64;/usr/lib/libwidget.so.1;libwidget.so.1;;libc.so.6"]
    );
}

#[test]
fn vardb_export_round_trips_through_import() {
    let dir = tempfile::tempdir().unwrap();
    let vdb = dir.path();
    let interner = Interner::new();
    let mut record = sample_record(&interner, "1.2.3");
    // The sample's placeholder environment blob is not real bzip2; the importer
    // decompresses environment.bz2, so drop it for this round-trip.
    record.environment = None;

    moraine_vdb::vardb::export_record(vdb, &record, &interner, None).unwrap();

    // The dbdir carries the new aux files.
    let dbdir = vdb.join("app-misc/widget-1.2.3");
    assert_eq!(
        std::fs::read_to_string(dbdir.join("INHERITED"))
            .unwrap()
            .trim(),
        "eutils toolchain-funcs"
    );
    assert_eq!(
        std::fs::read_to_string(dbdir.join("FEATURES"))
            .unwrap()
            .trim(),
        "userfetch"
    );
    assert_eq!(
        std::fs::read_to_string(dbdir.join("SIZE")).unwrap().trim(),
        "4096"
    );
    assert_eq!(
        std::fs::read_to_string(dbdir.join("BUILD_ID"))
            .unwrap()
            .trim(),
        "7"
    );
    // The verbatim NEEDED line is written, not a reconstruction.
    assert_eq!(
        std::fs::read_to_string(dbdir.join("NEEDED.ELF.2"))
            .unwrap()
            .trim(),
        "x86_64;/usr/lib/libwidget.so.1;libwidget.so.1;;libc.so.6"
    );

    // Re-importing the exported tree recovers the fields.
    let li = std::sync::Arc::new(Interner::new());
    let records = moraine_vdb::import_vdb(vdb, &li).unwrap();
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(li.resolve(r.slot.subslot.unwrap()).as_deref(), Some("2"));
    assert_eq!(r.build_time, Some(1_700_000_100));
    assert_eq!(r.build_id, Some(7));
    assert_eq!(r.size, Some(4096));
    assert_eq!(r.inherited, vec!["eutils", "toolchain-funcs"]);
    assert_eq!(r.features, vec!["userfetch"]);
    assert_eq!(
        r.needed,
        vec!["x86_64;/usr/lib/libwidget.so.1;libwidget.so.1;;libc.so.6"]
    );
    // PROVIDES/REQUIRES are derived from the NEEDED line.
    assert!(r.provides.provides(li.intern("libwidget.so.1")));
    assert!(r.requires.sonames().any(|s| s == li.intern("libc.so.6")));
}

#[test]
fn export_writes_description_homepage_toolchain_and_ebuild() {
    let dir = tempfile::tempdir().unwrap();
    let vdb = dir.path();
    let interner = Interner::new();
    let mut record = sample_record(&interner, "1.2.3");
    record.environment = None;
    record.toolchain = moraine_vdb::record::Toolchain {
        cbuild: "x86_64-pc-linux-gnu".to_string(),
        cc: "gcc".to_string(),
        cflags: "-O2 -pipe".to_string(),
        cxx: "g++".to_string(),
        cxxflags: "-O2".to_string(),
        ctarget: "x86_64-pc-linux-gnu".to_string(),
        asflags: "--noexecstack".to_string(),
        ldflags: "-Wl,-O1".to_string(),
    };

    let ebuild = b"EAPI=8\nDESCRIPTION=\"A sample widget\"\n";
    moraine_vdb::vardb::export_record(vdb, &record, &interner, Some(ebuild)).unwrap();

    let dbdir = vdb.join("app-misc/widget-1.2.3");
    assert_eq!(
        std::fs::read_to_string(dbdir.join("DESCRIPTION"))
            .unwrap()
            .trim(),
        "A sample widget"
    );
    assert_eq!(
        std::fs::read_to_string(dbdir.join("HOMEPAGE"))
            .unwrap()
            .trim(),
        "https://example.org"
    );
    for (file, want) in [
        ("CBUILD", "x86_64-pc-linux-gnu"),
        ("CC", "gcc"),
        ("CFLAGS", "-O2 -pipe"),
        ("CXX", "g++"),
        ("CXXFLAGS", "-O2"),
        ("CTARGET", "x86_64-pc-linux-gnu"),
        ("ASFLAGS", "--noexecstack"),
        ("LDFLAGS", "-Wl,-O1"),
    ] {
        assert_eq!(
            std::fs::read_to_string(dbdir.join(file)).unwrap().trim(),
            want,
            "{file}"
        );
    }
    // The ebuild copy is written as <PF>.ebuild.
    assert_eq!(
        std::fs::read(dbdir.join("widget-1.2.3.ebuild")).unwrap(),
        ebuild
    );

    // The new fields round-trip through a re-import.
    let li = std::sync::Arc::new(Interner::new());
    let records = moraine_vdb::import_vdb(vdb, &li).unwrap();
    let r = &records[0];
    assert_eq!(r.description, "A sample widget");
    assert_eq!(r.homepage, "https://example.org");
    assert_eq!(r.toolchain.cflags, "-O2 -pipe");
    assert_eq!(r.toolchain.ldflags, "-Wl,-O1");
}

#[test]
fn from_records_preserves_imported_counter() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path());
    let interner = std::sync::Arc::new(Interner::new());
    let mut a = sample_record(&interner, "1");
    a.counter = 42;
    let mut b = sample_record(&interner, "2");
    b.counter = 7;
    let store = Store::from_records(paths, std::sync::Arc::clone(&interner), vec![a, b]);
    // Each record keeps its imported COUNTER; the store counter is the maximum.
    assert!(store.records().iter().any(|r| r.counter == 42));
    assert!(store.records().iter().any(|r| r.counter == 7));
    assert_eq!(store.counter(), 42);
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

    add_stamped(&mut store, sample_record(&interner, "1.0"));
    let first = store.counter();
    add_stamped(&mut store, sample_record(&interner, "2.0"));
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
    add_stamped(&mut store, sample_record(&interner, "1.0"));
    store.compact().unwrap();

    // Append a journal record replacing the same package with a newer license.
    let mut store = Store::load(paths.clone()).unwrap();
    let li = store.interner().clone();
    let mut replacement = sample_record(&li, "1.0");
    replacement.license = "MIT".to_string();
    add_stamped(&mut store, replacement);

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

/// Build a record for `cp-version` with the given RDEPEND, reusing the sample.
fn record_with(interner: &Interner, cp: &str, version: &str, rdepend: &str) -> PackageRecord {
    let mut r = sample_record(interner, version);
    let (cat, pkg) = cp.split_once('/').unwrap();
    r.category = interner.intern(cat);
    r.package = interner.intern(pkg);
    let features = moraine_eapi::features_for("8");
    let ast = DepSpec::parse(rdepend, features, interner).unwrap();
    r.depends.rdepend = Some(Depend {
        raw: rdepend.to_string(),
        ast,
    });
    r
}

#[test]
fn move_ent_renames_and_update_ents_rewrites_deps() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path());
    let mut store = Store::empty(paths.clone());
    let i = store.interner().clone();

    add_stamped(
        &mut store,
        record_with(&i, "dev-util/foo", "1", "dev-libs/zlib"),
    );
    add_stamped(
        &mut store,
        record_with(&i, "app-misc/bar", "1", ">=dev-util/foo-1:0[ssl]"),
    );

    // Rename dev-util/foo -> dev-libs/foo.
    assert_eq!(
        store
            .move_ent("dev-util/foo", "dev-libs/foo", &|_| true)
            .unwrap()
            .len(),
        1
    );
    // Rewrite the dependency atoms referencing the old name everywhere.
    assert_eq!(
        store
            .update_ents(
                &[("dev-util/foo".into(), "dev-libs/foo".into())],
                &[],
                &|_| true
            )
            .unwrap()
            .len(),
        1
    );
    store.compact().unwrap();

    // Reload: the rename and dep rewrite both survive the journal.
    let loaded = Store::load(paths).unwrap();
    let li = loaded.interner();
    assert!(
        loaded
            .records()
            .iter()
            .any(|r| r.cpv(li) == "dev-libs/foo-1")
    );
    assert!(
        !loaded
            .records()
            .iter()
            .any(|r| r.cpv(li) == "dev-util/foo-1")
    );
    let bar = loaded
        .records()
        .iter()
        .find(|r| r.cpv(li) == "app-misc/bar-1")
        .unwrap();
    let rdep = bar.depends.get(DependKind::RDepend).unwrap();
    assert_eq!(rdep.raw, ">=dev-libs/foo-1:0[ssl]");
    // The AST was re-parsed in sync with the raw rewrite.
    let reparsed = DepSpec::parse(&rdep.raw, moraine_eapi::features_for("8"), li).unwrap();
    assert_eq!(format!("{:?}", rdep.ast), format!("{:?}", reparsed));
}

#[test]
fn update_ents_skips_self_blocker() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::empty(StorePaths::in_dir(dir.path()));
    let i = store.interner().clone();
    // The package's own new name would be its blocker target: must not rewrite.
    add_stamped(
        &mut store,
        record_with(&i, "dev-libs/foo", "1", "!dev-util/foo"),
    );
    store
        .update_ents(
            &[("dev-util/foo".into(), "dev-libs/foo".into())],
            &[],
            &|_| true,
        )
        .unwrap();
    let rec = &store.records()[0];
    assert_eq!(
        rec.depends.get(DependKind::RDepend).unwrap().raw,
        "!dev-util/foo"
    );
}

#[test]
fn move_ent_skips_when_destination_exists() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::empty(StorePaths::in_dir(dir.path()));
    let i = store.interner().clone();
    add_stamped(
        &mut store,
        record_with(&i, "dev-util/foo", "1", "dev-libs/zlib"),
    );
    add_stamped(
        &mut store,
        record_with(&i, "dev-libs/foo", "1", "dev-libs/zlib"),
    );
    // Destination dev-libs/foo-1 already exists: the move is skipped.
    assert_eq!(
        store
            .move_ent("dev-util/foo", "dev-libs/foo", &|_| true)
            .unwrap()
            .len(),
        0
    );
    assert!(
        store
            .records()
            .iter()
            .any(|r| r.cpv(&i) == "dev-util/foo-1")
    );
}

#[test]
fn move_slot_ent_rewrites_recorded_slot() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::empty(StorePaths::in_dir(dir.path()));
    let i = store.interner().clone();
    // sample_record uses slot "0".
    add_stamped(
        &mut store,
        record_with(&i, "dev-libs/bar", "1", "dev-libs/zlib"),
    );
    assert_eq!(
        store
            .move_slot_ent("dev-libs/bar", "0", "2", &|_| true)
            .unwrap()
            .len(),
        1
    );
    let rec = &store.records()[0];
    assert_eq!(i.resolve(rec.slot.slot).as_deref(), Some("2"));
    // The recorded sub-slot is preserved (the new slot token carries none).
    assert_eq!(i.resolve(rec.slot.subslot.unwrap()).as_deref(), Some("2"));
}
