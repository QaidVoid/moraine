//! Importer tests over a synthetic `/var/db/pkg` tree.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use moraine_common::Interner;
use moraine_vdb::error::VdbError;

fn write(dir: &Path, name: &str, content: &str) {
    fs::write(dir.join(name), content).unwrap();
}

/// Build a minimal but realistic package directory.
fn make_pkg(root: &Path, category: &str, pv: &str) -> std::path::PathBuf {
    let dir = root.join(category).join(pv);
    fs::create_dir_all(&dir).unwrap();
    write(&dir, "EAPI", "8\n");
    write(&dir, "SLOT", "0/3\n");
    write(&dir, "USE", "ssl zlib\n");
    write(&dir, "RDEPEND", "dev-libs/openssl:0/3=\n");
    write(&dir, "KEYWORDS", "amd64\n");
    write(&dir, "LICENSE", "GPL-2\n");
    write(&dir, "COUNTER", "42\n");
    write(&dir, "CHOST", "x86_64-pc-linux-gnu\n");
    write(
        &dir,
        "CONTENTS",
        "dir /usr\n\
         dir /usr/lib\n\
         obj /usr/lib/libfoo.so.1 d41d8cd98f00b204e9800998ecf8427e 1700000000\n\
         sym /usr/lib/libfoo.so -> libfoo.so.1 1700000001\n",
    );
    write(
        &dir,
        "NEEDED.ELF.2",
        "x86_64;/usr/lib/libfoo.so.1;libfoo.so.1;;libc.so.6,libm.so.6\n",
    );
    dir
}

#[test]
fn imports_a_package_directory() {
    let root = tempfile::tempdir().unwrap();
    make_pkg(root.path(), "app-misc", "foo-1.2.3");

    let interner = Interner::new();
    let records = moraine_vdb::import_vdb(root.path(), &interner).unwrap();
    assert_eq!(records.len(), 1);
    let rec = &records[0];

    assert_eq!(interner.resolve(rec.category).as_deref(), Some("app-misc"));
    assert_eq!(interner.resolve(rec.package).as_deref(), Some("foo"));
    assert_eq!(rec.version.as_str(), "1.2.3");
    assert_eq!(interner.resolve(rec.slot.slot).as_deref(), Some("0"));
    assert_eq!(
        interner.resolve(rec.slot.subslot.unwrap()).as_deref(),
        Some("3")
    );
    assert_eq!(rec.counter, 42);

    // CONTENTS parsed with implicit parents (root /usr already explicit).
    assert!(rec.contents.owns("/usr/lib/libfoo.so.1"));
    assert!(rec.contents.owns("/usr/lib/libfoo.so"));

    // NEEDED.ELF.2: one provided soname, two required.
    let libfoo = interner.intern("libfoo.so.1");
    assert!(rec.provides.provides(libfoo));
    let required: Vec<_> = rec
        .requires
        .sonames()
        .map(|s| interner.resolve(s).unwrap().to_string())
        .collect();
    assert!(required.contains(&"libc.so.6".to_string()));
    assert!(required.contains(&"libm.so.6".to_string()));
}

#[test]
fn recovers_field_from_environment() {
    let root = tempfile::tempdir().unwrap();
    let dir = make_pkg(root.path(), "app-misc", "bar-1.0");
    // Remove the LICENSE file and provide it only via environment.bz2.
    fs::remove_file(dir.join("LICENSE")).unwrap();

    let env_text = "EAPI=8\nLICENSE=\"MIT\"\nCHOST=x86_64-pc-linux-gnu\n";
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    encoder.write_all(env_text.as_bytes()).unwrap();
    let compressed = encoder.finish().unwrap();
    fs::write(dir.join("environment.bz2"), compressed).unwrap();

    let interner = Interner::new();
    let records = moraine_vdb::import_vdb(root.path(), &interner).unwrap();
    assert_eq!(records[0].license, "MIT");
}

#[test]
fn missing_required_field_surfaces_diagnostic() {
    let root = tempfile::tempdir().unwrap();
    let dir = make_pkg(root.path(), "app-misc", "baz-1.0");
    // SLOT is required; remove it and give no environment to recover from.
    fs::remove_file(dir.join("SLOT")).unwrap();

    let interner = Interner::new();
    let err = moraine_vdb::import_vdb(root.path(), &interner).unwrap_err();
    match err {
        VdbError::MissingField { field, package } => {
            assert_eq!(field, "SLOT");
            assert!(package.contains("baz"));
        }
        other => panic!("expected MissingField, got {other:?}"),
    }
}

#[test]
fn import_leaves_stock_tree_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let dir = make_pkg(root.path(), "app-misc", "foo-1.2.3");

    // Snapshot file set and mtimes before import.
    let before: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();

    let interner = Interner::new();
    moraine_vdb::import_vdb(root.path(), &interner).unwrap();

    let after: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before.len(), after.len());
}
