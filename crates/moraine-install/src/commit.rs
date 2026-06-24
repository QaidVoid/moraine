//! Post-transaction commit: world set, environment regeneration, news.
//!
//! The merge engine writes the world favorites file per merge as part of its
//! commit point, so [`WorldUpdate`] here is the explicit world editor the removal
//! and oneshot paths use, not a second writer for the install path.
//! [`env_update`] regenerates the aggregated environment from the installed
//! `env.d` fragments, and [`mark_news_read`] records news items as read.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::error::{InstallError, Result};

/// The colon-joined environment variables that accumulate across `env.d`
/// fragments rather than taking a last-writer-wins value.
const COLON_VARS: &[&str] = &[
    "PATH",
    "ROOTPATH",
    "MANPATH",
    "INFOPATH",
    "LDPATH",
    "PKG_CONFIG_PATH",
    "PRELINK_PATH",
    "PRELINK_PATH_MASK",
    "CONFIG_PROTECT",
    "CONFIG_PROTECT_MASK",
];

/// An edit to the world favorites file.
#[derive(Debug, Clone, Default)]
pub struct WorldUpdate {
    /// The `category/package` keys to add.
    pub add: Vec<String>,
    /// The `category/package` keys to remove.
    pub remove: Vec<String>,
}

impl WorldUpdate {
    /// Apply the edit to the world file at `path`, keeping it sorted and unique.
    ///
    /// Adds are skipped entirely when `oneshot` is set, matching `--oneshot`.
    pub fn apply(&self, path: &Path, oneshot: bool) -> Result<()> {
        let mut set = read_lines(path)?;
        if !oneshot {
            for cp in &self.add {
                set.insert(cp.clone());
            }
        }
        for cp in &self.remove {
            set.remove(cp);
        }
        write_lines(path, &set)
    }
}

/// The files [`env_update`] regenerated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvUpdateReport {
    /// The aggregated profile environment path.
    pub profile_env: PathBuf,
    /// The dynamic-linker configuration path.
    pub ld_so_conf: PathBuf,
}

/// Regenerate the aggregated environment from the `env.d` fragments under
/// `eroot`, writing `etc/profile.env` and `etc/ld.so.conf`.
///
/// Fragments are read in sorted filename order. Colon-list variables accumulate
/// across fragments; all others take the value of the last fragment that sets
/// them. `LDPATH` is written to `ld.so.conf` rather than `profile.env`, matching
/// `env-update`.
pub fn env_update(eroot: &Path) -> Result<EnvUpdateReport> {
    let env_d = eroot.join("etc/env.d");
    let mut colon: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut scalar: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();

    for path in fragment_files(&env_d)? {
        let body = std::fs::read_to_string(&path).map_err(|e| InstallError::io(&path, e))?;
        for (key, value) in parse_fragment(&body) {
            if COLON_VARS.contains(&key.as_str()) {
                let entry = colon.entry(key).or_default();
                for part in value.split(':').filter(|p| !p.is_empty()) {
                    if !entry.iter().any(|e| e == part) {
                        entry.push(part.to_owned());
                    }
                }
            } else {
                scalar.insert(key, value);
            }
        }
    }

    let mut profile = String::new();
    for (key, parts) in &colon {
        if key == "LDPATH" {
            continue;
        }
        profile.push_str(&format!("export {key}='{}'\n", parts.join(":")));
    }
    for (key, value) in &scalar {
        profile.push_str(&format!("export {key}='{value}'\n"));
    }

    let mut ld = String::new();
    if let Some(parts) = colon.get("LDPATH") {
        for part in parts {
            ld.push_str(part);
            ld.push('\n');
        }
    }

    let profile_env = eroot.join("etc/profile.env");
    let ld_so_conf = eroot.join("etc/ld.so.conf");
    write_atomic(&profile_env, profile.as_bytes())?;
    write_atomic(&ld_so_conf, ld.as_bytes())?;

    Ok(EnvUpdateReport {
        profile_env,
        ld_so_conf,
    })
}

/// Mark `items` read by appending them to the per-repository read-state file at
/// `read_file`, keeping it sorted and unique.
pub fn mark_news_read(read_file: &Path, items: &[String]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut set = read_lines(read_file)?;
    set.extend(items.iter().cloned());
    write_lines(read_file, &set)
}

/// The sorted list of regular files directly under `dir`, empty when absent.
fn fragment_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(InstallError::io(dir, e)),
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    Ok(files)
}

/// Parse `KEY=value` and `KEY="value"` lines from an `env.d` fragment.
fn parse_fragment(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            out.push((key.trim().to_owned(), value.to_owned()));
        }
    }
    out
}

/// Read a newline file into a sorted set, empty when absent.
fn read_lines(path: &Path) -> Result<BTreeSet<String>> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(body
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(InstallError::io(path, e)),
    }
}

/// Write a sorted set as one entry per line, atomically.
fn write_lines(path: &Path, set: &BTreeSet<String>) -> Result<()> {
    let mut body = String::new();
    for line in set {
        body.push_str(line);
        body.push('\n');
    }
    write_atomic(path, body.as_bytes())
}

/// Create parents and write `bytes` to `path` atomically.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| InstallError::io(parent, e))?;
    }
    moraine_common::fs::atomic_write(path, bytes)
        .map_err(|e| InstallError::io(path, std::io::Error::other(e.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_add_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let world = dir.path().join("world");
        WorldUpdate {
            add: vec!["app/a".into(), "app/b".into()],
            remove: vec![],
        }
        .apply(&world, false)
        .unwrap();
        WorldUpdate {
            add: vec![],
            remove: vec!["app/a".into()],
        }
        .apply(&world, false)
        .unwrap();
        let body = std::fs::read_to_string(&world).unwrap();
        assert_eq!(body, "app/b\n");
    }

    #[test]
    fn world_oneshot_skips_add() {
        let dir = tempfile::tempdir().unwrap();
        let world = dir.path().join("world");
        WorldUpdate {
            add: vec!["app/a".into()],
            remove: vec![],
        }
        .apply(&world, true)
        .unwrap();
        assert!(!world.exists() || std::fs::read_to_string(&world).unwrap().is_empty());
    }

    #[test]
    fn env_update_aggregates_paths() {
        let dir = tempfile::tempdir().unwrap();
        let env_d = dir.path().join("etc/env.d");
        std::fs::create_dir_all(&env_d).unwrap();
        std::fs::write(env_d.join("00basic"), "PATH=/bin\nLDPATH=/lib\n").unwrap();
        std::fs::write(env_d.join("10extra"), "PATH=/usr/bin\nLDPATH=/usr/lib\n").unwrap();
        let report = env_update(dir.path()).unwrap();
        let profile = std::fs::read_to_string(&report.profile_env).unwrap();
        assert!(profile.contains("export PATH='/bin:/usr/bin'"));
        assert!(!profile.contains("LDPATH"));
        let ld = std::fs::read_to_string(&report.ld_so_conf).unwrap();
        assert_eq!(ld, "/lib\n/usr/lib\n");
    }

    #[test]
    fn news_marked_read_is_unique_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let read = dir.path().join("news.read");
        mark_news_read(&read, &["2026-b".into(), "2026-a".into()]).unwrap();
        mark_news_read(&read, &["2026-a".into(), "2026-c".into()]).unwrap();
        let body = std::fs::read_to_string(&read).unwrap();
        assert_eq!(body, "2026-a\n2026-b\n2026-c\n");
    }
}
