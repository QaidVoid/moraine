//! Bash fixture tests for the vendored phase library.
//!
//! These drive a real `bash` against the materialized library and a fake eclass
//! tree, asserting the behavior the Rust unit tests cannot reach: `inherit`
//! sourcing and its die-on-missing, `EXPORT_FUNCTIONS` precedence, `E_*`/
//! `INHERITED` accumulation, `econf`'s mandatory and EAPI-conditional arguments,
//! the `eapply` `--` rule and directory expansion, `einstalldocs`, the bare
//! `default` command, and `nonfatal`/`die -n`. The tests are skipped when `bash`
//! is unavailable.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use moraine_build::bashlib::{self, PhaseLibrary};

/// Whether a usable bash is on PATH; the fixtures are skipped otherwise.
fn bash_available() -> bool {
    Command::new("bash")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A materialized library plus a fake repository/eclass tree under a tempdir.
struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    repo: std::path::PathBuf,
    library: PhaseLibrary,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("eclass")).unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        std::fs::create_dir_all(root.join("image")).unwrap();
        std::fs::create_dir_all(root.join("temp")).unwrap();
        let library = PhaseLibrary::materialize(root.join("bashlib")).unwrap();
        Fixture {
            _tmp: tmp,
            root,
            repo,
            library,
        }
    }

    fn eclass(&self, name: &str, body: &str) {
        std::fs::write(
            self.repo.join("eclass").join(format!("{name}.eclass")),
            body,
        )
        .unwrap();
    }

    fn ebuild(&self, body: &str) -> std::path::PathBuf {
        let path = self.root.join("pkg-1.ebuild");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn base_env(&self, eapi: &str) -> BTreeMap<String, String> {
        let w = |p: &Path| p.to_string_lossy().into_owned();
        let mut env = BTreeMap::new();
        for (k, v) in [
            ("EAPI", eapi.to_string()),
            ("CATEGORY", "dev-libs".to_string()),
            ("PF", "pkg-1".to_string()),
            ("P", "pkg-1".to_string()),
            ("PN", "pkg".to_string()),
            ("PV", "1".to_string()),
            ("PVR", "1".to_string()),
            ("PR", "r0".to_string()),
            ("SLOT", "0".to_string()),
            ("PORTAGE_REPO_NAME", "gentoo".to_string()),
            ("WORKDIR", w(&self.root.join("work"))),
            ("S", w(&self.root.join("work"))),
            ("T", w(&self.root.join("temp"))),
            ("D", w(&self.root.join("image"))),
            ("ED", w(&self.root.join("image"))),
            ("CHOST", "x86_64-pc-linux-gnu".to_string()),
            ("PORTAGE_ECLASS_LOCATIONS", format!("'{}'", w(&self.repo))),
        ] {
            env.insert(k.to_string(), v);
        }
        env
    }

    /// Run one phase the way the driver does: source the library, source the
    /// ebuild, fold, bind, and dispatch. Returns (stdout, success).
    fn run_phase(
        &self,
        eapi: &str,
        ebuild: &Path,
        phase_func: &str,
        extra_env: &[(&str, &str)],
    ) -> (String, bool) {
        let mut env = self.base_env(eapi);
        for (k, v) in extra_env {
            env.insert((*k).to_string(), (*v).to_string());
        }
        let short = phase_func
            .strip_prefix("src_")
            .or_else(|| phase_func.strip_prefix("pkg_"))
            .unwrap_or(phase_func);
        env.insert("EBUILD_PHASE".to_string(), short.to_string());
        env.insert("EBUILD_PHASE_FUNC".to_string(), phase_func.to_string());
        let mut script = String::new();
        for lib in &self.library.scripts {
            script.push_str(&format!(". '{}' || exit 1\n", lib.display()));
        }
        script.push_str(&format!(
            "[ -f '{ebuild}' ] && {{ . '{ebuild}' || die src; }}\n\
             {fold}\n{bind} \"${{EAPI:-0}}\" {func}\n",
            ebuild = ebuild.display(),
            fold = bashlib::FOLD_FUNC,
            bind = bashlib::BIND_FUNC,
            func = phase_func,
        ));
        if matches!(
            short,
            "prepare" | "configure" | "compile" | "test" | "install"
        ) {
            script.push_str(&format!("__cd_to_s {short}\n"));
        }
        script.push_str(&format!("{} {phase_func}\n", bashlib::DISPATCH_FUNC));
        self.bash(&script, &env)
    }

    fn bash(&self, script: &str, env: &BTreeMap<String, String>) -> (String, bool) {
        let out = Command::new("bash")
            .arg("-c")
            .arg(script)
            .envs(env)
            .current_dir(self.root.join("work"))
            .output()
            .unwrap();
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        (text, out.status.success())
    }
}

#[test]
fn inherit_sources_eclass_and_exports_function() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    fx.eclass(
        "foo",
        "foo_src_compile() { echo FOO_COMPILE; }\nEXPORT_FUNCTIONS src_compile\n",
    );
    let eb = fx.ebuild("EAPI=8\ninherit foo\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_compile", &[]);
    assert!(ok, "phase failed: {out}");
    assert!(
        out.contains("FOO_COMPILE"),
        "eclass function did not run: {out}"
    );
}

#[test]
fn inherit_dies_when_eclass_missing() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let eb = fx.ebuild("EAPI=8\ninherit nonexistent\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_compile", &[]);
    assert!(!ok, "expected failure sourcing a missing eclass: {out}");
    assert!(out.contains("could not be found"), "no die message: {out}");
}

