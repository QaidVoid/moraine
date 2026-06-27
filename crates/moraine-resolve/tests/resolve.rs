//! Integration tests for Gentoo resolution against in-memory fixtures.

mod fixture;

use fixture::{Fixture, PkgSpec, installed};
use moraine_resolve::solution::{DepClass, Root};
use moraine_resolve::{
    AutounmaskPolicy, Modifiers, ResolveError, UseChange, resolve, resolve_with,
};

fn pkg(cp: &'static str, version: &'static str) -> PkgSpec {
    PkgSpec {
        cp,
        version,
        ..Default::default()
    }
}

#[test]
fn version_operator_restricts_candidates() {
    let mut f = Fixture::new();
    f.add(pkg("cat/a", "1"));
    f.add(pkg("cat/a", "2"));
    f.add(pkg("cat/a", "3"));

    let sol = resolve(&f, &[">=cat/a-2"]).expect("resolves");
    let a = sol.package("cat/a").expect("a selected");
    // Best-first ranking prefers the highest visible version satisfying >=2.
    assert_eq!(a.version.as_str(), "3");
}

#[test]
fn masked_candidate_is_excluded() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/a",
        version: "2",
        visible: false,
        ..Default::default()
    });
    f.add(pkg("cat/a", "1"));

    let sol = resolve(&f, &["cat/a"]).expect("resolves");
    // The masked version 2 must not be chosen; only visible 1 remains.
    assert_eq!(sol.package("cat/a").unwrap().version.as_str(), "1");
}

#[test]
fn installed_slot_match_preferred_over_higher_unrelated() {
    let mut f = Fixture::new();
    // Two slots: slot 1 has version 1, slot 2 has version 2 (higher).
    f.add(PkgSpec {
        cp: "cat/a",
        version: "1",
        slot: "1",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/a",
        version: "2",
        slot: "2",
        ..Default::default()
    });
    // Installed in slot 1.
    f.add_installed(installed("cat/a", "1", "1", None, &[]));

    let sol = resolve(&f, &["cat/a"]).expect("resolves");
    // The installed-slot match (slot 1, version 1) is preferred over the higher
    // unrelated version in slot 2.
    assert_eq!(sol.package("cat/a").unwrap().slot, "1");
}

#[test]
fn dependency_classes_get_correct_roots() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        eapi: "8",
        bdepend: "cat/buildtool",
        depend: "cat/header",
        rdepend: "cat/lib",
        pdepend: "cat/post",
        idepend: "cat/installer",
        ..Default::default()
    });
    for dep in ["buildtool", "header", "lib", "post", "installer"] {
        f.add(pkg(Box::leak(format!("cat/{dep}").into_boxed_str()), "1"));
    }

    let sol = resolve(&f, &["cat/main"]).expect("resolves");

    let edge = |to: &str| {
        sol.edges
            .iter()
            .find(|e| moraine_resolve::endpoint_cp(&e.to) == to)
            .unwrap()
            .clone()
    };

    let bd = edge("cat/buildtool");
    assert_eq!(bd.class, DepClass::Bdepend);
    assert_eq!(bd.root, Root::BuildHost);
    assert!(bd.build_time);

    // EAPI 8 has bdepend, so DEPEND targets the sysroot.
    let dep = edge("cat/header");
    assert_eq!(dep.class, DepClass::Depend);
    assert_eq!(dep.root, Root::TargetSysroot);
    assert!(dep.build_time);

    for (to, class) in [
        ("cat/lib", DepClass::Rdepend),
        ("cat/post", DepClass::Pdepend),
        ("cat/installer", DepClass::Idepend),
    ] {
        let e = edge(to);
        assert_eq!(e.class, class);
        assert_eq!(e.root, Root::Target);
        assert!(!e.build_time);
    }
}

#[test]
fn depend_root_without_bdepend_is_running_root() {
    let mut f = Fixture::new();
    // EAPI 6 has no bdepend; DEPEND resolves against the running (build-host)
    // root.
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        eapi: "6",
        depend: "cat/header",
        ..Default::default()
    });
    f.add(pkg("cat/header", "1"));

    let sol = resolve(&f, &["cat/main"]).expect("resolves");
    let dep = sol
        .edges
        .iter()
        .find(|e| moraine_resolve::endpoint_cp(&e.to) == "cat/header")
        .unwrap();
    assert_eq!(dep.class, DepClass::Depend);
    assert_eq!(dep.root, Root::BuildHost);
}

#[test]
fn use_conditional_branch_only_contributes_when_live() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "foo? ( cat/dep ) !bar? ( cat/other )",
        iuse: &["foo", "bar"],
        use_enabled: &["foo"], // foo on, bar off
        ..Default::default()
    });
    f.add(pkg("cat/dep", "1"));
    f.add(pkg("cat/other", "1"));

    let sol = resolve(&f, &["cat/main"]).expect("resolves");
    // foo enabled -> cat/dep present; bar disabled -> !bar? live -> cat/other.
    assert!(sol.package("cat/dep").is_some());
    assert!(sol.package("cat/other").is_some());
}

