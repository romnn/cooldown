//! The Rust/Cargo tool adapter: `Cargo.lock`/`cargo metadata` for the resolved graph,
//! crates.io sparse-index publish times, and `cargo`-driven resolution/apply. Cargo has no native
//! cooldown engine, so verdicts are computed in the core; cargo is used only to resolve/apply a
//! chosen window. `[package.metadata.cooldown]` is read as a native config layer.

pub mod cargocmd;
mod edges;
pub mod index;
mod lockfile;
mod manifest;
mod native;
pub mod tool;
pub mod version;

use cooldown_core::ToolId;

/// The [`ToolId`] identifying the Rust/Cargo tool (`"cargo"`).
pub const CARGO_ID: ToolId = ToolId("cargo");

/// The project-relative marker for an interrupted Cargo mutation transaction.
pub const RECOVERY_MARKER: &str = edges::recovery::RECOVERY_MARKER;

/// Restores an interrupted Cargo mutation transaction rooted at `project_root`.
///
/// This recovery-only entry point performs no manifest parsing, registry setup, or Cargo command.
/// The caller must hold exclusive access for the project root.
///
/// # Errors
///
/// Returns a [`cooldown_core::CoreError`] when recovery state is malformed, belongs to another
/// project, no longer matches the tracked manifests and lock, or cannot be read or restored safely.
pub fn recover_interrupted_mutation(
    project_root: &camino::Utf8Path,
) -> cooldown_core::Result<bool> {
    let project = cooldown_core::Project {
        root: project_root.to_owned(),
        manifest: project_root.join("Cargo.toml"),
        kind: CARGO_ID,
        exclude_newer: None,
    };
    edges::enforce::recover_pending(&project)
}

pub use index::CratesIoIndex;
pub use tool::CargoTool;
