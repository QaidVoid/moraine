//! The typed CONTENTS file manifest.
//!
//! Each installed package records the files it owns. [`Contents`] models that as
//! a list of [`Entry`] values of three kinds: object files, symlinks, and
//! directories. Parent directories are synthesized implicitly for every recorded
//! path so callers can rely on complete directory coverage for ownership
//! queries, matching stock Portage's `getcontents` behaviour.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The kind and recorded fields of a single CONTENTS entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file: its md5 digest and mtime.
    Obj {
        /// The lowercase hex md5 digest recorded at install time.
        md5: String,
        /// The recorded mtime (seconds since the epoch).
        mtime: i64,
    },
    /// A symlink: its target and mtime.
    Sym {
        /// The recorded link target.
        target: String,
        /// The recorded mtime (seconds since the epoch).
        mtime: i64,
    },
    /// A directory. Carries no extra fields.
    Dir,
    /// A named pipe (FIFO). Carries no extra fields, matching Portage's `fif`
    /// CONTENTS node type.
    Fif,
    /// A character or block device node. Carries no extra fields, matching
    /// Portage's `dev` CONTENTS node type.
    Dev,
}

/// A single manifest entry: a path and its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The installed path, absolute within the install root.
    pub path: String,
    /// The entry kind and its recorded fields.
    pub kind: EntryKind,
}

/// The CONTENTS manifest of one package.
///
/// Built via [`Contents::from_entries`], which adds implicit parent directories.
/// Internally entries are keyed by path so ownership lookup is logarithmic and a
/// path appears at most once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contents {
    entries: BTreeMap<String, EntryKind>,
}

impl Contents {
    /// Build a manifest from explicit entries, synthesizing a directory entry for
    /// every parent directory up to the install root.
    ///
    /// An explicit entry always wins over a synthesized parent for the same path,
    /// so a path that is both an explicit directory and an ancestor of another
    /// path is stored once.
    pub fn from_entries(entries: impl IntoIterator<Item = Entry>) -> Self {
        let mut map: BTreeMap<String, EntryKind> = BTreeMap::new();
        let explicit: Vec<Entry> = entries.into_iter().collect();

        for entry in &explicit {
            for parent in ancestors(&entry.path) {
                map.entry(parent).or_insert(EntryKind::Dir);
            }
        }
        for entry in explicit {
            map.insert(entry.path, entry.kind);
        }
        Self { entries: map }
    }

    /// The number of entries, including synthesized parents.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the manifest holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up `path`, returning the owning entry kind when the package owns it.
    ///
    /// A path is owned when it is recorded explicitly or was synthesized as an
    /// implicit parent directory. An unrecorded path returns `None`.
    pub fn owner(&self, path: &str) -> Option<&EntryKind> {
        self.entries.get(path)
    }

    /// Whether the package owns `path`.
    pub fn owns(&self, path: &str) -> bool {
        self.entries.contains_key(path)
    }

    /// Resolve ownership of `path` against the live install root `eroot`,
    /// following symlinked parent directories.
    ///
    /// This first tries the exact recorded lookup, then mirrors Portage's
    /// `_match_contents`: it compares the queried path's basename against the
    /// recorded basenames, stats the queried path's parent directory following
    /// symlinks for its `(device, inode)` key, and treats a recorded entry as the
    /// owner when a recorded parent directory shares that inode key and holds an
    /// entry of the same basename. This recognizes a file recorded under `/lib`
    /// when queried as `/lib64` with `/lib` a symlink to `lib64`. Returns the
    /// owning entry kind when owned.
    pub fn owner_resolved(&self, path: &str, eroot: &Path) -> Option<&EntryKind> {
        use std::os::unix::fs::MetadataExt as _;

        if let Some(kind) = self.entries.get(path) {
            return Some(kind);
        }
        let (parent, basename) = split_parent(path);
        // Shortcut: no recorded entry shares this basename, so this package is not
        // the owner without examining parent inodes.
        if !self.entries.keys().any(|k| split_parent(k).1 == basename) {
            return None;
        }
        // Stat the queried parent following symlinks for its inode key.
        let queried = live_join(eroot, parent);
        let meta = std::fs::metadata(&queried).ok()?;
        let queried_key = (meta.dev(), meta.ino());
        // A recorded parent sharing that inode key and holding the basename owns
        // the queried path.
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for key in self.entries.keys() {
            let recorded_parent = split_parent(key).0;
            if !seen.insert(recorded_parent) {
                continue;
            }
            let Ok(rmeta) = std::fs::metadata(live_join(eroot, recorded_parent)) else {
                continue;
            };
            if (rmeta.dev(), rmeta.ino()) != queried_key {
                continue;
            }
            let candidate = join_path(recorded_parent, basename);
            if let Some(kind) = self.entries.get(&candidate) {
                return Some(kind);
            }
        }
        None
    }

