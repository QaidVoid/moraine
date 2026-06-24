//! The Moraine command-line frontend.
//!
//! The binary in `main.rs` is a thin wrapper around this library so the
//! behavior can be unit and snapshot tested. During bootstrap it only sets up
//! diagnostics and tracing and runs a placeholder workload; real actions arrive
//! in later phases.

use miette::Diagnostic;
use thiserror::Error;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// A demonstration error used to exercise the diagnostic reporter end to end.
#[derive(Debug, Error, Diagnostic)]
#[error("demonstration error: {what}")]
#[diagnostic(
    code(moraine::demo),
    help("this is a smoke-test diagnostic; run without --demo-error to proceed normally")
)]
pub struct DemoError {
    /// What the demonstration failure was about.
    pub what: String,
}

/// Initialize tracing for the process.
///
/// The level comes from the `RUST_LOG` environment variable when set; otherwise
/// it defaults to `info`, and `--verbose` lowers the floor to `debug`. This is
/// the runtime switch used to profile resolution phases later.
pub fn init_tracing(verbose: bool) {
    let default = if verbose {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };
    let filter = EnvFilter::builder()
        .with_default_directive(default.into())
        .from_env_lossy();
    // try_init so repeated calls in tests do not panic.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false))
        .try_init();
}

/// Run the placeholder workload inside an instrumented span.
///
/// This proves the end-to-end tracing path during bootstrap: the span lives in
/// the CLI while the work runs in `moraine-common`.
pub fn run_demo() {
    let span = tracing::info_span!("demo_workload");
    let _guard = span.enter();
    let digest = moraine_common::hash::blake3(b"moraine");
    tracing::info!(%digest, "computed greenfield digest");
}

/// Render a diagnostic to a plain, uncolored string.
///
/// Used by tests and snapshots; the interactive path uses miette's default
/// graphical handler installed by returning a [`miette::Result`] from `main`.
pub fn render_report(diagnostic: &dyn Diagnostic) -> String {
    use miette::{GraphicalReportHandler, GraphicalTheme};
    let mut out = String::new();
    let handler = GraphicalReportHandler::new_themed(GraphicalTheme::unicode_nocolor());
    let _ = handler.render_report(&mut out, diagnostic);
    out
}
