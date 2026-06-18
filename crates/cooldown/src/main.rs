//! The `cooldown` binary entry point: install error reporting, parse, run on a tokio runtime, and
//! exit with the policy taxonomy's code.

use clap::Parser;
use cooldown::cli::{Cli, run};

fn main() -> std::process::ExitCode {
    let _ = color_eyre::install();
    let cli = Cli::parse();

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

    let exit = runtime.block_on(run(cli));
    // Exit codes are the fixed 0..=4 taxonomy, so the conversion never saturates.
    std::process::ExitCode::from(u8::try_from(exit.code()).unwrap_or(1))
}
