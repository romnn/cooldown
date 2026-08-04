//! Publishes one accepted Cargo project state under durable recovery evidence.

use super::artifact::{
    CommitOutcome, PublicationError, PublicationOutcome, ensure_no_orphan_artifacts, path_exists,
    pending_recovery, private_publication_paths, publish_exclusive_bytes, publish_exclusive_json,
    recovery_anchor_path, recovery_path, remove_file, remove_recovery_anchor,
    remove_transaction_marker, validate_record_size,
};
use super::model::{ProjectRecoveryRecord, RecoveryAnchor};
use super::recover::TrustedRecoveryRecord;
use camino::Utf8Path;
use cooldown_core::{
    AcceptedProjectState, AcceptedPublication, CoreError, Diagnostic, DiagnosticKind, Project,
    Result, fs::RecoveryAuthority,
};

pub(crate) fn publish_accepted(
    project: &Project,
    accepted: &AcceptedProjectState,
    authority: &RecoveryAuthority,
) -> Result<AcceptedPublication> {
    publish_accepted_with(project, accepted, authority, AcceptedProjectState::install)
}

pub(super) fn publish_accepted_with<F>(
    project: &Project,
    accepted: &AcceptedProjectState,
    authority: &RecoveryAuthority,
    install: F,
) -> Result<AcceptedPublication>
where
    F: FnOnce(&AcceptedProjectState, &Utf8Path) -> Result<()>,
{
    publish_accepted_with_marker(
        project,
        accepted,
        authority,
        install,
        remove_transaction_marker,
    )
}

pub(super) fn publish_accepted_with_marker<F, R>(
    project: &Project,
    accepted: &AcceptedProjectState,
    authority: &RecoveryAuthority,
    install: F,
    remove_marker: R,
) -> Result<AcceptedPublication>
where
    F: FnOnce(&AcceptedProjectState, &Utf8Path) -> Result<()>,
    R: FnOnce(&Utf8Path) -> Result<CommitOutcome>,
{
    if accepted.changed_files().next().is_none() {
        return Ok(AcceptedPublication::Published {
            warnings: Vec::new(),
        });
    }
    authority.validate_current()?;
    if authority.project() != project.root {
        return Err(CoreError::LockConflict(format!(
            "Cargo recovery authority belongs to {}, not {}",
            authority.project(),
            project.root
        )));
    }
    accepted.validate_source(&project.root)?;
    let lock_path = project.root.join("Cargo.lock");
    let recovery_path = recovery_path(&lock_path);
    let anchor_path = recovery_anchor_path(authority)?;
    ensure_no_orphan_artifacts(&lock_path)?;
    if path_exists(&anchor_path)? || !private_publication_paths(&anchor_path)?.is_empty() {
        return Err(CoreError::LockUnreadable(format!(
            "unsettled Cargo recovery authority at {anchor_path}; run `cooldown recover -C {} --cargo` before publishing another project state",
            project.root
        )));
    }
    let record = ProjectRecoveryRecord::new(project, accepted)?;
    record.validate(project, &recovery_path)?;
    let record_contents =
        serde_json::to_vec(&record).map_err(|error| CoreError::Serialization(error.to_string()))?;
    validate_record_size(record_contents.len(), &recovery_path)?;
    let anchor = RecoveryAnchor::new(project, &record_contents)?;
    let anchor_publication = match publish_exclusive_json(&anchor_path, &anchor) {
        Ok(outcome) => outcome,
        Err(PublicationError::NotPublished(error)) => return Err(error),
        Err(error @ PublicationError::DurabilityUncertain(_)) => {
            let error = error.into_core_error(&anchor_path);
            return Err(pending_recovery(&anchor_path, &error));
        }
    };
    let publication = match publish_exclusive_bytes(&recovery_path, &record_contents) {
        Ok(outcome) => outcome,
        Err(PublicationError::NotPublished(error)) => match remove_recovery_anchor(&anchor_path) {
            Ok(CommitOutcome::Committed) => return Err(error),
            Ok(CommitOutcome::DurabilityUncertain(cleanup_error)) | Err(cleanup_error) => {
                return Err(pending_recovery(&anchor_path, &cleanup_error));
            }
        },
        Err(error @ PublicationError::DurabilityUncertain(_)) => {
            let error = error.into_core_error(&recovery_path);
            return Err(pending_recovery(&recovery_path, &error));
        }
    };
    if let Err(error) = install(accepted, &project.root) {
        return Err(pending_recovery(&recovery_path, &error));
    }
    let trusted = TrustedRecoveryRecord::open(project, &recovery_path, authority)
        .map_err(|error| pending_recovery(&recovery_path, &error))?;
    accepted
        .validate_candidate(&project.root)
        .map_err(|error| pending_recovery(&recovery_path, &error))?;
    trusted
        .validate_evidence()
        .map_err(|error| pending_recovery(&recovery_path, &error))?;
    let marker = match remove_marker(&recovery_path) {
        Ok(marker) => marker,
        Err(error) => {
            return Ok(AcceptedPublication::PublishedPendingRecovery {
                warnings: Vec::new(),
                error: pending_recovery(&recovery_path, &error),
            });
        }
    };
    let mut warnings = Vec::new();
    match marker {
        CommitOutcome::Committed => match trusted
            .anchor
            .validate_current()
            .and_then(|()| remove_recovery_anchor(&anchor_path))
        {
            Ok(CommitOutcome::Committed) => {}
            Ok(CommitOutcome::DurabilityUncertain(error)) | Err(error) => {
                warnings.push(Diagnostic::new(
                    DiagnosticKind::Filesystem,
                    format!(
                        "accepted Cargo project state is visible, but recovery-authority cleanup is incomplete at {anchor_path}: {error}"
                    ),
                ));
            }
        },
        CommitOutcome::DurabilityUncertain(error) => warnings.push(Diagnostic::new(
            DiagnosticKind::Filesystem,
            format!(
                "accepted Cargo project state is visible, but recovery-marker removal durability is uncertain; retained recovery authority at {anchor_path}: {error}"
            ),
        )),
    }
    retry_private_cleanup(publication, &mut warnings, remove_file);
    retry_private_cleanup(anchor_publication, &mut warnings, remove_file);
    Ok(AcceptedPublication::Published { warnings })
}

pub(super) fn retry_private_cleanup<F>(
    publication: PublicationOutcome,
    warnings: &mut Vec<Diagnostic>,
    cleanup: F,
) where
    F: FnOnce(&Utf8Path) -> Result<()>,
{
    let PublicationOutcome::CleanupPending { path, error } = publication else {
        return;
    };
    if let Err(retry_error) = cleanup(&path) {
        warnings.push(Diagnostic::new(
            DiagnosticKind::Filesystem,
            format!(
                "accepted Cargo project state is visible, but its private publication artifact remains at {path}: initial cleanup failed: {error}; retry failed: {retry_error}"
            ),
        ));
    }
}
