//! Package-move support for binary packages and the binhost index.
//!
//! After a repository `move`/`slotmove`, a binary package's embedded identity
//! (`CATEGORY`/`PF`) and dependency metadata, and the matching `Packages` index
//! stanza, must be rewritten to the new name or slot so the binhost view stays
//! consistent. The dependency rewrite reuses [`moraine_atom`]'s token-preserving
//! rewrite so dependency strings stay faithful.

use std::path::{Path, PathBuf};

use moraine_atom::{rewrite_dep_cp, rewrite_dep_slot};
use moraine_common::Interner;

use crate::index::PackagesIndex;
use crate::metadata::MetadataMap;

/// The `*DEPEND` metadata keys rewritten for a package move.
const DEPEND_KEYS: &[&str] = &["DEPEND", "RDEPEND", "BDEPEND", "PDEPEND", "IDEPEND"];

/// Split a cpv (`category/package-version`) into its `category/package` and the
/// version, using a version-suffix split.
fn split_cpv(cpv: &str) -> Option<(String, String)> {
    let (cat, rest) = cpv.split_once('/')?;
    let (pkg, version) = split_pkg_version(rest)?;
    Some((format!("{cat}/{pkg}"), version))
}

/// Split `pkg-version` at the version boundary.
fn split_pkg_version(pv: &str) -> Option<(String, String)> {
    let mut idx = 0;
    while let Some(rel) = pv[idx..].find('-') {
        let at = idx + rel;
        let tail = &pv[at + 1..];
        if tail.starts_with(|c: char| c.is_ascii_digit())
            && moraine_version::Version::parse(tail).is_ok()
        {
            return Some((pv[..at].to_string(), tail.to_string()));
        }
        idx = at + 1;
    }
    None
}

/// Rewrite the dependency metadata keys of `meta` for the given cp `renames` and
/// `slotmoves`, leaving `self_cp` blockers untouched to avoid self-blockers.
pub fn rewrite_dep_keys(
    meta: &mut MetadataMap,
    renames: &[(String, String)],
    slotmoves: &[(String, String, String)],
    self_cp: &str,
    interner: &Interner,
) {
    let features = moraine_eapi::features_for(&meta.get_str("EAPI").unwrap_or_default());
    for key in DEPEND_KEYS {
        let Some(raw) = meta.get_str(key) else {
            continue;
        };
        let mut new = raw.clone();
        for (old, dst) in renames {
            new = rewrite_dep_cp(&new, old, dst, self_cp, features, interner);
        }
        for (cp, old_slot, new_slot) in slotmoves {
            new = rewrite_dep_slot(&new, cp, old_slot, new_slot, features, interner);
        }
        if new != raw {
            meta.set_str(*key, &new);
        }
    }
}

/// Set the package's own identity in `CATEGORY`/`PF` to the new name.
fn rename_identity(meta: &mut MetadataMap, new_cat: &str, new_pkg: &str, version: &str) {
    meta.set_str("CATEGORY", new_cat);
    meta.set_str("PF", format!("{new_pkg}-{version}"));
}

impl PackagesIndex {
    /// Apply a package `move` to the binhost index: rename every stanza of
    /// `old_cp` to `new_cp` (skipping one whose destination cpv already exists),
    /// and rewrite the dependency atoms of every stanza referencing `old_cp`.
    /// Returns the number of stanzas renamed.
    pub fn move_ent(&mut self, old_cp: &str, new_cp: &str, interner: &Interner) -> usize {
        let Some((new_cat, new_pkg)) = new_cp.split_once('/') else {
            return 0;
        };
        let existing: std::collections::HashSet<String> =
            self.packages.iter().map(|p| p.cpv.clone()).collect();
        let renames = [(old_cp.to_string(), new_cp.to_string())];
        let mut renamed = 0;
        for entry in &mut self.packages {
            let self_cp = split_cpv(&entry.cpv).map(|(cp, _)| cp).unwrap_or_default();
            // Rewrite dependency references in every stanza.
            rewrite_dep_keys(&mut entry.metadata, &renames, &[], &self_cp, interner);
            // Rename the stanza itself when it is the moved package.
            if self_cp == old_cp
                && let Some((_, version)) = split_cpv(&entry.cpv)
            {
                let new_cpv = format!("{new_cp}-{version}");
                if existing.contains(&new_cpv) {
                    continue;
                }
                rename_identity(&mut entry.metadata, new_cat, new_pkg, &version);
                entry.cpv = new_cpv;
                renamed += 1;
            }
        }
        renamed
    }

    /// Apply a `slotmove` to the binhost index: rewrite the `SLOT` of every
    /// stanza of `atom_cp` at `old_slot` to `new_slot`, and rewrite the slot
    /// constraint of matching dependency atoms in every stanza. Returns the
    /// number of stanzas re-slotted.
    pub fn move_slot_ent(
        &mut self,
        atom_cp: &str,
        old_slot: &str,
        new_slot: &str,
        interner: &Interner,
    ) -> usize {
        let slotmoves = [(
            atom_cp.to_string(),
            old_slot.to_string(),
            new_slot.to_string(),
        )];
        let mut moved = 0;
        for entry in &mut self.packages {
            let self_cp = split_cpv(&entry.cpv).map(|(cp, _)| cp).unwrap_or_default();
            rewrite_dep_keys(&mut entry.metadata, &[], &slotmoves, &self_cp, interner);
            let recorded = entry.metadata.get_str("SLOT").unwrap_or_default();
            // Compare only the slot portion; the sub-slot is preserved.
            let recorded_slot = recorded.split('/').next().unwrap_or("");
            if self_cp == atom_cp && recorded_slot == old_slot {
                let new_full = match recorded.split_once('/') {
                    Some((_, sub)) => format!("{new_slot}/{sub}"),
                    None => new_slot.to_string(),
                };
                entry.metadata.set_str("SLOT", new_full);
                moved += 1;
            }
        }
        moved
    }
}

