//! The greenfield, mmap-backed metadata store.
//!
//! # On-disk format
//!
//! A store is a single file per repository, written atomically through
//! [`moraine_common::fs::atomic_write`] and read by mapping it once with
//! [`moraine_common::fs::mmap_read`]. The file is one MessagePack document
//! ([`OnDisk`]) whose layout is conceptually:
//!
//! - a **header**: a magic tag, a [`FORMAT_VERSION`], and a whole-store BLAKE2B
//!   checksum computed over the serialized payload;
//! - an **index**: entries sorted by `(category, package, version)` so a key
//!   lookup is a binary search and `cp -> versions` enumeration is a range scan,
//!   never a blob scan;
//! - a **blob region**: the per-entry records the index points into.
//!
//! On load the checksum is verified and the format version is checked. A
//! mismatch on either marks the store stale so the caller rebuilds it from the
//! importer rather than reading bad data.
//!
//! # Interning is not serialized
//!
//! [`moraine_common::Symbol`] values are per-[`moraine_common::Interner`] and are
//! not stable across runs, so neither symbols nor parsed
//! [`moraine_atom::Atom`]/[`moraine_atom::DepSpec`] ASTs are written to disk. The
//! on-disk record holds the raw dependency *strings* and plain string fields.
//! At load time the store builds its own interner and parses the strings into
//! ASTs held in memory, so runtime queries never re-parse dependency text.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use moraine_atom::DepSpec;
use moraine_common::{Interner, Symbol};
use moraine_eapi::features_for;
use moraine_version::Version;
use serde::{Deserialize, Serialize};

use crate::error::{RepoError, Result};

/// The on-disk format version. Bump when the encoding or the AST semantics
/// baked into the import change, which forces a full reimport.
pub const FORMAT_VERSION: u32 = 1;

/// The magic tag identifying a Moraine metadata store.
pub const MAGIC: [u8; 8] = *b"MORAREPO";

/// The serialized record for one ebuild version.
///
/// All fields are plain strings or string lists so nothing interner-specific is
/// written to disk. Dependency variables and `REQUIRED_USE` are kept as their
/// raw text and parsed into ASTs at load time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredEntry {
    /// The category, for example `dev-libs`.
    pub category: String,
    /// The package name, for example `openssl`.
    pub package: String,
    /// The version string, for example `3.0.1-r1`.
    pub version: String,
    /// The originating repository name.
    pub repository: String,
    /// The declared `EAPI`.
    pub eapi: String,
    /// The `SLOT` (without sub-slot).
    pub slot: String,
    /// The sub-slot, if the `SLOT` carried one.
    pub subslot: Option<String>,
    /// `DEPEND` source text.
    pub depend: String,
    /// `RDEPEND` source text.
    pub rdepend: String,
    /// `BDEPEND` source text.
    pub bdepend: String,
    /// `PDEPEND` source text.
    pub pdepend: String,
    /// `IDEPEND` source text.
    pub idepend: String,
    /// `REQUIRED_USE` source text.
    pub required_use: String,
    /// `SRC_URI` source text, needed to fetch and build from source.
    #[serde(default)]
    pub src_uri: String,
    /// `LICENSE` source text.
    #[serde(default)]
    pub license: String,
    /// `KEYWORDS` tokens.
    pub keywords: Vec<String>,
    /// `IUSE` tokens.
    pub iuse: Vec<String>,
    /// `PROPERTIES` tokens.
    pub properties: Vec<String>,
    /// `RESTRICT` tokens.
    pub restrict: Vec<String>,
    /// `DEFINED_PHASES` tokens.
    pub defined_phases: Vec<String>,
    /// `INHERIT` (direct eclass) tokens.
    pub inherit: Vec<String>,
    /// `INHERITED` transitive eclass tokens.
    #[serde(default)]
    pub inherited: Vec<String>,
    /// The source cache file's `_mtime_`, used for incremental reimport.
    pub mtime: String,
    /// The source cache file's `_md5_`, used for incremental reimport.
    pub md5: String,
}

/// The complete on-disk document.
#[derive(Debug, Serialize, Deserialize)]
struct OnDisk {
    magic: [u8; 8],
    format_version: u32,
    /// BLAKE2B over the MessagePack encoding of `entries`.
    checksum: String,
    /// Entries sorted by `(category, package, version-string)`.
    entries: Vec<StoredEntry>,
}

