//! Safe removal of a package's CONTENTS.
//!
//! Unmerge walks the package's recorded CONTENTS in reverse depth order and
//! removes each path only when it is still safe: a regular file only when it
//! still exists, is still owned by this package, and its md5 and mtime match
//! CONTENTS; a symlink only when it still points to the recorded target. A path
//! now owned by another package, a modified file, and a protected config are
//! skipped and left in place. A directory is removed only when empty after its
//! entries are processed. A still-needed shared library is deferred to the
//! preserve-libs reconciliation.

use moraine_common::Interner;
use moraine_vdb::contents::EntryKind;
use moraine_vdb::store::Store;

use crate::collision;
use crate::contents::{compute_md5, mtime_secs};
use crate::error::MergeError;
use crate::preserve;
use crate::{MergeContext, dir_entry_names};

/// The outcome of an unmerge walk: which paths were removed and which were
/// skipped, for reporting.
#[derive(Debug, Default)]
pub(crate) struct UnmergeResult {
    /// Paths removed from the live system.
    pub removed: Vec<String>,
    /// Paths skipped and left in place.
    pub skipped: Vec<String>,
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

    // Provided sonames so a still-needed library can be deferred.
    let provided: Vec<String> = record
        .provides
        .entries
        .iter()
        .filter_map(|e| interner.resolve(e.soname).map(|s| s.to_string()))
        .collect();

    // Walk in reverse depth order so deeper paths precede their parents.
    let mut entries: Vec<_> = record.contents.iter().collect();
    entries.sort_by(|a, b| b.path.cmp(&a.path));

    for entry in entries {
        let live = ctx.live_path(&entry.path);
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
                if should_skip_obj(
                    ctx,
                    store,
                    interner,
                    cpv,
                    &entry.path,
                    &provided,
                    md5,
                    *mtime,
                )? {
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
            EntryKind::Sym { target, .. } => {
                if should_skip_sym(ctx, store, interner, cpv, &entry.path, target)? {
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

#[allow(clippy::too_many_arguments)]
fn should_skip_obj(
    ctx: &MergeContext,
    store: &Store,
    interner: &Interner,
    cpv: &str,
    path: &str,
    provided: &[String],
    md5: &str,
    mtime: i64,
) -> Result<bool, MergeError> {
    // Protected configs are never removed on unmerge.
    if ctx.config_protect.is_protected(path) {
        return Ok(true);
    }
    // Now owned by another package: skip.
    if collision::owner_of(store, interner, path, Some(cpv)).is_some() {
        return Ok(true);
    }
    // Defer a still-needed shared library to preserve-libs reconciliation.
    if ctx.features.preserve_libs {
        let base = path.rsplit('/').next().unwrap_or(path);
        if provided.iter().any(|s| s == base)
            && preserve::soname_still_needed(store, interner, base, Some(cpv))
        {
            return Ok(true);
        }
    }
    let live = ctx.live_path(path);
    let Ok(bytes) = std::fs::read(&live) else {
        // Gone already or unreadable: nothing to remove.
        return Ok(true);
    };
    // A modified file (md5 or mtime mismatch) is skipped. An empty recorded md5
    // is a preserved-library placeholder: never remove it here.
    if md5.is_empty() {
        return Ok(true);
    }
    if compute_md5(&bytes) != md5 {
        return Ok(true);
    }
    if mtime_secs(&live)? != mtime {
        return Ok(true);
    }
    Ok(false)
}

fn should_skip_sym(
    ctx: &MergeContext,
    store: &Store,
    interner: &Interner,
    cpv: &str,
    path: &str,
    target: &str,
) -> Result<bool, MergeError> {
    if ctx.config_protect.is_protected(path) {
        return Ok(true);
    }
    if collision::owner_of(store, interner, path, Some(cpv)).is_some() {
        return Ok(true);
    }
    let live = ctx.live_path(path);
    let Ok(current) = std::fs::read_link(&live) else {
        return Ok(true);
    };
    Ok(current.to_string_lossy() != target)
}
