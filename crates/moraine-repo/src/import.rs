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
//! Reimport is incremental: an existing entry whose `_mtime_` and `_md5_` match
//! the source cache file is reused unchanged, so only changed entries are
//! re-parsed. Per-entry problems are collected as [`ImportIssue`] rather than
//! aborting the whole import; ebuilds with no cache entry are recorded as
//! missing metadata and never sourced.

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

    let cache_dir = cfg.md5_cache_dir();
    if !cache_dir.is_dir() {
        return Err(RepoError::Import(ImportError::NotADirectory {
            path: cache_dir,
        }));
    }

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
        .map(|(category, file)| import_one(cfg, category, file, &eclass_md5, previous))
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
    eclass_md5: &HashMap<String, String>,
    previous: &HashMap<(String, String, String), StoredEntry>,
) -> EntryOutcome {
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

    // Parse KEY=VALUE, split on the first `=`. A line without `=` is corrupt.
    let mut fields: HashMap<&str, &str> = HashMap::new();
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
        fields.insert(key, value);
    }

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

    // Incremental reuse: reuse the previous entry unchanged when both _mtime_
    // and _md5_ match, so its dependencies are not re-parsed.
    let key = (category.to_owned(), package.clone(), version.clone());
    if let Some(prev) = previous.get(&key)
        && prev.mtime == mtime
        && prev.md5 == md5
        && !mtime.is_empty()
    {
        return EntryOutcome::Kept(Box::new(prev.clone()));
    }

    // Validate eclass md5 pairs against on-disk eclasses.
    if let Some(eclasses) = fields.get("_eclasses_")
        && let Some(stale) = first_stale_eclass(eclasses, eclass_md5)
    {
        return EntryOutcome::Rejected(ImportIssue::StaleEclass { cpv, eclass: stale });
    }

    let eapi = fields.get("EAPI").copied().unwrap_or("0").trim().to_owned();
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
    let required_use_text = fields.get("REQUIRED_USE").copied().unwrap_or_default();
    if let Err(e) = DepSpec::parse(required_use_text, features, &interner) {
        return EntryOutcome::Rejected(ImportIssue::EapiViolation {
            cpv,
            variable: "REQUIRED_USE".to_owned(),
            reason: e.to_string(),
        });
    }

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
        keywords: tokens("KEYWORDS"),
        iuse: tokens("IUSE"),
        properties: tokens("PROPERTIES"),
        restrict: tokens("RESTRICT"),
        defined_phases: tokens("DEFINED_PHASES"),
        inherit: tokens("INHERIT"),
        mtime,
        md5,
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
                keywords: vec![],
                iuse: vec![],
                properties: vec![],
                restrict: vec![],
                defined_phases: vec![],
                inherit: vec![],
                mtime: "OLD".to_owned(),
                md5: "OLD".to_owned(),
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
