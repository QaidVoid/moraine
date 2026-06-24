//! The conflict-driven solver loop.
//!
//! This is the PubGrub algorithm: unit propagation over incompatibilities,
//! conflict resolution by clause learning, and backjumping (never a whole-state
//! deep copy). It is generic over the [`DependencyProvider`] and carries no
//! package-manager semantics.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{Cause, IncompatId, Incompatibility, PartialSolution};
use crate::provider::{Dependencies, DependencyProvider};
use crate::range::Range;
use crate::report::{Explanation, Failure, Solution};
use crate::term::{Relation, Term};

/// Relation of a whole incompatibility to the partial solution.
enum IncompatRelation {
    /// Every term is satisfied: a conflict.
    Satisfied,
    /// Exactly one term is inconclusive; the rest are satisfied.
    AlmostSatisfied(usize),
    /// Otherwise (a term is contradicted, or more than one is inconclusive).
    None,
}

struct Solver<'p, DP: DependencyProvider> {
    provider: &'p DP,
    incompatibilities: Vec<Incompatibility<DP::Package, DP::Version>>,
    by_package: BTreeMap<DP::Package, Vec<IncompatId>>,
    partial: PartialSolution<DP::Package, DP::Version>,
    root: DP::Package,
}

/// Solve for a conflict-free set of package versions starting from a root
/// request, or return a structured failure explanation.
pub fn solve<DP: DependencyProvider>(
    provider: &DP,
    root_package: DP::Package,
    root_version: DP::Version,
) -> Solution<DP::Package, DP::Version> {
    let mut solver = Solver {
        provider,
        incompatibilities: Vec::new(),
        by_package: BTreeMap::new(),
        partial: PartialSolution::default(),
        root: root_package.clone(),
    };
    solver.add_incompat(Incompatibility {
        terms: vec![(
            root_package.clone(),
            Term::negative(Range::singleton(root_version)),
        )],
        cause: Cause::Root,
    });
    solver.run(root_package)
}