#[test]
fn use_conditional_disabled_branch_absent() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "foo? ( cat/dep )",
        iuse: &["foo"],
        use_enabled: &[], // foo off
        ..Default::default()
    });
    f.add(pkg("cat/dep", "1"));

    let sol = resolve(&f, &["cat/main"]).expect("resolves");
    assert!(sol.package("cat/dep").is_none());
}

#[test]
fn use_dependency_atom_requires_flag() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "cat/dep[foo]",
        ..Default::default()
    });
    // The only candidate declares foo but has it off. USE autounmask proposes
    // enabling the settable flag, so resolution now succeeds with the change.
    f.add(PkgSpec {
        cp: "cat/dep",
        version: "1",
        iuse: &["foo"],
        use_enabled: &[], // foo off: satisfied by a proposed USE change
        ..Default::default()
    });

    let sol = resolve(&f, &["cat/main"]).expect("resolves via USE autounmask");
    let change = sol
        .autounmask
        .iter()
        .find(|c| c.cp == "cat/dep")
        .expect("a USE change is proposed");
    assert_eq!(
        change.change.use_changes,
        vec![UseChange {
            flag: "foo".to_owned(),
            enable: true,
        }]
    );

    // Now with foo enabled it resolves.
    let mut f2 = Fixture::new();
    f2.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "cat/dep[foo]",
        ..Default::default()
    });
    f2.add(PkgSpec {
        cp: "cat/dep",
        version: "1",
        iuse: &["foo"],
        use_enabled: &["foo"],
        ..Default::default()
    });
    let sol = resolve(&f2, &["cat/main"]).expect("resolves");
    assert!(sol.package("cat/dep").is_some());
}

#[test]
fn use_dependency_default_applies_when_absent() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        eapi: "8",
        rdepend: "cat/dep[foo(+)]",
        ..Default::default()
    });
    // Candidate's IUSE does not list foo; (+) treats it as enabled.
    f.add(PkgSpec {
        cp: "cat/dep",
        version: "1",
        iuse: &[],
        use_enabled: &[],
        ..Default::default()
    });
    let sol = resolve(&f, &["cat/main"]).expect("resolves with (+) default");
    assert!(sol.package("cat/dep").is_some());
}

#[test]
fn any_of_branch_pulls_in_all_atoms() {
    // `|| ( ( a b ) c )`: the chosen branch is a conjunction, so selecting it
    // pulls in both a and b, not just the first atom.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "|| ( ( cat/a cat/b ) cat/c )",
        ..Default::default()
    });
    f.add(pkg("cat/a", "1"));
    f.add(pkg("cat/b", "1"));
    f.add(pkg("cat/c", "1"));

    let sol = resolve(&f, &["cat/main"]).expect("resolves");
    assert!(sol.package("cat/a").is_some(), "a pulled in");
    assert!(
        sol.package("cat/b").is_some(),
        "b pulled in (the whole branch is a conjunction)"
    );
}

#[test]
fn any_of_branch_blocker_is_asserted() {
    // A chosen `||` branch carrying a blocker asserts it: `|| ( ( cat/a !cat/bad ) )`
    // pulls in cat/a and blocks cat/bad.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "|| ( ( cat/a !cat/bad ) )",
        ..Default::default()
    });
    f.add(pkg("cat/a", "1"));
    f.add(pkg("cat/bad", "1"));
    f.add_installed(installed("cat/bad", "1", "0", None, &[]));

    let sol = resolve(&f, &["cat/main"]).expect("resolves");
    assert!(sol.package("cat/a").is_some());
    let victims: Vec<_> = sol.blockers.iter().flat_map(|b| &b.victims).collect();
    assert!(
        victims.iter().any(|v| v.cp == "cat/bad"),
        "the branch's blocker schedules cat/bad's unmerge: {victims:?}"
    );
}

#[test]
fn any_of_group_falls_back() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "|| ( cat/missing cat/present )",
        ..Default::default()
    });
    // cat/missing has no candidates; cat/present does.
    f.add(pkg("cat/present", "1"));

    let sol = resolve(&f, &["cat/main"]).expect("resolves via second branch");
    assert!(sol.package("cat/present").is_some());
}

#[test]
fn required_use_violation_is_reported() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        eapi: "8",
        required_use: "foo",
        iuse: &["foo"],
        use_enabled: &[], // foo required but disabled
        ..Default::default()
    });

    let r = resolve(&f, &["cat/main"]);
    match r {
        Err(ResolveError::Unsatisfiable { explanation }) => {
            assert!(
                explanation.contains("REQUIRED_USE"),
                "explanation should name REQUIRED_USE:\n{explanation}"
            );
        }
        other => panic!("expected unsatisfiable, got {other:?}"),
    }
}

#[test]
fn weak_blocker_of_required_package_conflicts() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "cat/dep !cat/foo",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/dep",
        version: "1",
        rdepend: "cat/foo",
        ..Default::default()
    });
    f.add(pkg("cat/foo", "1"));

    // main blocks cat/foo, but cat/dep requires it: they cannot coexist at the
    // end state, so the request is unsatisfiable.
    let r = resolve(&f, &["cat/main"]);
    assert!(matches!(r, Err(ResolveError::Unsatisfiable { .. })));
}

