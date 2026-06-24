//! Property tests for CONTENTS implicit-parent synthesis and ownership lookup.

use moraine_vdb::contents::{Contents, Entry, EntryKind};
use proptest::prelude::*;

/// A path component made of safe characters and never empty.
fn component() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9]{0,5}"
}

/// An absolute path of one to five components.
fn abs_path() -> impl Strategy<Value = String> {
    prop::collection::vec(component(), 1..=5).prop_map(|parts| format!("/{}", parts.join("/")))
}

proptest! {
    #[test]
    fn every_ancestor_is_a_directory(path in abs_path()) {
        let entry = Entry {
            path: path.clone(),
            kind: EntryKind::Obj { md5: "0".repeat(32), mtime: 1 },
        };
        let contents = Contents::from_entries([entry]);

        // Every proper ancestor prefix must be present as a directory.
        let trimmed = path.trim_end_matches('/');
        let mut acc = String::new();
        for part in trimmed.split('/').filter(|p| !p.is_empty()) {
            acc.push('/');
            acc.push_str(part);
            if acc != trimmed {
                let is_dir = matches!(contents.owner(&acc), Some(EntryKind::Dir));
                prop_assert!(is_dir);
            }
        }
        // The path itself is owned as the object.
        let is_obj = matches!(contents.owner(trimmed), Some(EntryKind::Obj { .. }));
        prop_assert!(is_obj);
    }

    #[test]
    fn explicit_dir_is_never_duplicated(paths in prop::collection::vec(abs_path(), 1..8)) {
        let entries: Vec<Entry> = paths
            .iter()
            .map(|p| Entry { path: p.clone(), kind: EntryKind::Dir })
            .collect();
        let contents = Contents::from_entries(entries);

        // No path appears twice: iterating yields unique paths.
        let mut seen = std::collections::HashSet::new();
        for e in contents.iter() {
            prop_assert!(seen.insert(e.path), "duplicate path in contents");
        }
    }

    #[test]
    fn unrecorded_path_is_not_owned(
        recorded in abs_path(),
        other in abs_path(),
    ) {
        let contents = Contents::from_entries([Entry {
            path: recorded.clone(),
            kind: EntryKind::Obj { md5: "0".repeat(32), mtime: 1 },
        }]);

        // A path that is neither the recorded path nor any of its ancestors is
        // not owned.
        let trimmed = recorded.trim_end_matches('/');
        let is_ancestor_or_self = trimmed == other.trim_end_matches('/')
            || trimmed.starts_with(&format!("{}/", other.trim_end_matches('/')));
        if !is_ancestor_or_self {
            prop_assert!(!contents.owns(other.trim_end_matches('/')));
        }
    }
}
