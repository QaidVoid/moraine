//! Error types for configuration loading.

use std::path::PathBuf;

/// An error produced while loading or resolving configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A `make.conf` (or `make.defaults`) file failed to parse.
    #[error("failed to parse `{path}`: {reason}")]
    MakeConf {
        /// The offending file.
        path: PathBuf,
        /// What went wrong.
        reason: &'static str,
    },

    /// A profile `parent` reference could not be resolved.
    #[error("invalid profile parent `{reference}` in `{path}`: {reason}")]
    ProfileParent {
        /// The file containing the reference.
        path: PathBuf,
        /// The unresolved reference.
        reference: String,
        /// Why it could not be resolved.
        reason: &'static str,
    },

    /// A profile declared an unsupported EAPI.
    #[error("profile `{path}` declares unsupported EAPI `{eapi}`")]
    UnsupportedEapi {
        /// The profile node.
        path: PathBuf,
        /// The declared EAPI string.
        eapi: String,
    },

    /// A referenced package set is unknown.
    #[error("unknown package set `@{name}`")]
    UnknownSet {
        /// The set name without the leading `@`.
        name: String,
    },

    /// An I/O error while reading a configuration path.
    #[error("I/O error reading `{path}`")]
    Io {
        /// The path being read.
        path: PathBuf,
    },
}
