//! Binhost discovery and the indexed binary-package source.
//!
//! Gentoo configures binary hosts in `/etc/portage/binrepos.conf` (and the
//! legacy `PORTAGE_BINHOST` variable). Each host publishes a `Packages` index
//! listing every binary package with its `PATH` and `SIZE`. This module reads
//! those hosts, fetches and parses the index, and exposes an
//! [`IndexedBinhost`] that resolves a package's download URL and size from the
//! index rather than guessing the path.

use std::path::{Path, PathBuf};

use moraine_binpkg::PackagesIndex;
use moraine_binpkg::fetch::FetchCommand;
use moraine_config::VarMap;
use moraine_install::{BinpkgSource, InstallTask, Result};

/// The configured binhost base URIs, in priority order, from `binrepos.conf`
/// and `PORTAGE_BINHOST`.
pub fn binhost_uris(config_dir: &Path, vars: &VarMap) -> Vec<String> {
    let mut uris = parse_binrepos(&config_dir.join("etc/portage/binrepos.conf"));
    for uri in vars
        .get("PORTAGE_BINHOST")
        .unwrap_or_default()
        .split_whitespace()
    {
        let uri = uri.to_owned();
        if !uris.contains(&uri) {
            uris.push(uri);
        }
    }
    uris
}

/// Parse `binrepos.conf` (a file or a directory of `.conf` fragments), returning
/// the `sync-uri` values ordered by descending `priority`.
fn parse_binrepos(path: &Path) -> Vec<String> {
    let mut bodies = Vec::new();
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            let mut files: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "conf").unwrap_or(false))
                .collect();
            files.sort();
            for file in files {
                if let Ok(body) = std::fs::read_to_string(&file) {
                    bodies.push(body);
                }
            }
        }
    } else if let Ok(body) = std::fs::read_to_string(path) {
        bodies.push(body);
    }

    let mut hosts: Vec<(i32, String)> = Vec::new();
    let mut priority = 0i32;
    let mut sync_uri: Option<String> = None;
    let flush = |priority: i32, uri: &mut Option<String>, hosts: &mut Vec<(i32, String)>| {
        if let Some(u) = uri.take() {
            hosts.push((priority, u));
        }
    };
    for body in &bodies {
        for raw in body.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                flush(priority, &mut sync_uri, &mut hosts);
                priority = 0;
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                match key.trim() {
                    "sync-uri" => sync_uri = Some(value.trim().to_owned()),
                    "priority" => priority = value.trim().parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        flush(priority, &mut sync_uri, &mut hosts);
        priority = 0;
    }
    hosts.sort_by_key(|h| std::cmp::Reverse(h.0));
    hosts.into_iter().map(|(_, uri)| uri).collect()
}

/// A binary-package source backed by a fetched and parsed `Packages` index.
pub struct IndexedBinhost {
    base_uri: String,
    index: PackagesIndex,
    fetch: FetchCommand,
    stage: PathBuf,
}

impl IndexedBinhost {
    /// Load the `Packages` index for the first reachable host in `uris`.
    ///
    /// The index is cached under `cache_dir` and reused as-is on later runs; it
    /// is fetched only when missing, or when `refresh` is set (the `--sync`
    /// path). This mirrors how repository metadata refreshes on sync rather than
    /// on every invocation. Returns `None` when no host yields an index.
    pub fn load(
        uris: &[String],
        fetch: FetchCommand,
        cache_dir: &Path,
        refresh: bool,
    ) -> Option<IndexedBinhost> {
        for base in uris {
            let host_dir = cache_dir.join(host_key(base));
            let dest = host_dir.join("Packages");
            if refresh || !dest.exists() {
                let _ = std::fs::create_dir_all(&host_dir);
                let url = format!("{}/Packages", base.trim_end_matches('/'));
                if fetch.run(&url, &dest).is_err() && !dest.exists() {
                    continue;
                }
            }
            let Ok(text) = std::fs::read_to_string(&dest) else {
                continue;
            };
            if let Ok(index) = PackagesIndex::parse(&text) {
                return Some(IndexedBinhost {
                    base_uri: base.trim_end_matches('/').to_owned(),
                    index,
                    fetch,
                    stage: host_dir,
                });
            }
        }
        None
    }