/// Rename a local binary-package artifact for a cpv change: rename
/// `<pkgdir>/<old-cpv>.<ext>` to `<pkgdir>/<new-cpv>.<ext>` for every supported
/// extension, refusing to clobber an existing destination and skipping a package
/// with a detached signature sidecar. Returns the renamed paths.
pub fn rename_local_artifact(
    pkgdir: &Path,
    old_cpv: &str,
    new_cpv: &str,
) -> std::io::Result<Vec<PathBuf>> {
    let mut renamed = Vec::new();
    for ext in ["gpkg.tar", "tbz2", "xpak"] {
        let old = pkgdir.join(format!("{old_cpv}.{ext}"));
        if !old.exists() {
            continue;
        }
        // Skip a signed package: a detached signature would no longer match.
        let sig = pkgdir.join(format!("{old_cpv}.{ext}.sig"));
        if sig.exists() || pkgdir.join(format!("{old_cpv}.{ext}.asc")).exists() {
            continue;
        }
        let new = pkgdir.join(format!("{new_cpv}.{ext}"));
        if new.exists() {
            continue;
        }
        if let Some(parent) = new.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&old, &new)?;
        renamed.push(new);
    }
    Ok(renamed)
}

/// Build a stanza for tests and callers that need one inline.
#[cfg(test)]
fn entry(cpv: &str, kvs: &[(&str, &str)]) -> crate::index::PackageEntry {
    let mut metadata = MetadataMap::new();
    for (k, v) in kvs {
        metadata.set_str(*k, *v);
    }
    crate::index::PackageEntry {
        cpv: cpv.to_string(),
        metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_move_renames_stanza_and_rewrites_deps() {
        let i = Interner::new();
        let mut index = PackagesIndex::default();
        index.packages.push(entry(
            "dev-util/foo-1.2",
            &[
                ("CATEGORY", "dev-util"),
                ("PF", "foo-1.2"),
                ("SLOT", "0"),
                ("EAPI", "8"),
            ],
        ));
        index.packages.push(entry(
            "app-misc/bar-1",
            &[
                ("CATEGORY", "app-misc"),
                ("PF", "bar-1"),
                ("EAPI", "8"),
                ("RDEPEND", ">=dev-util/foo-1:0"),
            ],
        ));

        assert_eq!(index.move_ent("dev-util/foo", "dev-libs/foo", &i), 1);
        let foo = &index.packages[0];
        assert_eq!(foo.cpv, "dev-libs/foo-1.2");
        assert_eq!(
            foo.metadata.get_str("CATEGORY").as_deref(),
            Some("dev-libs")
        );
        let bar = &index.packages[1];
        assert_eq!(
            bar.metadata.get_str("RDEPEND").as_deref(),
            Some(">=dev-libs/foo-1:0")
        );
    }

    #[test]
    fn index_move_skips_existing_destination() {
        let i = Interner::new();
        let mut index = PackagesIndex::default();
        index
            .packages
            .push(entry("dev-util/foo-1", &[("EAPI", "8")]));
        index
            .packages
            .push(entry("dev-libs/foo-1", &[("EAPI", "8")]));
        assert_eq!(index.move_ent("dev-util/foo", "dev-libs/foo", &i), 0);
        assert_eq!(index.packages[0].cpv, "dev-util/foo-1");
    }

    #[test]
    fn index_slot_move_rewrites_slot() {
        let i = Interner::new();
        let mut index = PackagesIndex::default();
        index.packages.push(entry(
            "dev-libs/bar-1",
            &[
                ("SLOT", "0"),
                ("EAPI", "8"),
                ("CATEGORY", "dev-libs"),
                ("PF", "bar-1"),
            ],
        ));
        assert_eq!(index.move_slot_ent("dev-libs/bar", "0", "2", &i), 1);
        assert_eq!(
            index.packages[0].metadata.get_str("SLOT").as_deref(),
            Some("2")
        );
    }

    #[test]
    fn rename_artifact_skips_signed_and_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("dev-util/foo-1.gpkg.tar"), b"x").ok();
        std::fs::create_dir_all(p.join("dev-util")).unwrap();
        std::fs::write(p.join("dev-util/foo-1.gpkg.tar"), b"x").unwrap();
        // Signed: a sidecar signature blocks the rename.
        std::fs::write(p.join("dev-util/foo-1.gpkg.tar.sig"), b"s").unwrap();
        let renamed = rename_local_artifact(p, "dev-util/foo-1", "dev-libs/foo-1").unwrap();
        assert!(renamed.is_empty(), "signed package is not renamed");
        std::fs::remove_file(p.join("dev-util/foo-1.gpkg.tar.sig")).unwrap();

        std::fs::create_dir_all(p.join("dev-libs")).unwrap();
        let renamed = rename_local_artifact(p, "dev-util/foo-1", "dev-libs/foo-1").unwrap();
        assert_eq!(renamed.len(), 1);
        assert!(p.join("dev-libs/foo-1.gpkg.tar").exists());
    }
}
