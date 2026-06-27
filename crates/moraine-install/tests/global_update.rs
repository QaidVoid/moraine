//! Post-sync global-update integration tests: authoritative-tree dual-write,
//! binary-package rewrite, the master-repo gate, and malformed-file handling.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use moraine_binpkg::{MetadataMap, PackageEntry, PackagesIndex};
use moraine_common::Interner;
use moraine_install::global_update;
use moraine_merge::ConfigProtect;
use moraine_vdb::store::{Store, StorePaths};

/// Write a minimal authoritative dbdir for `cat/pkg-ver`.
fn write_dbdir(
    vdb: &Path,
    cat: &str,
    pkg: &str,
    ver: &str,
    slot: &str,
    repo: Option<&str>,
    rdepend: Option<&str>,
) {
    let dir = vdb.join(cat).join(format!("{pkg}-{ver}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SLOT"), format!("{slot}\n")).unwrap();
    std::fs::write(dir.join("EAPI"), "8\n").unwrap();
    std::fs::write(dir.join("COUNTER"), "1\n").unwrap();
    if let Some(r) = repo {
        std::fs::write(dir.join("repository"), format!("{r}\n")).unwrap();
    }
    if let Some(d) = rdepend {
        std::fs::write(dir.join("RDEPEND"), format!("{d}\n")).unwrap();
    }
}

/// Load an installed store by importing the authoritative tree.
fn load_store(vdb: &Path) -> Store {
    let interner = Arc::new(Interner::new());
    let records = moraine_vdb::import_vdb(vdb, &interner).unwrap();
    Store::from_records(StorePaths::in_dir(vdb), interner, records)
}

/// Create a repository whose `profiles/updates/<name>` holds `content`.
fn write_updates(repo: &Path, name: &str, content: &str) {
    let upd = repo.join("profiles/updates");
    std::fs::create_dir_all(&upd).unwrap();
    std::fs::write(upd.join(name), content).unwrap();
}

#[test]
fn move_dual_writes_authoritative_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let vdb = tmp.path().join("vdb");
    let repo = tmp.path().join("repo");
    let cfg = tmp.path().join("etc-portage");
    std::fs::create_dir_all(&cfg).unwrap();
    write_dbdir(&vdb, "dev-util", "foo", "1", "0", Some("gentoo"), None);
    write_updates(&repo, "1Q-2024", "move dev-util/foo dev-libs/foo\n");

    let mut store = load_store(&vdb);
    let repos = vec![("gentoo".to_string(), repo.clone())];
    let report = global_update(
        &mut store,
        &repos,
        &tmp.path().join("world"),
        &cfg,
        &vdb,
        None,
        &ConfigProtect::default(),
        &BTreeMap::new(),
    )
    .unwrap();

    assert_eq!(report.vdb_renames, 1);
    // The old dbdir is gone; the new one exists with updated identity.
    assert!(!vdb.join("dev-util/foo-1").exists());
    let newdir = vdb.join("dev-libs/foo-1");
    assert!(newdir.exists());
    assert_eq!(
        std::fs::read_to_string(newdir.join("CATEGORY"))
            .unwrap()
            .trim(),
        "dev-libs"
    );
    assert_eq!(
        std::fs::read_to_string(newdir.join("PF")).unwrap().trim(),
        "foo-1"
    );

    // A rebuild from the tree shows the moved name and does not revert it.
    let rebuilt = load_store(&vdb);
    let li = rebuilt.interner();
    assert!(
        rebuilt
            .records()
            .iter()
            .any(|r| r.cpv(li) == "dev-libs/foo-1")
    );
    assert!(
        !rebuilt
            .records()
            .iter()
            .any(|r| r.cpv(li) == "dev-util/foo-1")
    );
}

