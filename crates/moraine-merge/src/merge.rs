//! The image-merge transaction: stage, link, commit CONTENTS.
//!
//! A merge proceeds as: scan the image and run collision protection before
//! touching the live root; place files into EROOT with per-file atomic placement
//! (write a temporary sibling and rename, create directories first, place
//! symlinks atomically), honoring CONFIG_PROTECT; fsync containing directories so
//! placements are durable; then remove obsolete files of the replaced version
//! the new version no longer provides. CONTENTS is built during placement. The
//! caller records the installed state as the commit point.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;

use moraine_common::Interner;
use moraine_vdb::contents::{Entry, EntryKind};
use moraine_vdb::store::Store;

use crate::collision;
use crate::contents::{dir_entry, obj_entry, sym_entry};
use crate::error::{IoResultExt as _, MergeError};
use crate::image::{self, ImageItem, ImageKind};
use crate::plan::MergeOp;
use crate::preserve::{self, PreservedEntry, PreservedLibs};
use crate::protect::{self};
use crate::{MergeContext, dir_entry_names};

/// The result of placing an image into EROOT, before the state commit.
pub(crate) struct MergeResult {
    /// The CONTENTS entries to record (including `._cfg` variants and preserved
    /// libraries).
    pub entries: Vec<Entry>,
    /// The `._cfg` variant install paths created, surfaced as config updates.
    pub config_updates: Vec<String>,
    /// The libraries preserved during the same-slot replacement.
    pub preserved: Vec<PreservedEntry>,
}

