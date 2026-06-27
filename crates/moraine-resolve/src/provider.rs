//! The Gentoo dependency-provider for the generic solver.
//!
//! The solver's `Package` is an interned `category/package` string and its
//! `Version` is a [`moraine_version::Version`]. SLOT is treated as a candidate
//! filter and a post-resolution check; simultaneous installation of multiple
//! slots of the same `cp` is a known limitation deferred to a future revision,
//! so a slot collision (two required versions of one `cp`) surfaces naturally as
//! a solver conflict.
//!
//! `candidates` enumerates versions from the source, filters by the accumulated
//! range, and ranks them in Portage `dep_zapdeps` order (installed-slot match
//! first, then upgrades). The installed version of a slot is always offered as a
//! candidate even when it is masked, so an already-satisfied dependency is never
//! forced to change and a masked installed higher version is not downgraded to a
//! lower visible one (Portage's `_iter_match_pkgs_any` and `_downgrade_probe`),
//! with an autounmask pass that admits soft-masked candidates when nothing else
//! satisfies the atom.
//!
//! `dependencies` reduces the version's USE-conditional groups against its
//! resolved USE, encodes the result into solver requirements, enforces
//! REQUIRED_USE on the chosen USE, and reports a sub-slot rebuild trigger when
//! the chosen provider's sub-slot differs from an installed `:=` binding.

use std::collections::{BTreeMap, BTreeSet};

use moraine_eapi::features_for;
use moraine_solver::{Dependencies, DependencyProvider, Range};
use moraine_version::Version;

use crate::depnode::DepNode;
use crate::encode::{Encoder, UseProposal};
use crate::required_use::{RequiredUseOutcome, evaluate_required_use};
use crate::source::{PackageMeta, ResolveSource};

/// The synthetic root package name for a resolution request.
pub(crate) const REQUEST_CP: &str = "@request";

/// The solver package key for a `(cp, slot)`: `cp:slot`. A `cp`
/// (`category/package`) contains no colon and a main slot contains neither colon
/// nor slash, so the split is unambiguous and two slots of one `cp` are distinct
/// solver variables that can co-install.
pub(crate) fn package_key(cp: &str, slot: &str) -> String {
    format!("{cp}:{slot}")
}

/// Split a solver package key back into `(cp, slot)`. Returns `None` for the
/// synthetic root or any key without a slot separator.
pub(crate) fn split_key(key: &str) -> Option<(&str, &str)> {
    if key == REQUEST_CP {
        return None;
    }
    key.split_once(':')
}

/// The Gentoo dependency-provider.
pub struct GentooProvider<'s, S: ResolveSource> {
    source: &'s S,
    request: Vec<crate::depnode::NormAtom>,
    modifiers: crate::resolve::Modifiers,
    /// `||` branch-leader keys masked by the resolve layer's fallback loop.
    branch_mask: BTreeSet<String>,
    /// The `||` branch decisions made during the most recent encoding pass.
    branch_points: std::cell::RefCell<Vec<crate::encode::BranchPoint>>,
    /// The `(cp, slot)` keys already pulled into the partial solution, seeded
    /// from the previous solve's decisions and grown as the current pass forces
    /// alternatives. A slotless atom prefers a key already present here.
    in_graph: std::cell::RefCell<BTreeSet<String>>,
    /// USE-autounmask overrides seeded by the resolve layer: `cp` to its
    /// proposed enabled USE, consulted by [`Self::effective_use`] in place of
    /// `source.resolved_use`.
    use_overrides: BTreeMap<String, BTreeSet<String>>,
    /// USE-autounmask proposals discovered during the most recent encoding pass.
    use_proposals: std::cell::RefCell<Vec<UseProposal>>,
}

