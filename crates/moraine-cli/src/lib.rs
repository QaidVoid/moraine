//! The Moraine command-line frontend.
//!
//! The binary in `main.rs` is a thin wrapper around this library so the
//! behavior can be unit and snapshot tested. It parses an `emerge`-compatible
//! argument surface, expands targets and package sets, runs a read-only
//! resolution, and renders the `emerge`-style merge list, tree, conflict
//! diagnostics, and unread news. Nothing here builds, fetches, syncs, merges, or
//! writes persisted state.

pub mod args;
pub mod config;
pub mod corpus;
pub mod diagnostics;
pub mod news;
pub mod plan;
pub mod render;
pub mod run;
pub mod sets;

use miette::Diagnostic;
use thiserror::Error;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::args::Cli;

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

/// Dispatch the parsed command line through the read-only flow.
///
/// Loads configuration, expands targets and sets, and prints the request that
/// would be resolved together with the timing breakdown and any unread news.
/// The full solve over a real system runs only when a repository and installed
/// store are available; this phase is strictly read-only and returns a
/// [`miette::Result`] so failures render through the reporter.
pub fn dispatch(cli: &Cli) -> miette::Result<()> {
    use crate::config::installed_set_heads;
    use crate::news::{NewsEnv, render_news, unread_relevant};
    use std::collections::BTreeSet;

    if cli.targets.is_empty() {
        println!("No targets given. Pass atoms or package sets such as @world.");
        return Ok(());
    }

    let (ctx, request, timing) = run::load_and_expand(cli)?;

    println!("Targets resolved to {} atom(s):", request.atoms.len());
    for atom in &request.atoms {
        println!("  {atom}");
    }
    if !request.excluded.is_empty() {
        println!(
            "Excluded (pinned to installed): {}",
            request.excluded.join(", ")
        );
    }
    if request.oneshot {
        println!("Note: --oneshot, targets would not be added to the world set.");
    }

    let modifiers = ["update", "deep", "newuse"]
        .iter()
        .zip([request.update, request.deep, request.newuse])
        .filter(|(_, on)| *on)
        .map(|(name, _)| *name)
        .collect::<Vec<_>>();
    if !modifiers.is_empty() {
        println!("Modifiers: {}", modifiers.join(", "));
    }

    let roots = run::roots_from(cli);
    let installed = installed_set_heads(&ctx);
    let env = NewsEnv {
        installed,
        profile: ctx
            .profile
            .nodes
            .last()
            .map(|n| n.path.display().to_string())
            .unwrap_or_default(),
        arch: ctx.arch.clone(),
    };
    let news_dir = roots.root_dir().join("var/db/repos/gentoo/metadata/news");
    let unread: BTreeSet<String> = BTreeSet::new();
    match unread_relevant(&news_dir, &env, &unread) {
        Ok(items) => {
            let rendered = render_news(&items);
            if !rendered.is_empty() {
                print!("{rendered}");
            }
        }
        Err(error) => tracing::warn!(%error, "could not read news"),
    }

    if cli.timing || cli.is_verbose() {
        print!("{}", timing.report());
    }

    Ok(())
}
