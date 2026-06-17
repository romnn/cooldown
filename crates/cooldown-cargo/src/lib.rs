//! The Rust/Cargo ecosystem adapter: `Cargo.lock`/`cargo metadata` for the resolved graph,
//! crates.io sparse-index publish times, and `cargo`-driven resolution/apply. Cargo has no native
//! cooldown engine, so verdicts are computed in the core; cargo is used only to resolve/apply a
//! chosen window. `[package.metadata.cooldown]` is read as a native config layer.

pub mod cargocmd;
pub mod ecosystem;
pub mod index;
pub mod version;

pub use ecosystem::{CARGO_ID, CargoEcosystem};
pub use index::CratesIoIndex;
