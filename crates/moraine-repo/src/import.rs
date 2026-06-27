//! The metadata importer from stock md5-cache.
//!
//! Reads `metadata/md5-cache/<cat>/<P-V>` files (one `KEY=VALUE` per line, split
//! on the first `=`), retains the resolution subset of keys plus the synthetic
//! `_mtime_`, `_md5_`, and `_eclasses_`, validates eclass md5 checksums against
//! the on-disk eclasses resolved through the masters order, parses the
//! dependency variables into ASTs gated by the entry's EAPI, and produces
//! [`crate::store::StoredEntry`] values for the metadata store.
//!
//! Import runs data-parallel across category and package directories with rayon.
//! Reimport is incremental: an existing entry is reused unchanged only when its
//! `_md5_` and its recorded `_eclasses_` md5 set both match the source cache
//! file (plus `_mtime_` for an mtime-based cache), so an eclass-driven cache
//! regeneration with an unchanged ebuild `_md5_` is re-read rather than reused
//! and only changed entries are re-parsed. Per-entry problems are collected as
//! [`ImportIssue`] rather than aborting the whole import; ebuilds with no cache
//! entry are recorded as missing metadata and never sourced.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use moraine_atom::DepSpec;
use moraine_common::Interner;
use moraine_eapi::features_for;
use rayon::prelude::*;
use tracing::instrument;

use crate::discovery::{RepoConfig, RepoSet};
use crate::error::{ImportError, RepoError, Result};
use crate::store::{StoredEntry, split_slot};

/// A non-fatal problem found while importing a single cache entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportIssue {
    /// A cache line had no `=`, so the entry was rejected.
    CorruptCacheLine {
        /// The `<cat>/<P-V>` identifier of the offending entry.
        cpv: String,
        /// The offending line content.
        line: String,
    },
    /// An eclass md5 did not match the on-disk eclass, so the entry is stale.
    StaleEclass {
        /// The `<cat>/<P-V>` identifier.
        cpv: String,
        /// The eclass whose md5 did not match.
        eclass: String,
    },
    /// A dependency variable used syntax its EAPI forbids, so the entry is
    /// invalid and excluded.
    EapiViolation {
        /// The `<cat>/<P-V>` identifier.
        cpv: String,
        /// The variable (for example `RDEPEND`) that failed.
        variable: String,
        /// A short description of the parse failure.
        reason: String,
    },
    /// An ebuild exists with no md5-cache entry; its metadata is missing.
    MissingMetadata {
        /// The `<cat>/<P-V>` identifier of the ebuild.
        cpv: String,
    },
    /// The repository has no committed metadata cache, so it yields no metadata.
    NoMetadataCache {
        /// The cache directory that was absent.
        path: PathBuf,
    },
    /// An entry's `EAPI` is banned by the repository's `eapis-banned`, so it is
    /// excluded.
    BannedEapi {
        /// The `<cat>/<P-V>` identifier.
        cpv: String,
        /// The banned EAPI.
        eapi: String,
    },
    /// An entry's `EAPI` is not a supported EAPI, so it is disregarded rather
    /// than parsed permissively.
    UnsupportedEapi {
        /// The `<cat>/<P-V>` identifier.
        cpv: String,
        /// The unsupported EAPI.
        eapi: String,
    },
    /// An md5-dict entry's `_md5_` did not match the md5 of the on-disk ebuild,
    /// so the entry is stale and excluded for gap regeneration.
    EbuildMd5Mismatch {
        /// The `<cat>/<P-V>` identifier.
        cpv: String,
    },
    /// A PMS flat_list cache file's mtime did not match the on-disk ebuild's
    /// mtime, so the entry is stale and excluded for gap regeneration.
    StaleFlatListMtime {
        /// The `<cat>/<P-V>` identifier.
        cpv: String,
    },
}

/// The result of an import: the kept entries plus the issues encountered.
#[derive(Debug, Default)]
pub struct ImportReport {
    /// The entries that passed validation and are ready for the store.
    pub entries: Vec<StoredEntry>,
    /// Per-entry problems encountered, including missing-metadata gaps.
    pub issues: Vec<ImportIssue>,
}

/// The resolution subset of cache keys carrying dependency text or tokens, plus
/// the scalar `SLOT`/`EAPI`. Other keys are dropped during import.
const DEP_KEYS: &[&str] = &["DEPEND", "RDEPEND", "BDEPEND", "PDEPEND", "IDEPEND"];

