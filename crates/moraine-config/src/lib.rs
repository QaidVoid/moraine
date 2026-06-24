//! Gentoo configuration: make.conf, profile stacking, USE resolution, package
//! visibility, and package sets.
//!
//! This crate loads the configuration that drives dependency resolution and
//! exposes it as an immutable, queryable [`snapshot::ResolvedConfig`].

pub mod error;
pub mod makeconf;
pub mod profile;
pub mod sets;
pub mod snapshot;
pub mod stacking;
pub mod use_resolution;
pub mod visibility;

pub use error::ConfigError;
pub use makeconf::VarMap;
pub use profile::{ProfileNode, ProfileStack};
pub use snapshot::ResolvedConfig;
