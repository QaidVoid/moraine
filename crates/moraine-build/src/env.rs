//! Build environment computation.
//!
//! Computes the variables exported into the ebuild bash environment for one
//! resolved package, gating the root, prefix, and per-phase variables on the
//! active EAPI's feature table from [`moraine_eapi`]. It also owns the
//! saved-environment filter that carries mutable state across the per-phase
//! forked bash processes.
//!
//! The build flags, `FEATURES`, mirrors, and root and prefix paths come from the
//! caller as a [`ConfigEnv`], which the orchestrator fills from
//! `moraine-config`. Keeping the input as a plain map keeps this crate testable
//! without standing up a whole resolved configuration.

use std::collections::BTreeMap;

use moraine_eapi::EapiFeatures;
use tracing::instrument;

use crate::error::{BuildError, Result};
use crate::layout::BuildLayout;

/// The flag, feature, mirror, and root configuration the orchestrator resolves
/// from `moraine-config` and hands to the build engine.
///
/// Every value is the already-resolved string the ebuild environment expects.
/// Absent keys are simply not exported.
#[derive(Debug, Clone, Default)]
pub struct ConfigEnv {
    /// Build and toolchain variables (`CFLAGS`, `CXXFLAGS`, `LDFLAGS`, `CHOST`,
    /// `CBUILD`, `MAKEOPTS`, and any others the profile sets).
    pub vars: BTreeMap<String, String>,
    /// The resolved `FEATURES` tokens, order-preserving.
    pub features: Vec<String>,
    /// The configured `GENTOO_MIRRORS` URIs.
    pub mirrors: Vec<String>,
    /// The `ROOT` install offset (host root, normally `/`).
    pub root: String,
    /// The `SYSROOT` (build-against root, normally equal to `ROOT`).
    pub sysroot: String,
    /// The `EPREFIX` offset for prefix installs (empty for a non-prefix build).
    pub eprefix: String,
}

impl ConfigEnv {
    /// Build a minimal `ConfigEnv` rooted at `/` with the given features.
    pub fn rooted(features: impl IntoIterator<Item = String>) -> Self {
        ConfigEnv {
            vars: BTreeMap::new(),
            features: features.into_iter().collect(),
            mirrors: Vec::new(),
            root: "/".to_string(),
            sysroot: "/".to_string(),
            eprefix: String::new(),
        }
    }

    /// Whether the named feature is present.
    pub fn has_feature(&self, name: &str) -> bool {
        self.features.iter().any(|f| f == name)
    }
}

/// Identity of the package being built, as needed for the environment.
#[derive(Debug, Clone)]
pub struct PackageIdent {
    /// The package category, for example `dev-libs`.
    pub category: String,
    /// The full package name and version, `PF`, for example `foo-1.2.3-r1`.
    pub pf: String,
    /// The package name and version without revision, `P`, for example
    /// `foo-1.2.3`.
    pub p: String,
    /// The package name, `PN`, for example `foo`.
    pub pn: String,
    /// The version, `PV`, for example `1.2.3`.
    pub pv: String,
    /// The version with revision, `PVR`, for example `1.2.3-r1`.
    pub pvr: String,
    /// The version revision token, `PR`, for example `r1`.
    pub pr: String,
    /// The EAPI string, for example `8`.
    pub eapi: String,
    /// The originating repository name.
    pub repository: String,
}

impl PackageIdent {
    /// The numeric EAPI level, or `None` for an unsupported EAPI.
    pub fn eapi_level(&self) -> Option<u8> {
        moraine_eapi::level(&self.eapi)
    }

    /// The EAPI feature table for this package.
    pub fn eapi_features(&self) -> EapiFeatures {
        moraine_eapi::features_for(&self.eapi)
    }
}

/// The fully computed environment for one phase of one build.
///
/// This is an ordered map of variable name to value; the phase driver exports
/// each entry into the bash process. The map is recomputed per phase only for
/// the per-phase variables; the static portion is shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseEnv {
    vars: BTreeMap<String, String>,
}

