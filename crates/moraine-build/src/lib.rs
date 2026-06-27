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
pub mod depend;
pub mod elf;
pub mod env;
pub mod error;
pub mod fetch;
pub mod ipc;
pub mod isolation;
pub mod layout;
pub mod manifest;
pub mod metadata;
pub mod phase;
pub mod runner;
pub mod sandbox;
pub mod srcuri;
pub mod strip;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use tracing::instrument;

pub use depend::generate_metadata;
pub use elf::{NeededLine, SonameScan, scan_image_sonames};
pub use env::{ConfigEnv, EnvBuilder, PackageIdent, PhaseEnv};
pub use error::{BuildError, PhaseKind, Result};
pub use fetch::{
    CustomMirrors, FetchConfig, FetchStatus, FetchedFile, Fetcher, MirrorLayout, RestrictFlags,
};
pub use ipc::{IpcEndpoint, IpcHandler, Query, QueryRoot, Response as IpcResponse, VersionQuery};
pub use isolation::{Isolation, PrivilegeDrop};
pub use layout::BuildLayout;
pub use manifest::{Manifest, ManifestType, VerifyOutcome, verify_package};
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
    /// The full `IUSE_EFFECTIVE` set (IUSE plus implicit/forced/masked flags),
    /// exported so the strict `use()`/`in_iuse` checks can run. When empty the
    /// strict check is disabled (`PORTAGE_INTERNAL_CALLER` is not set), so an
    /// incomplete set cannot make `use()` die spuriously.
    pub iuse_effective: Vec<String>,
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
///
/// `version_query`, when `Some`, is the backend that answers the build-time
/// `has_version`/`best_version` queries: the engine starts an [`IpcEndpoint`]
/// responder over it for the lifetime of the phases and exports the client as
/// `MORAINE_IPC_HELPER`. Pass `None` only when no phase issues version queries
/// (the fake-runner unit tests); the real install path always passes a backend
/// so the bash helpers reach the responder instead of running an empty command.
#[instrument(name = "build_package", skip_all, fields(pf = %request.package.ident.pf))]
pub fn build_package<R: CommandRunner>(
    request: &BuildRequest,
    runner: &R,
    version_query: Option<&dyn VersionQuery>,
) -> Result<BuildOutcome> {
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

    // 2. Environment: the static EAPI-gated base plus the per-package values
    // computed outside the gates (USE, DEFINED_PHASES, IUSE_EFFECTIVE,
    // REQUIRED_USE). This also evaluates REQUIRED_USE against the resolved USE
    // before driving phases, guarding the direct build_package path (the resolver
    // guards the emerge path).
    let mut env = prepare_env(request, &layout)?;

    // 3. SRC_URI mapping and fetch.
    let src_map =
        srcuri::parse_and_reduce(&pkg.src_uri, &request.use_flags, pkg.ident.eapi_features())?;
    // A is the unpack list the default src_unpack and the S/WORKDIR fallback
    // read; export it into the phase environment.
    env.insert_base("A", src_map.a_string());
    let manifest = Manifest::read(&pkg.manifest_path)?;
    // Verify the ebuild and its `files/` aux entries against the Manifest before
    // the ebuild is sourced (the GLEP 74 integrity chain).
    if let Some(pkg_dir) = pkg.ebuild_path.parent() {
        let ebuild_name = pkg
            .ebuild_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        crate::manifest::verify_package(
            &manifest,
            pkg_dir,
            ebuild_name,
            &request.fetch.required_hashes,
            false,
        )?;
    }
    // 4. Phase library and sandbox (built before fetch so pkg_nofetch can run on
    // a fetch-restricted package whose distfiles are missing).
    let library = bashlib::PhaseLibrary::materialize(layout.temp.join("bashlib"))?;
    let sandbox = SandboxSelector::from_config(
        &request.config,
        pkg.restrict.iter().map(String::as_str),
        request.namespace_support,
    );
    // PROPERTIES drives the network exemption (`live` unpack, `test_network`
    // test); it comes from the USE-conditional-reduced metadata.
    let properties: Vec<String> = pkg
        .reduced_meta
        .get("PROPERTIES")
        .map(|p| p.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    // When a version-query backend is present, lay out the IPC endpoint under
    // `.ipc` and export its client into the phases as `MORAINE_IPC_HELPER`; the
    // responder is started around `run_all` below.
    let endpoint = match version_query {
        Some(_) => Some(ipc::IpcEndpoint::create(&layout.ipc)?),
        None => None,
    };
    let ipc_helper = endpoint.as_ref().map(|e| e.helper_path().to_path_buf());
    let driver = PhaseDriver::new(
        runner,
        &env,
        &layout,
        &library,
        &sandbox,
        &pkg.ebuild_path,
        pkg.defined_phases.clone(),
        request.run_tests,
        properties,
        ipc_helper,
    );

    let fetcher = Fetcher::new(runner, &request.fetch, &manifest, request.require_digest);
    let restrict = RestrictFlags::from_tokens(pkg.restrict.iter().map(String::as_str));
    let fetched = match fetcher.fetch_all(&src_map.a(), restrict) {
        Ok(fetched) => fetched,
        Err(BuildError::RestrictedFetch { distfile }) => {
            // Emit the standard fetch-restriction notice and run pkg_nofetch for
            // the ebuild's manual-download instructions before aborting.
            tracing::warn!(
                "Fetch failed for {}/{}: {distfile} requires manual download (RESTRICT=fetch)",
                pkg.ident.category,
                pkg.ident.pf
            );
            let _ = driver.run_nofetch();
            return Err(BuildError::RestrictedFetch { distfile });
        }
        Err(e) => return Err(e),
    };

    // 4b. Derive INHERITED/INHERIT provenance from the eclasses actually
    // sourced, falling back to the cache token when the generator yields
    // nothing (for example under a fake runner in tests).
    let generated = depend::generate_metadata(runner, &library, &pkg.ebuild_path, env.base())
        .unwrap_or_default();

    // 5. Drive phases. When a version-query backend is present, run the IPC
    // responder on a scoped thread for the lifetime of `run_all` so a phase's
    // `has_version`/`best_version` call is answered from the live store; the
    // responder is signaled to stop on every exit path so the scope can join it.
    let report = match (endpoint.as_ref(), version_query) {
        (Some(ep), Some(backend)) => std::thread::scope(|scope| {
            scope.spawn(|| ep.serve(backend));
            let result = driver.run_all();
            ep.shutdown();
            result
        }),
        _ => driver.run_all(),
    }?;

    // 5b. Strip ELF objects in the image, gated on nostrip/RESTRICT=strip and
    // honoring the dostrip include/exclude lists recorded during src_install.
    let dostrip = strip::parse_bash_string_array(
        report
            .final_env
            .get("PORTAGE_DOSTRIP")
            .map(String::as_str)
            .unwrap_or_default(),
    );
    let dostrip_skip = strip::parse_bash_string_array(
        report
            .final_env
            .get("PORTAGE_DOSTRIP_SKIP")
            .map(String::as_str)
            .unwrap_or_default(),
    );
    let strip_mask = report
        .final_env
        .get("STRIP_MASK")
        .map(String::as_str)
        .unwrap_or_default();
    strip::strip_image(
        &layout.image,
        &request.config,
        &pkg.restrict,
        &dostrip,
        &dostrip_skip,
        strip_mask,
        &layout.workdir,
        &pkg.ident.category,
        &pkg.ident.pf,
        runner,
    );

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
    // INHERITED comes from the eclasses actually sourced when available,
    // otherwise from the cache token the caller passed.
    match generated.get("INHERITED").map(|s| s.trim()) {
        Some(inherited) if !inherited.is_empty() => {
            info.set("INHERITED", inherited);
            if let Some(inherit) = generated.get("INHERIT").map(|s| s.trim())
                && !inherit.is_empty()
            {
                info.set("INHERIT", inherit);
            }
        }
        _ => info.set_tokens("INHERITED", pkg.inherited.iter().map(String::as_str)),
    }
    info.set_tokens("KEYWORDS", pkg.keywords.iter().map(String::as_str));
    info.set_tokens("RESTRICT", pkg.restrict.iter().map(String::as_str));
    info.set("BUILD_TIME", build_time().to_string());
    for (k, v) in &pkg.reduced_meta {
        info.set(k.clone(), v.clone());
    }
    for key in [
        "CFLAGS", "CXXFLAGS", "LDFLAGS", "CHOST", "CBUILD", "CC", "CXX", "CTARGET", "ASFLAGS",
    ] {
        if let Some(v) = request.config.vars.get(key) {
            info.set(key, v.clone());
        }
    }
    info.write(&layout.build_info)?;
    metadata::copy_ebuild(&pkg.ebuild_path, &layout.build_info)?;
    // The saved environment is the post-src_install ebuild environment (the
    // mutated final phase env), filtered for readonly metadata and shell-internal
    // variables, mirroring `__save_ebuild_env --exclude-init-phases |
    // __filter_readonly_variables`. Carrying the static base would drop any
    // variable an ebuild or eclass set during a phase.
    let saved_env = env::filter_saved_env(&BTreeMap::new(), &report.final_env);
    metadata::write_saved_environment(&layout.build_info, &saved_env)?;

    let applied_features = union_features(&report);

    Ok(BuildOutcome {
        image_dir: layout.image.clone(),
        build_info_dir: layout.build_info.clone(),
        report,
        fetched,
        applied_features,
    })
}

/// Run `pkg_pretend` for one package upfront, before any fetch, build, or merge.
///
/// Portage's Scheduler validates `pkg_pretend` once for the whole mergelist
/// before the merge loop, so a failing pretend aborts the transaction before
/// anything is fetched, built, or merged. This lays out the environment and
/// drives only the `pkg_pretend` phase through [`PhaseDriver::run_pretend`],
/// reusing the sandbox plan (now unsandboxed and networked for pretend). A
/// package whose EAPI does not define `pkg_pretend`, or that does not list
/// `pretend` in `DEFINED_PHASES`, runs nothing. `version_query`, when `Some`,
/// answers the build-time `has_version`/`best_version` queries the same way
/// [`build_package`] does, since a `pkg_pretend` often probes installed versions.
#[instrument(name = "pretend_package", skip_all, fields(pf = %request.package.ident.pf))]
pub fn pretend_package<R: CommandRunner>(
    request: &BuildRequest,
    runner: &R,
    version_query: Option<&dyn VersionQuery>,
) -> Result<()> {
    let pkg = &request.package;

    let build_root = request
        .config
        .vars
        .get("PORTAGE_TMPDIR")
        .cloned()
        .unwrap_or_else(|| request.fetch.distdir.to_string_lossy().to_string());
    let layout = BuildLayout::new(&build_root, &pkg.ident.category, &pkg.ident.pf)?;
    layout.create()?;

    let env = prepare_env(request, &layout)?;
    let library = bashlib::PhaseLibrary::materialize(layout.temp.join("bashlib"))?;
    let sandbox = SandboxSelector::from_config(
        &request.config,
        pkg.restrict.iter().map(String::as_str),
        request.namespace_support,
    );
    let properties: Vec<String> = pkg
        .reduced_meta
        .get("PROPERTIES")
        .map(|p| p.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let endpoint = match version_query {
        Some(_) => Some(ipc::IpcEndpoint::create(&layout.ipc)?),
        None => None,
    };
    let ipc_helper = endpoint.as_ref().map(|e| e.helper_path().to_path_buf());
    let driver = PhaseDriver::new(
        runner,
        &env,
        &layout,
        &library,
        &sandbox,
        &pkg.ebuild_path,
        pkg.defined_phases.clone(),
        request.run_tests,
        properties,
        ipc_helper,
    );

    match (endpoint.as_ref(), version_query) {
        (Some(ep), Some(backend)) => std::thread::scope(|scope| {
            scope.spawn(|| ep.serve(backend));
            let result = driver.run_pretend();
            ep.shutdown();
            result
        }),
        _ => driver.run_pretend(),
    }?;
    Ok(())
}

/// Lay out the EAPI-gated build environment for a package: the static base plus
/// the per-package values computed outside the EAPI gates (`USE`,
/// `DEFINED_PHASES`, `IUSE_EFFECTIVE` with the strict-check opt-in, and
/// `REQUIRED_USE`). Shared by [`build_package`] and [`pretend_package`]. Returns
/// an error when the resolved USE violates the package's `REQUIRED_USE`.
fn prepare_env(request: &BuildRequest, layout: &BuildLayout) -> Result<EnvBuilder> {
    let pkg = &request.package;
    let mut env = EnvBuilder::new(pkg.ident.clone(), request.config.clone(), layout)?;
    if let Some(required_use) = pkg.reduced_meta.get("REQUIRED_USE") {
        check_required_use(required_use, &request.use_flags)?;
        env.insert_base("REQUIRED_USE", required_use.clone());
    }
    env.insert_base("USE", sorted(&request.use_flags).join(" "));
    env.insert_base("DEFINED_PHASES", defined_phases_token(&pkg.defined_phases));
    if request.iuse_effective.is_empty() {
        env.insert_base("IUSE_EFFECTIVE", pkg.iuse.join(" "));
    } else {
        env.insert_base("IUSE_EFFECTIVE", request.iuse_effective.join(" "));
        env.insert_base("PORTAGE_INTERNAL_CALLER", "1");
    }
    Ok(env)
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

/// The `DEFINED_PHASES` token, the literal `-` when no phases are defined,
/// matching the stock `ebuild.sh` representation.
fn defined_phases_token(phases: &[String]) -> String {
    if phases.is_empty() {
        "-".to_string()
    } else {
        phases.join(" ")
    }
}

/// Evaluate a package's `REQUIRED_USE` against the resolved USE, returning an
/// error naming the failing sub-constraint on a violation.
fn check_required_use(required_use: &str, use_flags: &HashSet<String>) -> Result<()> {
    if required_use.trim().is_empty() {
        return Ok(());
    }
    let node = moraine_resolve::required_use::parse_required_use(required_use);
    let use_set: std::collections::BTreeSet<String> = use_flags.iter().cloned().collect();
    match moraine_resolve::required_use::evaluate_required_use(&node, &use_set) {
        moraine_resolve::required_use::RequiredUseOutcome::Satisfied => Ok(()),
        moraine_resolve::required_use::RequiredUseOutcome::Violated(constraint) => {
            Err(BuildError::RequiredUse { constraint })
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn use_set<'a>(flags: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
        flags.into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn defined_phases_token_uses_dash_when_empty() {
        assert_eq!(defined_phases_token(&[]), "-");
        assert_eq!(
            defined_phases_token(&["compile".into(), "install".into()]),
            "compile install"
        );
    }

    #[test]
    fn required_use_satisfied_and_violated() {
        // `^^ ( ssl gnutls )` is satisfied by exactly one of the two.
        let constraint = "^^ ( ssl gnutls )";
        assert!(check_required_use(constraint, &use_set(["ssl"])).is_ok());
        let err = check_required_use(constraint, &use_set(["ssl", "gnutls"]));
        assert!(matches!(err, Err(BuildError::RequiredUse { .. })));
        let err = check_required_use(constraint, &use_set([]));
        assert!(matches!(err, Err(BuildError::RequiredUse { .. })));
    }

    #[test]
    fn required_use_conditional() {
        // `python? ( ssl )` requires ssl only when python is enabled.
        let constraint = "python? ( ssl )";
        assert!(check_required_use(constraint, &use_set(["python", "ssl"])).is_ok());
        assert!(check_required_use(constraint, &use_set(["other"])).is_ok());
        assert!(check_required_use(constraint, &use_set(["python"])).is_err());
    }

    #[test]
    fn required_use_empty_is_ok() {
        assert!(check_required_use("", &use_set([])).is_ok());
        assert!(check_required_use("   ", &use_set([])).is_ok());
    }

    #[test]
    fn saved_environment_reflects_post_install_state() {
        // Drives a real bash through the build engine; skip cleanly when bash is
        // unavailable.
        if std::process::Command::new("bash")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let distdir = tmp.path().join("distdir");
        let buildroot = tmp.path().join("buildroot");
        let repo = tmp.path().join("repo/dev-libs/fixture");
        std::fs::create_dir_all(&distdir).unwrap();
        std::fs::create_dir_all(&repo).unwrap();

        // An ebuild whose src_install sets a variable. S points at WORKDIR so the
        // source phases can cd into an existing directory without a real tarball.
        let ebuild = repo.join("fixture-1.0.ebuild");
        std::fs::write(
            &ebuild,
            "EAPI=8\nS=\"${WORKDIR}\"\nsrc_install() { export MY_INSTALL_VAR=hello; }\n",
        )
        .unwrap();
        std::fs::write(repo.join("Manifest"), "").unwrap();

        let mut vars = BTreeMap::new();
        vars.insert(
            "PORTAGE_TMPDIR".to_string(),
            buildroot.to_string_lossy().to_string(),
        );

        let ident = PackageIdent {
            category: "dev-libs".into(),
            pf: "fixture-1.0".into(),
            p: "fixture-1.0".into(),
            pn: "fixture".into(),
            pv: "1.0".into(),
            pvr: "1.0".into(),
            pr: "r0".into(),
            eapi: "8".into(),
            repository: "test".into(),
        };
        let package = PackageSpec {
            ident,
            ebuild_path: ebuild,
            src_uri: String::new(),
            defined_phases: vec!["install".into()],
            restrict: vec![],
            slot: "0".into(),
            subslot: None,
            iuse: vec![],
            keywords: vec![],
            inherited: vec![],
            reduced_meta: BTreeMap::new(),
            manifest_path: repo.join("Manifest"),
        };
        let request = BuildRequest {
            package,
            // No sandbox/fakeroot features so the phases run as plain bash with no
            // external isolation binaries.
            config: ConfigEnv {
                vars,
                ..ConfigEnv::rooted([])
            },
            use_flags: HashSet::new(),
            iuse_effective: vec![],
            fetch: FetchConfig::new(&distdir),
            run_tests: false,
            require_digest: false,
            namespace_support: NamespaceSupport::default(),
            slot_bindings: Vec::new(),
        };

        let runner = SystemRunner::new();
        let outcome = build_package(&request, &runner, None).unwrap();

        // The compressed build-info environment carries the variable src_install
        // set, while readonly metadata like EAPI is filtered back out.
        use std::io::Read as _;
        let compressed = std::fs::read(outcome.build_info_dir.join("environment.bz2")).unwrap();
        let mut decoder = bzip2::read::BzDecoder::new(&compressed[..]);
        let mut body = String::new();
        decoder.read_to_string(&mut body).unwrap();
        assert!(
            body.contains("MY_INSTALL_VAR") && body.contains("hello"),
            "post-install variable missing from saved environment:\n{body}"
        );
        assert!(
            !body.contains("declare -x EAPI="),
            "readonly metadata must be filtered from the saved environment:\n{body}"
        );
    }
}
