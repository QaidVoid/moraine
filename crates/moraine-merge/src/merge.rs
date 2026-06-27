//! The image-merge transaction: stage, link, commit CONTENTS.
//!
//! A merge proceeds as: scan the image and run collision protection before
//! touching the live root; place files into EROOT with per-file atomic placement
//! (write a temporary sibling and rename, create directories first, place
//! symlinks atomically), honoring CONFIG_PROTECT; fsync containing directories so
//! placements are durable; then remove obsolete files of the replaced version
//! the new version no longer provides. CONTENTS is built during placement. The
//! caller records the installed state as the commit point.

use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::path::Path;

use moraine_common::Interner;
use moraine_vdb::contents::{Entry, EntryKind};
use moraine_vdb::store::Store;
use moraine_version::Version;

use crate::collision;
use crate::contents::{
    compute_md5, dev_entry, dir_entry, fif_entry, mtime_secs, obj_entry, sym_entry,
};
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

    let mut items = image::scan(&op.image_dir, ctx.features.xattr)?;

    // INSTALL_MASK: drop masked paths before any file enters CONTENTS, so masked
    // doc/man/info and configured paths are never merged or recorded.
    if !ctx.install_mask.is_empty() {
        let before = items.len();
        items.retain(|item| !ctx.install_mask.is_masked(&item.install_path));
        let removed = before - items.len();
        if removed > 0 {
            tracing::info!(count = removed, "INSTALL_MASK filtered staged paths");
        }
    }

    // Collision protection runs entirely before any mutation.
    let collisions = {
        let span = tracing::info_span!("collision_check", pkg = %op.state.cpv);
        let _e = span.enter();
        collision::detect(
            store,
            interner,
            &ctx.eroot,
            &items,
            op.replaces.as_deref(),
            &ctx.collision_ignore,
            &ctx.config_protect,
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
    // The first install path placed for each shared inode, so later occurrences
    // become hardlinks rather than duplicated content.
    let mut placed_inodes: HashMap<(u64, u64), String> = HashMap::new();
    // The md5 the replaced version recorded per path, for config-protect-if-
    // modified and the deleted-protected force case.
    let prior_md5 = prior_recorded_md5(store, interner, op.replaces.as_deref());
    let mut confmem = protect::ConfMem::load(ctx.confmem_file());

    // A downgrade behaves as `--noconfmem`, forcing a fresh `._cfg` variant for a
    // differing protected config so the admin is re-prompted on upgrade/downgrade
    // cycles, matching Portage's `downgrade` handling.
    let downgrade = is_downgrade(&op.state.version, op.replaces.as_deref());
    let force = ctx.noconfmem || downgrade;

    for item in &items {
        let shared = matches!(item.kind, ImageKind::File) && item.nlink > 1;
        if shared
            && let Some(first) = placed_inodes.get(&(item.dev, item.ino))
            && !ctx.config_protect.is_protected(&item.install_path)
        {
            place_hardlink(ctx, item, first, &mut entries)?;
        } else {
            place_item(
                ctx,
                item,
                &prior_md5,
                force,
                &mut confmem,
                &mut entries,
                &mut config_updates,
            )?;
            if shared {
                placed_inodes.insert((item.dev, item.ino), item.install_path.clone());
            }
        }
        if let Some(parent) = ctx.live_path(&item.install_path).parent() {
            touched_dirs.insert(parent.to_path_buf());
        }
    }
    // Persist any newly offered config contents so an identical update is not
    // re-offered on a later merge.
    if let Err(e) = confmem.save() {
        tracing::warn!(error = %e, "could not persist config memory");
    }

    // Secondhand/force pass: a symlink is placed regardless of whether its target
    // exists yet, so the only remaining concern is reporting a link left broken
    // after the whole image is merged, matching Portage's broken-symlink QA.
    for item in &items {
        if let ImageKind::Sym { target } = &item.kind {
            let resolved = if target.starts_with('/') {
                // An absolute target is install-root-relative; resolve under EROOT.
                ctx.live_path(target)
            } else {
                let live = ctx.live_path(&item.install_path);
                live.parent().unwrap_or_else(|| Path::new("/")).join(target)
            };
            if !resolved.exists() {
                tracing::warn!(path = %item.install_path, target, "merged a broken symlink");
            }
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
#[allow(clippy::too_many_arguments)]
fn place_item(
    ctx: &MergeContext,
    item: &ImageItem,
    prior_md5: &HashMap<String, String>,
    force: bool,
    confmem: &mut protect::ConfMem,
    entries: &mut Vec<Entry>,
    config_updates: &mut Vec<String>,
) -> Result<(), MergeError> {
    let live = ctx.live_path(&item.install_path);
    match &item.kind {
        ImageKind::Dir => {
            // A directory (or a symlink to one) that already exists keeps its
            // mode and ownership; only a directory this merge creates receives the
            // image metadata, matching Portage's chmod/chown of a freshly created
            // directory.
            let pre_existing = live.is_dir();
            if !pre_existing {
                backup_blocker(&live, true)?;
            }
            std::fs::create_dir_all(&live).with_path(&live)?;
            if !pre_existing {
                apply_metadata(&live, item, false)?;
            }
            entries.push(dir_entry(&item.install_path));
        }
        ImageKind::File => {
            let bytes = std::fs::read(&item.source).with_path(&item.source)?;
            // Ensure the parent directory exists.
            if let Some(parent) = live.parent() {
                std::fs::create_dir_all(parent).with_path(parent)?;
            }
            backup_blocker(&live, false)?;
            let (entry, written) = place_file(
                ctx,
                &item.install_path,
                &live,
                &bytes,
                item.mtime,
                prior_md5.get(&item.install_path).map(String::as_str),
                force,
                confmem,
                config_updates,
            )?;
            apply_metadata(&written, item, false)?;
            apply_source_mtime(&written, item, false);
            entries.push(entry);
        }
        ImageKind::Sym { target } => {
            if let Some(parent) = live.parent() {
                std::fs::create_dir_all(parent).with_path(parent)?;
            }
            let entry = if ctx.config_protect.is_protected(&item.install_path) {
                place_protected_symlink(&live, item, target, force, confmem, config_updates)?
            } else {
                backup_blocker(&live, false)?;
                place_symlink(&live, target)?;
                apply_metadata(&live, item, true)?;
                apply_source_mtime(&live, item, true);
                sym_entry(&item.install_path, target, item.mtime)
            };
            entries.push(entry);
        }
        ImageKind::Fifo => {
            if let Some(parent) = live.parent() {
                std::fs::create_dir_all(parent).with_path(parent)?;
            }
            backup_blocker(&live, false)?;
            if place_node(&live, item)? {
                apply_metadata(&live, item, false)?;
            }
            entries.push(fif_entry(&item.install_path));
        }
        ImageKind::Dev => {
            if let Some(parent) = live.parent() {
                std::fs::create_dir_all(parent).with_path(parent)?;
            }
            backup_blocker(&live, false)?;
            if place_node(&live, item)? {
                apply_metadata(&live, item, false)?;
            }
            entries.push(dev_entry(&item.install_path));
        }
    }
    Ok(())
}

/// Place a symlink under a `CONFIG_PROTECT` path, returning its CONTENTS entry.
///
/// When the live link is absent or already points at the same target, it is
/// written in place. When it points elsewhere, a `._cfgNNNN_<name>` symlink
/// variant is written beside it, the live link is left untouched, and the variant
/// is reported as a pending config update, mirroring `_protect` forcing a variant
/// when `dest_link != src_link` (bug #485598). The md5 of the new target string is
/// remembered in config memory; an identical update already offered is suppressed
/// unless `force` is set.
fn place_protected_symlink(
    live: &Path,
    item: &ImageItem,
    target: &str,
    force: bool,
    confmem: &mut protect::ConfMem,
    config_updates: &mut Vec<String>,
) -> Result<Entry, MergeError> {
    let install_path = &item.install_path;
    let live_target = std::fs::read_link(live)
        .ok()
        .map(|t| t.to_string_lossy().into_owned());

    if live_target.as_deref() != Some(target) && live_target.is_some() {
        // The live link points elsewhere: protect it.
        let md5 = compute_md5(target.as_bytes());
        if !force && confmem.already_offered(install_path, &md5) {
            // The same update was already offered and dismissed: leave the live
            // link untouched and record it as still owned.
            let live_target = live_target.unwrap_or_else(|| target.to_string());
            return Ok(sym_entry(install_path, &live_target, item.mtime));
        }
        let (entry, variant_install) =
            write_symlink_variant(install_path, live, target, item.mtime)?;
        confmem.record(install_path, &md5);
        config_updates.push(variant_install);
        return Ok(entry);
    }

    // Absent or identical: write the link in place.
    backup_blocker(live, false)?;
    place_symlink(live, target)?;
    apply_metadata(live, item, true)?;
    apply_source_mtime(live, item, true);
    Ok(sym_entry(install_path, target, item.mtime))
}

/// Write a `._cfgNNNN_<name>` symlink variant beside `live`, reusing the highest
/// existing variant when its link target already equals `target`. Returns the
/// `sym` CONTENTS entry for the variant and its install path.
fn write_symlink_variant(
    install_path: &str,
    live: &Path,
    target: &str,
    mtime: i64,
) -> Result<(Entry, String), MergeError> {
    let name = live
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let variant = choose_symlink_variant(live, &name, target);
    let variant_live = live.with_file_name(&variant);
    place_symlink(&variant_live, target)?;
    let variant_install = sibling_install_path(install_path, &variant);
    let entry = sym_entry(&variant_install, target, mtime);
    Ok((entry, variant_install))
}

/// Choose the `._cfgNNNN_<name>` symlink variant: reuse the highest existing
/// variant when its link target already equals `target`, otherwise allocate the
/// next index, mirroring Portage's `new_protect_filename` for symlinks.
fn choose_symlink_variant(live: &Path, name: &str, target: &str) -> String {
    if let Some(highest) = protect::highest_variant_path(live, name)
        && std::fs::read_link(&highest).is_ok_and(|t| t.to_string_lossy() == target)
        && let Some(highest_name) = highest.file_name()
    {
        return highest_name.to_string_lossy().into_owned();
    }
    protect::variant_name(name, &protect::sibling_names(live))
}

/// Whether replacing the prior `category/package-version` with `new_version` is a
/// version downgrade, mirroring Portage's `vercmp(...) < 0`. A missing or
/// unparsable prior version is not a downgrade.
fn is_downgrade(new_version: &str, prior_cpv: Option<&str>) -> bool {
    let Some(prior_version) = prior_cpv.and_then(cpv_version) else {
        return false;
    };
    match (Version::parse(new_version), Version::parse(&prior_version)) {
        (Ok(new), Ok(old)) => new < old,
        _ => false,
    }
}

/// Extract the version from a `category/package-version` cpv, at the first `-`
/// followed by a parsable version.
fn cpv_version(cpv: &str) -> Option<String> {
    let pv = cpv.rsplit_once('/').map(|(_, pv)| pv).unwrap_or(cpv);
    let mut idx = 0;
    while let Some(rel) = pv[idx..].find('-') {
        let at = idx + rel;
        let tail = &pv[at + 1..];
        if tail.as_bytes().first().is_some_and(u8::is_ascii_digit) && Version::parse(tail).is_ok() {
            return Some(tail.to_string());
        }
        idx = at + 1;
    }
    None
}

/// Rename a type-conflicting blocker out of the way before placement. A
/// directory blocking a non-directory placement, or a non-directory blocking a
/// directory placement, is moved to a numbered `.backup.N` sibling, mirroring
/// Portage's `_new_backup_path`. A same-type blocker is left for the normal
/// atomic replace.
fn backup_blocker(live: &Path, placing_dir: bool) -> Result<(), MergeError> {
    let Ok(meta) = std::fs::symlink_metadata(live) else {
        return Ok(());
    };
    // A symlink is treated as a non-directory blocker even if it points at one.
    let is_dir = meta.file_type().is_dir();
    if is_dir == placing_dir {
        return Ok(());
    }
    let backup = backup_path(live);
    std::fs::rename(live, &backup).with_path(live)?;
    tracing::warn!(
        path = %live.display(),
        backup = %backup.display(),
        "renamed type-conflicting blocker"
    );
    Ok(())
}

/// The first free `.backup.N` sibling of `live`.
fn backup_path(live: &Path) -> std::path::PathBuf {
    let base = live
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut n = 0;
    loop {
        let candidate = live.with_file_name(format!("{base}.backup.{n}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Re-create a shared-inode file as a hardlink to the already-placed sibling
/// rather than duplicating its content, mirroring Portage's hardlink merge map.
fn place_hardlink(
    ctx: &MergeContext,
    item: &ImageItem,
    first_install_path: &str,
    entries: &mut Vec<Entry>,
) -> Result<(), MergeError> {
    let live = ctx.live_path(&item.install_path);
    if let Some(parent) = live.parent() {
        std::fs::create_dir_all(parent).with_path(parent)?;
    }
    let target = ctx.live_path(first_install_path);
    let _ = std::fs::remove_file(&live);
    std::fs::hard_link(&target, &live).with_path(&live)?;
    // The hardlink shares the sibling's content; record its md5 and source mtime.
    let bytes = std::fs::read(&item.source).with_path(&item.source)?;
    apply_source_mtime(&live, item, false);
    entries.push(obj_entry(&item.install_path, &bytes, item.mtime));
    Ok(())
}

/// Create a FIFO or device node at `path` with `mknod`, returning whether the
/// node was created. A node already present is left in place untouched and only
/// its CONTENTS record is written, mirroring Portage merging a node only when the
/// destination does not already exist. The node type and permission bits come
/// from the source mode and the device number from `rdev`.
fn place_node(path: &Path, item: &ImageItem) -> Result<bool, MergeError> {
    use rustix::fs::{FileType, Mode, RawMode};
    if std::fs::symlink_metadata(path).is_ok() {
        return Ok(false);
    }
    let file_type = FileType::from_raw_mode(item.mode as RawMode);
    let mode = Mode::from_bits_truncate((item.mode & 0o7777) as RawMode);
    let dev = if matches!(file_type, FileType::CharacterDevice | FileType::BlockDevice) {
        item.rdev
    } else {
        0
    };
    rustix::fs::mknodat(rustix::fs::CWD, path, file_type, mode, dev).map_err(|e| {
        MergeError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::from_raw_os_error(e.raw_os_error()),
        }
    })?;
    Ok(true)
}

/// Reapply the source mode, ownership, and xattrs to a freshly placed path. The
/// operation degrades gracefully on `EPERM`/`EACCES` (an unprivileged root
/// cannot chown or set certain bits) and on `ENOTSUP` for xattrs, recording the
/// intent and continuing rather than aborting the merge, matching Portage.
fn apply_metadata(path: &Path, item: &ImageItem, is_symlink: bool) -> Result<(), MergeError> {
    // Ownership first; lchown does not dereference the symlink itself.
    if let Err(e) = std::os::unix::fs::lchown(path, Some(item.uid), Some(item.gid))
        && !is_permission_denied(&e)
    {
        return Err(MergeError::Io {
            path: path.to_path_buf(),
            source: e,
        });
    }
    // A symlink carries no mode of its own and no copyable xattrs.
    if is_symlink {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt as _;
    let perms = std::fs::Permissions::from_mode(item.mode & 0o7777);
    if let Err(e) = std::fs::set_permissions(path, perms)
        && !is_permission_denied(&e)
    {
        return Err(MergeError::Io {
            path: path.to_path_buf(),
            source: e,
        });
    }
    for (name, value) in &item.xattrs {
        if let Err(e) = xattr::set(path, name, value)
            && !is_permission_denied(&e)
            && e.raw_os_error() != Some(ENOTSUP)
        {
            return Err(MergeError::Io {
                path: path.to_path_buf(),
                source: e,
            });
        }
    }
    Ok(())
}

/// Stamp the source file's modification time onto a freshly placed path,
/// mirroring Portage's `os.utime(..., follow_symlinks=False)`. A symlink is
/// stamped without following it. The operation degrades gracefully: a permission
/// or unsupported error is logged and the merge continues, since the source mtime
/// is recorded in CONTENTS regardless, matching `apply_metadata`.
fn apply_source_mtime(path: &Path, item: &ImageItem, is_symlink: bool) {
    use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};
    let ts = Timespec {
        tv_sec: item.mtime,
        tv_nsec: item.mtime_nsec as _,
    };
    let times = Timestamps {
        last_access: ts,
        last_modification: ts,
    };
    let flags = if is_symlink {
        AtFlags::SYMLINK_NOFOLLOW
    } else {
        AtFlags::empty()
    };
    if let Err(e) = utimensat(CWD, path, &times, flags) {
        tracing::debug!(path = %path.display(), error = ?e, "could not stamp source mtime");
    }
}

/// Whether an I/O error is `EPERM`/`EACCES`, which Rust maps to
/// `PermissionDenied`.
fn is_permission_denied(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
}

/// `ENOTSUP`/`EOPNOTSUPP` on Linux, returned by a filesystem without xattrs.
const ENOTSUP: i32 = 95;

/// Place a regular file, honoring CONFIG_PROTECT and its edge cases. Returns the
/// CONTENTS entry for either the real path or the `._cfg` variant that was
/// written, paired with the live path actually written so the caller can reapply
/// metadata to it.
#[allow(clippy::too_many_arguments)]
fn place_file(
    ctx: &MergeContext,
    install_path: &str,
    live: &Path,
    bytes: &[u8],
    mtime: i64,
    prior_md5: Option<&str>,
    force: bool,
    confmem: &mut protect::ConfMem,
    config_updates: &mut Vec<String>,
) -> Result<(Entry, std::path::PathBuf), MergeError> {
    let in_place = |entry_path: &str| -> Result<(Entry, std::path::PathBuf), MergeError> {
        atomic_place_file(live, bytes)?;
        Ok((obj_entry(entry_path, bytes, mtime), live.to_path_buf()))
    };

    // A zero-byte `.keep` marker is never config-protected.
    let basename = live
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let is_keep = bytes.is_empty() && basename.starts_with(".keep");

    if !ctx.config_protect.is_protected(install_path) || is_keep {
        return in_place(install_path);
    }

    let exists = live.exists();
    if exists {
        let existing = std::fs::read(live).with_path(live)?;
        if existing == bytes {
            // Byte-identical: rewrite in place is a no-op.
            return in_place(install_path);
        }
        // config-protect-if-modified: overwrite in place when the admin has not
        // modified the live file from what was last installed.
        if ctx.features.config_protect_if_modified
            && let Some(recorded) = prior_md5
            && compute_md5(&existing) == recorded
        {
            return in_place(install_path);
        }
        // noconfmem: an identical update already offered for this path is not
        // re-offered; leave the live file as the admin left it. `force`
        // (`--noconfmem` or a downgrade) bypasses the suppression and always
        // writes a fresh variant.
        let md5 = compute_md5(bytes);
        if !force && confmem.already_offered(install_path, &md5) {
            return Ok((
                obj_entry(install_path, &existing, mtime),
                live.to_path_buf(),
            ));
        }
        let (entry, written, variant_install) = write_variant(install_path, live, bytes, mtime)?;
        confmem.record(install_path, &md5);
        config_updates.push(variant_install);
        Ok((entry, written))
    } else if prior_md5.is_some() {
        // A protected file recorded in the old contents was deleted by the admin:
        // force a `._cfg` variant rather than silently restoring it.
        let md5 = compute_md5(bytes);
        let (entry, written, variant_install) = write_variant(install_path, live, bytes, mtime)?;
        confmem.record(install_path, &md5);
        config_updates.push(variant_install);
        Ok((entry, written))
    } else {
        // First install of a protected file: write it in place.
        in_place(install_path)
    }
}

/// Write a `._cfgNNNN_` variant beside `live`, reusing the highest existing
/// variant when its bytes already equal `bytes` rather than allocating a new
/// index. Returns the CONTENTS entry, the written path, and the variant's
/// install path.
fn write_variant(
    install_path: &str,
    live: &Path,
    bytes: &[u8],
    mtime: i64,
) -> Result<(Entry, std::path::PathBuf, String), MergeError> {
    let name = live
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let variant = choose_variant(live, &name, bytes);
    let variant_live = live.with_file_name(&variant);
    atomic_place_file(&variant_live, bytes)?;
    let variant_install = sibling_install_path(install_path, &variant);
    let entry = obj_entry(&variant_install, bytes, mtime);
    Ok((entry, variant_live, variant_install))
}

/// Choose the `._cfgNNNN_<name>` variant: reuse the highest existing variant when
/// its content already equals `bytes`, otherwise allocate the next index past the
/// highest, mirroring Portage's `new_protect_filename`.
fn choose_variant(live: &Path, name: &str, bytes: &[u8]) -> String {
    if let Some(highest) = protect::highest_variant_path(live, name)
        && std::fs::read(&highest).is_ok_and(|c| c == bytes)
        && let Some(highest_name) = highest.file_name()
    {
        return highest_name.to_string_lossy().into_owned();
    }
    protect::variant_name(name, &protect::sibling_names(live))
}

/// The md5 the replaced version recorded for each obj path, used by
/// config-protect-if-modified and the deleted-protected force case. Empty when
/// there is no prior version.
fn prior_recorded_md5(
    store: &Store,
    interner: &Interner,
    prior_cpv: Option<&str>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(prior_cpv) = prior_cpv else {
        return out;
    };
    let Some(prior) = store
        .records()
        .iter()
        .find(|r| collision::record_is(r, interner, prior_cpv))
    else {
        return out;
    };
    for entry in prior.contents.iter() {
        if let EntryKind::Obj { md5, .. } = entry.kind
            && !md5.is_empty()
        {
            out.insert(entry.path, md5);
        }
    }
    out
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

    // Map each prior object path to its recorded soname, for preserve-libs
    // decisions. The recorded `NEEDED.ELF.2` linkage keys the real versioned
    // library directly, so its soname symlink need not be present in CONTENTS.
    let prior_sonames = preserve::needed_soname_map(&prior.needed);
    // The sonames the new version re-provides (by entry basename): a replacement
    // soname link/hardlink means the old library need not be preserved.
    let new_sonames: BTreeSet<&str> = new_entries.iter().map(|e| basename(&e.path)).collect();

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

    // Paths kept alive by preserve-libs, including soname-symlink chain partners,
    // so they are not removed when iterated.
    let mut preserved_paths: BTreeSet<String> = BTreeSet::new();
    for entry in &obsolete {
        if ctx.features.preserve_libs
            && let Some((bucket, soname)) = library_soname(&entry.path, &prior_sonames)
            && !new_sonames.contains(soname.as_str())
            && preserve::soname_still_needed(store, interner, &bucket, &soname, Some(prior_cpv))
        {
            // Preserve the matched library and its soname-symlink chain partners
            // (the bare soname symlink alongside the real versioned file).
            let mut chain = soname_chain_paths(entry, prior);
            chain.push(entry.path.clone());
            for path in chain {
                if !preserved_paths.insert(path.clone()) {
                    continue;
                }
                registry.insert(PreservedEntry {
                    cpv: prior_cpv.to_string(),
                    bucket: bucket.clone(),
                    soname: soname.clone(),
                    path: path.clone(),
                });
                preserved.push(PreservedEntry {
                    cpv: prior_cpv.to_string(),
                    bucket: bucket.clone(),
                    soname: soname.clone(),
                    path,
                });
            }
            tracing::info!(path = %entry.path, "preserving still-needed library");
        }
    }

    for entry in &obsolete {
        if preserved_paths.contains(&entry.path) {
            continue;
        }
        // An admin-modified obsolete file (md5 or mtime drift) is left in place,
        // mirroring `_unmerge_pkgfiles`. A `sym` entry keeps the prior behavior.
        if let EntryKind::Obj { md5, mtime } = &entry.kind
            && !obsolete_obj_removable(ctx, &entry.path, md5, *mtime)?
        {
            continue;
        }
        let live = ctx.live_path(&entry.path);
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

/// Whether an obsolete prior-version `obj` may be removed on a same-slot replace:
/// the live bytes must still match the recorded md5 and mtime, mirroring
/// `_unmerge_pkgfiles`. `FEATURES=unmerge-orphans` removes regardless of drift; a
/// preserved-library placeholder (empty md5) and a modified or unreadable file are
/// left in place. This reuses the same guard `unmerge.rs::classify_obj` applies so
/// the replace path and the standalone unmerge path stay consistent.
fn obsolete_obj_removable(
    ctx: &MergeContext,
    path: &str,
    md5: &str,
    mtime: i64,
) -> Result<bool, MergeError> {
    if md5.is_empty() {
        return Ok(false);
    }
    if ctx.features.unmerge_orphans {
        return Ok(true);
    }
    let live = ctx.live_path(path);
    let Ok(bytes) = std::fs::read(&live) else {
        return Ok(false);
    };
    if compute_md5(&bytes) != md5 {
        return Ok(false);
    }
    Ok(mtime_secs(&live)? == mtime)
}

/// The recorded `(bucket, soname)` of the library at `path`, from the per-object
/// `NEEDED.ELF.2` linkage map, or `None` when the path is not a recorded ELF
/// object with a soname.
fn library_soname(
    path: &str,
    soname_map: &HashMap<String, (String, String)>,
) -> Option<(String, String)> {
    soname_map.get(path).cloned()
}

/// The final path component of an install path.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The parent directory of an install path (no trailing slash), or `""` at the
/// root.
fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "",
        Some(idx) => &path[..idx],
        None => "",
    }
}

/// The soname-symlink chain partners of a preserved library entry: the symlink's
/// resolved target when the entry is itself a symlink, plus any sibling symlink
/// that points at the entry's basename (the bare soname symlink beside the real
/// versioned file). These are preserved together so the chain stays intact.
fn soname_chain_paths(entry: &Entry, prior: &moraine_vdb::record::PackageRecord) -> Vec<String> {
    let mut out = Vec::new();
    let dir = parent_dir(&entry.path);
    if let EntryKind::Sym { target, .. } = &entry.kind {
        out.push(resolve_sibling(dir, target));
    }
    let base = basename(&entry.path);
    for e in prior.contents.iter() {
        if let EntryKind::Sym { target, .. } = &e.kind
            && parent_dir(&e.path) == dir
            && basename(target) == base
            && e.path != entry.path
        {
            out.push(e.path.clone());
        }
    }
    out
}

/// Resolve a symlink `target` recorded for an entry in directory `dir` to an
/// install path: an absolute target is returned as-is, a relative target is
/// joined onto `dir`.
fn resolve_sibling(dir: &str, target: &str) -> String {
    if target.starts_with('/') {
        target.to_string()
    } else {
        format!("{dir}/{target}")
    }
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
