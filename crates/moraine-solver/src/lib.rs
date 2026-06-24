//! A generic conflict-driven dependency solver (PubGrub / CDCL style).
//!
//! The solver carries no package-manager semantics. It operates over a
//! [`provider::DependencyProvider`] that supplies best-first candidate versions
//! and the dependencies of a chosen version. Version constraints are
//! [`range::Range`] values; the core does unit propagation, conflict resolution
//! by clause learning, and backjumping, and returns either a conflict-free
//! selection or a structured [`report::Failure`] explanation.

pub mod model;
pub mod provider;
pub mod range;
pub mod report;
pub mod solver;
pub mod term;

pub use provider::{Dependencies, DependencyProvider, MapProvider};
pub use range::Range;
pub use report::{Explanation, Failure, Solution};
pub use solver::solve;
pub use term::{Relation, Term};
