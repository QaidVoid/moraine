//! End-to-end IPC: a real bash phase calling `has_version`/`best_version`
//! reaches the live [`moraine_build::IpcEndpoint`] responder over its FIFOs.
//!
//! These cover the seam the in-process unit tests cannot: the bash wrapper
//! invoking the exported `MORAINE_IPC_HELPER` client, the client relaying the
//! request to the responder, and the responder answering from a fixture
//! [`moraine_build::VersionQuery`]. The phase must succeed with the correct exit
//! status instead of dying with the unexpected helper exit code `127`. The tests
//! are skipped when `bash` is unavailable.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use moraine_build::bashlib::{self, PhaseLibrary};
use moraine_build::{IpcEndpoint, QueryRoot, VersionQuery};

/// Whether a usable bash is on PATH; the fixtures are skipped otherwise.
fn bash_available() -> bool {
    Command::new("bash")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A fixture installed set answering by `category/package` prefix, returning the
/// lexically greatest matching `cpv` as the best version.
struct FixtureStore {
    installed: Vec<String>,
}

impl FixtureStore {
    fn cp_matches<'a>(&'a self, atom: &'a str) -> impl Iterator<Item = &'a String> {
        let cp = atom
            .trim_start_matches(['>', '<', '=', '~', '!'])
            .to_owned();
        self.installed
            .iter()
            .filter(move |cpv| cpv.starts_with(&cp))
    }
}

impl VersionQuery for FixtureStore {
    fn has_version(&self, _root: QueryRoot, atom: &str, _caller_use: &[String]) -> bool {
        self.cp_matches(atom).next().is_some()
    }

    fn best_version(&self, _root: QueryRoot, atom: &str, _caller_use: &[String]) -> Option<String> {
        self.cp_matches(atom).max().cloned()
    }
}

/// Run one `pkg_setup` phase the way the driver does, with `MORAINE_IPC_HELPER`
/// exported, while the responder serves `backend` on a scoped thread. Returns
/// the combined output and whether the phase succeeded.
fn run_with_responder(backend: &FixtureStore, ebuild_body: &str) -> (String, bool) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for sub in [".ipc", "work", "temp", "image"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    let library = PhaseLibrary::materialize(root.join("bashlib")).unwrap();
    let endpoint = IpcEndpoint::create(&root.join(".ipc")).unwrap();
    let helper = endpoint.helper_path().to_string_lossy().into_owned();

    let ebuild = root.join("pkg-1.ebuild");
    std::fs::write(&ebuild, ebuild_body).unwrap();

    std::thread::scope(|scope| {
        scope.spawn(|| endpoint.serve(backend));
        let result = run_phase(&library, root, &ebuild, &helper);
        endpoint.shutdown();
        result
    })
}

/// Source the library and ebuild, bind the EAPI defaults, and dispatch
/// `pkg_setup`, with the IPC helper exported. Modeled on the bash fixture in
/// `bashlib_fixture.rs`.
fn run_phase(library: &PhaseLibrary, root: &Path, ebuild: &Path, helper: &str) -> (String, bool) {
    let w = |p: &Path| p.to_string_lossy().into_owned();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in [
        ("EAPI", "8".to_string()),
        ("CATEGORY", "dev-libs".to_string()),
        ("PF", "pkg-1".to_string()),
        ("P", "pkg-1".to_string()),
        ("PN", "pkg".to_string()),
        ("PV", "1".to_string()),
        ("PVR", "1".to_string()),
        ("PR", "r0".to_string()),
        ("SLOT", "0".to_string()),
        ("ROOT", "/".to_string()),
        ("USE", String::new()),
        ("PORTAGE_REPO_NAME", "gentoo".to_string()),
        ("WORKDIR", w(&root.join("work"))),
        ("S", w(&root.join("work"))),
        ("T", w(&root.join("temp"))),
        ("D", w(&root.join("image"))),
        ("ED", w(&root.join("image"))),
        ("CHOST", "x86_64-pc-linux-gnu".to_string()),
        ("EBUILD_PHASE", "setup".to_string()),
        ("EBUILD_PHASE_FUNC", "pkg_setup".to_string()),
        ("MORAINE_IPC_HELPER", helper.to_string()),
    ] {
        env.insert(k.to_string(), v);
    }

    let mut script = String::new();
    for lib in &library.scripts {
        script.push_str(&format!(". '{}' || exit 1\n", lib.display()));
    }
    script.push_str(&format!(
        "[ -f '{ebuild}' ] && {{ . '{ebuild}' || die src; }}\n\
         {fold}\n{bind} \"${{EAPI:-0}}\" pkg_setup\n{dispatch} pkg_setup\n",
        ebuild = ebuild.display(),
        fold = bashlib::FOLD_FUNC,
        bind = bashlib::BIND_FUNC,
        dispatch = bashlib::DISPATCH_FUNC,
    ));

    let out = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .envs(&env)
        .current_dir(root.join("work"))
        .output()
        .unwrap();
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

#[test]
fn has_version_and_best_version_answered_over_live_responder() {
    if !bash_available() {
        return;
    }
    let backend = FixtureStore {
        installed: vec!["dev-libs/foo-1.0".into(), "dev-libs/foo-2.0".into()],
    };
    let (out, ok) = run_with_responder(
        &backend,
        "EAPI=8\n\
         pkg_setup() {\n\
         \thas_version dev-libs/foo || die \"has_version should have succeeded\"\n\
         \techo \"BEST=$(best_version dev-libs/foo)\"\n\
         }\n",
    );
    assert!(ok, "phase died instead of answering the query: {out}");
    // best_version printed the highest matching cpv, captured by the substitution.
    assert!(
        out.contains("BEST=dev-libs/foo-2.0"),
        "best_version did not return the match: {out}"
    );
}

#[test]
fn absent_package_returns_one_without_dying() {
    if !bash_available() {
        return;
    }
    let backend = FixtureStore {
        installed: vec!["dev-libs/foo-1.0".into()],
    };
    let (out, ok) = run_with_responder(
        &backend,
        "EAPI=8\n\
         pkg_setup() {\n\
         \tif has_version dev-libs/absent; then echo PRESENT; else echo ABSENT; fi\n\
         \techo \"BEST=[$(best_version dev-libs/absent)]\"\n\
         }\n",
    );
    // has_version exited 1 (not a die), and best_version exited 0 with no output.
    assert!(ok, "absent query aborted the phase: {out}");
    assert!(
        out.contains("ABSENT"),
        "has_version did not report absent: {out}"
    );
    assert!(
        out.contains("BEST=[]"),
        "best_version for an absent package should be empty: {out}"
    );
}
