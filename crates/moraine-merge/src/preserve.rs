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
//! dependency: each line is `cpv\tbucket\tsoname\tpath`. It is rebuilt from
//! installed soname data when it fails to parse, so an older bucket-less
//! registry triggers the rebuild rather than needing migration.

use std::collections::HashMap;
use std::path::Path;

use moraine_common::Interner;
use moraine_vdb::store::Store;

use crate::error::{IoResultExt as _, MergeError};

/// One preserved library: the package that owned it, its multilib category
/// bucket, its soname, and its path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PreservedEntry {
    /// The `category/package-version` that owned the library.
    pub cpv: String,
    /// The multilib category bucket of the soname (for example `x86_64`), so a
    /// consumer in one ABI keeps only a provider in the same ABI alive.
    pub bucket: String,
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
            let mut parts = line.splitn(4, '\t');
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(cpv), Some(bucket), Some(soname), Some(p)) => entries.push(PreservedEntry {
                    cpv: cpv.to_string(),
                    bucket: bucket.to_string(),
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
            body.push_str(&e.bucket);
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
/// install path to its recorded `(bucket, soname)`.
///
/// Each line has the form `arch;path;soname;rpath;needed-csv;multilib-category`,
/// so the install path is field 1 and the soname field 2. The bucket is the
/// sixth field (the multilib category) when present, falling back to the first
/// field (the arch), the same selection the importer makes. A line whose path or
/// soname is empty carries no usable mapping and is skipped. This lets the merge
/// match a library file to its soname by its exact recorded linkage rather than
/// by file basename, so a versioned library (`libfoo.so.1.2.3` whose soname is
/// `libfoo.so.1`) is matched directly even when its soname symlink is absent
/// from CONTENTS.
pub(crate) fn needed_soname_map(needed: &[String]) -> HashMap<String, (String, String)> {
    let mut map = HashMap::new();
    for line in needed {
        let fields: Vec<&str> = line.split(';').collect();
        if fields.len() < 3 {
            continue;
        }
        let (path, soname) = (fields[1], fields[2]);
        if path.is_empty() || soname.is_empty() {
            continue;
        }
        let bucket = fields
            .get(5)
            .filter(|f| !f.is_empty())
            .copied()
            .unwrap_or(fields[0]);
        map.insert(path.to_string(), (bucket.to_string(), soname.to_string()));
    }
    map
}

/// Whether `(bucket, soname)` is still required by any installed package other
/// than `exclude_cpv` (the package being removed or replaced).
///
/// Matching is scoped to the multilib category bucket, mirroring
/// `LinkageMapELF`'s per-arch index, so a 32-bit consumer does not keep a 64-bit
/// library alive.
pub(crate) fn soname_still_needed(
    store: &Store,
    interner: &Interner,
    bucket: &str,
    soname: &str,
    exclude_cpv: Option<&str>,
) -> bool {
    let (Some(bucket_sym), Some(soname_sym)) = (
        interner_lookup(interner, bucket),
        interner_lookup(interner, soname),
    ) else {
        // The bucket or soname was never interned, so nothing requires it.
        return false;
    };
    for record in store.records() {
        if Some(record.cpv(interner).as_str()) == exclude_cpv {
            continue;
        }
        if record.requires.requires_in(bucket_sym, soname_sym) {
            return true;
        }
    }
    false
}

/// Whether an installed package other than `owner_cpv` provides `(bucket,
/// soname)` through a real on-disk file, so a consumer can link it instead of a
/// preserved library. Mirrors `_find_unused_preserved_libs` dropping a consumer
/// edge when `findProviders` returns a non-preserved provider.
fn alternative_provider(
    store: &Store,
    interner: &Interner,
    bucket: &str,
    soname: &str,
    owner_cpv: &str,
) -> bool {
    let (Some(bucket_sym), Some(soname_sym)) = (
        interner_lookup(interner, bucket),
        interner_lookup(interner, soname),
    ) else {
        return false;
    };
    store.records().iter().any(|record| {
        record.cpv(interner) != owner_cpv && record.provides.provides_in(bucket_sym, soname_sym)
    })
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
/// A preserved entry is dropped when no installed package still requires its
/// `(bucket, soname)`, or when an alternative non-preserved installed package
/// provides the same `(bucket, soname)` through a real on-disk file even while
/// some package still requires it, because the consumers can link the
/// alternative instead. This mirrors Portage's `_find_unused_preserved_libs`.
pub(crate) fn reconcile(
    registry: &mut PreservedLibs,
    store: &Store,
    interner: &Interner,
) -> Vec<String> {
    let mut to_remove = Vec::new();
    let mut kept = Vec::new();
    for entry in std::mem::take(&mut registry.entries) {
        let still_needed = soname_still_needed(
            store,
            interner,
            &entry.bucket,
            &entry.soname,
            Some(&entry.cpv),
        );
        let has_alternative =
            alternative_provider(store, interner, &entry.bucket, &entry.soname, &entry.cpv);
        if still_needed && !has_alternative {
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
    use moraine_vdb::record::{DependSet, PackageRecord, Slot, Toolchain};
    use moraine_vdb::soname::{Provides, Requires, SonameEntry};
    use moraine_vdb::store::StorePaths;
    use std::sync::Arc;

    use super::*;

    fn entry(cpv: &str, bucket: &str, soname: &str, path: &str) -> PreservedEntry {
        PreservedEntry {
            cpv: cpv.to_string(),
            bucket: bucket.to_string(),
            soname: soname.to_string(),
            path: path.to_string(),
        }
    }

    /// A minimal installed record with the given `(bucket, soname)` provides and
    /// requires, for the still-needed and reconcile tests.
    fn record(
        interner: &Interner,
        cpv: &str,
        provides: &[(&str, &str)],
        requires: &[(&str, &str)],
    ) -> PackageRecord {
        let (cp, version) = cpv.rsplit_once('-').unwrap();
        let (category, package) = cp.split_once('/').unwrap();
        let soname_entries = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(b, s)| SonameEntry {
                    bucket: interner.intern(b),
                    soname: interner.intern(s),
                })
                .collect()
        };
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
                entries: soname_entries(provides),
            },
            requires: Requires {
                entries: soname_entries(requires),
            },
            contents: Default::default(),
            environment: None,
            inherited: Vec::new(),
            features: Vec::new(),
            size: None,
            needed: Vec::new(),
            toolchain: Toolchain::default(),
            dbdir_mtime: 0,
        }
    }

    fn store(interner: &Arc<Interner>, records: Vec<PackageRecord>) -> Store {
        Store::from_records(
            StorePaths::in_dir("/nonexistent"),
            interner.clone(),
            records,
        )
    }

    #[test]
    fn registry_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reg");
        let mut reg = PreservedLibs::new();
        reg.insert(entry(
            "cat/pkg-1",
            "x86_64",
            "libfoo.so.1",
            "/usr/lib/libfoo.so.1",
        ));
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
    fn old_bucketless_registry_triggers_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reg");
        // An older three-column `cpv\tsoname\tpath` line no longer parses.
        std::fs::write(&path, "cat/pkg-1\tlibfoo.so.1\t/usr/lib/libfoo.so.1\n").unwrap();
        assert!(matches!(
            PreservedLibs::load(&path),
            Err(MergeError::Registry { .. })
        ));
    }

    #[test]
    fn needed_map_keys_path_to_bucket_and_soname() {
        let needed = vec![
            // A six-field line: the bucket is the multilib category field.
            "X86_64;/usr/lib64/libfoo.so.1.2.3;libfoo.so.1;;libc.so.6;x86_64".to_string(),
            // A legacy five-field line buckets by field 0.
            "x86_32;/usr/lib/libbar.so.1;libbar.so.1;;libc.so.6".to_string(),
            // A line with no soname (an executable) carries no mapping.
            "x86_64;/usr/bin/app;;;libfoo.so.1".to_string(),
        ];
        let map = needed_soname_map(&needed);
        assert_eq!(
            map.get("/usr/lib64/libfoo.so.1.2.3"),
            Some(&("x86_64".to_string(), "libfoo.so.1".to_string()))
        );
        assert_eq!(
            map.get("/usr/lib/libbar.so.1"),
            Some(&("x86_32".to_string(), "libbar.so.1".to_string()))
        );
        assert!(!map.contains_key("/usr/bin/app"));
    }

    #[test]
    fn insert_dedups() {
        let mut reg = PreservedLibs::new();
        let e = entry("cat/pkg-1", "x86_64", "libfoo.so.1", "/usr/lib/libfoo.so.1");
        reg.insert(e.clone());
        reg.insert(e);
        assert_eq!(reg.entries().len(), 1);
    }

    #[test]
    fn consumer_in_other_abi_does_not_keep_library_alive() {
        let interner = Arc::new(Interner::new());
        // The only consumer requires the soname in the 64-bit bucket.
        let consumer = record(
            &interner,
            "app/consumer-1",
            &[],
            &[("x86_64", "libfoo.so.1")],
        );
        let st = store(&interner, vec![consumer]);
        // A 64-bit consumer keeps a 64-bit library alive.
        assert!(soname_still_needed(
            &st,
            &interner,
            "x86_64",
            "libfoo.so.1",
            None
        ));
        // The same soname in the 32-bit bucket is not kept alive by it.
        assert!(!soname_still_needed(
            &st,
            &interner,
            "x86_32",
            "libfoo.so.1",
            None
        ));
    }

    #[test]
    fn reconcile_drops_library_with_alternative_provider() {
        let interner = Arc::new(Interner::new());
        // A consumer still requires the soname, but an alternative non-preserved
        // package provides it in the same bucket through a real file.
        let consumer = record(
            &interner,
            "app/consumer-1",
            &[],
            &[("x86_64", "libfoo.so.1")],
        );
        let alt = record(&interner, "lib/alt-2", &[("x86_64", "libfoo.so.1")], &[]);
        let st = store(&interner, vec![consumer, alt]);

        let mut reg = PreservedLibs::new();
        reg.insert(entry(
            "lib/old-1",
            "x86_64",
            "libfoo.so.1",
            "/usr/lib64/libfoo.so.1",
        ));
        let removed = reconcile(&mut reg, &st, &interner);
        assert_eq!(removed, vec!["/usr/lib64/libfoo.so.1".to_string()]);
        assert!(reg.is_empty());
    }

    #[test]
    fn reconcile_keeps_sole_provider() {
        let interner = Arc::new(Interner::new());
        // A consumer requires the soname and no other package provides it.
        let consumer = record(
            &interner,
            "app/consumer-1",
            &[],
            &[("x86_64", "libfoo.so.1")],
        );
        let st = store(&interner, vec![consumer]);

        let mut reg = PreservedLibs::new();
        reg.insert(entry(
            "lib/old-1",
            "x86_64",
            "libfoo.so.1",
            "/usr/lib64/libfoo.so.1",
        ));
        let removed = reconcile(&mut reg, &st, &interner);
        assert!(removed.is_empty());
        assert_eq!(reg.entries().len(), 1);
    }
}
