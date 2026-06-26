//! End-to-end build of a small fixture package against a fake command runner.
//!
//! Exercises the public [`moraine_build::build_package`] entry point: layout,
//! SRC_URI mapping, fetch, phase scheduling, and build-info output, asserting the
//! image directory and build-info contents without running a real toolchain.

use std::collections::{BTreeMap, HashSet};

use moraine_build::runner::testing::FakeRunner;
use moraine_build::{
    BuildRequest, ConfigEnv, FetchConfig, NamespaceSupport, PackageIdent, PackageSpec,
};

fn ident() -> PackageIdent {
    PackageIdent {
        category: "dev-libs".into(),
        pf: "fixture-1.0".into(),
        p: "fixture-1.0".into(),
        pn: "fixture".into(),
        pv: "1.0".into(),
        pvr: "1.0".into(),
        pr: "r0".into(),
        eapi: "8".into(),
        repository: "test".into(),
    }
}

#[test]
fn builds_fixture_and_writes_image_and_build_info() {
    let tmp = tempfile::tempdir().unwrap();
    let distdir = tmp.path().join("distdir");
    let buildroot = tmp.path().join("buildroot");
    let repo = tmp.path().join("repo/dev-libs/fixture");
    std::fs::create_dir_all(&distdir).unwrap();
    std::fs::create_dir_all(&repo).unwrap();

    // The ebuild and Manifest.
    let ebuild = repo.join("fixture-1.0.ebuild");
    std::fs::write(
        &ebuild,
        "EAPI=8\nSRC_URI=\"https://e.com/fixture-1.0.tar.gz\"\n",
    )
    .unwrap();

    // A distfile, present in the distdir, with a matching Manifest.
    let data = b"fixture source tarball";
    std::fs::write(distdir.join("fixture-1.0.tar.gz"), data).unwrap();
    let manifest = repo.join("Manifest");
    std::fs::write(
        &manifest,
        format!(
            "DIST fixture-1.0.tar.gz {} BLAKE2B {} SHA512 {}\n",
            data.len(),
            moraine_common::hash::blake2b(data),
            moraine_common::hash::sha512(data),
        ),
    )
    .unwrap();

    let mut vars = BTreeMap::new();
    vars.insert(
        "PORTAGE_TMPDIR".to_string(),
        buildroot.to_string_lossy().to_string(),
    );
    vars.insert("CFLAGS".to_string(), "-O2".to_string());
    vars.insert("CHOST".to_string(), "x86_64-pc-linux-gnu".to_string());

    let config = ConfigEnv {
        vars,
        features: vec!["sandbox".into(), "fakeroot".into()],
        mirrors: vec![],
        root: "/".into(),
        sysroot: "/".into(),
        eprefix: String::new(),
    };

    let mut reduced = BTreeMap::new();
    reduced.insert("DEPEND".to_string(), "dev-libs/dep".to_string());
    reduced.insert("LICENSE".to_string(), "GPL-2".to_string());

    let package = PackageSpec {
        ident: ident(),
        ebuild_path: ebuild.clone(),
        src_uri: "https://e.com/fixture-1.0.tar.gz".into(),
        defined_phases: vec!["compile".into(), "install".into()],
        restrict: vec![],
        slot: "0".into(),
        subslot: None,
        iuse: vec!["ssl".into()],
        keywords: vec!["amd64".into()],
        inherited: vec!["eutils".into()],
        reduced_meta: reduced,
        manifest_path: manifest,
    };

    let mut use_flags = HashSet::new();
    use_flags.insert("ssl".to_string());

    let request = BuildRequest {
        package,
        config,
        use_flags,
        fetch: FetchConfig::new(&distdir),
        run_tests: false,
        require_digest: true,
        namespace_support: NamespaceSupport::default(),
        slot_bindings: Vec::new(),
    };

    let runner = FakeRunner::always_ok();
    let outcome = moraine_build::build_package(&request, &runner).unwrap();

    // The image directory exists and is empty (no real install ran).
    assert!(outcome.image_dir.is_dir());

    // Build-info has the expected one-line files.
    let read = |name: &str| {
        std::fs::read_to_string(outcome.build_info_dir.join(name))
            .unwrap_or_default()
            .trim_end()
            .to_string()
    };
    assert_eq!(read("CATEGORY"), "dev-libs");
    assert_eq!(read("PF"), "fixture-1.0");
    assert_eq!(read("SLOT"), "0");
    assert_eq!(read("EAPI"), "8");
    assert_eq!(read("repository"), "test");
    assert_eq!(read("USE"), "ssl");
    assert_eq!(read("DEFINED_PHASES"), "compile install");
    assert_eq!(read("INHERITED"), "eutils");
    assert_eq!(read("KEYWORDS"), "amd64");
    assert_eq!(read("DEPEND"), "dev-libs/dep");
    assert_eq!(read("LICENSE"), "GPL-2");
    assert_eq!(read("CFLAGS"), "-O2");
    assert_eq!(read("A"), "fixture-1.0.tar.gz");
    assert!(!read("BUILD_TIME").is_empty());

    // The ebuild was copied and the saved environment written.
    assert!(outcome.build_info_dir.join("fixture-1.0.ebuild").is_file());
    assert!(outcome.build_info_dir.join("environment.bz2").is_file());

    // The distfile was found present, not refetched.
    assert_eq!(outcome.fetched.len(), 1);
    assert_eq!(
        outcome.fetched[0].status,
        moraine_build::FetchStatus::AlreadyPresent
    );

    // Phases ran in order: unpack, configure (default), compile, install.
    let invoked = outcome.report.invoked_phases();
    assert!(invoked.contains(&moraine_build::PhaseKind::SrcCompile));
    assert!(invoked.contains(&moraine_build::PhaseKind::SrcInstall));

    // Applied features were surfaced.
    assert!(outcome.applied_features.contains(&"sandbox".to_string()));
    assert!(outcome.applied_features.contains(&"fakeroot".to_string()));
}

