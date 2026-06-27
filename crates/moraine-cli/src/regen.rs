//! The ebuild metadata generator backing the post-sync regeneration fallback.
//!
//! `moraine-sync` exposes a [`MetadataGenerator`] trait but cannot depend on the
//! build engine (the dependency would form a cycle through `moraine-repo`). This
//! module is the CLI-side implementation: it sources an ebuild with a working
//! `inherit` through [`moraine_build::depend::generate_metadata`] and converts
//! the emitted `MORAINE_META` map into a [`StoredEntry`], so a missing or stale
//! cache entry is regenerated during a sync instead of excluding the package.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use moraine_build::SystemRunner;
use moraine_build::bashlib::PhaseLibrary;
use moraine_build::depend::generate_metadata;
use moraine_repo::RepoSet;
use moraine_repo::store::{StoredEntry, split_slot};
use moraine_sync::{Generated, MetadataGenerator};

use crate::resolve_install::{eclass_locations, package_ident, split_cpv, split_pf};

/// Regenerates store entries by sourcing ebuilds with the vendored phase library.
pub struct EbuildMetadataGenerator<'a> {
    repo_set: &'a RepoSet,
    runner: SystemRunner,
    library: PhaseLibrary,
    /// Holds the materialized library and the writable `T` for the depend phase.
    _scratch: tempfile::TempDir,
    scratch_path: PathBuf,
}

impl<'a> EbuildMetadataGenerator<'a> {
    /// Materialize the phase library into a scratch directory and build a
    /// generator over `repo_set`. Returns `None` when the library cannot be
    /// materialized (regeneration is then simply unavailable).
    pub fn new(repo_set: &'a RepoSet) -> Option<Self> {
        let scratch = tempfile::tempdir().ok()?;
        let scratch_path = scratch.path().to_path_buf();
        let library = PhaseLibrary::materialize(scratch_path.join("bashlib")).ok()?;
        Some(Self {
            repo_set,
            runner: SystemRunner,
            library,
            _scratch: scratch,
            scratch_path,
        })
    }

    /// Resolve the ebuild path for `cpv` in `repo`, when it exists on disk.
    fn ebuild_path(&self, repo: &str, category: &str, pn: &str, pf: &str) -> Option<PathBuf> {
        let cfg = self.repo_set.get(repo)?;
        let path = cfg
            .location
            .join(category)
            .join(pn)
            .join(format!("{pf}.ebuild"));
        path.is_file().then_some(path)
    }
}

impl MetadataGenerator for EbuildMetadataGenerator<'_> {
    fn generate(&self, repo: &str, cpv: &str, previous: Option<&StoredEntry>) -> Option<Generated> {
        let (category, pf) = split_cpv(cpv);
        if category.is_empty() {
            return None;
        }
        let (pn, pvr) = split_pf(&pf);
        if pvr.is_empty() {
            return None;
        }
        let ebuild_path = self.ebuild_path(repo, &category, &pn, &pf)?;
        let locations = eclass_locations(self.repo_set, repo);

        // Reuse the prior entry without sourcing when its provenance stamp still
        // matches the current ebuild and its inherited eclasses.
        if let Some(prev) = previous {
            let stamp = provenance_stamp(self.repo_set, repo, &ebuild_path, &prev.inherited);
            if !stamp.is_empty() && stamp == prev.md5 {
                return Some(Generated {
                    entry: prev.clone(),
                    regenerated: false,
                });
            }
        }

        let eapi = read_eapi(&ebuild_path).unwrap_or_else(|| "0".to_owned());
        let ident = package_ident(&category, &pf, &pn, &pvr, &eapi, repo);
        let base_env = base_env(&ident, &locations, &self.scratch_path);

        let meta = generate_metadata(&self.runner, &self.library, &ebuild_path, &base_env).ok()?;
        if meta.is_empty() || !meta.contains_key("EAPI") {
            return None;
        }
        let mut entry = to_stored_entry(&meta, &category, &pn, &pvr, repo);
        // Stamp provenance from the eclasses actually inherited so a later
        // unchanged refresh reuses this entry rather than regenerating it.
        entry.md5 = provenance_stamp(self.repo_set, repo, &ebuild_path, &entry.inherited);
        entry.mtime = ebuild_mtime(&ebuild_path);
        Some(Generated {
            entry,
            regenerated: true,
        })
    }
}

/// Build the base environment the depend-phase generator needs: the package
/// identity, a writable `T`, and the shell-quoted eclass search path.
fn base_env(
    ident: &moraine_build::PackageIdent,
    locations: &[String],
    scratch: &Path,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("EAPI".to_string(), ident.eapi.clone());
    env.insert("CATEGORY".to_string(), ident.category.clone());
    env.insert("PF".to_string(), ident.pf.clone());
    env.insert("P".to_string(), ident.p.clone());
    env.insert("PN".to_string(), ident.pn.clone());
    env.insert("PV".to_string(), ident.pv.clone());
    env.insert("PVR".to_string(), ident.pvr.clone());
    env.insert("PR".to_string(), ident.pr.clone());
    env.insert("PORTAGE_REPO_NAME".to_string(), ident.repository.clone());
    env.insert("T".to_string(), scratch.to_string_lossy().into_owned());
    env.insert(
        "PORTAGE_ECLASS_LOCATIONS".to_string(),
        shell_quote_join(locations),
    );
    env
}

