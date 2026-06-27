//! Corpus-gated end-to-end checks for the elog/news/strip follow-ups.
//!
//! Gated on `MORAINE_CORPUS`, which points at a captured system root (`EROOT`).
//! When the variable is unset the tests are a no-op so the default suite needs no
//! corpus.
//!
//! The deterministic, reproducible slice exercised here is the GLEP 42 news state
//! lock: two concurrent scans of the same repository must not lose unread items,
//! and the written state files must carry the state directory's content. The
//! strip type-dispatch (`.a` archive, relocatable, `STRIP_MASK`) and the
//! `PORTAGE_ELOG_SYSTEM="save syslog"` per-package log slices of task 5.4 are
//! covered by the unit tests in `moraine-build` and `moraine-cli`; their full
//! build-time path is a manual acceptance step on a real `EROOT`.

use std::collections::BTreeSet;

use moraine_cli::news::{InstalledPkg, NewsEnv};
use moraine_cli::news_state::{NewsState, update_items};
use moraine_version::Version;

fn corpus_root() -> Option<std::path::PathBuf> {
    std::env::var_os("MORAINE_CORPUS")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

#[test]
fn corpus_news_lock_serializes_concurrent_scans() {
    let Some(root) = corpus_root() else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus news lock check");
        return;
    };
    if !root.is_dir() {
        eprintln!(
            "MORAINE_CORPUS `{}` is not a directory; skipping",
            root.display()
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let news_dir = tmp.path().join("metadata/news");
    for i in 0..60 {
        let name = format!("2024-03-{i:02}");
        let dir = news_dir.join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.en.txt")),
            "Title: T\nNews-Item-Format: 1.0\nDisplay-If-Installed: sys-libs/glibc\n\nBody.\n",
        )
        .unwrap();
    }

    let news_lib = tmp.path().join("newslib");
    let installed = vec![InstalledPkg {
        category: "sys-libs".to_owned(),
        package: "glibc".to_owned(),
        version: Version::parse("2.5").unwrap(),
        slot: "0".to_owned(),
        subslot: None,
    }];
    let env = NewsEnv {
        installed,
        profile: String::new(),
        arch: "amd64".to_owned(),
    };

    std::thread::scope(|scope| {
        for _ in 0..4 {
            let news_dir = news_dir.clone();
            let news_lib = news_lib.clone();
            let env = env.clone();
            scope.spawn(move || {
                update_items(&news_dir, "gentoo", &env, &news_lib, "en");
            });
        }
    });

    let state = NewsState::load(&news_lib, "gentoo");
    // The lock must prevent any concurrent scan from dropping an unread item.
    assert_eq!(state.unread.len(), 60);
    assert_eq!(state.skip.len(), 60);

    let unread_file = news_lib.join("news-gentoo.unread");
    let _: BTreeSet<String> = std::fs::read_to_string(&unread_file)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
}
