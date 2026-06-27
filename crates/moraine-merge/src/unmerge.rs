//! Safe removal of a package's CONTENTS.
//!
//! Unmerge walks the package's recorded CONTENTS in reverse depth order and
//! removes each path only when it is still safe: a regular file only when it
//! still exists, is still owned by this package, and its md5 and mtime match
//! CONTENTS; a symlink only when its live mtime still matches CONTENTS, and never
//! a critical library-directory symlink. A path now owned by another package, a
//! modified file, and a protected config are skipped and left in place. A
//! directory is removed only when empty after its entries are processed. A
//! still-needed shared library is deferred to the preserve-libs reconciliation.

use moraine_common::Interner;
use moraine_vdb::contents::EntryKind;
use moraine_vdb::store::Store;

use std::collections::HashMap;

use crate::collision;
use crate::contents::{compute_md5, mtime_secs};
use crate::error::MergeError;
use crate::preserve::{self, PreservedEntry};
use crate::{MergeContext, dir_entry_names};

/// The outcome of an unmerge walk: which paths were removed and which were
/// skipped, for reporting.
#[derive(Debug, Default)]
pub(crate) struct UnmergeResult {
    /// Paths removed from the live system.
    pub removed: Vec<String>,
    /// Paths skipped and left in place.
    pub skipped: Vec<String>,
    /// Still-needed shared libraries deferred to preserve-libs, to be registered
    /// in the durable registry before the package record is removed.
    pub preserved: Vec<PreservedEntry>,
}

/// What the unmerge walk should do with a recorded `obj` entry.
enum ObjAction {
    /// Remove the file from the live system.
    Remove,
    /// Leave the file in place for an ordinary reason (protected, foreign-owned,
    /// modified, or already gone).
    Skip,
    /// Defer the file to preserve-libs reconciliation, carrying its recorded
    /// soname so the standalone-unmerge path can register it.
    Preserve(String),
}

