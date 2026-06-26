//! CONFIG_PROTECT classification and `._cfgNNNN_` variant naming.
//!
//! A target path is protected when it lies under a `CONFIG_PROTECT` directory
//! and not under a `CONFIG_PROTECT_MASK` directory, because the mask takes
//! precedence over protection. A `CONFIG_PROTECT` entry that names a regular
//! file rather than a directory protects only that exact path, mirroring
//! Portage's `_dirs` distinction. A protected path that already exists on the
//! live system and whose new content differs is never overwritten: the new
//! content is written to a sibling named `._cfgNNNN_<name>` at the next index
//! after the highest existing one, and the path is recorded as a pending config
//! update.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One CONFIG_PROTECT or CONFIG_PROTECT_MASK entry and whether it is a directory.
///
/// A directory entry protects (or masks) every path beneath it; a non-directory
/// entry matches only its exact path.
#[derive(Debug, Clone)]
struct ProtectEntry {
    path: String,
    is_dir: bool,
}

/// The CONFIG_PROTECT policy: the protected and masked entries.
///
/// Entries are absolute paths within the install root, matching the install
/// paths recorded in CONTENTS.
#[derive(Debug, Clone, Default)]
pub struct ConfigProtect {
    protect: Vec<ProtectEntry>,
    mask: Vec<ProtectEntry>,
}

impl ConfigProtect {
    /// Build a policy from the `CONFIG_PROTECT` and `CONFIG_PROTECT_MASK` lists,
    /// treating every entry as a directory prefix. Use [`ConfigProtect::with_root`]
    /// to classify entries by their type on the live system.
    pub fn new(
        protect: impl IntoIterator<Item = String>,
        mask: impl IntoIterator<Item = String>,
    ) -> Self {
        let as_dir = |p: String| ProtectEntry {
            path: normalize_prefix(p),
            is_dir: true,
        };
        Self {
            protect: protect.into_iter().map(as_dir).collect(),
            mask: mask.into_iter().map(as_dir).collect(),
        }
    }

    /// Build a policy classifying each entry as a directory or a file by its
    /// type under `root`, matching Portage's `_dirs` set. A non-directory entry
    /// forces an exact path match, so a sibling of a protected file is not
    /// itself protected.
    pub fn with_root(
        root: &Path,
        protect: impl IntoIterator<Item = String>,
        mask: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            protect: protect
                .into_iter()
                .map(|p| classify_entry(root, p))
                .collect(),
            mask: mask.into_iter().map(|p| classify_entry(root, p)).collect(),
        }
    }

    /// Whether `path` (absolute within the install root) is a protected config
    /// path. The longest matching protect entry wins, and a mask entry of equal
    /// or greater length overrides it, mirroring `ConfigProtect.isprotected`
    /// (`protected > masked`).
    pub fn is_protected(&self, path: &str) -> bool {
        let obj = normalize_prefix(path.to_string());
        let mut protected = 0usize;
        let mut masked = 0usize;
        for p in &self.protect {
            if p.path.len() > masked && entry_matches(&obj, p) {
                protected = p.path.len();
                for m in &self.mask {
                    if m.path.len() >= protected && entry_matches(&obj, m) {
                        masked = m.path.len();
                    }
                }
            }
        }
        protected > masked
    }
}

/// Classify a CONFIG_PROTECT entry by whether it resolves to a directory under
/// `root`.
fn classify_entry(root: &Path, p: String) -> ProtectEntry {
    let path = normalize_prefix(p);
    let live = root.join(path.trim_start_matches('/'));
    ProtectEntry {
        is_dir: live.is_dir(),
        path,
    }
}

/// Whether `obj` matches `entry`: an exact match for a non-directory entry, or
/// equal-or-beneath for a directory entry.
fn entry_matches(obj: &str, entry: &ProtectEntry) -> bool {
    if !obj.starts_with(&entry.path) {
        return false;
    }
    if entry.is_dir {
        obj.len() == entry.path.len() || obj[entry.path.len()..].starts_with('/')
    } else {
        obj.len() == entry.path.len()
    }
}

/// Normalize a prefix or path: ensure a leading slash and drop a trailing slash
/// (except for the root itself), so prefix comparison is uniform.
fn normalize_prefix(mut p: String) -> String {
    if !p.starts_with('/') {
        p.insert(0, '/');
    }
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    p
}

/// Compute the `._cfgNNNN_<name>` variant name one index past the highest
/// existing `._cfgNNNN_<name>` sibling, mirroring Portage's `new_protect_filename`.
///
/// `target_name` is the file name of the real target (`<name>`); `existing` is
/// the set of sibling entry names in the live directory. The returned name uses
/// the four-digit zero-padded index `highest + 1`, or `0000` when no variant
/// exists yet. A gap left by a removed variant is never reused.
pub fn variant_name(target_name: &str, existing: &[String]) -> String {
    let idx = highest_variant_index(target_name, existing)
        .map(|h| h + 1)
        .unwrap_or(0);
    format!("._cfg{idx:04}_{target_name}")
}

/// The highest existing `._cfgNNNN_<target_name>` index among `existing`, or
/// `None` when no such variant exists.
fn highest_variant_index(target_name: &str, existing: &[String]) -> Option<u32> {
    let suffix = format!("_{target_name}");
    existing
        .iter()
        .filter_map(|e| e.strip_prefix("._cfg")?.strip_suffix(&suffix)?.parse().ok())
        .max()
}