#[test]
fn weak_blocker_resolved_by_unmerge() {
    // A weak blocker against an installed package that nothing else needs lets
    // the parent install while the installed package is scheduled for unmerge,
    // rather than failing as a hard conflict.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "!cat/foo",
        ..Default::default()
    });
    f.add(pkg("cat/foo", "1"));
    f.add_installed(installed("cat/foo", "1", "0", None, &[]));

    let sol = resolve(&f, &["cat/main"]).expect("resolves");
    assert!(sol.package("cat/main").is_some());
    assert!(
        sol.package("cat/foo").is_none(),
        "blocked foo is not installed"
    );
    let victims: Vec<_> = sol.blockers.iter().flat_map(|b| &b.victims).collect();
    assert!(
        victims
            .iter()
            .any(|v| v.cp == "cat/foo" && v.version.as_str() == "1"),
        "the installed foo is scheduled for unmerge: {victims:?}"
    );
}

#[test]
fn self_block_does_not_block_same_slot_replacement() {
    // A package whose own dep string blocks an older version of itself in the
    // same slot must still install: the same-slot install is a replacement, not
    // a coexistence, so the self-block does not apply to the parent's own slot.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/foo",
        version: "2",
        slot: "0",
        rdepend: "!!=cat/foo-1",
        ..Default::default()
    });
    f.add_installed(installed("cat/foo", "1", "0", None, &[]));

    let sol = resolve(&f, &["cat/foo"]).expect("resolves: same-slot replacement");
    assert_eq!(sol.package("cat/foo").unwrap().version.as_str(), "2");
}

#[test]
fn strong_blocker_requires_eapi_support() {
    let mut f = Fixture::new();
    // EAPI 0 has no strong blockers.
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        eapi: "0",
        rdepend: "!!cat/foo",
        ..Default::default()
    });
    f.add(pkg("cat/foo", "1"));

    let r = resolve(&f, &["cat/main"]);
    // The version is unavailable due to invalid strong blocker, so the request
    // cannot be satisfied.
    assert!(matches!(r, Err(ResolveError::Unsatisfiable { .. })));
}

#[test]
fn package_provided_satisfies_without_install() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "cat/provided",
        ..Default::default()
    });
    f.add(pkg("cat/provided", "1"));
    f.add_provided("cat/provided", "1");

    let sol = resolve(&f, &["cat/main"]).expect("resolves");
    // The provided package is not added to the install set.
    assert!(sol.package("cat/provided").is_none());
    assert!(sol.package("cat/main").is_some());
}

#[test]
fn any_of_falls_back_to_a_branch_that_resolves() {
    // The leftmost `||` branch (cat/a) is individually satisfiable but forces
    // cat/lib-1, which conflicts with cat/main's hard =cat/lib-2. Only the cat/b
    // branch is consistent. The resolver must switch branches rather than report
    // the request unsatisfiable.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        depend: "=cat/lib-2",
        rdepend: "|| ( cat/a cat/b )",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/a",
        version: "1",
        depend: "=cat/lib-1",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/b",
        version: "1",
        depend: "=cat/lib-2",
        ..Default::default()
    });
    f.add(pkg("cat/lib", "1"));
    f.add(pkg("cat/lib", "2"));

    let sol = resolve(&f, &["cat/main"]).expect("resolves to the consistent branch");
    assert!(
        sol.package("cat/b").is_some(),
        "the cat/b branch must be selected: {:?}",
        sol.packages
    );
    assert!(
        sol.package("cat/a").is_none(),
        "the conflicting cat/a branch must not be selected"
    );
    // cat/lib-2 (not cat/lib-1) is the consistent choice.
    let lib = sol.package("cat/lib").expect("cat/lib selected");
    assert_eq!(lib.version.as_str(), "2");
}

#[test]
fn virtual_expands_to_provider() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "virtual/foo",
        ..Default::default()
    });
    // Two virtual versions; highest preferred. Each RDEPENDs a provider.
    f.add(PkgSpec {
        cp: "virtual/foo",
        version: "1",
        rdepend: "cat/provider-a",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "virtual/foo",
        version: "2",
        rdepend: "cat/provider-b",
        ..Default::default()
    });
    f.add(pkg("cat/provider-a", "1"));
    f.add(pkg("cat/provider-b", "1"));

    let sol = resolve(&f, &["cat/main"]).expect("resolves");
    // A provider must be selected.
    assert!(
        sol.package("cat/provider-a").is_some() || sol.package("cat/provider-b").is_some(),
        "a virtual provider should be installed"
    );
    // The virtual package node itself is retained in the solution (GLEP 37), not
    // flattened away.
    assert!(
        sol.package("virtual/foo").is_some(),
        "the virtual node is retained in the install set"
    );
}

