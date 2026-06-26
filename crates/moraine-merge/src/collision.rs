//! Collision protection: detecting target paths that conflict with existing
//! files before any mutation.
//!
//! Every target path is checked for an existing owner via the installed store.
//! A path owned by a different installed package, or an existing file owned by
//! no installed package, is a collision. A path owned by the version being
//! replaced in the same slot is not a collision. With `collision-protect` any
//! collision aborts before mutation; with `protect-owned` only owned-by-other
//! collisions abort; with neither, collisions are reported and overwritten.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use moraine_common::Interner;
use moraine_vdb::record::PackageRecord;
use moraine_vdb::store::Store;

use crate::Features;
use crate::image::{ImageItem, ImageKind};

/// Why a target path is a collision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollisionKind {
    /// The path is owned by another installed package (its `category/package-version`).
    OwnedByOther(String),
    /// The path exists on the live system but is owned by no installed package.
    Unowned,
    /// A symlink in the image lands on an existing directory in the live root, a
    /// hard collision banned by PMS regardless of FEATURES.
    SymlinkOntoDir,
    /// Two distinct image entries resolve to one real path through a symlinked
    /// parent and carry different content. Names the colliding sibling path.
    Internal(String),
}

/// A detected collision: the conflicting install path and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    /// The conflicting install-root-relative path.
    pub path: String,
    /// The kind of collision.
    pub kind: CollisionKind,
}

/// Find the `category/package-version` of the installed package that owns
/// `install_path`, excluding `exclude_cpv` (the version being replaced).
pub(crate) fn owner_of(
    store: &Store,
    interner: &Interner,
    install_path: &str,
    exclude_cpv: Option<&str>,
) -> Option<String> {
    for record in store.records() {
        if !record.contents.owns(install_path) {
            continue;
        }
        let cpv = record.cpv(interner);
        if Some(cpv.as_str()) == exclude_cpv {
            continue;
        }
        return Some(cpv);
    }
    None
}

/// Detect collisions for the file and symlink entries of a merge.
///
/// Directories are never collisions. `exclude_cpv` is the prior same-slot version
/// whose owned paths are not collisions. A path is a collision when it is owned
/// by another package, when it exists on the live system but is owned by no
/// package, when a symlink lands on an existing directory, or when two image
/// entries resolve to one real path with differing content. Paths matching a
/// `collision_ignore` glob are exempt.
pub(crate) fn detect(
    store: &Store,
    interner: &Interner,
    eroot: &Path,
    items: &[ImageItem],
    exclude_cpv: Option<&str>,
    collision_ignore: &[String],
) -> Vec<Collision> {
    let mut out = Vec::new();
    // Real-path -> the first image entry that resolved to it, for internal
    // collision detection through symlinked parents.
    let mut real_seen: HashMap<PathBuf, ImageItem> = HashMap::new();
    for item in items {
        if matches!(item.kind, ImageKind::Dir) {
            continue;
        }
        let path = &item.install_path;
        if crate::path_matches_any(path, collision_ignore) {
            continue;
        }
        if let Some(owner) = owner_of(store, interner, path, exclude_cpv) {
            out.push(Collision {
                path: path.clone(),
                kind: CollisionKind::OwnedByOther(owner),
            });
            continue;
        }
        // Not owned by any package; a collision only if something is already
        // there. A symlink onto a directory is a hard collision banned by PMS.
        let live = eroot.join(path.trim_start_matches('/'));
        if let Ok(meta) = std::fs::symlink_metadata(&live) {
            if matches!(item.kind, ImageKind::Sym { .. }) && meta.is_dir() {
                out.push(Collision {
                    path: path.clone(),
                    kind: CollisionKind::SymlinkOntoDir,
                });
            } else {
                out.push(Collision {
                    path: path.clone(),
                    kind: CollisionKind::Unowned,
                });
            }
        }
        // Internal collision: a sibling image entry resolving to the same real
        // path with different content.
        let real = real_target(eroot, path);
        if let Some(prev) = real_seen.get(&real) {
            if prev.install_path != *path && entries_differ(prev, item) {
                out.push(Collision {
                    path: path.clone(),
                    kind: CollisionKind::Internal(prev.install_path.clone()),
                });
            }
        } else {
            real_seen.insert(real, item.clone());
        }
    }
    out
}

/// The real destination of `install_path`: the canonical realpath of its parent
/// in the live root (resolving symlinked parents like `/lib64 -> lib`) joined
/// with the basename. Falls back to the lexical path when the parent does not
/// exist yet.
fn real_target(eroot: &Path, install_path: &str) -> PathBuf {
    let live = eroot.join(install_path.trim_start_matches('/'));
    match (live.parent(), live.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(name),
        _ => live,
    }
}

/// Whether two image entries that map to one real path carry different content.
/// Two regular files differ when their bytes differ; a symlink differs from a
/// non-symlink, and two symlinks differ when their targets differ.
fn entries_differ(a: &ImageItem, b: &ImageItem) -> bool {
    match (&a.kind, &b.kind) {
        (ImageKind::File, ImageKind::File) => {
            match (std::fs::read(&a.source), std::fs::read(&b.source)) {
                (Ok(x), Ok(y)) => x != y,
                _ => true,
            }
        }
        (ImageKind::Sym { target: x }, ImageKind::Sym { target: y }) => x != y,
        _ => true,
    }
}

/// Decide, given FEATURES, which detected collisions must abort the merge.
///
/// `collision-protect` aborts on any collision; `protect-owned` aborts only on a
/// collision with a file owned by another package. A symlink-onto-directory is a
/// hard collision banned by PMS and always aborts. Returns the aborting paths.
pub(crate) fn aborting(features: Features, collisions: &[Collision]) -> Vec<String> {
    collisions
        .iter()
        .filter(|c| match c.kind {
            CollisionKind::OwnedByOther(_) => features.collision_protect || features.protect_owned,
            CollisionKind::Unowned | CollisionKind::Internal(_) => features.collision_protect,
            CollisionKind::SymlinkOntoDir => true,
        })
        .map(|c| c.path.clone())
        .collect()
}

/// Whether the record matches `cpv`, used to skip the replaced version.
pub(crate) fn record_is(record: &PackageRecord, interner: &Interner, cpv: &str) -> bool {
    record.cpv(interner) == cpv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aborting_respects_features() {
        let owned = Collision {
            path: "/a".to_string(),
            kind: CollisionKind::OwnedByOther("cat/pkg-1".to_string()),
        };
        let unowned = Collision {
            path: "/b".to_string(),
            kind: CollisionKind::Unowned,
        };
        let all = [owned.clone(), unowned.clone()];

        let none = Features::default();
        assert!(aborting(none, &all).is_empty());

        let protect_owned = Features {
            protect_owned: true,
            ..Features::default()
        };
        assert_eq!(aborting(protect_owned, &all), vec!["/a".to_string()]);

        let collision_protect = Features {
            collision_protect: true,
            ..Features::default()
        };
        assert_eq!(
            aborting(collision_protect, &all),
            vec!["/a".to_string(), "/b".to_string()]
        );
    }
}
