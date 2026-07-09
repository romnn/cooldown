//! The CLI composition root: clap parsing, config discovery, adapter wiring, and dispatch. This is
//! the only place that knows the full cast of tools.

mod args;
mod commands;
mod output;
mod present;
mod runtime;
mod setup;
mod workspace_free;

pub use args::{Cli, CliOverrides};
pub use runtime::run;

pub(in crate::cli) use args::{Command, GlobalArgs, LogLevel};
pub(in crate::cli) use runtime::generated_at;