    /// Whether the binhost index lists a package for `cpv`.
    pub fn contains(&self, cpv: &str) -> bool {
        self.entry(cpv).is_some()
    }

    /// Every `cpv` the binhost index lists, for binary-aware version selection.
    pub fn cpvs(&self) -> impl Iterator<Item = &str> {
        self.index.packages.iter().map(|e| e.cpv.as_str())
    }

    /// The newest-build metadata for each unique cpv the index lists, for
    /// building binary candidates the compatibility check consults.
    pub fn candidate_metadata(&self) -> Vec<(String, &moraine_binpkg::MetadataMap)> {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for entry in &self.index.packages {
            if seen.insert(entry.cpv.clone())
                && let Some(best) = self.entry(&entry.cpv)
            {
                out.push((entry.cpv.clone(), &best.metadata));
            }
        }
        out
    }

    /// The binary package build id recorded for `cpv`, if present.
    pub fn build_id(&self, cpv: &str) -> Option<String> {
        self.entry(cpv)?
            .metadata
            .get_str("BUILD_ID")
            .map(|s| s.trim().to_owned())
    }

    /// The download size recorded for `cpv` in the index, if present.
    pub fn size_of(&self, cpv: &str) -> Option<u64> {
        self.entry(cpv)?
            .metadata
            .get_str("SIZE")?
            .trim()
            .parse()
            .ok()
    }

    /// The index stanza for `cpv`.
    ///
    /// With `binpkg-multi-instance` a cpv can have several stanzas differing
    /// only by `BUILD_ID`; like `emerge`, the newest build (highest `BUILD_ID`)
    /// is chosen so the displayed id and the fetched container agree.
    fn entry(&self, cpv: &str) -> Option<&moraine_binpkg::PackageEntry> {
        self.index
            .packages
            .iter()
            .filter(|e| e.cpv == cpv)
            .max_by_key(|e| {
                e.metadata
                    .get_str("BUILD_ID")
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or(0)
            })
    }

    /// The on-host relative path of `cpv`'s container, from `PATH` or derived.
    fn path_of(&self, cpv: &str) -> Option<String> {
        let entry = self.entry(cpv)?;
        if let Some(path) = entry.metadata.get_str("PATH") {
            return Some(path);
        }
        // Fall back to the conventional layout. With `binpkg-multi-instance` a
        // stanza carries a `BUILD_ID`, giving `<cat>/<pf>-<buildid>.gpkg.tar`;
        // otherwise the single-instance `<cat>/<pf>.gpkg.tar`.
        let (category, pf) = cpv.split_once('/')?;
        match entry.metadata.get_str("BUILD_ID") {
            Some(id) if !id.trim().is_empty() => {
                Some(format!("{category}/{pf}-{}.gpkg.tar", id.trim()))
            }
            _ => Some(format!("{category}/{pf}.gpkg.tar")),
        }
    }
}

impl BinpkgSource for IndexedBinhost {
    fn fetch(&self, task: &InstallTask) -> Result<Option<Vec<u8>>> {
        let Some(path) = self.path_of(&task.cpv) else {
            return Ok(None);
        };
        let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
        let dest = self.stage.join(format!("{pf}.gpkg.tar"));
        let url = format!("{}/{}", self.base_uri, path);
        if self.fetch.run(&url, &dest).is_err() {
            return Ok(None);
        }
        let bytes = match std::fs::read(&dest) {
            Ok(bytes) if !bytes.is_empty() => bytes,
            _ => return Ok(None),
        };
        // Bind the downloaded bytes to the published index digests before the
        // container is trusted. A mismatch reports the container unavailable so
        // the resolver falls back to a source candidate.
        match self.entry(&task.cpv) {
            Some(entry) if !container_matches_entry(entry, &bytes) => Ok(None),
            _ => Ok(Some(bytes)),
        }
    }
}

