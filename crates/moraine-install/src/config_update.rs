//! Protected-config update resolution.
//!
//! A merge into a `CONFIG_PROTECT` path writes the new file to a sibling
//! `._cfgNNNN_<name>` variant instead of overwriting the live file. The merge
//! engine reports those variant paths; this module models a [`PendingUpdate`] and
//! resolves it by applying the new file, keeping the existing file, or writing a
//! caller-supplied merged result. The interactive prompt that chooses the
//! resolution lives in the CLI; this layer performs the chosen filesystem change.

use std::path::{Path, PathBuf};

use crate::error::{InstallError, Result};

/// A pending protected-config update left by a merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpdate {
    /// The live file the variant would replace.
    pub target: PathBuf,
    /// The `._cfgNNNN_<name>` variant file holding the new content.
    pub variant: PathBuf,
}

impl PendingUpdate {
    /// Build a pending update from a variant path, deriving the live target by
    /// stripping the `._cfgNNNN_` prefix from the file name. Returns `None` when
    /// the path does not name a config variant.
    pub fn from_variant(variant: impl Into<PathBuf>) -> Option<Self> {
        let variant = variant.into();
        let name = variant.file_name()?.to_str()?;
        // A backup artifact is never a pending update, even if it otherwise has
        // the `._cfgNNNN_` shape.
        if is_backup_name(name) {
            return None;
        }
        let target_name = strip_variant_prefix(name)?;
        let target = variant.with_file_name(target_name);
        Some(PendingUpdate { target, variant })
    }
}

/// How to resolve a pending protected-config update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Replace the live file with the variant's content.
    Apply,
    /// Discard the variant, leaving the live file unchanged.
    Keep,
    /// Write the supplied merged content to the live file.
    Merge(Vec<u8>),
}

/// Resolve one pending update, returning whether the live file changed.
///
/// `Apply` replaces the live file with the variant and removes the variant.
/// `Keep` removes the variant and leaves the live file untouched. `Merge` writes
/// the supplied bytes to the live file and removes the variant.
pub fn resolve_update(update: &PendingUpdate, resolution: Resolution) -> Result<bool> {
    match resolution {
        Resolution::Apply => {
            // A symlink variant is recreated as a symlink so its target string is
            // applied verbatim rather than its dereferenced bytes.
            let meta = std::fs::symlink_metadata(&update.variant)
                .map_err(|e| InstallError::io(&update.variant, e))?;
            if meta.file_type().is_symlink() {
                let link_target = std::fs::read_link(&update.variant)
                    .map_err(|e| InstallError::io(&update.variant, e))?;
                recreate_symlink(&update.target, &link_target)?;
            } else {
                let bytes = std::fs::read(&update.variant)
                    .map_err(|e| InstallError::io(&update.variant, e))?;
                write_target(&update.target, &bytes)?;
            }
            remove_variant(&update.variant)?;
            Ok(true)
        }
        Resolution::Keep => {
            remove_variant(&update.variant)?;
            Ok(false)
        }
        Resolution::Merge(bytes) => {
            write_target(&update.target, &bytes)?;
            remove_variant(&update.variant)?;
            Ok(true)
        }
    }
}

/// Strip a `._cfgNNNN_` prefix from a variant file name, returning the live
/// name. Returns `None` when the name is not a config variant.
fn strip_variant_prefix(name: &str) -> Option<String> {
    let rest = name.strip_prefix("._cfg")?;
    if rest.len() < 5 {
        return None;
    }
    let (digits, tail) = rest.split_at(4);
    if digits.chars().all(|c| c.is_ascii_digit()) {
        tail.strip_prefix('_').map(str::to_owned)
    } else {
        None
    }
}

/// Whether a file name is a backup artifact (`*~` or `*.bak`, case-insensitive)
/// excluded from pending updates, matching `find_updated_config_files`.
fn is_backup_name(name: &str) -> bool {
    name.ends_with('~') || name.to_ascii_lowercase().ends_with(".bak")
}