impl PhaseEnv {
    /// The variable map, sorted by name.
    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }

    /// Look up a single variable.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// Render the environment as `KEY=value` lines, sorted by key. The value is
    /// not shell-quoted; this rendering is for inspection and snapshots, not for
    /// sourcing.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (k, v) in &self.vars {
            out.push_str(k);
            out.push('=');
            out.push_str(v);
            out.push('\n');
        }
        out
    }
}

/// Builds the static and per-phase ebuild environment for a package.
#[derive(Debug, Clone)]
pub struct EnvBuilder {
    ident: PackageIdent,
    config: ConfigEnv,
    features: EapiFeatures,
    base: BTreeMap<String, String>,
}

impl EnvBuilder {
    /// Construct the environment builder for a package, computing the static
    /// portion of the environment immediately.
    #[instrument(name = "env_setup", skip_all, fields(pf = %ident.pf, eapi = %ident.eapi))]
    pub fn new(ident: PackageIdent, config: ConfigEnv, layout: &BuildLayout) -> Result<Self> {
        let features = ident.eapi_features();
        let mut base = BTreeMap::new();

        // Flag and toolchain variables straight from the resolved config.
        for (k, v) in &config.vars {
            base.insert(k.clone(), v.clone());
        }

        base.insert("FEATURES".to_string(), config.features.join(" "));
        base.insert("GENTOO_MIRRORS".to_string(), config.mirrors.join(" "));

        // Package identity.
        base.insert("CATEGORY".to_string(), ident.category.clone());
        base.insert("PF".to_string(), ident.pf.clone());
        base.insert("P".to_string(), ident.p.clone());
        base.insert("PN".to_string(), ident.pn.clone());
        base.insert("PV".to_string(), ident.pv.clone());
        base.insert("PVR".to_string(), ident.pvr.clone());
        base.insert("PR".to_string(), ident.pr.clone());
        base.insert("EAPI".to_string(), ident.eapi.clone());
        base.insert("PORTAGE_REPO_NAME".to_string(), ident.repository.clone());

        // Build-tree paths.
        base.insert("PORTAGE_BUILDDIR".to_string(), path_str(&layout.builddir)?);
        base.insert("WORKDIR".to_string(), path_str(&layout.workdir)?);
        base.insert("T".to_string(), path_str(&layout.temp)?);
        base.insert("D".to_string(), path_str(&layout.image)?);
        base.insert("HOME".to_string(), path_str(&layout.home)?);
        base.insert("PORTAGE_BUILD_HOME".to_string(), path_str(&layout.home)?);

        // Root and prefix variables, EAPI-gated.
        Self::insert_root_vars(&mut base, &ident, &config, features)?;

        Ok(EnvBuilder {
            ident,
            config,
            features,
            base,
        })
    }

    /// Insert the `ROOT`/`SYSROOT`/`EROOT`/`ESYSROOT`/`BROOT`/`ED`/`EPREFIX`
    /// variables according to the EAPI feature gates.
    ///
    /// `ROOT` and `SYSROOT` are always exported. The prefix variables `ED`,
    /// `EPREFIX`, and `EROOT` are dropped for EAPIs that predate prefix support.
    /// `ESYSROOT` and `BROOT` are only exported for EAPIs that define sysroot and
    /// broot respectively.
    fn insert_root_vars(
        base: &mut BTreeMap<String, String>,
        ident: &PackageIdent,
        config: &ConfigEnv,
        features: EapiFeatures,
    ) -> Result<()> {
        let root = with_trailing_slash(&config.root);
        let sysroot = with_trailing_slash(&config.sysroot);
        base.insert("ROOT".to_string(), root.clone());
        base.insert("SYSROOT".to_string(), sysroot.clone());

        if features.prefix {
            let eprefix = config.eprefix.clone();
            // EROOT/ED/ESYSROOT/BROOT compose the prefix onto the roots.
            let eroot = join_offset(&root, &eprefix);
            base.insert("EPREFIX".to_string(), eprefix.clone());
            base.insert("EROOT".to_string(), eroot);
            base.insert("ED".to_string(), join_offset("/", &eprefix));
        }

        // ESYSROOT and BROOT are EAPI 7+ (the bdepend gate matches the broot and
        // sysroot introduction in stock Portage's feature table).
        if features.bdepend {
            let esysroot = join_offset(&sysroot, &config.eprefix);
            base.insert("ESYSROOT".to_string(), esysroot);
            // BROOT is the build (host) prefix root; for a non-cross build it is
            // the EPREFIX onto `/`.
            base.insert("BROOT".to_string(), join_offset("/", &config.eprefix));
        }

        let _ = ident;
        Ok(())
    }

