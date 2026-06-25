//! Integration tests for Gentoo resolution against in-memory fixtures.

mod fixture;

use fixture::{Fixture, PkgSpec, installed};
use moraine_resolve::solution::{DepClass, Root};
use moraine_resolve::{ResolveError, resolve};

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

    let edge = |to: &str| sol.edges.iter().find(|e| e.to == to).unwrap().clone();

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
    let dep = sol.edges.iter().find(|e| e.to == "cat/header").unwrap();
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
    // Only a candidate with foo enabled satisfies the atom.
    f.add(PkgSpec {
        cp: "cat/dep",
        version: "1",
        iuse: &["foo"],
        use_enabled: &[], // foo off: does not satisfy [foo]
        ..Default::default()
    });

    let r = resolve(&f, &["cat/main"]);
    assert!(matches!(r, Err(ResolveError::Unsatisfiable { .. })));

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
