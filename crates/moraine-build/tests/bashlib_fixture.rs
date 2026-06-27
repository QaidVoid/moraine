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
fn unpack_tar_zst_and_standalone_xz() {
    if !bash_available() {
        return;
    }
    // Skip cleanly when the compressors are unavailable.
    let have = |p: &str| {
        Command::new(p)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !have("zstd") || !have("xz") || !have("tar") {
        return;
    }
    let fx = Fixture::new();
    let dist = fx.root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    // Build a .tar.zst whose member is a single file.
    let payload = fx.root.join("payload");
    std::fs::create_dir_all(payload.join("sub")).unwrap();
    std::fs::write(payload.join("sub/file.txt"), "hello\n").unwrap();
    let tar_zst = dist.join("pkg-data.tar.zst");
    assert!(
        Command::new("tar")
            .args(["--zstd", "-cf"])
            .arg(&tar_zst)
            .arg("-C")
            .arg(&payload)
            .arg("sub")
            .status()
            .unwrap()
            .success()
    );
    // Build a standalone .xz of a plain file.
    std::fs::write(dist.join("note"), "standalone\n").unwrap();
    assert!(
        Command::new("xz")
            .arg("-z")
            .arg(dist.join("note"))
            .status()
            .unwrap()
            .success()
    );

    let eb = fx.ebuild("EAPI=8\nsrc_unpack() { unpack pkg-data.tar.zst note.xz; }\n");
    let (out, ok) = fx.run_phase(
        "8",
        &eb,
        "src_unpack",
        &[("DISTDIR", &dist.to_string_lossy())],
    );
    assert!(ok, "unpack failed: {out}");
    let work = fx.root.join("work");
    assert!(
        work.join("sub/file.txt").is_file(),
        ".tar.zst not extracted: {out}"
    );
    assert!(work.join("note").is_file(), ".xz not extracted: {out}");
}

#[test]
fn unpack_skips_unsupported_suffix() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let dist = fx.root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("readme.txt"), "not an archive\n").unwrap();
    // unpack of a non-archive must skip and continue rather than die.
    let eb = fx.ebuild("EAPI=8\nsrc_unpack() { unpack readme.txt; echo SURVIVED; }\n");
    let (out, ok) = fx.run_phase(
        "8",
        &eb,
        "src_unpack",
        &[("DISTDIR", &dist.to_string_lossy())],
    );
    assert!(ok, "unpack of unsupported suffix aborted: {out}");
    assert!(out.contains("SURVIVED"), "phase did not continue: {out}");
}

#[test]
fn unpack_normalizes_extracted_permissions() {
    if !bash_available() {
        return;
    }
    if Command::new("tar").arg("--version").output().is_err() {
        return;
    }
    let fx = Fixture::new();
    let dist = fx.root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    let payload = fx.root.join("payload");
    std::fs::create_dir_all(&payload).unwrap();
    let restricted = payload.join("locked.txt");
    std::fs::write(&restricted, "data\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Owner-only perms: readable so tar can archive it, but no group/other
        // read for the post-unpack chmod to add.
        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let tar = dist.join("pkg.tar");
    assert!(
        Command::new("tar")
            .args(["-cf"])
            .arg(&tar)
            .arg("-C")
            .arg(&payload)
            .arg("locked.txt")
            .status()
            .unwrap()
            .success()
    );
    let eb = fx.ebuild("EAPI=8\nsrc_unpack() { unpack pkg.tar; }\n");
    let (out, ok) = fx.run_phase(
        "8",
        &eb,
        "src_unpack",
        &[("DISTDIR", &dist.to_string_lossy())],
    );
    assert!(ok, "unpack failed: {out}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(fx.root.join("work/locked.txt"))
            .unwrap()
            .permissions()
            .mode();
        // The post-unpack chmod (a+rX) adds group and other read bits.
        assert_eq!(mode & 0o044, 0o044, "read bits not added: {mode:o}");
    }
}

#[test]
fn dosym_r_writes_relative_target() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let eb = fx.ebuild("EAPI=8\nsrc_install() { dosym -r /usr/lib/foo.so /usr/bin/foo.so; }\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_install", &[]);
    assert!(ok, "{out}");
    let link = fx.root.join("image/usr/bin/foo.so");
    let target = std::fs::read_link(&link).unwrap();
    assert_eq!(
        target,
        std::path::PathBuf::from("../lib/foo.so"),
        "dosym -r did not produce a relative target: {out}"
    );
}