/// Serialize `entries` and write the store atomically to `path`.
///
/// The entries are sorted by key, a whole-store BLAKE2B checksum is computed,
/// and the document is written through a temp file plus rename so a reader never
/// observes a partial store.
pub fn write_store(path: impl AsRef<Path>, mut entries: Vec<StoredEntry>) -> Result<()> {
    entries.sort_by(|a, b| {
        (a.category.as_str(), a.package.as_str(), a.version.as_str()).cmp(&(
            b.category.as_str(),
            b.package.as_str(),
            b.version.as_str(),
        ))
    });
    let payload = rmp_serde::to_vec(&entries).map_err(|e| {
        RepoError::Import(crate::error::ImportError::Serialize {
            reason: e.to_string(),
        })
    })?;
    let checksum = moraine_common::hash::blake2b(&payload);
    let doc = OnDisk {
        magic: MAGIC,
        format_version: FORMAT_VERSION,
        checksum,
        entries,
    };
    let bytes = rmp_serde::to_vec(&doc).map_err(|e| {
        RepoError::Import(crate::error::ImportError::Serialize {
            reason: e.to_string(),
        })
    })?;
    moraine_common::fs::atomic_write(path, &bytes)?;
    Ok(())
}

/// Read the raw [`StoredEntry`] list from a store, validating the magic, format
/// version, and checksum but without parsing ASTs.
///
/// This is the incremental-reimport seed path: the returned entries retain
/// `_mtime_` and `_md5_` so the importer can reuse unchanged entries. A version
/// mismatch or checksum failure returns a typed error signalling a rebuild.
#[tracing::instrument(skip_all, fields(path = %path.as_ref().display()))]
pub fn read_entries(path: impl AsRef<Path>) -> Result<Vec<StoredEntry>> {
    let map = moraine_common::fs::mmap_read(path)?;
    let doc: OnDisk = rmp_serde::from_slice(&map).map_err(|e| RepoError::StoreCorruption {
        reason: format!("cannot deserialize store: {e}"),
    })?;
    if doc.magic != MAGIC {
        return Err(RepoError::StoreCorruption {
            reason: "bad magic tag".to_owned(),
        });
    }
    if doc.format_version != FORMAT_VERSION {
        return Err(RepoError::FormatVersionMismatch {
            found: doc.format_version,
            expected: FORMAT_VERSION,
        });
    }
    let payload = rmp_serde::to_vec(&doc.entries).map_err(|e| RepoError::StoreCorruption {
        reason: format!("cannot re-encode entries for checksum: {e}"),
    })?;
    if moraine_common::hash::blake2b(&payload) != doc.checksum {
        return Err(RepoError::StoreCorruption {
            reason: "whole-store checksum mismatch".to_owned(),
        });
    }
    Ok(doc.entries)
}

/// An entry loaded into memory with its key parsed and its dependency ASTs
/// pre-built against the store's own interner.
#[derive(Debug)]
pub struct LoadedEntry {
    /// The interned category.
    pub category: Symbol,
    /// The interned package name.
    pub package: Symbol,
    /// The parsed version.
    pub version: Version,
    /// The interned repository name.
    pub repository: Symbol,
    /// The declared EAPI string.
    pub eapi: String,
    /// The interned slot.
    pub slot: Symbol,
    /// The interned sub-slot, if any.
    pub subslot: Option<Symbol>,
    /// Parsed `DEPEND`.
    pub depend: DepSpec,
    /// Parsed `RDEPEND`.
    pub rdepend: DepSpec,
    /// Parsed `BDEPEND`.
    pub bdepend: DepSpec,
    /// Parsed `PDEPEND`.
    pub pdepend: DepSpec,
    /// Parsed `IDEPEND`.
    pub idepend: DepSpec,
    /// Parsed `REQUIRED_USE`.
    pub required_use: DepSpec,
    /// Interned `KEYWORDS`.
    pub keywords: Vec<Symbol>,
    /// Interned `IUSE`.
    pub iuse: Vec<Symbol>,
    /// Interned `PROPERTIES`.
    pub properties: Vec<Symbol>,
    /// Interned `RESTRICT`.
    pub restrict: Vec<Symbol>,
    /// Interned `DEFINED_PHASES`.
    pub defined_phases: Vec<Symbol>,
    /// Interned `INHERIT`.
    pub inherit: Vec<Symbol>,
}

