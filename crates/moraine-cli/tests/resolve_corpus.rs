//! Corpus-gated end-to-end resolution harness.
//!
//! Resolving over real data needs a populated greenfield repository store and an
//! installed store, which only exist on a real system. This harness runs only
//! when `MORAINE_CORPUS` points at a system root (an `EROOT`), mirroring the
//! gating the resolver's own corpus tests use. With the variable unset it is a
//! no-op so the default `cargo test` stays hermetic.

use std::path::PathBuf;
use std::sync::Arc;

use moraine_common::Interner;
use moraine_config::resolve_config;
use moraine_repo::{build_index_with, discover};
use moraine_resolve::{RealSource, resolve, serialize};
use moraine_vdb::store::{Store, StorePaths};

#[test]
fn resolves_an_atom_with_dependencies_from_corpus() {
    let Ok(root) = std::env::var("MORAINE_CORPUS") else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus resolution harness");
        return;
    };
    let root = PathBuf::from(root);
    let atom =
        std::env::var("MORAINE_CORPUS_ATOM").unwrap_or_else(|_| "sys-apps/portage".to_owned());

    let interner = Arc::new(Interner::new());
    let repos_conf = root.join("etc/portage/repos.conf");
    let store_dir = root.join("var/cache/moraine/repos");
    let repo_index = build_index_with(&repos_conf, &store_dir, Some(Arc::clone(&interner)))
        .expect("build the repository index from the corpus");
    let repo_set = discover(&repos_conf).expect("discover the corpus repositories");
    let _ = &repo_set;
    let vdb = Store::load(StorePaths::in_dir(root.join("var/db/pkg")))
        .expect("load the corpus installed store");

    // A minimal resolved configuration is enough to exercise the path; a full one
    // would read the corpus profile, which the loader supports.
    let repo_masks = moraine_cli::config::repo_mask_inputs(&repo_set);
    let config = resolve_config(
        &Default::default(),
        &Default::default(),
        &root,
        &repo_masks,
        Vec::new(),
        Vec::new(),
        &interner,
    );

    let source = RealSource::new(&repo_index, &vdb, &config);
    let solution = resolve(&source, &[atom.as_str()]).expect("resolve the corpus atom");
    let order = serialize(&solution).expect("serialize the merge order");
    assert!(
        !order.is_empty(),
        "resolution should produce at least one task"
    );
}
