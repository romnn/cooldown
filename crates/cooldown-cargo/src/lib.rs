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

/// A canonical project scope for explicit or repository-wide Cargo recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryScope {
    root: camino::Utf8PathBuf,
    kind: RecoveryScopeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryScopeKind {
    Repository,
    Explicit,
}

/// Trusted authority projects and non-fatal artifacts that could not be attributed to a project.
#[derive(Debug)]
pub struct RecoveryAuthorityDiscovery {
    /// Canonical project roots whose authority belongs to the requested scope.
    pub projects: Vec<camino::Utf8PathBuf>,
    /// Malformed authority artifacts in the shared coordination namespace.
    pub warnings: Vec<RecoveryDiscoveryWarning>,
}

/// A recovery authority artifact that could not be decoded enough to determine its project.
#[derive(Debug)]
pub struct RecoveryDiscoveryWarning {
    /// The untrusted artifact path.
    pub path: camino::Utf8PathBuf,
    /// The validation failure that prevented attribution.
    pub error: cooldown_core::CoreError,
}

impl RecoveryScope {
    /// Canonicalizes a repository root whose descendant Cargo projects may be recovered.
    ///
    /// # Errors
    ///
    /// Returns a filesystem or path-encoding error when `root` cannot be canonicalized.
    pub fn repository(root: &camino::Utf8Path) -> cooldown_core::Result<Self> {
        Self::canonical(root, RecoveryScopeKind::Repository)
    }

    /// Canonicalizes an explicit project subtree and its relevant ancestor projects.
    ///
    /// # Errors
    ///
    /// Returns a filesystem or path-encoding error when `root` cannot be canonicalized.
    pub fn explicit(root: &camino::Utf8Path) -> cooldown_core::Result<Self> {
        Self::canonical(root, RecoveryScopeKind::Explicit)
    }

    fn canonical(root: &camino::Utf8Path, kind: RecoveryScopeKind) -> cooldown_core::Result<Self> {
        let canonical = std::fs::canonicalize(root)?;
        let root = camino::Utf8PathBuf::from_path_buf(canonical).map_err(|path| {
            cooldown_core::CoreError::PathEncoding(format!(
                "non-UTF-8 recovery scope: {}",
                path.display()
            ))
        })?;
        Ok(RecoveryScope { root, kind })
    }

    /// Returns the root used for artifact and project discovery.
    #[must_use]
    pub fn root(&self) -> &camino::Utf8Path {
        &self.root
    }

    /// Returns whether `project` belongs to this recovery request.
    #[must_use]
    pub fn includes(&self, project: &camino::Utf8Path) -> bool {
        match self.kind {
            RecoveryScopeKind::Repository => project.starts_with(&self.root),
            RecoveryScopeKind::Explicit => {
                project.starts_with(&self.root) || self.root.starts_with(project)
            }
        }
    }

    /// Returns whether malformed authority without a project identity must fail discovery.
    #[must_use]
    pub(crate) const fn requires_complete_authority_scan(&self) -> bool {
        matches!(self.kind, RecoveryScopeKind::Repository)
    }

    pub(crate) fn includes_unknown_authority(&self, authority_name: &str) -> bool {
        if !matches!(self.kind, RecoveryScopeKind::Explicit) {
            return true;
        }
        self.root.ancestors().any(|ancestor| {
            authority_name
                == format!(
                    "{:016x}.cargo-recovery.anchor",
                    cooldown_core::fs::fnv1a_64(ancestor.as_str())
                )
        })
    }
}

/// Discovers trusted authority projects and unattributable artifacts in `scope`'s Git repository.
///
/// # Errors
///
/// Returns a [`cooldown_core::CoreError`] when trusted authority is malformed, names an invalid
/// project, or cannot be inspected safely.
pub fn recovery_authority_projects(
    scope: &RecoveryScope,
) -> cooldown_core::Result<RecoveryAuthorityDiscovery> {
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
    let project_root = lease.coordination().project().to_owned();
    let project = cooldown_core::Project {
        root: project_root.clone(),
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
    use color_eyre::eyre;

    #[test]
    fn recovery_scope_canonicalizes_repository_and_explicit_roots() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary directory is not UTF-8"))?;
        std::fs::create_dir_all(root.join("project/member"))?;
        std::fs::create_dir_all(root.join("sibling"))?;

        let repository = RecoveryScope::repository(&root.join("project/.."))?;
        assert!(repository.includes(&root.join("project")));
        assert!(repository.includes(&root.join("sibling")));

        let explicit = RecoveryScope::explicit(&root.join("project/member/.."))?;
        assert!(explicit.includes(root));
        assert!(explicit.includes(&root.join("project/member")));
        assert!(!explicit.includes(&root.join("sibling")));
        Ok(())
    }
}