impl<DP: DependencyProvider> Solver<'_, DP> {
    fn run(&mut self, mut next: DP::Package) -> Solution<DP::Package, DP::Version> {
        loop {
            self.propagate(next.clone())?;
            match self.choose_package() {
                None => return Ok(self.partial.decisions().into_iter().collect()),
                Some(pkg) => {
                    next = self.decide(pkg);
                }
            }
        }
    }

    fn decide(&mut self, pkg: DP::Package) -> DP::Package {
        let range = self
            .partial
            .accumulated_term(&pkg)
            .map(|t| t.set())
            .unwrap_or_else(Range::full);
        let candidate = self.provider.candidates(&pkg, &range).into_iter().next();
        match candidate {
            None => {
                let term = self
                    .partial
                    .accumulated_term(&pkg)
                    .unwrap_or_else(Term::any);
                self.add_incompat(Incompatibility {
                    terms: vec![(pkg.clone(), term)],
                    cause: Cause::NoVersions(pkg.clone()),
                });
                pkg
            }
            Some(version) => match self.provider.dependencies(&pkg, &version) {
                Dependencies::Unavailable(reason) => {
                    self.add_incompat(Incompatibility {
                        terms: vec![(pkg.clone(), Term::positive(Range::singleton(version)))],
                        cause: Cause::Unavailable(pkg.clone(), reason),
                    });
                    pkg
                }
                Dependencies::Known(deps) => {
                    for (dep_pkg, dep_term) in deps {
                        self.add_incompat(Incompatibility {
                            terms: vec![
                                (
                                    pkg.clone(),
                                    Term::positive(Range::singleton(version.clone())),
                                ),
                                (dep_pkg.clone(), dep_term.negate()),
                            ],
                            cause: Cause::Dependency {
                                dependent: pkg.clone(),
                                dependency: dep_pkg,
                            },
                        });
                    }
                    self.partial.add_decision(pkg.clone(), version);
                    pkg
                }
            },
        }
    }

    fn add_incompat(&mut self, incompat: Incompatibility<DP::Package, DP::Version>) -> IncompatId {
        let id = self.incompatibilities.len();
        for (pkg, _) in &incompat.terms {
            self.by_package.entry(pkg.clone()).or_default().push(id);
        }
        self.incompatibilities.push(incompat);
        id
    }

    fn relate(&self, id: IncompatId) -> IncompatRelation {
        let mut inconclusive = None;
        let mut count = 0;
        for (i, (pkg, term)) in self.incompatibilities[id].terms.iter().enumerate() {
            let acc = self.partial.accumulated_or_universe(pkg);
            match term.relation(&acc) {
                Relation::Contradicted => return IncompatRelation::None,
                Relation::Satisfied => {}
                Relation::Inconclusive => {
                    count += 1;
                    inconclusive = Some(i);
                }
            }
        }
        match (count, inconclusive) {
            (0, _) => IncompatRelation::Satisfied,
            (1, Some(i)) => IncompatRelation::AlmostSatisfied(i),
            _ => IncompatRelation::None,
        }
    }

    fn propagate(&mut self, package: DP::Package) -> Result<(), Failure<DP::Package, DP::Version>> {
        let mut changed = vec![package];
        while let Some(pkg) = changed.pop() {
            let ids = self.by_package.get(&pkg).cloned().unwrap_or_default();
            for id in ids.into_iter().rev() {
                match self.relate(id) {
                    IncompatRelation::Satisfied => {
                        let root_cause = self.resolve_conflict(id)?;
                        if let IncompatRelation::AlmostSatisfied(ti) = self.relate(root_cause) {
                            let (p, t) = self.incompatibilities[root_cause].terms[ti].clone();
                            self.partial
                                .add_derivation(p.clone(), t.negate(), root_cause);
                            changed.clear();
                            changed.push(p);
                        } else {
                            changed.clear();
                        }
                        break;
                    }
                    IncompatRelation::AlmostSatisfied(ti) => {
                        let (p, t) = self.incompatibilities[id].terms[ti].clone();
                        self.partial.add_derivation(p.clone(), t.negate(), id);
                        if !changed.contains(&p) {
                            changed.push(p);
                        }
                    }
                    IncompatRelation::None => {}
                }
            }
        }
        Ok(())
    }

    fn is_failure(&self, id: IncompatId) -> bool {
        let terms = &self.incompatibilities[id].terms;
        terms.is_empty() || (terms.len() == 1 && terms[0].0 == self.root)
    }

    fn resolve_conflict(
        &mut self,
        mut incompat: IncompatId,
    ) -> Result<IncompatId, Failure<DP::Package, DP::Version>> {
        loop {
            if self.is_failure(incompat) {
                return Err(Failure {
                    explanation: self.build_explanation(incompat, &mut BTreeSet::new()),
                });
            }

            let terms = self.incompatibilities[incompat].terms.clone();
            let mut most_recent_term = 0usize;
            let mut most_recent_sat: Option<usize> = None;
            let mut previous_level = 0u32;
            for (ti, (pkg, term)) in terms.iter().enumerate() {
                let sat = self
                    .partial
                    .satisfier(pkg, term)
                    .expect("a conflicting term must have a satisfier");
                match most_recent_sat {
                    None => {
                        most_recent_sat = Some(sat);
                        most_recent_term = ti;
                    }
                    Some(prev) if sat > prev => {
                        previous_level =
                            previous_level.max(self.partial.assignments[prev].decision_level);
                        most_recent_sat = Some(sat);
                        most_recent_term = ti;
                    }
                    Some(_) => {
                        previous_level =
                            previous_level.max(self.partial.assignments[sat].decision_level);
                    }
                }
            }

            let sat_idx = most_recent_sat.expect("a conflict has at least one term");
            let satisfier = self.partial.assignments[sat_idx].clone();
            let (mr_pkg, mr_term) = terms[most_recent_term].clone();

            if previous_level < satisfier.decision_level || satisfier.cause.is_none() {
                self.partial.backtrack(previous_level);
                return Ok(incompat);
            }

            let cause_id = satisfier.cause.expect("a derivation has a cause");
            let cause_terms = self.incompatibilities[cause_id].terms.clone();

            let mut merged: Vec<(DP::Package, Term<DP::Version>)> = Vec::new();
            for (i, (pkg, term)) in terms.iter().enumerate() {
                if i != most_recent_term {
                    merge_term(&mut merged, pkg.clone(), term.clone());
                }
            }
            for (pkg, term) in &cause_terms {
                if *pkg != satisfier.package {
                    merge_term(&mut merged, pkg.clone(), term.clone());
                }
            }
            if let Some(diff) = satisfier.term.difference(&mr_term) {
                merge_term(&mut merged, mr_pkg, diff.negate());
            }

            incompat = self.add_incompat(Incompatibility {
                terms: merged,
                cause: Cause::Derived(incompat, cause_id),
            });
        }
    }

    fn build_explanation(
        &self,
        id: IncompatId,
        visited: &mut BTreeSet<IncompatId>,
    ) -> Explanation<DP::Package, DP::Version> {
        if !visited.insert(id) {
            return Explanation::Shared(id);
        }
        let inc = &self.incompatibilities[id];
        match &inc.cause {
            Cause::Derived(a, b) => Explanation::Derived {
                incompat: id,
                terms: inc.terms.clone(),
                causes: vec![
                    self.build_explanation(*a, visited),
                    self.build_explanation(*b, visited),
                ],
            },
            other => Explanation::External {
                incompat: id,
                description: describe_cause(other),
                terms: inc.terms.clone(),
            },
        }
    }

    fn choose_package(&self) -> Option<DP::Package> {
        let mut candidates: BTreeSet<DP::Package> = BTreeSet::new();
        for a in &self.partial.assignments {
            if self.partial.is_decided(&a.package) {
                continue;
            }
            if let Some(term) = self.partial.accumulated_term(&a.package)
                && term.is_positive()
                && !term.is_empty()
            {
                candidates.insert(a.package.clone());
            }
        }
        candidates.into_iter().next()
    }
}

fn merge_term<P: Eq, V: Ord + Clone>(merged: &mut Vec<(P, Term<V>)>, pkg: P, term: Term<V>) {
    if let Some(slot) = merged.iter_mut().find(|(p, _)| *p == pkg) {
        slot.1 = slot.1.intersection(&term);
    } else {
        merged.push((pkg, term));
    }
}

fn describe_cause<P: std::fmt::Debug>(cause: &Cause<P>) -> String {
    match cause {
        Cause::Root => "the root request".to_owned(),
        Cause::Dependency {
            dependent,
            dependency,
        } => format!("{dependent:?} depends on {dependency:?}"),
        Cause::NoVersions(p) => format!("no versions of {p:?} satisfy the constraint"),
        Cause::Unavailable(p, reason) => format!("{p:?} is unavailable: {reason}"),
        Cause::Derived(..) => "a learned conflict".to_owned(),
    }
}
