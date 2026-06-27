//! Build isolation selection.
//!
//! Selects the isolation for a phase from the active `FEATURES`, mirroring how
//! stock Portage chooses between the external `sandbox`/`fakeroot` binaries and
//! Linux namespace unsharing in `_doebuild_spawn`/`spawn`. This module computes
//! the plan (a wrapper command prefix plus the `SANDBOX_*` write-confinement
//! variables and the set of namespaces to unshare) and reports which isolation
//! was actually applied; launching is the phase driver's job through the
//! injected runner.
//!
//! The plan is built so that on a kernel without unprivileged user namespaces a
//! namespace can be marked unavailable and dropped, falling back to the
//! `sandbox` binary as the portable write-confinement layer.

use std::path::Path;

use crate::env::ConfigEnv;
use crate::error::PhaseKind;

/// The kind of privilege the install image staging runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegeMode {
    /// Run with real privileges (no faked root).
    Direct,
    /// Wrap with `fakeroot` so the image can record arbitrary ownership.
    Fakeroot,
    /// Drop to the unprivileged build user (`userpriv`).
    UserPriv,
}

/// The isolation plan for one phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPlan {
    /// The wrapper command (program plus args) to prepend to the phase command,
    /// for example `["sandbox"]` or `["fakeroot", "--"]`. Empty when no wrapper
    /// applies.
    pub wrapper: Vec<String>,
    /// The `SANDBOX_*` variables confining writes, merged into the phase env.
    pub sandbox_vars: Vec<(String, String)>,
    /// The Linux namespaces to unshare for this phase.
    pub namespaces: Vec<Namespace>,
    /// Whether the network is isolated for this phase.
    pub network_isolated: bool,
    /// The privilege mode applied.
    pub privilege: PrivilegeMode,
    /// The FEATURES that were honored in this plan, for surfacing to the caller.
    pub applied_features: Vec<String>,
}

/// A Linux namespace the build can unshare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// Network namespace (`network-sandbox`).
    Network,
    /// Mount namespace (`mount-sandbox`).
    Mount,
    /// PID namespace (`pid-sandbox`).
    Pid,
    /// IPC namespace (`ipc-sandbox`).
    Ipc,
}

impl Namespace {
    /// The FEATURE name that selects this namespace.
    pub fn feature(self) -> &'static str {
        match self {
            Namespace::Network => "network-sandbox",
            Namespace::Mount => "mount-sandbox",
            Namespace::Pid => "pid-sandbox",
            Namespace::Ipc => "ipc-sandbox",
        }
    }
}

/// What namespaces the host kernel supports, so the plan can fall back where one
/// is unavailable. The default reports all available; tests and the orchestrator
/// can mark some unavailable.
#[derive(Debug, Clone, Copy)]
pub struct NamespaceSupport {
    /// Whether unprivileged network-namespace unsharing is available.
    pub network: bool,
    /// Whether mount-namespace unsharing is available.
    pub mount: bool,
    /// Whether PID-namespace unsharing is available.
    pub pid: bool,
    /// Whether IPC-namespace unsharing is available.
    pub ipc: bool,
}

impl Default for NamespaceSupport {
    fn default() -> Self {
        NamespaceSupport {
            network: true,
            mount: true,
            pid: true,
            ipc: true,
        }
    }
}

impl NamespaceSupport {
    fn supports(&self, ns: Namespace) -> bool {
        match ns {
            Namespace::Network => self.network,
            Namespace::Mount => self.mount,
            Namespace::Pid => self.pid,
            Namespace::Ipc => self.ipc,
        }
    }
}

/// Selects the isolation plan for each phase from FEATURES and RESTRICT.
#[derive(Debug, Clone)]
pub struct SandboxSelector {
    use_sandbox: bool,
    use_usersandbox: bool,
    use_userpriv: bool,
    use_fakeroot: bool,
    network_sandbox: bool,
    mount_sandbox: bool,
    pid_sandbox: bool,
    ipc_sandbox: bool,
    restrict_network_sandbox: bool,
    support: NamespaceSupport,
}

