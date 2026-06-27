//! Corpus-gated end-to-end resolution harness.
//!
//! Resolving over real data needs a populated greenfield repository store, an
//! installed store, and the active profile (which sets `USE`, `PYTHON_TARGETS`,
//! and the rest that real `REQUIRED_USE` constraints depend on). This harness
//! runs only when `MORAINE_CORPUS` points at a system root (an `EROOT`) whose
//! `etc/portage/make.profile` resolves; otherwise it no-ops so the default
//! `cargo test` stays hermetic.

use std::path::PathBuf;
use std::sync::Arc;

use moraine_cli::config::{ConfigContext, Roots};
use moraine_common::Interner;
use moraine_config::resolve_config;
use moraine_repo::{build_index_with, discover};
use moraine_resolve::{RealSource, resolve, serialize};
use moraine_vdb::store::{Store, StorePaths};

#[test]
fn resolves_an_atom_with_dependencies_from_corpus() {
    let Some(root) = std::env::var_os("MORAINE_CORPUS").filter(|v| !v.is_empty()) else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus resolution harness");
        return;
    };
    let root = PathBuf::from(root);
    let repos_conf = root.join("etc/portage/repos.conf");
    let vdb_dir = root.join("var/db/pkg");
    // The profile must resolve, since a real atom's REQUIRED_USE (for example
    // sys-apps/portage needing a python target) is only satisfiable once the
    // profile's USE is applied.
    if !repos_conf.exists() || !vdb_dir.is_dir() || !root.join("etc/portage/make.profile").exists()
    {
        eprintln!("corpus missing repos.conf, var/db/pkg, or a resolved make.profile; skipping");
        return;
    }
    let atom =
        std::env::var("MORAINE_CORPUS_ATOM").unwrap_or_else(|_| "sys-apps/portage".to_owned());

    let interner = Arc::new(Interner::new());
    // Build the greenfield store into a temporary directory (it is an output,
    // not part of the captured corpus).
    let store = tempfile::tempdir().expect("temp store dir");
    let repo_index = build_index_with(&repos_conf, store.path(), Some(Arc::clone(&interner)))
        .expect("build the repository index from the corpus");

    let vdb = Store::load(StorePaths::in_dir(vdb_dir)).expect("load the corpus installed store");

    // Load the real configuration from the corpus: the profile stack, make.conf,
    // and the @system/@world selections, exactly as the install path does.
    let roots = Roots {
        root: Some(root.clone()),
        config_root: Some(root.clone()),
        profile: None,
    };
    let ctx = ConfigContext::load(&roots).expect("corpus config loads");
    let repo_set = discover(&repos_conf).expect("discover the corpus repositories");
    let repo_masks = moraine_cli::config::repo_mask_inputs(&repo_set);
    let config = resolve_config(
        &ctx.profile,
        &ctx.vars,
        &root,
        &repo_masks,
        ctx.system.clone(),
        ctx.world.clone(),
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
