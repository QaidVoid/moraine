//! Snapshot test for the rendered diagnostic output.

use moraine_cli::{DemoError, render_report};

#[test]
fn demo_error_renders_expected_report() {
    let err = DemoError {
        what: "snapshot sample".to_owned(),
    };
    insta::assert_snapshot!(render_report(&err));
}