/// A loaded, read-only metadata store backed by an in-memory index.
///
/// All query methods take `&self` and perform no interior mutation, so a loaded
/// store is safe to share across threads without external locking.
pub struct LoadedStore {
    interner: Arc<Interner>,
    /// Entries in `(category, package, version)` order.
    entries: Vec<LoadedEntry>,
    /// Maps `(category, package)` to a contiguous `[start, end)` range in
    /// `entries`, for `cp -> versions` enumeration without scanning blobs.
    cp_ranges: HashMap<(Symbol, Symbol), (usize, usize)>,
}

impl std::fmt::Debug for LoadedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedStore")
            .field("entries", &self.entries.len())
            .field("packages", &self.cp_ranges.len())
            .finish()
    }
}

impl LoadedStore {
    /// Load and validate a store from `path`.
    ///
    /// Verifies the magic tag, format version, and whole-store BLAKE2B checksum,
    /// then parses every entry's key, dependency variables, and `REQUIRED_USE`
    /// into ASTs against a fresh interner. A version mismatch or checksum
    /// failure returns a typed error signalling a rebuild is required.
    #[tracing::instrument(skip_all, fields(path = %path.as_ref().display()))]
    pub fn load(path: impl AsRef<Path>) -> Result<LoadedStore> {
        Self::from_entries(read_entries(path)?)
    }

    /// Load a store from `path`, parsing every entry against the shared
    /// `interner` so its symbols compare equal to other stores and to
    /// configuration atoms parsed against the same interner.
    pub fn load_with(path: impl AsRef<Path>, interner: Arc<Interner>) -> Result<LoadedStore> {
        Self::from_entries_with(read_entries(path)?, interner)
    }

    /// Build a loaded store directly from stored entries, parsing keys and ASTs.
    /// Used by [`LoadedStore::load`] and by the importer to build in memory.
    pub fn from_entries(entries: Vec<StoredEntry>) -> Result<LoadedStore> {
        Self::from_entries_with(entries, Arc::new(Interner::new()))
    }

    /// Build a loaded store from entries against the shared `interner`.
    pub fn from_entries_with(
        entries: Vec<StoredEntry>,
        interner: Arc<Interner>,
    ) -> Result<LoadedStore> {
        let mut loaded = Vec::with_capacity(entries.len());
        for e in &entries {
            loaded.push(parse_entry(e, &interner)?);
        }
        let mut store = LoadedStore {
            interner,
            entries: loaded,
            cp_ranges: HashMap::new(),
        };
        // The on-disk order is by version *string*; re-sort each cp group into
        // `moraine-version` order and rebuild the contiguous cp ranges.
        store.sort_versions();
        Ok(store)
    }

    /// The store's interner. Callers parse atoms against this interner so their
    /// symbols compare equal to the store's.
    pub fn interner(&self) -> &Interner {
        &self.interner
    }

    /// The store's shared interner handle, for parsing atoms that must compare
    /// equal to this store's symbols.
    pub fn interner_arc(&self) -> Arc<Interner> {
        Arc::clone(&self.interner)
    }

    /// All loaded entries in key order.
    pub fn entries(&self) -> &[LoadedEntry] {
        &self.entries
    }

    /// Enumerate every `(category, package)` present in the store.
    pub fn catalog(&self) -> impl Iterator<Item = (Symbol, Symbol)> + '_ {
        self.cp_ranges.keys().copied()
    }

    /// The entries for a `(category, package)`, already ordered by version.
    pub fn versions_of(&self, category: Symbol, package: Symbol) -> &[LoadedEntry] {
        match self.cp_ranges.get(&(category, package)) {
            Some(&(start, end)) => &self.entries[start..end],
            None => &[],
        }
    }

    /// Look up a single entry by exact `(category, package, version)`.
    pub fn get(
        &self,
        category: Symbol,
        package: Symbol,
        version: &Version,
    ) -> Option<&LoadedEntry> {
        let slice = self.versions_of(category, package);
        slice.iter().find(|e| &e.version == version)
    }
}

