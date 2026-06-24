//! CONTENTS computation during placement.
//!
//! A CONTENTS record is produced for every installed path using the three stock
//! record kinds: `dir`, `obj` (path, md5, mtime), and `sym` (path, target,
//! mtime). The md5 of an `obj` record is computed from the bytes actually
//! written to the live file and the mtime is the post-placement mtime of the
//! placed file, so ownership, unmerge, and modification detection are exact.

use std::path::Path;

use moraine_vdb::contents::{Entry, EntryKind};

use crate::error::{IoResultExt as _, MergeError};

/// Compute the lowercase hex md5 digest of `data`.
///
/// md5 is retained for stock-vdb import compatibility and modification
/// detection, matching the digest recorded in stock CONTENTS files.
pub fn compute_md5(data: &[u8]) -> String {
    moraine_common::hash::md5(data)
}

/// The post-placement mtime of `path` as whole seconds since the epoch.
pub(crate) fn mtime_secs(path: &Path) -> Result<i64, MergeError> {
    let meta = std::fs::symlink_metadata(path).with_path(path)?;
    let modified = meta.modified().with_path(path)?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(secs)
}

/// Build an `obj` CONTENTS entry from a placed file's bytes and live path.
pub(crate) fn obj_entry(
    install_path: &str,
    bytes: &[u8],
    live: &Path,
) -> Result<Entry, MergeError> {
    Ok(Entry {
        path: install_path.to_string(),
        kind: EntryKind::Obj {
            md5: compute_md5(bytes),
            mtime: mtime_secs(live)?,
        },
    })
}

/// Build a `sym` CONTENTS entry from a placed symlink's target and live path.
pub(crate) fn sym_entry(
    install_path: &str,
    target: &str,
    live: &Path,
) -> Result<Entry, MergeError> {
    Ok(Entry {
        path: install_path.to_string(),
        kind: EntryKind::Sym {
            target: target.to_string(),
            mtime: mtime_secs(live)?,
        },
    })
}

/// Build a `dir` CONTENTS entry for `install_path`.
pub(crate) fn dir_entry(install_path: &str) -> Entry {
    Entry {
        path: install_path.to_string(),
        kind: EntryKind::Dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_known_vector() {
        assert_eq!(compute_md5(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn obj_entry_carries_md5_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"abc").unwrap();
        let e = obj_entry("/f", b"abc", &p).unwrap();
        match e.kind {
            EntryKind::Obj { md5, mtime } => {
                assert_eq!(md5, "900150983cd24fb0d6963f7d28e17f72");
                assert!(mtime > 0);
            }
            _ => panic!("expected obj"),
        }
    }
}