/// Import a repository's md5-cache into store entries.
///
/// `repo_set` provides the masters order for eclass resolution; `repo` names the
/// repository being imported. `previous` is an index of existing store entries
/// by `(category, package, version)` for incremental reuse; pass an empty map
/// for a cold import.
#[instrument(skip(repo_set, previous), fields(repo = repo))]
pub fn import_repo(
    repo_set: &RepoSet,
    repo: &str,
    previous: &HashMap<(String, String, String), StoredEntry>,
) -> Result<ImportReport> {
    let cfg = repo_set.get(repo).ok_or_else(|| {
        RepoError::Import(ImportError::NotADirectory {
            path: PathBuf::from(repo),
        })
    })?;

    // Select the cache directory and format (md5-dict or PMS flat_list) per the
    // repository's `cache-formats`. A synced tree without any committed cache
    // yields no metadata gracefully rather than failing the whole repository,
    // matching Portage's `_sync_callback` (a missing cache is not regenerated).
    let Some((cache_dir, format)) = cfg.selected_cache() else {
        let mut report = ImportReport::default();
        report.issues.push(ImportIssue::NoMetadataCache {
            path: cfg.md5_cache_dir(),
        });
        return Ok(report);
    };

    // Resolve on-disk eclass md5s once, through the masters search path.
    let eclass_md5 = resolve_eclass_md5(repo_set, repo);

    // Collect the (category, P-V file) work items.
    let mut work: Vec<(String, PathBuf)> = Vec::new();
    for cat_entry in read_dir(&cache_dir)? {
        if !cat_entry.is_dir() {
            continue;
        }
        let category = match cat_entry.file_name().and_then(|n| n.to_str()) {
            Some(c) => c.to_owned(),
            None => continue,
        };
        for file in read_dir(&cat_entry)? {
            if file.is_file() {
                work.push((category.clone(), file));
            }
        }
    }

    // Import each entry in parallel; per-thread interners are discarded since
    // only the raw strings are stored on disk.
    let results: Vec<EntryOutcome> = work
        .par_iter()
        .map(|(category, file)| import_one(cfg, category, file, format, &eclass_md5, previous))
        .collect();

    let mut report = ImportReport::default();
    for outcome in results {
        match outcome {
            EntryOutcome::Kept(entry) => report.entries.push(*entry),
            EntryOutcome::Rejected(issue) => report.issues.push(issue),
        }
    }

    // Record ebuilds with no md5-cache entry as missing metadata.
    report.issues.extend(missing_metadata(cfg, &report.entries));

    Ok(report)
}

/// The outcome of importing one cache file.
enum EntryOutcome {
    Kept(Box<StoredEntry>),
    Rejected(ImportIssue),
}

