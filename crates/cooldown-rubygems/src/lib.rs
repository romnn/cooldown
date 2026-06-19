//! The Ruby/Bundler tool adapter. It reads the resolved graph from `Gemfile.lock`, resolves
//! publish times from rubygems.org, computes verdicts in the cooldown core, and drives `bundle`
//! only to apply a chosen version.

pub mod lock;
pub mod registry;
pub mod tool;
pub mod version;

pub use registry::{RUBYGEMS, RubyGems};
pub use tool::{BUNDLER_ID, BundlerTool};
