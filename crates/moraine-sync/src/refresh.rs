//! Post-sync metadata refresh.
//!
//! On a successful, changed sync the engine refreshes the greenfield metadata
//! store for the repository. The refresh delegates to the `moraine-repo`
//! incremental importer, which reuses entries whose `_mtime_` and `_md5_` match
//! the source cache and re-parses only the changed ebuilds. When the existing
//! store cannot be read, or the caller forces it, the refresh falls back to a
//! full reimport with no prior entries.
//!
//! The refresh is expressed through the [`MetadataRefresher`] trait so the engine
//! can be tested against a fake that records which repositories were refreshed
//! and whether the incremental or full path ran. [`RepoRefresher`] is the
//! production implementation backed by `moraine-repo`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use moraine_repo::{ImportIssue, RepoSet, StoredEntry, import_repo, previous_index, store};
use tracing::instrument;

use crate::error::SyncError;

/// Regenerates the metadata for a single package by sourcing its ebuild with a
/// working `inherit`.
///
/// The implementation lives in `moraine-cli`, where the build engine that owns
/// the bash phase library is available. `moraine-sync` consumes it only through
/// this trait so it need not depend on `moraine-build` (which depends on
/// `moraine-repo`, which would form a cycle).
pub trait MetadataGenerator: Send + Sync {
    /// Regenerate or reuse the stored entry for `cpv` (a `category/package-version`
    /// identifier) in `repo`.
    ///
    /// `previous` is the prior stored entry for this package, if the store had
    /// one, so an unchanged package (same ebuild and eclasses) can be reused
    /// without sourcing the ebuild again. Returns `None` when no entry can be
    /// produced, leaving the package excluded.
    fn generate(&self, repo: &str, cpv: &str, previous: Option<&StoredEntry>) -> Option<Generated>;
}

/// A regenerated-or-reused entry from a [`MetadataGenerator`].
#[derive(Debug, Clone)]
pub struct Generated {
    /// The stored entry to merge into the refreshed set.
    pub entry: StoredEntry,
    /// Whether the ebuild was freshly sourced (`true`) or the prior entry was
    /// reused unchanged (`false`).
    pub regenerated: bool,
}

/// The mode a refresh used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// Reused prior entries and re-parsed only changed ebuilds.
    Incremental,
    /// Reimported the whole repository with no prior entries.
    Full,
}

/// The outcome of a metadata refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshReport {
    /// Whether the refresh ran incrementally or as a full reimport.
    pub mode: RefreshMode,
    /// The number of entries in the refreshed store.
    pub entries: usize,
    /// The number of entries regenerated from a missing or stale cache gap.
    pub regenerated: usize,
}

/// Refreshes the greenfield metadata store for one repository.
pub trait MetadataRefresher: Send + Sync {
    /// Refresh `repo`'s metadata. When `force_full` is set the refresh must use
    /// the full-reimport path rather than incremental reuse.
    fn refresh(&self, repo: &str, force_full: bool) -> Result<RefreshReport, SyncError>;
}

/// The production [`MetadataRefresher`] backed by `moraine-repo`.
pub struct RepoRefresher<'a> {
    repo_set: &'a RepoSet,
    store_dir: PathBuf,
    generator: Option<&'a dyn MetadataGenerator>,
}

impl<'a> RepoRefresher<'a> {
    /// Build a refresher writing per-repository store files under `store_dir`.
    pub fn new(repo_set: &'a RepoSet, store_dir: impl AsRef<Path>) -> Self {
        Self {
            repo_set,
            store_dir: store_dir.as_ref().to_path_buf(),
            generator: None,
        }
    }

    /// Attach a metadata generator used to regenerate entries the import
    /// reported as missing or stale, instead of leaving them excluded.
    pub fn with_generator(mut self, generator: &'a dyn MetadataGenerator) -> Self {
        self.generator = Some(generator);
        self
    }

    /// The store file path for `repo`.
    fn store_path(&self, repo: &str) -> PathBuf {
        self.store_dir.join(format!("{repo}.mrepo"))
    }
}

