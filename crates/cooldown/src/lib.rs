//! The `cooldown` binary's library: the application use cases ([`app`]), config discovery
//! ([`discovery`]), and the CLI composition root ([`cli`]). Exposed as a library so integration
//! tests can drive the use cases and config discovery directly.

pub mod app;
pub mod cli;
pub mod discovery;
mod scan;

pub use app::Exit;