impl<'s, S: ResolveSource> GentooProvider<'s, S> {
    /// Create a provider over the given data source with no request atoms (used
    /// when the provider is exercised directly in tests).
    pub fn new(source: &'s S) -> Self {
        GentooProvider {
            source,
            request: Vec::new(),
            modifiers: crate::resolve::Modifiers::default(),
            branch_mask: BTreeSet::new(),
            branch_points: std::cell::RefCell::new(Vec::new()),
            in_graph: std::cell::RefCell::new(BTreeSet::new()),
            use_overrides: BTreeMap::new(),
            use_proposals: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Create a provider whose synthetic root depends on the given request
    /// atoms, masking the given `||` branch-leader keys, seeding the given
    /// in-graph `(cp, slot)` keys, and seeding the given USE-autounmask
    /// overrides.
    pub(crate) fn with_request(
        source: &'s S,
        request: Vec<crate::depnode::NormAtom>,
        modifiers: crate::resolve::Modifiers,
        branch_mask: BTreeSet<String>,
        in_graph: BTreeSet<String>,
        use_overrides: BTreeMap<String, BTreeSet<String>>,
    ) -> Self {
        GentooProvider {
            source,
            request,
            modifiers,
            branch_mask,
            branch_points: std::cell::RefCell::new(Vec::new()),
            in_graph: std::cell::RefCell::new(in_graph),
            use_overrides,
            use_proposals: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// The `||` branch decisions recorded during the most recent solve.
    pub(crate) fn branch_points(&self) -> Vec<crate::encode::BranchPoint> {
        self.branch_points.borrow().clone()
    }

    /// The USE-autounmask proposals recorded during the most recent solve.
    pub(crate) fn use_proposals(&self) -> Vec<UseProposal> {
        self.use_proposals.borrow().clone()
    }

    /// The effective enabled USE for a candidate: the seeded USE-autounmask
    /// override for its `cp` when present, otherwise the source's resolved USE.
    fn effective_use(&self, meta: &PackageMeta) -> BTreeSet<String> {
        self.use_overrides
            .get(&meta.cp)
            .cloned()
            .unwrap_or_else(|| self.source.resolved_use(meta))
    }

    /// Borrow the underlying source.
    pub fn source(&self) -> &'s S {
        self.source
    }

    /// The metadata for a concrete `(cp, slot)` at `version`, if known.
    pub(crate) fn meta(&self, cp: &str, slot: &str, version: &Version) -> Option<PackageMeta> {
        self.source
            .versions_of(cp)
            .into_iter()
            .find(|m| m.slot == slot && &m.version == version)
    }

    /// Rank the visible candidate versions of one `(cp, slot)` within `range`,
    /// highest version first. The slot is fixed by the solver key, so multiple
    /// slots of one `cp` are independent variables.
    fn ranked_candidates(&self, cp: &str, slot: &str, range: &Range<Version>) -> Vec<Version> {
        let installed = self.source.installed(cp);

        // Candidate pass: visible versions of this slot, plus the installed
        // version of this slot even when masked, so long as the repository still
        // carries it. Portage always offers the installed package as a candidate
        // (`_iter_match_pkgs_any`), so an already-satisfied dependency is never
        // forced to change. Offering the installed version also avoids a
        // downgrade: when every visible candidate is strictly lower than the
        // installed one, the installed version stays in the set and ranks first,
        // matching `_downgrade_probe`, which permits a downgrade only when no
        // available package is greater than or equal to the installed one.
        let mut strict: Vec<PackageMeta> = self
            .source
            .versions_of(cp)
            .into_iter()
            .filter(|m| {
                m.slot == slot
                    && range.contains(&m.version)
                    && (self.source.is_visible(m)
                        || installed
                            .iter()
                            .any(|i| i.version == m.version && i.slot == m.slot))
            })
            .collect();

        if strict.is_empty() {
            // Autounmask pass: include soft-masked (keyword/license) candidates so
            // the solver can resolve through them. The required acceptance change
            // is reported after resolution; hard-masked (`package.mask`) versions
            // stay excluded.
            let mut relaxed: Vec<PackageMeta> = self
                .source
                .versions_of(cp)
                .into_iter()
                .filter(|m| {
                    m.slot == slot
                        && range.contains(&m.version)
                        && self.source.acceptability(m).is_autounmaskable()
                })
                .collect();
            relaxed.sort_by(|a, b| {
                let ab = self.source.has_binary(cp, &a.version);
                let bb = self.source.has_binary(cp, &b.version);
                bb.cmp(&ab).then_with(|| b.version.cmp(&a.version))
            });
            return relaxed.into_iter().map(|m| m.version).collect();
        }

        // Prefer the version installed in this slot (Portage's default keeps an
        // installed package rather than needlessly upgrading or downgrading it),
        // then the highest version. Under `--update` the installed-version
        // preference is dropped so the highest available version wins; an
        // installed version higher than every visible one still ranks first
        // there, since it is the highest candidate, so it is not downgraded.
        let installed_versions: BTreeSet<Version> = installed
            .iter()
            .filter(|i| i.slot == slot)
            .map(|i| i.version.clone())
            .collect();
        let update = self.modifiers.update;
        strict.sort_by(|a, b| {
            let by_installed = if update {
                std::cmp::Ordering::Equal
            } else {
                let ai = installed_versions.contains(&a.version);
                let bi = installed_versions.contains(&b.version);
                bi.cmp(&ai)
            };
            let ab = self.source.has_binary(cp, &a.version);
            let bb = self.source.has_binary(cp, &b.version);
            by_installed
                .then_with(|| bb.cmp(&ab))
                .then_with(|| b.version.cmp(&a.version))
        });
        strict.into_iter().map(|m| m.version).collect()
    }
}

impl<S: ResolveSource> DependencyProvider for GentooProvider<'_, S> {
    type Package = String;
    type Version = Version;

    fn candidates(&self, package: &String, range: &Range<Version>) -> Vec<Version> {
        if package == REQUEST_CP {
            let v = Version::parse("0").expect("synthetic version parses");
            return if range.contains(&v) {
                vec![v]
            } else {
                Vec::new()
            };
        }
        match split_key(package) {
            Some((cp, slot)) => self.ranked_candidates(cp, slot, range),
            None => Vec::new(),
        }
    }

    fn dependencies(&self, package: &String, version: &Version) -> Dependencies<String, Version> {
        if package == REQUEST_CP {
            let encoder = Encoder {
                source: self.source,
                branch_mask: &self.branch_mask,
                branch_points: &self.branch_points,
                in_graph: &self.in_graph,
                use_overrides: &self.use_overrides,
                use_proposals: &self.use_proposals,
            };
            return Dependencies::Known(encoder.request_requirements(&self.request));
        }
        let Some((cp, slot)) = split_key(package) else {
            return Dependencies::Unavailable(format!("malformed package key {package}"));
        };
        let Some(meta) = self.meta(cp, slot, version) else {
            return Dependencies::Unavailable(format!("no metadata for {package}-{version}"));
        };
        let package = cp;
        let features = features_for(&meta.eapi);
        // The candidate's own USE reflects any seeded USE-autounmask override, so
        // its conditional dependencies are reduced against the proposed USE.
        let resolved_use = self.effective_use(&meta);

        // Validate strong blockers against the EAPI feature table.
        let encoder = Encoder {
            source: self.source,
            branch_mask: &self.branch_mask,
            branch_points: &self.branch_points,
            in_graph: &self.in_graph,
            use_overrides: &self.use_overrides,
            use_proposals: &self.use_proposals,
        };
        let nodes: [&DepNode; 5] = [
            &meta.bdepend,
            &meta.depend,
            &meta.rdepend,
            &meta.pdepend,
            &meta.idepend,
        ];
        if let Err(e) = encoder.validate_blockers(package, &nodes, features) {
            return Dependencies::Unavailable(e.to_string());
        }

        // Enforce REQUIRED_USE on the resolved USE; a violation makes this
        // (package, version, USE) choice unusable, which the solver records and
        // explains.
        if features.required_use
            && let RequiredUseOutcome::Violated(constraint) =
                evaluate_required_use(&meta.required_use, &resolved_use)
        {
            return Dependencies::Unavailable(format!("REQUIRED_USE violated: {constraint}"));
        }

        // An already-installed package at this exact version and slot is not
        // rebuilt, so its build-time dependencies are not pulled into the graph
        // (matching Portage's default and a binary install). A new install or
        // upgrade still pulls its build deps. Under `--newuse`/`--changed-use` a
        // USE change makes the package a reinstall, so its build deps are pulled
        // again; `--newuse` also fires on an IUSE-only change. The same helper
        // drives the `already_installed` decision so the two sites stay
        // consistent.
        let use_rebuild =
            crate::resolve::use_changed(self.source, &meta, &resolved_use, self.modifiers);
        let skip_build = self.source.installed_matches(cp, &meta.version, slot) && !use_rebuild;

        match encoder.requirements(&meta, &resolved_use, features, skip_build) {
            Ok(reqs) => Dependencies::Known(reqs),
            Err(reason) => Dependencies::Unavailable(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::required_use::{RequiredUseOutcome, evaluate_required_use, parse_required_use};

    #[test]
    fn arch_gated_required_use_enforced_on_matching_arch() {
        // The profile arch keyword is in the resolved USE, so an arch-gated
        // REQUIRED_USE constraint is active rather than trivially satisfied.
        let constraint = parse_required_use("x86? ( cpu_flags_x86_sse2 )");

        // On an x86 profile the constraint is active and unsatisfied.
        let on_x86: BTreeSet<String> = ["x86".to_owned()].into_iter().collect();
        assert!(matches!(
            evaluate_required_use(&constraint, &on_x86),
            RequiredUseOutcome::Violated(_)
        ));

        // Enabling the required flag satisfies it.
        let satisfied: BTreeSet<String> = ["x86".to_owned(), "cpu_flags_x86_sse2".to_owned()]
            .into_iter()
            .collect();
        assert!(matches!(
            evaluate_required_use(&constraint, &satisfied),
            RequiredUseOutcome::Satisfied
        ));

        // Without the arch flag the conditional is inactive, so it is satisfied.
        let off: BTreeSet<String> = BTreeSet::new();
        assert!(matches!(
            evaluate_required_use(&constraint, &off),
            RequiredUseOutcome::Satisfied
        ));
    }
}
