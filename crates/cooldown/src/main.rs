//! The `cooldown` binary entry point: install error reporting, parse, run on a tokio runtime, and
//! exit with the policy taxonomy's code.

use clap::{CommandFactory, FromArgMatches};
use cooldown::cli::{Cli, CliOverrides, run};

fn main() -> std::process::ExitCode {
    let _ = color_eyre::install();
    // Parse into `ArgMatches` first so we can tell which flags were set explicitly (for config
    // precedence), then reconstruct the typed `Cli` from the same matches.
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    let overrides = CliOverrides::from_matches(&matches);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start runtime: {e}");
            return std::process::ExitCode::from(4);
        }
    };

    let exit = runtime.block_on(run(cli, overrides));
    // Exit codes are the fixed 0..=4 taxonomy, so the conversion never saturates.
    std::process::ExitCode::from(u8::try_from(exit.code()).unwrap_or(1))
}
