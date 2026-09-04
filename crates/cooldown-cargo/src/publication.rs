//! Publishes accepted Cargo states and recovers interrupted source transactions.

mod artifact;
mod discovery;
mod model;
mod publish;
mod recover;

pub(crate) use artifact::{has_project_recovery_artifacts, is_recovery_artifact_name};
pub(crate) use discovery::{recovery_authority_projects, require_recovery_authority};
pub(crate) use model::RECOVERY_MARKER;
pub(crate) use publish::publish_accepted;
pub(crate) use recover::{ensure_no_pending, recover_pending};

const RECOVERY_ANCHOR_SUFFIX: &str = ".cargo-recovery.anchor";

pub(super) fn recovery_anchor_name(project: &camino::Utf8Path) -> String {
    format!(
        "{:016x}{RECOVERY_ANCHOR_SUFFIX}",
        cooldown_core::fs::fnv1a_64(project.as_str())
    )
}

#[cfg(test)]
use camino::{Utf8Path, Utf8PathBuf};
#[cfg(test)]
use cooldown_core::{AcceptedProjectState, CoreError, Project, ProjectMutationJournal};
#[cfg(all(test, unix))]
use cooldown_core::{
    AcceptedPublication, MutationRecovery, RecoveryDisposition, Result, fs::RecoveryAuthority,
};
#[cfg(all(test, unix))]
use std::fs::File;

#[cfg(test)]
mod tests {
    use super::artifact::{
        CommitOutcome, MAX_RECOVERY_RECORD_BYTES, PublicationError, PublicationOutcome,
        RemovalError, TrustedArtifact, cleanup_linked_publication_files,
        ensure_no_orphan_artifacts, publish_exclusive_json, publish_exclusive_json_with,
        recovery_path, remove_file, remove_file_with, remove_transaction_marker_with,
        validate_record_size,
    };
    use super::model::{ProjectRecoveryRecord, RecoveryAnchor};
    use super::publish::retry_private_cleanup;
    // Publication and recovery evidence only exists where the platform grants recovery authority,
    // so the tests driving it — and the items only they reach — are Unix-only.
    #[cfg(unix)]
    use super::artifact::{
        create_synced_private_file, publish_exclusive_bytes, recovery_anchor_path,
    };
    #[cfg(unix)]
    use super::publish::{publish_accepted_with, publish_accepted_with_marker};
    #[cfg(unix)]
    use super::recover::{TrustedRecoveryRecord, recover_project_publication_with};
    use super::*;
    use crate::CARGO_ID;
    use crate::test_support::canonical_root;
    use color_eyre::eyre;

