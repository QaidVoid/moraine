//! The in-memory query API over a loaded [`Store`].
//!
//! Every query reads the loaded snapshot and is lock-free. Atom matching uses
//! `moraine-atom` semantics over `moraine-version` ordering, honoring version,
//! slot, and sub-slot constraints. The API also exposes recorded USE, the
//! recorded slot and sub-slot, recorded `:=` slot-operator bindings, and soname
//! `PROVIDES`/`REQUIRES`.

use moraine_atom::{Atom, PackageRef, SlotOp};
use moraine_common::Symbol;

use crate::record::PackageRecord;
use crate::store::Store;

/// One installed package as `(category, package, version)`.
#[derive(Debug, Clone, Copy)]
pub struct Installed<'a> {
    /// The interned category.
    pub category: Symbol,
    /// The interned package name.
    pub package: Symbol,
    /// The package version string.
    pub version: &'a str,
}

/// A recorded slot-operator binding found in a `*DEPEND` field.
///
/// The bound slot and sub-slot are returned exactly as recorded so the resolver
/// can compare them against currently available sub-slots without any
/// normalization.
#[derive(Debug, Clone, Copy)]
pub struct SlotBinding {
    /// The depended-upon category.
    pub category: Symbol,
    /// The depended-upon package name.
    pub package: Symbol,
    /// The bound slot, if the recorded atom carried one.
    pub slot: Option<Symbol>,
    /// The bound sub-slot, if the recorded atom carried one.
    pub subslot: Option<Symbol>,
}

impl Store {
    /// Enumerate every installed package as `(category, package, version)`.
    pub fn installed(&self) -> impl Iterator<Item = Installed<'_>> {
        self.records().iter().map(|r| Installed {
            category: r.category,
            package: r.package,
            version: r.version.as_str(),
        })
    }

    /// Return the installed records matching `atom`, honoring version, slot, and
    /// sub-slot constraints.
    pub fn match_atom(&self, atom: &Atom) -> Vec<&PackageRecord> {
        self.records()
            .iter()
            .filter(|r| {
                let pkg = PackageRef {
                    category: r.category,
                    package: r.package,
                    version: &r.version,
                    slot: Some(r.slot.slot),
                    subslot: r.slot.subslot,
                    repo: r.repository,
                };
                atom.matches(&pkg)
            })
            .collect()
    }

    /// The recorded USE flags of `record`.
    pub fn recorded_use<'a>(&self, record: &'a PackageRecord) -> &'a [Symbol] {
        &record.use_flags
    }

    /// The recorded slot and sub-slot of `record` as `(slot, sub-slot)`.
    pub fn recorded_slot(&self, record: &PackageRecord) -> (Symbol, Option<Symbol>) {
        (record.slot.slot, record.slot.subslot)
    }

    /// Collect every recorded `:=` slot-operator binding across the `*DEPEND`
    /// fields of `record`, returning the bound slot and sub-slot unnormalized.
    ///
    /// Only atoms carrying the `:=` operator are returned, since those are the
    /// bindings `solver-gentoo` compares against currently available sub-slots to
    /// decide slot-operator rebuilds.
    pub fn slot_operator_bindings(&self, record: &PackageRecord) -> Vec<SlotBinding> {
        let mut out = Vec::new();
        for kind in crate::record::DependKind::ALL {
            let Some(dep) = record.depends.get_meaningful(kind, record.features()) else {
                continue;
            };
            for atom in dep.ast.atoms() {
                if atom.slot_op() == Some(SlotOp::Equal) {
                    out.push(SlotBinding {
                        category: atom.category(),
                        package: atom.package(),
                        slot: atom.slot(),
                        subslot: atom.subslot(),
                    });
                }
            }
        }
        out
    }

    /// The installed records that provide `soname` in any ABI bucket.
    pub fn soname_providers(&self, soname: Symbol) -> Vec<&PackageRecord> {
        self.records()
            .iter()
            .filter(|r| r.provides.provides(soname))
            .collect()
    }

    /// The sonames `record` requires as `(multilib-category bucket, soname)`
    /// pairs, as recorded in its linkage data.
    pub fn required_sonames<'a>(
        &self,
        record: &'a PackageRecord,
    ) -> impl Iterator<Item = (Symbol, Symbol)> + 'a {
        record.requires.entries.iter().map(|e| (e.bucket, e.soname))
    }

    /// The `category/package:slot` atom of `record`, resolving its interned cp
    /// and slot tokens.
    fn slot_atom(&self, record: &PackageRecord) -> String {
        let interner = self.interner();
        let category = interner.resolve(record.category).unwrap_or_default();
        let package = interner.resolve(record.package).unwrap_or_default();
        let slot = interner.resolve(record.slot.slot).unwrap_or_default();
        format!("{category}/{package}:{slot}")
    }

    /// A `category/package:slot` atom for every installed package, backing the
    /// `@installed` set, mirroring `EverythingSet.load`. A slot atom is emitted
    /// even for a single installed slot so the set never widens to an upgrade.
    pub fn installed_slot_atoms(&self) -> Vec<String> {
        self.records().iter().map(|r| self.slot_atom(r)).collect()
    }

    /// The `category/package:slot` atom of each installed record whose recorded
    /// `PROPERTIES` token list contains `token`, backing the `@live-rebuild` set
    /// (`VariableSet` with `variable=PROPERTIES includes=live`).
    pub fn slot_atoms_with_property(&self, token: &str) -> Vec<String> {
        self.records()
            .iter()
            .filter(|r| r.properties.split_whitespace().any(|p| p == token))
            .map(|r| self.slot_atom(r))
            .collect()
    }

    /// The `category/package:slot` atom of each installed record whose `CONTENTS`
    /// lists a path under `prefix` and under none of `excludes`, backing the
    /// `@module-rebuild` set and mirroring `OwnerSet.mapPathsToAtoms`. A trailing
    /// `*` in `prefix` or an `excludes` entry matches by string prefix;
    /// otherwise the entry matches a path equal to it or nested beneath it.
    pub fn slot_atoms_owning_path(&self, prefix: &str, excludes: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for record in self.records() {
            let mut owns_prefix = false;
            let mut owns_exclude = false;
            for entry in record.contents.iter() {
                if path_matches(&entry.path, prefix) {
                    owns_prefix = true;
                }
                if excludes.iter().any(|ex| path_matches(&entry.path, ex)) {
                    owns_exclude = true;
                }
                if owns_prefix && owns_exclude {
                    break;
                }
            }
            if owns_prefix && !owns_exclude {
                out.push(self.slot_atom(record));
            }
        }
        out
    }
}