/// Parse a stored entry into a loaded entry against `interner`.
fn parse_entry(e: &StoredEntry, interner: &Interner) -> Result<LoadedEntry> {
    let features = features_for(&e.eapi);
    let version = Version::parse(&e.version).map_err(|err| RepoError::StoreCorruption {
        reason: format!("invalid version `{}`: {}", e.version, err.reason),
    })?;
    let parse_dep = |s: &str| -> Result<DepSpec> {
        DepSpec::parse(s, features, interner).map_err(|err| RepoError::StoreCorruption {
            reason: format!("invalid stored dependency `{s}`: {err}"),
        })
    };
    let intern_all = |xs: &[String]| xs.iter().map(|x| interner.intern(x)).collect::<Vec<_>>();

    Ok(LoadedEntry {
        category: interner.intern(&e.category),
        package: interner.intern(&e.package),
        version,
        repository: interner.intern(&e.repository),
        eapi: e.eapi.clone(),
        slot: interner.intern(&e.slot),
        subslot: e.subslot.as_deref().map(|s| interner.intern(s)),
        depend: parse_dep(&e.depend)?,
        rdepend: parse_dep(&e.rdepend)?,
        bdepend: parse_dep(&e.bdepend)?,
        pdepend: parse_dep(&e.pdepend)?,
        idepend: parse_dep(&e.idepend)?,
        required_use: parse_dep(&e.required_use)?,
        keywords: intern_all(&e.keywords),
        iuse: intern_all(&e.iuse),
        properties: intern_all(&e.properties),
        restrict: intern_all(&e.restrict),
        defined_phases: intern_all(&e.defined_phases),
        inherit: intern_all(&e.inherit),
    })
}

/// Parse a `SLOT` field into `(slot, subslot)`. A `SLOT` of `a/b` yields slot
/// `a` and sub-slot `b`; a trailing `=`/`*` slot operator is ignored here since
/// the store records the package's own slot, not an atom constraint.
pub fn split_slot(slot: &str) -> (String, Option<String>) {
    let slot = slot.trim().trim_end_matches(['=', '*']);
    match slot.split_once('/') {
        Some((s, sub)) => (s.to_owned(), Some(sub.to_owned())),
        None => (slot.to_owned(), None),
    }
}

impl LoadedStore {
    /// Re-sort the in-memory entries within each `cp` group into
    /// [`moraine_version`] order. Called once after building so `versions_of`
    /// and enumeration return version-ordered slices even though the on-disk
    /// order is by version string.
    pub(crate) fn sort_versions(&mut self) {
        // Sort the whole vector by (category-name, package-name, version) so the
        // cp ranges stay contiguous and versions are ordered.
        let interner = &self.interner;
        self.entries.sort_by(|a, b| {
            let ca = interner.resolve(a.category).unwrap_or_default();
            let cb = interner.resolve(b.category).unwrap_or_default();
            let pa = interner.resolve(a.package).unwrap_or_default();
            let pb = interner.resolve(b.package).unwrap_or_default();
            (ca.as_ref(), pa.as_ref())
                .cmp(&(cb.as_ref(), pb.as_ref()))
                .then_with(|| a.version.cmp(&b.version))
        });
        let mut cp_ranges: HashMap<(Symbol, Symbol), (usize, usize)> = HashMap::new();
        for (idx, e) in self.entries.iter().enumerate() {
            cp_ranges
                .entry((e.category, e.package))
                .and_modify(|r| r.1 = idx + 1)
                .or_insert((idx, idx + 1));
        }
        self.cp_ranges = cp_ranges;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(cat: &str, pkg: &str, ver: &str) -> StoredEntry {
        StoredEntry {
            category: cat.to_owned(),
            package: pkg.to_owned(),
            version: ver.to_owned(),
            repository: "gentoo".to_owned(),
            eapi: "8".to_owned(),
            slot: "0".to_owned(),
            subslot: Some("1".to_owned()),
            depend: "dev-libs/a".to_owned(),
            rdepend: "|| ( dev-libs/b dev-libs/c )".to_owned(),
            bdepend: String::new(),
            pdepend: String::new(),
            idepend: String::new(),
            required_use: String::new(),
            src_uri: "https://example.com/src.tar.gz".to_owned(),
            license: "GPL-2".to_owned(),
            keywords: vec!["amd64".to_owned(), "~arm64".to_owned()],
            iuse: vec!["ssl".to_owned()],
            properties: vec![],
            restrict: vec![],
            defined_phases: vec!["compile".to_owned()],
            inherit: vec!["toolchain".to_owned()],
            inherited: vec!["toolchain".to_owned(), "multilib".to_owned()],
            mtime: "1700000000".to_owned(),
            md5: "deadbeef".to_owned(),
        }
    }

    #[test]
    fn write_load_roundtrip_exposes_fields() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("gentoo.mrepo");
        write_store(&path, vec![entry("dev-libs", "openssl", "3.0.1")]).unwrap();

        let store = LoadedStore::load(&path).unwrap();
        let i = store.interner();
        let e = store
            .get(
                i.intern("dev-libs"),
                i.intern("openssl"),
                &Version::parse("3.0.1").unwrap(),
            )
            .unwrap();
        assert_eq!(e.eapi, "8");
        assert_eq!(e.slot, i.intern("0"));
        assert_eq!(e.subslot, Some(i.intern("1")));
        assert_eq!(e.depend.atoms().len(), 1);
        assert_eq!(e.rdepend.atoms().len(), 2);
        assert_eq!(e.repository, i.intern("gentoo"));
    }

