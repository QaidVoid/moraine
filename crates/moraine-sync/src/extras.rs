//! Repository settings that `moraine-repo` discovery does not retain.
//!
//! The discovery model in `moraine-repo` keeps only the `sync-*` keys from
//! `repos.conf`, so the `auto-sync` and `post-sync` keys the sync engine needs
//! are not exposed through [`moraine_repo::RepoConfig`]. This module re-reads
//! just those two keys from the `repos.conf` file or fragment directory, keyed
//! by the `repos.conf` section name, so the engine can apply them without
//! reparsing `sync-*`.

use std::collections::HashMap;
use std::path::Path;

use crate::error::SyncError;

/// The `auto-sync` and `post-sync` settings for one repository section.
#[derive(Debug, Clone, Default)]
pub struct RepoExtras {
    /// The `auto-sync` value, when present.
    pub auto_sync: Option<bool>,
    /// The `post-sync` action argv, when present.
    pub post_sync: Option<Vec<String>>,
}

/// The `auto-sync`/`post-sync` settings for every repository, by section name.
#[derive(Debug, Clone, Default)]
pub struct ExtrasMap {
    by_section: HashMap<String, RepoExtras>,
}

impl ExtrasMap {
    /// An empty map; every repository falls back to defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// The extras for a repository section, defaulting when absent.
    pub fn get(&self, section: &str) -> RepoExtras {
        self.by_section.get(section).cloned().unwrap_or_default()
    }

    /// Parse `auto-sync` and `post-sync` from a `repos.conf` file or a directory
    /// of `*.conf` fragments.
    pub fn load(repos_conf: impl AsRef<Path>) -> Result<Self, SyncError> {
        let path = repos_conf.as_ref();
        let mut map = Self::new();
        if path.is_dir() {
            let mut fragments: Vec<_> = std::fs::read_dir(path)
                .map_err(|source| SyncError::Io {
                    path: path.to_path_buf(),
                    reason: source.to_string(),
                })?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().map(|e| e == "conf").unwrap_or(false))
                .collect();
            fragments.sort();
            for frag in fragments {
                map.parse_file(&frag)?;
            }
        } else {
            map.parse_file(path)?;
        }
        Ok(map)
    }

    fn parse_file(&mut self, path: &Path) -> Result<(), SyncError> {
        let content = std::fs::read_to_string(path).map_err(|source| SyncError::Io {
            path: path.to_path_buf(),
            reason: source.to_string(),
        })?;
        let mut current: Option<String> = None;
        for raw in content.lines() {
            let line = match raw.find('#') {
                Some(0) => "",
                Some(idx) => &raw[..idx],
                None => raw,
            }
            .trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[') {
                if let Some(name) = rest.strip_suffix(']') {
                    current = Some(name.trim().to_owned());
                }
            } else if let Some((key, value)) = line.split_once('=')
                && let Some(section) = &current
            {
                let key = key.trim();
                let value = value.trim();
                let entry = self.by_section.entry(section.clone()).or_default();
                match key {
                    "auto-sync" => {
                        entry.auto_sync =
                            Some(!matches!(value, "no" | "false" | "0" | "No" | "False"));
                    }
                    "post-sync" => {
                        entry.post_sync =
                            Some(value.split_whitespace().map(str::to_owned).collect());
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parses_auto_sync_and_post_sync() {
        let tmp = TempDir::new().unwrap();
        let conf = tmp.path().join("repos.conf");
        std::fs::write(
            &conf,
            "[g]\nlocation = /x\nauto-sync = no\npost-sync = /bin/echo done\n",
        )
        .unwrap();
        let extras = ExtrasMap::load(&conf).unwrap();
        let g = extras.get("g");
        assert_eq!(g.auto_sync, Some(false));
        assert_eq!(
            g.post_sync,
            Some(vec!["/bin/echo".to_owned(), "done".to_owned()])
        );
    }
}