/// Whether `bytes` matches the integrity fields recorded in the index `entry`.
///
/// Compares the file size to `SIZE` and the computed `MD5`/`SHA1` to the recorded
/// digests. When the stanza records no `SIZE`, the digest check is skipped,
/// mirroring Portage's `BinpkgVerifier` short-circuit when `size` is absent.
fn container_matches_entry(entry: &moraine_binpkg::PackageEntry, bytes: &[u8]) -> bool {
    let Some(size) = entry
        .metadata
        .get_str("SIZE")
        .and_then(|s| s.trim().parse::<u64>().ok())
    else {
        return true;
    };
    if size != bytes.len() as u64 {
        return false;
    }
    if let Some(md5) = entry.metadata.get_str("MD5")
        && md5.trim() != moraine_common::hash::md5(bytes)
    {
        return false;
    }
    if let Some(sha1) = entry.metadata.get_str("SHA1")
        && sha1.trim() != moraine_common::hash::sha1(bytes)
    {
        return false;
    }
    true
}

/// A filesystem-safe cache key for a binhost URI.
fn host_key(uri: &str) -> String {
    uri.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// A binary-package source that tries each member in order, returning the first
/// container found. Used to prefer a local package over a binhost.
pub struct ChainSource {
    sources: Vec<Box<dyn BinpkgSource>>,
}

impl ChainSource {
    /// Build a chain over the given sources, tried in order.
    pub fn new(sources: Vec<Box<dyn BinpkgSource>>) -> Self {
        ChainSource { sources }
    }
}

impl BinpkgSource for ChainSource {
    fn fetch(&self, task: &InstallTask) -> Result<Option<Vec<u8>>> {
        for source in &self.sources {
            if let Some(bytes) = source.fetch(task)? {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binrepos_by_priority() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binrepos.conf");
        std::fs::write(
            &path,
            "[gentoobinhost]\npriority = 1\nsync-uri = https://a/binpkgs\n\n\
             [local]\npriority = 5\nsync-uri = https://b/binpkgs\n",
        )
        .unwrap();
        let uris = parse_binrepos(&path);
        assert_eq!(uris, vec!["https://b/binpkgs", "https://a/binpkgs"]);
    }

    #[test]
    fn entry_picks_highest_build_id() {
        use moraine_binpkg::{MetadataMap, PackageEntry, PackagesIndex};

        let mut index = PackagesIndex::new();
        for id in ["1", "21", "3"] {
            let mut meta = MetadataMap::new();
            meta.set_str("BUILD_ID", id);
            index.packages.push(PackageEntry {
                cpv: "app-text/xmlto-0.0.28-r11".into(),
                metadata: meta,
            });
        }
        let binhost = IndexedBinhost {
            base_uri: "https://binhost".into(),
            index,
            fetch: FetchCommand::default(),
            stage: PathBuf::from("/tmp"),
        };
        assert_eq!(
            binhost.build_id("app-text/xmlto-0.0.28-r11").as_deref(),
            Some("21")
        );
    }

    #[test]
    fn container_digest_match_and_mismatch() {
        use moraine_binpkg::{MetadataMap, PackageEntry};

        let container = b"the binary package bytes";
        let mut meta = MetadataMap::new();
        meta.set_str("SIZE", container.len().to_string());
        meta.set_str("MD5", moraine_common::hash::md5(container));
        meta.set_str("SHA1", moraine_common::hash::sha1(container));
        let entry = PackageEntry {
            cpv: "dev-libs/foo-1".into(),
            metadata: meta,
        };
        // Matching bytes pass.
        assert!(container_matches_entry(&entry, container));
        // A byte-flipped container of the same length fails on the hash.
        let mut flipped = container.to_vec();
        flipped[0] ^= 0xff;
        assert!(!container_matches_entry(&entry, &flipped));
        // A truncated container fails on size.
        assert!(!container_matches_entry(
            &entry,
            &container[..container.len() - 1]
        ));

        // A stanza with no SIZE skips the check.
        let bare = PackageEntry {
            cpv: "dev-libs/bar-1".into(),
            metadata: MetadataMap::new(),
        };
        assert!(container_matches_entry(&bare, b"anything"));
    }

    #[test]
    fn portage_binhost_var_is_appended() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc/portage")).unwrap();
        let mut vars = VarMap::new();
        vars.set(
            "PORTAGE_BINHOST".to_owned(),
            "https://legacy/host".to_owned(),
        );
        let uris = binhost_uris(dir.path(), &vars);
        assert_eq!(uris, vec!["https://legacy/host"]);
    }
}
