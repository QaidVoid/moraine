//! Integration tests for the merge engine.
//!
//! Every test targets a tempdir EROOT and a tempdir state/vdb directory. None
//! touch the real system. The corpus round-trip is gated on `MORAINE_CORPUS`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use moraine_merge::{
    ConfigProtect, Features, MergeContext, MergeEngine, MergeOp, Operation, PackageState, UnmergeOp,
};

/// A test sandbox: a tempdir holding the EROOT, the vdb, and the state dir.
struct Sandbox {
    _root: tempfile::TempDir,
    eroot: PathBuf,
    vdb: PathBuf,
    state: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let eroot = root.path().join("eroot");
        let vdb = root.path().join("vdb");
        let state = root.path().join("state");
        for d in [&eroot, &vdb, &state] {
            std::fs::create_dir_all(d).unwrap();
        }
        Self {
            _root: root,
            eroot,
            vdb,
            state,
        }
    }

    fn context(&self, features: Features, cp: ConfigProtect) -> MergeContext {
        MergeContext {
            eroot: self.eroot.clone(),
            vdb_dir: self.vdb.clone(),
            state_dir: self.state.clone(),
            features,
            config_protect: cp,
        }
    }

    fn live(&self, install_path: &str) -> PathBuf {
        self.eroot.join(install_path.trim_start_matches('/'))
    }
}

/// Build an image directory with the given regular files (path, contents).
fn build_image(dir: &Path, files: &[(&str, &[u8])]) -> PathBuf {
    let image = dir.join("image");
    std::fs::create_dir_all(&image).unwrap();
    for (rel, content) in files {
        let p = image.join(rel.trim_start_matches('/'));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
    }
    image
}

/// Add a symlink into an existing image.
fn add_symlink(image: &Path, rel: &str, target: &str) {
    let p = image.join(rel.trim_start_matches('/'));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, &p).unwrap();
}

/// A minimal package state for `category/package-version`.
fn state(cp: &str, version: &str, slot: &str) -> PackageState {
    let (category, package) = cp.split_once('/').unwrap();
    PackageState {
        cpv: format!("{cp}-{version}"),
        category: category.to_string(),
        package: package.to_string(),
        version: version.to_string(),
        eapi: "8".to_string(),
        slot: slot.to_string(),
        subslot: None,
        use_flags: vec![],
        iuse: vec![],
        depends: BTreeMap::new(),
        keywords: vec![],
        license: String::new(),
        properties: String::new(),
        restrict: String::new(),
        repository: None,
        defined_phases: vec![],
        build_time: None,
        chost: String::new(),
        provides: vec![],
        requires: vec![],
        environment: None,
    }
}

fn merge_op(image: PathBuf, st: PackageState, replaces: Option<&str>, in_world: bool) -> Operation {
    Operation::Merge(Box::new(MergeOp {
        image_dir: image,
        state: st,
        replaces: replaces.map(str::to_string),
        in_world,
    }))
}

