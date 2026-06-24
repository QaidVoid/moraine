//! Structured conflict explanations.
//!
//! When solving fails, the solver returns a derivation tree rooted at the
//! contradiction. Each node records the incompatibility that forced it and its
//! contributing causes. A sub-derivation reached more than once is emitted as a
//! [`Explanation::Shared`] reference so a consumer can render it once.

use crate::model::IncompatId;
use crate::term::Term;

/// A node in a failure explanation.
#[derive(Debug, Clone)]
pub enum Explanation<P, V> {
    /// An external fact (root request, a dependency, a missing version, or an
    /// unavailable version) that needs no further derivation.
    External {
        /// The incompatibility this node corresponds to.
        incompat: IncompatId,
        /// A human-readable description of the cause.
        description: String,
        /// The terms of the incompatibility.
        terms: Vec<(P, Term<V>)>,
    },
    /// A derived step with its contributing sub-derivations.
    Derived {
        /// The incompatibility this node corresponds to.
        incompat: IncompatId,
        /// The terms of the incompatibility.
        terms: Vec<(P, Term<V>)>,
        /// The causes that combine to produce this step.
        causes: Vec<Explanation<P, V>>,
    },
    /// A reference to a sub-derivation already shown elsewhere in the tree.
    Shared(IncompatId),
}

/// A solve failure carrying its root-cause explanation.
#[derive(Debug, Clone)]
pub struct Failure<P, V> {
    /// The structured derivation tree explaining the contradiction.
    pub explanation: Explanation<P, V>,
}

/// The outcome of a solve: a selected `package -> version` map, or a failure.
pub type Solution<P, V> = Result<std::collections::BTreeMap<P, V>, Failure<P, V>>;
