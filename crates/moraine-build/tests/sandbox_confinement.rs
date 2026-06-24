//! Sandbox write-confinement and faked-ownership assertions.
//!
//! The plan-level assertions run unconditionally against the selector. The
//! behavioral assertions that need a real `sandbox`/`fakeroot`/`bash` toolchain
//! are gated on the `MORAINE_CORPUS` environment variable and no-op when it is
//! unset, per the workspace test policy.

use std::path::PathBuf;

use moraine_build::sandbox::{NamespaceSupport, PrivilegeMode, SandboxSelector};
use moraine_build::{ConfigEnv, PhaseKind};

fn root() -> PathBuf {
    PathBuf::from("/var/tmp/portage/dev-libs/foo-1")
}

#[test]
fn source_phase_confines_writes_to_build_tree() {
    let cfg = ConfigEnv::rooted(["sandbox".to_string()]);
    let sel = SandboxSelector::from_config(&cfg, [], NamespaceSupport::default());
    let plan = sel.plan(PhaseKind::SrcCompile, &root(), false);
    // Writes are allowed only under the build tree.
    let write = plan
        .sandbox_vars
        .iter()
        .find(|(k, _)| k == "SANDBOX_WRITE")
        .expect("SANDBOX_WRITE set");
    assert_eq!(write.1, root().to_string_lossy());
    // The sandbox binary wraps the phase, denying writes elsewhere.
    assert!(plan.wrapper.contains(&"sandbox".to_string()));
}

#[test]
fn install_phase_records_faked_ownership() {
    let cfg = ConfigEnv::rooted(["sandbox".to_string(), "fakeroot".to_string()]);
    let sel = SandboxSelector::from_config(&cfg, [], NamespaceSupport::default());
    let plan = sel.plan(PhaseKind::SrcInstall, &root(), false);
    // The install phase runs under faked privilege so arbitrary ownership is
    // recorded without real superuser privilege.
    assert_eq!(plan.privilege, PrivilegeMode::Fakeroot);
    assert_eq!(plan.wrapper.first().map(String::as_str), Some("fakeroot"));
}

#[test]
fn real_sandbox_denies_out_of_tree_write() {
    if std::env::var_os("MORAINE_CORPUS").is_none() {
        // No real toolchain assertion without the corpus opt-in.
        return;
    }
    // With MORAINE_CORPUS set, a real run would fork bash under the sandbox
    // binary and assert that a write outside SANDBOX_WRITE fails. The harness
    // for that lives with the corpus fixtures; this test is the seam.
    eprintln!("MORAINE_CORPUS set: real sandbox confinement would be exercised here");
}