#[test]
fn move_rewrites_local_binpkg_and_index() {
    let tmp = tempfile::tempdir().unwrap();
    let vdb = tmp.path().join("vdb");
    let repo = tmp.path().join("repo");
    let pkgdir = tmp.path().join("binpkgs");
    let cfg = tmp.path().join("etc-portage");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(&pkgdir).unwrap();
    write_dbdir(&vdb, "dev-util", "foo", "1", "0", Some("gentoo"), None);
    write_updates(&repo, "1Q-2024", "move dev-util/foo dev-libs/foo\n");

    // A cached binpkg artifact and the local index stanza.
    std::fs::create_dir_all(pkgdir.join("dev-util")).unwrap();
    std::fs::write(pkgdir.join("dev-util/foo-1.gpkg.tar"), b"binpkg").unwrap();
    let mut index = PackagesIndex::new();
    let mut meta = MetadataMap::new();
    meta.set_str("CATEGORY", "dev-util");
    meta.set_str("PF", "foo-1");
    meta.set_str("SLOT", "0");
    meta.set_str("EAPI", "8");
    index.packages.push(PackageEntry {
        cpv: "dev-util/foo-1".to_string(),
        metadata: meta,
    });
    std::fs::write(pkgdir.join("Packages"), index.emit(&Interner::new())).unwrap();

    let mut store = load_store(&vdb);
    let repos = vec![("gentoo".to_string(), repo.clone())];
    global_update(
        &mut store,
        &repos,
        &tmp.path().join("world"),
        &cfg,
        &vdb,
        Some(&pkgdir),
        &ConfigProtect::default(),
        &BTreeMap::new(),
    )
    .unwrap();

    // The artifact is renamed and the index stanza is re-keyed.
    assert!(!pkgdir.join("dev-util/foo-1.gpkg.tar").exists());
    assert!(pkgdir.join("dev-libs/foo-1.gpkg.tar").exists());
    let reparsed =
        PackagesIndex::parse(&std::fs::read_to_string(pkgdir.join("Packages")).unwrap()).unwrap();
    assert!(reparsed.packages.iter().any(|p| p.cpv == "dev-libs/foo-1"));
    assert_eq!(
        reparsed.packages[0].metadata.get_str("CATEGORY").as_deref(),
        Some("dev-libs")
    );
}

#[test]
fn master_repo_move_gates_on_record_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let vdb = tmp.path().join("vdb");
    let gentoo = tmp.path().join("gentoo");
    let other = tmp.path().join("other");
    let cfg = tmp.path().join("etc-portage");
    std::fs::create_dir_all(&cfg).unwrap();
    // foo-1 has no repository (master-repo fallback applies); foo-2 belongs to a
    // different repository present in the update set (must not move).
    write_dbdir(&vdb, "dev-util", "foo", "1", "0", None, None);
    write_dbdir(&vdb, "dev-util", "foo", "2", "0", Some("other"), None);
    write_updates(&gentoo, "1Q-2024", "move dev-util/foo dev-libs/foo\n");
    // `other` ships an updates directory so it joins repo_map, but no directives.
    std::fs::create_dir_all(other.join("profiles/updates")).unwrap();

    let mut store = load_store(&vdb);
    // gentoo is the master repository (first in masters-first search order).
    let repos = vec![
        ("gentoo".to_string(), gentoo.clone()),
        ("other".to_string(), other.clone()),
    ];
    let report = global_update(
        &mut store,
        &repos,
        &tmp.path().join("world"),
        &cfg,
        &vdb,
        None,
        &ConfigProtect::default(),
        &BTreeMap::new(),
    )
    .unwrap();

    assert_eq!(report.vdb_renames, 1);
    assert!(vdb.join("dev-libs/foo-1").exists());
    assert!(!vdb.join("dev-util/foo-1").exists());
    // The other-repo record is left untouched.
    assert!(vdb.join("dev-util/foo-2").exists());
    assert!(!vdb.join("dev-libs/foo-2").exists());
}

