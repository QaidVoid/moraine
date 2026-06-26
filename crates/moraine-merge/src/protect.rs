//! CONFIG_PROTECT classification and `._cfgNNNN_` variant naming.
//!
//! A target path is protected when it lies under a `CONFIG_PROTECT` directory
//! and not under a `CONFIG_PROTECT_MASK` directory, because the mask takes
//! precedence over protection. A protected path that already exists on the live
//! system and whose new content differs is never overwritten: the new content is
//! written to a sibling named `._cfgNNNN_<name>` at the lowest unused index, and
//! the path is recorded as a pending config update.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The CONFIG_PROTECT policy: the protected and masked directory prefixes.
///
/// Prefixes are absolute paths within the install root, matching the install
/// paths recorded in CONTENTS. A directory prefix protects every path beneath
/// it.
#[derive(Debug, Clone, Default)]
pub struct ConfigProtect {
    protect: Vec<String>,
    mask: Vec<String>,
}

impl ConfigProtect {
    /// Build a policy from the `CONFIG_PROTECT` and `CONFIG_PROTECT_MASK`
    /// directory lists, each a set of absolute prefixes within the install root.
    pub fn new(
        protect: impl IntoIterator<Item = String>,
        mask: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            protect: protect.into_iter().map(normalize_prefix).collect(),
            mask: mask.into_iter().map(normalize_prefix).collect(),
        }
    }

    /// Whether `path` (absolute within the install root) is a protected config
    /// path: under a protected prefix and not under a mask prefix.
    pub fn is_protected(&self, path: &str) -> bool {
        let path = normalize_prefix(path.to_string());
        if self.mask.iter().any(|m| under(&path, m)) {
            return false;
        }
        self.protect.iter().any(|p| under(&path, p))
    }
}

/// Whether `path` is equal to or lies beneath the directory prefix `prefix`.
fn under(path: &str, prefix: &str) -> bool {
    if path == prefix {
        return true;
    }
    match path.strip_prefix(prefix) {
        Some(rest) => rest.starts_with('/'),
        None => false,
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

/// Compute the `._cfgNNNN_<name>` variant path at the lowest unused index for
/// the real target path `target`, given the directory entries that already
/// exist beside it.
///
/// `target_name` is the file name of the real target (`<name>`); `existing` is
/// the set of sibling entry names in the live directory. The returned name uses
/// the four-digit zero-padded lowest index not already taken by an existing
/// `._cfgNNNN_<name>` entry.
pub fn variant_name(target_name: &str, existing: &[String]) -> String {
    let prefix_for = |idx: u32| format!("._cfg{idx:04}_{target_name}");
    let mut idx = 0u32;
    loop {
        let candidate = prefix_for(idx);
        if !existing.iter().any(|e| e == &candidate) {
            return candidate;
        }
        idx += 1;
    }
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
    fn variant_takes_lowest_unused_index() {
        assert_eq!(variant_name("foo.conf", &[]), "._cfg0000_foo.conf");
        assert_eq!(
            variant_name("foo.conf", &["._cfg0000_foo.conf".to_string()]),
            "._cfg0001_foo.conf"
        );
        assert_eq!(
            variant_name(
                "foo.conf",
                &[
                    "._cfg0000_foo.conf".to_string(),
                    "._cfg0002_foo.conf".to_string(),
                ]
            ),
            "._cfg0001_foo.conf"
        );
    }
}