/// Perform the file-placement half of a merge. Does not write the installed
/// record; the caller does that as the commit point after this returns.
pub(crate) fn place_image(
    ctx: &MergeContext,
    store: &Store,
    interner: &Interner,
    registry: &mut PreservedLibs,
    op: &MergeOp,
) -> Result<MergeResult, MergeError> {
    let span = tracing::info_span!("merge", pkg = %op.state.cpv);
    let _enter = span.enter();

    let items = image::scan(&op.image_dir)?;

    // Collision protection runs entirely before any mutation.
    let targets: Vec<String> = items
        .iter()
        .filter(|i| !matches!(i.kind, ImageKind::Dir))
        .map(|i| i.install_path.clone())
        .collect();
    let collisions = {
        let span = tracing::info_span!("collision_check", pkg = %op.state.cpv);
        let _e = span.enter();
        collision::detect(
            store,
            interner,
            &ctx.eroot,
            &targets,
            op.replaces.as_deref(),
        )
    };
    let aborting = collision::aborting(ctx.features, &collisions);
    if !aborting.is_empty() {
        tracing::warn!(
            count = aborting.len(),
            "collision protection aborting merge"
        );
        return Err(MergeError::Collision { paths: aborting });
    }
    if !collisions.is_empty() {
        tracing::info!(count = collisions.len(), "overwriting collisions");
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut config_updates: Vec<String> = Vec::new();
    let mut touched_dirs: BTreeSet<std::path::PathBuf> = BTreeSet::new();

    for item in &items {
        place_item(ctx, item, &mut entries, &mut config_updates)?;
        if let Some(parent) = ctx.live_path(&item.install_path).parent() {
            touched_dirs.insert(parent.to_path_buf());
        }
    }

    // Make placed files durable by fsyncing the directories that hold them.
    for dir in &touched_dirs {
        fsync_dir(dir)?;
    }

    // Same-slot replacement: remove obsolete prior-version files now that the new
    // files are durable, preserving still-needed shared libraries.
    let preserved = if let Some(prior) = op.replaces.as_deref() {
        remove_obsolete(ctx, store, interner, registry, prior, &entries)?
    } else {
        Vec::new()
    };

    Ok(MergeResult {
        entries,
        config_updates,
        preserved,
    })
}

/// Place a single image item into the live root, appending its CONTENTS entry.
fn place_item(
    ctx: &MergeContext,
    item: &ImageItem,
    entries: &mut Vec<Entry>,
    config_updates: &mut Vec<String>,
) -> Result<(), MergeError> {
    let live = ctx.live_path(&item.install_path);
    match &item.kind {
        ImageKind::Dir => {
            std::fs::create_dir_all(&live).with_path(&live)?;
            entries.push(dir_entry(&item.install_path));
        }
        ImageKind::File => {
            let bytes = std::fs::read(&item.source).with_path(&item.source)?;
            // Ensure the parent directory exists.
            if let Some(parent) = live.parent() {
                std::fs::create_dir_all(parent).with_path(parent)?;
            }
            let entry = place_file(ctx, &item.install_path, &live, &bytes, config_updates)?;
            entries.push(entry);
        }
        ImageKind::Sym { target } => {
            if let Some(parent) = live.parent() {
                std::fs::create_dir_all(parent).with_path(parent)?;
            }
            place_symlink(&live, target)?;
            entries.push(sym_entry(&item.install_path, target, &live)?);
        }
    }
    Ok(())
}

/// Place a regular file, honoring CONFIG_PROTECT. Returns the CONTENTS entry for
/// either the real path or the `._cfg` variant that was written.
fn place_file(
    ctx: &MergeContext,
    install_path: &str,
    live: &Path,
    bytes: &[u8],
    config_updates: &mut Vec<String>,
) -> Result<Entry, MergeError> {
    if ctx.config_protect.is_protected(install_path) && live.exists() {
        let existing = std::fs::read(live).with_path(live)?;
        if existing != bytes {
            // Differing protected file: write a `._cfgNNNN_` variant beside it.
            let name = live
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let siblings = protect::sibling_names(live);
            let variant = protect::variant_name(&name, &siblings);
            let variant_live = live.with_file_name(&variant);
            atomic_place_file(&variant_live, bytes)?;
            let variant_install = sibling_install_path(install_path, &variant);
            config_updates.push(variant_install.clone());
            return obj_entry(&variant_install, bytes, &variant_live);
        }
        // Byte-identical: no variant, leave content (rewrite is a no-op).
    }
    atomic_place_file(live, bytes)?;
    obj_entry(install_path, bytes, live)
}

/// Compute the install path of a sibling variant from the real install path.
fn sibling_install_path(install_path: &str, variant_name: &str) -> String {
    match install_path.rfind('/') {
        Some(idx) => format!("{}/{}", &install_path[..idx], variant_name),
        None => variant_name.to_string(),
    }
}

/// Write `bytes` to `path` via a temporary sibling and rename so the target is
/// never observed partially written.
fn atomic_place_file(path: &Path, bytes: &[u8]) -> Result<(), MergeError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = sibling_tmp(path, &name);
    // Clear any stale temporary file from a prior interrupted attempt.
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::File::create(&tmp).with_path(&tmp)?;
    file.write_all(bytes).with_path(&tmp)?;
    file.sync_all().with_path(&tmp)?;
    drop(file);
    std::fs::rename(&tmp, path).with_path(path)?;
    Ok(())
}

/// A deterministic temporary sibling path for atomic placement.
fn sibling_tmp(path: &Path, name: &str) -> std::path::PathBuf {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!(".moraine-tmp-{name}"))
}

/// Place a symlink atomically: create it at a temporary name and rename over the
/// target, so no path is observed half-linked.
fn place_symlink(path: &Path, target: &str) -> Result<(), MergeError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".moraine-symtmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ));
    // Clear any stale temporary link.
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).with_path(&tmp)?;
    std::fs::rename(&tmp, path).with_path(path)?;
    Ok(())
}