    /// The EAPI feature table for the package.
    pub fn features(&self) -> EapiFeatures {
        self.features
    }

    /// The package identity.
    pub fn ident(&self) -> &PackageIdent {
        &self.ident
    }

    /// The resolved configuration.
    pub fn config(&self) -> &ConfigEnv {
        &self.config
    }

    /// Compute the environment for a specific phase, adding `EBUILD_PHASE` and,
    /// where the EAPI exports it, `EBUILD_PHASE_FUNC`.
    #[instrument(name = "phase_env", skip(self), fields(phase = phase))]
    pub fn for_phase(&self, phase: &str, phase_func: &str) -> PhaseEnv {
        let mut vars = self.base.clone();
        vars.insert("EBUILD_PHASE".to_string(), phase.to_string());
        // EBUILD_PHASE_FUNC is exported only from EAPI 4 onward. The required_use
        // gate (EAPI 4+) matches that introduction in the stock feature table.
        if self.features.required_use {
            vars.insert("EBUILD_PHASE_FUNC".to_string(), phase_func.to_string());
        }
        PhaseEnv { vars }
    }

    /// The static base environment without any per-phase variables, used for the
    /// saved environment and metadata.
    pub fn base(&self) -> &BTreeMap<String, String> {
        &self.base
    }
}

/// Variables that are readonly ebuild metadata and must never be re-imported as
/// mutable from a saved environment. Mirrors the stock readonly variable set in
/// `filter-readonly-variables`.
const READONLY_VARS: &[&str] = &[
    "CATEGORY",
    "P",
    "PF",
    "PN",
    "PV",
    "PVR",
    "PR",
    "EAPI",
    "PORTAGE_BUILDDIR",
    "WORKDIR",
    "T",
    "D",
    "HOME",
    "ROOT",
    "SYSROOT",
    "EROOT",
    "ESYSROOT",
    "BROOT",
    "ED",
    "EPREFIX",
    "EBUILD_PHASE",
    "EBUILD_PHASE_FUNC",
    "PORTAGE_REPO_NAME",
];

/// Bash-internal variables that must be dropped from a saved environment.
/// Mirrors the stock shell-internal filter set.
const SHELL_INTERNAL_PREFIXES: &[&str] = &["BASH", "PIPESTATUS", "FUNCNAME", "SHELLOPTS"];
const SHELL_INTERNAL_EXACT: &[&str] = &[
    "EUID", "UID", "PPID", "PWD", "OLDPWD", "RANDOM", "SECONDS", "LINENO", "_", "IFS",
];

