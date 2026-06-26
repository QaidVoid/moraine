//! The Moraine build engine.
//!
//! `moraine-build` is the write-path build half of the Moraine package manager:
//! it takes one resolved package and produces an installable image directory plus
//! the `build-info` metadata the merge engine consumes, without merging anything
//! into the live filesystem. It computes the EAPI-gated build environment, lays
//! out the build tree, fetches and verifies distfiles, drives the EAPI phase
//! functions through a vendored bash phase library, sandboxes the build, answers
//! the ebuild IPC `has_version`/`best_version` queries, and captures the build
//! log and elog messages.
//!
//! # The bash boundary
//!
//! The engine does not reimplement the ebuild bash language. It vendors a bash
//! phase library (see [`bashlib`]) and drives it: for each phase it forks a bash
//! process, exports the environment, sources the library and the ebuild, and
//! invokes one phase function. Rust owns the schedule, environment, fetch,
//! sandbox, IPC, and logging.
//!
//! # Testability
//!
//! Every external process goes through the injectable [`runner::CommandRunner`]
//! so tests substitute a fake and assert environment construction, `SRC_URI`
//! mapping, phase ordering and `DEFINED_PHASES` skipping, `RESTRICT` handling,
//! and Manifest verification without running real builds, downloads, or
//! sandboxes. The orchestrator [`build_package`] takes plain inputs and the
//! runner by reference, so the full pipeline can be driven against fakes.

pub mod bashlib;
pub mod env;
pub mod error;
pub mod fetch;
pub mod ipc;
pub mod layout;
pub mod manifest;
pub mod metadata;
pub mod phase;
pub mod runner;
pub mod sandbox;
pub mod srcuri;

use std::collections::HashSet;
use std::path::PathBuf;

use tracing::instrument;

pub use env::{ConfigEnv, EnvBuilder, PackageIdent, PhaseEnv};
pub use error::{BuildError, PhaseKind, Result};
pub use fetch::{FetchConfig, FetchStatus, FetchedFile, Fetcher, RestrictFlags};
pub use ipc::{IpcHandler, Query, QueryRoot, Response as IpcResponse, VersionQuery};
pub use layout::BuildLayout;
pub use manifest::{Manifest, VerifyOutcome};
pub use metadata::BuildInfo;
pub use phase::{ElogLevel, ElogMessage, PhaseDriver, PhaseReport, PhaseRun};
pub use runner::{CommandOutput, CommandRunner, CommandSpec, SystemRunner};
pub use sandbox::{NamespaceSupport, SandboxPlan, SandboxSelector};
pub use srcuri::{DistFile, SrcUriMap};

/// The package metadata the build engine needs, resolved by the caller from
/// `moraine-repo` and `moraine-config`.
#[derive(Debug, Clone)]
pub struct PackageSpec {
    /// The package identity and EAPI.
    pub ident: PackageIdent,
    /// The path to the ebuild file in the repository.
    pub ebuild_path: PathBuf,
    /// The raw `SRC_URI` value.
    pub src_uri: String,
    /// The `DEFINED_PHASES` short-name tokens (`compile`, `install`, ...).
    pub defined_phases: Vec<String>,
    /// The `RESTRICT` tokens.
    pub restrict: Vec<String>,
    /// The package `SLOT` (the slot part only).
    pub slot: String,
    /// The package sub-slot, if the `SLOT` declared one (`slot/subslot`).
    pub subslot: Option<String>,
    /// The `IUSE` tokens.
    pub iuse: Vec<String>,
    /// The `KEYWORDS` tokens.
    pub keywords: Vec<String>,
    /// The transitively inherited eclasses (`INHERITED`).
    pub inherited: Vec<String>,
    /// The USE-conditional-reduced metadata values keyed by name (`DEPEND`,
    /// `RDEPEND`, `BDEPEND`, `LICENSE`, `PROPERTIES`, ...).
    pub reduced_meta: std::collections::BTreeMap<String, String>,
    /// The path to the repository `Manifest` for the package.
    pub manifest_path: PathBuf,
}

/// A build request: the package, the resolved configuration, the resolved USE
/// set, and the fetch configuration.
#[derive(Debug, Clone)]
pub struct BuildRequest {
    /// The package being built.
    pub package: PackageSpec,
    /// The resolved build environment configuration.
    pub config: ConfigEnv,
    /// The resolved USE flags for the package.
    pub use_flags: HashSet<String>,
    /// The fetch configuration.
    pub fetch: FetchConfig,
    /// Whether `src_test` runs (`FEATURES=test` and not `RESTRICT=test`).
    pub run_tests: bool,
    /// Whether a missing Manifest DIST entry is a hard error.
    pub require_digest: bool,
    /// The kernel namespace support for the sandbox plan.
    pub namespace_support: NamespaceSupport,
    /// The resolved `:=` slot bindings for this package, as
    /// `(dependency_cp, slot, subslot)`. Used to rewrite each `:=` dependency
    /// atom to its bound `:slot/subslot=` form before recording, so the stored
    /// `*DEPEND` carries the linked slot like Portage's
    /// `evaluate_slot_operator_equal_deps`.
    pub slot_bindings: Vec<(String, String, Option<String>)>,
}