/// Import a single md5-cache file into an entry or a rejection.
fn import_one(
    cfg: &RepoConfig,
    category: &str,
    file: &Path,
    format: crate::flatlist::CacheFormat,
    eclass_md5: &HashMap<String, String>,
    previous: &HashMap<(String, String, String), StoredEntry>,
) -> EntryOutcome {
    use crate::flatlist::CacheFormat;
    let pv = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();
    let cpv = format!("{category}/{pv}");

    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(_) => {
            return EntryOutcome::Rejected(ImportIssue::MissingMetadata { cpv });
        }
    };

    // Parse the cache file by format. The md5-dict form is `KEY=VALUE` with a
    // corrupt-line guard; the PMS flat_list form is positional or hashed.
    let owned_fields: HashMap<String, String> = match format {
        CacheFormat::Pms => crate::flatlist::parse(&content),
        CacheFormat::Md5Dict => {
            let mut map = HashMap::new();
            for line in content.lines() {
                if line.is_empty() {
                    continue;
                }
                let Some((key, value)) = line.split_once('=') else {
                    return EntryOutcome::Rejected(ImportIssue::CorruptCacheLine {
                        cpv,
                        line: line.to_owned(),
                    });
                };
                map.insert(key.to_owned(), value.to_owned());
            }
            map
        }
    };
    let fields: HashMap<&str, &str> = owned_fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mtime = fields
        .get("_mtime_")
        .copied()
        .unwrap_or_default()
        .to_owned();
    let md5 = fields.get("_md5_").copied().unwrap_or_default().to_owned();

    let (package, version) = match split_pv(&pv) {
        Some(pair) => pair,
        None => {
            return EntryOutcome::Rejected(ImportIssue::CorruptCacheLine { cpv, line: pv });
        }
    };

    // Validate the md5-dict `_md5_` against the on-disk ebuild it describes,
    // mirroring `cache/template.py:_validate_entry`. The md5-dict validation
    // digest is the ebuild's content md5, so a mismatch means the committed
    // cache is stale; the entry is excluded and reported for gap regeneration.
    // Scoped to the md5-dict format and to entries that carry a non-empty
    // `_md5_`; the PMS flat_list format carries no ebuild `_md5_` and is
    // validated below by the cache-file-versus-ebuild mtime check instead.
    let ebuild_path = cfg
        .location
        .join(category)
        .join(&package)
        .join(format!("{pv}.ebuild"));
    if matches!(format, CacheFormat::Md5Dict)
        && !md5.is_empty()
        && let Ok(bytes) = std::fs::read(&ebuild_path)
        && moraine_common::hash::md5(&bytes) != md5
    {
        return EntryOutcome::Rejected(ImportIssue::EbuildMd5Mismatch { cpv });
    }

    // Validate the PMS flat_list format by mtime, mirroring Portage's
    // `validation_chf="mtime"` for the flat_hash database. egencache sets the
    // cache file's mtime equal to the ebuild's mtime (`cache/metadata.py`), and
    // `flat_hash._getitem` derives `_mtime_` from the cache file stat, so an
    // entry whose cache-file mtime no longer equals the ebuild's mtime is stale.
    // It is excluded and reported as a gap, the same as a md5-dict `_md5_`
    // mismatch. The check is skipped when the ebuild is absent, mirroring the
    // md5-dict path that validates only when the ebuild can be read.
    if matches!(format, CacheFormat::Pms)
        && let (Some(cache_mtime), Some(ebuild_mtime)) =
            (mtime_secs(file), mtime_secs(&ebuild_path))
        && cache_mtime != ebuild_mtime
    {
        return EntryOutcome::Rejected(ImportIssue::StaleFlatListMtime { cpv });
    }

    // Incremental reuse: the md5-dict validity key is both the ebuild content
    // hash (`_md5_`) and the inherited eclass md5 set (`_eclasses_`), mirroring
    // `cache/template.py:_validate_entry`. Reuse a previous entry only when its
    // `_md5_` matches (real gentoo md5-cache entries carry `_md5_` but no
    // `_mtime_`) AND its recorded eclass provenance matches the current cache
    // file's `_eclasses_` set. Because `_md5_` is unchanged by an eclass bump,
    // an eclass-driven egencache regeneration changes `_eclasses_` (and the
    // regenerated `RDEPEND`/`IUSE`/`KEYWORDS`) while `_md5_` stays the same, so
    // the eclass comparison forces a re-read instead of reusing stale metadata.
    // An mtime-based cache additionally requires a matching non-empty `_mtime_`.
    let current_eclasses = fields.get("_eclasses_").copied().unwrap_or_default();
    let key = (category.to_owned(), package.clone(), version.clone());
    if let Some(prev) = previous.get(&key) {
        let md5_ok = !md5.is_empty() && prev.md5 == md5;
        let eclasses_ok = prev.eclasses == current_eclasses;
        let mtime_ok = mtime.is_empty() || (prev.mtime == mtime);
        if md5_ok && eclasses_ok && mtime_ok {
            return EntryOutcome::Kept(Box::new(prev.clone()));
        }
    }

    // Validate eclass md5 pairs against on-disk eclasses.
    if let Some(eclasses) = fields.get("_eclasses_")
        && let Some(stale) = first_stale_eclass(eclasses, eclass_md5)
    {
        return EntryOutcome::Rejected(ImportIssue::StaleEclass { cpv, eclass: stale });
    }

    // Default a cache entry that omits `EAPI` to `0` (PMS), matching
    // `dbapi/porttree.py:_pull_valid_cache`. The repository `profiles/eapi`
    // value governs profile directories, not the per-ebuild cache default.
    let eapi = match fields.get("EAPI").map(|s| s.trim()) {
        Some(e) if !e.is_empty() => e.to_owned(),
        _ => "0".to_owned(),
    };
    // Reject a banned EAPI; warn on a deprecated one but keep the entry.
    if cfg.eapis_banned.iter().any(|b| b == &eapi) {
        return EntryOutcome::Rejected(ImportIssue::BannedEapi { cpv, eapi });
    }
    if cfg.eapis_deprecated.iter().any(|d| d == &eapi) {
        tracing::warn!(cpv = %cpv, eapi = %eapi, "entry uses a deprecated EAPI");
    }
    // Disregard an entry whose EAPI is not a supported EAPI rather than admitting
    // it through the permissive parse fallback, mirroring the `eapi_is_supported`
    // gate in `_pull_valid_cache`.
    if moraine_eapi::level(&eapi).is_none() {
        return EntryOutcome::Rejected(ImportIssue::UnsupportedEapi { cpv, eapi });
    }
    let features = features_for(&eapi);

    // Parse dependency variables, gated by EAPI. A failure rejects the entry.
    let interner = Interner::new();
    for var in DEP_KEYS {
        let text = fields.get(var).copied().unwrap_or_default();
        if let Err(e) = DepSpec::parse(text, features, &interner) {
            return EntryOutcome::Rejected(ImportIssue::EapiViolation {
                cpv,
                variable: (*var).to_owned(),
                reason: e.to_string(),
            });
        }
    }
    // REQUIRED_USE is a USE-flag constraint grammar (bare flags plus `||`/`^^`/
    // `??` groups), not a dependency atom expression, so it is stored verbatim
    // and parsed by the resolver rather than validated as atoms here.

    let (slot, subslot) = split_slot(fields.get("SLOT").copied().unwrap_or("0"));
    let tokens = |k: &str| {
        fields
            .get(k)
            .copied()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let text = |k: &str| fields.get(k).copied().unwrap_or_default().to_owned();

    EntryOutcome::Kept(Box::new(StoredEntry {
        category: category.to_owned(),
        package,
        version,
        repository: cfg.name.clone(),
        eapi,
        slot,
        subslot,
        depend: text("DEPEND"),
        rdepend: text("RDEPEND"),
        bdepend: text("BDEPEND"),
        pdepend: text("PDEPEND"),
        idepend: text("IDEPEND"),
        required_use: text("REQUIRED_USE"),
        src_uri: text("SRC_URI"),
        license: text("LICENSE"),
        keywords: tokens("KEYWORDS"),
        iuse: tokens("IUSE"),
        properties: tokens("PROPERTIES"),
        restrict: tokens("RESTRICT"),
        defined_phases: tokens("DEFINED_PHASES"),
        inherit: tokens("INHERIT"),
        inherited: tokens("INHERITED"),
        mtime,
        md5,
        eclasses: current_eclasses.to_owned(),
    }))
}

