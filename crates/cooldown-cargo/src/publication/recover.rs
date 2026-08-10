//! Settles trusted interrupted Cargo publications and consumes their evidence safely.

use super::artifact::{
    CommitOutcome, MAX_RECOVERY_ANCHOR_BYTES, MAX_RECOVERY_RECORD_BYTES, TrustedArtifact,
    cleanup_linked_publication_files, ensure_no_orphan_artifacts, path_exists,
    private_publication_paths, recovery_anchor_path, recovery_path, remove_recovery_anchor,
    remove_transaction_marker,
};
use super::model::{ExpectedProjectState, ProjectRecoveryRecord, RecoveryAnchor};
use camino::Utf8Path;
use cooldown_core::{
    CoreError, Diagnostic, DiagnosticKind, MutationRecovery, Project, RecoveryDisposition, Result,
    fs::RecoveryAuthority,
};

pub(super) struct TrustedRecoveryRecord {
    pub(super) record: TrustedArtifact,
    pub(super) anchor: TrustedArtifact,
    pub(super) authority: RecoveryAuthority,
}

impl TrustedRecoveryRecord {
    pub(super) fn open(
        project: &Project,
        record_path: &Utf8Path,
        authority: &RecoveryAuthority,
    ) -> Result<Self> {
        authority.validate_current()?;
        let anchor_path = recovery_anchor_path(authority)?;
        let anchor = TrustedArtifact::open(&anchor_path, MAX_RECOVERY_ANCHOR_BYTES, true)?;
        let anchor_record: RecoveryAnchor = anchor.decode("recovery authority")?;
        anchor_record.validate_without_record(project, &anchor_path)?;
        let record = TrustedArtifact::open(record_path, MAX_RECOVERY_RECORD_BYTES, false)?;
        anchor_record.validate(project, &record.contents, &anchor_path)?;
        Ok(TrustedRecoveryRecord {
            record,
            anchor,
            authority: authority.clone(),
        })
    }

    pub(super) fn decode<T: serde::de::DeserializeOwned>(
        &self,
        record_path: &Utf8Path,
    ) -> Result<T> {
        serde_json::from_slice(&self.record.contents).map_err(|error| {
            CoreError::LockUnreadable(format!(
                "invalid Cargo.lock recovery record at {record_path}: {error}; left all files untouched"
            ))
        })
    }

    pub(super) fn validate_evidence(&self) -> Result<()> {
        self.authority.validate_current()?;
        self.anchor.validate_current()?;
        self.record.validate_current()
    }
}

pub(crate) fn recover_pending(
    project: &Project,
    authority: &RecoveryAuthority,
) -> Result<MutationRecovery> {
    authority.validate_current()?;
    if authority.project() != project.root {
        return Err(CoreError::LockConflict(format!(
            "Cargo recovery authority belongs to {}, not {}",
            authority.project(),
            project.root
        )));
    }
    let lock_path = project.root.join("Cargo.lock");
    let recovery_path = recovery_path(&lock_path);
    let anchor_path = recovery_anchor_path(authority)?;
    if !path_exists(&recovery_path)? {
        ensure_no_orphan_artifacts(&lock_path)?;
        return recover_orphan_anchor(project, &anchor_path);
    }
    let trusted = TrustedRecoveryRecord::open(project, &recovery_path, authority)?;
    recover_project_publication(project, &recovery_path, &trusted)
}

pub(super) fn recover_orphan_anchor(
    project: &Project,
    anchor_path: &Utf8Path,
) -> Result<MutationRecovery> {
    if !path_exists(anchor_path)? {
        return recover_private_anchor_publications(project, anchor_path);
    }
    let artifact = TrustedArtifact::open(anchor_path, MAX_RECOVERY_ANCHOR_BYTES, true)?;
    let anchor: RecoveryAnchor = artifact.decode("recovery authority")?;
    anchor.validate_without_record(project, anchor_path)?;
    cleanup_linked_publication_files(anchor_path)?;
    artifact.validate_current()?;
    let mut recovery = MutationRecovery::settled(RecoveryDisposition::CleanupOnly);
    match remove_recovery_anchor(anchor_path)? {
        CommitOutcome::Committed => {}
        CommitOutcome::DurabilityUncertain(error) => recovery.warnings.push(Diagnostic::new(
            DiagnosticKind::Filesystem,
            format!(
                "removed stale Cargo recovery authority at {anchor_path}, but directory durability is uncertain: {error}"
            ),
        )),
    }
    Ok(recovery)
}