#[test]
fn virtual_expansion_offers_only_the_selected_version_providers() {
    // The highest visible virtual (x-2) endorses prov-a or prov-c; the lower
    // virtual (x-1) endorses prov-a or prov-b. Expansion must offer only the
    // selected (highest) virtual's providers, so prov-b is never reachable.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "virtual/x",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "virtual/x",
        version: "2",
        rdepend: "|| ( cat/prov-a cat/prov-c )",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "virtual/x",
        version: "1",
        rdepend: "|| ( cat/prov-a cat/prov-b )",
        ..Default::default()
    });
    // prov-a is absent, so a correct expansion of x-2 must fall to prov-c. If the
    // lower virtual's providers leaked in, the solver could instead pick prov-b.
    f.add(pkg("cat/prov-b", "1"));
    f.add(pkg("cat/prov-c", "1"));

    let sol = resolve(&f, &["cat/main"]).expect("resolves");
    assert!(
        sol.package("cat/prov-c").is_some(),
        "the selected virtual's provider prov-c should be installed"
    );
    assert!(
        sol.package("cat/prov-b").is_none(),
        "a lower virtual version's provider must never be offered"
    );
}

#[test]
fn slot_operator_equal_records_binding() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        eapi: "8",
        rdepend: "cat/lib:=",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/lib",
        version: "1",
        slot: "2",
        subslot: Some("2.1"),
        ..Default::default()
    });

    let sol = resolve(&f, &["cat/main"]).expect("resolves");
    let main = sol.package("cat/main").unwrap();
    let binding = main
        .slot_bindings
        .iter()
        .find(|b| b.dependency == "cat/lib")
        .expect("binding recorded");
    assert_eq!(binding.slot, "2");
    assert_eq!(binding.subslot.as_deref(), Some("2.1"));
}

#[test]
fn exact_slot_atom_restricts_candidates() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        eapi: "8",
        rdepend: "cat/lib:2",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/lib",
        version: "1",
        slot: "1",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/lib",
        version: "2",
        slot: "2",
        ..Default::default()
    });

    let sol = resolve(&f, &["cat/main"]).expect("resolves");
    // Only the slot-2 candidate satisfies cat/lib:2.
    assert_eq!(sol.package("cat/lib").unwrap().slot, "2");
}

#[test]
fn unsolvable_blocker_names_packages() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        rdepend: "cat/dep !cat/foo",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/dep",
        version: "1",
        rdepend: "cat/foo",
        ..Default::default()
    });
    f.add(pkg("cat/foo", "1"));

    match resolve(&f, &["cat/main"]) {
        Err(ResolveError::Unsatisfiable { explanation }) => {
            // The derivation names the blocking and blocked packages.
            assert!(
                explanation.contains("cat/foo"),
                "explanation should name the blocked package:\n{explanation}"
            );
        }
        other => panic!("expected unsatisfiable, got {other:?}"),
    }
}

#[test]
fn slot_collision_is_a_conflict() {
    // Two request atoms force two different versions of the same cp in the same
    // slot, which the solver cannot satisfy simultaneously.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/lib",
        version: "1",
        slot: "0",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/lib",
        version: "2",
        slot: "0",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/a",
        version: "1",
        rdepend: "=cat/lib-1",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/b",
        version: "1",
        rdepend: "=cat/lib-2",
        ..Default::default()
    });

    // a needs lib-1, b needs lib-2; only one slot-0 version can be chosen.
    match resolve(&f, &["cat/a", "cat/b"]) {
        Err(ResolveError::Unsatisfiable { explanation }) => {
            assert!(
                explanation.contains("cat/lib"),
                "explanation should name the colliding cp:\n{explanation}"
            );
        }
        other => panic!("expected unsatisfiable slot collision, got {other:?}"),
    }
}

#[test]
fn two_slots_of_one_cp_coinstall() {
    // The headline of slot-keying: two distinct slots of one cp are independent
    // solver variables and install together rather than colliding.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "dev-lang/python",
        version: "3.11",
        slot: "3.11",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "dev-lang/python",
        version: "3.12",
        slot: "3.12",
        ..Default::default()
    });

    let sol = resolve(&f, &["dev-lang/python:3.11", "dev-lang/python:3.12"]).expect("resolves");
    let pythons: Vec<_> = sol
        .packages
        .iter()
        .filter(|p| p.cp == "dev-lang/python")
        .collect();
    assert_eq!(pythons.len(), 2, "both slots co-install: {pythons:?}");
    assert!(pythons.iter().any(|p| p.slot == "3.11"));
    assert!(pythons.iter().any(|p| p.slot == "3.12"));
}

#[test]
fn two_slots_of_one_cp_serialize_to_two_tasks() {
    // The slot-keyed solution must survive serialization: both slots reach the
    // merge plan as their own task rather than collapsing to one.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "dev-lang/python",
        version: "3.11",
        slot: "3.11",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "dev-lang/python",
        version: "3.12",
        slot: "3.12",
        ..Default::default()
    });

    let sol = resolve(&f, &["dev-lang/python:3.11", "dev-lang/python:3.12"]).expect("resolves");
    let tasks = moraine_resolve::serialize(&sol).expect("serializes");
    let python_tasks: Vec<_> = tasks.iter().filter(|t| t.cp == "dev-lang/python").collect();
    assert_eq!(
        python_tasks.len(),
        2,
        "both slots produce a merge task: {python_tasks:?}"
    );
    let task_311 = python_tasks
        .iter()
        .find(|t| t.slot == "3.11")
        .expect("3.11 task");
    let task_312 = python_tasks
        .iter()
        .find(|t| t.slot == "3.12")
        .expect("3.12 task");
    // Each task carries its own slot's version, not the other slot's.
    assert_eq!(task_311.version, "3.11");
    assert_eq!(task_312.version, "3.12");
}

