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

    /// The sonames `record` requires, as recorded in its linkage data.
    pub fn required_sonames<'a>(
        &self,
        record: &'a PackageRecord,
    ) -> impl Iterator<Item = Symbol> + 'a {
        record.requires.sonames()
    }
}
