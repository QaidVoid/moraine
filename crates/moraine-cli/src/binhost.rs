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

/// A binary-package source backed by the fetched and merged `Packages` indices
/// of every configured binhost.
pub struct IndexedBinhost {
    index: PackagesIndex,
    fetch: FetchCommand,
    stage: PathBuf,
}

impl IndexedBinhost {
    /// Load and merge the `Packages` index of every reachable host in `uris`.
    ///
    /// Each host's index is cached under `cache_dir` and reused as-is on later
    /// runs; it is fetched only when missing, or when `refresh` is set (the
    /// `--sync` path). The hosts are merged in the given descending-priority
    /// order with first-seen-wins precedence: a cpv already contributed by a
    /// higher-priority host is not overwritten, mirroring Portage's iteration
    /// over `_binrepos_conf` with `cpv_exists`. Each merged stanza records its
    /// own `BASE_URI`, set to the producing host's index header `URI` and falling
    /// back to that host's sync-uri, so a merged index keeps every stanza
    /// pointing at the host it came from. Returns `None` when no host yields an
    /// index.
    pub fn load(
        uris: &[String],
        fetch: FetchCommand,
        cache_dir: &Path,
        refresh: bool,
    ) -> Option<IndexedBinhost> {
        let mut merged = PackagesIndex::default();
        let mut have_any = false;
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
            let Ok(mut index) = PackagesIndex::parse(&text) else {
                continue;
            };

            // Each stanza downloads from this host's header `URI`, falling back to
            // the host's sync-uri, mirroring `BASE_URI = pkgindex.header.get(
            // "URI", base_url)`.
            let remote_base = index
                .header
                .get("URI")
                .cloned()
                .unwrap_or_else(|| base.trim_end_matches('/').to_owned());
            for entry in &mut index.packages {
                entry.metadata.set_str("BASE_URI", &remote_base);
            }

            if !have_any {
                merged.header = index.header.clone();
                have_any = true;
            }
            // First-seen-wins: keep every stanza whose cpv was not already
            // contributed by a higher-priority host. Stanzas within this host
            // sharing a cpv (multi-instance builds) are all kept.
            let existing: std::collections::BTreeSet<String> =
                merged.packages.iter().map(|p| p.cpv.clone()).collect();
            for entry in index.packages {
                if !existing.contains(&entry.cpv) {
                    merged.packages.push(entry);
                }
            }
        }
        if !have_any {
            return None;
        }
        let _ = std::fs::create_dir_all(cache_dir);
        Some(IndexedBinhost {
            index: merged,
            fetch,
            stage: cache_dir.to_path_buf(),
        })
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
        let Some(entry) = self.entry(&task.cpv) else {
            return Ok(None);
        };
        let Some(path) = self.path_of(&task.cpv) else {
            return Ok(None);
        };
        // Resolve the download base from the stanza/header `BASE_URI`/`URI`
        // injected at load time rather than a sync-uri, then join the derived
        // path, mirroring Portage's per-stanza `BASE_URI`.
        let Some(base) = entry.download_base_uri(&self.index.header) else {
            return Ok(None);
        };
        let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
        let dest = self.stage.join(format!("{pf}.gpkg.tar"));
        let url = format!("{base}/{}", path.trim_start_matches('/'));
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
        if !container_matches_entry(entry, &bytes) {
            return Ok(None);
        }
        Ok(Some(bytes))
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
    fn merges_all_hosts_by_priority() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        let high = "https://high/binpkgs";
        let low = "https://low/binpkgs";

        // High-priority host: foo (only here) and shared.
        let high_dir = cache.join(host_key(high));
        std::fs::create_dir_all(&high_dir).unwrap();
        std::fs::write(
            high_dir.join("Packages"),
            "VERSION: 0\n\nCPV: dev-libs/foo-1\nSLOT: 0\n\nCPV: dev-libs/shared-1\nSLOT: 0\n",
        )
        .unwrap();
        // Low-priority host: bar (only here) and shared (must lose to high).
        let low_dir = cache.join(host_key(low));
        std::fs::create_dir_all(&low_dir).unwrap();
        std::fs::write(
            low_dir.join("Packages"),
            "VERSION: 0\n\nCPV: dev-libs/bar-1\nSLOT: 0\n\nCPV: dev-libs/shared-1\nSLOT: 0\n",
        )
        .unwrap();

        let uris = vec![high.to_string(), low.to_string()];
        let binhost = IndexedBinhost::load(&uris, FetchCommand::default(), cache, false).unwrap();

        // A package present only on the lower-priority host is still offered.
        assert!(binhost.contains("dev-libs/bar-1"));
        // The package present only on the high host is offered too.
        assert!(binhost.contains("dev-libs/foo-1"));
        // The colliding cpv is taken from the higher-priority host: its BASE_URI
        // is the high host's base, not the low host's.
        let shared = binhost.entry("dev-libs/shared-1").unwrap();
        assert_eq!(
            shared.metadata.get_str("BASE_URI").as_deref(),
            Some("https://high/binpkgs")
        );
        // Exactly one stanza for the colliding cpv survives.
        let count = binhost
            .index
            .packages
            .iter()
            .filter(|p| p.cpv == "dev-libs/shared-1")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn fetch_uses_header_uri_not_sync_uri() {
        use moraine_binpkg::{MetadataMap, PackageEntry, PackagesIndex};

        let dir = tempfile::tempdir().unwrap();
        let mut meta = MetadataMap::new();
        // The stanza's injected BASE_URI (a package mirror) differs from the
        // index host; the fetch must use it plus the stanza PATH.
        meta.set_str("BASE_URI", "https://mirror/pkgs");
        meta.set_str("PATH", "dev-libs/foo-1.gpkg.tar");
        let mut index = PackagesIndex::new();
        index
            .header
            .insert("URI".into(), "https://indexhost/pkgs".into());
        index.packages.push(PackageEntry {
            cpv: "dev-libs/foo-1".into(),
            metadata: meta,
        });

        // A fetch command that records the requested URI and writes some bytes,
        // so the test can assert which URL the download targeted.
        let fetch = FetchCommand {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                "printf '%s' \"$2\" > \"$1.url\"; printf 'DATA' > \"$1\"".into(),
                "sh".into(),
                "{file}".into(),
                "{uri}".into(),
            ],
        };
        let binhost = IndexedBinhost {
            index,
            fetch,
            stage: dir.path().to_path_buf(),
        };
        let task = InstallTask::merge("dev-libs/foo-1", "dev-libs/foo", "0");
        let bytes = binhost.fetch(&task).unwrap();
        assert_eq!(bytes.as_deref(), Some(b"DATA".as_slice()));
        let url = std::fs::read_to_string(dir.path().join("foo-1.gpkg.tar.url")).unwrap();
        assert_eq!(url, "https://mirror/pkgs/dev-libs/foo-1.gpkg.tar");
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
