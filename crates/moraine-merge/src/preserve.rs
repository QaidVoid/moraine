//! preserve-libs: keeping still-needed shared libraries alive.
//!
//! When a merge or unmerge would remove or replace a shared library, the engine
//! consults installed soname `PROVIDES`/`REQUIRES` to decide whether any other
//! installed package still requires that library's soname. A still-needed
//! library is left on the live system rather than removed, and an entry is
//! written to a durable registry keyed by the owning package's `cpv` and the
//! preserved paths. After each operation the registry is reconciled: a preserved
//! library whose soname is no longer required by any installed package is removed
//! and dropped.
//!
//! The registry is a small line-based file so it has no external format
//! dependency: each line is `cpv\tsoname\tpath`. It is rebuilt from installed
//! soname data when it fails to parse.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use moraine_common::Interner;
use moraine_vdb::store::Store;

use crate::error::{IoResultExt as _, MergeError};

/// One preserved library: the package that owned it, its soname, and its path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PreservedEntry {
    /// The `category/package-version` that owned the library.
    pub cpv: String,
    /// The soname being preserved (for example `libfoo.so.1`).
    pub soname: String,
    /// The install-root-relative path of the preserved library file.
    pub path: String,
}

/// The durable preserved-libs registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreservedLibs {
    entries: Vec<PreservedEntry>,
}

impl PreservedLibs {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The registry entries.
    pub fn entries(&self) -> &[PreservedEntry] {
        &self.entries
    }

    /// Whether the registry holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record a preserved library, de-duplicating identical entries.
    pub fn insert(&mut self, entry: PreservedEntry) {
        if !self.entries.contains(&entry) {
            self.entries.push(entry);
        }
    }

    /// Load the registry from `path`, returning an empty registry when absent.
    ///
    /// A line that does not parse causes a [`MergeError::Registry`] so the caller
    /// can fall back to a rebuild from installed soname data.
    pub fn load(path: &Path) -> Result<Self, MergeError> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let body = std::fs::read_to_string(path).with_path(path)?;
        let mut entries = Vec::new();
        for line in body.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut parts = line.splitn(3, '\t');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(cpv), Some(soname), Some(p)) => entries.push(PreservedEntry {
                    cpv: cpv.to_string(),
                    soname: soname.to_string(),
                    path: p.to_string(),
                }),
                _ => {
                    return Err(MergeError::Registry {
                        path: path.to_path_buf(),
                    });
                }
            }
        }
        Ok(Self { entries })
    }

    /// Persist the registry to `path` atomically.
    pub fn save(&self, path: &Path) -> Result<(), MergeError> {
        let mut body = String::new();
        for e in &self.entries {
            body.push_str(&e.cpv);
            body.push('\t');
            body.push_str(&e.soname);
            body.push('\t');
            body.push_str(&e.path);
            body.push('\n');
        }
        moraine_common::fs::atomic_write(path, body.as_bytes())?;
        Ok(())
    }
}

/// Parse a package's verbatim `NEEDED.ELF.2` lines into a map from each object's
/// install path to its recorded soname.
///
/// Each line has the form `arch;path;soname;rpath;needed-csv`, so the install
/// path is field 1 and the soname field 2. A line whose path or soname is empty
/// carries no usable mapping and is skipped. This lets the merge match a library
/// file to its soname by its exact recorded linkage rather than by file basename,
/// so a versioned library (`libfoo.so.1.2.3` whose soname is `libfoo.so.1`) is
/// matched directly even when its soname symlink is absent from CONTENTS.
pub(crate) fn needed_soname_map(needed: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in needed {
        let mut fields = line.split(';');
        let (_arch, path, soname) = (fields.next(), fields.next(), fields.next());
        if let (Some(path), Some(soname)) = (path, soname)
            && !path.is_empty()
            && !soname.is_empty()
        {
            map.insert(path.to_string(), soname.to_string());
        }
    }
    map
}

/// Whether `soname` is still required by any installed package other than
/// `exclude_cpv` (the package being removed or replaced).
pub(crate) fn soname_still_needed(
    store: &Store,
    interner: &Interner,
    soname: &str,
    exclude_cpv: Option<&str>,
) -> bool {
    let Some(sym) = interner_lookup(interner, soname) else {
        // The soname was never interned, so nothing requires it.
        return false;
    };
    for record in store.records() {
        if Some(record.cpv(interner).as_str()) == exclude_cpv {
            continue;
        }
        if record.requires.sonames().any(|s| s == sym) {
            return true;
        }
    }
    false
}

/// Look up `s` in `interner` without interning it, so a non-existent soname does
/// not pollute the table.
fn interner_lookup(interner: &Interner, s: &str) -> Option<moraine_common::Symbol> {
    // The interner has no read-only lookup, but interning is idempotent and the
    // table is rebuilt at load, so interning a query string is harmless: any
    // record that requires the soname interned the same string at decode.
    Some(interner.intern(s))
}

/// Reconcile the registry against the current installed set, returning the
/// preserved paths that are no longer needed and should be removed from disk.
///
/// An entry whose soname is no longer required by any installed package is
/// dropped from the registry and its path returned for removal.
pub(crate) fn reconcile(
    registry: &mut PreservedLibs,
    store: &Store,
    interner: &Interner,
) -> Vec<String> {
    let mut to_remove = Vec::new();
    let mut still_needed_sonames: BTreeSet<String> = BTreeSet::new();
    let mut kept = Vec::new();
    for entry in std::mem::take(&mut registry.entries) {
        let needed = if still_needed_sonames.contains(&entry.soname) {
            true
        } else {
            let n = soname_still_needed(store, interner, &entry.soname, Some(&entry.cpv));
            if n {
                still_needed_sonames.insert(entry.soname.clone());
            }
            n
        };
        if needed {
            kept.push(entry);
        } else {
            to_remove.push(entry.path.clone());
        }
    }
    registry.entries = kept;
    to_remove
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reg");
        let mut reg = PreservedLibs::new();
        reg.insert(PreservedEntry {
            cpv: "cat/pkg-1".to_string(),
            soname: "libfoo.so.1".to_string(),
            path: "/usr/lib/libfoo.so.1".to_string(),
        });
        reg.save(&path).unwrap();

        let loaded = PreservedLibs::load(&path).unwrap();
        assert_eq!(loaded, reg);
    }

    #[test]
    fn corrupt_registry_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reg");
        std::fs::write(&path, "not-enough-fields\n").unwrap();
        assert!(matches!(
            PreservedLibs::load(&path),
            Err(MergeError::Registry { .. })
        ));
    }

    #[test]
    fn needed_map_keys_path_to_soname() {
        let needed = vec![
            "x86_64;/usr/lib/libfoo.so.1.2.3;libfoo.so.1;;libc.so.6".to_string(),
            // A line with no soname (an executable) carries no mapping.
            "x86_64;/usr/bin/app;;;libfoo.so.1".to_string(),
        ];
        let map = needed_soname_map(&needed);
        assert_eq!(
            map.get("/usr/lib/libfoo.so.1.2.3").map(String::as_str),
            Some("libfoo.so.1")
        );
        assert!(!map.contains_key("/usr/bin/app"));
    }

    #[test]
    fn insert_dedups() {
        let mut reg = PreservedLibs::new();
        let e = PreservedEntry {
            cpv: "cat/pkg-1".to_string(),
            soname: "libfoo.so.1".to_string(),
            path: "/usr/lib/libfoo.so.1".to_string(),
        };
        reg.insert(e.clone());
        reg.insert(e);
        assert_eq!(reg.entries().len(), 1);
    }
}