/// Recreate the live target as a symlink to `link_target`, replacing any existing
/// path and creating parents.
fn recreate_symlink(target: &Path, link_target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| InstallError::io(parent, e))?;
    }
    match std::fs::remove_file(target) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(InstallError::io(target, e)),
    }
    std::os::unix::fs::symlink(link_target, target).map_err(|e| InstallError::io(target, e))
}

/// Write `bytes` to the live target atomically, creating parents.
fn write_target(target: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| InstallError::io(parent, e))?;
    }
    moraine_common::fs::atomic_write(target, bytes)
        .map_err(|e| InstallError::io(target, std::io::Error::other(e.to_string())))
}

/// Remove a variant file, tolerating an already-absent file.
fn remove_variant(variant: &Path) -> Result<()> {
    match std::fs::remove_file(variant) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(InstallError::io(variant, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(content: &str) -> (tempfile::TempDir, PendingUpdate) {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("foo.conf");
        let variant = dir.path().join("._cfg0000_foo.conf");
        std::fs::write(&target, "old\n").unwrap();
        std::fs::write(&variant, content).unwrap();
        let update = PendingUpdate::from_variant(&variant).unwrap();
        assert_eq!(update.target, target);
        (dir, update)
    }

    #[test]
    fn from_variant_derives_target() {
        let update = PendingUpdate::from_variant("/etc/._cfg0003_bar.conf").unwrap();
        assert_eq!(update.target, PathBuf::from("/etc/bar.conf"));
    }

    #[test]
    fn from_variant_rejects_plain_name() {
        assert!(PendingUpdate::from_variant("/etc/bar.conf").is_none());
    }

    #[test]
    fn from_variant_rejects_backup_suffixes() {
        assert!(PendingUpdate::from_variant("/etc/._cfg0000_foo.conf~").is_none());
        assert!(PendingUpdate::from_variant("/etc/._cfg0000_foo.conf.bak").is_none());
        assert!(PendingUpdate::from_variant("/etc/._cfg0000_foo.conf.BAK").is_none());
    }

    #[test]
    fn apply_replaces_live_file() {
        let (_dir, update) = setup("new\n");
        let changed = resolve_update(&update, Resolution::Apply).unwrap();
        assert!(changed);
        assert_eq!(std::fs::read_to_string(&update.target).unwrap(), "new\n");
        assert!(!update.variant.exists());
    }

    #[test]
    fn keep_discards_variant() {
        let (_dir, update) = setup("new\n");
        let changed = resolve_update(&update, Resolution::Keep).unwrap();
        assert!(!changed);
        assert_eq!(std::fs::read_to_string(&update.target).unwrap(), "old\n");
        assert!(!update.variant.exists());
    }

    #[test]
    fn merge_writes_supplied_content() {
        let (_dir, update) = setup("new\n");
        let changed = resolve_update(&update, Resolution::Merge(b"merged\n".to_vec())).unwrap();
        assert!(changed);
        assert_eq!(std::fs::read_to_string(&update.target).unwrap(), "merged\n");
        assert!(!update.variant.exists());
    }

    #[test]
    fn apply_recreates_symlink_variant_as_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("link");
        let variant = dir.path().join("._cfg0000_link");
        // The live path is a symlink the admin customized.
        std::os::unix::fs::symlink("old-target", &target).unwrap();
        // The variant is itself a symlink to the new target.
        std::os::unix::fs::symlink("new-target", &variant).unwrap();
        let update = PendingUpdate::from_variant(&variant).unwrap();
        assert_eq!(update.target, target);

        let changed = resolve_update(&update, Resolution::Apply).unwrap();
        assert!(changed);
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "live path is recreated as a symlink, not a regular file"
        );
        assert_eq!(
            std::fs::read_link(&target).unwrap(),
            PathBuf::from("new-target")
        );
        assert!(!variant.exists());
    }
}