#[test]
fn installed_package_skips_build_deps() {
    // An already-installed package is not rebuilt, so an unavailable build-time
    // dependency must not make it unresolvable; only its runtime deps matter.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/tool",
        version: "1",
        eapi: "8",
        bdepend: "cat/missing-builddep",
        rdepend: "cat/runtimelib",
        ..Default::default()
    });
    f.add(pkg("cat/runtimelib", "1"));
    // cat/missing-builddep has no candidate at all.
    f.add_installed(installed("cat/tool", "1", "0", None, &[]));

    let sol = resolve(&f, &["cat/tool"]).expect("installed package resolves despite missing bdep");
    assert!(sol.package("cat/tool").is_some());
    assert!(
        sol.package("cat/runtimelib").is_some(),
        "runtime dep pulled"
    );

    // A NOT-installed package with the same missing build dep does fail.
    let mut f2 = Fixture::new();
    f2.add(PkgSpec {
        cp: "cat/tool",
        version: "1",
        eapi: "8",
        bdepend: "cat/missing-builddep",
        ..Default::default()
    });
    assert!(resolve(&f2, &["cat/tool"]).is_err());
}

#[test]
fn ranged_blocker_only_removes_matching_versions() {
    // `!<cat/foo-2` removes the installed cat/foo-1 but leaves cat/foo-3.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/parent",
        version: "1",
        rdepend: "!<cat/foo-2",
        ..Default::default()
    });
    f.add_installed(installed("cat/foo", "1", "1", None, &[]));
    f.add_installed(installed("cat/foo", "3", "3", None, &[]));

    let sol = resolve(&f, &["cat/parent"]).expect("resolves");
    let victims: Vec<_> = sol.blockers.iter().flat_map(|b| &b.victims).collect();
    assert!(
        victims.iter().any(|v| v.version.as_str() == "1"),
        "cat/foo-1 (<2) is a victim"
    );
    assert!(
        !victims.iter().any(|v| v.version.as_str() == "3"),
        "cat/foo-3 (>=2) must not be removed: {victims:?}"
    );
}

#[test]
fn slotted_blocker_keeps_other_slot() {
    // `!cat/foo:1` removes the installed slot 1 but leaves slot 2.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/parent",
        version: "1",
        rdepend: "!cat/foo:1",
        ..Default::default()
    });
    f.add_installed(installed("cat/foo", "1", "1", None, &[]));
    f.add_installed(installed("cat/foo", "2", "2", None, &[]));

    let sol = resolve(&f, &["cat/parent"]).expect("resolves");
    let victims: Vec<_> = sol.blockers.iter().flat_map(|b| &b.victims).collect();
    assert!(victims.iter().any(|v| v.slot == "1"));
    assert!(
        !victims.iter().any(|v| v.slot == "2"),
        "slot 2 must be kept: {victims:?}"
    );
}

#[test]
fn blocker_cannot_remove_the_package_manager() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/parent",
        version: "1",
        rdepend: "!sys-apps/portage",
        ..Default::default()
    });
    f.add_installed(installed("sys-apps/portage", "3.0", "0", None, &[]));

    match resolve(&f, &["cat/parent"]) {
        Err(ResolveError::UnresolvableBlocker { victim, .. }) => {
            assert_eq!(victim, "sys-apps/portage");
        }
        other => panic!("expected UnresolvableBlocker refusal, got {other:?}"),
    }
}

#[test]
fn slotless_dep_satisfied_by_existing_slot() {
    // A slotless dep does not force a new slot when an available slot already
    // satisfies it: it expands to a disjunction over the cp's slots.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "dev-lang/python",
        version: "3.11",
        slot: "3.11",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "dev-lang/python",
        version: "3.12",
        slot: "3.12",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "app/uses-python",
        version: "1",
        rdepend: "dev-lang/python",
        ..Default::default()
    });

    let sol = resolve(&f, &["app/uses-python"]).expect("resolves");
    let pythons = sol
        .packages
        .iter()
        .filter(|p| p.cp == "dev-lang/python")
        .count();
    assert_eq!(pythons, 1, "a slotless dep selects exactly one slot");
}

#[test]
fn non_overlapping_any_of_is_plain_disjunction() {
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        // Disjoint cp sets across branches: plain disjunction, no DNF.
        rdepend: "|| ( cat/x cat/y )",
        ..Default::default()
    });
    f.add(pkg("cat/x", "1"));
    f.add(pkg("cat/y", "1"));

    let sol = resolve(&f, &["cat/main"]).expect("resolves");
    // The preferred (first) branch cat/x is selected.
    assert!(sol.package("cat/x").is_some());
}

