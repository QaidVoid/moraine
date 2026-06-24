//! The vendored bash phase library boundary.
//!
//! The ebuild language is bash, and the entire eclass ecosystem is written
//! against the helper functions that `ebuild.sh`, `phase-functions.sh`, and
//! `phase-helpers.sh` expose. Reimplementing that surface in Rust would mean
//! tracking a moving PMS target, so the build engine instead vendors a bash
//! phase library and drives it: for each phase the driver forks a bash process,
//! exports the computed environment, sources the ebuild and this library, and
//! invokes a single phase function through the `__ebuild_phase` dispatcher.
//!
//! # The boundary
//!
//! - **Rust owns**: environment computation, the build-tree layout, fetch,
//!   sandbox selection, the IPC manager, log and elog capture, and the phase
//!   schedule (order, `DEFINED_PHASES` skipping, per-phase forking).
//! - **Bash owns**: the EAPI phase defaults (`default_src_*`) and the helper
//!   surface (`econf`, `emake`, `unpack`, `use`, the `do*`/`new*` family,
//!   `eapply`, the `elog` family, and the IPC-backed `has_version`/
//!   `best_version`).
//!
//! # Where the library lives
//!
//! The library is shipped with the crate as the two scripts under
//! `src/bashlib/` and embedded into the binary with [`include_str!`]. It is not
//! discovered from a host Portage install, so a build is reproducible and does
//! not depend on stock Portage being present. The vendored copies here are a
//! thin, auditable subset documenting the contract; a production deployment
//! replaces them with the full lightly-patched fork of the stock `bin/*.sh` set,
//! selected by EAPI through the same `___eapi_*` predicate mechanism the scripts
//! already use.

use std::path::{Path, PathBuf};

use crate::error::{IoExt as _, Result};

/// The vendored phase-function library (defaults and the `__ebuild_phase`
/// dispatcher).
pub const PHASE_FUNCTIONS: &str = include_str!("bashlib/phase-functions.sh");

/// The vendored phase-helper library (the `econf`/`emake`/`unpack`/`use`/`do*`
/// helper surface and the IPC-backed version queries).
pub const PHASE_HELPERS: &str = include_str!("bashlib/phase-helpers.sh");

/// The name of the dispatcher function the driver invokes per phase.
pub const DISPATCH_FUNC: &str = "__ebuild_phase";

/// The materialized phase library on disk, ready to be sourced by a phase
/// process.
#[derive(Debug, Clone)]
pub struct PhaseLibrary {
    /// The phase-functions script path.
    pub functions: PathBuf,
    /// The phase-helpers script path.
    pub helpers: PathBuf,
}

impl PhaseLibrary {
    /// Materialize the vendored library into `dir`, returning the script paths.
    /// The driver sources both scripts in each phase process.
    pub fn materialize(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).at(dir)?;
        let functions = dir.join("phase-functions.sh");
        let helpers = dir.join("phase-helpers.sh");
        moraine_common::fs::atomic_write(&functions, PHASE_FUNCTIONS.as_bytes())?;
        moraine_common::fs::atomic_write(&helpers, PHASE_HELPERS.as_bytes())?;
        Ok(PhaseLibrary { functions, helpers })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_embeds_dispatcher_and_helpers() {
        assert!(PHASE_FUNCTIONS.contains(DISPATCH_FUNC));
        assert!(PHASE_HELPERS.contains("econf"));
        assert!(PHASE_HELPERS.contains("has_version"));
        assert!(PHASE_FUNCTIONS.contains("default_src_compile"));
    }

    #[test]
    fn materialize_writes_both_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let lib = PhaseLibrary::materialize(dir.path()).unwrap();
        assert!(lib.functions.is_file());
        assert!(lib.helpers.is_file());
        let body = std::fs::read_to_string(&lib.functions).unwrap();
        assert!(body.contains(DISPATCH_FUNC));
    }
}
