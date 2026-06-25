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
//! range and by visibility, and ranks them in Portage `dep_zapdeps` order
//! (installed-slot match first, then upgrades), with a relaxed second pass that
//! includes masked-installed candidates when the strict pass is empty.
//!
//! `dependencies` reduces the version's USE-conditional groups against its
//! resolved USE, encodes the result into solver requirements, enforces
//! REQUIRED_USE on the chosen USE, and reports a sub-slot rebuild trigger when
//! the chosen provider's sub-slot differs from an installed `:=` binding.

use std::collections::BTreeSet;

use moraine_eapi::features_for;
use moraine_solver::{Dependencies, DependencyProvider, Range, Requirements, Term};
use moraine_version::Version;

use crate::depnode::DepNode;
use crate::encode::Encoder;
use crate::required_use::{RequiredUseOutcome, evaluate_required_use};
use crate::source::{PackageMeta, ResolveSource};

/// The synthetic root package name for a resolution request.
pub(crate) const REQUEST_CP: &str = "@request";

/// The Gentoo dependency-provider.
pub struct GentooProvider<'s, S: ResolveSource> {
    source: &'s S,
    request: Vec<crate::depnode::NormAtom>,
}

impl<'s, S: ResolveSource> GentooProvider<'s, S> {
    /// Create a provider over the given data source with no request atoms (used
    /// when the provider is exercised directly in tests).
    pub fn new(source: &'s S) -> Self {
        GentooProvider {
            source,
            request: Vec::new(),
        }
    }

    /// Create a provider whose synthetic root depends on the given request
    /// atoms.
    pub(crate) fn with_request(source: &'s S, request: Vec<crate::depnode::NormAtom>) -> Self {
        GentooProvider { source, request }
    }

    /// Borrow the underlying source.
    pub fn source(&self) -> &'s S {
        self.source
    }

    /// The metadata for a concrete `cp` at `version`, if known.
    pub(crate) fn meta(&self, cp: &str, version: &Version) -> Option<PackageMeta> {
        self.source
            .versions_of(cp)
            .into_iter()
            .find(|m| &m.version == version)
    }

    /// Rank the visible candidate versions of `cp` within `range`, best first.
    fn ranked_candidates(&self, cp: &str, range: &Range<Version>) -> Vec<Version> {
        let installed = self.source.installed(cp);
        let installed_slots: BTreeSet<String> = installed.iter().map(|i| i.slot.clone()).collect();

        // Strict pass: only visible candidates.
        let mut strict: Vec<PackageMeta> = self
            .source
            .versions_of(cp)
            .into_iter()
            .filter(|m| range.contains(&m.version) && self.source.is_visible(m))
            .collect();

        if strict.is_empty() {
            // Relaxed pass: include masked-installed candidates.
            let mut relaxed: Vec<PackageMeta> = self
                .source
                .versions_of(cp)
                .into_iter()
                .filter(|m| {
                    range.contains(&m.version)
                        && installed
                            .iter()
                            .any(|i| i.version == m.version && i.slot == m.slot)
                })
                .collect();
            sort_candidates(&mut relaxed, &installed_slots);
            return relaxed.into_iter().map(|m| m.version).collect();
        }

        sort_candidates(&mut strict, &installed_slots);
        strict.into_iter().map(|m| m.version).collect()
    }
}

/// Sort candidates in Portage preference order: installed-slot matches first
/// (highest version within), then upgrades (highest version overall).
fn sort_candidates(metas: &mut [PackageMeta], installed_slots: &BTreeSet<String>) {
    metas.sort_by(|a, b| {
        let a_inst = installed_slots.contains(&a.slot);
        let b_inst = installed_slots.contains(&b.slot);
        // Installed-slot match first.
        b_inst
            .cmp(&a_inst)
            // Then highest version first.
            .then_with(|| b.version.cmp(&a.version))
    });
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
        self.ranked_candidates(package, range)
    }

    fn dependencies(&self, package: &String, version: &Version) -> Dependencies<String, Version> {
        if package == REQUEST_CP {
            let encoder = Encoder {
                source: self.source,
            };
            let mut reqs: Requirements<String, Version> = Requirements::new();
            let parent_use = BTreeSet::new();
            for atom in &self.request {
                if atom.cp.starts_with("virtual/") {
                    match encoder.expand_virtual_pub(atom) {
                        Some(alts) => {
                            if let Some((cp, term)) = alts.first() {
                                reqs.clauses
                                    .push(moraine_solver::Clause::single(cp.clone(), term.clone()));
                            }
                            if alts.len() > 1 {
                                reqs.clauses.push(moraine_solver::Clause::any_of(alts));
                            }
                        }
                        // No provider: force a clause the solver cannot satisfy.
                        None => reqs.clauses.push(moraine_solver::Clause::single(
                            atom.cp.clone(),
                            Term::positive(Range::full()),
                        )),
                    }
                    continue;
                }
                match encoder.required_term_pub(atom, &parent_use) {
                    Some(term) => reqs
                        .clauses
                        .push(moraine_solver::Clause::single(atom.cp.clone(), term)),
                    // No candidate: require any version so the solver fails with
                    // a clean "no versions" explanation for this cp.
                    None => reqs.clauses.push(moraine_solver::Clause::single(
                        atom.cp.clone(),
                        Term::positive(Range::full()),
                    )),
                }
            }
            return Dependencies::Known(reqs);
        }
        let Some(meta) = self.meta(package, version) else {
            return Dependencies::Unavailable(format!("no metadata for {package}-{version}"));
        };
        let features = features_for(&meta.eapi);
        let resolved_use = self.source.resolved_use(&meta);

        // Validate strong blockers against the EAPI feature table.
        let encoder = Encoder {
            source: self.source,
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

        match encoder.requirements(&meta, &resolved_use, features) {
            Ok(reqs) => Dependencies::Known(reqs),
            Err(reason) => Dependencies::Unavailable(reason),
        }
    }
}