/// Walk and remove the CONTENTS of `cpv` from the live root.
///
/// The record for `cpv` is read from `store`. Ownership and modification guards
/// are applied per path. This is idempotent: re-running removes only paths still
/// owned and matching, so an interrupted unmerge recovers by re-run.
pub(crate) fn unmerge(
    ctx: &MergeContext,
    store: &Store,
    interner: &Interner,
    cpv: &str,
) -> Result<UnmergeResult, MergeError> {
    let span = tracing::info_span!("unmerge", pkg = %cpv);
    let _enter = span.enter();

    let mut result = UnmergeResult::default();

    let Some(record) = store
        .records()
        .iter()
        .find(|r| collision::record_is(r, interner, cpv))
    else {
        return Ok(result);
    };

    // Per-object soname map so a still-needed library can be matched by its
    // recorded `NEEDED.ELF.2` linkage and deferred.
    let soname_map = preserve::needed_soname_map(&record.needed);

    // Walk in reverse depth order so deeper paths precede their parents.
    let mut entries: Vec<_> = record.contents.iter().collect();
    entries.sort_by(|a, b| b.path.cmp(&a.path));

    for entry in entries {
        let live = ctx.live_path(&entry.path);
        // UNINSTALL_IGNORE: never remove a path matching an ignore glob.
        if crate::path_matches_any(&entry.path, &ctx.uninstall_ignore) {
            result.skipped.push(entry.path.clone());
            continue;
        }
        match &entry.kind {
            EntryKind::Dir => {
                // A directory is removed only when empty after its entries are
                // processed (we walk depth-first, so children came first).
                if live.is_dir() && dir_entry_names(&live).is_empty() {
                    match std::fs::remove_dir(&live) {
                        Ok(()) => result.removed.push(entry.path.clone()),
                        Err(_) => result.skipped.push(entry.path.clone()),
                    }
                } else {
                    result.skipped.push(entry.path.clone());
                }
            }
            EntryKind::Obj { md5, mtime } => {
                match classify_obj(
                    ctx,
                    store,
                    interner,
                    cpv,
                    &entry.path,
                    &soname_map,
                    md5,
                    *mtime,
                )? {
                    ObjAction::Skip => {
                        result.skipped.push(entry.path.clone());
                        continue;
                    }
                    ObjAction::Preserve(soname) => {
                        result.skipped.push(entry.path.clone());
                        result.preserved.push(PreservedEntry {
                            cpv: cpv.to_string(),
                            soname,
                            path: entry.path.clone(),
                        });
                        continue;
                    }
                    ObjAction::Remove => {}
                }
                // Neutralize any outstanding hardlink to a suid/sgid file by
                // stripping its mode before unlinking, matching Portage.
                neutralize_mode(&live);
                match std::fs::remove_file(&live) {
                    Ok(()) => result.removed.push(entry.path.clone()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        result.skipped.push(entry.path.clone());
                    }
                    Err(source) => return Err(MergeError::Io { path: live, source }),
                }
            }
            EntryKind::Sym { mtime, .. } => {
                // Never remove a critical library-directory symlink even when its
                // recorded target still matches (bug #423127).
                if LIBDIR_SYMLINKS.contains(&entry.path.as_str()) {
                    result.skipped.push(entry.path.clone());
                    continue;
                }
                if should_skip_sym(ctx, store, interner, cpv, &entry.path, *mtime)? {
                    result.skipped.push(entry.path.clone());
                    continue;
                }
                // Preserve a symlink-to-directory while another package owns a
                // path reachable through it (bug #326685).
                if live.is_dir() && other_owns_through(store, interner, &entry.path, cpv) {
                    result.skipped.push(entry.path.clone());
                    continue;
                }
                match std::fs::remove_file(&live) {
                    Ok(()) => result.removed.push(entry.path.clone()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        result.skipped.push(entry.path.clone());
                    }
                    Err(source) => return Err(MergeError::Io { path: live, source }),
                }
            }
            EntryKind::Fif | EntryKind::Dev => {
                // Special files carry no content to verify; remove unless config
                // protected or now owned by another package.
                if ctx.config_protect.is_protected(&entry.path)
                    || collision::owner_of(store, interner, &ctx.eroot, &entry.path, Some(cpv))
                        .is_some()
                {
                    result.skipped.push(entry.path.clone());
                    continue;
                }
                match std::fs::remove_file(&live) {
                    Ok(()) => result.removed.push(entry.path.clone()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        result.skipped.push(entry.path.clone());
                    }
                    Err(source) => return Err(MergeError::Io { path: live, source }),
                }
            }
        }
    }

    Ok(result)
}

/// Strip a regular file's mode to zero before unlink so an outstanding hardlink
/// to a suid/sgid file is rendered harmless. Best effort: a symlink is skipped
/// and an unprivileged failure is ignored.
fn neutralize_mode(live: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Ok(meta) = std::fs::symlink_metadata(live)
        && meta.file_type().is_file()
    {
        let _ = std::fs::set_permissions(live, std::fs::Permissions::from_mode(0o0));
    }
}

/// Whether an installed package other than `cpv` owns any path reachable through
/// the directory symlink at `sym_path` (a path under `sym_path/`).
fn other_owns_through(store: &Store, interner: &Interner, sym_path: &str, cpv: &str) -> bool {
    let prefix = format!("{sym_path}/");
    for record in store.records() {
        if record.cpv(interner) == cpv {
            continue;
        }
        if record.contents.iter().any(|e| e.path.starts_with(&prefix)) {
            return true;
        }
    }
    false
}