/// Find the first `_eclasses_` pair whose md5 does not match the on-disk
/// eclass, returning the eclass name. The md5-cache form is tab-separated
/// `name<TAB>md5` pairs.
fn first_stale_eclass(eclasses: &str, on_disk: &HashMap<String, String>) -> Option<String> {
    let parts: Vec<&str> = eclasses.split('\t').filter(|p| !p.is_empty()).collect();
    let mut i = 0;
    while i + 1 < parts.len() {
        let name = parts[i];
        let md5 = parts[i + 1];
        match on_disk.get(name) {
            Some(actual) if actual == md5 => {}
            _ => return Some(name.to_owned()),
        }
        i += 2;
    }
    None
}

/// Resolve every eclass available to `repo` through its masters order to its
/// on-disk md5, with `eclass-overrides` already applied by the search path.
/// Earlier entries in the search path win, so a closer eclass shadows a master.
#[instrument(skip(repo_set), fields(repo = repo))]
fn resolve_eclass_md5(repo_set: &RepoSet, repo: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for dir in repo_set.eclass_search_path(repo) {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let Some(name) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".eclass"))
            else {
                continue;
            };
            // Earlier search-path directories win (overrides, then self, then
            // masters), so do not overwrite an already-resolved eclass.
            if out.contains_key(name) {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                out.insert(name.to_owned(), moraine_common::hash::md5(&bytes));
            }
        }
    }
    out
}

/// Record ebuilds present in the tree but absent from the imported entries as
/// missing metadata. Scans `<repo>/<cat>/<pkg>/*.ebuild`.
fn missing_metadata(cfg: &RepoConfig, kept: &[StoredEntry]) -> Vec<ImportIssue> {
    use std::collections::HashSet;
    let have: HashSet<(&str, &str, &str)> = kept
        .iter()
        .map(|e| (e.category.as_str(), e.package.as_str(), e.version.as_str()))
        .collect();

    let mut issues = Vec::new();
    for entry in walkdir::WalkDir::new(&cfg.location)
        .min_depth(3)
        .max_depth(3)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().map(|e| e == "ebuild").unwrap_or(false)
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && let Some(parent) = path.parent()
            && let Some(pkg) = parent.file_name().and_then(|s| s.to_str())
            && let Some(cat) = parent
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
            && let Some((p, v)) = split_pv(stem)
            && p == pkg
            && !have.contains(&(cat, pkg, v.as_str()))
        {
            issues.push(ImportIssue::MissingMetadata {
                cpv: format!("{cat}/{stem}"),
            });
        }
    }
    issues
}

/// Split a `P-V` directory/file stem into `(package, version)` by finding the
/// `-` that precedes a parseable version.
fn split_pv(pv: &str) -> Option<(String, String)> {
    let bytes = pv.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'-'
            && i > 0
            && i + 1 < bytes.len()
            && moraine_version::Version::parse(&pv[i + 1..]).is_ok()
        {
            return Some((pv[..i].to_owned(), pv[i + 1..].to_owned()));
        }
    }
    None
}