#[test]
fn export_functions_precedence_ebuild_wins() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    fx.eclass(
        "foo",
        "foo_src_compile() { echo FROM_ECLASS; }\nEXPORT_FUNCTIONS src_compile\n",
    );
    // The ebuild defines src_compile itself, overriding the eclass stub.
    let eb = fx.ebuild("EAPI=8\ninherit foo\nsrc_compile() { echo FROM_EBUILD; }\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_compile", &[]);
    assert!(ok, "{out}");
    assert!(out.contains("FROM_EBUILD"), "{out}");
    assert!(!out.contains("FROM_ECLASS"), "{out}");
}

#[test]
fn e_metadata_and_inherited_accumulate() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    fx.eclass("foo", "IUSE=\"ssl\"\nDEPEND=\"dev-libs/libfoo\"\n");
    let eb = fx.ebuild("EAPI=8\ninherit foo\nIUSE=\"threads\"\n");
    // Source and emit metadata directly.
    let mut script = String::new();
    for lib in &fx.library.scripts {
        script.push_str(&format!(". '{}' || exit 1\n", lib.display()));
    }
    script.push_str(&format!(". '{}'\n__emit_metadata\n", eb.display()));
    let mut env = fx.base_env("8");
    env.insert("EBUILD_PHASE".to_string(), "depend".to_string());
    let (out, ok) = fx.bash(&script, &env);
    assert!(ok, "{out}");
    assert!(out.contains("MORAINE_META IUSE=threads ssl"), "{out}");
    assert!(out.contains("MORAINE_META DEPEND=dev-libs/libfoo"), "{out}");
    assert!(out.contains("MORAINE_META INHERITED=foo"), "{out}");
}

#[test]
fn econf_passes_mandatory_arguments() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    // A fake configure that records its arguments.
    let configure = fx.root.join("work/configure");
    std::fs::write(
        &configure,
        "#!/usr/bin/env bash\nfor a in \"$@\"; do echo \"ARG:$a\"; done\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&configure, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let eb = fx.ebuild("EAPI=8\nsrc_configure() { econf; }\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_configure", &[]);
    assert!(ok, "{out}");
    for expected in [
        "ARG:--prefix=/usr",
        "ARG:--mandir=/usr/share/man",
        "ARG:--infodir=/usr/share/info",
        "ARG:--datadir=/usr/share",
        "ARG:--sysconfdir=/etc",
        "ARG:--localstatedir=/var/lib",
        "ARG:--host=x86_64-pc-linux-gnu",
    ] {
        assert!(out.contains(expected), "missing {expected} in: {out}");
    }
}

#[test]
fn eapply_applies_directory_and_dashdash() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let work = fx.root.join("work");
    std::fs::write(work.join("file.txt"), "a\nb\nc\n").unwrap();
    let patches = work.join("patches");
    std::fs::create_dir_all(&patches).unwrap();
    std::fs::write(
        patches.join("01.patch"),
        "--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n",
    )
    .unwrap();
    // default_src_prepare for EAPI 8 applies PATCHES with the `--` rule.
    let eb = fx.ebuild("EAPI=8\nPATCHES=( \"${WORKDIR}/patches\" )\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_prepare", &[]);
    assert!(ok, "{out}");
    let patched = std::fs::read_to_string(work.join("file.txt")).unwrap();
    assert_eq!(patched, "a\nB\nc\n", "patch not applied: {out}");
}

#[test]
fn einstalldocs_installs_declared_docs() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let work = fx.root.join("work");
    std::fs::write(work.join("README"), "readme\n").unwrap();
    std::fs::write(work.join("GUIDE.txt"), "guide\n").unwrap();
    let eb = fx.ebuild("EAPI=8\nDOCS=( README GUIDE.txt )\nsrc_install() { einstalldocs; }\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_install", &[]);
    assert!(ok, "{out}");
    let docdir = fx.root.join("image/usr/share/doc/pkg-1");
    assert!(
        docdir.join("README").is_file(),
        "README not installed: {out}"
    );
    assert!(
        docdir.join("GUIDE.txt").is_file(),
        "GUIDE not installed: {out}"
    );
}

#[test]
fn bare_default_runs_eapi_default() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let configure = fx.root.join("work/configure");
    std::fs::write(&configure, "#!/usr/bin/env bash\necho CONFIGURE_RAN\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&configure, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // src_configure calls the bare `default`, which runs default_src_configure
    // (econf) for EAPI 8.
    let eb = fx.ebuild("EAPI=8\nsrc_configure() { default; }\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_configure", &[]);
    assert!(ok, "{out}");
    assert!(
        out.contains("CONFIGURE_RAN"),
        "default did not run econf: {out}"
    );
}

#[test]
fn nonfatal_and_die_n_do_not_abort() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let mut script = String::new();
    for lib in &fx.library.scripts {
        script.push_str(&format!(". '{}' || exit 1\n", lib.display()));
    }
    // nonfatal lets a helper failure return instead of dying; die -n with
    // PORTAGE_NONFATAL returns rather than aborting.
    script.push_str(
        "nonfatal false; echo \"after_nonfatal=$?\"\n\
         PORTAGE_NONFATAL=1 die -n msg; echo \"after_die_n=$?\"\n\
         echo REACHED_END\n",
    );
    let env = fx.base_env("8");
    let (out, ok) = fx.bash(&script, &env);
    assert!(ok, "script aborted: {out}");
    assert!(
        out.contains("REACHED_END"),
        "die -n aborted the script: {out}"
    );
}