/// The result of a successful build.
#[derive(Debug, Clone)]
pub struct BuildOutcome {
    /// The image directory `D` ready for the merge engine.
    pub image_dir: PathBuf,
    /// The `build-info` metadata directory.
    pub build_info_dir: PathBuf,
    /// The phase execution report (per-phase records and elog messages).
    pub report: PhaseReport,
    /// The distfiles that were fetched or found present.
    pub fetched: Vec<FetchedFile>,
    /// The union of FEATURES applied across all phases.
    pub applied_features: Vec<String>,
}

/// Build one resolved package, producing the image and build-info directories.
///
/// The `runner` is the injectable external-command surface; pass
/// [`SystemRunner`] in production or a fake in tests. The build stops at the
/// image and metadata; it does not merge them into the live filesystem.
#[instrument(name = "build_package", skip_all, fields(pf = %request.package.ident.pf))]
pub fn build_package<R: CommandRunner>(request: &BuildRequest, runner: &R) -> Result<BuildOutcome> {
    let pkg = &request.package;

    // 1. Build-tree layout.
    let build_root = request
        .config
        .vars
        .get("PORTAGE_TMPDIR")
        .cloned()
        .unwrap_or_else(|| request.fetch.distdir.to_string_lossy().to_string());
    let layout = BuildLayout::new(&build_root, &pkg.ident.category, &pkg.ident.pf)?;
    layout.create()?;

    // 2. Environment.
    let env = EnvBuilder::new(pkg.ident.clone(), request.config.clone(), &layout)?;

    // 3. SRC_URI mapping and fetch.
    let src_map =
        srcuri::parse_and_reduce(&pkg.src_uri, &request.use_flags, pkg.ident.eapi_features())?;
    let manifest = Manifest::read(&pkg.manifest_path)?;
    let fetcher = Fetcher::new(runner, &request.fetch, &manifest, request.require_digest);
    let restrict = RestrictFlags::from_tokens(pkg.restrict.iter().map(String::as_str));
    let fetched = fetcher.fetch_all(&src_map.a(), restrict)?;

    // 4. Phase library and sandbox.
    let library = bashlib::PhaseLibrary::materialize(layout.temp.join("bashlib"))?;
    let sandbox = SandboxSelector::from_config(
        &request.config,
        pkg.restrict.iter().map(String::as_str),
        request.namespace_support,
    );

    // 5. Drive phases.
    let driver = PhaseDriver::new(
        runner,
        &env,
        &layout,
        &library,
        &sandbox,
        &pkg.ebuild_path,
        pkg.defined_phases.clone(),
        request.run_tests,
        None,
    );
    let report = driver.run_all()?;

    // 6. Build-info metadata.
    let mut info = BuildInfo::default();
    info.set("CATEGORY", &pkg.ident.category);
    info.set("PF", &pkg.ident.pf);
    info.set("SLOT", &pkg.slot);
    info.set("EAPI", &pkg.ident.eapi);
    info.set("repository", &pkg.ident.repository);
    info.set("USE", sorted(&request.use_flags).join(" "));
    info.set("A", src_map.a_string());
    info.set_tokens("IUSE", pkg.iuse.iter().map(String::as_str));
    info.set_tokens(
        "DEFINED_PHASES",
        pkg.defined_phases.iter().map(String::as_str),
    );
    info.set_tokens("INHERITED", pkg.inherited.iter().map(String::as_str));
    info.set_tokens("KEYWORDS", pkg.keywords.iter().map(String::as_str));
    info.set_tokens("RESTRICT", pkg.restrict.iter().map(String::as_str));
    info.set("BUILD_TIME", build_time().to_string());
    for (k, v) in &pkg.reduced_meta {
        info.set(k.clone(), v.clone());
    }
    for key in ["CFLAGS", "CXXFLAGS", "LDFLAGS", "CHOST", "CBUILD"] {
        if let Some(v) = request.config.vars.get(key) {
            info.set(key, v.clone());
        }
    }
    info.write(&layout.build_info)?;
    metadata::copy_ebuild(&pkg.ebuild_path, &layout.build_info)?;
    metadata::write_saved_environment(&layout.build_info, env.base())?;

    let applied_features = union_features(&report);

    Ok(BuildOutcome {
        image_dir: layout.image.clone(),
        build_info_dir: layout.build_info.clone(),
        report,
        fetched,
        applied_features,
    })
}

/// The current UNIX time in seconds for `BUILD_TIME`, zero if the clock is before
/// the epoch.
fn build_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sorted(set: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.iter().cloned().collect();
    v.sort();
    v
}

fn union_features(report: &PhaseReport) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for run in &report.runs {
        for f in &run.applied_features {
            if !seen.contains(f) {
                seen.push(f.clone());
            }
        }
    }
    seen
}
