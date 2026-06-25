//! Snapshot tests for serialized merge order and conflict explanations.

mod fixture;

use fixture::{Fixture, PkgSpec};
use moraine_resolve::solution::{DepClass, DepEdge, ResolvedPackage, ResolvedSolution, Root};
use moraine_resolve::{ResolveError, resolve, serialize};
use moraine_version::Version;

fn rp(cp: &str, version: &str) -> ResolvedPackage {
    ResolvedPackage {
        cp: cp.to_owned(),
        version: Version::parse(version).unwrap(),
        slot: "0".to_owned(),
        subslot: None,
        use_enabled: Default::default(),
        slot_bindings: Vec::new(),
        already_installed: false,
        subslot_rebuild: false,
    }
}

fn edge(from: &str, to: &str, class: DepClass) -> DepEdge {
    DepEdge {
        from: from.to_owned(),
        to: to.to_owned(),
        class,
        root: Root::Target,
        build_time: class.is_build_time(),
        slot_op: false,
        optional: false,
    }
}

#[test]
fn serialized_order_snapshot() {
    // A small graph with build and runtime edges and a runtime cycle.
    let solution = ResolvedSolution {
        packages: vec![
            rp("cat/app", "1"),
            rp("cat/lib", "1"),
            rp("cat/tool", "1"),
            rp("sys-libs/glibc", "2"),
        ],
        edges: vec![
            edge("cat/app", "cat/lib", DepClass::Rdepend),
            edge("cat/app", "cat/tool", DepClass::Depend),
            edge("cat/lib", "sys-libs/glibc", DepClass::Rdepend),
            edge("cat/tool", "sys-libs/glibc", DepClass::Depend),
        ],
        blockers: Vec::new(),
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let tasks = serialize(&solution).expect("serializes");
    let rendered: String = tasks
        .iter()
        .map(|t| format!("{:?} {}-{}:{}", t.kind, t.cp, t.version, t.slot))
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn required_use_explanation_snapshot() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        eapi: "8",
        required_use: "|| ( foo bar )",
        iuse: &["foo", "bar"],
        use_enabled: &[], // neither foo nor bar enabled: violates REQUIRED_USE
        ..Default::default()
    });

    let err = resolve(&f, &["cat/main"]).expect_err("violates REQUIRED_USE");
    let ResolveError::Unsatisfiable { explanation } = err else {
        panic!("expected unsatisfiable");
    };
    assert!(explanation.contains("REQUIRED_USE"));
    insta::assert_snapshot!(explanation);
}
