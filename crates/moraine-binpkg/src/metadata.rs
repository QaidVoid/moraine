//! The canonical metadata map shared across binary packages and the index.
//!
//! Binary packages, the installed store, and the binhost index all carry the
//! same set of named aux keys. This module models that set as a string-keyed
//! map, the form xpak and GPKG metadata tars use, so importers and the
//! greenfield reader populate a single in-memory type. The provenance keys
//! BUILD_ID, BUILD_TIME, CHOST, USE, PROVIDES, and REQUIRES extend the
//! installed-store key set for binary artifacts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The build-identifier key, a per-build counter for a given cpv.
pub const KEY_BUILD_ID: &str = "BUILD_ID";
/// The build-time key, seconds since the Unix epoch.
pub const KEY_BUILD_TIME: &str = "BUILD_TIME";
/// The build-host CHOST triple key.
pub const KEY_CHOST: &str = "CHOST";
/// The recorded enabled USE flags key, space separated.
pub const KEY_USE: &str = "USE";
/// The soname PROVIDES key.
pub const KEY_PROVIDES: &str = "PROVIDES";
/// The soname REQUIRES key.
pub const KEY_REQUIRES: &str = "REQUIRES";
/// The slot key.
pub const KEY_SLOT: &str = "SLOT";
/// The EAPI key.
pub const KEY_EAPI: &str = "EAPI";
/// The description key (canonical name; `DESC` in the index).
pub const KEY_DESCRIPTION: &str = "DESCRIPTION";
/// The modification-time key (canonical name; `MTIME` in the index).
pub const KEY_MTIME: &str = "_mtime_";
/// The origin-repository key (canonical name; `REPO` in the index).
pub const KEY_REPOSITORY: &str = "repository";

/// The canonical metadata map: aux key name to its recorded bytes.
///
/// Values are stored as bytes because some keys (for example the saved build
/// environment) are not UTF-8. Most keys are short single-line UTF-8 strings,
/// for which [`MetadataMap::get_str`] and [`MetadataMap::set_str`] are
/// convenient.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataMap {
    entries: BTreeMap<String, Vec<u8>>,
}

impl MetadataMap {
    /// Create an empty metadata map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert raw bytes for `key`, replacing any existing value.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Vec<u8>>) {
        self.entries.insert(key.into(), value.into());
    }

    /// Set a single-line string value for `key`, trimming trailing newlines.
    pub fn set_str(&mut self, key: impl Into<String>, value: impl AsRef<str>) {
        let trimmed = value.as_ref().trim_end_matches(['\n', '\r']);
        self.entries.insert(key.into(), trimmed.as_bytes().to_vec());
    }

    /// Borrow the raw bytes recorded for `key`.
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.entries.get(key).map(Vec::as_slice)
    }

    /// Return the value for `key` decoded as a trimmed UTF-8 string.
    ///
    /// Returns `None` when the key is absent or its bytes are not valid UTF-8.
    pub fn get_str(&self, key: &str) -> Option<String> {
        let bytes = self.entries.get(key)?;
        let text = std::str::from_utf8(bytes).ok()?;
        Some(text.trim_end_matches(['\n', '\r']).to_string())
    }

    /// Whether `key` is present.
    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Remove `key`, returning its bytes if it was present.
    pub fn remove(&mut self, key: &str) -> Option<Vec<u8>> {
        self.entries.remove(key)
    }

    /// Iterate over the entries in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Vec<u8>)> {
        self.entries.iter()
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The recorded enabled USE flags, split on whitespace.
    pub fn use_flags(&self) -> Vec<String> {
        self.get_str(KEY_USE)
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// The recorded CHOST, if present.
    pub fn chost(&self) -> Option<String> {
        self.get_str(KEY_CHOST)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_str_trims_newlines() {
        let mut m = MetadataMap::new();
        m.set_str(KEY_CHOST, "x86_64-pc-linux-gnu\n");
        assert_eq!(m.chost().as_deref(), Some("x86_64-pc-linux-gnu"));
    }

    #[test]
    fn use_flags_split_on_whitespace() {
        let mut m = MetadataMap::new();
        m.set_str(KEY_USE, "ssl  zlib\nthreads");
        assert_eq!(m.use_flags(), vec!["ssl", "zlib", "threads"]);
    }

    #[test]
    fn raw_bytes_preserved() {
        let mut m = MetadataMap::new();
        m.insert("environment.bz2", vec![0u8, 159, 146, 150]);
        assert_eq!(m.get("environment.bz2"), Some(&[0u8, 159, 146, 150][..]));
        assert!(m.get_str("environment.bz2").is_none());
    }
}