impl MetadataRefresher for RepoRefresher<'_> {
    #[instrument(skip(self), fields(repo = repo))]
    fn refresh(&self, repo: &str, force_full: bool) -> Result<RefreshReport, SyncError> {
        std::fs::create_dir_all(&self.store_dir).map_err(|source| SyncError::Io {
            path: self.store_dir.clone(),
            reason: source.to_string(),
        })?;

        let store_path = self.store_path(repo);

        // Seed incremental reuse from the existing store. When the store cannot
        // be read its recorded state is inconsistent with the repository, so we
        // fall back to a full reimport with no prior entries.
        let (previous, mode) = if force_full {
            (std::collections::HashMap::new(), RefreshMode::Full)
        } else {
            match store::read_entries(&store_path) {
                Ok(entries) => (previous_index(&entries), RefreshMode::Incremental),
                Err(_) => (std::collections::HashMap::new(), RefreshMode::Full),
            }
        };

        let report =
            import_repo(self.repo_set, repo, &previous).map_err(|source| SyncError::Refresh {
                repo: repo.to_owned(),
                reason: source.to_string(),
            })?;

        // Regenerate the entries the import reported as a cache gap (missing or
        // stale), merging them into the set before the single store write. Only
        // these two issues are gaps; a banned EAPI, an EAPI violation, or a
        // corrupt line is a genuine rejection and is left excluded. The prior
        // store entries are passed so an unchanged package can be reused without
        // sourcing its ebuild again.
        let mut entries = report.entries;
        let regenerated = self.regenerate_gaps(repo, &report.issues, &previous, &mut entries);

        let count = entries.len();
        store::write_store(&store_path, entries).map_err(|source| SyncError::Refresh {
            repo: repo.to_owned(),
            reason: source.to_string(),
        })?;

        Ok(RefreshReport {
            mode,
            entries: count,
            regenerated,
        })
    }
}

impl RepoRefresher<'_> {
    /// Regenerate the entries named by `MissingMetadata`/`StaleEclass` issues
    /// through the attached generator, merging each result into `entries`
    /// (replacing any same-`cpv` entry). Returns the number regenerated. With no
    /// generator this is a no-op and the gaps stay excluded.
    fn regenerate_gaps(
        &self,
        repo: &str,
        issues: &[ImportIssue],
        previous: &HashMap<(String, String, String), StoredEntry>,
        entries: &mut Vec<StoredEntry>,
    ) -> usize {
        match self.generator {
            Some(generator) => regenerate_into(generator, repo, issues, previous, entries),
            None => 0,
        }
    }
}

/// Regenerate the cache-gap entries (missing or stale) through `generator`,
/// merging each result into `entries` and replacing any same-package entry. The
/// prior store entry for a gap is passed to the generator so an unchanged
/// package is reused rather than re-sourced. Returns the count of entries that
/// were freshly regenerated (reused entries are merged but not counted).
fn regenerate_into(
    generator: &dyn MetadataGenerator,
    repo: &str,
    issues: &[ImportIssue],
    previous: &HashMap<(String, String, String), StoredEntry>,
    entries: &mut Vec<StoredEntry>,
) -> usize {
    let prev_by_cpv: HashMap<String, &StoredEntry> = previous
        .values()
        .map(|e| (format!("{}/{}-{}", e.category, e.package, e.version), e))
        .collect();

    let mut regenerated = 0;
    for issue in issues {
        let cpv = match issue {
            ImportIssue::MissingMetadata { cpv } | ImportIssue::StaleEclass { cpv, .. } => cpv,
            _ => continue,
        };
        if let Some(result) = generator.generate(repo, cpv, prev_by_cpv.get(cpv.as_str()).copied())
        {
            match entries.iter_mut().find(|e| same_package(e, &result.entry)) {
                Some(slot) => *slot = result.entry,
                None => entries.push(result.entry),
            }
            if result.regenerated {
                regenerated += 1;
            }
        }
    }
    regenerated
}