#[test]
fn doman_routes_localized_page() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let work = fx.root.join("work");
    std::fs::write(work.join("foo.de.1"), "german man page\n").unwrap();
    let eb = fx.ebuild("EAPI=8\nsrc_install() { doman foo.de.1; }\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_install", &[]);
    assert!(ok, "{out}");
    assert!(
        fx.root.join("image/usr/share/man/de/man1/foo.1").is_file(),
        "localized man page not routed to de/man1/foo.1: {out}"
    );
}

#[test]
fn domo_honors_moprefix() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let work = fx.root.join("work");
    std::fs::write(work.join("de.mo"), "catalog\n").unwrap();
    let eb = fx.ebuild("EAPI=8\nsrc_install() { MOPREFIX=myapp domo de.mo; }\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_install", &[]);
    assert!(ok, "{out}");
    assert!(
        fx.root
            .join("image/usr/share/locale/de/LC_MESSAGES/myapp.mo")
            .is_file(),
        "domo did not use MOPREFIX for the catalog name: {out}"
    );
}

#[test]
fn doins_recursive_normalizes_mode() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let work = fx.root.join("work");
    let tree = work.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    let exec = tree.join("script");
    std::fs::write(&exec, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exec, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let eb = fx.ebuild("EAPI=8\nsrc_install() { insinto /opt; doins -r tree; }\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_install", &[]);
    assert!(ok, "{out}");
    let installed = fx.root.join("image/opt/tree/script");
    assert!(
        installed.is_file(),
        "recursive doins did not install: {out}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&installed).unwrap().permissions().mode() & 0o777;
        // INSOPTIONS default normalizes to 0644, not the source 0755.
        assert_eq!(mode, 0o644, "mode not normalized: {mode:o}");
    }
}

#[test]
fn fperms_routes_options_and_symbolic_mode() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let image = fx.root.join("image");
    let tree = image.join("some/dir");
    std::fs::create_dir_all(&tree).unwrap();
    let inner = tree.join("file");
    std::fs::write(&inner, "data\n").unwrap();
    let loose = image.join("some/loose");
    std::fs::write(&loose, "data\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // `-R` must be forwarded as an option to chmod, `0755` taken as the mode, and
    // only the path prefixed; `-x` must be treated as the mode, not an option.
    let eb =
        fx.ebuild("EAPI=8\nsrc_install() { fperms -R 0755 some/dir; fperms -x some/loose; }\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_install", &[]);
    assert!(ok, "fperms aborted: {out}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = std::fs::metadata(&tree).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&inner).unwrap().permissions().mode() & 0o777;
        let loose_mode = std::fs::metadata(&loose).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o755, "recursive dir mode wrong: {dir_mode:o}");
        assert_eq!(file_mode, 0o755, "recursive file mode wrong: {file_mode:o}");
        assert_eq!(loose_mode, 0o644, "-x not applied as mode: {loose_mode:o}");
    }
}

#[test]
fn into_slash_routes_dobin_to_image_root() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let work = fx.root.join("work");
    std::fs::write(work.join("foo"), "#!/bin/sh\n").unwrap();
    std::fs::write(work.join("bar"), "#!/bin/sh\n").unwrap();

    // `into /` routes dobin to the image root rather than collapsing back to /usr.
    let eb = fx.ebuild("EAPI=8\nsrc_install() { into /; dobin foo; }\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_install", &[]);
    assert!(ok, "dobin after into / aborted: {out}");
    assert!(
        fx.root.join("image/bin/foo").is_file(),
        "into /; dobin did not land foo at image/bin/foo: {out}"
    );

    // A fresh phase without `into` defaults the destination tree back to /usr.
    let eb = fx.ebuild("EAPI=8\nsrc_install() { dobin bar; }\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_install", &[]);
    assert!(ok, "dobin without into aborted: {out}");
    assert!(
        fx.root.join("image/usr/bin/bar").is_file(),
        "dobin without into did not default to image/usr/bin/bar: {out}"
    );
}

