//! Snapshot test for the rendered failure explanation.

use moraine_solver::{Explanation, MapProvider, Range, Term, solve};

fn render(explanation: &Explanation<&'static str, u32>, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    match explanation {
        Explanation::External { description, .. } => {
            out.push_str(&format!("{indent}- {description}\n"));
        }
        Explanation::Derived { causes, .. } => {
            out.push_str(&format!("{indent}- derived from:\n"));
            for cause in causes {
                render(cause, depth + 1, out);
            }
        }
        Explanation::Shared(id) => {
            out.push_str(&format!("{indent}- (see step {id})\n"));
        }
    }
}

#[test]
fn unsatisfiable_explanation_snapshot() {
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

    let failure = solve(&p, "root", 1).expect_err("unsatisfiable");
    let mut out = String::new();
    render(&failure.explanation, 0, &mut out);

    // The rendered tree is structured and deterministic.
    assert!(out.contains("depends on"), "explanation:\n{out}");
    assert!(
        out.lines().count() >= 2,
        "explanation should be a tree:\n{out}"
    );
    insta::assert_snapshot!(out);
}