/// Whether two entries name the same `category/package-version`.
fn same_package(a: &StoredEntry, b: &StoredEntry) -> bool {
    a.category == b.category && a.package == b.package && a.version == b.version
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A fake generator returning a prebuilt entry per `cpv`. It reuses the
    /// previous entry (without regenerating) when its `md5` matches the prebuilt
    /// entry's, mimicking the unchanged-ebuild provenance check.
    struct FakeGen {
        by_cpv: HashMap<String, StoredEntry>,
    }

    impl MetadataGenerator for FakeGen {
        fn generate(
            &self,
            _repo: &str,
            cpv: &str,
            previous: Option<&StoredEntry>,
        ) -> Option<Generated> {
            let entry = self.by_cpv.get(cpv).cloned()?;
            if let Some(prev) = previous
                && prev.md5 == entry.md5
                && !entry.md5.is_empty()
            {
                return Some(Generated {
                    entry: prev.clone(),
                    regenerated: false,
                });
            }
            Some(Generated {
                entry,
                regenerated: true,
            })
        }
    }

    fn entry(category: &str, package: &str, version: &str) -> StoredEntry {
        StoredEntry {
            category: category.to_owned(),
            package: package.to_owned(),
            version: version.to_owned(),
            repository: "gentoo".to_owned(),
            eapi: "8".to_owned(),
            slot: "0".to_owned(),
            ..StoredEntry::default()
        }
    }

    fn prev_index(entries: &[StoredEntry]) -> HashMap<(String, String, String), StoredEntry> {
        entries
            .iter()
            .map(|e| {
                (
                    (e.category.clone(), e.package.clone(), e.version.clone()),
                    e.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn missing_metadata_is_regenerated_and_appended() {
        let fake = FakeGen {
            by_cpv: HashMap::from([("dev-libs/foo-1".to_owned(), entry("dev-libs", "foo", "1"))]),
        };
        let issues = vec![ImportIssue::MissingMetadata {
            cpv: "dev-libs/foo-1".to_owned(),
        }];
        let mut entries = Vec::new();
        let n = regenerate_into(&fake, "gentoo", &issues, &HashMap::new(), &mut entries);
        assert_eq!(n, 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].package, "foo");
    }

    #[test]
    fn unchanged_entry_is_reused_not_regenerated() {
        // A previous entry with the same provenance md5 is reused: merged into
        // the set but not counted as regenerated.
        let mut current = entry("dev-libs", "foo", "1");
        current.md5 = "stamp".to_owned();
        let fake = FakeGen {
            by_cpv: HashMap::from([("dev-libs/foo-1".to_owned(), current.clone())]),
        };
        let issues = vec![ImportIssue::MissingMetadata {
            cpv: "dev-libs/foo-1".to_owned(),
        }];
        let previous = prev_index(&[current]);
        let mut entries = Vec::new();
        let n = regenerate_into(&fake, "gentoo", &issues, &previous, &mut entries);
        assert_eq!(n, 0, "unchanged entry is reused, not regenerated");
        assert_eq!(entries.len(), 1, "but it is still present in the store");
    }

    #[test]
    fn stale_eclass_regeneration_replaces_existing_entry() {
        // A reused entry for the same package is overwritten by the regeneration.
        let mut fresh = entry("dev-libs", "foo", "1");
        fresh.rdepend = "dev-libs/new".to_owned();
        let fake = FakeGen {
            by_cpv: HashMap::from([("dev-libs/foo-1".to_owned(), fresh)]),
        };
        let issues = vec![ImportIssue::StaleEclass {
            cpv: "dev-libs/foo-1".to_owned(),
            eclass: "toolchain".to_owned(),
        }];
        let mut stale = entry("dev-libs", "foo", "1");
        stale.rdepend = "dev-libs/old".to_owned();
        let mut entries = vec![stale];
        let n = regenerate_into(&fake, "gentoo", &issues, &HashMap::new(), &mut entries);
        assert_eq!(n, 1);
        assert_eq!(entries.len(), 1, "no duplicate entry");
        assert_eq!(entries[0].rdepend, "dev-libs/new");
    }

    #[test]
    fn genuine_rejections_are_not_regenerated() {
        let fake = FakeGen {
            by_cpv: HashMap::from([("dev-libs/foo-1".to_owned(), entry("dev-libs", "foo", "1"))]),
        };
        let issues = vec![
            ImportIssue::BannedEapi {
                cpv: "dev-libs/foo-1".to_owned(),
                eapi: "4".to_owned(),
            },
            ImportIssue::CorruptCacheLine {
                cpv: "dev-libs/bar-1".to_owned(),
                line: "junk".to_owned(),
            },
        ];
        let mut entries = Vec::new();
        let n = regenerate_into(&fake, "gentoo", &issues, &HashMap::new(), &mut entries);
        assert_eq!(n, 0);
        assert!(entries.is_empty());
    }

    #[test]
    fn generation_failure_leaves_gap_excluded() {
        let fake = FakeGen {
            by_cpv: HashMap::new(),
        };
        let issues = vec![ImportIssue::MissingMetadata {
            cpv: "dev-libs/foo-1".to_owned(),
        }];
        let mut entries = Vec::new();
        let n = regenerate_into(&fake, "gentoo", &issues, &HashMap::new(), &mut entries);
        assert_eq!(n, 0);
        assert!(entries.is_empty());
    }
}