#[test]
fn subslot_rebuild_detected() {
    let mut f = Fixture::new();
    // main installed with a := binding to cat/lib slot 2 subslot 2.1.
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        eapi: "8",
        rdepend: "cat/lib:=",
        ..Default::default()
    });
    // The available provider now has subslot 2.2.
    f.add(PkgSpec {
        cp: "cat/lib",
        version: "2",
        slot: "2",
        subslot: Some("2.2"),
        ..Default::default()
    });
    f.add_installed(installed(
        "cat/main",
        "1",
        "0",
        None,
        &[("cat/lib", "2", Some("2.1"))],
    ));
    f.add_installed(installed("cat/lib", "1", "2", Some("2.1"), &[]));

    let sol = resolve(&f, &["cat/main", "cat/lib"]).expect("resolves");
    let main = sol.package("cat/main").unwrap();
    assert!(
        main.subslot_rebuild,
        "main should be flagged for subslot rebuild"
    );
}

#[test]
fn update_prefers_highest_over_installed() {
    let mut f = Fixture::new();
    f.add(pkg("cat/a", "1"));
    f.add(pkg("cat/a", "2"));
    f.add_installed(installed("cat/a", "1", "0", None, &[]));

    // Default: the installed version is kept rather than upgraded.
    let sol = resolve(&f, &["cat/a"]).expect("resolves");
    assert_eq!(sol.package("cat/a").unwrap().version.as_str(), "1");

    // --update: the highest visible version is selected.
    let sol = resolve_with(
        &f,
        &["cat/a"],
        Modifiers {
            update: true,
            ..Default::default()
        },
    )
    .expect("resolves");
    assert_eq!(sol.package("cat/a").unwrap().version.as_str(), "2");
}

#[test]
fn newuse_reinstalls_on_use_change() {
    let mut f = Fixture::new();
    // The repository build enables `foo`; the installed build has it disabled.
    f.add(PkgSpec {
        cp: "cat/a",
        version: "1",
        iuse: &["foo"],
        use_enabled: &["foo"],
        ..Default::default()
    });
    f.add_installed(installed("cat/a", "1", "0", None, &[]));

    // Default: same version, so it is a no-op reinstall (already installed).
    let sol = resolve(&f, &["cat/a"]).expect("resolves");
    assert!(sol.package("cat/a").unwrap().already_installed);

    // --newuse: the USE set changed, so the package is a reinstall, not a no-op.
    let sol = resolve_with(
        &f,
        &["cat/a"],
        Modifiers {
            newuse: true,
            ..Default::default()
        },
    )
    .expect("resolves");
    assert!(!sol.package("cat/a").unwrap().already_installed);
}

#[test]
fn installed_package_blocker_blocks_new_install() {
    // An installed Y declares `!cat/x` in RDEPEND. Installing cat/x while Y
    // stays installed is an unresolvable blocker.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/y",
        version: "1",
        rdepend: "!cat/x",
        ..Default::default()
    });
    f.add(pkg("cat/x", "1"));
    f.add_installed(installed("cat/y", "1", "0", None, &[]));

    match resolve(&f, &["cat/x"]) {
        Err(ResolveError::UnresolvableBlocker {
            blocker, victim, ..
        }) => {
            assert_eq!(blocker, "cat/y");
            assert_eq!(victim, "cat/x");
        }
        other => panic!("expected an installed-package blocker, got {other:?}"),
    }
}

#[test]
fn deep_catches_broken_reverse_dependency() {
    // cat/consumer (installed) needs <cat/lib-2. Upgrading cat/lib to 2 under
    // --update --deep would break the consumer, which the consistency pass
    // catches.
    let mut f = Fixture::new();
    f.add(pkg("cat/lib", "1"));
    f.add(pkg("cat/lib", "2"));
    f.add(PkgSpec {
        cp: "cat/consumer",
        version: "1",
        rdepend: "<cat/lib-2",
        ..Default::default()
    });
    f.add_installed(installed("cat/lib", "1", "0", None, &[]));
    f.add_installed(installed("cat/consumer", "1", "0", None, &[]));

    // Without --deep the upgrade is allowed (no reverse-dep validation).
    let sol = resolve_with(
        &f,
        &["cat/lib"],
        Modifiers {
            update: true,
            ..Default::default()
        },
    )
    .expect("resolves without deep");
    assert_eq!(sol.package("cat/lib").unwrap().version.as_str(), "2");

    // With --deep the broken consumer is caught.
    let r = resolve_with(
        &f,
        &["cat/lib"],
        Modifiers {
            update: true,
            deep: true,
            ..Default::default()
        },
    );
    match r {
        Err(ResolveError::BrokenReverseDependency {
            dependent,
            dependency,
            ..
        }) => {
            assert_eq!(dependent, "cat/consumer");
            assert_eq!(dependency, "cat/lib");
        }
        other => panic!("expected a broken reverse dependency, got {other:?}"),
    }
}