    #[test]
    fn versions_returned_in_version_order() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.mrepo");
        // Insert out of order, including a revision; load must sort by version.
        write_store(
            &path,
            vec![
                entry("dev-libs", "x", "1.10"),
                entry("dev-libs", "x", "1.2"),
                entry("dev-libs", "x", "1.2-r1"),
            ],
        )
        .unwrap();
        let store = LoadedStore::load(&path).unwrap();
        let i = store.interner();
        let versions: Vec<&str> = store
            .versions_of(i.intern("dev-libs"), i.intern("x"))
            .iter()
            .map(|e| e.version.as_str())
            .collect();
        assert_eq!(versions, vec!["1.2", "1.2-r1", "1.10"]);
    }

    #[test]
    fn catalog_enumerates_all_cp() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.mrepo");
        write_store(
            &path,
            vec![
                entry("dev-libs", "a", "1"),
                entry("dev-libs", "b", "1"),
                entry("app-misc", "c", "1"),
            ],
        )
        .unwrap();
        let store = LoadedStore::load(&path).unwrap();
        assert_eq!(store.catalog().count(), 3);
    }

    #[test]
    fn format_version_mismatch_forces_rebuild() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.mrepo");
        // Hand-write a document with a bad format version.
        let doc = OnDisk {
            magic: MAGIC,
            format_version: FORMAT_VERSION + 99,
            checksum: String::new(),
            entries: vec![],
        };
        let bytes = rmp_serde::to_vec(&doc).unwrap();
        moraine_common::fs::atomic_write(&path, &bytes).unwrap();
        match LoadedStore::load(&path) {
            Err(RepoError::FormatVersionMismatch { found, expected }) => {
                assert_eq!(found, FORMAT_VERSION + 99);
                assert_eq!(expected, FORMAT_VERSION);
            }
            other => panic!("expected version mismatch, got {other:?}"),
        }
    }

    #[test]
    fn checksum_failure_forces_rebuild() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.mrepo");
        let doc = OnDisk {
            magic: MAGIC,
            format_version: FORMAT_VERSION,
            checksum: "not-the-real-checksum".to_owned(),
            entries: vec![entry("dev-libs", "a", "1")],
        };
        let bytes = rmp_serde::to_vec(&doc).unwrap();
        moraine_common::fs::atomic_write(&path, &bytes).unwrap();
        assert!(matches!(
            LoadedStore::load(&path),
            Err(RepoError::StoreCorruption { .. })
        ));
    }

    #[test]
    fn read_entries_retains_mtime_md5() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.mrepo");
        write_store(&path, vec![entry("dev-libs", "a", "1")]).unwrap();
        let raw = read_entries(&path).unwrap();
        assert_eq!(raw[0].mtime, "1700000000");
        assert_eq!(raw[0].md5, "deadbeef");
    }

    #[test]
    fn split_slot_parses_subslot() {
        assert_eq!(split_slot("0"), ("0".to_owned(), None));
        assert_eq!(
            split_slot("2/2.1"),
            ("2".to_owned(), Some("2.1".to_owned()))
        );
        assert_eq!(split_slot("0="), ("0".to_owned(), None));
    }
}
