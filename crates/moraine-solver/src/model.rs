//! Core model: incompatibilities, assignments, and the partial solution.

use std::fmt::Debug;
use std::hash::Hash;

use crate::range::Range;
use crate::term::{Relation, Term};

/// An index into the solver's incompatibility set.
pub type IncompatId = usize;

/// Why an incompatibility exists, forming the derivation graph.
#[derive(Debug, Clone)]
pub enum Cause<P> {
    /// The root request.
    Root,
    /// `dependent` depends on `dependency`.
    Dependency {
        /// The dependent package.
        dependent: P,
        /// The depended-upon package.
        dependency: P,
    },
    /// No versions of the package satisfy a required range.
    NoVersions(P),
    /// A chosen version is unavailable for the given reason.
    Unavailable(P, String),
    /// A no-good learned by resolving two incompatibilities during conflict
    /// analysis, linking to its parents to form the derivation graph.
    Derived(IncompatId, IncompatId),
}

/// A set of terms that cannot all hold simultaneously.
#[derive(Debug, Clone)]
pub struct Incompatibility<P, V> {
    /// The terms, one per package.
    pub terms: Vec<(P, Term<V>)>,
    /// The cause of this incompatibility.
    pub cause: Cause<P>,
}

/// A single assignment in the partial solution: a decision (a chosen version)
/// or a derivation (a term forced by unit propagation).
#[derive(Debug, Clone)]
pub struct Assignment<P, V> {
    /// The package assigned.
    pub package: P,
    /// The assigned term.
    pub term: Term<V>,
    /// The decision level at which this assignment was made.
    pub decision_level: u32,
    /// The incompatibility that forced a derivation, or `None` for a decision.
    pub cause: Option<IncompatId>,
    /// The chosen version, present only for decisions.
    pub version: Option<V>,
}

/// The ordered list of assignments, the heart of the solver state.
#[derive(Debug)]
pub struct PartialSolution<P, V> {
    /// All assignments in chronological order.
    pub assignments: Vec<Assignment<P, V>>,
    /// The current decision level (number of decisions in effect).
    pub decision_level: u32,
}

impl<P, V> Default for PartialSolution<P, V> {
    fn default() -> Self {
        PartialSolution {
            assignments: Vec::new(),
            decision_level: 0,
        }
    }
}

impl<P: Clone + Eq + Ord + Hash + Debug, V: Clone + Ord + Debug> PartialSolution<P, V> {
    /// The accumulated allowed set for a package (the intersection of every
    /// assignment's term set), or the full range if unassigned.
    pub fn accumulated_set(&self, package: &P) -> Range<V> {
        let mut acc = Range::full();
        for a in &self.assignments {
            if &a.package == package {
                acc = acc.intersection(&a.term.set());
            }
        }
        acc
    }

    /// The accumulated term for a package (the conjunction of every assignment's
    /// term), or `None` if the package is unassigned. The polarity reflects
    /// whether the package is positively required.
    pub fn accumulated_term(&self, package: &P) -> Option<Term<V>> {
        let mut acc: Option<Term<V>> = None;
        for a in &self.assignments {
            if &a.package == package {
                acc = Some(match acc {
                    None => a.term.clone(),
                    Some(prev) => prev.intersection(&a.term),
                });
            }
        }
        acc
    }

    /// The accumulated term for a package, or the universe term if unassigned.
    pub fn accumulated_or_universe(&self, package: &P) -> Term<V> {
        self.accumulated_term(package).unwrap_or_else(Term::any)
    }

    /// Whether the package has a concrete decision.
    pub fn is_decided(&self, package: &P) -> bool {
        self.assignments
            .iter()
            .any(|a| &a.package == package && a.version.is_some())
    }

    /// Whether the package has been positively required by some assignment, so
    /// it needs a decision.
    pub fn is_required(&self, package: &P) -> bool {
        self.assignments
            .iter()
            .any(|a| &a.package == package && a.term.is_positive())
    }

    /// Record a decision, incrementing the decision level.
    pub fn add_decision(&mut self, package: P, version: V) {
        self.decision_level += 1;
        self.assignments.push(Assignment {
            term: Term::positive(Range::singleton(version.clone())),
            decision_level: self.decision_level,
            cause: None,
            version: Some(version),
            package,
        });
    }

    /// Record a derivation forced by `cause`.
    pub fn add_derivation(&mut self, package: P, term: Term<V>, cause: IncompatId) {
        self.assignments.push(Assignment {
            package,
            term,
            decision_level: self.decision_level,
            cause: Some(cause),
            version: None,
        });
    }

    /// Remove all assignments above `level` and reset the decision level.
    pub fn backtrack(&mut self, level: u32) {
        self.assignments.retain(|a| a.decision_level <= level);
        self.decision_level = level;
    }

    /// The global index of the earliest assignment after which `term` for
    /// `package` is satisfied by the accumulated set.
    pub fn satisfier(&self, package: &P, term: &Term<V>) -> Option<usize> {
        let mut acc = Term::any();
        for (i, a) in self.assignments.iter().enumerate() {
            if &a.package == package {
                acc = acc.intersection(&a.term);
                if term.relation(&acc) == Relation::Satisfied {
                    return Some(i);
                }
            }
        }
        None
    }

    /// The concrete decisions as `(package, version)` pairs.
    pub fn decisions(&self) -> Vec<(P, V)> {
        self.assignments
            .iter()
            .filter_map(|a| a.version.clone().map(|v| (a.package.clone(), v)))
            .collect()
    }
}
