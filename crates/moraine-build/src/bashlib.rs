//! The vendored bash phase library boundary.
//!
//! The ebuild language is bash, and the entire eclass ecosystem is written
//! against the helper functions that `ebuild.sh`, `phase-functions.sh`, and
//! `phase-helpers.sh` expose. Reimplementing that surface in Rust would mean
//! tracking a moving PMS target, so the build engine instead vendors a bash
//! phase library and drives it: for each phase the driver forks a bash process,
//! exports the computed environment, sources the library and the ebuild (so the
//! ebuild's top-level `inherit` runs), folds the accumulated eclass metadata,
//! binds the EAPI default phase set, and invokes one phase function with its
//! pre/post hooks.
//!
//! # The boundary
//!
//! - **Rust owns**: environment computation (including `PORTAGE_ECLASS_LOCATIONS`,
//!   the resolved `USE`, `S`, `IUSE_EFFECTIVE`, and `REQUIRED_USE`), the
//!   build-tree layout, fetch, sandbox selection, the IPC manager, log and elog
//!   capture, and the phase schedule.
//! - **Bash owns**: `inherit`/`EXPORT_FUNCTIONS` and the `E_*` metadata fold, the
//!   EAPI phase defaults and the bare `default` command, and the helper surface
//!   (`econf`, `eapply`, `einstalldocs`, the `do*`/`new*` family, `use`/`use_with`/
//!   `usex`, `nonfatal`/`assert`/`die`, the `elog` family, and the IPC-backed
//!   `has_version`/`best_version`).
//!
//! # Where the library lives
//!
//! The library is shipped with the crate as the scripts under `src/bashlib/`
//! and embedded into the binary with [`include_str!`]. It is not discovered from
//! a host Portage install, so a build is reproducible and does not depend on
//! stock Portage being present. `eapi.sh` and `version-functions.sh` are
//! vendored verbatim from stock Portage; the rest are faithful ports of the
//! stock function bodies adapted to moraine's per-phase boundary, keeping the
//! two moraine patches: the `elog` family emits the `MORAINE_ELOG` tag the Rust
//! driver scans, and `has_version`/`best_version` call `${MORAINE_IPC_HELPER}`.

use std::path::{Path, PathBuf};

use crate::error::{IoExt as _, Result};

/// The EAPI predicate library (`___eapi_*`), vendored verbatim.
pub const EAPI: &str = include_str!("bashlib/eapi.sh");

/// The version-manipulation helpers (`ver_cut`/`ver_rs`/`ver_test`), vendored
/// verbatim.
pub const VERSION_FUNCTIONS: &str = include_str!("bashlib/version-functions.sh");

/// The low-level surface (`die`/`nonfatal`/`assert`, `has`/`contains_word`, the
/// `elog` family).
pub const ISOLATED_FUNCTIONS: &str = include_str!("bashlib/isolated-functions.sh");

/// The eclass machinery and phase dispatch (`inherit`/`EXPORT_FUNCTIONS`, the
/// `E_*` fold, `__ebuild_phase_funcs`, the hook dispatcher).
pub const PHASE_FUNCTIONS: &str = include_str!("bashlib/phase-functions.sh");

/// The helper surface (`econf`/`eapply`/`einstalldocs`/`use`/the `do*`/`new*`
/// family and the EAPI default phase implementations).
pub const PHASE_HELPERS: &str = include_str!("bashlib/phase-helpers.sh");

/// The metadata-fold function the driver calls after sourcing the ebuild.
pub const FOLD_FUNC: &str = "__fold_eclass_metadata";

/// The function that binds the EAPI default phase set and the bare `default`.
pub const BIND_FUNC: &str = "__ebuild_phase_funcs";

/// The hook-aware dispatcher the driver invokes per phase.
pub const DISPATCH_FUNC: &str = "__ebuild_phase_with_hooks";

/// The vendored library scripts in the order a phase process must source them.
const SCRIPT_ORDER: &[(&str, &str)] = &[
    ("eapi.sh", EAPI),
    ("version-functions.sh", VERSION_FUNCTIONS),
    ("isolated-functions.sh", ISOLATED_FUNCTIONS),
    ("phase-functions.sh", PHASE_FUNCTIONS),
    ("phase-helpers.sh", PHASE_HELPERS),
];

/// The materialized phase library on disk, ready to be sourced by a phase
/// process.
#[derive(Debug, Clone)]
pub struct PhaseLibrary {
    /// The scripts to source, in dependency order.
    pub scripts: Vec<PathBuf>,
}

impl PhaseLibrary {
    /// Materialize the vendored library into `dir`, returning the ordered script
    /// paths. The driver sources every script in order in each phase process.
    pub fn materialize(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).at(dir)?;
        let mut scripts = Vec::with_capacity(SCRIPT_ORDER.len());
        for (name, body) in SCRIPT_ORDER {
            let path = dir.join(name);
            moraine_common::fs::atomic_write(&path, body.as_bytes())?;
            scripts.push(path);
        }
        Ok(PhaseLibrary { scripts })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_embeds_machinery_and_helpers() {
        assert!(PHASE_FUNCTIONS.contains("inherit()"));
        assert!(PHASE_FUNCTIONS.contains("EXPORT_FUNCTIONS"));
        assert!(PHASE_FUNCTIONS.contains(DISPATCH_FUNC));
        assert!(PHASE_FUNCTIONS.contains(BIND_FUNC));
        assert!(PHASE_FUNCTIONS.contains(FOLD_FUNC));
        assert!(PHASE_HELPERS.contains("econf()"));
        assert!(PHASE_HELPERS.contains("eapply()"));
        assert!(PHASE_HELPERS.contains("einstalldocs"));
        assert!(PHASE_HELPERS.contains("has_version"));
        assert!(ISOLATED_FUNCTIONS.contains("MORAINE_ELOG"));
        assert!(EAPI.contains("___eapi_has_eapply"));
        assert!(VERSION_FUNCTIONS.contains("ver_cut"));
    }

    #[test]
    fn materialize_writes_every_script() {
        let dir = tempfile::tempdir().unwrap();
        let lib = PhaseLibrary::materialize(dir.path()).unwrap();
        assert_eq!(lib.scripts.len(), SCRIPT_ORDER.len());
        for script in &lib.scripts {
            assert!(script.is_file(), "{} not materialized", script.display());
        }
        // The eclass machinery is in the fourth script (phase-functions.sh).
        let body = std::fs::read_to_string(&lib.scripts[3]).unwrap();
        assert!(body.contains("inherit()"));
    }
}