/// Classify a recorded `obj` entry: remove it, skip it, or defer it to
/// preserve-libs. The config-protect, foreign-ownership, and preserve-libs guards
/// run first; `unmerge-orphans` then bypasses the md5/mtime gate for a genuine
/// file the package still owns.
#[allow(clippy::too_many_arguments)]
fn classify_obj(
    ctx: &MergeContext,
    store: &Store,
    interner: &Interner,
    cpv: &str,
    path: &str,
    soname_map: &HashMap<String, String>,
    md5: &str,
    mtime: i64,
) -> Result<ObjAction, MergeError> {
    // Protected configs are never removed on unmerge.
    if ctx.config_protect.is_protected(path) {
        return Ok(ObjAction::Skip);
    }
    // Now owned by another package: skip.
    if collision::owner_of(store, interner, &ctx.eroot, path, Some(cpv)).is_some() {
        return Ok(ObjAction::Skip);
    }
    // Defer a still-needed shared library to preserve-libs reconciliation,
    // matching the library to its soname by recorded `NEEDED.ELF.2` linkage.
    if ctx.features.preserve_libs
        && let Some(soname) = soname_map.get(path)
        && preserve::soname_still_needed(store, interner, soname, Some(cpv))
    {
        return Ok(ObjAction::Preserve(soname.clone()));
    }
    // An empty recorded md5 is a preserved-library placeholder: never remove it.
    if md5.is_empty() {
        return Ok(ObjAction::Skip);
    }
    // unmerge-orphans: a non-protected, non-foreign, non-preserved file the
    // package still owns is unlinked regardless of md5/mtime drift.
    if ctx.features.unmerge_orphans {
        return Ok(ObjAction::Remove);
    }
    let live = ctx.live_path(path);
    let Ok(bytes) = std::fs::read(&live) else {
        // Gone already or unreadable: nothing to remove.
        return Ok(ObjAction::Skip);
    };
    // A modified file (md5 or mtime mismatch) is skipped.
    if compute_md5(&bytes) != md5 {
        return Ok(ObjAction::Skip);
    }
    if mtime_secs(&live)? != mtime {
        return Ok(ObjAction::Skip);
    }
    Ok(ObjAction::Remove)
}

/// EROOT-relative symlink paths that are never removed on unmerge (bug #423127).
const LIBDIR_SYMLINKS: [&str; 3] = ["/lib", "/usr/lib", "/usr/local/lib"];

/// Whether a recorded symlink at `path` should be left in place rather than
/// removed. Config-protected and now-foreign-owned symlinks are preserved, then
/// a symlink whose live lstat mtime differs from the recorded mtime is treated as
/// administrator-modified and preserved. Mirrors Portage, which decides this
/// solely by mtime and never compares the live link target (vartree.py:2966).
fn should_skip_sym(
    ctx: &MergeContext,
    store: &Store,
    interner: &Interner,
    cpv: &str,
    path: &str,
    mtime: i64,
) -> Result<bool, MergeError> {
    if ctx.config_protect.is_protected(path) {
        return Ok(true);
    }
    if collision::owner_of(store, interner, &ctx.eroot, path, Some(cpv)).is_some() {
        return Ok(true);
    }
    let live = ctx.live_path(path);
    match mtime_secs(&live) {
        Ok(live_mtime) => Ok(live_mtime != mtime),
        // Gone already or unreadable: nothing to remove, so leave it.
        Err(_) => Ok(true),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moraine_vdb::store::StorePaths;

    use super::*;
    use crate::{ConfigProtect, Features};

    fn empty_ctx(eroot: &std::path::Path) -> MergeContext {
        MergeContext {
            eroot: eroot.to_path_buf(),
            vdb_dir: eroot.join("vdb"),
            state_dir: eroot.join("state"),
            features: Features::default(),
            config_protect: ConfigProtect::default(),
            collision_ignore: Vec::new(),
            uninstall_ignore: Vec::new(),
            install_mask: Default::default(),
        }
    }

    #[test]
    fn sym_skip_decided_by_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let eroot = dir.path();
        std::os::unix::fs::symlink("target", eroot.join("link")).unwrap();
        let recorded = mtime_secs(&eroot.join("link")).unwrap();

        let interner = Arc::new(Interner::new());
        let store = Store::from_records(
            StorePaths::in_dir(eroot.join("vdb")),
            interner.clone(),
            Vec::new(),
        );
        let ctx = empty_ctx(eroot);

        // A symlink whose live mtime matches the recorded mtime is removable.
        assert!(!should_skip_sym(&ctx, &store, &interner, "cat/pkg-1", "/link", recorded).unwrap());
        // A symlink whose live mtime differs is preserved as administrator-modified.
        assert!(
            should_skip_sym(&ctx, &store, &interner, "cat/pkg-1", "/link", recorded + 1).unwrap()
        );
    }
}
