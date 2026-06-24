//! Entry point for the `moraine` command.

use moraine_cli::args::Cli;
use moraine_cli::{DemoError, dispatch, init_tracing};

fn main() -> miette::Result<()> {
    let cli = match Cli::parse_from_args(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };

    init_tracing(cli.is_verbose());

    if cli.demo_error {
        return Err(DemoError {
            what: "requested via --demo-error".to_owned(),
        }
        .into());
    }

    dispatch(&cli)
}
