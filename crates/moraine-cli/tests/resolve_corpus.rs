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

    // Two-slot collapse regression: whenever the solution co-installs more than
    // one slot of a single cp, every one of those slots must reach the serialized
    // plan as its own merge task rather than collapsing to one.
    use std::collections::BTreeMap;
    let mut slots_per_cp: BTreeMap<&str, std::collections::BTreeSet<&str>> = BTreeMap::new();
    for p in &solution.packages {
        slots_per_cp
            .entry(p.cp.as_str())
            .or_default()
            .insert(p.slot.as_str());
    }
    for (cp, slots) in slots_per_cp.iter().filter(|(_, s)| s.len() > 1) {
        let tasks_for_cp = order.iter().filter(|t| &t.cp == cp).count();
        assert_eq!(
            tasks_for_cp,
            slots.len(),
            "every co-installed slot of {cp} must produce its own merge task (slots {slots:?})"
        );
    }
}

/// Proves that an extended-wildcard `package.accept_keywords` line and a
/// `package.env` override take effect when the configuration is loaded against
/// real corpus profile data. The repository tree and profile come from the
/// corpus; the `/etc/portage` overrides are written into a temporary config root
/// layered over it. Skips cleanly when `MORAINE_CORPUS` is unset.
#[test]
fn extended_wildcard_acceptance_and_package_env_take_effect() {
    let Some(corpus) = std::env::var_os("MORAINE_CORPUS").filter(|v| !v.is_empty()) else {
        eprintln!("MORAINE_CORPUS unset; skipping extended-wildcard corpus harness");
        return;
    };
    let corpus = PathBuf::from(corpus);
    let make_profile = corpus.join("etc/portage/make.profile");
    if !corpus.join("etc/portage/repos.conf").exists() || !make_profile.exists() {
        eprintln!("corpus missing repos.conf or a resolved make.profile; skipping");
        return;
    }

    // A temporary config root that layers the wildcard acceptance and per-package
    // env over the corpus, reusing the corpus profile and repositories.
    let config_root = tempfile::tempdir().expect("temp config root");
    let portage = config_root.path().join("etc/portage");
    std::fs::create_dir_all(portage.join("env")).unwrap();
    // The corpus make.profile and repos.conf are symlinked so the real profile
    // stack and repositories resolve. repos.conf may be a single file or a
    // repos.conf.d directory, and a symlink handles both.
    std::os::unix::fs::symlink(&make_profile, portage.join("make.profile")).unwrap();
    std::os::unix::fs::symlink(
        corpus.join("etc/portage/repos.conf"),
        portage.join("repos.conf"),
    )
    .unwrap();
    std::fs::write(portage.join("package.accept_keywords"), "*/* ~amd64\n").unwrap();
    std::fs::write(portage.join("package.env"), "*/* lowopt.conf\n").unwrap();
    std::fs::write(portage.join("env/lowopt.conf"), "CFLAGS=\"-O1\"\n").unwrap();

    let interner = Arc::new(Interner::new());
    let roots = Roots {
        root: Some(corpus.clone()),
        config_root: Some(config_root.path().to_path_buf()),
        profile: None,
    };
    let ctx = ConfigContext::load(&roots).expect("corpus config loads");
    let repo_set = discover(portage.join("repos.conf")).expect("discover repositories");
    let repo_masks = moraine_cli::config::repo_mask_inputs(&repo_set);
    let config = resolve_config(
        &ctx.profile,
        &ctx.vars,
        config_root.path(),
        &repo_masks,
        ctx.system.clone(),
        ctx.world.clone(),
        &interner,
    );

    // The `*/*` lines match any candidate, so an arbitrary package reference
    // built against the shared interner exercises both overrides.
    let version = moraine_version::Version::parse("1.0").unwrap();
    let pref = moraine_atom::PackageRef {
        category: interner.intern("dev-libs"),
        package: interner.intern("anything"),
        version: &version,
        slot: Some(interner.intern("0")),
        subslot: None,
        repo: None,
    };

    // The extended-wildcard accept_keywords line grants the testing keyword.
    let extra = config.package_keywords(&pref);
    assert!(
        extra.iter().any(|k| k == "~amd64"),
        "`*/* ~amd64` should grant ~amd64 to every candidate, got {extra:?}"
    );

    // The extended-wildcard package.env line supplies the per-package CFLAGS.
    let overlay = config.package_env_overlay(&pref);
    assert!(
        overlay.contains(&("CFLAGS".to_owned(), "-O1".to_owned())),
        "`*/* lowopt.conf` should set CFLAGS=-O1, got {overlay:?}"
    );
}
