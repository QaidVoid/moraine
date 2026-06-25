//! Gentoo dependency resolution and merge-order serialization.
//!
//! This crate teaches the generic [`moraine_solver`] solver Gentoo semantics. It
//! implements the solver's dependency-provider over the greenfield repository,
//! installed store, and configuration, encodes the full Gentoo dependency
//! grammar (the five dependency classes, USE conditionals and USE-dependency
//! atoms, REQUIRED_USE, `||` any-of groups, slot operators and bindings,
//! blockers, and virtuals) into solver requirements, and produces a
//! [`solution::ResolvedSolution`] describing what to install.
//!
//! The merge-order layer turns that solution into a deterministic ordered list
//! of merge and uninstall tasks, reproducing Portage's hardness ranking,
//! progressive cycle breaking, ASAP forcing, and blocker scheduling.

pub mod depnode;
pub mod encode;
pub mod error;
pub mod graph;
pub mod license;
pub mod normalize;
pub mod provider;
pub mod realsource;
pub mod required_use;
pub mod resolve;
pub mod serialize;
pub mod solution;
pub mod source;

pub use error::{MergeOrderError, ResidualCycle, ResolveError};
pub use graph::{EdgeFlags, MergeGraph, MergeNode, NodeKind};
pub use provider::GentooProvider;
pub use realsource::RealSource;
pub use resolve::resolve;
pub use serialize::{Task, TaskKind, serialize};
pub use solution::{
    AutounmaskChange, DepClass, DepEdge, RecordedBlocker, ResolvedPackage, ResolvedSolution, Root,
    SlotBinding,
};
pub use source::{AcceptChange, Acceptability, InstalledMeta, PackageMeta, ResolveSource};
