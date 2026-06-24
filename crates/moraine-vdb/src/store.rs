//! The installed store: a primary file plus a delta journal.
//!
//! [`Store::load`] reads the primary file in one mmap-backed pass and decodes its
//! records in parallel with `rayon`, then merges the journal so the highest
//! counter per package wins. [`Store::add`] and [`Store::remove`] append to the
//! journal without rewriting the primary file. [`Store::compact`] folds the
//! journal back into the primary file and rebuilds the intern table from live
//! records only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moraine_common::{Interner, Symbol};
use rayon::prelude::*;

use crate::codec::{TokenBuilder, decode_record, encode_record};
use crate::error::{IoResultExt as _, VdbError};
use crate::journal;
use crate::record::PackageRecord;
use crate::wire::{FORMAT_VERSION, WireDelta, WireRecord, WireStore};

/// The identity of an installed package: category/package/version strings.
///
/// Used as a journal merge key. It is string-based because journal frames carry
/// their own token tables and the primary file's symbols are not comparable
/// across tables.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PackageKey {
    pub category: String,
    pub package: String,
    pub version: String,
}

/// The on-disk paths of a store rooted at a directory.
#[derive(Debug, Clone)]
pub struct StorePaths {
    /// The primary store file.
    pub primary: PathBuf,
    /// The delta journal.
    pub journal: PathBuf,
}

impl StorePaths {
    /// Default store paths under `dir`: `installed.mvdb` and `installed.journal`.
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        Self {
            primary: dir.join("installed.mvdb"),
            journal: dir.join("installed.journal"),
        }
    }
}

/// The loaded installed set, ready for queries.
pub struct Store {
    paths: StorePaths,
    interner: Arc<Interner>,
    records: Vec<PackageRecord>,
    counter: u64,
}

impl Store {
    /// Create an empty store bound to `paths` with a fresh interner.
    ///
    /// Records added later must carry [`Symbol`]s from this store's
    /// [`interner`](Self::interner); build them against it or use
    /// [`from_records`](Self::from_records) to adopt an existing interner.
    pub fn empty(paths: StorePaths) -> Self {
        Self {
            paths,
            interner: Arc::new(Interner::new()),
            records: Vec::new(),
            counter: 0,
        }
    }

    /// Create a store from records that were built against `interner`, adopting
    /// that interner so every record's [`Symbol`]s resolve.
    ///
    /// Each record is stamped with an increasing counter. This is the path the
    /// importer feeds: import produces records and one interner, then this builds
    /// a store ready to [`compact`](Self::compact).
    pub fn from_records(
        paths: StorePaths,
        interner: Arc<Interner>,
        records: Vec<PackageRecord>,
    ) -> Self {
        let mut store = Self {
            paths,
            interner,
            records: Vec::new(),
            counter: 0,
        };
        for mut record in records {
            store.counter += 1;
            record.counter = store.counter;
            store.records.push(record);
        }
        store
    }

    /// The interner backing every [`Symbol`] in the loaded records.
    pub fn interner(&self) -> &Arc<Interner> {
        &self.interner
    }

    /// The current store counter.
    pub fn counter(&self) -> u64 {
        self.counter
    }

    /// The loaded records.
    pub fn records(&self) -> &[PackageRecord] {
        &self.records
    }

