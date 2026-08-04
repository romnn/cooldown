//! Discovers and validates Cargo recovery authority for requested project scopes.

use super::artifact::{
    MAX_RECOVERY_ANCHOR_BYTES, TrustedArtifact, publication_target, recovery_anchor_path,
    untrusted_record,
};
use super::model::{RECOVERY_ANCHOR_FORMAT, RecoveryAnchor, is_sha256_digest};
use camino::Utf8PathBuf;
use cooldown_core::{CoreError, Project, Result, fs::RecoveryAuthority};

pub(crate) fn require_recovery_authority<'a>(
    project: &Project,
    coordination: &'a cooldown_core::fs::ProjectCoordination,
) -> Result<&'a RecoveryAuthority> {
    coordination.validate_current()?;
    if coordination.project() != project.root {
        return Err(CoreError::LockConflict(format!(
            "project coordination belongs to {}, not {}",
            coordination.project(),
            project.root
        )));
    }
    coordination.recovery_authority().ok_or_else(|| {
        CoreError::LockUnreadable(format!(
            "recoverable Cargo publication is unavailable for {}; recovery requires a Git worktree on a platform where cooldown can verify owner-private authority",
            project.root
        ))
    })
}

pub(crate) fn recovery_authority_projects(
    scope: &crate::RecoveryScope,
) -> Result<crate::RecoveryAuthorityDiscovery> {
    let coordination = cooldown_core::fs::ProjectCoordination::resolve(scope.root())?;
    let Some(scan_authority) = coordination.recovery_authority() else {
        return Ok(crate::RecoveryAuthorityDiscovery {
            projects: Vec::new(),
            warnings: Vec::new(),
        });
    };
    let entries = std::fs::read_dir(scan_authority.directory())?;
    let mut projects = Vec::new();
    let mut warnings = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let target = publication_target(&name).unwrap_or(&name);
        if !target.ends_with(".cargo-recovery.anchor") {
            continue;
        }
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            CoreError::PathEncoding(format!(
                "non-UTF-8 recovery authority path: {}",
                path.display()
            ))
        })?;
        let untrusted = match TrustedArtifact::open(&path, MAX_RECOVERY_ANCHOR_BYTES, false) {
            Ok(artifact) => artifact,
            Err(error) if scope.includes_unknown_authority(target) => return Err(error),
            Err(error) => {
                warnings.push(crate::RecoveryDiscoveryWarning { path, error });
                continue;
            }
        };
        let anchor: RecoveryAnchor = match untrusted.decode("recovery authority") {
            Ok(anchor) => anchor,
            Err(error) if scope.requires_complete_authority_scan() => return Err(error),
            Err(error) => {
                warnings.push(crate::RecoveryDiscoveryWarning { path, error });
                continue;
            }
        };
        let project_root = Utf8PathBuf::from(anchor.project_root.clone());
        if !scope.includes(&project_root) {
            continue;
        }
        let artifact = TrustedArtifact::open(&path, MAX_RECOVERY_ANCHOR_BYTES, true)?;
        let anchor: RecoveryAnchor = artifact.decode("recovery authority")?;
        if anchor.format != RECOVERY_ANCHOR_FORMAT || !is_sha256_digest(&anchor.record_digest) {
            return Err(untrusted_record(&path));
        }
        let project = Project {
            root: project_root.clone(),
            manifest: project_root.join("Cargo.toml"),
            kind: crate::CARGO_ID,
            exclude_newer: None,
        };
        let project_coordination = cooldown_core::fs::ProjectCoordination::resolve(&project_root)?;
        let project_authority = require_recovery_authority(&project, &project_coordination)?;
        let expected = recovery_anchor_path(project_authority)?;
        if expected.file_name() != Some(target)
            || project_authority.directory() != scan_authority.directory()
        {
            return Err(untrusted_record(&path));
        }
        anchor.validate_without_record(&project, &path)?;
        projects.push(project_root);
    }
    projects.sort();
    projects.dedup();
    warnings.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(crate::RecoveryAuthorityDiscovery { projects, warnings })
}
