//! The Go ecosystem adapter: GOPROXY publish-time reads, a faithful `x/mod` semver/pseudo-version
//! port, and `go`-driven resolution/apply. Go has no native cooldown config, so its native policy
//! layer is always empty — policy comes from `cooldown.toml`/global/CLI.

pub mod ecosystem;
pub mod gocmd;
pub mod proxy;
pub mod semver;

pub use ecosystem::{GO_ID, GoEcosystem};
pub use proxy::GoProxy;
