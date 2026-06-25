//! Gentoo configuration: make.conf, profile stacking, USE resolution, package
//! visibility, and package sets.
//!
//! This crate loads the configuration that drives dependency resolution and
//! exposes it as an immutable, queryable [`snapshot::ResolvedConfig`].

pub mod error;
pub mod keywords;
pub mod license;
pub mod loader;
pub mod makeconf;
pub mod profile;
pub mod sets;
pub mod snapshot;
pub mod stacking;
pub mod use_resolution;
pub mod visibility;

pub use error::ConfigError;
pub use keywords::KeywordsManager;
pub use license::{LicenseManager, LicenseReq};
pub use loader::{RepoMaskInput, resolve_config};
pub use makeconf::VarMap;
pub use profile::{ProfileNode, ProfileStack};
pub use snapshot::{ResolvedConfig, Visibility};
pub use visibility::MaskReason;
