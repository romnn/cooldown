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

/// The project roots an explicit or repository-wide Cargo recovery may settle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryScope {
    /// Every Cargo project beneath the repository root.
    Repository(camino::Utf8PathBuf),
    /// The selected project subtree and relevant ancestor projects.
    Explicit(camino::Utf8PathBuf),
}

impl RecoveryScope {
    /// Returns the root used for artifact and project discovery.
    #[must_use]
    pub fn root(&self) -> &camino::Utf8Path {
        match self {
            RecoveryScope::Repository(root) | RecoveryScope::Explicit(root) => root,
        }
    }

    /// Returns whether `project` belongs to this recovery request.
    #[must_use]
    pub fn includes(&self, project: &camino::Utf8Path) -> bool {
        match self {
            RecoveryScope::Repository(root) => project.starts_with(root),
            RecoveryScope::Explicit(target) => {
                project.starts_with(target) || target.starts_with(project)
            }
        }
    }

    /// Returns whether malformed authority without a project identity must fail discovery.
    #[must_use]
    pub(crate) const fn requires_complete_authority_scan(&self) -> bool {
        matches!(self, RecoveryScope::Repository(_))
    }

    pub(crate) fn includes_unknown_authority(&self, authority_name: &str) -> bool {
        let RecoveryScope::Explicit(target) = self else {
            return true;
        };
        target.ancestors().any(|ancestor| {
            authority_name
                == format!(
                    "{:016x}.cargo-recovery.anchor",
                    cooldown_core::fs::fnv1a_64(ancestor.as_str())
                )
        })
    }
}

/// Finds trusted authority projects included by `scope` in its Git repository.
///
/// # Errors
///
/// Returns a [`cooldown_core::CoreError`] when trusted authority is malformed, names an invalid
/// project, or cannot be inspected safely.
pub fn recovery_authority_projects(
    scope: &RecoveryScope,
) -> cooldown_core::Result<Vec<camino::Utf8PathBuf>> {
    publication::recovery_authority_projects(scope)
}

/// Returns whether a file name uses Cargo mutation recovery's reserved artifact shape.
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
/// It acquires exclusive project access before inspecting or consuming recovery evidence.
/// Recovery fails closed outside a Git worktree and on platforms where cooldown cannot prove the
/// recovery authority is private to the current user.
///
/// # Errors
///
/// Returns a [`cooldown_core::CoreError`] when recovery state is malformed, belongs to another
/// project, no longer matches the tracked manifests and lock, or cannot be read or settled safely.
pub fn recover_interrupted_mutation(
    project_root: &camino::Utf8Path,
) -> cooldown_core::Result<cooldown_core::MutationRecovery> {
    let lease = cooldown_core::fs::ProjectWriteLease::acquire(project_root)?;
    let project = cooldown_core::Project {
        root: project_root.to_owned(),
        manifest: project_root.join("Cargo.toml"),
        kind: CARGO_ID,
        exclude_newer: None,
    };
    let authority = publication::require_recovery_authority(&project, lease.coordination())?;
    publication::recover_pending(&project, authority)
}

pub use index::CratesIoIndex;
pub use tool::CargoTool;

#[cfg(test)]
mod tests {
    use super::RecoveryScope;
    use camino::Utf8PathBuf;

    #[test]
    fn recovery_scope_distinguishes_repository_and_explicit_roots() {
        let repository = RecoveryScope::Repository(Utf8PathBuf::from("/repo"));
        assert!(repository.includes(camino::Utf8Path::new("/repo/project")));
        assert!(repository.includes(camino::Utf8Path::new("/repo/sibling")));
        assert!(!repository.includes(camino::Utf8Path::new("/sibling")));

        let explicit = RecoveryScope::Explicit(Utf8PathBuf::from("/repo/project"));
        assert!(explicit.includes(camino::Utf8Path::new("/repo")));
        assert!(explicit.includes(camino::Utf8Path::new("/repo/project/member")));
        assert!(!explicit.includes(camino::Utf8Path::new("/repo/sibling")));
    }
}
