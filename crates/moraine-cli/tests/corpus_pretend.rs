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

fn corpus_root() -> Option<PathBuf> {
    let value = std::env::var_os("MORAINE_CORPUS")?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

/// Snapshot the mtimes of every file under a directory, for the read-only check.
fn snapshot_mtimes(root: &Path) -> BTreeMap<PathBuf, std::time::SystemTime> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
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

    let cli = Cli::parse_from_args(
        [
            "-puDN",
            "--pretend",
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
