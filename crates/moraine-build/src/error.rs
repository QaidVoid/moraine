//! Typed errors for the build engine.
//!
//! Every fallible operation in this crate returns [`BuildError`]. Library code
//! never prints; callers render these variants as they see fit. The variants are
//! grouped by the build stage that produces them: environment computation,
//! source acquisition, phase execution, sandbox setup, and the ebuild IPC
//! channel.

use moraine_common::CommonError;

/// The phases of a source build, used to identify where a failure occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseKind {
    /// `pkg_pretend` (EAPI 4+).
    PkgPretend,
    /// `pkg_setup`.
    PkgSetup,
    /// `src_unpack`.
    SrcUnpack,
    /// `src_prepare` (EAPI 2+).
    SrcPrepare,
    /// `src_configure` (EAPI 2+).
    SrcConfigure,
    /// `src_compile`.
    SrcCompile,
    /// `src_test`.
    SrcTest,
    /// `src_install`.
    SrcInstall,
    /// `pkg_nofetch`, run when a fetch-restricted distfile is missing.
    PkgNofetch,
}

impl PhaseKind {
    /// The ebuild phase function name for this phase, for example `src_compile`.
    pub fn func_name(self) -> &'static str {
        match self {
            PhaseKind::PkgPretend => "pkg_pretend",
            PhaseKind::PkgSetup => "pkg_setup",
            PhaseKind::SrcUnpack => "src_unpack",
            PhaseKind::SrcPrepare => "src_prepare",
            PhaseKind::SrcConfigure => "src_configure",
            PhaseKind::SrcCompile => "src_compile",
            PhaseKind::SrcTest => "src_test",
            PhaseKind::SrcInstall => "src_install",
            PhaseKind::PkgNofetch => "pkg_nofetch",
        }
    }

    /// The short phase name used in `EBUILD_PHASE`, for example `compile`.
    pub fn short_name(self) -> &'static str {
        match self {
            PhaseKind::PkgPretend => "pretend",
            PhaseKind::PkgSetup => "setup",
            PhaseKind::SrcUnpack => "unpack",
            PhaseKind::SrcPrepare => "prepare",
            PhaseKind::SrcConfigure => "configure",
            PhaseKind::SrcCompile => "compile",
            PhaseKind::SrcTest => "test",
            PhaseKind::SrcInstall => "install",
            PhaseKind::PkgNofetch => "nofetch",
        }
    }
}

impl std::fmt::Display for PhaseKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.func_name())
    }
}

/// The error type for all `moraine-build` operations.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// The build environment could not be computed.
    #[error("build environment error: {reason}")]
    Environment {
        /// A short description of the problem.
        reason: String,
    },

    /// A `SRC_URI` value could not be parsed or reduced.
    #[error("invalid SRC_URI: {reason}")]
    SrcUri {
        /// A short description of the parse failure.
        reason: String,
    },

    /// A distfile could not be fetched from any configured source.
    #[error("failed to fetch distfile `{distfile}` after {attempts} attempt(s)")]
    Fetch {
        /// The destination distfile name.
        distfile: String,
        /// The number of fetch attempts made.
        attempts: u32,
    },

    /// A distfile is fetch-restricted and absent, requiring manual fetching.
    #[error("distfile `{distfile}` is fetch-restricted and missing; manual fetch required")]
    RestrictedFetch {
        /// The destination distfile name.
        distfile: String,
    },

    /// A distfile failed Manifest verification after all attempts.
    #[error("distfile `{distfile}` failed Manifest verification: {reason}")]
    Verification {
        /// The destination distfile name.
        distfile: String,
        /// Why verification failed.
        reason: String,
    },

    /// The Manifest lacked a DIST entry for a required distfile.
    #[error("no Manifest DIST entry for `{distfile}`")]
    MissingDigest {
        /// The distfile that has no digest.
        distfile: String,
    },

    /// A packaged file (`EBUILD`/`AUX`/`MISC`) failed Manifest verification
    /// before the ebuild was sourced.
    #[error("Manifest verification failed for `{name}`: {reason}")]
    ManifestMismatch {
        /// The `TYPE/name` of the entry that failed.
        name: String,
        /// Why verification failed.
        reason: String,
    },

    /// A phase process exited non-zero.
    #[error("phase {phase} failed with exit status {status}")]
    Phase {
        /// The phase that failed.
        phase: PhaseKind,
        /// The exit status reported by the runner.
        status: i32,
    },

    /// A phase could not be spawned at all.
    #[error("could not run phase {phase}: {reason}")]
    PhaseSpawn {
        /// The phase that could not be spawned.
        phase: PhaseKind,
        /// Why the spawn failed.
        reason: String,
    },

    /// The sandbox could not be set up as requested.
    #[error("sandbox setup failed: {reason}")]
    Sandbox {
        /// Why sandbox setup failed.
        reason: String,
    },

    /// The ebuild IPC channel failed.
    #[error("IPC error: {reason}")]
    Ipc {
        /// Why the IPC operation failed.
        reason: String,
    },

    /// The resolved USE violates the package's `REQUIRED_USE`.
    #[error("REQUIRED_USE not satisfied: {constraint}")]
    RequiredUse {
        /// The failing sub-constraint.
        constraint: String,
    },

    /// An underlying I/O or primitive operation failed.
    #[error(transparent)]
    Common(#[from] CommonError),
}

impl BuildError {
    /// Build an [`BuildError::Environment`] from any displayable reason.
    pub(crate) fn environment(reason: impl Into<String>) -> Self {
        BuildError::Environment {
            reason: reason.into(),
        }
    }

    /// Build a [`BuildError::SrcUri`] from any displayable reason.
    pub(crate) fn src_uri(reason: impl Into<String>) -> Self {
        BuildError::SrcUri {
            reason: reason.into(),
        }
    }
}

/// Attach a filesystem path to an [`std::io::Result`], converting the error into
/// a [`BuildError`] through [`CommonError`].
pub(crate) trait IoExt<T> {
    /// Convert an I/O error into [`BuildError::Common`] carrying `path`.
    fn at(self, path: impl AsRef<std::path::Path>) -> std::result::Result<T, BuildError>;
}

impl<T> IoExt<T> for std::io::Result<T> {
    fn at(self, path: impl AsRef<std::path::Path>) -> std::result::Result<T, BuildError> {
        self.map_err(|source| {
            BuildError::Common(CommonError::Io {
                path: path.as_ref().to_path_buf(),
                source,
            })
        })
    }
}

/// A convenience result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, BuildError>;