    fn project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            kind: CARGO_ID,
            manifest: root.join("Cargo.toml"),
            exclude_newer: None,
        }
    }

    fn setup() -> (tempfile::TempDir, Project, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = canonical_root(&dir).expect("canonical temp path");
        std::fs::create_dir_all(root.join(".git")).expect("create Git directory");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write Git HEAD");
        let project = project(&root);
        let lock_path = root.join("Cargo.lock");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"original").expect("write lock");
        (dir, project, lock_path)
    }

    /// Resolves the trusted authority that publication and recovery evidence is anchored in.
    ///
    /// Coordination only grants one where the platform can prove the namespace is private to the
    /// current user, so every test reaching this helper is `#[cfg(unix)]`: off Unix the authority
    /// does not exist, Cargo selects in-place execution instead, and there is no publication to
    /// assert against.
    #[cfg(unix)]
    fn recovery_authority(project: &Project) -> Result<RecoveryAuthority> {
        let coordination = cooldown_core::fs::ProjectCoordination::resolve(&project.root)?;
        coordination
            .recovery_authority()
            .cloned()
            .ok_or_else(|| CoreError::System("test project has no recovery authority".to_string()))
    }

    #[cfg(unix)]
    fn test_publish(
        project: &Project,
        accepted: &AcceptedProjectState,
    ) -> Result<AcceptedPublication> {
        publish_accepted(project, accepted, &recovery_authority(project)?)
    }

    #[cfg(unix)]
    fn test_recover(project: &Project) -> Result<MutationRecovery> {
        recover_pending(project, &recovery_authority(project)?)
    }

    fn accepted_project_state(
        project: &Project,
        paths: &[(&str, &str)],
    ) -> eyre::Result<AcceptedProjectState> {
        let candidate = tempfile::tempdir()?;
        let candidate_root = Utf8PathBuf::from_path_buf(candidate.path().to_owned())
            .map_err(|path| eyre::eyre!("non-UTF-8 candidate path: {path:?}"))?;
        let mut relative_paths = Vec::new();
        for (path, contents) in paths {
            let relative = Utf8Path::new(path);
            relative_paths.push(relative);
            let target = candidate_root.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(target, contents)?;
        }
        let original = ProjectMutationJournal::capture(&project.root, relative_paths)?;
        let candidate = cooldown_core::ProjectMutationState::capture(&candidate_root, &original)?;
        Ok(AcceptedProjectState::new(
            original,
            candidate,
            cooldown_core::ProjectInputSnapshot::default(),
        )?)
    }

    #[cfg(unix)]
    fn publish_trusted_record<T: serde::Serialize>(
        project: &Project,
        marker: &Utf8Path,
        record: &T,
    ) -> eyre::Result<()> {
        let contents = serde_json::to_vec(record)?;
        let anchor_path = recovery_anchor_path(&recovery_authority(project)?)?;
        let anchor = RecoveryAnchor::new(project, &contents)?;
        publish_exclusive_json(&anchor_path, &anchor)
            .map_err(|error| eyre::eyre!("publish recovery anchor: {error:?}"))?;
        publish_exclusive_bytes(marker, &contents)
            .map_err(|error| eyre::eyre!("publish recovery marker: {error:?}"))?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn accepted_project_state_publishes_once_and_consumes_its_record() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;

        let publication = test_publish(&project, &accepted)?;

        std::assert_matches!(
            publication,
            AcceptedPublication::Published { warnings } if warnings.is_empty()
        );
        assert_eq!(std::fs::read_to_string(&lock_path)?, "accepted");
        assert!(!recovery_path(&lock_path).exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn project_publication_refuses_source_drift_before_creating_a_record() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;
        std::fs::write(&lock_path, "external")?;

        let error = test_publish(&project, &accepted)
            .err()
            .ok_or_else(|| eyre::eyre!("drifted publication succeeded"))?;

        std::assert_matches!(error, CoreError::LockConflict(_));
        assert_eq!(std::fs::read_to_string(&lock_path)?, "external");
        assert!(!recovery_path(&lock_path).exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_restores_a_mixed_whole_project_publication() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let manifest_path = project.root.join("Cargo.toml");
        std::fs::write(&manifest_path, "original manifest")?;
        let accepted = accepted_project_state(
            &project,
            &[
                ("Cargo.lock", "accepted lock"),
                ("Cargo.toml", "accepted manifest"),
            ],
        )?;
        let marker = recovery_path(&lock_path);
        let record = ProjectRecoveryRecord::new(&project, &accepted)?;
        publish_trusted_record(&project, &marker, &record)?;
        std::fs::write(&lock_path, "accepted lock")?;

        std::assert_matches!(
            test_recover(&project)?.disposition,
            RecoveryDisposition::Restored
        );
        assert_eq!(std::fs::read_to_string(&lock_path)?, "original");
        assert_eq!(
            std::fs::read_to_string(&manifest_path)?,
            "original manifest"
        );
        assert!(!marker.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn project_content_cannot_forge_recovery_authority() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let manifest_path = project.root.join("Cargo.toml");
        std::fs::write(&manifest_path, "original manifest")?;
        let accepted = accepted_project_state(
            &project,
            &[
                ("Cargo.lock", "forged lock"),
                ("Cargo.toml", "forged manifest"),
            ],
        )?;
        let marker = recovery_path(&lock_path);
        let record = ProjectRecoveryRecord::new(&project, &accepted)?;
        std::fs::write(&marker, serde_json::to_vec(&record)?)?;
        std::fs::write(&lock_path, "forged lock")?;

        let error = test_recover(&project)
            .err()
            .ok_or_else(|| eyre::eyre!("unanchored recovery record was accepted"))?;

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        assert_eq!(std::fs::read_to_string(&lock_path)?, "forged lock");
        assert_eq!(
            std::fs::read_to_string(&manifest_path)?,
            "original manifest"
        );
        assert!(marker.exists());
        Ok(())
    }

    #[test]
    fn matching_project_local_anchor_cannot_authorize_recovery() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = canonical_root(&directory)?;
        let project = project(&root);
        let lock_path = root.join("Cargo.lock");
        std::fs::write(&lock_path, "original")?;
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "forged")])?;
        let record = ProjectRecoveryRecord::new(&project, &accepted)?;
        let record_contents = serde_json::to_vec(&record)?;
        let marker = recovery_path(&lock_path);
        std::fs::write(&marker, &record_contents)?;
        let coordination = cooldown_core::fs::ProjectCoordination::resolve(&root)?;
        let anchor_path = Utf8PathBuf::from_path_buf(coordination.directory().join(format!(
            "{:016x}.cargo-recovery.anchor",
            cooldown_core::fs::fnv1a_64(root.as_str())
        )))
        .map_err(|path| eyre::eyre!("non-UTF-8 anchor path: {path:?}"))?;
        let anchor = RecoveryAnchor::new(&project, &record_contents)?;
        std::fs::write(&anchor_path, serde_json::to_vec(&anchor)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&anchor_path, std::fs::Permissions::from_mode(0o600))?;
        }

        let error = require_recovery_authority(&project, &coordination)
            .err()
            .ok_or_else(|| eyre::eyre!("project-local authority was trusted"))?;

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        assert_eq!(std::fs::read_to_string(lock_path)?, "original");
        assert!(marker.exists());
        assert!(anchor_path.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn missing_authority_rejects_an_oversized_marker_before_reading_it() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        let file = File::create(&marker)?;
        file.set_len(MAX_RECOVERY_RECORD_BYTES + 1)?;

        let error = test_recover(&project)
            .err()
            .ok_or_else(|| eyre::eyre!("oversized untrusted marker was accepted"))?;

        std::assert_matches!(&error, CoreError::LockUnreadable(_));
        assert!(!error.to_string().contains("safety limit"));
        assert_eq!(
            std::fs::metadata(marker)?.len(),
            MAX_RECOVERY_RECORD_BYTES + 1
        );
        Ok(())
    }

    #[test]
    fn bounded_recovery_reader_rejects_oversized_evidence() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(directory.path().join("record"))
            .map_err(|path| eyre::eyre!("non-UTF-8 record path: {path:?}"))?;
        std::fs::write(&path, b"12345")?;

        let error = TrustedArtifact::open(&path, 4, false)
            .err()
            .ok_or_else(|| eyre::eyre!("oversized recovery evidence was accepted"))?;

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        Ok(())
    }

    #[test]
    fn publication_preflights_recovery_record_size() -> eyre::Result<()> {
        let path = Utf8Path::new("Cargo.lock.cooldown-recovery");

        validate_record_size((MAX_RECOVERY_RECORD_BYTES - 1).try_into()?, path)?;
        let error = validate_record_size((MAX_RECOVERY_RECORD_BYTES + 1).try_into()?, path)
            .err()
            .ok_or_else(|| eyre::eyre!("oversized publication record was accepted"))?;

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_anchor_rejects_marker_content_drift() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;
        let marker = recovery_path(&lock_path);
        let record = ProjectRecoveryRecord::new(&project, &accepted)?;
        publish_trusted_record(&project, &marker, &record)?;
        let mut changed_record = serde_json::to_vec(&record)?;
        changed_record.push(b' ');
        cooldown_core::fs::atomic_write(marker.as_std_path(), &changed_record)?;

        let error = test_recover(&project)
            .err()
            .ok_or_else(|| eyre::eyre!("marker drift was accepted by its recovery anchor"))?;

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        assert_eq!(std::fs::read_to_string(&lock_path)?, "original");
        assert!(marker.exists());
        assert!(recovery_anchor_path(&recovery_authority(&project)?)?.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_retains_evidence_when_project_drifts_before_cleanup() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let manifest_path = project.root.join("Cargo.toml");
        std::fs::write(&manifest_path, "original manifest")?;
        let accepted = accepted_project_state(
            &project,
            &[
                ("Cargo.lock", "accepted lock"),
                ("Cargo.toml", "accepted manifest"),
            ],
        )?;
        let marker = recovery_path(&lock_path);
        let record = ProjectRecoveryRecord::new(&project, &accepted)?;
        publish_trusted_record(&project, &marker, &record)?;
        accepted.install(&project.root)?;
        let authority = recovery_authority(&project)?;
        let anchor_path = recovery_anchor_path(&authority)?;
        let trusted = TrustedRecoveryRecord::open(&project, &marker, &authority)?;

        let error = recover_project_publication_with(&project, &marker, &trusted, || {
            std::fs::write(&manifest_path, "external manifest")?;
            Ok(())
        })
        .err()
        .ok_or_else(|| eyre::eyre!("recovery consumed evidence after project drift"))?;

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        assert_eq!(
            std::fs::read_to_string(&manifest_path)?,
            "external manifest"
        );
        assert!(marker.exists());
        assert!(anchor_path.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_revalidates_the_restored_preimage_before_cleanup() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let manifest_path = project.root.join("Cargo.toml");
        std::fs::write(&manifest_path, "original manifest")?;
        let accepted = accepted_project_state(
            &project,
            &[
                ("Cargo.lock", "accepted lock"),
                ("Cargo.toml", "accepted manifest"),
            ],
        )?;
        let marker = recovery_path(&lock_path);
        let record = ProjectRecoveryRecord::new(&project, &accepted)?;
        publish_trusted_record(&project, &marker, &record)?;
        std::fs::write(&lock_path, "accepted lock")?;
        let authority = recovery_authority(&project)?;
        let anchor_path = recovery_anchor_path(&authority)?;
        let trusted = TrustedRecoveryRecord::open(&project, &marker, &authority)?;

        let error = recover_project_publication_with(&project, &marker, &trusted, || {
            std::fs::write(&manifest_path, "external manifest")?;
            Ok(())
        })
        .err()
        .ok_or_else(|| eyre::eyre!("recovery consumed evidence after restored-state drift"))?;

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        assert_eq!(std::fs::read_to_string(&lock_path)?, "original");
        assert_eq!(
            std::fs::read_to_string(&manifest_path)?,
            "external manifest"
        );
        assert!(marker.exists());
        assert!(anchor_path.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_retains_authority_when_marker_identity_changes_before_cleanup() -> eyre::Result<()>
    {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;
        let marker = recovery_path(&lock_path);
        let record = ProjectRecoveryRecord::new(&project, &accepted)?;
        publish_trusted_record(&project, &marker, &record)?;
        accepted.install(&project.root)?;
        let authority = recovery_authority(&project)?;
        let anchor_path = recovery_anchor_path(&authority)?;
        let trusted = TrustedRecoveryRecord::open(&project, &marker, &authority)?;
        let replacement_contents = serde_json::to_vec(&record)?;

        let error = recover_project_publication_with(&project, &marker, &trusted, || {
            let replacement = marker.with_extension("replacement");
            std::fs::write(&replacement, &replacement_contents)?;
            std::fs::rename(replacement, &marker)?;
            Ok(())
        })
        .err()
        .ok_or_else(|| eyre::eyre!("recovery consumed replaced evidence"))?;

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        assert!(marker.exists());
        assert!(anchor_path.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_retains_evidence_when_authority_identity_changes_before_cleanup() -> eyre::Result<()>
    {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;
        let marker = recovery_path(&lock_path);
        let record = ProjectRecoveryRecord::new(&project, &accepted)?;
        publish_trusted_record(&project, &marker, &record)?;
        accepted.install(&project.root)?;
        let authority = recovery_authority(&project)?;
        let anchor_path = recovery_anchor_path(&authority)?;
        let trusted = TrustedRecoveryRecord::open(&project, &marker, &authority)?;
        let replacement_contents = std::fs::read(&anchor_path)?;

        let error = recover_project_publication_with(&project, &marker, &trusted, || {
            let replacement = anchor_path.with_extension("replacement");
            std::fs::write(&replacement, &replacement_contents)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600))?;
            }
            std::fs::rename(replacement, &anchor_path)?;
            Ok(())
        })
        .err()
        .ok_or_else(|| eyre::eyre!("recovery consumed replaced authority"))?;

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        assert!(marker.exists());
        assert!(anchor_path.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn git_recovery_authority_lives_in_repository_metadata() -> eyre::Result<()> {
        let (_directory, project, _lock_path) = setup();
        std::fs::create_dir_all(project.root.join(".git"))?;
        std::fs::write(project.root.join(".git/HEAD"), "ref: refs/heads/main\n")?;
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;

        let authority = recovery_authority(&project)?;
        let error = publish_accepted_with(&project, &accepted, &authority, |_accepted, _root| {
            Err(CoreError::Filesystem(
                "injected publication failure".to_string(),
            ))
        })
        .err()
        .ok_or_else(|| eyre::eyre!("injected publication unexpectedly succeeded"))?;

        std::assert_matches!(error, CoreError::PendingRecovery(_));
        let anchor = recovery_anchor_path(&authority)?;
        assert!(anchor.starts_with(project.root.join(".git/cooldown/locks")));
        assert!(anchor.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn authority_discovery_finds_anchor_only_projects() -> eyre::Result<()> {
        let (_directory, project, _lock_path) = setup();
        let authority = recovery_authority(&project)?;
        let anchor_path = recovery_anchor_path(&authority)?;
        let anchor = RecoveryAnchor::new(&project, b"record")?;
        publish_exclusive_json(&anchor_path, &anchor)
            .map_err(|error| eyre::eyre!("publish anchor: {error:?}"))?;

        let scope = crate::RecoveryScope::explicit(&project.root)?;
        let discovery = recovery_authority_projects(&scope)?;
        assert_eq!(discovery.projects, [project.root]);
        assert!(discovery.warnings.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn authority_discovery_warns_when_an_anchored_project_was_deleted() -> eyre::Result<()> {
        let (_directory, repository, _lock_path) = setup();
        let deleted_root = repository.root.join("deleted-project");
        std::fs::create_dir_all(&deleted_root)?;
        std::fs::write(deleted_root.join("Cargo.lock"), "original")?;
        let deleted = project(&deleted_root);
        let authority = recovery_authority(&deleted)?;
        let anchor_path = recovery_anchor_path(&authority)?;
        let anchor = RecoveryAnchor::new(&deleted, b"record")?;
        publish_exclusive_json(&anchor_path, &anchor)
            .map_err(|error| eyre::eyre!("publish anchor: {error:?}"))?;
        std::fs::remove_dir_all(&deleted_root)?;

        let scope = crate::RecoveryScope::repository(&repository.root)?;
        let discovery = recovery_authority_projects(&scope)?;

        assert!(discovery.projects.is_empty());
        assert_eq!(discovery.warnings.len(), 1);
        assert_eq!(discovery.warnings[0].path, anchor_path);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn authority_discovery_and_recovery_find_private_anchor_publications() -> eyre::Result<()> {
        let (_directory, project, _lock_path) = setup();
        let authority = recovery_authority(&project)?;
        let anchor_path = recovery_anchor_path(&authority)?;
        let anchor = RecoveryAnchor::new(&project, b"record")?;
        let contents = serde_json::to_vec(&anchor)?;
        let private = create_synced_private_file(&anchor_path, &contents)?;

        let scope = crate::RecoveryScope::explicit(&project.root)?;
        let discovery = recovery_authority_projects(&scope)?;
        assert_eq!(
            discovery.projects.as_slice(),
            std::slice::from_ref(&project.root)
        );
        assert!(discovery.warnings.is_empty());
        std::assert_matches!(
            test_recover(&project)?.disposition,
            RecoveryDisposition::CleanupOnly
        );
        assert!(!private.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn explicit_authority_discovery_sorts_warnings_for_malformed_linked_worktrees()
    -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let base = canonical_root(&directory)?;
        let common = base.join("common.git");
        let first_root = base.join("first");
        let second_root = base.join("second");
        let third_root = base.join("third");
        std::fs::create_dir_all(&common)?;
        std::fs::write(common.join("HEAD"), "ref: refs/heads/main\n")?;
        for (root, name) in [
            (&first_root, "first"),
            (&second_root, "second"),
            (&third_root, "third"),
        ] {
            let git_dir = common.join(format!("worktrees/{name}"));
            std::fs::create_dir_all(&git_dir)?;
            std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")?;
            std::fs::write(git_dir.join("commondir"), "../..\n")?;
            std::fs::create_dir_all(root)?;
            std::fs::write(
                root.join(".git"),
                format!("gitdir: ../common.git/worktrees/{name}\n"),
            )?;
            std::fs::write(root.join("Cargo.lock"), "original")?;
        }
        let first = project(&first_root);
        let second = project(&second_root);
        let third = project(&third_root);
        let first_authority = recovery_authority(&first)?;
        let second_authority = recovery_authority(&second)?;
        let third_authority = recovery_authority(&third)?;
        assert_eq!(first_authority.directory(), second_authority.directory());
        assert_eq!(first_authority.directory(), third_authority.directory());
        let first_anchor_path = recovery_anchor_path(&first_authority)?;
        let first_anchor = RecoveryAnchor::new(&first, b"record")?;
        publish_exclusive_json(&first_anchor_path, &first_anchor)
            .map_err(|error| eyre::eyre!("publish first anchor: {error:?}"))?;
        let second_anchor_path = recovery_anchor_path(&second_authority)?;
        let third_anchor_path = recovery_anchor_path(&third_authority)?;
        for path in [&third_anchor_path, &second_anchor_path] {
            std::fs::write(path, "not valid recovery authority")?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }

        let scope = crate::RecoveryScope::explicit(&first.root)?;
        let discovery = recovery_authority_projects(&scope)?;
        assert_eq!(discovery.projects, [first.root]);
        let mut expected = vec![second_anchor_path, third_anchor_path];
        expected.sort();
        let warnings = discovery
            .warnings
            .into_iter()
            .map(|warning| warning.path)
            .collect::<Vec<_>>();
        assert_eq!(warnings, expected);
        Ok(())
    }

    #[test]
    fn public_recovery_entry_acquires_the_project_lease() -> eyre::Result<()> {
        let (_directory, project, _lock_path) = setup();
        let _lease = cooldown_core::fs::ProjectWriteLease::acquire(
            &project.root,
            &cooldown_core::fs::ManifestFamily::of(&project.manifest),
        )?;

        let error = crate::recover_interrupted_mutation(&project.root)
            .err()
            .ok_or_else(|| eyre::eyre!("recovery ignored the held project lease"))?;

        std::assert_matches!(error, CoreError::LockConflict(_));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn public_recovery_canonicalizes_relative_components_and_symlink_spelling() -> eyre::Result<()>
    {
        use std::os::unix::fs::symlink;

        let (directory, project, _lock_path) = setup();
        let relative_spelling = project.root.join(".");
        let symlink_spelling = project.root.join("linked-project");
        symlink(&project.root, &symlink_spelling)?;

        let relative = crate::recover_interrupted_mutation(&relative_spelling)?;
        let linked = crate::recover_interrupted_mutation(&symlink_spelling)?;

        std::assert_matches!(relative.disposition, RecoveryDisposition::Unchanged);
        std::assert_matches!(linked.disposition, RecoveryDisposition::Unchanged);
        drop(directory);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn failed_publication_keeps_one_receipt_that_recovers_the_preimage() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let manifest_path = project.root.join("Cargo.toml");
        std::fs::write(&manifest_path, "original manifest")?;
        let accepted = accepted_project_state(
            &project,
            &[
                ("Cargo.lock", "accepted lock"),
                ("Cargo.toml", "accepted manifest"),
            ],
        )?;

        let authority = recovery_authority(&project)?;
        let error = publish_accepted_with(&project, &accepted, &authority, |_accepted, root| {
            std::fs::write(root.join("Cargo.lock"), "accepted lock")?;
            Err(CoreError::Filesystem(
                "injected publication failure".to_string(),
            ))
        })
        .err()
        .ok_or_else(|| eyre::eyre!("injected publication unexpectedly succeeded"))?;

        std::assert_matches!(error, CoreError::PendingRecovery(_));
        assert!(recovery_path(&lock_path).exists());
        std::assert_matches!(
            test_recover(&project)?.disposition,
            RecoveryDisposition::Restored
        );
        assert_eq!(std::fs::read_to_string(&lock_path)?, "original");
        assert_eq!(
            std::fs::read_to_string(&manifest_path)?,
            "original manifest"
        );
        assert!(!recovery_path(&lock_path).exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn marker_cleanup_failure_preserves_the_published_result_and_recovery_owner() -> eyre::Result<()>
    {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;

        let publication = publish_accepted_with_marker(
            &project,
            &accepted,
            &recovery_authority(&project)?,
            AcceptedProjectState::install,
            |_path| {
                Err(CoreError::Filesystem(
                    "injected marker cleanup failure".to_string(),
                ))
            },
        )?;

        std::assert_matches!(
            publication,
            AcceptedPublication::PublishedPendingRecovery {
                error: CoreError::PendingRecovery(_),
                ..
            }
        );
        assert_eq!(std::fs::read_to_string(&lock_path)?, "accepted");
        assert!(recovery_path(&lock_path).exists());
        std::assert_matches!(
            test_recover(&project)?.disposition,
            RecoveryDisposition::Accepted
        );
        assert_eq!(std::fs::read_to_string(&lock_path)?, "accepted");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_accepts_a_fully_published_project() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;
        let marker = recovery_path(&lock_path);
        let record = ProjectRecoveryRecord::new(&project, &accepted)?;
        publish_trusted_record(&project, &marker, &record)?;
        accepted.install(&project.root)?;

        std::assert_matches!(
            test_recover(&project)?.disposition,
            RecoveryDisposition::Accepted
        );
        assert_eq!(std::fs::read_to_string(&lock_path)?, "accepted");
        assert!(!marker.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_refuses_drift_in_an_unchanged_tracked_manifest() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let manifest_path = project.root.join("Cargo.toml");
        std::fs::write(&manifest_path, "original manifest")?;
        let accepted = accepted_project_state(
            &project,
            &[
                ("Cargo.lock", "accepted"),
                ("Cargo.toml", "original manifest"),
            ],
        )?;
        let marker = recovery_path(&lock_path);
        let record = ProjectRecoveryRecord::new(&project, &accepted)?;
        publish_trusted_record(&project, &marker, &record)?;
        accepted.install(&project.root)?;
        std::fs::write(&manifest_path, "external manifest")?;

        std::assert_matches!(test_recover(&project), Err(CoreError::LockUnreadable(_)));
        assert_eq!(std::fs::read_to_string(&lock_path)?, "accepted");
        assert_eq!(
            std::fs::read_to_string(&manifest_path)?,
            "external manifest"
        );
        assert!(marker.exists());
        Ok(())
    }

    #[test]
    fn whole_project_record_rejects_inconsistent_permissions() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;
        let mut record = ProjectRecoveryRecord::new(&project, &accepted)?;
        let file = record
            .files
            .first_mut()
            .ok_or_else(|| eyre::eyre!("record omitted its changed lock"))?;
        file.original_permissions.present = false;

        std::assert_matches!(
            record.validate(&project, &recovery_path(&lock_path)),
            Err(CoreError::LockUnreadable(_))
        );
        Ok(())
    }

    #[test]
    fn exclusive_publication_never_clobbers_an_existing_marker() {
        let (_dir, _project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        cooldown_core::fs::atomic_write(marker.as_std_path(), b"user data")
            .expect("write existing marker");
        let state = serde_json::json!({ "state": "candidate" });

        let error = publish_exclusive_json(&marker, &state).expect_err("reject existing marker");

        std::assert_matches!(
            error,
            PublicationError::NotPublished(CoreError::LockConflict(_))
        );
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read marker"),
            "user data"
        );
    }

    #[test]
    fn publication_reports_uncertain_durability_without_removing_evidence() -> eyre::Result<()> {
        let (_dir, _project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        let state = serde_json::json!({ "state": "candidate" });

        // Publication has crossed its visible commit point before the injected directory failure.
        let result = publish_exclusive_json_with(
            &marker,
            &state,
            |_path| {
                Err(CoreError::Filesystem(
                    "injected directory sync failure".to_string(),
                ))
            },
            remove_file,
        );

        // Both names remain so restart recovery can inspect the uncertain publication safely.
        std::assert_matches!(result, Err(PublicationError::DurabilityUncertain(_)));
        assert!(marker.exists());
        assert!(!publication_temps(&lock_path)?.is_empty());
        Ok(())
    }

    #[test]
    fn publication_cleanup_failure_does_not_hide_a_committed_publication() -> eyre::Result<()> {
        let (_dir, _project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        let state = serde_json::json!({ "state": "candidate" });

        // Private-name cleanup occurs after the public marker is durably committed.
        let result = publish_exclusive_json_with(
            &marker,
            &state,
            |_path| Ok(()),
            |_path| {
                Err(CoreError::Filesystem(
                    "injected cleanup failure".to_string(),
                ))
            },
        );

        // Cleanup failure cannot turn an already committed publication into an apparent failure.
        let outcome = result.map_err(|error| eyre::eyre!("publication failed: {error:?}"))?;
        std::assert_matches!(outcome, PublicationOutcome::CleanupPending { .. });
        assert!(marker.exists());
        assert!(!publication_temps(&lock_path)?.is_empty());
        Ok(())
    }

    #[test]
    fn recovery_removes_only_private_names_linked_to_the_public_marker() -> eyre::Result<()> {
        let (_dir, _project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        let state = serde_json::json!({ "state": "candidate" });
        let outcome = publish_exclusive_json_with(
            &marker,
            &state,
            |_path| Ok(()),
            |_path| Err(CoreError::Filesystem("retain private name".to_string())),
        )
        .map_err(|error| eyre::eyre!("publish marker: {error:?}"))?;
        let PublicationOutcome::CleanupPending { path, .. } = outcome else {
            return Err(eyre::eyre!("private name was not retained"));
        };
        let unrelated = path.with_file_name(format!(
            ".{}.999999.0.publish",
            marker
                .file_name()
                .ok_or_else(|| eyre::eyre!("marker has no file name"))?
        ));
        std::fs::write(&unrelated, "unrelated")?;

        cleanup_linked_publication_files(&marker)?;

        assert!(!path.exists());
        assert!(unrelated.exists());
        assert!(marker.exists());
        Ok(())
    }

    #[test]
    fn private_cleanup_retry_reports_the_owned_artifact() {
        let path = Utf8PathBuf::from("Cargo.lock.private.publish");
        let mut warnings = Vec::new();
        retry_private_cleanup(
            PublicationOutcome::CleanupPending {
                path: path.clone(),
                error: CoreError::Filesystem("initial cleanup failed".to_string()),
            },
            &mut warnings,
            |_path| Err(CoreError::Filesystem("retry failed".to_string())),
        );

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains(path.as_str()));
        assert!(warnings[0].message.contains("retry failed"));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_artifacts_are_owner_only() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        let state = serde_json::json!({ "state": "candidate" });
        let outcome = publish_exclusive_json_with(
            &marker,
            &state,
            |_path| Ok(()),
            |_path| Err(CoreError::Filesystem("retain private name".to_string())),
        )
        .map_err(|error| eyre::eyre!("publish marker: {error:?}"))?;
        let PublicationOutcome::CleanupPending { path, .. } = outcome else {
            return Err(eyre::eyre!("publication did not retain its private name"));
        };
        let anchor_path = recovery_anchor_path(&recovery_authority(&project)?)?;
        let anchor = RecoveryAnchor::new(&project, b"record")?;
        publish_exclusive_json(&anchor_path, &anchor)
            .map_err(|error| eyre::eyre!("publish anchor: {error:?}"))?;

        for path in [&marker, &path, &anchor_path] {
            assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }

    #[test]
    fn orphan_publications_are_reported_without_deletion() -> eyre::Result<()> {
        let (_dir, project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        let record = serde_json::json!({ "state": "candidate" });
        publish_exclusive_json_with(
            &marker,
            &record,
            |_path| Ok(()),
            |_path| {
                Err(CoreError::Filesystem(
                    "injected cleanup failure".to_string(),
                ))
            },
        )
        .map_err(|error| eyre::eyre!("publish recovery marker: {error:?}"))?;
        std::fs::remove_file(&marker)?;
        assert_eq!(publication_temps(&lock_path)?.len(), 1);

        let error = ensure_no_orphan_artifacts(&lock_path)
            .err()
            .ok_or_else(|| eyre::eyre!("orphan publication was not reported"))?;

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        std::assert_matches!(
            ensure_no_pending(&project),
            Err(CoreError::LockUnreadable(_))
        );
        assert_eq!(publication_temps(&lock_path)?.len(), 1);
        Ok(())
    }

    #[test]
    fn unrelated_publication_file_does_not_block_cargo_operations() -> eyre::Result<()> {
        let (_dir, project, lock_path) = setup();
        let unrelated = project.root.join(".myapp.123.0.publish");
        std::fs::write(&unrelated, "unrelated")?;

        ensure_no_pending(&project)?;

        assert!(unrelated.exists());
        assert_eq!(std::fs::read_to_string(&lock_path)?, "original");
        Ok(())
    }

    #[test]
    fn recovery_artifact_names_cover_public_and_private_markers() {
        for name in [
            RECOVERY_MARKER,
            ".Cargo.lock.cooldown-recovery.123.0.publish",
        ] {
            assert!(is_recovery_artifact_name(name), "missed {name}");
        }
        for name in [
            "Cargo.lock",
            "Cargo.lock.cooldown-recovery-123-456.state",
            ".myapp.123.0.publish",
            ".Cargo.lock.cooldown-recovery.bad.0.publish",
        ] {
            assert!(!is_recovery_artifact_name(name), "accepted {name}");
        }
    }

    #[test]
    fn removal_reports_uncertain_durability_after_unlink() -> eyre::Result<()> {
        let (_dir, _project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        std::fs::write(&marker, b"recovery evidence")?;

        // The unlink is visible before the injected parent-directory sync failure.
        let result = remove_file_with(&marker, |_path| {
            Err(CoreError::Filesystem(
                "injected directory sync failure".to_string(),
            ))
        });

        // The typed outcome prevents callers from mistaking uncertain durability for no mutation.
        std::assert_matches!(result, Err(RemovalError::DurabilityUncertain(_)));
        assert!(!marker.exists());
        Ok(())
    }

    #[test]
    fn marker_sync_failure_becomes_a_committed_warning() -> eyre::Result<()> {
        let (_dir, _project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        std::fs::write(&marker, b"recovery evidence")?;

        let outcome = remove_transaction_marker_with(&marker, |_path| {
            Err(CoreError::Filesystem(
                "injected directory sync failure".to_string(),
            ))
        })?;
        let CommitOutcome::DurabilityUncertain(error) = outcome else {
            return Err(eyre::eyre!(
                "post-unlink sync failure was not reported as committed"
            ));
        };

        assert!(error.to_string().contains("syncing its directory failed"));
        assert!(!marker.exists());
        Ok(())
    }

    fn publication_temps(lock_path: &Utf8Path) -> eyre::Result<Vec<std::fs::DirEntry>> {
        let parent = lock_path
            .parent()
            .ok_or_else(|| eyre::eyre!("lock path has no parent: {lock_path}"))?;
        let entries = std::fs::read_dir(parent)?.collect::<std::io::Result<Vec<_>>>()?;
        Ok(entries
            .into_iter()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".publish"))
            .collect())
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_marker_leaves_user_data_untouched() {
        let (_dir, project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        cooldown_core::fs::atomic_write(marker.as_std_path(), b"user data").expect("write marker");

        let error = test_recover(&project).expect_err("reject marker");

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "original"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read marker"),
            "user data"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_side_pending_check_never_recovers_the_lock() -> eyre::Result<()> {
        let (_dir, project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "candidate")])?;
        let record = ProjectRecoveryRecord::new(&project, &accepted)?;
        publish_trusted_record(&project, &marker, &record)?;
        accepted.install(&project.root)?;

        let error = ensure_no_pending(&project)
            .err()
            .ok_or_else(|| eyre::eyre!("pending transaction was ignored"))?;

        std::assert_matches!(error, CoreError::StaleLock(_));
        assert_eq!(std::fs::read_to_string(&lock_path)?, "candidate");
        assert!(marker.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_marker_symlinks_fail_closed() -> eyre::Result<()> {
        use std::os::unix::fs::symlink;

        let (_dir, project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        symlink("missing-recovery-record", &marker)?;

        std::assert_matches!(
            ensure_no_pending(&project),
            Err(CoreError::LockUnreadable(_))
        );
        std::assert_matches!(test_recover(&project), Err(CoreError::LockUnreadable(_)));
        assert_eq!(std::fs::read_to_string(lock_path)?, "original");
        assert!(std::fs::symlink_metadata(marker)?.file_type().is_symlink());
        Ok(())
    }
}
