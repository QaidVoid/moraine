//! Smoke test: a typed error renders through the reporter and exits non-zero.

use std::process::Command;

#[test]
fn demo_error_reports_and_exits_nonzero() {
    let bin = env!("CARGO_BIN_EXE_moraine");
    // Ignore any EMERGE_DEFAULT_OPTS from the host make.conf so the test stays
    // about the demo-error path rather than the host's persisted options.
    let output = Command::new(bin)
        .arg("--ignore-default-opts")
        .arg("--demo-error")
        .output()
        .expect("failed to run moraine binary");

    assert!(
        !output.status.success(),
        "expected a non-zero exit status for a reported error"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("demonstration error"),
        "diagnostic message missing from stderr: {stderr}"
    );
    assert!(
        stderr.contains("moraine::demo"),
        "diagnostic code missing from stderr: {stderr}"
    );
}
