//! Integration tests for the solver over the synthetic provider.

use moraine_solver::{
    Clause, Dependencies, DependencyProvider, Explanation, MapProvider, Range, Range as R,
    Requirements, Term, solve,
};

/// A provider that exposes disjunctive clauses and conflicts directly so the
/// new `Requirements` shape can be exercised end to end.
struct ClauseProvider;

impl DependencyProvider for ClauseProvider {
    type Package = &'static str;
    type Version = u32;

    fn candidates(&self, package: &&'static str, range: &R<u32>) -> Vec<u32> {
        let versions: &[u32] = match *package {
            "root" => &[1],
            "a" => &[1, 2],
            "b" => &[1, 2],
            "x" => &[1, 2, 3],
            _ => &[],
        };
        versions
            .iter()
            .rev()
            .copied()
            .filter(|v| range.contains(v))
            .collect()
    }

    fn dependencies(
        &self,
        package: &&'static str,
        version: &u32,
    ) -> Dependencies<&'static str, u32> {
        match (*package, *version) {
            // root needs (a>=2 OR b>=1) and forbids a in [2,inf).
            ("root", 1) => Dependencies::Known(Requirements {
                clauses: vec![Clause::any_of(vec![
                    ("a", Term::positive(R::at_least(2))),
                    ("b", Term::positive(R::at_least(1))),
                ])],
                conflicts: vec![("a", Term::positive(R::at_least(2)))],
            }),
            _ => Dependencies::Known(Requirements::new()),
        }
    }
}

#[test]
fn disjunction_falls_back_to_second_alternative() {
    // The first alternative (a>=2) is forbidden by the conflict, so the solver
    // must satisfy the clause via b.
    let solution = solve(&ClauseProvider, "root", 1).expect("should solve");
    assert!(solution.contains_key("b"));
    // a must not be chosen at >=2.
    assert!(solution.get("a").map(|v| *v < 2).unwrap_or(true));
}

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