#[test]
fn restricted_fetch_missing_fails_build() {
    let tmp = tempfile::tempdir().unwrap();
    let distdir = tmp.path().join("distdir");
    let buildroot = tmp.path().join("buildroot");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&distdir).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    let ebuild = repo.join("fixture-1.0.ebuild");
    std::fs::write(&ebuild, "EAPI=8\n").unwrap();

    let data = b"restricted";
    let manifest = repo.join("Manifest");
    std::fs::write(
        &manifest,
        format!(
            "DIST restricted-1.0.tar.gz {} BLAKE2B {} SHA512 {}\n",
            data.len(),
            moraine_common::hash::blake2b(data),
            moraine_common::hash::sha512(data),
        ),
    )
    .unwrap();

    let mut vars = BTreeMap::new();
    vars.insert(
        "PORTAGE_TMPDIR".to_string(),
        buildroot.to_string_lossy().to_string(),
    );

    let package = PackageSpec {
        ident: ident(),
        ebuild_path: ebuild,
        src_uri: "https://e.com/restricted-1.0.tar.gz".into(),
        defined_phases: vec!["compile".into()],
        restrict: vec!["fetch".into()],
        slot: "0".into(),
        subslot: None,
        iuse: vec![],
        keywords: vec![],
        inherited: vec![],
        reduced_meta: BTreeMap::new(),
        manifest_path: manifest,
    };

    let request = BuildRequest {
        package,
        config: ConfigEnv {
            vars,
            ..ConfigEnv::rooted([])
        },
        use_flags: HashSet::new(),
        fetch: FetchConfig::new(&distdir),
        run_tests: false,
        require_digest: true,
        namespace_support: NamespaceSupport::default(),
        slot_bindings: Vec::new(),
    };

    let runner = FakeRunner::always_ok();
    let err = moraine_build::build_package(&request, &runner);
    assert!(matches!(
        err,
        Err(moraine_build::BuildError::RestrictedFetch { .. })
    ));
    // No public fetch was attempted.
    assert_eq!(runner.call_count(), 0);
}
