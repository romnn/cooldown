//! The Go tool adapter: GOPROXY publish-time reads, a faithful `x/mod` semver/pseudo-version
//! port, and `go`-driven resolution/apply. Go has no native cooldown config, so its native policy
//! layer is always empty — policy comes from `cooldown.toml`/global/CLI.

pub mod gocmd;
mod mutation;
pub mod proxy;
pub mod semver;
pub mod tool;

pub use proxy::GoProxy;
pub use tool::{GO_ID, GoTool};