/// The integer-second modification time of `path`, mirroring Portage's
/// integer mtime comparison for the flat_hash cache. Returns `None` when the
/// path cannot be stat'd.
fn mtime_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// List the paths inside a directory, returning a typed error on failure.
fn read_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let read = std::fs::read_dir(dir).map_err(|source| {
        RepoError::Common(moraine_common::CommonError::Io {
            path: dir.to_path_buf(),
            source,
        })
    })?;
    Ok(read.flatten().map(|e| e.path()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    use crate::discovery::discover;

    /// A builder for a repository tree with md5-cache entries and eclasses.
    struct RepoBuilder {
        loc: PathBuf,
    }

    impl RepoBuilder {
        fn new(root: &Path, name: &str) -> Self {
            let loc = root.join(name);
            fs::create_dir_all(loc.join("profiles")).unwrap();
            fs::create_dir_all(loc.join("metadata/md5-cache")).unwrap();
            fs::create_dir_all(loc.join("eclass")).unwrap();
            fs::write(loc.join("profiles/repo_name"), format!("{name}\n")).unwrap();
            Self { loc }
        }

        fn eclass(&self, name: &str, body: &str) -> String {
            let path = self.loc.join("eclass").join(format!("{name}.eclass"));
            fs::write(&path, body).unwrap();
            moraine_common::hash::md5(body.as_bytes())
        }

        fn cache(&self, cat: &str, pv: &str, body: &str) {
            let dir = self.loc.join("metadata/md5-cache").join(cat);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(pv), body).unwrap();
        }

        fn ebuild(&self, cat: &str, pkg: &str, pv: &str) {
            let dir = self.loc.join(cat).join(pkg);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("{pv}.ebuild")), "# ebuild\n").unwrap();
        }
    }

    fn repos_conf(root: &Path, body: &str) -> PathBuf {
        let conf = root.join("repos.conf");
        fs::write(&conf, body).unwrap();
        conf
    }

    #[test]
    fn well_formed_entry_imported() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        r.cache(
            "dev-libs",
            "openssl-3.0.1",
            "EAPI=8\nSLOT=0/3\nDEPEND=dev-lang/perl\nRDEPEND=dev-libs/zlib\nKEYWORDS=amd64 ~arm64\nIUSE=ssl\n_mtime_=1\n_md5_=abc\n",
        );
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        assert_eq!(report.entries.len(), 1);
        let e = &report.entries[0];
        assert_eq!(e.category, "dev-libs");
        assert_eq!(e.package, "openssl");
        assert_eq!(e.version, "3.0.1");
        assert_eq!(e.slot, "0");
        assert_eq!(e.subslot.as_deref(), Some("3"));
        assert_eq!(e.keywords, vec!["amd64", "~arm64"]);
        assert_eq!(e.mtime, "1");
    }

    #[test]
    fn malformed_cache_line_rejected() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        r.cache(
            "dev-libs",
            "bad-1",
            "EAPI=8\nthis line has no equals\nSLOT=0\n",
        );
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        assert!(report.entries.is_empty());
        assert!(
            report
                .issues
                .iter()
                .any(|i| matches!(i, ImportIssue::CorruptCacheLine { .. }))
        );
    }

    #[test]
    fn valid_eclass_md5_admits_entry() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        let md5 = r.eclass("toolchain", "# toolchain eclass\n");
        r.cache(
            "dev-libs",
            "a-1",
            &format!("EAPI=8\nSLOT=0\n_eclasses_=toolchain\t{md5}\n_mtime_=1\n_md5_=x\n"),
        );
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        assert_eq!(report.entries.len(), 1);
    }

    #[test]
    fn stale_eclass_md5_excludes_entry() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        r.eclass("toolchain", "# toolchain eclass\n");
        r.cache(
            "dev-libs",
            "a-1",
            "EAPI=8\nSLOT=0\n_eclasses_=toolchain\tffffffffffffffffffffffffffffffff\n_mtime_=1\n_md5_=x\n",
        );
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        assert!(report.entries.is_empty());
        assert!(report.issues.iter().any(|i| matches!(
            i,
            ImportIssue::StaleEclass { eclass, .. } if eclass == "toolchain"
        )));
    }

    #[test]
    fn eclass_resolved_through_master() {
        let tmp = TempDir::new().unwrap();
        let master = RepoBuilder::new(tmp.path(), "gentoo");
        let md5 = master.eclass("toolchain", "# master toolchain\n");
        let child = RepoBuilder::new(tmp.path(), "overlay");
        child.cache(
            "dev-libs",
            "a-1",
            &format!("EAPI=8\nSLOT=0\n_eclasses_=toolchain\t{md5}\n_mtime_=1\n_md5_=x\n"),
        );
        let conf = repos_conf(
            tmp.path(),
            &format!(
                "[gentoo]\nlocation = {}\n[overlay]\nlocation = {}\nmasters = gentoo\n",
                master.loc.display(),
                child.loc.display()
            ),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "overlay", &HashMap::new()).unwrap();
        assert_eq!(report.entries.len(), 1, "eclass from master must validate");
    }

    #[test]
    fn eapi_violation_excluded() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        // BDEPEND with a slot operator under EAPI 4 (no slot operators).
        r.cache(
            "dev-libs",
            "a-1",
            "EAPI=4\nSLOT=0\nRDEPEND=dev-libs/zlib:=\n_mtime_=1\n_md5_=x\n",
        );
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        assert!(report.entries.is_empty());
        assert!(report.issues.iter().any(|i| matches!(
            i,
            ImportIssue::EapiViolation { variable, .. } if variable == "RDEPEND"
        )));
    }

    #[test]
    fn incremental_reuse_on_match() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        r.cache(
            "dev-libs",
            "a-1",
            "EAPI=8\nSLOT=0\nRDEPEND=dev-libs/zlib\n_mtime_=10\n_md5_=hash1\n",
        );
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let first = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        assert_eq!(first.entries.len(), 1);

        // Build a previous index and mutate the stored RDEPEND so we can prove
        // the reused entry is the previous one, not a re-read of the file.
        let mut prev_entry = first.entries[0].clone();
        prev_entry.rdepend = "dev-libs/REUSED-MARKER".to_owned();
        let mut previous = HashMap::new();
        previous.insert(
            (
                prev_entry.category.clone(),
                prev_entry.package.clone(),
                prev_entry.version.clone(),
            ),
            prev_entry,
        );

        let second = import_repo(&set, "gentoo", &previous).unwrap();
        assert_eq!(second.entries.len(), 1);
        assert_eq!(
            second.entries[0].rdepend, "dev-libs/REUSED-MARKER",
            "unchanged entry must be reused, not re-read"
        );
    }

    #[test]
    fn md5_only_reuse_without_mtime() {
        // Real gentoo md5-cache entries carry `_md5_` but no `_mtime_`; the fast
        // path must still fire on a matching `_md5_`.
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        r.cache(
            "dev-libs",
            "a-1",
            "EAPI=8\nSLOT=0\nRDEPEND=dev-libs/zlib\n_md5_=h1\n",
        );
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let first = import_repo(&set, "gentoo", &HashMap::new()).unwrap();

        let mut prev = first.entries[0].clone();
        prev.rdepend = "dev-libs/REUSED".to_owned();
        let mut previous = HashMap::new();
        previous.insert(
            (
                prev.category.clone(),
                prev.package.clone(),
                prev.version.clone(),
            ),
            prev,
        );
        let second = import_repo(&set, "gentoo", &previous).unwrap();
        assert_eq!(second.entries[0].rdepend, "dev-libs/REUSED");
    }

    #[test]
    fn eclass_regen_with_same_md5_reparsed() {
        // An eclass bump regenerates the cache entry (new RDEPEND and a new
        // `_eclasses_` md5 set) while the ebuild `_md5_` is unchanged. A previous
        // entry whose recorded eclass provenance differs must not be reused; the
        // regenerated RDEPEND must win.
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        let md5 = r.eclass("toolchain", "# bumped toolchain\n");
        r.cache(
            "dev-libs",
            "a-1",
            &format!(
                "EAPI=8\nSLOT=0\nRDEPEND=dev-libs/new\n_eclasses_=toolchain\t{md5}\n_md5_=samehash\n"
            ),
        );
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let first = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        assert_eq!(first.entries.len(), 1);

        // Simulate the pre-bump store entry: same ebuild `_md5_`, a marker
        // RDEPEND, but a stale recorded eclass provenance.
        let mut prev = first.entries[0].clone();
        prev.rdepend = "dev-libs/REUSED-MARKER".to_owned();
        prev.eclasses = "toolchain\tstalemd5".to_owned();
        let mut previous = HashMap::new();
        previous.insert(
            (
                prev.category.clone(),
                prev.package.clone(),
                prev.version.clone(),
            ),
            prev,
        );

        let second = import_repo(&set, "gentoo", &previous).unwrap();
        assert_eq!(second.entries.len(), 1);
        assert_eq!(
            second.entries[0].rdepend, "dev-libs/new",
            "eclass-driven regen must be re-read, not reused"
        );
        assert_eq!(second.entries[0].eclasses, format!("toolchain\t{md5}"));
    }

    #[test]
    fn reuse_when_md5_and_eclasses_match() {
        // Matching `_md5_` and matching `_eclasses_` provenance: the previous
        // entry is reused unchanged, even with a non-empty eclass set.
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        let md5 = r.eclass("toolchain", "# toolchain\n");
        r.cache(
            "dev-libs",
            "a-1",
            &format!(
                "EAPI=8\nSLOT=0\nRDEPEND=dev-libs/zlib\n_eclasses_=toolchain\t{md5}\n_md5_=h1\n"
            ),
        );
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let first = import_repo(&set, "gentoo", &HashMap::new()).unwrap();

        // The previous entry's recorded eclasses already equal the current cache
        // file's `_eclasses_` from the first import, so reuse must fire.
        let mut prev = first.entries[0].clone();
        prev.rdepend = "dev-libs/REUSED".to_owned();
        let mut previous = HashMap::new();
        previous.insert(
            (
                prev.category.clone(),
                prev.package.clone(),
                prev.version.clone(),
            ),
            prev,
        );
        let second = import_repo(&set, "gentoo", &previous).unwrap();
        assert_eq!(second.entries[0].rdepend, "dev-libs/REUSED");
    }

    #[test]
    fn banned_eapi_rejected_and_default_eapi_applied() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        fs::write(r.loc.join("metadata/layout.conf"), "eapis-banned = 4\n").unwrap();
        fs::write(r.loc.join("profiles/eapi"), "8\n").unwrap();
        r.cache("dev-libs", "banned-1", "EAPI=4\nSLOT=0\n");
        // No EAPI line: a cache entry that omits EAPI defaults to EAPI 0 (PMS),
        // not the repository's profiles/eapi value.
        r.cache(
            "dev-libs",
            "def-1",
            "SLOT=0\nRDEPEND=dev-libs/zlib\n_md5_=x\n",
        );
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        assert_eq!(set.get("gentoo").unwrap().default_eapi, "8");
        let report = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        assert!(
            report
                .issues
                .iter()
                .any(|i| matches!(i, ImportIssue::BannedEapi { eapi, .. } if eapi == "4"))
        );
        let def = report.entries.iter().find(|e| e.package == "def").unwrap();
        assert_eq!(def.eapi, "0");
    }

    /// Set `path`'s modification time to `secs` seconds since the epoch.
    fn set_mtime(path: &Path, secs: u64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let f = fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn pms_flat_list_cache_imported() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "overlay");
        // A pms-only overlay: no md5-cache, a positional metadata/cache entry.
        fs::remove_dir_all(r.loc.join("metadata/md5-cache")).unwrap();
        fs::write(r.loc.join("metadata/layout.conf"), "cache-formats = pms\n").unwrap();
        let cache = r.loc.join("metadata/cache/dev-libs");
        fs::create_dir_all(&cache).unwrap();
        // Current Portage auxdbkey_order: IDEPEND at index 9, EAPI at index 15
        // (the sixteenth line).
        let lines = [
            "",              // DEPEND
            "dev-libs/zlib", // RDEPEND
            "0/3",           // SLOT
            "",              // SRC_URI
            "",              // RESTRICT
            "",              // HOMEPAGE
            "GPL-2",         // LICENSE
            "",              // DESCRIPTION
            "amd64",         // KEYWORDS
            "",              // IDEPEND
            "",              // INHERITED
            "ssl",           // IUSE
            "",              // REQUIRED_USE
            "",              // PDEPEND
            "",              // BDEPEND
            "8",             // EAPI
        ];
        let cache_file = cache.join("foo-1.2");
        fs::write(&cache_file, lines.join("\n")).unwrap();
        r.ebuild("dev-libs", "foo", "foo-1.2");
        let ebuild_file = r.loc.join("dev-libs/foo/foo-1.2.ebuild");
        let conf = repos_conf(
            tmp.path(),
            &format!("[overlay]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();

        // Stale: the ebuild's mtime is later than the cache file's, so the entry
        // is excluded and reported as a flat_list staleness gap.
        set_mtime(&cache_file, 1000);
        set_mtime(&ebuild_file, 2000);
        let stale = import_repo(&set, "overlay", &HashMap::new()).unwrap();
        assert!(stale.entries.iter().all(|e| e.package != "foo"));
        assert!(stale.issues.iter().any(|i| matches!(
            i,
            ImportIssue::StaleFlatListMtime { cpv } if cpv == "dev-libs/foo-1.2"
        )));

        // Fresh: equal mtimes admit the entry with its parsed fields.
        set_mtime(&ebuild_file, 1000);
        let report = import_repo(&set, "overlay", &HashMap::new()).unwrap();
        let e = report.entries.iter().find(|e| e.package == "foo").unwrap();
        assert_eq!(e.version, "1.2");
        assert_eq!(e.slot, "0");
        assert_eq!(e.subslot.as_deref(), Some("3"));
        assert_eq!(e.rdepend, "dev-libs/zlib");
        assert_eq!(e.eapi, "8");
    }

    #[test]
    fn pms_idepend_and_eapi_from_correct_slots() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "overlay");
        fs::remove_dir_all(r.loc.join("metadata/md5-cache")).unwrap();
        fs::write(r.loc.join("metadata/layout.conf"), "cache-formats = pms\n").unwrap();
        let cache = r.loc.join("metadata/cache/dev-libs");
        fs::create_dir_all(&cache).unwrap();
        // Current Portage auxdbkey_order: IDEPEND at index 9, EAPI at index 15.
        let lines = [
            "",                // DEPEND
            "dev-libs/zlib",   // RDEPEND
            "0",               // SLOT
            "",                // SRC_URI
            "",                // RESTRICT
            "",                // HOMEPAGE
            "GPL-2",           // LICENSE
            "",                // DESCRIPTION
            "amd64",           // KEYWORDS
            "dev-build/cmake", // IDEPEND
            "",                // INHERITED
            "",                // IUSE
            "",                // REQUIRED_USE
            "",                // PDEPEND
            "",                // BDEPEND
            "8",               // EAPI
        ];
        fs::write(cache.join("bar-2"), lines.join("\n")).unwrap();
        let conf = repos_conf(
            tmp.path(),
            &format!("[overlay]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "overlay", &HashMap::new()).unwrap();
        let e = report.entries.iter().find(|e| e.package == "bar").unwrap();
        assert_eq!(e.eapi, "8");
        assert_eq!(e.idepend, "dev-build/cmake");
        assert_eq!(e.rdepend, "dev-libs/zlib");
    }

    #[test]
    fn no_eapi_defaults_to_eapi_zero() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        // profiles/eapi is 8, but a cache entry omitting EAPI defaults to 0.
        fs::write(r.loc.join("profiles/eapi"), "8\n").unwrap();
        r.cache("dev-libs", "noeapi-1", "SLOT=0\n_md5_=x\n");
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        let e = report
            .entries
            .iter()
            .find(|e| e.package == "noeapi")
            .unwrap();
        assert_eq!(e.eapi, "0");
    }

    #[test]
    fn unsupported_eapi_disregarded() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        r.cache("dev-libs", "future-1", "EAPI=999\nSLOT=0\n_md5_=x\n");
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        assert!(report.entries.iter().all(|e| e.package != "future"));
        assert!(report.issues.iter().any(|i| matches!(
            i,
            ImportIssue::UnsupportedEapi { eapi, .. } if eapi == "999"
        )));
    }

    #[test]
    fn ebuild_md5_mismatch_reported_as_gap() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        // An md5-dict entry whose `_md5_` does not match the on-disk ebuild.
        r.ebuild("dev-libs", "drift", "drift-1");
        r.cache("dev-libs", "drift-1", "EAPI=8\nSLOT=0\n_md5_=deadbeef\n");
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        assert!(report.entries.iter().all(|e| e.package != "drift"));
        assert!(report.issues.iter().any(|i| matches!(
            i,
            ImportIssue::EbuildMd5Mismatch { cpv } if cpv == "dev-libs/drift-1"
        )));
    }

    #[test]
    fn matching_ebuild_md5_admits_entry() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        r.ebuild("dev-libs", "ok", "ok-1");
        // The builder writes "# ebuild\n"; use its md5 so the entry is admitted.
        let md5 = moraine_common::hash::md5(b"# ebuild\n");
        r.cache(
            "dev-libs",
            "ok-1",
            &format!("EAPI=8\nSLOT=0\n_md5_={md5}\n"),
        );
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        assert!(report.entries.iter().any(|e| e.package == "ok"));
    }

    #[test]
    fn later_declared_master_wins_eclass_tiebreak() {
        let tmp = TempDir::new().unwrap();
        // Two masters define the same eclass with differing content; the child
        // declares `masters = a b`, so the later-declared `b` must win.
        let a = RepoBuilder::new(tmp.path(), "a");
        let _a_md5 = a.eclass("shared", "# from a\n");
        let b = RepoBuilder::new(tmp.path(), "b");
        let b_md5 = b.eclass("shared", "# from b\n");
        let child = RepoBuilder::new(tmp.path(), "child");
        // The cache references `b`'s eclass md5; with the correct tiebreak the
        // resolver picks `b`'s copy and the entry validates.
        child.cache(
            "dev-libs",
            "c-1",
            &format!("EAPI=8\nSLOT=0\n_eclasses_=shared\t{b_md5}\n_mtime_=1\n_md5_=x\n"),
        );
        let conf = repos_conf(
            tmp.path(),
            &format!(
                "[a]\nlocation = {}\nmasters =\n[b]\nlocation = {}\nmasters =\n[child]\nlocation = {}\nmasters = a b\n",
                a.loc.display(),
                b.loc.display(),
                child.loc.display()
            ),
        );
        let set = discover(&conf).unwrap();
        // The search path must list b's eclass dir before a's.
        let path = set.eclass_search_path("child");
        let ia = path.iter().position(|p| p == &a.loc.join("eclass"));
        let ib = path.iter().position(|p| p == &b.loc.join("eclass"));
        assert!(ib < ia, "later-declared master b must precede a: {path:?}");
        let report = import_repo(&set, "child", &HashMap::new()).unwrap();
        assert_eq!(report.entries.len(), 1, "b's eclass md5 must validate");
    }

    #[test]
    fn missing_md5_cache_skips_gracefully() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "overlay");
        // Remove the md5-cache directory the builder created.
        fs::remove_dir_all(r.loc.join("metadata/md5-cache")).unwrap();
        let conf = repos_conf(
            tmp.path(),
            &format!("[overlay]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "overlay", &HashMap::new()).unwrap();
        assert!(report.entries.is_empty());
        assert!(
            report
                .issues
                .iter()
                .any(|i| matches!(i, ImportIssue::NoMetadataCache { .. }))
        );
    }

    #[test]
    fn changed_entry_reparsed() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        r.cache(
            "dev-libs",
            "a-1",
            "EAPI=8\nSLOT=0\nRDEPEND=dev-libs/zlib\n_mtime_=20\n_md5_=hash2\n",
        );
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();

        // Previous entry has a different mtime/md5, so it must be re-read.
        let mut prev = HashMap::new();
        prev.insert(
            ("dev-libs".to_owned(), "a".to_owned(), "1".to_owned()),
            StoredEntry {
                category: "dev-libs".to_owned(),
                package: "a".to_owned(),
                version: "1".to_owned(),
                repository: "gentoo".to_owned(),
                eapi: "8".to_owned(),
                slot: "0".to_owned(),
                subslot: None,
                depend: String::new(),
                rdepend: "dev-libs/OLD".to_owned(),
                bdepend: String::new(),
                pdepend: String::new(),
                idepend: String::new(),
                required_use: String::new(),
                src_uri: String::new(),
                license: String::new(),
                keywords: vec![],
                iuse: vec![],
                properties: vec![],
                restrict: vec![],
                defined_phases: vec![],
                inherit: vec![],
                inherited: vec![],
                mtime: "OLD".to_owned(),
                md5: "OLD".to_owned(),
                eclasses: String::new(),
            },
        );
        let report = import_repo(&set, "gentoo", &prev).unwrap();
        assert_eq!(report.entries[0].rdepend, "dev-libs/zlib");
    }

    #[test]
    fn missing_metadata_recorded_without_sourcing() {
        let tmp = TempDir::new().unwrap();
        let r = RepoBuilder::new(tmp.path(), "gentoo");
        // An ebuild with no md5-cache entry.
        r.ebuild("dev-libs", "ghost", "ghost-1.0");
        let conf = repos_conf(
            tmp.path(),
            &format!("[gentoo]\nlocation = {}\n", r.loc.display()),
        );
        let set = discover(&conf).unwrap();
        let report = import_repo(&set, "gentoo", &HashMap::new()).unwrap();
        assert!(report.entries.is_empty());
        assert!(report.issues.iter().any(|i| matches!(
            i,
            ImportIssue::MissingMetadata { cpv } if cpv == "dev-libs/ghost-1.0"
        )));
    }
}