#[test]
fn dolib_preserves_symlink_source() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let work = fx.root.join("work");
    std::fs::write(work.join("libfoo.so.1"), "lib\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("libfoo.so.1", work.join("libfoo.so")).unwrap();
    let eb = fx.ebuild("EAPI=8\nsrc_install() { dolib.so libfoo.so.1 libfoo.so; }\n");
    let (out, ok) = fx.run_phase("8", &eb, "src_install", &[]);
    assert!(ok, "dolib.so aborted: {out}");
    let libdir = fx.root.join("image/usr/lib");
    assert!(
        libdir.join("libfoo.so.1").is_file(),
        "regular library not installed: {out}"
    );
    let link = libdir.join("libfoo.so");
    assert!(
        link.symlink_metadata().unwrap().file_type().is_symlink(),
        "symlink source was dereferenced into a regular file: {out}"
    );
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        std::path::PathBuf::from("libfoo.so.1"),
        "symlink target not preserved: {out}"
    );
}

#[test]
fn fowners_resolves_owner_against_target_root() {
    if !bash_available() {
        return;
    }
    let fx = Fixture::new();
    let target = fx.root.join("target");
    std::fs::create_dir_all(target.join("etc")).unwrap();
    std::fs::write(
        target.join("etc/passwd"),
        "root:x:0:0:root:/root:/bin/bash\nmessagebus:x:101:102:System Message Bus:/dev/null:/sbin/nologin\n",
    )
    .unwrap();
    std::fs::write(target.join("etc/group"), "root:x:0:\nmessagebus:x:102:\n").unwrap();
    let t = target.to_string_lossy().into_owned();

    // A non-root target resolves the symbolic owner to numeric uid:gid from the
    // target passwd/group. A fake chown captures the resolved owner so the test
    // does not need privileges to change ownership to root.
    let eb = fx.ebuild(
        "EAPI=8\nchown() { echo \"CHOWN:$*\"; }\nsrc_install() { fowners messagebus:messagebus usr/bin/foo; }\n",
    );
    let (out, ok) = fx.run_phase(
        "8",
        &eb,
        "src_install",
        &[
            ("ROOT", &t),
            ("SYSROOT", &t),
            ("ESYSROOT", &t),
            ("EROOT", &t),
        ],
    );
    assert!(ok, "cross-root fowners aborted: {out}");
    assert!(
        out.contains("CHOWN:101:102"),
        "owner not resolved to numeric uid:gid: {out}"
    );

    // With ROOT=/ (unset gating root) the owner passes straight through.
    let eb = fx.ebuild(
        "EAPI=8\nchown() { echo \"CHOWN:$*\"; }\nsrc_install() { fowners root:root usr/bin/foo; }\n",
    );
    let (out, ok) = fx.run_phase("8", &eb, "src_install", &[]);
    assert!(ok, "default-root fowners aborted: {out}");
    assert!(
        out.contains("CHOWN:root:root"),
        "owner not passed through for ROOT=/: {out}"
    );
}

#[test]
fn corpus_helper_fidelity_end_to_end() {
    if std::env::var_os("MORAINE_CORPUS").is_none() {
        // No real end-to-end build without the corpus opt-in.
        return;
    }
    // With MORAINE_CORPUS set, a real build of a package shipping a .info
    // manual, a localized man page, and a message catalog would assert those
    // land in the image and that a dostrip -x exclusion is observed by the
    // strip stage. The corpus harness drives that; this test is the seam.
    eprintln!("MORAINE_CORPUS set: real helper-fidelity end-to-end would run here");
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
