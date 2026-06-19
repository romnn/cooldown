//! The Elixir/Hex tool adapter. It reads the resolved graph from `mix.lock`, the direct deps from
//! `mix.exs`, resolves publish times from hex.pm, computes verdicts in the cooldown core, and
//! drives `mix` only to apply a chosen version.

pub mod lock;
pub mod registry;
pub mod tool;
pub mod version;

pub use registry::{HEXPM, Hex};
pub use tool::{HEX_ID, HexTool};
