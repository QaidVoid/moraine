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
    /// Fetch and parse the `Packages` index from the first reachable host in
    /// `uris`, returning `None` when none are reachable.
    pub fn load(uris: &[String], fetch: FetchCommand, stage: &Path) -> Option<IndexedBinhost> {
        std::fs::create_dir_all(stage).ok()?;
        for base in uris {
            let dest = stage.join("Packages");
            let url = format!("{}/Packages", base.trim_end_matches('/'));
            if fetch.run(&url, &dest).is_err() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&dest) else {
                continue;
            };
            if let Ok(index) = PackagesIndex::parse(&text) {
                return Some(IndexedBinhost {
                    base_uri: base.trim_end_matches('/').to_owned(),
                    index,
                    fetch,
                    stage: stage.to_path_buf(),
                });
            }
        }
        None
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
    fn entry(&self, cpv: &str) -> Option<&moraine_binpkg::PackageEntry> {
        self.index.packages.iter().find(|e| e.cpv == cpv)
    }

    /// The on-host relative path of `cpv`'s container, from `PATH` or derived.
    fn path_of(&self, cpv: &str) -> Option<String> {
        let entry = self.entry(cpv)?;
        if let Some(path) = entry.metadata.get_str("PATH") {
            return Some(path);
        }
        // Fall back to the conventional `<cat>/<pf>.gpkg` layout.
        let (category, pf) = cpv.split_once('/')?;
        Some(format!("{category}/{pf}.gpkg"))
    }
}

impl BinpkgSource for IndexedBinhost {
    fn fetch(&self, task: &InstallTask) -> Result<Option<Vec<u8>>> {
        let Some(path) = self.path_of(&task.cpv) else {
            return Ok(None);
        };
        let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
        let dest = self.stage.join(format!("{pf}.gpkg"));
        let url = format!("{}/{}", self.base_uri, path);
        if self.fetch.run(&url, &dest).is_err() {
            return Ok(None);
        }
        match std::fs::read(&dest) {
            Ok(bytes) if !bytes.is_empty() => Ok(Some(bytes)),
            _ => Ok(None),
        }
    }
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
