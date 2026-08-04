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
mod publication;
mod staging;
pub mod tool;
pub mod version;

use cooldown_core::ToolId;

/// The [`ToolId`] identifying the Rust/Cargo tool (`"cargo"`).
pub const CARGO_ID: ToolId = ToolId("cargo");

/// The project-relative marker for an interrupted Cargo mutation transaction.
pub const RECOVERY_MARKER: &str = publication::RECOVERY_MARKER;

/// Whether a file name uses Cargo mutation recovery's reserved artifact shape.
///
/// This recognizes the public transaction marker, private transaction state, and private
/// publication names that may remain after an interrupted publication.
#[must_use]
pub fn is_recovery_artifact_name(name: &str) -> bool {
    publication::is_recovery_artifact_name(name)
}

/// Settles an interrupted Cargo mutation transaction rooted at `project_root`.
///
/// This recovery-only entry point performs no manifest parsing, registry setup, or Cargo command.
/// The caller must hold exclusive access for the project root.
///
/// # Errors
///
/// Returns a [`cooldown_core::CoreError`] when recovery state is malformed, belongs to another
/// project, no longer matches the tracked manifests and lock, or cannot be read or settled safely.
pub fn recover_interrupted_mutation(
    project_root: &camino::Utf8Path,
) -> cooldown_core::Result<cooldown_core::MutationRecovery> {
    let project = cooldown_core::Project {
        root: project_root.to_owned(),
        manifest: project_root.join("Cargo.toml"),
        kind: CARGO_ID,
        exclude_newer: None,
    };
    publication::recover_pending(&project)
}

pub use index::CratesIoIndex;
pub use tool::CargoTool;
