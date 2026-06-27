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
use crate::protect::ConfigProtect;

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
/// `collision_ignore` glob are exempt. A config-protected path is treated as
/// owned and is never an owned-by-other or unowned collision, matching Portage's
/// `_collision_protect` exemption; the symlink-onto-directory ban and internal
/// collisions still apply.
pub(crate) fn detect(
    store: &Store,
    interner: &Interner,
    eroot: &Path,
    items: &[ImageItem],
    exclude_cpv: Option<&str>,
    collision_ignore: &[String],
    config_protect: &ConfigProtect,
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
        // A config-protected path is treated as owned: its content is resolved by
        // the `._cfg` variant logic during placement, so it is exempt from the
        // ownership-based collision check.
        let protected = config_protect.is_protected(path);
        if !protected && let Some(owner) = owner_of(store, interner, path, exclude_cpv) {
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
            } else if !protected {
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
    use std::sync::Arc;

    use moraine_vdb::contents::Contents;
    use moraine_vdb::record::{DependSet, Slot, Toolchain};
    use moraine_vdb::soname::{Provides, Requires};
    use moraine_vdb::store::StorePaths;

    use super::*;
    use crate::ConfigProtect;

    /// A record owning a single `obj` path, for ownership-collision tests.
    fn owned_record(interner: &Interner, cpv: &str, path: &str) -> PackageRecord {
        let (cp, version) = cpv.rsplit_once('-').unwrap();
        let (category, package) = cp.split_once('/').unwrap();
        let contents = Contents::from_entries([moraine_vdb::contents::Entry {
            path: path.to_string(),
            kind: moraine_vdb::contents::EntryKind::Obj {
                md5: "d41d8cd98f00b204e9800998ecf8427e".to_string(),
                mtime: 1,
            },
        }]);
        PackageRecord {
            category: interner.intern(category),
            package: interner.intern(package),
            version: moraine_version::Version::parse(version).unwrap(),
            eapi: "8".to_string(),
            slot: Slot {
                slot: interner.intern("0"),
                subslot: None,
            },
            use_flags: Vec::new(),
            iuse: Vec::new(),
            depends: DependSet::default(),
            keywords: Vec::new(),
            license: String::new(),
            description: String::new(),
            homepage: String::new(),
            properties: String::new(),
            restrict: String::new(),
            repository: None,
            defined_phases: Vec::new(),
            build_time: None,
            build_id: None,
            counter: 0,
            chost: String::new(),
            provides: Provides {
                entries: Vec::new(),
            },
            requires: Requires {
                entries: Vec::new(),
            },
            contents,
            environment: None,
            inherited: Vec::new(),
            features: Vec::new(),
            size: None,
            needed: Vec::new(),
            toolchain: Toolchain::default(),
            dbdir_mtime: 0,
        }
    }

    fn file_item(install_path: &str, source: &Path) -> ImageItem {
        ImageItem {
            install_path: install_path.to_string(),
            source: source.to_path_buf(),
            kind: ImageKind::File,
            mode: 0o644,
            uid: 0,
            gid: 0,
            rdev: 0,
            dev: 0,
            ino: 0,
            nlink: 1,
            mtime: 0,
            mtime_nsec: 0,
            xattrs: Vec::new(),
        }
    }

    #[test]
    fn config_protected_paths_are_exempt_from_collision() {
        let dir = tempfile::tempdir().unwrap();
        let eroot = dir.path().join("eroot");
        std::fs::create_dir_all(eroot.join("etc")).unwrap();
        // A protected path that already exists unowned on the live system.
        std::fs::write(eroot.join("etc/unowned.conf"), b"live").unwrap();
        // A protected path owned by another installed package.
        std::fs::write(eroot.join("etc/owned.conf"), b"live").unwrap();

        let interner = Interner::new();
        let owner = owned_record(&interner, "cat/other-1", "/etc/owned.conf");
        let store = Store::from_records(
            StorePaths::in_dir(dir.path().join("vdb")),
            Arc::new(interner),
            vec![owner],
        );
        let store_interner = store.interner().clone();

        let src = dir.path().join("src");
        std::fs::write(&src, b"new").unwrap();
        let items = vec![
            file_item("/etc/unowned.conf", &src),
            file_item("/etc/owned.conf", &src),
        ];

        // Without protection both are collisions (unowned and owned-by-other).
        let bare = ConfigProtect::default();
        let unprotected = detect(&store, &store_interner, &eroot, &items, None, &[], &bare);
        assert_eq!(unprotected.len(), 2, "both paths collide when unprotected");

        // With /etc protected neither is reported as a collision.
        let cp = ConfigProtect::new(["/etc".to_string()], std::iter::empty());
        let protected = detect(&store, &store_interner, &eroot, &items, None, &[], &cp);
        assert!(
            protected.is_empty(),
            "config-protected paths must be exempt: {protected:?}"
        );
    }

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
