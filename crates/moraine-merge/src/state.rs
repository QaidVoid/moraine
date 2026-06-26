//! Installed-state recording and post-merge actions.
//!
//! The installed record carries the recorded USE, the resolved slot and sub-slot,
//! the dependency variables (with each `:=` binding rewritten to `:slot/subslot=`
//! against the provider it linked against), the saved build environment, the
//! soname `PROVIDES`/`REQUIRES`, and a counter drawn from a global monotonic
//! counter. The record becomes visible only after CONTENTS and all files are
//! durable, which is the commit point. After completion the engine triggers elog
//! dispatch, news-item marking, and config-update notices as recorded outcomes.

use std::collections::BTreeMap;

use moraine_atom::{Atom, SlotOp};
use moraine_common::Interner;
use moraine_eapi::PERMISSIVE;
use moraine_vdb::contents::{Contents, Entry};
use moraine_vdb::record::{Depend, DependKind, DependSet, EnvironmentRef, PackageRecord, Slot};
use moraine_vdb::soname::{Provides, Requires, SonameEntry};
use moraine_version::Version;

use crate::error::MergeError;

/// Rewrite every `:=` slot-operator atom in a USE-reduced `*DEPEND` string to its
/// bound `:slot/subslot=` form, using the providers this package linked against.
///
/// `bindings` maps a dependency `cp` to the `(slot, subslot)` of its selected
/// provider, as `(dependency_cp, slot, subslot)`. This mirrors Portage's
/// `evaluate_slot_operator_equal_deps`, baking the linked slot into the recorded
/// dependency so a later sub-slot change is detectable. Structural tokens (`||`,
/// `(`, `)`) and atoms with no matching `:=` binding are left unchanged.
pub fn rewrite_slot_operators(
    dep_string: &str,
    bindings: &[(String, String, Option<String>)],
    interner: &Interner,
) -> String {
    dep_string
        .split_whitespace()
        .map(|token| rewrite_atom_token(token, bindings, interner))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Rewrite a single `*DEPEND` token if it is a `:=` atom with a known binding.
fn rewrite_atom_token(
    token: &str,
    bindings: &[(String, String, Option<String>)],
    interner: &Interner,
) -> String {
    let Ok(atom) = Atom::parse(token, PERMISSIVE, interner) else {
        return token.to_owned();
    };
    // Only an `=` binding whose sub-slot is not yet baked is rewritten.
    if atom.slot_op() != Some(SlotOp::Equal) || atom.subslot().is_some() {
        return token.to_owned();
    }
    let cp = {
        let category = interner.resolve(atom.category()).unwrap_or_default();
        let package = interner.resolve(atom.package()).unwrap_or_default();
        format!("{category}/{package}")
    };
    let Some((_, slot, subslot)) = bindings.iter().find(|(dep_cp, _, _)| dep_cp == &cp) else {
        return token.to_owned();
    };
    // A bare `:=` takes the binding's slot; an explicit `:slot=` keeps its slot
    // and only gains the bound sub-slot.
    let slot_sym = atom.slot().unwrap_or_else(|| interner.intern(slot));
    let subslot_sym = subslot.as_ref().map(|s| interner.intern(s));
    atom.with_bound_slot(slot_sym, subslot_sym).render(interner)
}

/// A soname tagged with its ABI bucket, as plain strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Soname {
    /// The ABI bucket token (for example `x86_64`).
    pub bucket: String,
    /// The soname (for example `libfoo.so.1`).
    pub soname: String,
}

