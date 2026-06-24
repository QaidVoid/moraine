//! Snapshot tests for the `Packages` index serialization and candidate
//! compatibility verdicts.

use moraine_binpkg::index::{PackageEntry, PackagesIndex};
use moraine_binpkg::metadata::{
    KEY_CHOST, KEY_DESCRIPTION, KEY_MTIME, KEY_REPOSITORY, KEY_REQUIRES, KEY_USE, MetadataMap,
};
use moraine_binpkg::resolution::{BinaryCandidate, TargetConfig, Verdict, check_compatibility};
use moraine_common::Interner;
use moraine_version::Version;
use std::collections::BTreeSet;

fn build_index() -> PackagesIndex {
    let mut index = PackagesIndex::new();
    index.header.insert("ARCH".into(), "amd64".into());
    index
        .header
        .insert("ACCEPT_KEYWORDS".into(), "amd64".into());
    index.header.insert("FEATURES".into(), "binpkg-logs".into());

    let mut meta = MetadataMap::new();
    meta.set_str(KEY_DESCRIPTION, "Foundational library");
    meta.set_str(KEY_REPOSITORY, "gentoo");
    meta.set_str(KEY_MTIME, "1700000000");
    meta.set_str(KEY_USE, "ssl");
    meta.set_str(KEY_CHOST, "x86_64-pc-linux-gnu");
    meta.set_str("SLOT", "0");
    meta.set_str("EAPI", "8");
    meta.set_str("BUILD_ID", "1");
    meta.set_str("BUILD_TIME", "1700000000");
    meta.set_str("SIZE", "12345");
    meta.set_str("RDEPEND", "ssl? ( dev-libs/openssl ) sys-libs/zlib");
    index.packages.push(PackageEntry {
        cpv: "dev-libs/foo-1.2.3".into(),
        metadata: meta,
    });
    index
}

#[test]
fn packages_index_serialization() {
    let interner = Interner::new();
    let text = build_index().emit(&interner);
    insta::assert_snapshot!("packages_index", text);
}

fn candidate(use_str: &str, chost: &str, requires: Option<&str>) -> BinaryCandidate {
    let mut m = MetadataMap::new();
    m.set_str(KEY_USE, use_str);
    m.set_str(KEY_CHOST, chost);
    if let Some(r) = requires {
        m.set_str(KEY_REQUIRES, r);
    }
    BinaryCandidate {
        cp: "dev-libs/foo".into(),
        version: Version::parse("1.2.3").unwrap(),
        metadata: m,
    }
}

fn target(use_flags: &[&str], chost: &str, sonames: &[(&str, &str)]) -> TargetConfig {
    TargetConfig {
        chost: chost.into(),
        selected_use: use_flags.iter().map(|s| s.to_string()).collect(),
        forced_use: BTreeSet::new(),
        masked_use: BTreeSet::new(),
        available_sonames: sonames
            .iter()
            .map(|(b, s)| (b.to_string(), s.to_string()))
            .collect(),
    }
}

#[test]
fn candidate_verdicts() {
    let cases: Vec<(&str, Verdict)> = vec![
        (
            "accept",
            check_compatibility(
                &candidate("ssl", "x86_64-pc-linux-gnu", None),
                &target(&["ssl"], "x86_64-pc-linux-gnu", &[]),
            ),
        ),
        (
            "use_mismatch",
            check_compatibility(
                &candidate("ssl", "x86_64-pc-linux-gnu", None),
                &target(&["ssl", "zlib"], "x86_64-pc-linux-gnu", &[]),
            ),
        ),
        (
            "chost_mismatch",
            check_compatibility(
                &candidate("ssl", "i686-pc-linux-gnu", None),
                &target(&["ssl"], "x86_64-pc-linux-gnu", &[]),
            ),
        ),
        (
            "soname_unsatisfied",
            check_compatibility(
                &candidate(
                    "",
                    "x86_64-pc-linux-gnu",
                    Some("x86_64: libc.so.6 libssl.so.3"),
                ),
                &target(&[], "x86_64-pc-linux-gnu", &[("x86_64", "libc.so.6")]),
            ),
        ),
    ];
    insta::assert_debug_snapshot!("candidate_verdicts", cases);
}