    /// The store's on-disk paths.
    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }

    /// Load the store from `paths`, merging the primary file and the journal.
    ///
    /// The full installed set is read in a single bulk mmap of the primary file
    /// and its records are decoded in parallel. If the primary file is absent the
    /// load starts empty. The journal is then applied so the record with the
    /// greatest counter per package wins and a partial trailing journal record is
    /// discarded.
    pub fn load(paths: StorePaths) -> Result<Self, VdbError> {
        let span = tracing::info_span!("vdb.load", primary = %paths.primary.display());
        let _enter = span.enter();

        let interner = Arc::new(Interner::new());

        // Bulk-load the primary file.
        let (mut counter, primary_records, primary_tokens) = if paths.primary.exists() {
            let map = moraine_common::fs::mmap_read(&paths.primary)?;
            let store: WireStore =
                rmp_serde::from_slice(&map[..]).map_err(|source| VdbError::DecodeStore {
                    path: paths.primary.clone(),
                    source,
                })?;
            if store.version != FORMAT_VERSION {
                return Err(VdbError::UnsupportedVersion {
                    found: store.version,
                    expected: FORMAT_VERSION,
                });
            }
            (store.counter, store.records, Arc::new(store.tokens))
        } else {
            (0, Vec::new(), Arc::new(Vec::new()))
        };

        // Parallel decode of the primary records into the shared interner.
        let decoded: Vec<PackageRecord> = primary_records
            .par_iter()
            .map(|wire| decode_record(wire, &primary_tokens, &interner))
            .collect::<Result<_, _>>()?;

        // Index by package key, tracking the highest counter seen.
        let mut live: HashMap<PackageKey, PackageRecord> = HashMap::with_capacity(decoded.len());
        for rec in decoded {
            let key = Self::key_for(&rec, &interner);
            insert_if_newer(&mut live, key, rec);
        }

        // Apply the journal.
        if paths.journal.exists() {
            let bytes = std::fs::read(&paths.journal).with_path(&paths.journal)?;
            let deltas = journal::read_all(&bytes)?;
            for delta in deltas {
                match delta {
                    WireDelta::Add { tokens, record } => {
                        if record.counter > counter {
                            counter = record.counter;
                        }
                        let rec = decode_record(&record, &tokens, &interner)?;
                        let key = Self::key_for(&rec, &interner);
                        insert_if_newer(&mut live, key, rec);
                    }
                    WireDelta::Remove {
                        category,
                        package,
                        version,
                        counter: c,
                    } => {
                        if c > counter {
                            counter = c;
                        }
                        let key = PackageKey {
                            category,
                            package,
                            version,
                        };
                        // A removal only takes effect if it is newer than the
                        // record it targets.
                        if live.get(&key).is_some_and(|r| c >= r.counter) {
                            live.remove(&key);
                        }
                    }
                }
            }
        }

        let records: Vec<PackageRecord> = live.into_values().collect();
        tracing::info!(count = records.len(), counter, "loaded installed set");

        Ok(Self {
            paths,
            interner,
            records,
            counter,
        })
    }

    /// Add or replace `record` in the store by appending to the journal.
    ///
    /// The record is stamped with the next counter value and the in-memory set is
    /// updated so the change is visible without a reload. The primary file is not
    /// rewritten.
    pub fn add(&mut self, mut record: PackageRecord) -> Result<(), VdbError> {
        self.counter += 1;
        record.counter = self.counter;

        let mut tb = TokenBuilder::default();
        let wire = encode_record(&record, &self.interner, &mut tb);
        let delta = WireDelta::Add {
            tokens: tb.into_tokens(),
            record: Box::new(wire),
        };
        journal::append(&self.paths.journal, &delta)?;

        let key = Self::key_for(&record, &self.interner);
        self.records
            .retain(|r| Self::key_for(r, &self.interner) != key);
        self.records.push(record);
        Ok(())
    }

    /// Remove the package identified by `category/package-version` from the store
    /// by appending a removal delta. Returns whether a matching record existed.
    pub fn remove(
        &mut self,
        category: Symbol,
        package: Symbol,
        version: &str,
    ) -> Result<bool, VdbError> {
        self.counter += 1;
        let cat = self
            .interner
            .resolve(category)
            .map(|a| a.to_string())
            .unwrap_or_default();
        let pkg = self
            .interner
            .resolve(package)
            .map(|a| a.to_string())
            .unwrap_or_default();

        let delta = WireDelta::Remove {
            category: cat.clone(),
            package: pkg.clone(),
            version: version.to_string(),
            counter: self.counter,
        };
        journal::append(&self.paths.journal, &delta)?;

        let before = self.records.len();
        self.records.retain(|r| {
            !(r.category == category && r.package == package && r.version.as_str() == version)
        });
        Ok(self.records.len() != before)
    }

    /// Fold the journal into the primary file and rebuild the token table from
    /// the live records only, dropping tokens no longer referenced.
    ///
    /// After a successful compaction the journal is emptied.
    pub fn compact(&mut self) -> Result<(), VdbError> {
        let span = tracing::info_span!("vdb.compact", count = self.records.len());
        let _enter = span.enter();

        self.write_primary()?;

        // Truncate the journal: it is now fully represented in the primary file.
        if self.paths.journal.exists() {
            std::fs::write(&self.paths.journal, []).with_path(&self.paths.journal)?;
        }
        tracing::info!("compaction complete");
        Ok(())
    }

    /// Write the current in-memory set to the primary file atomically, rebuilding
    /// the token table from live records. Does not touch the journal.
    pub fn write_primary(&self) -> Result<(), VdbError> {
        let mut tb = TokenBuilder::default();
        let records: Vec<WireRecord> = self
            .records
            .iter()
            .map(|rec| encode_record(rec, &self.interner, &mut tb))
            .collect();
        let store = WireStore {
            version: FORMAT_VERSION,
            counter: self.counter,
            tokens: tb.into_tokens(),
            records,
        };
        let bytes = rmp_serde::to_vec(&store).map_err(|source| VdbError::EncodeStore { source })?;
        moraine_common::fs::atomic_write(&self.paths.primary, &bytes)?;
        Ok(())
    }

    pub(crate) fn key_for(rec: &PackageRecord, interner: &Interner) -> PackageKey {
        PackageKey {
            category: interner
                .resolve(rec.category)
                .map(|a| a.to_string())
                .unwrap_or_default(),
            package: interner
                .resolve(rec.package)
                .map(|a| a.to_string())
                .unwrap_or_default(),
            version: rec.version.as_str().to_string(),
        }
    }
}

/// Insert `rec` under `key`, keeping whichever record has the greater counter.
fn insert_if_newer(
    live: &mut HashMap<PackageKey, PackageRecord>,
    key: PackageKey,
    rec: PackageRecord,
) {
    match live.get(&key) {
        Some(existing) if existing.counter >= rec.counter => {}
        _ => {
            live.insert(key, rec);
        }
    }
}