/// The installed state to record for a merged package, as plain data.
///
/// This mirrors [`PackageRecord`] but holds strings rather than interned
/// [`Symbol`](moraine_common::Symbol)s so the caller does not need a shared
/// interner; the engine interns into the store at commit. Each `:=` binding in
/// the `*DEPEND` strings is rewritten to `:slot/subslot=` against the linked
/// provider before recording (see [`rewrite_slot_operators`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageState {
    /// The `category/package-version`, used as the operation label.
    pub cpv: String,
    /// The category.
    pub category: String,
    /// The package name.
    pub package: String,
    /// The version string.
    pub version: String,
    /// The recorded EAPI.
    pub eapi: String,
    /// The resolved slot.
    pub slot: String,
    /// The resolved sub-slot, if any.
    pub subslot: Option<String>,
    /// The recorded USE flags.
    pub use_flags: Vec<String>,
    /// The recorded IUSE tokens.
    pub iuse: Vec<String>,
    /// The `*DEPEND` strings keyed by family name (`DEPEND`, `RDEPEND`, ...),
    /// with each `:=` binding rewritten to its linked `:slot/subslot=` form.
    pub depends: BTreeMap<String, String>,
    /// The recorded `KEYWORDS`.
    pub keywords: Vec<String>,
    /// The recorded `LICENSE`.
    pub license: String,
    /// The recorded `PROPERTIES`.
    pub properties: String,
    /// The recorded `RESTRICT`.
    pub restrict: String,
    /// The origin repository, if recorded.
    pub repository: Option<String>,
    /// The recorded `DEFINED_PHASES`.
    pub defined_phases: Vec<String>,
    /// The recorded `BUILD_TIME`.
    pub build_time: Option<u64>,
    /// The recorded `CHOST`.
    pub chost: String,
    /// The sonames this package provides.
    pub provides: Vec<Soname>,
    /// The sonames this package requires.
    pub requires: Vec<Soname>,
    /// The saved build environment blob, if one was captured.
    pub environment: Option<Vec<u8>>,
    /// The recorded `INHERITED` eclass names.
    pub inherited: Vec<String>,
    /// The `FEATURES` active when the package was built.
    pub features: Vec<String>,
    /// The installed `SIZE` in bytes, if known.
    pub size: Option<u64>,
    /// The `BUILD_ID` for binpkg-multi-instance installs, if any.
    pub build_id: Option<u64>,
    /// The verbatim `NEEDED.ELF.2` lines from the post-build scan.
    pub needed: Vec<String>,
}

impl PackageState {
    /// Build a [`PackageRecord`] against `interner`, attaching `contents` and
    /// stamping `counter`.
    ///
    /// `*DEPEND` strings are parsed into ASTs; an unparsable string is recorded
    /// with an empty AST rather than failing the merge, since the verbatim string
    /// remains the source of truth for round-trip fidelity.
    pub(crate) fn into_record(
        self,
        interner: &Interner,
        contents: Contents,
        counter: u64,
    ) -> Result<PackageRecord, MergeError> {
        let version = Version::parse(&self.version).map_err(|_| MergeError::Version {
            version: self.version.clone(),
            package: format!("{}/{}", self.category, self.package),
        })?;

        let features = moraine_eapi::features_for(&self.eapi);
        let mut depends = DependSet::default();
        for kind in DependKind::ALL {
            if let Some(raw) = self.depends.get(kind.name()) {
                let ast = moraine_atom::DepSpec::parse(raw, features, interner)
                    .unwrap_or_else(|_| moraine_atom::DepSpec::AllOf(Vec::new()));
                *depends.slot_mut(kind) = Some(Depend {
                    raw: raw.clone(),
                    ast,
                });
            }
        }

        let provides = Provides {
            entries: self
                .provides
                .iter()
                .map(|s| SonameEntry {
                    bucket: interner.intern(&s.bucket),
                    soname: interner.intern(&s.soname),
                })
                .collect(),
        };
        let requires = Requires {
            entries: self
                .requires
                .iter()
                .map(|s| SonameEntry {
                    bucket: interner.intern(&s.bucket),
                    soname: interner.intern(&s.soname),
                })
                .collect(),
        };

        let environment = self.environment.map(|blob| EnvironmentRef {
            digest: moraine_common::hash::blake3(&blob),
            blob,
        });

        Ok(PackageRecord {
            category: interner.intern(&self.category),
            package: interner.intern(&self.package),
            version,
            eapi: self.eapi,
            slot: Slot {
                slot: interner.intern(&self.slot),
                subslot: self.subslot.as_deref().map(|s| interner.intern(s)),
            },
            use_flags: self.use_flags.iter().map(|u| interner.intern(u)).collect(),
            iuse: self.iuse,
            depends,
            keywords: self.keywords,
            license: self.license,
            properties: self.properties,
            restrict: self.restrict,
            repository: self.repository.as_deref().map(|r| interner.intern(r)),
            defined_phases: self.defined_phases,
            build_time: self.build_time,
            build_id: self.build_id,
            counter,
            chost: self.chost,
            provides,
            requires,
            contents,
            environment,
            inherited: self.inherited,
            features: self.features,
            size: self.size,
            needed: self.needed,
        })
    }
}

