//! End-to-end `--pretend` tests over a real corpus.
//!
//! These are gated on the `MORAINE_CORPUS` environment variable, which must
//! point at a captured Gentoo system root (config, repository store, installed
//! store). When the variable is unset they no-op so the default `cargo test`
//! run stays hermetic.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use moraine_cli::args::Cli;
use moraine_cli::config::{ConfigContext, Roots};
use moraine_cli::sets::{Modifiers, expand};

/// The corpus system root, when configured and complete enough to resolve
/// `@world`: it must hold `etc/portage` and a `make.profile` (the profile makes
/// `@system`/`@world` meaningful). An incomplete corpus skips rather than fails.
fn corpus_root() -> Option<PathBuf> {
    let value = std::env::var_os("MORAINE_CORPUS").filter(|v| !v.is_empty())?;
    let root = PathBuf::from(value);
    if !root.join("etc/portage").is_dir() {
        eprintln!("corpus has no etc/portage; skipping");
        return None;
    }
    // `exists()` follows the symlink, so a make.profile pointing outside the
    // corpus (a broken link) skips rather than resolving to an empty profile.
    if !root.join("etc/portage/make.profile").exists() {
        eprintln!("corpus make.profile is absent or unresolved; skipping @world tests");
        return None;
    }
    Some(root)
}

/// Snapshot the mtimes of every file under a directory, for the read-only check.
///
/// Moraine's own derived store cache under `var/cache/moraine` is excluded: a
/// `--pretend` run may build that cache (as Portage builds its metadata cache),
/// which is not a modification of the installed system's persisted state.
fn snapshot_mtimes(root: &Path) -> BTreeMap<PathBuf, std::time::SystemTime> {
    let cache = root.join("var/cache/moraine");
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir == cache {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(meta) = entry.metadata()
                && let Ok(modified) = meta.modified()
            {
                out.insert(path, modified);
            }
        }
    }
    out
}

#[test]
fn pretend_world_expands_over_corpus() {
    let Some(root) = corpus_root() else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus @world expansion");
        return;
    };

    let roots = Roots {
        root: Some(root.clone()),
        config_root: Some(root.clone()),
        profile: None,
    };
    let ctx = ConfigContext::load(&roots).expect("corpus config loads");
    let request = expand(
        &ctx,
        &["@world".to_owned()],
        &[],
        Modifiers {
            update: true,
            deep: true,
            newuse: true,
            oneshot: false,
        },
    )
    .expect("@world expands");
    assert!(
        !request.atoms.is_empty(),
        "expected @world to expand to at least one atom"
    );
}

#[test]
fn pretend_world_leaves_state_unchanged() {
    let Some(root) = corpus_root() else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus read-only invariant");
        return;
    };

    let before = snapshot_mtimes(&root);

    // `-p` already requests pretend; do not also pass `--pretend` (that is the
    // same flag twice and clap rejects the conflict).
    let cli = Cli::parse_from_args(
        [
            "-puDN",
            "--root",
            root.to_str().unwrap(),
            "--config-root",
            root.to_str().unwrap(),
            "@world",
        ]
        .map(String::from),
    )
    .expect("parses");
    let _ = moraine_cli::dispatch(&cli);

    let after = snapshot_mtimes(&root);
    assert_eq!(
        before, after,
        "a --pretend run must not modify any persisted state"
    );
}
