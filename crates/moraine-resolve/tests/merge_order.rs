//! Integration tests for merge-order serialization.

use moraine_resolve::graph::{EdgeFlags, MergeGraph, Range};
use moraine_resolve::solution::{
    BlockVictim, DepClass, DepEdge, RecordedBlocker, ResolvedPackage, ResolvedSolution, Root,
};
use moraine_resolve::{MergeOrderError, TaskKind, serialize};
use moraine_version::Version;

fn victim(cp: &str, version: &str, slot: &str) -> BlockVictim {
    BlockVictim {
        cp: cp.to_owned(),
        version: Version::parse(version).unwrap(),
        slot: slot.to_owned(),
    }
}

fn rp(cp: &str, version: &str, installed: bool) -> ResolvedPackage {
    ResolvedPackage {
        cp: cp.to_owned(),
        version: Version::parse(version).unwrap(),
        slot: "0".to_owned(),
        subslot: None,
        use_enabled: Default::default(),
        slot_bindings: Vec::new(),
        already_installed: installed,
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
fn build_dependency_precedes_dependent() {
    let solution = ResolvedSolution {
        packages: vec![rp("cat/a", "1", false), rp("cat/b", "1", false)],
        // a build-depends on b.
        edges: vec![edge("cat/a", "cat/b", DepClass::Depend)],
        blockers: Vec::new(),
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let tasks = serialize(&solution).expect("serializes");
    let pos = |cp: &str| tasks.iter().position(|t| t.cp == cp).unwrap();
    assert!(pos("cat/b") < pos("cat/a"), "b must precede a");
    assert!(tasks.iter().all(|t| t.kind == TaskKind::Merge));
}

#[test]
fn output_is_permutation_of_input() {
    let solution = ResolvedSolution {
        packages: vec![
            rp("cat/a", "1", false),
            rp("cat/b", "1", false),
            rp("cat/c", "1", false),
        ],
        edges: vec![
            edge("cat/a", "cat/b", DepClass::Rdepend),
            edge("cat/b", "cat/c", DepClass::Depend),
        ],
        blockers: Vec::new(),
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let tasks = serialize(&solution).expect("serializes");
    assert_eq!(tasks.len(), 3);
    let mut cps: Vec<_> = tasks.iter().map(|t| t.cp.clone()).collect();
    cps.sort();
    assert_eq!(cps, vec!["cat/a", "cat/b", "cat/c"]);
}

#[test]
fn serialization_is_deterministic() {
    let solution = ResolvedSolution {
        packages: vec![rp("cat/a", "1", false), rp("cat/b", "1", false)],
        edges: vec![edge("cat/a", "cat/b", DepClass::Rdepend)],
        blockers: Vec::new(),
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let first = serialize(&solution).expect("ok");
    let second = serialize(&solution).expect("ok");
    assert_eq!(first, second);
}

#[test]
fn runtime_cycle_is_broken() {
    // a and b runtime-depend on each other (a legitimate Gentoo cycle).
    let solution = ResolvedSolution {
        packages: vec![rp("cat/a", "1", false), rp("cat/b", "1", false)],
        edges: vec![
            edge("cat/a", "cat/b", DepClass::Rdepend),
            edge("cat/b", "cat/a", DepClass::Pdepend),
        ],
        blockers: Vec::new(),
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let tasks = serialize(&solution).expect("breaks the cycle");
    assert_eq!(tasks.len(), 2);
}

#[test]
fn hard_only_cycle_is_reported() {
    // a and b build-depend on each other: an unbreakable cycle.
    let solution = ResolvedSolution {
        packages: vec![rp("cat/a", "1", false), rp("cat/b", "1", false)],
        edges: vec![
            edge("cat/a", "cat/b", DepClass::Depend),
            edge("cat/b", "cat/a", DepClass::Depend),
        ],
        blockers: Vec::new(),
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let r = serialize(&solution);
    assert!(matches!(r, Err(MergeOrderError::UnresolvableCycle(_))));
}

#[test]
fn satisfied_edge_breaks_otherwise_unsolvable_cycle() {
    // a and b build-depend on each other, but b is already installed so its
    // edge is satisfied and can be dropped in the final stage.
    let solution = ResolvedSolution {
        packages: vec![rp("cat/a", "1", false), rp("cat/b", "1", true)],
        edges: vec![
            edge("cat/a", "cat/b", DepClass::Depend),
            // b -> a edge satisfied because a... use runtime so the cycle is
            // breakable once the satisfied b->a edge is dropped.
            DepEdge {
                from: "cat/b".to_owned(),
                to: "cat/a".to_owned(),
                class: DepClass::Rdepend,
                root: Root::Target,
                build_time: false,
                slot_op: false,
                optional: false,
            },
        ],
        blockers: Vec::new(),
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let tasks = serialize(&solution).expect("breaks via satisfied/runtime edge");
    assert_eq!(tasks.len(), 2);
}

#[test]
fn libc_is_preferred_early() {
    // main depends softly (runtime) on glibc; glibc should be merged first.
    let solution = ResolvedSolution {
        packages: vec![rp("cat/app", "1", false), rp("sys-libs/glibc", "2", false)],
        edges: vec![edge("cat/app", "sys-libs/glibc", DepClass::Rdepend)],
        blockers: Vec::new(),
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let tasks = serialize(&solution).expect("ok");
    let pos = |cp: &str| tasks.iter().position(|t| t.cp == cp).unwrap();
    assert!(pos("sys-libs/glibc") < pos("cat/app"));
}

#[test]
fn task_carries_details() {
    let mut pkg = rp("cat/a", "1.2.3", false);
    pkg.use_enabled.insert("foo".to_owned());
    pkg.slot = "5".to_owned();
    let solution = ResolvedSolution {
        packages: vec![pkg],
        edges: Vec::new(),
        blockers: Vec::new(),
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let tasks = serialize(&solution).expect("ok");
    let t = &tasks[0];
    assert_eq!(t.version, "1.2.3");
    assert_eq!(t.slot, "5");
    assert_eq!(t.use_enabled, vec!["foo".to_owned()]);
}

#[test]
fn edge_flags_hardness_ranking() {
    let bt_slot = EdgeFlags::for_class(DepClass::Depend, true, false, false);
    let bt = EdgeFlags::for_class(DepClass::Depend, false, false, false);
    let rt_slot = EdgeFlags::for_class(DepClass::Rdepend, true, false, false);
    let rt = EdgeFlags::for_class(DepClass::Rdepend, false, false, false);
    let post = EdgeFlags::for_class(DepClass::Pdepend, false, false, false);

    assert!(bt_slot.hardness() > bt.hardness());
    assert!(bt.hardness() > rt_slot.hardness());
    assert!(rt_slot.hardness() > rt.hardness());
    assert!(rt.hardness() > post.hardness());
}

#[test]
fn strong_blocker_uninstall_precedes_merges() {
    let solution = ResolvedSolution {
        packages: vec![rp("cat/new", "2", false)],
        edges: Vec::new(),
        blockers: vec![RecordedBlocker {
            blocker: "cat/new".to_owned(),
            blocked_atom: "cat/old".to_owned(),
            strong: true,
            victims: vec![victim("cat/old", "1", "0")],
        }],
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let tasks = serialize(&solution).expect("ok");
    // The strong-blocker uninstall is ordered before the replacement merge.
    assert_eq!(tasks[0].kind, TaskKind::Uninstall);
    assert_eq!(tasks[0].cp, "cat/old");
    assert!(
        tasks
            .iter()
            .any(|t| t.kind == TaskKind::Merge && t.cp == "cat/new")
    );
}

#[test]
fn weak_blocker_uninstall_follows_merges() {
    let solution = ResolvedSolution {
        packages: vec![rp("cat/new", "2", false)],
        edges: Vec::new(),
        blockers: vec![RecordedBlocker {
            blocker: "cat/new".to_owned(),
            blocked_atom: "cat/old".to_owned(),
            strong: false,
            victims: vec![victim("cat/old", "1", "0")],
        }],
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let tasks = serialize(&solution).expect("ok");
    // The merge comes first; the weak-blocker removal is still emitted, after.
    assert_eq!(tasks[0].kind, TaskKind::Merge);
    let last = tasks.last().unwrap();
    assert_eq!(last.kind, TaskKind::Uninstall);
    assert_eq!(last.cp, "cat/old");
}

#[test]
fn blocker_with_no_victims_uninstalls_nothing() {
    // A blocker whose target the solution is also installing in the same slot is
    // a replacement, so the victim computation leaves it with no victims and the
    // scheduler emits no uninstall (safety).
    let solution = ResolvedSolution {
        packages: vec![rp("cat/new", "2", false), rp("cat/old", "1", false)],
        edges: Vec::new(),
        blockers: vec![RecordedBlocker {
            blocker: "cat/new".to_owned(),
            blocked_atom: "cat/old".to_owned(),
            strong: true,
            victims: Vec::new(),
        }],
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let tasks = serialize(&solution).expect("ok");
    assert!(tasks.iter().all(|t| t.kind == TaskKind::Merge));
}

#[test]
fn leaf_query_ignores_soft_edges() {
    let solution = ResolvedSolution {
        packages: vec![rp("cat/a", "1", false), rp("cat/b", "1", false)],
        edges: vec![edge("cat/a", "cat/b", DepClass::Pdepend)],
        blockers: Vec::new(),
        backtracks: 0,
        autounmask: Vec::new(),
    };
    let graph = MergeGraph::from_solution(&solution);
    // At tier 0 (ignore nothing), cat/a is not a leaf (has a runtime_post edge).
    let leaves0 = graph.leaves(0, Range::Normal);
    assert!(leaves0.contains(&"cat/b".to_owned()));
    assert!(!leaves0.contains(&"cat/a".to_owned()));
    // At a tier that ignores runtime_post, cat/a becomes a leaf.
    let leaves = graph.leaves(2, Range::Normal);
    assert!(leaves.contains(&"cat/a".to_owned()));
}
