//! The dependency-provider boundary.
//!
//! The solver core knows nothing about package-manager semantics. It asks the
//! provider for candidate versions of a package within a range (best-first,
//! provider-ranked) and for the dependencies of a chosen version. All domain
//! knowledge lives behind this trait.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::hash::Hash;

use crate::range::Range;
use crate::term::Term;

/// A single requirement clause: a disjunction over alternatives.
///
/// A plain dependency is a clause with exactly one alternative. A `||`
/// any-of group or a virtual expansion is a clause with several
/// alternatives, of which at least one must hold. Each alternative is a
/// `(package, term)` pair, in the provider's preference order.
#[derive(Debug, Clone)]
pub struct Clause<P, V> {
    /// The alternatives, at least one of which must be satisfied. Listed in
    /// preference order (most preferred first).
    pub alternatives: Vec<(P, Term<V>)>,
}

impl<P, V> Clause<P, V> {
    /// A clause with a single alternative (an ordinary dependency).
    pub fn single(package: P, term: Term<V>) -> Self {
        Clause {
            alternatives: vec![(package, term)],
        }
    }

    /// A clause that is a disjunction over the given alternatives.
    pub fn any_of(alternatives: Vec<(P, Term<V>)>) -> Self {
        Clause { alternatives }
    }
}

/// The full requirement set of a concrete package version: a conjunction of
/// disjunctive clauses plus a set of conflicts.
///
/// A `conflict (q, term)` means "if this version is chosen, then `q` must NOT
/// be in `term`'s set", which encodes blockers and slot collisions.
#[derive(Debug, Clone)]
pub struct Requirements<P, V> {
    /// The clauses, all of which must be satisfied (each is a disjunction).
    pub clauses: Vec<Clause<P, V>>,
    /// Conflicts: pairs that must not both hold together with this version.
    pub conflicts: Vec<(P, Term<V>)>,
}

impl<P, V> Default for Requirements<P, V> {
    fn default() -> Self {
        Requirements {
            clauses: Vec::new(),
            conflicts: Vec::new(),
        }
    }
}

impl<P, V> Requirements<P, V> {
    /// An empty requirement set.
    pub fn new() -> Self {
        Requirements::default()
    }

    /// Build a requirement set from a list of plain single-alternative
    /// dependencies, with no disjunctions and no conflicts. Keeps simple
    /// call-sites and tests terse.
    pub fn simple(deps: Vec<(P, Term<V>)>) -> Self {
        Requirements {
            clauses: deps
                .into_iter()
                .map(|(p, t)| Clause::single(p, t))
                .collect(),
            conflicts: Vec::new(),
        }
    }
}

/// The dependencies of a concrete package version.
#[derive(Debug, Clone)]
pub enum Dependencies<P, V> {
    /// The version is available and carries these requirements.
    Known(Requirements<P, V>),
    /// The version cannot be used; the string explains why.
    Unavailable(String),
}

/// A list of `(package, term)` dependency requirements.
pub type DepList<P, V> = Vec<(P, Term<V>)>;

/// Supplies candidate versions and dependencies to the solver.
pub trait DependencyProvider {
    /// The package identifier type.
    type Package: Clone + Eq + Ord + Hash + Debug;
    /// The version type.
    type Version: Clone + Ord + Debug;

    /// Candidate versions for `package` within `range`, best preference first.
    /// The core imposes no reordering; the provider's order is authoritative.
    fn candidates(
        &self,
        package: &Self::Package,
        range: &Range<Self::Version>,
    ) -> Vec<Self::Version>;

    /// The dependencies of `package` at `version`.
    fn dependencies(
        &self,
        package: &Self::Package,
        version: &Self::Version,
    ) -> Dependencies<Self::Package, Self::Version>;
}

/// A simple in-memory provider over an integer-like version type, for tests and
/// benchmarks.
#[derive(Debug, Default, Clone)]
pub struct MapProvider<P: Ord + Clone> {
    versions: BTreeMap<P, Vec<u32>>,
    deps: BTreeMap<(P, u32), DepList<P, u32>>,
}

impl<P: Ord + Clone + Hash + Debug> MapProvider<P> {
    /// Create an empty provider.
    pub fn new() -> Self {
        MapProvider {
            versions: BTreeMap::new(),
            deps: BTreeMap::new(),
        }
    }

    /// Register a package with its available versions (any order; the provider
    /// serves them highest-first).
    pub fn add_package(&mut self, package: P, mut versions: Vec<u32>) {
        versions.sort_unstable();
        versions.dedup();
        self.versions.insert(package, versions);
    }

    /// Register the dependencies of a concrete package version.
    pub fn add_dependency(&mut self, package: P, version: u32, deps: DepList<P, u32>) {
        self.deps.insert((package, version), deps);
    }
}

impl<P: Ord + Clone + Hash + Debug> DependencyProvider for MapProvider<P> {
    type Package = P;
    type Version = u32;

    fn candidates(&self, package: &P, range: &Range<u32>) -> Vec<u32> {
        match self.versions.get(package) {
            // Highest version first.
            Some(vs) => vs
                .iter()
                .rev()
                .copied()
                .filter(|v| range.contains(v))
                .collect(),
            None => Vec::new(),
        }
    }

    fn dependencies(&self, package: &P, version: &u32) -> Dependencies<P, u32> {
        match self.deps.get(&(package.clone(), *version)) {
            Some(deps) => Dependencies::Known(Requirements::simple(deps.clone())),
            None => Dependencies::Known(Requirements::new()),
        }
    }
}