pub(super) fn recover_private_anchor_publications(
    project: &Project,
    anchor_path: &Utf8Path,
) -> Result<MutationRecovery> {
    let mut artifacts = Vec::new();
    for path in private_publication_paths(anchor_path)? {
        let artifact = TrustedArtifact::open(&path, MAX_RECOVERY_ANCHOR_BYTES, true)?;
        let anchor: RecoveryAnchor = artifact.decode("recovery authority")?;
        anchor.validate_without_record(project, &path)?;
        artifacts.push(artifact);
    }
    if artifacts.is_empty() {
        return Ok(MutationRecovery::settled(RecoveryDisposition::Unchanged));
    }
    let mut recovery = MutationRecovery::settled(RecoveryDisposition::CleanupOnly);
    for artifact in artifacts {
        artifact.validate_current()?;
        if let CommitOutcome::DurabilityUncertain(error) =
            remove_transaction_marker(&artifact.path)?
        {
            recovery.warnings.push(Diagnostic::new(
                DiagnosticKind::Filesystem,
                format!(
                    "removed stale private Cargo recovery authority at {}, but directory durability is uncertain: {error}",
                    artifact.path
                ),
            ));
        }
    }
    Ok(recovery)
}

pub(super) fn recover_project_publication(
    project: &Project,
    recovery_path: &Utf8Path,
    trusted: &TrustedRecoveryRecord,
) -> Result<MutationRecovery> {
    recover_project_publication_with(project, recovery_path, trusted, || Ok(()))
}

pub(super) fn recover_project_publication_with<F>(
    project: &Project,
    recovery_path: &Utf8Path,
    trusted: &TrustedRecoveryRecord,
    before_final_validation: F,
) -> Result<MutationRecovery>
where
    F: FnOnce() -> Result<()>,
{
    let record: ProjectRecoveryRecord = trusted.decode(recovery_path)?;
    record.validate(project, recovery_path)?;
    let original = record.original_journal(&project.root)?;
    let live = original.capture_state()?;
    let (saw_original, saw_candidate) = record.classify_state(project, &live, recovery_path)?;
    let (disposition, expected) = if saw_original && saw_candidate {
        original.restore_if_unchanged(&live)?;
        (
            RecoveryDisposition::Restored,
            ExpectedProjectState::Original,
        )
    } else if saw_candidate {
        (
            RecoveryDisposition::Accepted,
            ExpectedProjectState::Candidate,
        )
    } else {
        (
            RecoveryDisposition::CleanupOnly,
            ExpectedProjectState::Original,
        )
    };
    cleanup_linked_publication_files(recovery_path)?;
    before_final_validation()?;
    let final_live = original.capture_state()?;
    record.validate_expected_state(project, &final_live, expected, recovery_path)?;
    finish_recovery_authority(MutationRecovery::settled(disposition), trusted)
}

pub(super) fn finish_recovery_authority(
    mut recovery: MutationRecovery,
    trusted: &TrustedRecoveryRecord,
) -> Result<MutationRecovery> {
    trusted.validate_evidence()?;
    let recovery_path = &trusted.record.path;
    let anchor_path = &trusted.anchor.path;
    match remove_transaction_marker(recovery_path)? {
        CommitOutcome::Committed => {
            trusted.anchor.validate_current()?;
            match remove_recovery_anchor(anchor_path) {
                Ok(CommitOutcome::Committed) => {}
                Ok(CommitOutcome::DurabilityUncertain(error)) | Err(error) => {
                    recovery.warnings.push(Diagnostic::new(
                        DiagnosticKind::Filesystem,
                        format!(
                            "recovery settled the visible Cargo project files, but recovery-authority cleanup is incomplete at {anchor_path}: {error}"
                        ),
                    ));
                }
            }
        }
        CommitOutcome::DurabilityUncertain(error) => recovery.warnings.push(Diagnostic::new(
            DiagnosticKind::Filesystem,
            format!(
                "recovery settled the visible Cargo project files, but marker-removal durability is uncertain at {recovery_path}; retained recovery authority at {anchor_path}: {error}"
            ),
        )),
    }
    Ok(recovery)
}

/// Refuses a read while an interrupted mutation still owns recovery state.
pub(crate) fn ensure_no_pending(project: &Project) -> Result<()> {
    let lock_path = project.root.join("Cargo.lock");
    let recovery_path = recovery_path(&lock_path);
    if path_exists(&recovery_path)? {
        return Err(CoreError::StaleLock(format!(
            "pending Cargo.lock transaction at {recovery_path}; run `cooldown recover` to restore it"
        )));
    }
    ensure_no_orphan_artifacts(&lock_path)
}