#[test]
fn deep_depth_zero_disables_the_consistency_pass() {
    // The same broken upgrade as `deep_catches_broken_reverse_dependency`:
    // `--deep=0` skips the consistency pass (Portage's `deep != 0`), while
    // unbounded `--deep` and `--deep=1` still catch the broken consumer.
    let mut f = Fixture::new();
    f.add(pkg("cat/lib", "1"));
    f.add(pkg("cat/lib", "2"));
    f.add(PkgSpec {
        cp: "cat/consumer",
        version: "1",
        rdepend: "<cat/lib-2",
        ..Default::default()
    });
    f.add_installed(installed("cat/lib", "1", "0", None, &[]));
    f.add_installed(installed("cat/consumer", "1", "0", None, &[]));

    let deep = |depth: Option<u32>| {
        resolve_with(
            &f,
            &["cat/lib"],
            Modifiers {
                update: true,
                deep: true,
                deep_depth: depth,
                ..Default::default()
            },
        )
    };

    // Depth zero disables the pass, so the breaking upgrade is allowed.
    let sol = deep(Some(0)).expect("deep=0 skips the consistency pass");
    assert_eq!(sol.package("cat/lib").unwrap().version.as_str(), "2");

    // Unbounded `--deep` and a positive depth both run the pass and catch it.
    for depth in [None, Some(1)] {
        match deep(depth) {
            Err(ResolveError::BrokenReverseDependency { dependent, .. }) => {
                assert_eq!(dependent, "cat/consumer");
            }
            other => panic!("expected a broken reverse dependency, got {other:?}"),
        }
    }
}

#[test]
fn slot_operator_pulls_in_installed_consumer_for_rebuild() {
    // cat/consumer is installed with a := binding to cat/lib:2/2.1. The available
    // cat/lib is now 2/2.2 (a sub-slot bump). Resolving just cat/lib must pull in
    // the installed consumer and flag it for rebuild, though it is not requested.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/lib",
        version: "2",
        slot: "2",
        subslot: Some("2.2"),
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/consumer",
        version: "1",
        eapi: "8",
        rdepend: "cat/lib:=",
        ..Default::default()
    });
    f.add_installed(installed(
        "cat/consumer",
        "1",
        "0",
        None,
        &[("cat/lib", "2", Some("2.1"))],
    ));
    f.add_installed(installed("cat/lib", "1", "2", Some("2.1"), &[]));

    let sol = resolve(&f, &["cat/lib"]).expect("resolves");
    assert_eq!(
        sol.package("cat/lib").unwrap().subslot.as_deref(),
        Some("2.2")
    );
    let consumer = sol
        .package("cat/consumer")
        .expect("the installed consumer is pulled into the solution");
    assert!(
        consumer.subslot_rebuild,
        "the pulled-in consumer must be flagged for a slot-operator rebuild"
    );
}

#[test]
fn slot_operator_binding_only_for_satisfied_branch() {
    // main has `|| ( cat/a:= cat/b:= )`. Only cat/b is installable, so the
    // solution links cat/b and records a binding for cat/b, never for the
    // unsatisfied cat/a branch.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/main",
        version: "1",
        eapi: "8",
        rdepend: "|| ( cat/a:= cat/b:= )",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/b",
        version: "1",
        slot: "2",
        subslot: Some("2.1"),
        ..Default::default()
    });

    let sol = resolve(&f, &["cat/main"]).expect("resolves via the cat/b branch");
    let main = sol.package("cat/main").unwrap();
    assert!(
        main.slot_bindings.iter().any(|b| b.dependency == "cat/b"),
        "a binding is recorded for the linked branch cat/b: {:?}",
        main.slot_bindings
    );
    assert!(
        !main.slot_bindings.iter().any(|b| b.dependency == "cat/a"),
        "no binding for the unlinked branch cat/a"
    );
}

#[test]
fn changed_slot_forces_reinstall() {
    // The installed cat/a-1 recorded sub-slot 1; the current ebuild now declares
    // sub-slot 2. --changed-slot reinstalls it.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/a",
        version: "1",
        slot: "0",
        subslot: Some("2"),
        ..Default::default()
    });
    f.add_installed(installed("cat/a", "1", "0", Some("1"), &[]));

    let sol = resolve(&f, &["cat/a"]).expect("resolves");
    assert!(!sol.package("cat/a").unwrap().subslot_rebuild);

    let sol = resolve_with(
        &f,
        &["cat/a"],
        Modifiers {
            changed_slot: true,
            ..Default::default()
        },
    )
    .expect("resolves");
    assert!(
        sol.package("cat/a").unwrap().subslot_rebuild,
        "a sub-slot change must force a reinstall under --changed-slot"
    );
}

