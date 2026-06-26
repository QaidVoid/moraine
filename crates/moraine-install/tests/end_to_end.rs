//! Hermetic end-to-end transaction tests through the real merge engine.
//!
//! These install a binary package into a tempdir root, then unmerge it, driving
//! the orchestrator with the real [`EngineApplier`] (not a fake) so the whole
//! write path is exercised: unpack, merge into the live root, record installed
//! state, and unmerge. They need no external corpus.

use moraine_binpkg::MetadataMap;
use moraine_binpkg::greenfield::{WriteOptions, write_bytes};
use moraine_install::{
    BinpkgRunner, EngineApplier, InstallTask, LocalPkgdir, SourceKind, Transaction,
    TransactionEngine,
};
use moraine_merge::{ConfigProtect, Features, MergeContext};

/// Build a binary package whose image contains a single file.
fn binpkg(rel: &str, body: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, rel, body).unwrap();
    let image = builder.into_inner().unwrap();

    let mut meta = MetadataMap::new();
    meta.set_str("EAPI", "8");
    meta.set_str("SLOT", "0");
    meta.set_str("USE", "");
    write_bytes(&meta, &image, &WriteOptions::default()).unwrap()
}

fn context(root: &std::path::Path) -> MergeContext {
    MergeContext {
        eroot: root.to_path_buf(),
        vdb_dir: root.join("var/db/pkg"),
        state_dir: root.join("var/lib/portage"),
        features: Features::default(),
        config_protect: ConfigProtect::new(Vec::new(), Vec::new()),
        collision_ignore: Vec::new(),
        uninstall_ignore: Vec::new(),
        install_mask: Default::default(),
    }
}

#[test]
fn install_then_unmerge_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let state_dir = root.join("var/lib/portage");
    std::fs::create_dir_all(root.join("var/db/pkg")).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();

    // Stage a binary package under PKGDIR.
    let pkgdir = root.join("var/cache/binpkgs");
    std::fs::create_dir_all(pkgdir.join("app-misc")).unwrap();
    std::fs::write(
        pkgdir.join("app-misc/hello-1.0.gpkg"),
        binpkg("usr/bin/hello", b"#!/bin/sh\necho hi\n"),
    )
    .unwrap();

    // Install through the orchestrator and the real merge engine.
    let runner = BinpkgRunner::new(
        LocalPkgdir {
            pkgdir: pkgdir.clone(),
        },
        state_dir.join("stage"),
    );
    let applier = EngineApplier::new(context(root));
    let engine = TransactionEngine::new(&runner, &applier, &state_dir);

    let mut task = InstallTask::merge("app-misc/hello-1.0", "app-misc/hello", "0");
    task.source = SourceKind::Binary;
    task.in_world = true;
    let report = engine.run(&Transaction::new(vec![task])).unwrap();
    assert_eq!(report.applied.len(), 1);

    // The file is merged into the live root and the journal is cleared.
    assert!(root.join("usr/bin/hello").exists());
    assert!(!moraine_install::has_pending(&state_dir));

    // Now unmerge it.
    let uninstall = InstallTask::uninstall("app-misc/hello-1.0", "app-misc/hello", "0");
    engine.run(&Transaction::new(vec![uninstall])).unwrap();
    assert!(!root.join("usr/bin/hello").exists());
}
