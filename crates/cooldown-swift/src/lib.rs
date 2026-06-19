//! The Swift Package Manager tool adapter. It reads the resolved pins from `Package.resolved`,
//! resolves publish times from GitHub Releases (`SwiftPM` has no central package index with publish
//! times), computes verdicts in the cooldown core, and drives `swift` only to apply.

pub mod lock;
pub mod registry;
pub mod tool;
pub mod version;

pub use registry::{GITHUB, GitHubReleases};
pub use tool::{SWIFT_ID, SwiftTool};
