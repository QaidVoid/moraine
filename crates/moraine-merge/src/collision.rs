//! Collision protection: detecting target paths that conflict with existing
//! files before any mutation.
//!
//! Every target path is checked for an existing owner via the installed store.
//! A path owned by a different installed package, or an existing file owned by
//! no installed package, is a collision. A path owned by the version being
//! replaced in the same slot is not a collision. With `collision-protect` any
//! collision aborts before mutation; with `protect-owned` only owned-by-other
//! collisions abort; with neither, collisions are reported and overwritten.

use std::path::Path;

use moraine_common::Interner;
use moraine_vdb::record::PackageRecord;
use moraine_vdb::store::Store;

use crate::Features;

/// Why a target path is a collision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollisionKind {
    /// The path is owned by another installed package (its `category/package-version`).
    OwnedByOther(String),
    /// The path exists on the live system but is owned by no installed package.
    Unowned,
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

/// Detect collisions for the file and symlink target paths of a merge.
///
/// `targets` are the install-root-relative file and symlink paths (directories
/// are never collisions). `exclude_cpv` is the prior same-slot version whose
/// owned paths are not collisions. A path is a collision when it is owned by
/// another package, or it exists on the live system but is owned by no package.
pub(crate) fn detect(
    store: &Store,
    interner: &Interner,
    eroot: &Path,
    targets: &[String],
    exclude_cpv: Option<&str>,
) -> Vec<Collision> {
    let mut out = Vec::new();
    for path in targets {
        if let Some(owner) = owner_of(store, interner, path, exclude_cpv) {
            out.push(Collision {
                path: path.clone(),
                kind: CollisionKind::OwnedByOther(owner),
            });
            continue;
        }
        // Not owned by any package; a collision only if a file is already there.
        let live = eroot.join(path.trim_start_matches('/'));
        if std::fs::symlink_metadata(&live).is_ok() {
            out.push(Collision {
                path: path.clone(),
                kind: CollisionKind::Unowned,
            });
        }
    }
    out
}

/// Decide, given FEATURES, which detected collisions must abort the merge.
///
/// `collision-protect` aborts on any collision; `protect-owned` aborts only on a
/// collision with a file owned by another package. Returns the aborting paths.
pub(crate) fn aborting(features: Features, collisions: &[Collision]) -> Vec<String> {
    collisions
        .iter()
        .filter(|c| match c.kind {
            CollisionKind::OwnedByOther(_) => features.collision_protect || features.protect_owned,
            CollisionKind::Unowned => features.collision_protect,
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