/// The recorded outcome of post-merge actions for an operation.
///
/// These are modeled as recorded outcomes rather than live dispatch to external
/// services. The CLI renders them; the merge engine does not run phase functions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostMergeReport {
    /// elog messages dispatched for the operation.
    pub elog: Vec<String>,
    /// News items marked relevant by the operation.
    pub news_marked: Vec<String>,
    /// Pending config updates: the `._cfgNNNN_` variant install paths created.
    pub config_updates: Vec<String>,
}

impl PostMergeReport {
    /// Whether any post-merge action was recorded.
    pub fn is_empty(&self) -> bool {
        self.elog.is_empty() && self.news_marked.is_empty() && self.config_updates.is_empty()
    }
}

/// Helper for building the explicit CONTENTS entries of a merged package.
pub(crate) fn contents_from(entries: Vec<Entry>) -> Contents {
    Contents::from_entries(entries)
}

#[cfg(test)]
mod tests {
    use super::rewrite_slot_operators;
    use moraine_common::Interner;

    fn binding(cp: &str, slot: &str, subslot: Option<&str>) -> (String, String, Option<String>) {
        (
            cp.to_owned(),
            slot.to_owned(),
            subslot.map(|s| s.to_owned()),
        )
    }

    #[test]
    fn rewrites_equal_binding_to_bound_slot() {
        let i = Interner::new();
        let b = vec![binding("dev-libs/foo", "2", Some("2.1"))];
        assert_eq!(
            rewrite_slot_operators("dev-libs/foo:=", &b, &i),
            "dev-libs/foo:2/2.1="
        );
    }

    #[test]
    fn rewrites_equal_binding_without_subslot() {
        let i = Interner::new();
        let b = vec![binding("dev-libs/foo", "2", None)];
        assert_eq!(
            rewrite_slot_operators("dev-libs/foo:=", &b, &i),
            "dev-libs/foo:2="
        );
    }

    #[test]
    fn bakes_subslot_onto_explicit_slot() {
        let i = Interner::new();
        let b = vec![binding("dev-libs/foo", "2", Some("2.1"))];
        assert_eq!(
            rewrite_slot_operators("dev-libs/foo:2=", &b, &i),
            "dev-libs/foo:2/2.1="
        );
    }

    #[test]
    fn leaves_unbound_and_structural_tokens() {
        let i = Interner::new();
        let b = vec![binding("dev-libs/foo", "2", Some("2.1"))];
        // foo is rewritten; bar has no binding; ||, (, ) are preserved.
        assert_eq!(
            rewrite_slot_operators("|| ( dev-libs/foo:= dev-libs/bar )", &b, &i),
            "|| ( dev-libs/foo:2/2.1= dev-libs/bar )"
        );
    }

    #[test]
    fn leaves_non_slot_operator_atoms() {
        let i = Interner::new();
        let b = vec![binding("dev-libs/foo", "2", Some("2.1"))];
        // A plain slot dep (no `=`) and a star op are untouched.
        assert_eq!(
            rewrite_slot_operators("dev-libs/foo:2 dev-libs/foo:*", &b, &i),
            "dev-libs/foo:2 dev-libs/foo:*"
        );
    }
}