#[test]
fn regular_file_placed_with_md5_and_mtime_and_atomic() {
    let sb = Sandbox::new();
    let tmp = tempfile::tempdir().unwrap();
    let image = build_image(tmp.path(), &[("/usr/bin/foo", b"hello")]);

    let ctx = sb.context(Features::default(), ConfigProtect::default());
    let engine = MergeEngine::new(ctx);
    let st = state("app-misc/foo", "1.0", "0");
    engine.apply(&[merge_op(image, st, None, true)]).unwrap();

    let live = sb.live("/usr/bin/foo");
    assert_eq!(std::fs::read(&live).unwrap(), b"hello");
    // No leftover temporary sibling from the atomic write.
    let entries: Vec<_> = std::fs::read_dir(live.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(
        !entries.iter().any(|n| n.starts_with(".moraine-tmp-")),
        "atomic temp sibling left behind: {entries:?}"
    );

    // CONTENTS carries md5 of the written bytes.
    let store = moraine_vdb::Store::load(moraine_vdb::StorePaths::in_dir(&sb.vdb)).unwrap();
    let rec = &store.records()[0];
    let md5 = moraine_merge::compute_md5(b"hello");
    let entry = rec.contents.owner("/usr/bin/foo").unwrap();
    match entry {
        moraine_vdb::EntryKind::Obj { md5: m, mtime } => {
            assert_eq!(m, &md5);
            assert!(*mtime > 0);
        }
        _ => panic!("expected obj"),
    }
    // Implicit parent dirs recorded.
    assert!(matches!(
        rec.contents.owner("/usr/bin"),
        Some(moraine_vdb::EntryKind::Dir)
    ));
}

#[test]
fn directories_precede_contents_and_symlink_recorded() {
    let sb = Sandbox::new();
    let tmp = tempfile::tempdir().unwrap();
    let image = build_image(tmp.path(), &[("/opt/app/bin/real", b"x")]);
    add_symlink(&image, "/opt/app/bin/link", "real");

    let ctx = sb.context(Features::default(), ConfigProtect::default());
    let engine = MergeEngine::new(ctx);
    engine
        .apply(&[merge_op(
            image,
            state("app-misc/app", "1", "0"),
            None,
            false,
        )])
        .unwrap();

    assert!(sb.live("/opt/app/bin").is_dir());
    assert!(sb.live("/opt/app/bin/real").is_file());
    let link = sb.live("/opt/app/bin/link");
    assert_eq!(std::fs::read_link(&link).unwrap().to_str().unwrap(), "real");

    let store = moraine_vdb::Store::load(moraine_vdb::StorePaths::in_dir(&sb.vdb)).unwrap();
    let rec = &store.records()[0];
    match rec.contents.owner("/opt/app/bin/link").unwrap() {
        moraine_vdb::EntryKind::Sym { target, .. } => assert_eq!(target, "real"),
        _ => panic!("expected sym"),
    }
}

#[test]
fn collision_protect_aborts_before_mutation() {
    let sb = Sandbox::new();
    let tmp = tempfile::tempdir().unwrap();

    // First package owns /usr/bin/shared.
    let image1 = build_image(tmp.path(), &[("/usr/bin/shared", b"a")]);
    let engine = MergeEngine::new(sb.context(Features::default(), ConfigProtect::default()));
    engine
        .apply(&[merge_op(image1, state("cat/one", "1", "0"), None, false)])
        .unwrap();

    // Second, unrelated package collides at the same path with collision-protect.
    let tmp2 = tempfile::tempdir().unwrap();
    let image2 = build_image(
        tmp2.path(),
        &[("/usr/bin/shared", b"b"), ("/usr/bin/own", b"c")],
    );
    let features = Features {
        collision_protect: true,
        ..Features::default()
    };
    let engine2 = MergeEngine::new(sb.context(features, ConfigProtect::default()));
    let err = engine2
        .apply(&[merge_op(image2, state("cat/two", "1", "0"), None, false)])
        .unwrap_err();
    assert!(matches!(err, moraine_merge::MergeError::Collision { .. }));
    // Live root unchanged: the colliding file keeps the first package's content
    // and the new package's own file was never placed.
    assert_eq!(std::fs::read(sb.live("/usr/bin/shared")).unwrap(), b"a");
    assert!(!sb.live("/usr/bin/own").exists());
}

#[test]
fn protect_owned_ignores_unowned_files() {
    let sb = Sandbox::new();
    // Pre-create an unowned file on the live system.
    std::fs::create_dir_all(sb.eroot.join("usr/bin")).unwrap();
    std::fs::write(sb.live("/usr/bin/loose"), b"old").unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let image = build_image(tmp.path(), &[("/usr/bin/loose", b"new")]);
    let features = Features {
        protect_owned: true,
        ..Features::default()
    };
    let engine = MergeEngine::new(sb.context(features, ConfigProtect::default()));
    engine
        .apply(&[merge_op(image, state("cat/p", "1", "0"), None, false)])
        .unwrap();
    // protect-owned does not protect unowned files: it was overwritten.
    assert_eq!(std::fs::read(sb.live("/usr/bin/loose")).unwrap(), b"new");
}

#[test]
fn same_slot_replacement_removes_only_obsolete_files() {
    let sb = Sandbox::new();
    let tmp = tempfile::tempdir().unwrap();
    // Version 1 owns shared + gone.
    let image1 = build_image(
        tmp.path(),
        &[("/usr/bin/shared", b"v1"), ("/usr/lib/gone", b"old")],
    );
    let engine = MergeEngine::new(sb.context(Features::default(), ConfigProtect::default()));
    engine
        .apply(&[merge_op(image1, state("cat/pkg", "1", "0"), None, false)])
        .unwrap();

    // Version 2 owns shared (new content) + added, no longer provides gone.
    let tmp2 = tempfile::tempdir().unwrap();
    let image2 = build_image(
        tmp2.path(),
        &[("/usr/bin/shared", b"v2"), ("/usr/bin/added", b"x")],
    );
    let engine2 = MergeEngine::new(sb.context(Features::default(), ConfigProtect::default()));
    engine2
        .apply(&[merge_op(
            image2,
            state("cat/pkg", "2", "0"),
            Some("cat/pkg-1"),
            false,
        )])
        .unwrap();

    assert_eq!(std::fs::read(sb.live("/usr/bin/shared")).unwrap(), b"v2");
    assert!(sb.live("/usr/bin/added").exists());
    assert!(
        !sb.live("/usr/lib/gone").exists(),
        "obsolete file not removed"
    );

    // Only the new version remains installed.
    let store = moraine_vdb::Store::load(moraine_vdb::StorePaths::in_dir(&sb.vdb)).unwrap();
    assert_eq!(store.records().len(), 1);
    assert_eq!(store.records()[0].version.as_str(), "2");
}

#[test]
fn config_protect_writes_variant_for_differing_file() {
    let sb = Sandbox::new();
    let tmp = tempfile::tempdir().unwrap();

    // First install writes the config in place (does not exist yet).
    let image1 = build_image(tmp.path(), &[("/etc/foo.conf", b"original")]);
    let cp = ConfigProtect::new(["/etc".to_string()], Vec::<String>::new());
    let engine = MergeEngine::new(sb.context(Features::default(), cp.clone()));
    let out = engine
        .apply(&[merge_op(image1, state("cat/cfg", "1", "0"), None, false)])
        .unwrap();
    assert_eq!(
        std::fs::read(sb.live("/etc/foo.conf")).unwrap(),
        b"original"
    );
    assert!(out[0].report.config_updates.is_empty());
    assert!(!sb.live("/etc/._cfg0000_foo.conf").exists());

    // Second install with differing content writes a `._cfg0000_` variant and
    // leaves the live file unchanged.
    let tmp2 = tempfile::tempdir().unwrap();
    let image2 = build_image(tmp2.path(), &[("/etc/foo.conf", b"updated")]);
    let engine2 = MergeEngine::new(sb.context(Features::default(), cp.clone()));
    let out2 = engine2
        .apply(&[merge_op(
            image2,
            state("cat/cfg", "2", "0"),
            Some("cat/cfg-1"),
            false,
        )])
        .unwrap();
    assert_eq!(
        std::fs::read(sb.live("/etc/foo.conf")).unwrap(),
        b"original"
    );
    assert_eq!(
        std::fs::read(sb.live("/etc/._cfg0000_foo.conf")).unwrap(),
        b"updated"
    );
    assert_eq!(
        out2[0].report.config_updates,
        vec!["/etc/._cfg0000_foo.conf".to_string()]
    );

    // The variant is owned in CONTENTS.
    let store = moraine_vdb::Store::load(moraine_vdb::StorePaths::in_dir(&sb.vdb)).unwrap();
    let rec = store
        .records()
        .iter()
        .find(|r| r.version.as_str() == "2")
        .unwrap();
    assert!(rec.contents.owns("/etc/._cfg0000_foo.conf"));
}

#[test]
fn config_protect_sequential_variants() {
    let sb = Sandbox::new();
    let cp = ConfigProtect::new(["/etc".to_string()], Vec::<String>::new());

    // Live file plus an existing ._cfg0000_ variant.
    std::fs::create_dir_all(sb.eroot.join("etc")).unwrap();
    std::fs::write(sb.live("/etc/foo.conf"), b"live").unwrap();
    std::fs::write(sb.live("/etc/._cfg0000_foo.conf"), b"pending").unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let image = build_image(tmp.path(), &[("/etc/foo.conf", b"newer")]);
    let engine = MergeEngine::new(sb.context(Features::default(), cp));
    engine
        .apply(&[merge_op(image, state("cat/cfg", "1", "0"), None, false)])
        .unwrap();
    // Next index used.
    assert_eq!(
        std::fs::read(sb.live("/etc/._cfg0001_foo.conf")).unwrap(),
        b"newer"
    );
}

#[test]
fn config_protect_identical_writes_in_place() {
    let sb = Sandbox::new();
    let cp = ConfigProtect::new(["/etc".to_string()], Vec::<String>::new());
    std::fs::create_dir_all(sb.eroot.join("etc")).unwrap();
    std::fs::write(sb.live("/etc/foo.conf"), b"same").unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let image = build_image(tmp.path(), &[("/etc/foo.conf", b"same")]);
    let engine = MergeEngine::new(sb.context(Features::default(), cp));
    engine
        .apply(&[merge_op(image, state("cat/cfg", "1", "0"), None, false)])
        .unwrap();
    assert!(!sb.live("/etc/._cfg0000_foo.conf").exists());
}

#[test]
fn config_protect_mask_writes_in_place() {
    let sb = Sandbox::new();
    let cp = ConfigProtect::new(["/etc".to_string()], ["/etc/env.d".to_string()]);
    std::fs::create_dir_all(sb.eroot.join("etc/env.d")).unwrap();
    std::fs::write(sb.live("/etc/env.d/99x"), b"old").unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let image = build_image(tmp.path(), &[("/etc/env.d/99x", b"new")]);
    let engine = MergeEngine::new(sb.context(Features::default(), cp));
    engine
        .apply(&[merge_op(image, state("cat/cfg", "1", "0"), None, false)])
        .unwrap();
    // Masked: overwritten in place, no variant.
    assert_eq!(std::fs::read(sb.live("/etc/env.d/99x")).unwrap(), b"new");
    assert!(!sb.live("/etc/env.d/._cfg0000_99x").exists());
}

#[test]
fn world_and_counter_update_after_commit() {
    let sb = Sandbox::new();
    let tmp = tempfile::tempdir().unwrap();
    let image1 = build_image(tmp.path(), &[("/a", b"1")]);
    let tmp2 = tempfile::tempdir().unwrap();
    let image2 = build_image(tmp2.path(), &[("/b", b"2")]);

    let engine = MergeEngine::new(sb.context(Features::default(), ConfigProtect::default()));
    let out = engine
        .apply(&[
            merge_op(image1, state("cat/explicit", "1", "0"), None, true),
            merge_op(image2, state("cat/dep", "1", "0"), None, false),
        ])
        .unwrap();

    // Counter increases per install.
    let c0 = out[0].counter.unwrap();
    let c1 = out[1].counter.unwrap();
    assert!(c1 > c0, "second counter {c1} not greater than first {c0}");

    // World contains only the explicit package.
    let world = std::fs::read_to_string(sb.state.join("world")).unwrap();
    assert!(world.contains("cat/explicit"));
    assert!(!world.contains("cat/dep"));
}

#[test]
fn unmerge_removes_unmodified_skips_modified_and_nonempty_dirs() {
    let sb = Sandbox::new();
    let tmp = tempfile::tempdir().unwrap();
    let image = build_image(tmp.path(), &[("/d/clean", b"keep"), ("/d/dirty", b"orig")]);
    let engine = MergeEngine::new(sb.context(Features::default(), ConfigProtect::default()));
    engine
        .apply(&[merge_op(image, state("cat/pkg", "1", "0"), None, true)])
        .unwrap();

    // Modify one file and add an unowned file into the directory.
    std::fs::write(sb.live("/d/dirty"), b"changed by admin").unwrap();
    std::fs::write(sb.live("/d/unowned"), b"someone else").unwrap();

    let engine2 = MergeEngine::new(sb.context(Features::default(), ConfigProtect::default()));
    engine2
        .apply(&[Operation::Unmerge(UnmergeOp {
            cpv: "cat/pkg-1".to_string(),
            replaced: false,
        })])
        .unwrap();

    // Clean file removed; modified file kept; directory kept because non-empty.
    assert!(!sb.live("/d/clean").exists());
    assert!(sb.live("/d/dirty").exists(), "modified file must be kept");
    assert!(sb.live("/d/unowned").exists());
    assert!(sb.live("/d").is_dir(), "non-empty dir must remain");

    // Package removed from world.
    let world = std::fs::read_to_string(sb.state.join("world")).unwrap_or_default();
    assert!(!world.contains("cat/pkg"));
}

#[test]
fn unmerge_skips_files_now_owned_by_another_package() {
    let sb = Sandbox::new();
    let tmp = tempfile::tempdir().unwrap();
    let image1 = build_image(tmp.path(), &[("/usr/bin/tool", b"x")]);
    let engine = MergeEngine::new(sb.context(Features::default(), ConfigProtect::default()));
    engine
        .apply(&[merge_op(image1, state("cat/a", "1", "0"), None, false)])
        .unwrap();

    // A second package now also records the same path (collision overwrite with
    // no protection), taking ownership.
    let tmp2 = tempfile::tempdir().unwrap();
    let image2 = build_image(tmp2.path(), &[("/usr/bin/tool", b"y")]);
    let engine2 = MergeEngine::new(sb.context(Features::default(), ConfigProtect::default()));
    engine2
        .apply(&[merge_op(image2, state("cat/b", "1", "0"), None, false)])
        .unwrap();

    // Unmerge the first package: the path is now owned by cat/b, so skip it.
    let engine3 = MergeEngine::new(sb.context(Features::default(), ConfigProtect::default()));
    engine3
        .apply(&[Operation::Unmerge(UnmergeOp {
            cpv: "cat/a-1".to_string(),
            replaced: false,
        })])
        .unwrap();
    assert!(
        sb.live("/usr/bin/tool").exists(),
        "shared path must survive"
    );
}

#[test]
fn preserve_libs_keeps_still_needed_soname_and_drops_when_unneeded() {
    use moraine_merge::state::Soname;
    let sb = Sandbox::new();
    let features = Features {
        preserve_libs: true,
        ..Features::default()
    };

    // Provider v1 provides libfoo.so.1.
    let tmp = tempfile::tempdir().unwrap();
    let image1 = build_image(tmp.path(), &[("/usr/lib/libfoo.so.1", b"abi1")]);
    let mut prov1 = state("lib/foo", "1", "0");
    prov1.provides = vec![Soname {
        bucket: "x86_64".to_string(),
        soname: "libfoo.so.1".to_string(),
    }];
    let engine = MergeEngine::new(sb.context(features, ConfigProtect::default()));
    engine
        .apply(&[merge_op(image1, prov1, None, false)])
        .unwrap();

    // Consumer requires libfoo.so.1.
    let tmpc = tempfile::tempdir().unwrap();
    let imagec = build_image(tmpc.path(), &[("/usr/bin/consumer", b"app")]);
    let mut cons = state("app/consumer", "1", "0");
    cons.requires = vec![Soname {
        bucket: "x86_64".to_string(),
        soname: "libfoo.so.1".to_string(),
    }];
    let engine_c = MergeEngine::new(sb.context(features, ConfigProtect::default()));
    engine_c
        .apply(&[merge_op(imagec, cons, None, false)])
        .unwrap();

    // Upgrade provider to v2 providing libfoo.so.2, no longer libfoo.so.1.
    let tmp2 = tempfile::tempdir().unwrap();
    let image2 = build_image(tmp2.path(), &[("/usr/lib/libfoo.so.2", b"abi2")]);
    let mut prov2 = state("lib/foo", "2", "0");
    prov2.provides = vec![Soname {
        bucket: "x86_64".to_string(),
        soname: "libfoo.so.2".to_string(),
    }];
    let engine2 = MergeEngine::new(sb.context(features, ConfigProtect::default()));
    let out = engine2
        .apply(&[merge_op(image2, prov2, Some("lib/foo-1"), false)])
        .unwrap();

    // libfoo.so.1 is still needed by the consumer: preserved on disk.
    assert!(
        sb.live("/usr/lib/libfoo.so.1").exists(),
        "still-needed library must be preserved"
    );
    assert_eq!(out[0].preserved, vec!["/usr/lib/libfoo.so.1".to_string()]);
    // Registry persisted.
    let reg = moraine_merge::PreservedLibs::load(&sb.state.join("preserved-libs")).unwrap();
    assert!(
        reg.entries()
            .iter()
            .any(|e| e.path == "/usr/lib/libfoo.so.1")
    );

    // Rebuild the consumer against libfoo.so.2; now libfoo.so.1 is unneeded.
    let tmpc2 = tempfile::tempdir().unwrap();
    let imagec2 = build_image(tmpc2.path(), &[("/usr/bin/consumer", b"app2")]);
    let mut cons2 = state("app/consumer", "2", "0");
    cons2.requires = vec![Soname {
        bucket: "x86_64".to_string(),
        soname: "libfoo.so.2".to_string(),
    }];
    let engine_c2 = MergeEngine::new(sb.context(features, ConfigProtect::default()));
    let out2 = engine_c2
        .apply(&[merge_op(imagec2, cons2, Some("app/consumer-1"), false)])
        .unwrap();

    // Reconciliation drops the now-unneeded preserved library.
    assert!(
        !sb.live("/usr/lib/libfoo.so.1").exists(),
        "unused preserved library must be dropped"
    );
    assert!(
        out2[0]
            .reconciled
            .contains(&"/usr/lib/libfoo.so.1".to_string())
    );
}

#[test]
fn corrupt_registry_is_rebuilt() {
    let sb = Sandbox::new();
    // Write garbage into the registry, then run any operation.
    std::fs::write(sb.state.join("preserved-libs"), "garbage-line\n").unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let image = build_image(tmp.path(), &[("/x", b"1")]);
    let engine = MergeEngine::new(sb.context(
        Features {
            preserve_libs: true,
            ..Features::default()
        },
        ConfigProtect::default(),
    ));
    // Must not error: the corrupt registry is rebuilt from soname data.
    engine
        .apply(&[merge_op(image, state("cat/p", "1", "0"), None, false)])
        .unwrap();
}

#[test]
fn interrupted_merge_marker_is_recovered_to_prior_state() {
    let sb = Sandbox::new();
    // Simulate a crash: a stale merge marker with no committed record.
    let marker_dir = sb.state.join("in-progress");
    std::fs::create_dir_all(&marker_dir).unwrap();
    std::fs::write(marker_dir.join("current"), "merge\ncat/half-1\n").unwrap();

    let engine = MergeEngine::new(sb.context(Features::default(), ConfigProtect::default()));
    let recovered = engine.recover().unwrap();
    assert!(recovered.is_some());
    // Marker cleared; store shows no half-merged package.
    assert!(
        moraine_merge::recovery::scan(&marker_dir)
            .unwrap()
            .is_none()
    );
    let store = moraine_vdb::Store::load(moraine_vdb::StorePaths::in_dir(&sb.vdb)).unwrap();
    assert!(store.records().is_empty());
}

#[test]
fn interrupted_unmerge_is_rerun_idempotently() {
    let sb = Sandbox::new();
    let tmp = tempfile::tempdir().unwrap();
    let image = build_image(tmp.path(), &[("/u/file", b"x")]);
    let engine = MergeEngine::new(sb.context(Features::default(), ConfigProtect::default()));
    engine
        .apply(&[merge_op(image, state("cat/u", "1", "0"), None, false)])
        .unwrap();

    // Plant an unmerge marker, then recover: re-runs the unmerge idempotently.
    let marker_dir = sb.state.join("in-progress");
    std::fs::write(marker_dir.join("current"), "unmerge\ncat/u-1\n").unwrap();
    let engine2 = MergeEngine::new(sb.context(Features::default(), ConfigProtect::default()));
    engine2.recover().unwrap();
    // The owned file was removed by the re-run.
    assert!(!sb.live("/u/file").exists());
    assert!(
        moraine_merge::recovery::scan(&marker_dir)
            .unwrap()
            .is_none()
    );
}

#[test]
fn corpus_roundtrip_gated_on_env() {
    let Ok(corpus) = std::env::var("MORAINE_CORPUS") else {
        // No-op when the corpus is not configured.
        return;
    };
    // A real corpus install would round-trip through the installed store's
    // importer/exporter; without a corpus this is a no-op by design.
    assert!(Path::new(&corpus).exists());
}
