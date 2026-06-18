//! The Rust/Cargo tool adapter: `Cargo.lock`/`cargo metadata` for the resolved graph,
//! crates.io sparse-index publish times, and `cargo`-driven resolution/apply. Cargo has no native
//! cooldown engine, so verdicts are computed in the core; cargo is used only to resolve/apply a
//! chosen window. `[package.metadata.cooldown]` is read as a native config layer.

pub mod cargocmd;
pub mod index;
pub mod tool;
pub mod version;

pub use index::CratesIoIndex;
pub use tool::{CARGO_ID, CargoTool};
