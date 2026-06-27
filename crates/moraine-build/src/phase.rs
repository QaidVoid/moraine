//! The EAPI phase driver.
//!
//! Runs the source-build phases in EAPI order, skipping phases the active EAPI
//! does not define and phases absent from `DEFINED_PHASES` that have no default
//! work, and forking a bash process per phase that sources the vendored phase
//! library and invokes one phase function through the [`bashlib`] dispatcher.
//! Cross-phase state is carried through the filtered saved environment from
//! [`crate::env`]. The driver captures the build log and the elog messages each
//! phase emits.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tracing::{info, instrument};

use crate::bashlib::{self, PhaseLibrary};
use crate::env::{EnvBuilder, filter_saved_env};
use crate::error::{BuildError, PhaseKind, Result};
use crate::isolation::{Isolation, PrivilegeDrop, resolve_build_user};
use crate::layout::BuildLayout;
use crate::runner::{CommandOutput, CommandRunner, CommandSpec};
use crate::sandbox::{PrivilegeMode, SandboxPlan, SandboxSelector};

/// The severity of a captured elog message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElogLevel {
    /// `einfo`.
    Info,
    /// `elog`.
    Log,
    /// `ewarn`.
    Warn,
    /// `eerror`.
    Error,
    /// `eqawarn`.
    Qa,
}

impl ElogLevel {
    fn parse(tag: &str) -> Option<Self> {
        match tag {
            "INFO" => Some(ElogLevel::Info),
            "LOG" => Some(ElogLevel::Log),
            "WARN" => Some(ElogLevel::Warn),
            "ERROR" => Some(ElogLevel::Error),
            "QA" => Some(ElogLevel::Qa),
            _ => None,
        }
    }
}

/// One captured elog message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElogMessage {
    /// The message severity.
    pub level: ElogLevel,
    /// The phase that emitted it.
    pub phase: String,
    /// The message text.
    pub text: String,
}

/// The record of running one phase.
#[derive(Debug, Clone)]
pub struct PhaseRun {
    /// The phase.
    pub phase: PhaseKind,
    /// Whether the phase was actually invoked (false when skipped).
    pub invoked: bool,
    /// The exit status, when invoked.
    pub status: Option<i32>,
    /// The FEATURES the sandbox plan applied for this phase.
    pub applied_features: Vec<String>,
}

/// The accumulated outcome of driving all phases.
#[derive(Debug, Clone, Default)]
pub struct PhaseReport {
    /// Per-phase records, in execution order.
    pub runs: Vec<PhaseRun>,
    /// All captured elog messages, in order.
    pub elog: Vec<ElogMessage>,
}

impl PhaseReport {
    /// The phases that were actually invoked, in order.
    pub fn invoked_phases(&self) -> Vec<PhaseKind> {
        self.runs
            .iter()
            .filter(|r| r.invoked)
            .map(|r| r.phase)
            .collect()
    }
}

/// The full ordered phase set; each is gated by the EAPI and `DEFINED_PHASES`.
const PHASE_ORDER: &[PhaseKind] = &[
    PhaseKind::PkgPretend,
    PhaseKind::PkgSetup,
    PhaseKind::SrcUnpack,
    PhaseKind::SrcPrepare,
    PhaseKind::SrcConfigure,
    PhaseKind::SrcCompile,
    PhaseKind::SrcTest,
    PhaseKind::SrcInstall,
];

/// Drives the phase schedule for one build.
pub struct PhaseDriver<'a, R: CommandRunner> {
    runner: &'a R,
    env: &'a EnvBuilder,
    layout: &'a BuildLayout,
    library: &'a PhaseLibrary,
    sandbox: &'a SandboxSelector,
    ebuild_path: PathBuf,
    defined_phases: Vec<String>,
    run_tests: bool,
    properties: Vec<String>,
    ipc_helper: Option<PathBuf>,
    userpriv_target: Option<PrivilegeDrop>,
}