impl SandboxSelector {
    /// Build a selector from the resolved config FEATURES and the package's
    /// `RESTRICT` token list.
    pub fn from_config<'a>(
        config: &ConfigEnv,
        restrict: impl IntoIterator<Item = &'a str>,
        support: NamespaceSupport,
    ) -> Self {
        let restrict_network_sandbox = restrict.into_iter().any(|t| t == "network-sandbox");
        SandboxSelector {
            use_sandbox: config.has_feature("sandbox"),
            use_usersandbox: config.has_feature("usersandbox"),
            use_userpriv: config.has_feature("userpriv"),
            use_fakeroot: config.has_feature("fakeroot"),
            network_sandbox: config.has_feature("network-sandbox"),
            mount_sandbox: config.has_feature("mount-sandbox"),
            pid_sandbox: config.has_feature("pid-sandbox"),
            ipc_sandbox: config.has_feature("ipc-sandbox"),
            restrict_network_sandbox,
            support,
        }
    }

    /// Compute the isolation plan for a phase, given the build tree root (the
    /// path writes are confined to) and whether the phase needs the network.
    ///
    /// `network_needed` is set for phases the IPC channel or live sources require
    /// network for (for example `src_unpack` of a live ebuild or `src_test` when
    /// tests need the network), and for any phase when `RESTRICT=network-sandbox`
    /// applies.
    pub fn plan(&self, phase: PhaseKind, build_root: &Path, network_needed: bool) -> SandboxPlan {
        let mut applied = Vec::new();
        let is_install = phase == PhaseKind::SrcInstall;

        // Filesystem write confinement via the sandbox binary. The install phase
        // also confines, but additionally runs under faked privilege.
        let sandbox_active = if is_install {
            self.use_sandbox || self.use_usersandbox
        } else {
            self.use_sandbox || (self.use_userpriv && self.use_usersandbox)
        };

        let mut wrapper = Vec::new();
        let mut sandbox_vars = Vec::new();

        // Privilege mode. `fakeroot` is enforced through the wrapper command, so
        // it is reported at plan time; `userpriv` is reported only once the
        // runner confirms the privilege drop, so it is not pushed here.
        let privilege = if is_install && self.use_fakeroot {
            applied.push("fakeroot".to_string());
            PrivilegeMode::Fakeroot
        } else if self.use_userpriv {
            PrivilegeMode::UserPriv
        } else {
            PrivilegeMode::Direct
        };

        if privilege == PrivilegeMode::Fakeroot {
            wrapper.push("fakeroot".to_string());
            wrapper.push("--".to_string());
        }

        if sandbox_active {
            wrapper.push("sandbox".to_string());
            if self.use_sandbox {
                applied.push("sandbox".to_string());
            }
            if self.use_usersandbox {
                applied.push("usersandbox".to_string());
            }
            let root = build_root.to_string_lossy();
            // Writes are allowed only under the build tree; everything else is
            // read-only.
            sandbox_vars.push(("SANDBOX_WRITE".to_string(), root.to_string()));
            sandbox_vars.push((
                "SANDBOX_PREDICT".to_string(),
                format!("{}:/dev/null:/dev/zero", root),
            ));
            sandbox_vars.push(("SANDBOX_ON".to_string(), "1".to_string()));
        }

        // Network isolation: active when network-sandbox is set, the phase does
        // not need the network, and RESTRICT does not disable it.
        let network_isolated = self.network_sandbox
            && !network_needed
            && !self.restrict_network_sandbox
            && self.support.network;

        // Namespace selection. The namespaces are computed here but reported as
        // applied only after the runner confirms the unshare, so they are not
        // pushed into `applied_features` at plan time.
        let mut namespaces = Vec::new();
        if network_isolated {
            namespaces.push(Namespace::Network);
        }
        for (enabled, ns) in [
            (self.mount_sandbox, Namespace::Mount),
            (self.pid_sandbox, Namespace::Pid),
            (self.ipc_sandbox, Namespace::Ipc),
        ] {
            if enabled && self.support.supports(ns) {
                namespaces.push(ns);
            }
        }

        SandboxPlan {
            wrapper,
            sandbox_vars,
            namespaces,
            network_isolated,
            privilege,
            applied_features: applied,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn cfg(features: &[&str]) -> ConfigEnv {
        ConfigEnv {
            vars: BTreeMap::new(),
            features: features.iter().map(|s| s.to_string()).collect(),
            mirrors: Vec::new(),
            root: "/".into(),
            sysroot: "/".into(),
            eprefix: String::new(),
            config_root: "/".into(),
            eclass_locations: Vec::new(),
            bashrc_files: Vec::new(),
        }
    }

    fn root() -> PathBuf {
        PathBuf::from("/var/tmp/portage/dev-libs/foo-1")
    }

    #[test]
    fn sandbox_confines_writes_to_build_tree() {
        let sel = SandboxSelector::from_config(&cfg(&["sandbox"]), [], NamespaceSupport::default());
        let plan = sel.plan(PhaseKind::SrcCompile, &root(), false);
        assert!(plan.wrapper.contains(&"sandbox".to_string()));
        let write = plan
            .sandbox_vars
            .iter()
            .find(|(k, _)| k == "SANDBOX_WRITE")
            .unwrap();
        assert!(write.1.contains("foo-1"));
        assert!(plan.applied_features.contains(&"sandbox".to_string()));
    }

    #[test]
    fn install_uses_fakeroot() {
        let sel = SandboxSelector::from_config(
            &cfg(&["sandbox", "fakeroot"]),
            [],
            NamespaceSupport::default(),
        );
        let plan = sel.plan(PhaseKind::SrcInstall, &root(), false);
        assert_eq!(plan.privilege, PrivilegeMode::Fakeroot);
        assert_eq!(plan.wrapper.first().unwrap(), "fakeroot");
    }

    #[test]
    fn userpriv_selected() {
        let sel =
            SandboxSelector::from_config(&cfg(&["userpriv"]), [], NamespaceSupport::default());
        let plan = sel.plan(PhaseKind::SrcCompile, &root(), false);
        assert_eq!(plan.privilege, PrivilegeMode::UserPriv);
        // `userpriv` is reported as applied only by the runner, not at plan time.
        assert!(!plan.applied_features.contains(&"userpriv".to_string()));
    }

    #[test]
    fn network_sandbox_isolates_when_not_needed() {
        let sel = SandboxSelector::from_config(
            &cfg(&["network-sandbox"]),
            [],
            NamespaceSupport::default(),
        );
        let plan = sel.plan(PhaseKind::SrcCompile, &root(), false);
        assert!(plan.network_isolated);
        assert!(plan.namespaces.contains(&Namespace::Network));
    }

    #[test]
    fn network_needed_phase_keeps_network() {
        let sel = SandboxSelector::from_config(
            &cfg(&["network-sandbox"]),
            [],
            NamespaceSupport::default(),
        );
        let plan = sel.plan(PhaseKind::SrcUnpack, &root(), true);
        assert!(!plan.network_isolated);
        assert!(!plan.namespaces.contains(&Namespace::Network));
    }

    #[test]
    fn restrict_network_sandbox_disables_isolation() {
        let sel = SandboxSelector::from_config(
            &cfg(&["network-sandbox"]),
            ["network-sandbox"],
            NamespaceSupport::default(),
        );
        let plan = sel.plan(PhaseKind::SrcCompile, &root(), false);
        assert!(!plan.network_isolated);
    }

    #[test]
    fn falls_back_when_namespace_unavailable() {
        let support = NamespaceSupport {
            network: false,
            ..NamespaceSupport::default()
        };
        let sel = SandboxSelector::from_config(&cfg(&["network-sandbox"]), [], support);
        let plan = sel.plan(PhaseKind::SrcCompile, &root(), false);
        assert!(!plan.network_isolated);
        assert!(!plan.namespaces.contains(&Namespace::Network));
    }

    #[test]
    fn multiple_namespaces_selected() {
        let sel = SandboxSelector::from_config(
            &cfg(&["mount-sandbox", "pid-sandbox", "ipc-sandbox"]),
            [],
            NamespaceSupport::default(),
        );
        let plan = sel.plan(PhaseKind::SrcCompile, &root(), false);
        assert!(plan.namespaces.contains(&Namespace::Mount));
        assert!(plan.namespaces.contains(&Namespace::Pid));
        assert!(plan.namespaces.contains(&Namespace::Ipc));
    }
}