#[test]
fn changed_deps_forces_reinstall() {
    // The installed cat/a-1 recorded RDEPEND cat/old; the current ebuild now
    // RDEPENDs cat/new. --changed-deps reinstalls it.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/a",
        version: "1",
        rdepend: "cat/new",
        ..Default::default()
    });
    f.add(pkg("cat/new", "1"));
    let mut inst = installed("cat/a", "1", "0", None, &[]);
    inst.recorded_deps
        .insert("RDEPEND".to_owned(), "cat/old".to_owned());
    f.add_installed(inst);

    let sol = resolve(&f, &["cat/a"]).expect("resolves");
    assert!(!sol.package("cat/a").unwrap().subslot_rebuild);

    let sol = resolve_with(
        &f,
        &["cat/a"],
        Modifiers {
            changed_deps: true,
            ..Default::default()
        },
    )
    .expect("resolves");
    assert!(
        sol.package("cat/a").unwrap().subslot_rebuild,
        "a dependency change must force a reinstall under --changed-deps"
    );
}

#[test]
fn keyword_autounmask_refused_by_default() {
    // A stable-profile dependency whose only candidate is visible solely through
    // a `~arch` keyword: by default the resolver reports the change and does not
    // auto-apply it.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/app",
        version: "1",
        rdepend: "cat/dep",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/dep",
        version: "1",
        accept_keyword: Some("~amd64"),
        ..Default::default()
    });

    let sol = resolve(&f, &["cat/app"]).expect("resolves through the keyword suggestion");
    let change = sol
        .autounmask
        .iter()
        .find(|c| c.cp == "cat/dep")
        .expect("keyword change recorded");
    assert_eq!(change.change.keyword.as_deref(), Some("~amd64"));
    assert!(
        !change.auto_applied,
        "a keyword change is a suggestion under the default policy"
    );

    // With keyword autounmask explicitly enabled the change is applied.
    let sol = resolve_with(
        &f,
        &["cat/app"],
        Modifiers {
            autounmask: AutounmaskPolicy {
                keep_keywords: false,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .expect("resolves with keyword autounmask enabled");
    let change = sol
        .autounmask
        .iter()
        .find(|c| c.cp == "cat/dep")
        .expect("keyword change recorded");
    assert!(
        change.auto_applied,
        "the keyword change is applied when keyword autounmask is enabled"
    );
}

#[test]
fn license_autounmask_refused_by_default() {
    // A dependency whose only candidate carries a non-accepted license: by
    // default the resolver reports the change and does not auto-apply it.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/app",
        version: "1",
        rdepend: "cat/dep",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "cat/dep",
        version: "1",
        accept_licenses: &["MyEULA"],
        ..Default::default()
    });

    let sol = resolve(&f, &["cat/app"]).expect("resolves through the license suggestion");
    let change = sol
        .autounmask
        .iter()
        .find(|c| c.cp == "cat/dep")
        .expect("license change recorded");
    assert_eq!(change.change.licenses, vec!["MyEULA".to_owned()]);
    assert!(
        !change.auto_applied,
        "a license change is a suggestion under the default policy"
    );

    // With license autounmask explicitly enabled the change is applied.
    let sol = resolve_with(
        &f,
        &["cat/app"],
        Modifiers {
            autounmask: AutounmaskPolicy {
                keep_license: false,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .expect("resolves with license autounmask enabled");
    let change = sol
        .autounmask
        .iter()
        .find(|c| c.cp == "cat/dep")
        .expect("license change recorded");
    assert!(
        change.auto_applied,
        "the license change is applied when license autounmask is enabled"
    );
}

#[test]
fn use_autounmask_proposes_settable_flag() {
    // dev-libs/foo declares ssl but has it off; the consumer needs [ssl]. USE
    // autounmask proposes enabling the settable flag and resolves.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/app",
        version: "1",
        rdepend: "dev-libs/foo[ssl]",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "dev-libs/foo",
        version: "1",
        iuse: &["ssl"],
        use_enabled: &[],
        ..Default::default()
    });

    let sol = resolve(&f, &["cat/app"]).expect("resolves via USE autounmask");
    let change = sol
        .autounmask
        .iter()
        .find(|c| c.cp == "dev-libs/foo")
        .expect("USE change recorded");
    assert_eq!(
        change.change.use_changes,
        vec![UseChange {
            flag: "ssl".to_owned(),
            enable: true,
        }]
    );
    assert!(
        change.auto_applied,
        "USE autounmask is applied under the default policy"
    );
    // foo is selected with the proposed flag enabled.
    let foo = sol.package("dev-libs/foo").expect("foo is selected");
    assert!(foo.use_enabled.contains("ssl"));
}

#[test]
fn use_autounmask_skips_locked_flag() {
    // The needed flag is pinned by use.mask/use.force, so it cannot be toggled
    // and the dependency stays unsatisfiable.
    let mut f = Fixture::new();
    f.add(PkgSpec {
        cp: "cat/app",
        version: "1",
        rdepend: "dev-libs/foo[ssl]",
        ..Default::default()
    });
    f.add(PkgSpec {
        cp: "dev-libs/foo",
        version: "1",
        iuse: &["ssl"],
        use_enabled: &[],
        locked_use: &["ssl"],
        ..Default::default()
    });

    let r = resolve(&f, &["cat/app"]);
    assert!(
        matches!(r, Err(ResolveError::Unsatisfiable { .. })),
        "a locked flag cannot be toggled, so the dependency is unsatisfiable"
    );
}