    /// Iterate all entries in path order, including synthesized parents.
    pub fn iter(&self) -> impl Iterator<Item = Entry> + '_ {
        self.entries.iter().map(|(path, kind)| Entry {
            path: path.clone(),
            kind: kind.clone(),
        })
    }

    /// Construct directly from a path-keyed map (used by the loader).
    pub(crate) fn from_map(entries: BTreeMap<String, EntryKind>) -> Self {
        Self { entries }
    }
}

/// Split `path` into its parent directory and basename. The root `/` is used as
/// the parent when the path has no other parent component.
fn split_parent(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some(("", base)) => ("/", base),
        Some((parent, base)) => (parent, base),
        None => ("", path),
    }
}

/// Join an install-root-relative `parent` directory and `basename` into a path.
fn join_path(parent: &str, basename: &str) -> String {
    if parent == "/" {
        format!("/{basename}")
    } else {
        format!("{parent}/{basename}")
    }
}

/// Map an install-root-relative absolute path to its live filesystem path under
/// `eroot`.
fn live_join(eroot: &Path, path: &str) -> PathBuf {
    eroot.join(path.trim_start_matches('/'))
}

/// Every ancestor directory of `path`, from the install root down to the
/// immediate parent. The path itself is not included.
fn ancestors(path: &str) -> Vec<String> {
    let trimmed = path.trim_end_matches('/');
    let mut out = Vec::new();
    let mut idx = 0;
    let bytes = trimmed.as_bytes();
    // Skip a leading slash so the root itself is not emitted as an empty string.
    let start = if bytes.first() == Some(&b'/') { 1 } else { 0 };
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if b == b'/' && i > idx {
            out.push(trimmed[..i].to_string());
        }
        if b == b'/' {
            idx = i;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(path: &str) -> Entry {
        Entry {
            path: path.to_string(),
            kind: EntryKind::Obj {
                md5: "0".repeat(32),
                mtime: 1,
            },
        }
    }

    #[test]
    fn synthesizes_all_parents() {
        let c = Contents::from_entries([obj("/usr/bin/foo")]);
        assert!(matches!(c.owner("/usr"), Some(EntryKind::Dir)));
        assert!(matches!(c.owner("/usr/bin"), Some(EntryKind::Dir)));
        assert!(matches!(
            c.owner("/usr/bin/foo"),
            Some(EntryKind::Obj { .. })
        ));
    }

    #[test]
    fn explicit_dir_not_duplicated() {
        let c = Contents::from_entries([
            Entry {
                path: "/usr/bin".to_string(),
                kind: EntryKind::Dir,
            },
            obj("/usr/bin/foo"),
        ]);
        let count = c.iter().filter(|e| e.path == "/usr/bin").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn unowned_path_returns_none() {
        let c = Contents::from_entries([obj("/usr/bin/foo")]);
        assert!(c.owner("/usr/bin/bar").is_none());
        assert!(!c.owns("/etc"));
    }

    #[test]
    fn ownership_resolves_through_symlinked_parent() {
        let dir = tempfile::tempdir().unwrap();
        let eroot = dir.path();
        // Live layout: /lib64 is the real directory and /lib is a symlink to it.
        std::fs::create_dir_all(eroot.join("lib64")).unwrap();
        std::fs::write(eroot.join("lib64/foo"), b"x").unwrap();
        std::os::unix::fs::symlink("lib64", eroot.join("lib")).unwrap();

        // The package recorded the file under /lib/foo.
        let c = Contents::from_entries([obj("/lib/foo")]);

        // Queried as /lib64/foo, ownership resolves through the symlinked parent.
        assert!(c.owner_resolved("/lib64/foo", eroot).is_some());
        // The exact recorded path is still owned without resolution.
        assert!(c.owner_resolved("/lib/foo", eroot).is_some());
        // An unrecorded basename is not owned.
        assert!(c.owner_resolved("/lib64/bar", eroot).is_none());
    }
}
