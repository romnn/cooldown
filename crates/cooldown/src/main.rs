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
    std::process::ExitCode::from(exit.code() as u8)
}
