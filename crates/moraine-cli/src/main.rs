//! Entry point for the `moraine` command.

use moraine_cli::args::parse_with_default_opts;
use moraine_cli::{DemoError, config, dispatch, init_tracing, run};

fn main() -> miette::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_with_default_opts(&argv, |cli| {
        config::emerge_default_opts(&run::roots_from(cli))
    }) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };

    init_tracing(cli.is_verbose());
    moraine_cli::render::init_color();

    if cli.demo_error {
        return Err(DemoError {
            what: "requested via --demo-error".to_owned(),
        }
        .into());
    }

    dispatch(&cli)
}