impl<'a, R: CommandRunner> PhaseDriver<'a, R> {
    /// Construct a phase driver.
    ///
    /// `defined_phases` is the package's `DEFINED_PHASES` token list (short
    /// names like `compile`). `run_tests` controls whether `src_test` runs.
    /// `properties` is the package's `PROPERTIES` token list, consulted for the
    /// network exemption (`live` unpack, `test_network` test). `ipc_helper`, when
    /// set, is the path the bash helper invokes for version queries and is
    /// exported as `MORAINE_IPC_HELPER`.
    ///
    /// The build-user privilege drop target for `userpriv` is resolved here, in
    /// the parent, only when the engine runs as root: the configured
    /// `PORTAGE_USERNAME`/`PORTAGE_GRPNAME` (default `portage`) are looked up in
    /// the local `/etc/passwd` and `/etc/group`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runner: &'a R,
        env: &'a EnvBuilder,
        layout: &'a BuildLayout,
        library: &'a PhaseLibrary,
        sandbox: &'a SandboxSelector,
        ebuild_path: impl Into<PathBuf>,
        defined_phases: Vec<String>,
        run_tests: bool,
        properties: Vec<String>,
        ipc_helper: Option<PathBuf>,
    ) -> Self {
        let userpriv_target = resolve_userpriv_target(env);
        PhaseDriver {
            runner,
            env,
            layout,
            library,
            sandbox,
            ebuild_path: ebuild_path.into(),
            defined_phases,
            run_tests,
            properties,
            ipc_helper,
            userpriv_target,
        }
    }

    /// Whether a phase is defined by the active EAPI.
    fn phase_defined_by_eapi(&self, phase: PhaseKind) -> bool {
        let f = self.env.features();
        match phase {
            // pkg_pretend is EAPI 4+; the required_use gate matches its
            // introduction in the stock feature table.
            PhaseKind::PkgPretend => f.required_use,
            // src_prepare and src_configure are EAPI 2+; use_deps gates at 2.
            PhaseKind::SrcPrepare | PhaseKind::SrcConfigure => f.use_deps,
            // pkg_nofetch is not part of the normal phase order; it is run on
            // demand by the fetch-restricted path via `run_nofetch`.
            PhaseKind::PkgNofetch => true,
            _ => true,
        }
    }

    /// Whether the phase should run at all, considering EAPI definition,
    /// `DEFINED_PHASES`, and the test gate.
    fn should_run(&self, phase: PhaseKind) -> bool {
        if !self.phase_defined_by_eapi(phase) {
            return false;
        }
        if phase == PhaseKind::SrcTest && !self.run_tests {
            return false;
        }
        // A phase runs if the ebuild defines it (it is in DEFINED_PHASES) or if
        // the phase has default work even when not listed. The phases that have
        // meaningful default work for an unlisted phase are unpack, configure,
        // compile, and install. pkg_setup and pkg_pretend with no override are
        // no-ops and can be skipped when not listed.
        let short = phase.short_name();
        if self.defined_phases.iter().any(|p| p == short) {
            return true;
        }
        // Run the EAPI default for every source phase even when the ebuild does
        // not define it: src_prepare's default applies PATCHES and runs
        // eapply_user (EAPI 6+), src_test's default runs the make check/test
        // target, and the rest unpack/configure/compile/install. The pkg_*
        // phases have no-op defaults when unlisted, so they stay skipped.
        matches!(
            phase,
            PhaseKind::SrcUnpack
                | PhaseKind::SrcPrepare
                | PhaseKind::SrcConfigure
                | PhaseKind::SrcCompile
                | PhaseKind::SrcTest
                | PhaseKind::SrcInstall
        )
    }

    /// Whether a phase needs network access (so the network sandbox exempts it).
    ///
    /// Mirrors `doebuild`: `src_unpack` is exempt only when `PROPERTIES`
    /// contains `live`, `src_test` only when `PROPERTIES` contains
    /// `test_network`, and every other phase is isolated.
    fn network_needed(&self, phase: PhaseKind) -> bool {
        match phase {
            PhaseKind::SrcUnpack => self.properties.iter().any(|p| p == "live"),
            PhaseKind::SrcTest => self.properties.iter().any(|p| p == "test_network"),
            _ => false,
        }
    }

    /// Run all phases in order, carrying state forward and capturing logs.
    #[instrument(name = "drive_phases", skip(self))]
    pub fn run_all(&self) -> Result<PhaseReport> {
        let mut report = PhaseReport::default();
        let mut carried: BTreeMap<String, String> = BTreeMap::new();

        for &phase in PHASE_ORDER {
            if !self.should_run(phase) {
                report.runs.push(PhaseRun {
                    phase,
                    invoked: false,
                    status: None,
                    applied_features: Vec::new(),
                });
                continue;
            }
            let (run, new_carried) = self.run_phase(phase, &carried, &mut report.elog)?;
            carried = new_carried;
            report.runs.push(run);
        }
        Ok(report)
    }

    /// Run the `pkg_nofetch` phase on demand for a fetch-restricted package whose
    /// distfiles are missing, so the ebuild can print its manual-download
    /// instructions. Returns the phase's elog messages.
    pub fn run_nofetch(&self) -> Result<Vec<ElogMessage>> {
        let mut elog = Vec::new();
        let (_, _) = self.run_phase(PhaseKind::PkgNofetch, &BTreeMap::new(), &mut elog)?;
        Ok(elog)
    }

    /// Run a single phase, returning its record and the updated carried env.
    #[instrument(name = "phase", skip(self, carried, elog), fields(phase = %phase))]
    fn run_phase(
        &self,
        phase: PhaseKind,
        carried: &BTreeMap<String, String>,
        elog: &mut Vec<ElogMessage>,
    ) -> Result<(PhaseRun, BTreeMap<String, String>)> {
        let plan = self
            .sandbox
            .plan(phase, self.layout.build_root(), self.network_needed(phase));

        let spec = self.build_command(phase, carried, &plan)?;
        let output = self.runner.run(&spec).map_err(|e| BuildError::PhaseSpawn {
            phase,
            reason: e.reason,
        })?;

        self.capture_elog(phase, &output, elog)?;

        if !output.success() {
            return Err(BuildError::Phase {
                phase,
                status: output.status,
            });
        }

        let new_carried = self.read_saved_env(carried)?;
        info!(phase = %phase, "phase completed");
        // Report the wrapper-level features from the plan merged with the
        // isolation the runner actually enforced, so `applied_features` reflects
        // only what was applied.
        let mut applied_features = plan.applied_features.clone();
        for token in &output.applied_isolation {
            if !applied_features.contains(token) {
                applied_features.push(token.clone());
            }
        }
        Ok((
            PhaseRun {
                phase,
                invoked: true,
                status: Some(output.status),
                applied_features,
            },
            new_carried,
        ))
    }

    /// Build the bash command for a phase: the sandbox wrapper, then `bash -c`
    /// sourcing the library, the ebuild, and the carried env, then dispatching the
    /// phase function and saving the resulting env.
    fn build_command(
        &self,
        phase: PhaseKind,
        carried: &BTreeMap<String, String>,
        plan: &SandboxPlan,
    ) -> Result<CommandSpec> {
        let phase_env = self.env.for_phase(phase.short_name(), phase.func_name());
        let mut env = phase_env.vars().clone();
        for (k, v) in &plan.sandbox_vars {
            env.insert(k.clone(), v.clone());
        }
        if let Some(helper) = &self.ipc_helper {
            env.insert(
                "MORAINE_IPC_HELPER".to_string(),
                helper.to_string_lossy().to_string(),
            );
        }

        let saved_env_path = self.layout.temp.join("environment");
        let carried_path = self.layout.temp.join("environment.carried");
        // Write the carried env so the phase can re-source it.
        write_env_file(&carried_path, carried)?;

        let script = self.phase_script(phase, &carried_path, &saved_env_path);

        let program = plan
            .wrapper
            .first()
            .cloned()
            .unwrap_or_else(|| "bash".to_string());
        let mut args: Vec<String> = plan.wrapper.iter().skip(1).cloned().collect();
        if !plan.wrapper.is_empty() {
            args.push("bash".to_string());
        }
        args.push("-c".to_string());
        args.push(script);

        // The isolation the runner enforces in the child: the planned namespaces
        // plus the build-user drop when `userpriv` is selected and enforceable.
        let privilege = if plan.privilege == PrivilegeMode::UserPriv {
            self.userpriv_target.clone()
        } else {
            None
        };
        let isolation = Isolation {
            namespaces: plan.namespaces.clone(),
            privilege,
        };

        Ok(CommandSpec {
            program,
            args,
            env,
            cwd: self.layout.workdir.clone(),
            log_path: Some(self.layout.build_log.clone()),
            isolation,
        })
    }

    /// The bash `-c` script body for a phase.
    ///
    /// The library scripts are sourced in dependency order, then the carried
    /// environment from prior phases, then the ebuild (so its top-level
    /// `inherit` runs). After sourcing the ebuild the driver folds the
    /// accumulated eclass `E_*` metadata, binds the EAPI default phase set and
    /// the bare `default`, changes into `${S}` for the source phases, and
    /// dispatches the phase function with its pre/post hooks.
    fn phase_script(
        &self,
        phase: PhaseKind,
        carried_path: &std::path::Path,
        saved_env_path: &std::path::Path,
    ) -> String {
        // The library, carried env, and ebuild are sourced without `set -e`:
        // ebuilds and eclasses are not written to be errexit-clean (stock
        // Portage never runs them under `set -e`), so failures propagate through
        // `die` and helper auto-die instead. Sourcing failures are guarded
        // explicitly so a broken library or ebuild still aborts the phase.
        let mut script = String::new();
        for lib in &self.library.scripts {
            script.push_str(&format!(
                ". {} || exit 1\n",
                shquote(&lib.to_string_lossy())
            ));
        }
        script.push_str(&format!(
            "[ -f {carried} ] && {{ . {carried} || exit 1; }}\n\
             [ -f {ebuild} ] && {{ . {ebuild} || die \"error sourcing ebuild\"; }}\n\
             {fold}\n\
             {bind} \"${{EAPI:-0}}\" {func}\n",
            carried = shquote(&carried_path.to_string_lossy()),
            ebuild = shquote(&self.ebuild_path.to_string_lossy()),
            fold = bashlib::FOLD_FUNC,
            bind = bashlib::BIND_FUNC,
            func = phase.func_name(),
        ));
        // The source phases run in the unpacked source directory; the pkg_*
        // phases and src_unpack stay in WORKDIR (the process cwd).
        if cd_to_source(phase) {
            script.push_str(&format!("__cd_to_s {}\n", phase.short_name()));
        }
        script.push_str(&format!(
            "{dispatch} {func}\n\
             rc=$?\n\
             ( set -o posix; set ) > {saved} 2>/dev/null || true\n\
             exit $rc\n",
            dispatch = bashlib::DISPATCH_FUNC,
            func = phase.func_name(),
            saved = shquote(&saved_env_path.to_string_lossy()),
        ));
        script
    }

    /// Read the saved environment a phase wrote and merge it into the carried
    /// env through the filter.
    fn read_saved_env(
        &self,
        carried: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        let saved_path = self.layout.temp.join("environment");
        let incoming = match std::fs::read_to_string(&saved_path) {
            Ok(text) => parse_env_file(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                return Err(BuildError::Common(moraine_common::CommonError::Io {
                    path: saved_path,
                    source: e,
                }));
            }
        };
        Ok(filter_saved_env(carried, &incoming))
    }

    /// Scan a phase's output for the elog tag emitted by the bash helper family.
    fn capture_elog(
        &self,
        phase: PhaseKind,
        output: &CommandOutput,
        elog: &mut Vec<ElogMessage>,
    ) -> Result<()> {
        // When a log file captured the output, read it; otherwise scan stdout.
        let text = if output.stdout.is_empty() {
            std::fs::read_to_string(&self.layout.build_log).unwrap_or_default()
        } else {
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        let _ = phase;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("MORAINE_ELOG ") {
                let mut parts = rest.splitn(3, ' ');
                let (Some(level), Some(phase_name), Some(text)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                if let Some(level) = ElogLevel::parse(level) {
                    let msg = ElogMessage {
                        level,
                        phase: phase_name.to_string(),
                        text: text.to_string(),
                    };
                    if !elog.contains(&msg) {
                        elog.push(msg);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Resolve the `userpriv` build-user drop target from the configured
/// `PORTAGE_USERNAME`/`PORTAGE_GRPNAME`, only when the engine runs as root.
///
/// When not root the privilege drop is a no-op and `userpriv` is not reported as
/// applied, matching Portage's `uid == 0` guard. Returns `None` when not root or
/// when the build user cannot be resolved.
fn resolve_userpriv_target(env: &EnvBuilder) -> Option<PrivilegeDrop> {
    #[cfg(target_os = "linux")]
    if !rustix::process::getuid().is_root() {
        return None;
    }
    let vars = &env.config().vars;
    let username = vars
        .get("PORTAGE_USERNAME")
        .map(String::as_str)
        .unwrap_or("portage");
    let groupname = vars
        .get("PORTAGE_GRPNAME")
        .map(String::as_str)
        .unwrap_or("portage");
    resolve_build_user(username, groupname)
}

/// Quote a string for safe inclusion in a single-quoted bash context.
fn shquote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

/// Whether a phase runs in the unpacked source directory `${S}`. The source
/// phases after unpack `cd` into `${S}`; the `pkg_*` phases and `src_unpack`
/// stay in `WORKDIR`.
fn cd_to_source(phase: PhaseKind) -> bool {
    matches!(
        phase,
        PhaseKind::SrcPrepare
            | PhaseKind::SrcConfigure
            | PhaseKind::SrcCompile
            | PhaseKind::SrcTest
            | PhaseKind::SrcInstall
    )
}

/// Write a `KEY=value` env file, one per line, as plain exports.
///
/// The function bodies the ebuild and eclasses define are re-established by
/// re-sourcing the ebuild (with a working `inherit`) each phase, so the saved
/// environment only carries plain variable assignments; `set -o posix; set`
/// never emits function definitions for this carry to capture.
fn write_env_file(path: &std::path::Path, env: &BTreeMap<String, String>) -> Result<()> {
    let mut body = String::new();
    for (k, v) in env {
        body.push_str(&format!("export {}={}\n", k, shquote(v)));
    }
    moraine_common::fs::atomic_write(path, body.as_bytes())?;
    Ok(())
}

/// Parse a `set`-style env dump into a map, keeping simple `KEY=value` lines and
/// dropping multi-line function bodies and shell internals we cannot represent.
fn parse_env_file(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        // `set` quotes values with single quotes; strip a single surrounding
        // pair when present.
        let value = value
            .strip_prefix('\'')
            .and_then(|v| v.strip_suffix('\''))
            .unwrap_or(value);
        out.insert(key.to_string(), value.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::{ConfigEnv, PackageIdent};
    use crate::runner::testing::{FakeRunner, Response};
    use crate::sandbox::NamespaceSupport;

    fn ident(eapi: &str) -> PackageIdent {
        PackageIdent {
            category: "dev-libs".into(),
            pf: "foo-1".into(),
            p: "foo-1".into(),
            pn: "foo".into(),
            pv: "1".into(),
            pvr: "1".into(),
            pr: "r0".into(),
            eapi: eapi.into(),
            repository: "gentoo".into(),
        }
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        layout: BuildLayout,
        library: PhaseLibrary,
        ebuild: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let layout = BuildLayout::new(tmp.path(), "dev-libs", "foo-1").unwrap();
        layout.create().unwrap();
        let library = PhaseLibrary::materialize(layout.temp.join("bashlib")).unwrap();
        let ebuild = tmp.path().join("foo-1.ebuild");
        std::fs::write(&ebuild, "# ebuild\n").unwrap();
        Fixture {
            _tmp: tmp,
            layout,
            library,
            ebuild,
        }
    }

    fn selector(features: &[&str]) -> SandboxSelector {
        let cfg = ConfigEnv::rooted(features.iter().map(|s| s.to_string()));
        SandboxSelector::from_config(&cfg, [], NamespaceSupport::default())
    }

    #[test]
    fn phases_run_in_order() {
        let fx = fixture();
        let cfg = ConfigEnv::rooted([]);
        let env = EnvBuilder::new(ident("8"), cfg, &fx.layout).unwrap();
        let sel = selector(&[]);
        let runner = FakeRunner::always_ok();
        let defined = vec![
            "setup".into(),
            "unpack".into(),
            "prepare".into(),
            "configure".into(),
            "compile".into(),
            "install".into(),
        ];
        let driver = PhaseDriver::new(
            &runner,
            &env,
            &fx.layout,
            &fx.library,
            &sel,
            &fx.ebuild,
            defined,
            false,
            Vec::new(),
            None,
        );
        let report = driver.run_all().unwrap();
        let invoked = report.invoked_phases();
        assert_eq!(
            invoked,
            vec![
                PhaseKind::PkgSetup,
                PhaseKind::SrcUnpack,
                PhaseKind::SrcPrepare,
                PhaseKind::SrcConfigure,
                PhaseKind::SrcCompile,
                PhaseKind::SrcInstall,
            ]
        );
    }

    #[test]
    fn eapi_gates_prepare_and_configure() {
        let fx = fixture();
        let cfg = ConfigEnv::rooted([]);
        let env = EnvBuilder::new(ident("1"), cfg, &fx.layout).unwrap();
        let sel = selector(&[]);
        let runner = FakeRunner::always_ok();
        let driver = PhaseDriver::new(
            &runner,
            &env,
            &fx.layout,
            &fx.library,
            &sel,
            &fx.ebuild,
            vec!["prepare".into(), "configure".into(), "compile".into()],
            false,
            Vec::new(),
            None,
        );
        let report = driver.run_all().unwrap();
        let invoked = report.invoked_phases();
        // EAPI 1 has no src_prepare or src_configure.
        assert!(!invoked.contains(&PhaseKind::SrcPrepare));
        assert!(!invoked.contains(&PhaseKind::SrcConfigure));
        assert!(invoked.contains(&PhaseKind::SrcCompile));
    }

    #[test]
    fn empty_phase_skipped() {
        let fx = fixture();
        let cfg = ConfigEnv::rooted([]);
        let env = EnvBuilder::new(ident("8"), cfg, &fx.layout).unwrap();
        let sel = selector(&[]);
        let runner = FakeRunner::always_ok();
        // pkg_setup not in DEFINED_PHASES and has no default work, so skipped.
        let driver = PhaseDriver::new(
            &runner,
            &env,
            &fx.layout,
            &fx.library,
            &sel,
            &fx.ebuild,
            vec!["compile".into()],
            false,
            Vec::new(),
            None,
        );
        let report = driver.run_all().unwrap();
        let invoked = report.invoked_phases();
        assert!(!invoked.contains(&PhaseKind::PkgSetup));
        assert!(invoked.contains(&PhaseKind::SrcCompile));
        // unpack/configure/install still run defaults.
        assert!(invoked.contains(&PhaseKind::SrcUnpack));
    }

    #[test]
    fn nonzero_phase_fails_build() {
        let fx = fixture();
        let cfg = ConfigEnv::rooted([]);
        let env = EnvBuilder::new(ident("8"), cfg, &fx.layout).unwrap();
        let sel = selector(&[]);
        let runner = FakeRunner::default();
        // First invoked phase fails.
        runner.push(Response::Output {
            status: 7,
            bytes: Vec::new(),
        });
        let driver = PhaseDriver::new(
            &runner,
            &env,
            &fx.layout,
            &fx.library,
            &sel,
            &fx.ebuild,
            vec!["setup".into(), "compile".into()],
            false,
            Vec::new(),
            None,
        );
        let err = driver.run_all();
        assert!(matches!(err, Err(BuildError::Phase { status: 7, .. })));
    }

    #[test]
    fn elog_captured_with_severity_and_phase() {
        let fx = fixture();
        let cfg = ConfigEnv::rooted([]);
        let env = EnvBuilder::new(ident("8"), cfg, &fx.layout).unwrap();
        let sel = selector(&[]);
        let runner = FakeRunner::default();
        // The phase writes an elog tag to the log.
        runner.push(Response::Output {
            status: 0,
            bytes: b"MORAINE_ELOG WARN compile something happened\n".to_vec(),
        });
        let driver = PhaseDriver::new(
            &runner,
            &env,
            &fx.layout,
            &fx.library,
            &sel,
            &fx.ebuild,
            vec!["compile".into()],
            false,
            Vec::new(),
            None,
        );
        let report = driver.run_all().unwrap();
        let msg = report
            .elog
            .iter()
            .find(|m| m.text.contains("something happened"));
        let msg = msg.expect("elog captured");
        assert_eq!(msg.level, ElogLevel::Warn);
        assert_eq!(msg.phase, "compile");
    }

    #[test]
    fn saved_env_carries_across_phases() {
        let fx = fixture();
        let cfg = ConfigEnv::rooted([]);
        let env = EnvBuilder::new(ident("8"), cfg, &fx.layout).unwrap();
        let sel = selector(&[]);
        let runner = FakeRunner::default();
        // Simulate the setup phase writing a saved env, then verify the next
        // phase carries it. The FakeRunner does not run bash, so the driver's
        // read_saved_env reads whatever is at temp/environment. We pre-seed it
        // before the second phase by writing through the log response is not
        // enough; instead assert read/filter directly.
        let _ = (&env, &sel, &runner);
        // Write a saved environment file as a phase would.
        let saved = fx.layout.temp.join("environment");
        std::fs::write(&saved, "MY_STATE='kept'\nEAPI='tampered'\n").unwrap();
        let driver = PhaseDriver::new(
            &runner,
            &env,
            &fx.layout,
            &fx.library,
            &sel,
            &fx.ebuild,
            vec!["compile".into()],
            false,
            Vec::new(),
            None,
        );
        let carried = BTreeMap::new();
        let merged = driver.read_saved_env(&carried).unwrap();
        assert_eq!(merged.get("MY_STATE"), Some(&"kept".to_string()));
        // EAPI is readonly metadata and must not be carried.
        assert_eq!(merged.get("EAPI"), None);
    }

    #[test]
    fn command_sources_library_and_dispatches() {
        let fx = fixture();
        let cfg = ConfigEnv::rooted(["sandbox".to_string()]);
        let env = EnvBuilder::new(ident("8"), cfg, &fx.layout).unwrap();
        let sel = selector(&["sandbox"]);
        let runner = FakeRunner::always_ok();
        let driver = PhaseDriver::new(
            &runner,
            &env,
            &fx.layout,
            &fx.library,
            &sel,
            &fx.ebuild,
            vec!["compile".into()],
            false,
            Vec::new(),
            None,
        );
        driver.run_all().unwrap();
        let calls = runner.calls();
        let compile = calls
            .iter()
            .find(|c| c.args.iter().any(|a| a.contains("src_compile")))
            .expect("compile call");
        let script = compile.args.last().unwrap();
        assert!(script.contains("phase-functions.sh"));
        assert!(script.contains("__ebuild_phase_with_hooks src_compile"));
        // Sandbox wrapper is the program.
        assert_eq!(compile.program, "sandbox");
        assert!(compile.env.contains_key("SANDBOX_WRITE"));
    }

    #[test]
    fn network_needed_follows_properties() {
        let fx = fixture();
        let cfg = ConfigEnv::rooted([]);
        let env = EnvBuilder::new(ident("8"), cfg, &fx.layout).unwrap();
        let sel = selector(&[]);
        let runner = FakeRunner::always_ok();

        let make = |props: Vec<String>| {
            PhaseDriver::new(
                &runner,
                &env,
                &fx.layout,
                &fx.library,
                &sel,
                &fx.ebuild,
                vec!["compile".into()],
                false,
                props,
                None,
            )
        };

        let live = make(vec!["live".into()]);
        assert!(live.network_needed(PhaseKind::SrcUnpack));
        assert!(!live.network_needed(PhaseKind::SrcTest));

        let test_net = make(vec!["test_network".into()]);
        assert!(test_net.network_needed(PhaseKind::SrcTest));
        assert!(!test_net.network_needed(PhaseKind::SrcUnpack));

        let plain = make(Vec::new());
        assert!(!plain.network_needed(PhaseKind::SrcUnpack));
        assert!(!plain.network_needed(PhaseKind::SrcTest));
        assert!(!plain.network_needed(PhaseKind::SrcCompile));
    }

    #[test]
    fn isolation_reaches_command_spec_and_applied_features() {
        let fx = fixture();
        let cfg = ConfigEnv::rooted(["mount-sandbox".to_string()]);
        let env = EnvBuilder::new(ident("8"), cfg, &fx.layout).unwrap();
        let sel = selector(&["mount-sandbox"]);
        let runner = FakeRunner::always_ok();
        let driver = PhaseDriver::new(
            &runner,
            &env,
            &fx.layout,
            &fx.library,
            &sel,
            &fx.ebuild,
            vec!["compile".into()],
            false,
            Vec::new(),
            None,
        );
        let report = driver.run_all().unwrap();

        // The planned namespace reaches the spec the runner received.
        let calls = runner.calls();
        let compile = calls
            .iter()
            .find(|c| c.args.iter().any(|a| a.contains("src_compile")))
            .expect("compile call");
        assert!(
            compile
                .isolation
                .namespaces
                .contains(&crate::sandbox::Namespace::Mount)
        );

        // applied_features reflects only the runner-reported enforced isolation.
        let compile_run = report
            .runs
            .iter()
            .find(|r| r.phase == PhaseKind::SrcCompile && r.invoked)
            .expect("compile run");
        assert!(
            compile_run
                .applied_features
                .contains(&"mount-sandbox".to_string())
        );
    }
}