/// Filter a saved environment so it is safe to re-source into the next phase.
///
/// Readonly metadata variables are removed (they are re-exported fresh by
/// [`EnvBuilder::for_phase`]), bash-internal variables are removed, and the
/// cumulative `SANDBOX_*` path variables are merged so each phase sees the union
/// of every prior phase's predicted and allowed write paths.
///
/// `incoming` is the environment captured at the end of a phase. `previous` is
/// the merged environment carried so far. The returned map is the new carried
/// environment.
#[instrument(name = "saved_env_filter", skip_all)]
pub fn filter_saved_env(
    previous: &BTreeMap<String, String>,
    incoming: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = previous.clone();
    for (key, value) in incoming {
        if is_readonly(key) || is_shell_internal(key) {
            continue;
        }
        if let Some(suffix) = key.strip_prefix("SANDBOX_") {
            // SANDBOX_WRITE / SANDBOX_PREDICT / SANDBOX_READ / SANDBOX_DENY are
            // colon-separated path lists; accumulate the union across phases.
            let merged = merge_sandbox_paths(out.get(key).map(String::as_str), value);
            out.insert(format!("SANDBOX_{suffix}"), merged);
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    out
}

fn is_readonly(key: &str) -> bool {
    READONLY_VARS.contains(&key)
}

fn is_shell_internal(key: &str) -> bool {
    if SHELL_INTERNAL_EXACT.contains(&key) {
        return true;
    }
    SHELL_INTERNAL_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// Merge two colon-separated path lists, preserving order and dropping empties
/// and duplicates.
fn merge_sandbox_paths(existing: Option<&str>, incoming: &str) -> String {
    let mut seen = Vec::new();
    for part in existing
        .into_iter()
        .flat_map(|s| s.split(':'))
        .chain(incoming.split(':'))
    {
        if part.is_empty() {
            continue;
        }
        if !seen.iter().any(|p| p == part) {
            seen.push(part.to_string());
        }
    }
    seen.join(":")
}

fn path_str(path: &std::path::Path) -> Result<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| BuildError::environment(format!("non-UTF-8 path: {}", path.display())))
}

/// Ensure a directory path ends with exactly one trailing slash, except the bare
/// root which stays `/`.
fn with_trailing_slash(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    if path.ends_with('/') {
        path.to_string()
    } else {
        format!("{path}/")
    }
}

/// Compose a prefix offset onto a root, normalizing slashes. An empty offset
/// yields the root unchanged.
fn join_offset(root: &str, offset: &str) -> String {
    let root = with_trailing_slash(root);
    if offset.is_empty() {
        return root.trim_end_matches('/').to_string() + "/";
    }
    let offset = offset.trim_start_matches('/').trim_end_matches('/');
    format!("{}{}", root, offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::BuildLayout;

    fn ident(eapi: &str) -> PackageIdent {
        PackageIdent {
            category: "dev-libs".into(),
            pf: "foo-1.2.3-r1".into(),
            p: "foo-1.2.3".into(),
            pn: "foo".into(),
            pv: "1.2.3".into(),
            pvr: "1.2.3-r1".into(),
            pr: "r1".into(),
            eapi: eapi.into(),
            repository: "gentoo".into(),
        }
    }

    fn layout() -> (tempfile::TempDir, BuildLayout) {
        let tmp = tempfile::tempdir().unwrap();
        let layout = BuildLayout::new(tmp.path(), "dev-libs", "foo-1.2.3-r1").expect("layout");
        (tmp, layout)
    }

    #[test]
    fn flags_and_use_exported() {
        let (_t, layout) = layout();
        let mut cfg = ConfigEnv::rooted(["sandbox".to_string()]);
        cfg.vars.insert("CFLAGS".into(), "-O2".into());
        cfg.vars.insert("CXXFLAGS".into(), "-O2".into());
        cfg.vars.insert("USE".into(), "ssl threads".into());
        cfg.vars.insert("MAKEOPTS".into(), "-j4".into());
        cfg.vars
            .insert("CHOST".into(), "x86_64-pc-linux-gnu".into());
        let b = EnvBuilder::new(ident("8"), cfg, &layout).unwrap();
        let env = b.for_phase("compile", "src_compile");
        assert_eq!(env.get("CFLAGS"), Some("-O2"));
        assert_eq!(env.get("USE"), Some("ssl threads"));
        assert_eq!(env.get("MAKEOPTS"), Some("-j4"));
        assert_eq!(env.get("FEATURES"), Some("sandbox"));
        assert_eq!(env.get("EAPI"), Some("8"));
    }

    #[test]
    fn phase_func_gated_on_eapi() {
        let (_t, layout) = layout();
        let cfg = ConfigEnv::rooted([]);
        let b3 = EnvBuilder::new(ident("3"), cfg.clone(), &layout).unwrap();
        assert_eq!(
            b3.for_phase("compile", "src_compile")
                .get("EBUILD_PHASE_FUNC"),
            None
        );
        let b4 = EnvBuilder::new(ident("4"), cfg, &layout).unwrap();
        assert_eq!(
            b4.for_phase("compile", "src_compile")
                .get("EBUILD_PHASE_FUNC"),
            Some("src_compile")
        );
    }

    #[test]
    fn sysroot_broot_gated_at_seven() {
        let (_t, layout) = layout();
        let cfg = ConfigEnv::rooted([]);
        let b6 = EnvBuilder::new(ident("6"), cfg.clone(), &layout).unwrap();
        let e6 = b6.for_phase("compile", "src_compile");
        assert_eq!(e6.get("ESYSROOT"), None);
        assert_eq!(e6.get("BROOT"), None);
        let b7 = EnvBuilder::new(ident("7"), cfg, &layout).unwrap();
        let e7 = b7.for_phase("compile", "src_compile");
        assert!(e7.get("ESYSROOT").is_some());
        assert!(e7.get("BROOT").is_some());
    }

    #[test]
    fn prefix_vars_gated_at_three() {
        let (_t, layout) = layout();
        let cfg = ConfigEnv::rooted([]);
        let b2 = EnvBuilder::new(ident("2"), cfg.clone(), &layout).unwrap();
        let e2 = b2.for_phase("compile", "src_compile");
        assert_eq!(e2.get("ED"), None);
        assert_eq!(e2.get("EPREFIX"), None);
        assert_eq!(e2.get("EROOT"), None);
        let b3 = EnvBuilder::new(ident("3"), cfg, &layout).unwrap();
        let e3 = b3.for_phase("compile", "src_compile");
        assert!(e3.get("EPREFIX").is_some());
        assert!(e3.get("EROOT").is_some());
    }

    #[test]
    fn saved_env_drops_readonly_and_keeps_mutable() {
        let prev = BTreeMap::new();
        let mut incoming = BTreeMap::new();
        incoming.insert("EAPI".into(), "tampered".into());
        incoming.insert("MY_VAR".into(), "value".into());
        incoming.insert("BASH_VERSION".into(), "5".into());
        let out = filter_saved_env(&prev, &incoming);
        assert_eq!(out.get("MY_VAR"), Some(&"value".to_string()));
        assert_eq!(out.get("EAPI"), None);
        assert_eq!(out.get("BASH_VERSION"), None);
    }

    #[test]
    fn saved_env_merges_sandbox_paths() {
        let mut prev = BTreeMap::new();
        prev.insert("SANDBOX_WRITE".into(), "/tmp/a:/tmp/b".into());
        let mut incoming = BTreeMap::new();
        incoming.insert("SANDBOX_WRITE".into(), "/tmp/b:/tmp/c".into());
        let out = filter_saved_env(&prev, &incoming);
        assert_eq!(out.get("SANDBOX_WRITE").unwrap(), "/tmp/a:/tmp/b:/tmp/c");
    }

    #[test]
    fn function_carries_across_phases() {
        // Functions are captured as a synthetic key with a body; the filter must
        // carry it forward like any other mutable variable.
        let prev = BTreeMap::new();
        let mut incoming = BTreeMap::new();
        incoming.insert("FUNCTION:myfunc".into(), "myfunc() { echo hi; }".into());
        let out = filter_saved_env(&prev, &incoming);
        assert_eq!(out.get("FUNCTION:myfunc").unwrap(), "myfunc() { echo hi; }");
    }
}