/// Recursively copy a directory tree.
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&from, &to);
        } else if entry.file_type().unwrap().is_file() {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

#[test]
fn corpus_post_sync_move_reaches_tree_and_survives_rebuild() {
    // Gated on MORAINE_CORPUS (an EROOT); no-op when unset or absent.
    let Some(root) = std::env::var_os("MORAINE_CORPUS").filter(|v| !v.is_empty()) else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus global-update e2e");
        return;
    };
    let corpus_vdb = Path::new(&root).join("var/db/pkg");
    if !corpus_vdb.is_dir() {
        eprintln!("no {} in corpus; skipping", corpus_vdb.display());
        return;
    }

    // Find an installed package to rename.
    let probe = Arc::new(Interner::new());
    let imported = moraine_vdb::import_vdb(&corpus_vdb, &probe).expect("import corpus");
    let Some(sample) = imported.first() else {
        eprintln!("corpus produced no records; skipping");
        return;
    };
    let cat = probe.resolve(sample.category).unwrap().to_string();
    let pkg = probe.resolve(sample.package).unwrap().to_string();
    let ver = sample.version.as_str().to_string();
    let old_cp = format!("{cat}/{pkg}");
    let new_cp = format!("moraine-test/{pkg}");

    let tmp = tempfile::tempdir().unwrap();
    let vdb = tmp.path().join("vdb");
    copy_tree(
        &corpus_vdb.join(&cat).join(format!("{pkg}-{ver}")),
        &vdb.join(&cat).join(format!("{pkg}-{ver}")),
    );

    // A synthetic repo shipping the rename, and a world entry for the package.
    let repo = tmp.path().join("repo");
    write_updates(&repo, "9Z-2024", &format!("move {old_cp} {new_cp}\n"));
    let world = tmp.path().join("world");
    std::fs::write(&world, format!("{old_cp}\n")).unwrap();
    let cfg = tmp.path().join("etc-portage");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("package.use"), format!("{old_cp} foo\n")).unwrap();

    let mut store = load_store(&vdb);
    let repos = vec![("gentoo".to_string(), repo.clone())];
    global_update(
        &mut store,
        &repos,
        &world,
        &cfg,
        &vdb,
        None,
        &ConfigProtect::default(),
        &BTreeMap::new(),
    )
    .unwrap();

    // The authoritative tree reflects the new name.
    assert!(
        vdb.join("moraine-test")
            .join(format!("{pkg}-{ver}"))
            .exists()
    );
    assert!(!vdb.join(&cat).join(format!("{pkg}-{ver}")).exists());
    // The world and config files reflect the new name.
    assert!(std::fs::read_to_string(&world).unwrap().contains(&new_cp));
    assert!(
        std::fs::read_to_string(cfg.join("package.use"))
            .unwrap()
            .contains(&new_cp)
    );

    // A fresh rebuild from the tree does not revert the move.
    let rebuilt = load_store(&vdb);
    let li = rebuilt.interner();
    let new_cpv = format!("{new_cp}-{ver}");
    assert!(rebuilt.records().iter().any(|r| r.cpv(li) == new_cpv));
}

#[test]
fn malformed_update_file_is_not_stamped_but_applies_valid_directives() {
    let tmp = tempfile::tempdir().unwrap();
    let vdb = tmp.path().join("vdb");
    let repo = tmp.path().join("repo");
    let cfg = tmp.path().join("etc-portage");
    std::fs::create_dir_all(&cfg).unwrap();
    write_dbdir(&vdb, "dev-util", "foo", "1", "0", Some("gentoo"), None);
    write_updates(
        &repo,
        "1Q-2024",
        "move dev-util/foo dev-libs/foo\nthis is a garbage line\n",
    );

    let mut store = load_store(&vdb);
    let repos = vec![("gentoo".to_string(), repo.clone())];
    let report = global_update(
        &mut store,
        &repos,
        &tmp.path().join("world"),
        &cfg,
        &vdb,
        None,
        &ConfigProtect::default(),
        &BTreeMap::new(),
    )
    .unwrap();

    // The valid move applies, but the malformed file's mtime is not recorded.
    assert_eq!(report.vdb_renames, 1);
    assert!(vdb.join("dev-libs/foo-1").exists());
    assert!(
        report.applied_files.is_empty(),
        "a file with parse errors is not stamped as applied"
    );
}