/// Whether `path` is matched by `pattern`: a trailing `*` matches by string
/// prefix, otherwise the path must equal `pattern` or be nested directly
/// beneath it.
fn path_matches(path: &str, pattern: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => path.starts_with(prefix),
        None => {
            path == pattern
                || (path.starts_with(pattern) && path.as_bytes().get(pattern.len()) == Some(&b'/'))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moraine_common::Interner;

    use super::*;
    use crate::contents::{Contents, Entry, EntryKind};
    use crate::record::{DependSet, Slot, Toolchain};
    use crate::soname::{Provides, Requires};
    use crate::store::StorePaths;

    fn record(
        interner: &Interner,
        cp: &str,
        slot: &str,
        properties: &str,
        paths: &[&str],
    ) -> PackageRecord {
        let (cat, pkg) = cp.split_once('/').unwrap();
        let contents = Contents::from_entries(paths.iter().map(|p| Entry {
            path: (*p).to_string(),
            kind: EntryKind::Obj {
                md5: "0".repeat(32),
                mtime: 0,
            },
        }));
        PackageRecord {
            category: interner.intern(cat),
            package: interner.intern(pkg),
            version: moraine_version::Version::parse("1").unwrap(),
            eapi: "8".to_string(),
            slot: Slot {
                slot: interner.intern(slot),
                subslot: None,
            },
            use_flags: Vec::new(),
            iuse: Vec::new(),
            depends: DependSet::default(),
            keywords: Vec::new(),
            license: String::new(),
            description: String::new(),
            homepage: String::new(),
            properties: properties.to_string(),
            restrict: String::new(),
            repository: None,
            defined_phases: Vec::new(),
            build_time: None,
            build_id: None,
            counter: 0,
            chost: String::new(),
            provides: Provides {
                entries: Vec::new(),
            },
            requires: Requires {
                entries: Vec::new(),
            },
            contents,
            environment: None,
            inherited: Vec::new(),
            features: Vec::new(),
            size: None,
            needed: Vec::new(),
            toolchain: Toolchain::default(),
            dbdir_mtime: 0,
        }
    }

    fn store_with(interner: Arc<Interner>, records: Vec<PackageRecord>) -> Store {
        Store::from_records(StorePaths::in_dir(std::env::temp_dir()), interner, records)
    }

    #[test]
    fn installed_slot_atoms_enumerates_every_package() {
        let interner = Arc::new(Interner::new());
        let records = vec![
            record(&interner, "sys-apps/portage", "0", "", &[]),
            record(&interner, "dev-lang/rust", "stable", "", &[]),
        ];
        let store = store_with(interner.clone(), records);
        let mut atoms = store.installed_slot_atoms();
        atoms.sort();
        assert_eq!(
            atoms,
            vec![
                "dev-lang/rust:stable".to_string(),
                "sys-apps/portage:0".to_string(),
            ]
        );
    }

    #[test]
    fn slot_atoms_with_property_filters_on_live() {
        let interner = Arc::new(Interner::new());
        let records = vec![
            record(&interner, "dev-vcs/git", "0", "live", &[]),
            record(&interner, "app-misc/stable", "0", "", &[]),
        ];
        let store = store_with(interner.clone(), records);
        assert_eq!(
            store.slot_atoms_with_property("live"),
            vec!["dev-vcs/git:0".to_string()]
        );
    }

    #[test]
    fn slot_atoms_owning_path_honors_prefix_and_exclude() {
        let interner = Arc::new(Interner::new());
        let records = vec![
            // Owns a file under /lib/modules and nothing under the exclusion.
            record(
                &interner,
                "sys-kernel/module",
                "0",
                "",
                &["/lib/modules/6.0/foo.ko"],
            ),
            // Owns a file under /lib/modules but also under /usr/src/linux*, so
            // the exclusion removes it from the result.
            record(
                &interner,
                "sys-kernel/sources",
                "0",
                "",
                &[
                    "/lib/modules/6.0/build/Makefile",
                    "/usr/src/linux-6.0/Makefile",
                ],
            ),
        ];
        let store = store_with(interner.clone(), records);
        let atoms = store.slot_atoms_owning_path("/lib/modules", &["/usr/src/linux*"]);
        assert_eq!(atoms, vec!["sys-kernel/module:0".to_string()]);
    }
}
