//! Entry point for the `moraine` command.

use moraine_cli::{DemoError, init_tracing, run_demo};

fn main() -> miette::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
    init_tracing(verbose);

    if args.iter().any(|a| a == "--demo-error") {
        return Err(DemoError {
            what: "requested via --demo-error".to_owned(),
        }
        .into());
    }

    run_demo();
    Ok(())
}
