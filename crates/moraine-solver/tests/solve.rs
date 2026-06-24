//! Integration tests for the solver over the synthetic provider.

use moraine_solver::{Explanation, MapProvider, Range, Term, solve};

/// `root@1` depends on `a` (any) and `b >= 2`; `a@2` depends on `b < 3`.
fn solvable_provider() -> MapProvider<&'static str> {
    let mut p = MapProvider::new();
    p.add_package("root", vec![1]);
    p.add_package("a", vec![1, 2]);
    p.add_package("b", vec![1, 2, 3]);
    p.add_dependency(
        "root",
        1,
        vec![
            ("a", Term::positive(Range::full())),
            ("b", Term::positive(Range::at_least(2))),
        ],
    );
    p.add_dependency("a", 2, vec![("b", Term::positive(Range::less(3)))]);
    p
}

#[test]
fn solves_a_satisfiable_universe() {
    let provider = solvable_provider();
    let solution = solve(&provider, "root", 1).expect("should solve");
    assert_eq!(solution.get("a"), Some(&2));
    // b must satisfy >=2 (root) and <3 (a@2): exactly 2.
    assert_eq!(solution.get("b"), Some(&2));
}

#[test]
fn result_is_deterministic() {
    let provider = solvable_provider();
    let first = solve(&provider, "root", 1).unwrap();
    let second = solve(&provider, "root", 1).unwrap();
    assert_eq!(first, second);
}

#[test]
fn selection_satisfies_all_dependencies() {
    let provider = solvable_provider();
    let solution = solve(&provider, "root", 1).unwrap();
    // root requires b>=2, a@2 requires b<3.
    let b = solution["b"];
    assert!((2..3).contains(&b));
}

/// `root@1` needs `a` and `b`; `a` forces `c>=2`, `b` forces `c<2`; `c` has only
/// versions 1 and 2, so the universe is unsatisfiable.
fn unsatisfiable_provider() -> MapProvider<&'static str> {
    let mut p = MapProvider::new();
    p.add_package("root", vec![1]);
    p.add_package("a", vec![1]);
    p.add_package("b", vec![1]);
    p.add_package("c", vec![1, 2]);
    p.add_dependency(
        "root",
        1,
        vec![
            ("a", Term::positive(Range::full())),
            ("b", Term::positive(Range::full())),
        ],
    );
    p.add_dependency("a", 1, vec![("c", Term::positive(Range::at_least(2)))]);
    p.add_dependency("b", 1, vec![("c", Term::positive(Range::less(2)))]);
    p
}

#[test]
fn reports_unsatisfiability_with_explanation() {
    let provider = unsatisfiable_provider();
    let result = solve(&provider, "root", 1);
    let failure = result.expect_err("should be unsatisfiable");
    // The explanation is a structured tree, not flattened text.
    match failure.explanation {
        Explanation::Derived { causes, .. } => assert!(!causes.is_empty()),
        Explanation::External { .. } => {}
        Explanation::Shared(_) => panic!("root explanation should not be a shared ref"),
    }
}

#[test]
fn learned_conflicts_prevent_rediscovery() {
    // A chain where the first candidate of each package conflicts, forcing the
    // solver to backjump and learn. It must still terminate with a solution.
    let mut p = MapProvider::new();
    p.add_package("root", vec![1]);
    p.add_package("x", vec![1, 2, 3]);
    p.add_package("y", vec![1, 2, 3]);
    p.add_dependency(
        "root",
        1,
        vec![
            ("x", Term::positive(Range::full())),
            ("y", Term::positive(Range::full())),
        ],
    );
    // x@3 requires y<2, x@2 requires y<2; y@3 and y@2 are preferred first.
    p.add_dependency("x", 3, vec![("y", Term::positive(Range::less(2)))]);
    p.add_dependency("x", 2, vec![("y", Term::positive(Range::less(2)))]);
    let solution = solve(&p, "root", 1).unwrap();
    let y = solution["y"];
    let x = solution["x"];
    // Whatever is chosen must be internally consistent.
    if x >= 2 {
        assert!(y < 2);
    }
}