/// fsync a directory so renamed-in entries are durable.
fn fsync_dir(dir: &Path) -> Result<(), MergeError> {
    match std::fs::File::open(dir) {
        Ok(f) => f.sync_all().with_path(dir),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(MergeError::Io {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

/// Remove the prior-version files the new version no longer provides, after the
/// new files are durable. Still-needed shared libraries are preserved when
/// `preserve-libs` is set; the preserved entries are returned.
fn remove_obsolete(
    ctx: &MergeContext,
    store: &Store,
    interner: &Interner,
    registry: &mut PreservedLibs,
    prior_cpv: &str,
    new_entries: &[Entry],
) -> Result<Vec<PreservedEntry>, MergeError> {
    let span = tracing::info_span!("replace", prior = %prior_cpv);
    let _enter = span.enter();

    let new_paths: BTreeSet<&str> = new_entries.iter().map(|e| e.path.as_str()).collect();
    let mut preserved = Vec::new();

    let Some(prior) = store
        .records()
        .iter()
        .find(|r| collision::record_is(r, interner, prior_cpv))
    else {
        return Ok(preserved);
    };

    // Map the soname provided by each prior path, for preserve-libs decisions.
    let prior_sonames = provided_sonames(prior, interner);

    // Collect prior obj/sym paths the new version no longer provides, deepest
    // first so directories are emptied before removal is attempted. A protected
    // config the new version no longer ships (for example because it wrote a
    // `._cfg` variant beside the live file) is never removed.
    let mut obsolete: Vec<Entry> = prior
        .contents
        .iter()
        .filter(|e| !matches!(e.kind, EntryKind::Dir))
        .filter(|e| !new_paths.contains(e.path.as_str()))
        .filter(|e| !ctx.config_protect.is_protected(&e.path))
        .collect();
    obsolete.sort_by(|a, b| b.path.cmp(&a.path));

    for entry in &obsolete {
        let live = ctx.live_path(&entry.path);
        // preserve-libs: keep a still-needed shared library in place.
        if ctx.features.preserve_libs
            && let Some(soname) = library_soname(&entry.path, &prior_sonames)
            && preserve::soname_still_needed(store, interner, &soname, Some(prior_cpv))
        {
            registry.insert(PreservedEntry {
                cpv: prior_cpv.to_string(),
                soname: soname.clone(),
                path: entry.path.clone(),
            });
            preserved.push(PreservedEntry {
                cpv: prior_cpv.to_string(),
                soname,
                path: entry.path.clone(),
            });
            tracing::info!(path = %entry.path, "preserving still-needed library");
            continue;
        }
        match std::fs::remove_file(&live) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(MergeError::Io { path: live, source });
            }
        }
    }

    // Remove now-empty prior-version directories the new version does not own.
    let mut dirs: Vec<String> = prior
        .contents
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::Dir))
        .filter(|e| !new_paths.contains(e.path.as_str()))
        .map(|e| e.path)
        .collect();
    dirs.sort_by(|a, b| b.cmp(a));
    for dir in &dirs {
        let live = ctx.live_path(dir);
        if dir_entry_names(&live).is_empty() {
            let _ = std::fs::remove_dir(&live);
        }
    }

    Ok(preserved)
}

/// The sonames provided by a record, paired with the basename they map to.
///
/// stock `PROVIDES` does not carry the path, so the engine matches a soname to a
/// library by the file's basename, which is the soname for a real `.so.N` file.
fn provided_sonames(
    record: &moraine_vdb::record::PackageRecord,
    interner: &Interner,
) -> Vec<String> {
    record
        .provides
        .entries
        .iter()
        .filter_map(|e| interner.resolve(e.soname).map(|s| s.to_string()))
        .collect()
}

/// Whether `path`'s basename is one of the provided sonames; if so, the soname.
fn library_soname(path: &str, sonames: &[String]) -> Option<String> {
    let base = path.rsplit('/').next().unwrap_or(path);
    sonames.iter().find(|s| s.as_str() == base).cloned()
}

/// Build the explicit + variant entries as a final CONTENTS-ready list. The
/// preserved library paths are already present in the prior contents and are
/// re-added so they remain owned.
pub(crate) fn entries_with_preserved(
    mut entries: Vec<Entry>,
    preserved: &[PreservedEntry],
) -> Vec<Entry> {
    for p in preserved {
        if !entries.iter().any(|e| e.path == p.path) {
            // Record the preserved path as an obj with no digest; it stays owned
            // by the new version so unmerge and reconciliation can find it.
            entries.push(Entry {
                path: p.path.clone(),
                kind: EntryKind::Obj {
                    md5: String::new(),
                    mtime: 0,
                },
            });
        }
    }
    entries
}