/// The live path of the highest-indexed `._cfgNNNN_<target_name>` variant beside
/// `target`, or `None` when no variant exists yet. Callers compare its content
/// against a new file to reuse an identical variant instead of allocating a new
/// index.
pub fn highest_variant_path(target: &Path, target_name: &str) -> Option<PathBuf> {
    let dir = target.parent()?;
    let siblings = sibling_names(target);
    let highest = highest_variant_index(target_name, &siblings)?;
    Some(dir.join(format!("._cfg{highest:04}_{target_name}")))
}

/// The config memory (`CONFIG_MEMORY_FILE`): the md5 digests already offered to
/// the admin for each protected config path, so an identical update is not
/// re-offered as a fresh `._cfg` variant (Portage's `noconfmem`).
///
/// The on-disk form is one line per path: `<install_path>\t<md5>,<md5>,...`.
#[derive(Debug, Default)]
pub(crate) struct ConfMem {
    path: PathBuf,
    offered: BTreeMap<String, Vec<String>>,
}

impl ConfMem {
    /// Load the config memory from `path`, returning an empty store when the file
    /// is absent or unreadable.
    pub(crate) fn load(path: PathBuf) -> Self {
        let mut offered = BTreeMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                if let Some((install_path, digests)) = line.split_once('\t') {
                    offered.insert(
                        install_path.to_string(),
                        digests.split(',').map(str::to_string).collect(),
                    );
                }
            }
        }
        Self { path, offered }
    }

    /// Whether `md5` has already been offered for `install_path`.
    pub(crate) fn already_offered(&self, install_path: &str, md5: &str) -> bool {
        self.offered
            .get(install_path)
            .is_some_and(|v| v.iter().any(|m| m == md5))
    }

    /// Record that `md5` was offered for `install_path`.
    pub(crate) fn record(&mut self, install_path: &str, md5: &str) {
        let entry = self.offered.entry(install_path.to_string()).or_default();
        if !entry.iter().any(|m| m == md5) {
            entry.push(md5.to_string());
        }
    }

    /// Persist the config memory, creating the parent directory if needed.
    pub(crate) fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        for (install_path, digests) in &self.offered {
            out.push_str(install_path);
            out.push('\t');
            out.push_str(&digests.join(","));
            out.push('\n');
        }
        std::fs::write(&self.path, out)
    }
}

/// List the sibling entry names in the directory containing `target`, returning
/// an empty list when the directory does not exist.
pub(crate) fn sibling_names(target: &Path) -> Vec<String> {
    let Some(dir) = target.parent() else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_when_under_protect_and_not_mask() {
        let cp = ConfigProtect::new(["/etc".to_string()], Vec::new());
        assert!(cp.is_protected("/etc/foo.conf"));
        assert!(cp.is_protected("/etc"));
        assert!(!cp.is_protected("/usr/bin/foo"));
    }

    #[test]
    fn mask_overrides_protect() {
        let cp = ConfigProtect::new(["/etc".to_string()], ["/etc/env.d".to_string()]);
        assert!(cp.is_protected("/etc/foo.conf"));
        assert!(!cp.is_protected("/etc/env.d/99editor"));
    }

    #[test]
    fn prefix_does_not_match_sibling_substring() {
        let cp = ConfigProtect::new(["/etc".to_string()], Vec::new());
        // `/etcfoo` must not be considered under `/etc`.
        assert!(!cp.is_protected("/etcfoo/bar"));
    }

    #[test]
    fn variant_takes_highest_plus_one_without_reusing_gaps() {
        assert_eq!(variant_name("foo.conf", &[]), "._cfg0000_foo.conf");
        assert_eq!(
            variant_name("foo.conf", &["._cfg0000_foo.conf".to_string()]),
            "._cfg0001_foo.conf"
        );
        // A gap left by a removed `._cfg0001_` is not reused; the next index is
        // one past the highest, mirroring Portage.
        assert_eq!(
            variant_name(
                "foo.conf",
                &[
                    "._cfg0000_foo.conf".to_string(),
                    "._cfg0002_foo.conf".to_string(),
                ]
            ),
            "._cfg0003_foo.conf"
        );
    }

    #[test]
    fn highest_variant_path_finds_the_top_index() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("foo.conf");
        std::fs::write(tmp.path().join("._cfg0000_foo.conf"), b"a").unwrap();
        std::fs::write(tmp.path().join("._cfg0003_foo.conf"), b"b").unwrap();
        let highest = highest_variant_path(&target, "foo.conf").unwrap();
        assert_eq!(highest, tmp.path().join("._cfg0003_foo.conf"));
        assert!(highest_variant_path(&tmp.path().join("none.conf"), "none.conf").is_none());
    }

    #[test]
    fn non_directory_entry_requires_exact_match() {
        let tmp = tempfile::tempdir().unwrap();
        // A single protected file, not a directory, under the root.
        std::fs::create_dir_all(tmp.path().join("etc")).unwrap();
        std::fs::write(tmp.path().join("etc/hosts"), b"").unwrap();
        let cp =
            ConfigProtect::with_root(tmp.path(), ["/etc/hosts".to_string()], Vec::<String>::new());
        assert!(cp.is_protected("/etc/hosts"));
        // A sibling sharing the prefix is not protected by a file entry.
        assert!(!cp.is_protected("/etc/hosts.allow"));
        assert!(!cp.is_protected("/etc/hosts/sub"));
    }

    #[test]
    fn longest_prefix_and_mask_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("etc/env.d")).unwrap();
        let cp =
            ConfigProtect::with_root(tmp.path(), ["/etc".to_string()], ["/etc/env.d".to_string()]);
        assert!(cp.is_protected("/etc/foo.conf"));
        // A masked directory of equal-or-greater length overrides protection.
        assert!(!cp.is_protected("/etc/env.d/99editor"));
    }
}