/// Convert the `MORAINE_META` map into a [`StoredEntry`].
fn to_stored_entry(
    meta: &BTreeMap<String, String>,
    category: &str,
    package: &str,
    version: &str,
    repo: &str,
) -> StoredEntry {
    let text = |key: &str| meta.get(key).cloned().unwrap_or_default();
    let tokens = |key: &str| {
        meta.get(key)
            .map(|v| v.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let (slot, subslot) = split_slot(&text("SLOT"));
    StoredEntry {
        category: category.to_owned(),
        package: package.to_owned(),
        version: version.to_owned(),
        repository: repo.to_owned(),
        eapi: text("EAPI"),
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
        mtime: String::new(),
        md5: String::new(),
    }
}

/// The first `EAPI=` assignment in the ebuild, with surrounding quotes stripped.
fn read_eapi(ebuild: &Path) -> Option<String> {
    let text = std::fs::read_to_string(ebuild).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("EAPI=") {
            let value = rest.trim().trim_matches(['"', '\'']);
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// A provenance stamp over the ebuild bytes and the md5 of each inherited
/// eclass, so a change to the ebuild or any inherited eclass invalidates it.
fn provenance_stamp(repo_set: &RepoSet, repo: &str, ebuild: &Path, inherited: &[String]) -> String {
    let Ok(mut bytes) = std::fs::read(ebuild) else {
        return String::new();
    };
    let eclass_dirs = repo_set.eclass_search_path(repo);
    let mut names: Vec<&String> = inherited.iter().collect();
    names.sort();
    for name in names {
        bytes.push(b'\n');
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(b':');
        if let Some(md5) = eclass_md5(&eclass_dirs, name) {
            bytes.extend_from_slice(md5.as_bytes());
        }
    }
    moraine_common::hash::md5(&bytes)
}

/// The md5 of the first `<dir>/<name>.eclass` found across the eclass dirs.
fn eclass_md5(eclass_dirs: &[PathBuf], name: &str) -> Option<String> {
    for dir in eclass_dirs {
        let path = dir.join(format!("{name}.eclass"));
        if let Ok(bytes) = std::fs::read(&path) {
            return Some(moraine_common::hash::md5(&bytes));
        }
    }
    None
}

/// The ebuild's modification time in seconds since the epoch, as a string.
fn ebuild_mtime(ebuild: &Path) -> String {
    std::fs::metadata(ebuild)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

/// Join paths into the single shell-quoted string `inherit` evaluates into the
/// `PORTAGE_ECLASS_LOCATIONS` array.
fn shell_quote_join(paths: &[String]) -> String {
    paths
        .iter()
        .map(|p| format!("'{}'", p.replace('\'', r#"'\''"#)))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_metadata_map_to_entry() {
        let meta = BTreeMap::from([
            ("EAPI".to_string(), "8".to_string()),
            ("SLOT".to_string(), "0/3".to_string()),
            ("DEPEND".to_string(), "dev-libs/libfoo".to_string()),
            ("RDEPEND".to_string(), "dev-libs/libfoo".to_string()),
            ("IUSE".to_string(), "ssl threads".to_string()),
            ("KEYWORDS".to_string(), "amd64 ~arm64".to_string()),
            ("DEFINED_PHASES".to_string(), "compile install".to_string()),
            ("INHERITED".to_string(), "foo bar".to_string()),
            ("INHERIT".to_string(), "foo".to_string()),
            ("REQUIRED_USE".to_string(), "^^ ( ssl )".to_string()),
        ]);
        let e = to_stored_entry(&meta, "dev-libs", "pkg", "1.2", "gentoo");
        assert_eq!(e.category, "dev-libs");
        assert_eq!(e.package, "pkg");
        assert_eq!(e.version, "1.2");
        assert_eq!(e.eapi, "8");
        assert_eq!(e.slot, "0");
        assert_eq!(e.subslot.as_deref(), Some("3"));
        assert_eq!(e.depend, "dev-libs/libfoo");
        assert_eq!(e.iuse, vec!["ssl", "threads"]);
        assert_eq!(e.keywords, vec!["amd64", "~arm64"]);
        assert_eq!(e.defined_phases, vec!["compile", "install"]);
        assert_eq!(e.inherited, vec!["foo", "bar"]);
        assert_eq!(e.inherit, vec!["foo"]);
        assert_eq!(e.required_use, "^^ ( ssl )");
    }

    #[test]
    fn reads_eapi_assignment() {
        let tmp = tempfile::tempdir().unwrap();
        let eb = tmp.path().join("p-1.ebuild");
        std::fs::write(&eb, "# a comment\nEAPI=\"8\"\nDESCRIPTION=x\n").unwrap();
        assert_eq!(read_eapi(&eb).as_deref(), Some("8"));
        std::fs::write(&eb, "EAPI=7\n").unwrap();
        assert_eq!(read_eapi(&eb).as_deref(), Some("7"));
    }

    #[test]
    fn shell_quote_join_escapes_quotes() {
        assert_eq!(
            shell_quote_join(&["/a".to_string(), "/it's".to_string()]),
            r#"'/a' '/it'\''s'"#
        );
    }
}
