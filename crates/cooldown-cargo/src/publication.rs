//! Publishes accepted Cargo project states and recovers interrupted source transactions.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    AcceptedProjectState, AcceptedPublication, CoreError, Diagnostic, DiagnosticKind,
    MutationRecovery, Project, ProjectMutationFile, ProjectMutationJournal, RecoveryDisposition,
    Result, fs::RecoveryAuthority,
};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};

const RECOVERY_FORMAT: &str = "cooldown-cargo-lock-recovery-v2";
const RECOVERY_STATE_FORMAT: &str = "cooldown-cargo-lock-recovery-state-v1";
const PROJECT_RECOVERY_FORMAT: &str = "cooldown-cargo-project-recovery-v1";
const RECOVERY_ANCHOR_FORMAT: &str = "cooldown-cargo-recovery-anchor-v1";
const MAX_RECOVERY_ANCHOR_BYTES: u64 = 16 * 1024;
const MAX_RECOVERY_RECORD_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const RECOVERY_MARKER: &str = "Cargo.lock.cooldown-recovery";

// Retained so newer versions can recover transactions written before isolated staging.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct RecoveryRecord {
    format: String,
    project_root: String,
    original_digest: String,
    state_file: String,
    original_lock: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct RecoveryState {
    format: String,
    previous_digest: String,
    candidate_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct ProjectRecoveryRecord {
    format: String,
    project_root: String,
    files: Vec<ProjectRecoveryFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct RecoveryAnchor {
    format: String,
    project_root: String,
    record_digest: String,
}

struct TrustedRecoveryRecord {
    record: TrustedArtifact,
    anchor: TrustedArtifact,
    authority: RecoveryAuthority,
}

struct TrustedArtifact {
    path: Utf8PathBuf,
    contents: Vec<u8>,
    identity: same_file::Handle,
    owner_private: bool,
}

#[derive(Clone, Copy)]
enum ExpectedProjectState {
    Original,
    Candidate,
}

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
            "recoverable Cargo publication is unavailable for non-Git project {}; move the project into a Git worktree before running a mutation",
            project.root
        ))
    })
}

pub(crate) fn recovery_authority_projects(root: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let coordination = cooldown_core::fs::ProjectCoordination::resolve(root)?;
    let Some(scan_authority) = coordination.recovery_authority() else {
        return Ok(Vec::new());
    };
    let entries = std::fs::read_dir(scan_authority.directory())?;
    let mut projects = Vec::new();
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
        let artifact = TrustedArtifact::open(&path, MAX_RECOVERY_ANCHOR_BYTES, true)?;
        let anchor: RecoveryAnchor = artifact.decode("recovery authority")?;
        if anchor.format != RECOVERY_ANCHOR_FORMAT || !is_sha256_digest(&anchor.record_digest) {
            return Err(untrusted_record(&path));
        }
        let project_root = Utf8PathBuf::from(anchor.project_root.clone());
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
    Ok(projects)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct ProjectRecoveryFile {
    path: String,
    original: Option<String>,
    candidate: Option<String>,
    original_permissions: RecoveryPermissions,
    candidate_permissions: RecoveryPermissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct RecoveryPermissions {
    present: bool,
    readonly: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    unix_mode: Option<u32>,
}

#[derive(Debug)]
enum PublicationError {
    NotPublished(CoreError),
    DurabilityUncertain(CoreError),
}

#[derive(Debug)]
enum PublicationOutcome {
    Published,
    CleanupPending { path: Utf8PathBuf, error: CoreError },
}

#[derive(Debug)]
enum RemovalError {
    NotRemoved(CoreError),
    DurabilityUncertain(CoreError),
}

/// The visible commit state after consuming a recovery marker.
#[derive(Debug)]
pub(super) enum CommitOutcome {
    Committed,
    DurabilityUncertain(CoreError),
}

impl PublicationError {
    fn into_core_error(self, path: &Utf8Path) -> CoreError {
        match self {
            PublicationError::NotPublished(error) => error,
            PublicationError::DurabilityUncertain(error) => CoreError::LockConflict(format!(
                "published recovery state at {path}, but syncing its directory failed; recovery evidence was left intact: {error}"
            )),
        }
    }
}

impl RemovalError {
    fn into_core_error(self, path: &Utf8Path) -> CoreError {
        match self {
            RemovalError::NotRemoved(error) => error,
            RemovalError::DurabilityUncertain(error) => CoreError::LockConflict(format!(
                "removed recovery state at {path}, but syncing its directory failed; remaining recovery evidence was left intact: {error}"
            )),
        }
    }
}

impl RecoveryState {
    #[cfg(test)]
    fn new(previous_lock: &str, candidate_lock: &str) -> Result<Self> {
        if previous_lock == candidate_lock {
            return Err(CoreError::System(
                "a speculative Cargo.lock candidate must differ from its previous state"
                    .to_string(),
            ));
        }
        Ok(RecoveryState {
            format: RECOVERY_STATE_FORMAT.to_string(),
            previous_digest: lock_digest(previous_lock),
            candidate_digest: lock_digest(candidate_lock),
        })
    }

    fn validate(&self, path: &Utf8Path) -> Result<()> {
        if self.format == RECOVERY_STATE_FORMAT
            && is_sha256_digest(&self.previous_digest)
            && is_sha256_digest(&self.candidate_digest)
            && self.previous_digest != self.candidate_digest
        {
            Ok(())
        } else {
            Err(untrusted_record(path))
        }
    }
}

impl RecoveryRecord {
    fn validate(&self, project: &Project, lock_path: &Utf8Path, path: &Utf8Path) -> Result<()> {
        let state_path = validated_state_path(lock_path, &self.state_file)?;
        let valid = self.format == RECOVERY_FORMAT
            && self.project_root == canonical_project_root(project)?
            && self.original_digest == lock_digest(&self.original_lock)
            && state_path.parent() == lock_path.parent();
        if valid {
            Ok(())
        } else {
            Err(untrusted_record(path))
        }
    }
}

impl ProjectRecoveryRecord {
    fn new(project: &Project, accepted: &AcceptedProjectState) -> Result<Self> {
        let files = accepted
            .files()
            .map(|(original, candidate)| ProjectRecoveryFile::new(original, candidate))
            .collect::<Result<Vec<_>>>()?;
        if files.is_empty() {
            return Err(CoreError::System(
                "cannot publish an unchanged Cargo project state".to_string(),
            ));
        }
        Ok(ProjectRecoveryRecord {
            format: PROJECT_RECOVERY_FORMAT.to_string(),
            project_root: canonical_project_root(project)?,
            files,
        })
    }

    fn validate(&self, project: &Project, path: &Utf8Path) -> Result<()> {
        if self.format != PROJECT_RECOVERY_FORMAT
            || self.project_root != canonical_project_root(project)?
            || self.files.is_empty()
            || !self.files.iter().any(ProjectRecoveryFile::is_changed)
        {
            return Err(untrusted_record(path));
        }
        let mut paths = std::collections::BTreeSet::new();
        for file in &self.files {
            file.validate(path)?;
            if !paths.insert(file.path.as_str()) {
                return Err(untrusted_record(path));
            }
        }
        Ok(())
    }

    fn original_journal(&self, root: &Utf8Path) -> Result<ProjectMutationJournal> {
        let files = self
            .files
            .iter()
            .map(|file| file.original_file(root))
            .collect::<Result<Vec<_>>>()?;
        ProjectMutationJournal::from_snapshot(root, files)
    }

    fn classify_state(
        &self,
        project: &Project,
        live: &cooldown_core::ProjectMutationState,
        recovery_path: &Utf8Path,
    ) -> Result<(bool, bool)> {
        let mut saw_original = false;
        let mut saw_candidate = false;
        for (recorded, live) in self.files.iter().zip(live.files()) {
            if !recorded.is_changed() {
                if !recorded.matches_original(live) {
                    return Err(CoreError::LockUnreadable(format!(
                        "{} changed after the accepted project trial at {recovery_path}; left all files untouched",
                        project.root.join(live.path())
                    )));
                }
                continue;
            }
            if recorded.matches_original(live) {
                saw_original = true;
            } else if recorded.matches_candidate(live) {
                saw_candidate = true;
            } else {
                return Err(CoreError::LockUnreadable(format!(
                    "{} no longer matches either side of the interrupted project publication at {recovery_path}; left all files untouched",
                    project.root.join(live.path())
                )));
            }
        }
        Ok((saw_original, saw_candidate))
    }

    fn validate_expected_state(
        &self,
        project: &Project,
        live: &cooldown_core::ProjectMutationState,
        expected: ExpectedProjectState,
        recovery_path: &Utf8Path,
    ) -> Result<()> {
        for (recorded, live) in self.files.iter().zip(live.files()) {
            let matches = match expected {
                ExpectedProjectState::Original => recorded.matches_original(live),
                ExpectedProjectState::Candidate => {
                    if recorded.is_changed() {
                        recorded.matches_candidate(live)
                    } else {
                        recorded.matches_original(live)
                    }
                }
            };
            if !matches {
                return Err(CoreError::LockUnreadable(format!(
                    "{} changed before recovery evidence could be consumed at {recovery_path}; retained the recovery record",
                    project.root.join(live.path())
                )));
            }
        }
        Ok(())
    }
}

impl RecoveryAnchor {
    fn new(project: &Project, record: &[u8]) -> Result<Self> {
        Ok(RecoveryAnchor {
            format: RECOVERY_ANCHOR_FORMAT.to_string(),
            project_root: canonical_project_root(project)?,
            record_digest: bytes_digest(record),
        })
    }

    fn validate(&self, project: &Project, record: &[u8], path: &Utf8Path) -> Result<()> {
        let valid = self.format == RECOVERY_ANCHOR_FORMAT
            && self.project_root == canonical_project_root(project)?
            && self.record_digest == bytes_digest(record)
            && is_sha256_digest(&self.record_digest);
        if valid {
            Ok(())
        } else {
            Err(untrusted_record(path))
        }
    }

    fn validate_without_record(&self, project: &Project, path: &Utf8Path) -> Result<()> {
        let valid = self.format == RECOVERY_ANCHOR_FORMAT
            && self.project_root == canonical_project_root(project)?
            && is_sha256_digest(&self.record_digest);
        if valid {
            Ok(())
        } else {
            Err(untrusted_record(path))
        }
    }
}

impl TrustedRecoveryRecord {
    fn open(
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

    fn format(&self, record_path: &Utf8Path) -> Result<String> {
        let value: serde_json::Value = self.decode(record_path)?;
        value
            .get("format")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| untrusted_record(record_path))
    }

    fn decode<T: serde::de::DeserializeOwned>(&self, record_path: &Utf8Path) -> Result<T> {
        serde_json::from_slice(&self.record.contents).map_err(|error| {
            CoreError::LockUnreadable(format!(
                "invalid Cargo.lock recovery record at {record_path}: {error}; left all files untouched"
            ))
        })
    }

    fn validate_evidence(&self) -> Result<()> {
        self.authority.validate_current()?;
        self.anchor.validate_current()?;
        self.record.validate_current()
    }
}

impl TrustedArtifact {
    fn open(path: &Utf8Path, limit: u64, owner_private: bool) -> Result<Self> {
        let (mut file, identity) = open_recovery_artifact(path)?.ok_or_else(|| {
            CoreError::LockUnreadable(format!(
                "Cargo.lock recovery artifact disappeared at {path}; left all files untouched"
            ))
        })?;
        if owner_private {
            cooldown_core::fs::validate_owner_private_file(&file, path.as_std_path()).map_err(
                |error| {
                    CoreError::LockUnreadable(format!(
                        "untrusted Cargo.lock recovery authority at {path}: {error}; left all files untouched"
                    ))
                },
            )?;
        }
        let contents = read_bounded(&mut file, path, limit)?;
        if identity != same_file::Handle::from_path(path)? {
            return Err(untrusted_record(path));
        }
        Ok(TrustedArtifact {
            path: path.to_owned(),
            contents,
            identity,
            owner_private,
        })
    }

    fn decode<T: serde::de::DeserializeOwned>(&self, label: &str) -> Result<T> {
        serde_json::from_slice(&self.contents).map_err(|error| {
            CoreError::LockUnreadable(format!(
                "invalid Cargo.lock {label} at {}: {error}; left all files untouched",
                self.path
            ))
        })
    }

    fn validate_current(&self) -> Result<()> {
        let limit = u64::try_from(self.contents.len()).map_err(|error| {
            CoreError::System(format!(
                "recovery evidence size cannot be represented: {error}"
            ))
        })?;
        let current = Self::open(&self.path, limit, self.owner_private)?;
        if self.identity != current.identity || self.contents != current.contents {
            return Err(CoreError::LockUnreadable(format!(
                "Cargo.lock recovery evidence changed at {}; left it untouched",
                self.path
            )));
        }
        Ok(())
    }
}

impl ProjectRecoveryFile {
    fn new(original: &ProjectMutationFile, candidate: &ProjectMutationFile) -> Result<Self> {
        let original_contents = utf8_contents(original)?;
        let candidate_contents = utf8_contents(candidate)?;
        Ok(ProjectRecoveryFile {
            path: original.path().to_string(),
            original: original_contents,
            candidate: candidate_contents,
            original_permissions: RecoveryPermissions::new(original.permissions()),
            candidate_permissions: RecoveryPermissions::new(candidate.permissions()),
        })
    }

    fn validate(&self, record_path: &Utf8Path) -> Result<()> {
        let relative = Utf8Path::new(&self.path);
        let safe_path = !relative.as_str().is_empty()
            && !relative.is_absolute()
            && relative
                .components()
                .all(|component| matches!(component, camino::Utf8Component::Normal(_)));
        let cargo_output = matches!(relative.file_name(), Some("Cargo.toml" | "Cargo.lock"));
        let original_valid = self.original.is_some() == self.original_permissions.is_present();
        let candidate_valid = self.candidate.is_some() == self.candidate_permissions.is_present();
        if !safe_path
            || !cargo_output
            || !original_valid
            || !candidate_valid
            || !self.original_permissions.is_valid()
            || !self.candidate_permissions.is_valid()
        {
            return Err(untrusted_record(record_path));
        }
        Ok(())
    }

    fn original_file(&self, root: &Utf8Path) -> Result<ProjectMutationFile> {
        ProjectMutationFile::from_snapshot(
            Utf8PathBuf::from(&self.path),
            self.original
                .as_ref()
                .map(|value| value.as_bytes().to_vec()),
            self.original_permissions.to_permissions(root)?,
        )
    }

    fn matches_original(&self, live: &ProjectMutationFile) -> bool {
        self.matches(live, self.original.as_deref(), self.original_permissions)
    }

    fn is_changed(&self) -> bool {
        self.original != self.candidate || self.original_permissions != self.candidate_permissions
    }

    fn matches_candidate(&self, live: &ProjectMutationFile) -> bool {
        self.matches(live, self.candidate.as_deref(), self.candidate_permissions)
    }

    fn matches(
        &self,
        live: &ProjectMutationFile,
        contents: Option<&str>,
        permissions: RecoveryPermissions,
    ) -> bool {
        live.path() == Utf8Path::new(&self.path)
            && live.contents() == contents.map(str::as_bytes)
            && permissions.matches(live.permissions())
    }
}

impl RecoveryPermissions {
    fn new(permissions: Option<&std::fs::Permissions>) -> Self {
        #[cfg(unix)]
        let unix_mode = permissions.map(|permissions| {
            use std::os::unix::fs::PermissionsExt as _;
            permissions.mode()
        });
        #[cfg(not(unix))]
        let unix_mode = None;
        RecoveryPermissions {
            present: permissions.is_some(),
            readonly: permissions.is_some_and(std::fs::Permissions::readonly),
            unix_mode,
        }
    }

    const fn is_present(self) -> bool {
        self.present
    }

    const fn is_valid(self) -> bool {
        if !self.present {
            return !self.readonly && self.unix_mode.is_none();
        }
        #[cfg(unix)]
        {
            self.unix_mode.is_some()
        }
        #[cfg(not(unix))]
        {
            self.unix_mode.is_none()
        }
    }

    fn matches(self, permissions: Option<&std::fs::Permissions>) -> bool {
        Self::new(permissions) == self
    }

    #[cfg_attr(
        unix,
        expect(
            clippy::unnecessary_wraps,
            reason = "the shared cross-platform signature remains fallible on non-Unix targets"
        )
    )]
    fn to_permissions(self, root: &Utf8Path) -> Result<Option<std::fs::Permissions>> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = root;
            Ok(self.unix_mode.map(std::fs::Permissions::from_mode))
        }
        #[cfg(not(unix))]
        {
            if !self.present {
                return Ok(None);
            }
            let mut permissions = std::fs::metadata(root)?.permissions();
            permissions.set_readonly(self.readonly);
            Ok(Some(permissions))
        }
    }
}

fn utf8_contents(file: &ProjectMutationFile) -> Result<Option<String>> {
    file.contents()
        .map(|contents| {
            String::from_utf8(contents.to_vec()).map_err(|error| {
                CoreError::Serialization(format!(
                    "Cargo mutation output {} is not UTF-8: {error}",
                    file.path()
                ))
            })
        })
        .transpose()
}

/// Publishes one accepted Cargo project state under coordination-anchored recovery authority.
pub(crate) fn publish_accepted(
    project: &Project,
    accepted: &AcceptedProjectState,
    authority: &RecoveryAuthority,
) -> Result<AcceptedPublication> {
    publish_accepted_with(project, accepted, authority, AcceptedProjectState::install)
}

fn publish_accepted_with<F>(
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

fn publish_accepted_with_marker<F, R>(
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

fn remove_recovery_anchor(path: &Utf8Path) -> Result<CommitOutcome> {
    cleanup_linked_publication_files(path)?;
    remove_transaction_marker(path)
}

fn retry_private_cleanup<F>(
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

/// Settles a validated interrupted transaction while the caller holds exclusive project access.
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
    let format = trusted.format(&recovery_path)?;
    if format == PROJECT_RECOVERY_FORMAT {
        return recover_project_publication(project, &recovery_path, &trusted);
    }
    if format != RECOVERY_FORMAT {
        return Err(untrusted_record(&recovery_path));
    }
    let record: RecoveryRecord = trusted.decode(&recovery_path)?;
    record.validate(project, &lock_path, &recovery_path)?;
    let state_path = validated_state_path(&lock_path, &record.state_file)?;
    let state: RecoveryState = read_json(&state_path)?;
    state.validate(&state_path)?;

    let current = std::fs::read_to_string(&lock_path)?;
    let current_digest = lock_digest(&current);
    if current != record.original_lock
        && current_digest != state.previous_digest
        && current_digest != state.candidate_digest
    {
        return Err(CoreError::LockUnreadable(format!(
            "Cargo.lock no longer matches the interrupted transaction recorded at {recovery_path}; left all files untouched"
        )));
    }
    let restored = current != record.original_lock;
    if restored {
        durable_write(lock_path.as_std_path(), record.original_lock.as_bytes())?;
    }
    ensure_lock_equals(
        &lock_path,
        &record.original_lock,
        "recovering the interrupted correction",
        Some(&recovery_path),
    )?;
    cleanup_linked_publication_files(&recovery_path)?;
    cleanup_linked_publication_files(&state_path)?;
    ensure_lock_equals(
        &lock_path,
        &record.original_lock,
        "consuming interrupted-correction recovery evidence",
        Some(&recovery_path),
    )?;
    let recovery = MutationRecovery::settled(if restored {
        RecoveryDisposition::Restored
    } else {
        RecoveryDisposition::CleanupOnly
    });
    let mut recovery = finish_recovery_authority(recovery, &trusted)?;
    if let Err(error) = remove_file(&state_path) {
        recovery.warnings.push(Diagnostic::new(
            DiagnosticKind::Filesystem,
            format!("recovery settled Cargo.lock, but could not remove state artifact {state_path}: {error}"),
        ));
    }
    Ok(recovery)
}

fn recover_orphan_anchor(project: &Project, anchor_path: &Utf8Path) -> Result<MutationRecovery> {
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

fn recover_private_anchor_publications(
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

fn private_publication_paths(public_path: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let parent = public_path.parent().ok_or_else(|| {
        CoreError::PathEncoding(format!("recovery path has no parent: {public_path}"))
    })?;
    let public_name = public_path.file_name().ok_or_else(|| {
        CoreError::PathEncoding(format!("recovery path has no file name: {public_path}"))
    })?;
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if publication_target(&name) != Some(public_name) {
            continue;
        }
        paths.push(Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            CoreError::PathEncoding(format!(
                "non-UTF-8 recovery publication path: {}",
                path.display()
            ))
        })?);
    }
    paths.sort();
    Ok(paths)
}

fn recover_project_publication(
    project: &Project,
    recovery_path: &Utf8Path,
    trusted: &TrustedRecoveryRecord,
) -> Result<MutationRecovery> {
    recover_project_publication_with(project, recovery_path, trusted, || Ok(()))
}

fn recover_project_publication_with<F>(
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

fn finish_recovery_authority(
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

fn recovery_path(lock_path: &Utf8Path) -> Utf8PathBuf {
    lock_path.with_file_name(RECOVERY_MARKER)
}

#[cfg(test)]
fn unique_state_path(lock_path: &Utf8Path) -> Utf8PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    lock_path.with_extension(format!(
        "lock.cooldown-recovery-{}-{nonce}.state",
        std::process::id()
    ))
}

fn validated_state_path(lock_path: &Utf8Path, state_file: &str) -> Result<Utf8PathBuf> {
    if !is_recovery_state_name(state_file) {
        return Err(CoreError::LockUnreadable(format!(
            "untrusted Cargo.lock recovery state path `{state_file}`; left all files untouched"
        )));
    }
    Ok(lock_path
        .parent()
        .unwrap_or_else(|| Utf8Path::new(""))
        .join(state_file))
}

fn canonical_project_root(project: &Project) -> Result<String> {
    let canonical = std::fs::canonicalize(&project.root)?;
    Utf8PathBuf::from_path_buf(canonical)
        .map(|path| path.to_string())
        .map_err(|path| CoreError::PathEncoding(format!("non-utf8 path: {}", path.display())))
}

fn lock_digest(text: &str) -> String {
    bytes_digest(text.as_bytes())
}

fn bytes_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn ensure_lock_equals(
    lock_path: &Utf8Path,
    expected: &str,
    transition: &str,
    recovery_path: Option<&Utf8Path>,
) -> Result<()> {
    if lock_equals(lock_path, expected)? {
        return Ok(());
    }
    let retention = recovery_path.map_or_else(
        || "left Cargo.lock untouched".to_string(),
        |path| format!("left Cargo.lock and the recovery record at {path} untouched"),
    );
    Err(CoreError::LockUnreadable(format!(
        "Cargo.lock changed while {transition}; {retention}"
    )))
}

fn lock_equals(lock_path: &Utf8Path, expected: &str) -> Result<bool> {
    Ok(std::fs::read_to_string(lock_path)? == expected)
}

fn path_exists(path: &Utf8Path) -> Result<bool> {
    Ok(open_recovery_artifact(path)?.is_some())
}

fn recovery_anchor_path(authority: &RecoveryAuthority) -> Result<Utf8PathBuf> {
    let name = format!(
        "{:016x}.cargo-recovery.anchor",
        cooldown_core::fs::fnv1a_64(authority.project().as_str())
    );
    Utf8PathBuf::from_path_buf(authority.directory().join(name)).map_err(|path| {
        CoreError::PathEncoding(format!(
            "non-UTF-8 Cargo recovery authority path: {}",
            path.display()
        ))
    })
}

fn open_recovery_artifact(path: &Utf8Path) -> Result<Option<(File, same_file::Handle)>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(untrusted_record(path));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|error| {
        CoreError::LockUnreadable(format!(
            "cannot safely open Cargo.lock recovery artifact at {path}: {error}; left all files untouched"
        ))
    })?;
    let metadata = file.metadata()?;
    let path_metadata = std::fs::symlink_metadata(path)?;
    let path_identity = same_file::Handle::from_path(path)?;
    let final_metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || !path_metadata.file_type().is_file()
        || !final_metadata.file_type().is_file()
        || same_file::Handle::from_file(file.try_clone()?)? != path_identity
    {
        return Err(untrusted_record(path));
    }
    Ok(Some((file, path_identity)))
}

fn publish_exclusive_json<T: serde::Serialize>(
    path: &Utf8Path,
    value: &T,
) -> Result<PublicationOutcome, PublicationError> {
    let contents = serde_json::to_vec(value).map_err(|error| {
        PublicationError::NotPublished(CoreError::Serialization(error.to_string()))
    })?;
    publish_exclusive_bytes(path, &contents)
}

fn publish_exclusive_bytes(
    path: &Utf8Path,
    contents: &[u8],
) -> Result<PublicationOutcome, PublicationError> {
    #[cfg(unix)]
    let sync_parent = sync_parent_directory;
    #[cfg(not(unix))]
    let sync_parent = |_path: &Utf8Path| Ok(());
    publish_exclusive_bytes_with(path, contents, sync_parent, remove_file)
}

#[cfg(test)]
fn publish_exclusive_json_with<T, S, C>(
    path: &Utf8Path,
    value: &T,
    sync_parent: S,
    cleanup_private: C,
) -> Result<PublicationOutcome, PublicationError>
where
    T: serde::Serialize,
    S: FnOnce(&Utf8Path) -> Result<()>,
    C: FnOnce(&Utf8Path) -> Result<()>,
{
    let contents = serde_json::to_vec(value).map_err(|error| {
        PublicationError::NotPublished(CoreError::Serialization(error.to_string()))
    })?;
    publish_exclusive_bytes_with(path, &contents, sync_parent, cleanup_private)
}

fn publish_exclusive_bytes_with<S, C>(
    path: &Utf8Path,
    contents: &[u8],
    sync_parent: S,
    cleanup_private: C,
) -> Result<PublicationOutcome, PublicationError>
where
    S: FnOnce(&Utf8Path) -> Result<()>,
    C: FnOnce(&Utf8Path) -> Result<()>,
{
    let temp =
        create_synced_private_file(path, contents).map_err(PublicationError::NotPublished)?;
    if let Err(error) = std::fs::hard_link(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(PublicationError::NotPublished(
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                CoreError::LockConflict(format!(
                    "pending Cargo.lock transaction state already exists at {path}"
                ))
            } else {
                error.into()
            },
        ));
    }
    sync_parent(path).map_err(PublicationError::DurabilityUncertain)?;
    if let Err(error) = cleanup_private(&temp) {
        tracing::warn!(
            path = %temp,
            error = %error,
            "could not remove private Cargo.lock recovery publication file"
        );
        return Ok(PublicationOutcome::CleanupPending { path: temp, error });
    }
    Ok(PublicationOutcome::Published)
}

fn durable_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    cooldown_core::fs::atomic_write_durable(path, bytes)
        .map_err(|error| error.into_core_error(path))
}

fn create_synced_private_file(path: &Utf8Path, contents: &[u8]) -> Result<Utf8PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Utf8Path::new(""));
    let name = path
        .file_name()
        .ok_or_else(|| CoreError::PathEncoding(format!("path has no file name: {path}")))?;
    for attempt in 0..100_u8 {
        let temp = parent.join(format!(
            ".{name}.{}.{}.publish",
            std::process::id(),
            attempt
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = match options.open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = file.write_all(contents).and_then(|()| file.sync_all()) {
            let _ = std::fs::remove_file(&temp);
            return Err(error.into());
        }
        return Ok(temp);
    }
    Err(CoreError::Filesystem(format!(
        "could not create a private recovery file for {path}"
    )))
}

fn ensure_no_orphan_artifacts(lock_path: &Utf8Path) -> Result<()> {
    let parent = lock_path.parent().unwrap_or_else(|| Utf8Path::new(""));
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if is_recovery_artifact_name(&name) {
            return Err(CoreError::LockUnreadable(format!(
                "unreferenced Cargo.lock recovery artifact at {}; left it untouched; inspect and remove it explicitly",
                parent.join(name)
            )));
        }
    }
    Ok(())
}

pub(crate) fn is_recovery_artifact_name(name: &str) -> bool {
    name == RECOVERY_MARKER
        || is_recovery_state_name(name)
        || publication_target(name)
            .is_some_and(|target| target == RECOVERY_MARKER || is_recovery_state_name(target))
}

fn is_recovery_state_name(name: &str) -> bool {
    let component = Utf8Path::new(name);
    component
        .parent()
        .is_some_and(|parent| parent.as_str().is_empty())
        && name.starts_with("Cargo.lock.cooldown-recovery-")
        && component.extension() == Some("state")
}

fn publication_target(name: &str) -> Option<&str> {
    let body = name.strip_prefix('.')?.strip_suffix(".publish")?;
    let (body, attempt) = body.rsplit_once('.')?;
    let (target, process) = body.rsplit_once('.')?;
    (!attempt.is_empty()
        && !process.is_empty()
        && attempt.bytes().all(|byte| byte.is_ascii_digit())
        && process.bytes().all(|byte| byte.is_ascii_digit()))
    .then_some(target)
}

fn cleanup_linked_publication_files(public_path: &Utf8Path) -> Result<()> {
    let Some(parent) = public_path.parent() else {
        return Err(CoreError::PathEncoding(format!(
            "recovery path has no parent: {public_path}"
        )));
    };
    let Some(public_name) = public_path.file_name() else {
        return Err(CoreError::PathEncoding(format!(
            "recovery path has no file name: {public_path}"
        )));
    };
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if publication_target(name) != Some(public_name) {
            continue;
        }
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            CoreError::PathEncoding(format!(
                "non-UTF-8 recovery artifact path: {}",
                path.display()
            ))
        })?;
        if same_file::is_same_file(public_path, &path)? {
            remove_file(&path)?;
        }
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Utf8Path) -> Result<T> {
    TrustedArtifact::open(path, MAX_RECOVERY_RECORD_BYTES, false)?.decode("recovery record")
}

fn read_bounded(file: &mut File, path: &Utf8Path, limit: u64) -> Result<Vec<u8>> {
    if file.metadata()?.len() > limit {
        return Err(CoreError::LockUnreadable(format!(
            "Cargo.lock recovery artifact at {path} exceeds the {limit}-byte safety limit; left it untouched"
        )));
    }
    let capacity = usize::try_from(file.metadata()?.len()).map_err(|error| {
        CoreError::LockUnreadable(format!(
            "Cargo.lock recovery artifact size at {path} cannot be represented: {error}; left it untouched"
        ))
    })?;
    let mut contents = Vec::with_capacity(capacity);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut contents)?;
    if u64::try_from(contents.len()).map_or(true, |size| size > limit) {
        return Err(CoreError::LockUnreadable(format!(
            "Cargo.lock recovery artifact at {path} exceeds the {limit}-byte safety limit; left it untouched"
        )));
    }
    Ok(contents)
}

fn remove_file(path: &Utf8Path) -> Result<()> {
    #[cfg(unix)]
    let sync_parent = sync_parent_directory;
    #[cfg(not(unix))]
    let sync_parent = |_path: &Utf8Path| Ok(());
    remove_file_with(path, sync_parent).map_err(|error| error.into_core_error(path))
}

fn remove_transaction_marker(path: &Utf8Path) -> Result<CommitOutcome> {
    #[cfg(unix)]
    let sync_parent = sync_parent_directory;
    #[cfg(not(unix))]
    let sync_parent = |_path: &Utf8Path| Ok(());
    remove_transaction_marker_with(path, sync_parent)
}

fn remove_transaction_marker_with<S>(path: &Utf8Path, sync_parent: S) -> Result<CommitOutcome>
where
    S: FnOnce(&Utf8Path) -> Result<()>,
{
    match remove_file_with(path, sync_parent) {
        Ok(()) => Ok(CommitOutcome::Committed),
        Err(RemovalError::NotRemoved(error)) => Err(error),
        Err(error @ RemovalError::DurabilityUncertain(_)) => Ok(
            CommitOutcome::DurabilityUncertain(error.into_core_error(path)),
        ),
    }
}

fn remove_file_with<S>(path: &Utf8Path, sync_parent: S) -> Result<(), RemovalError>
where
    S: FnOnce(&Utf8Path) -> Result<()>,
{
    match std::fs::remove_file(path) {
        Ok(()) => sync_parent(path).map_err(RemovalError::DurabilityUncertain),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RemovalError::NotRemoved(error.into())),
    }
}

#[cfg(unix)]
fn sync_parent_directory(path: &Utf8Path) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Utf8Path::new(""));
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn untrusted_record(path: &Utf8Path) -> CoreError {
    CoreError::LockUnreadable(format!(
        "untrusted Cargo.lock recovery record at {path}; left all files untouched"
    ))
}

fn pending_recovery(path: &Utf8Path, error: &CoreError) -> CoreError {
    CoreError::PendingRecovery(format!(
        "{error}; recovery evidence at {path} remains authoritative; run `cooldown recover`"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CARGO_ID;
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
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("UTF-8 temp path");
        std::fs::create_dir_all(root.join(".git")).expect("create Git directory");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write Git HEAD");
        let project = project(&root);
        let lock_path = root.join("Cargo.lock");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"original").expect("write lock");
        (dir, project, lock_path)
    }

    fn recovery_authority(project: &Project) -> Result<RecoveryAuthority> {
        let coordination = cooldown_core::fs::ProjectCoordination::resolve(&project.root)?;
        coordination
            .recovery_authority()
            .cloned()
            .ok_or_else(|| CoreError::System("test project has no recovery authority".to_string()))
    }

    fn test_publish(
        project: &Project,
        accepted: &AcceptedProjectState,
    ) -> Result<AcceptedPublication> {
        publish_accepted(project, accepted, &recovery_authority(project)?)
    }

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

    fn publish_legacy_recovery(
        project: &Project,
        lock_path: &Utf8Path,
        current: &str,
    ) -> eyre::Result<(Utf8PathBuf, Utf8PathBuf)> {
        let marker = recovery_path(lock_path);
        let state_path = unique_state_path(lock_path);
        let state = RecoveryState::new("original", current)?;
        publish_exclusive_json(&state_path, &state)
            .map_err(|error| eyre::eyre!("publish legacy state: {error:?}"))?;
        let record = RecoveryRecord {
            format: RECOVERY_FORMAT.to_string(),
            project_root: canonical_project_root(project)?,
            original_digest: lock_digest("original"),
            state_file: state_path
                .file_name()
                .ok_or_else(|| eyre::eyre!("state path has no file name"))?
                .to_string(),
            original_lock: "original".to_string(),
        };
        publish_trusted_record(project, &marker, &record)?;
        std::fs::write(lock_path, current)?;
        Ok((marker, state_path))
    }

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
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("non-UTF-8 project path: {path:?}"))?;
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

    #[test]
    fn authority_discovery_finds_anchor_only_projects() -> eyre::Result<()> {
        let (_directory, project, _lock_path) = setup();
        let authority = recovery_authority(&project)?;
        let anchor_path = recovery_anchor_path(&authority)?;
        let anchor = RecoveryAnchor::new(&project, b"record")?;
        publish_exclusive_json(&anchor_path, &anchor)
            .map_err(|error| eyre::eyre!("publish anchor: {error:?}"))?;

        assert_eq!(recovery_authority_projects(&project.root)?, [project.root]);
        Ok(())
    }

    #[test]
    fn authority_discovery_and_recovery_find_private_anchor_publications() -> eyre::Result<()> {
        let (_directory, project, _lock_path) = setup();
        let authority = recovery_authority(&project)?;
        let anchor_path = recovery_anchor_path(&authority)?;
        let anchor = RecoveryAnchor::new(&project, b"record")?;
        let contents = serde_json::to_vec(&anchor)?;
        let private = create_synced_private_file(&anchor_path, &contents)?;

        let projects = recovery_authority_projects(&project.root)?;
        assert_eq!(projects.as_slice(), std::slice::from_ref(&project.root));
        std::assert_matches!(
            test_recover(&project)?.disposition,
            RecoveryDisposition::CleanupOnly
        );
        assert!(!private.exists());
        Ok(())
    }

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
    fn recovery_restores_a_legacy_recorded_transaction_state() -> eyre::Result<()> {
        let (_dir, project, lock_path) = setup();
        let (marker, state_path) = publish_legacy_recovery(&project, &lock_path, "candidate")?;

        std::assert_matches!(
            test_recover(&project)?.disposition,
            RecoveryDisposition::Restored
        );
        assert_eq!(std::fs::read_to_string(&lock_path)?, "original");
        assert!(!marker.exists());
        assert!(!state_path.exists());
        Ok(())
    }

    #[test]
    fn recovery_reports_a_valid_unreferenced_state_file_without_deleting_it() -> eyre::Result<()> {
        let (_dir, project, lock_path) = setup();
        let state_path = unique_state_path(&lock_path);
        let state = RecoveryState::new("original", "candidate")?;
        publish_exclusive_json(&state_path, &state)
            .map_err(|error| eyre::eyre!("publish orphan state: {error:?}"))?;

        let error = test_recover(&project)
            .err()
            .ok_or_else(|| eyre::eyre!("orphan state was not reported"))?;

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        assert!(state_path.exists());
        assert_eq!(std::fs::read_to_string(&lock_path)?, "original");
        Ok(())
    }

    #[test]
    fn exclusive_publication_never_clobbers_an_existing_marker() {
        let (_dir, _project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        cooldown_core::fs::atomic_write(marker.as_std_path(), b"user data")
            .expect("write existing marker");
        let state = RecoveryState::new("original", "candidate").expect("build state");

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
        let state = RecoveryState::new("original", "candidate")?;

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
        let state = RecoveryState::new("original", "candidate")?;

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
        let state = RecoveryState::new("original", "candidate")?;
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
        let state = RecoveryState::new("original", "candidate")?;
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
        let state_path = unique_state_path(&lock_path);
        let record = RecoveryRecord {
            format: RECOVERY_FORMAT.to_string(),
            project_root: canonical_project_root(&project)?,
            original_digest: lock_digest("original"),
            state_file: state_path
                .file_name()
                .ok_or_else(|| eyre::eyre!("state path has no file name"))?
                .to_string(),
            original_lock: "original".to_string(),
        };
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
    fn recovery_artifact_names_cover_public_private_and_staged_state() {
        for name in [
            RECOVERY_MARKER,
            "Cargo.lock.cooldown-recovery-123-456.state",
            ".Cargo.lock.cooldown-recovery.123.0.publish",
            ".Cargo.lock.cooldown-recovery-123-456.state.123.0.publish",
        ] {
            assert!(is_recovery_artifact_name(name), "missed {name}");
        }
        for name in [
            "Cargo.lock",
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

    #[test]
    fn malformed_state_digest_leaves_the_transaction_untouched() -> eyre::Result<()> {
        let (_dir, project, lock_path) = setup();
        let (marker, state_path) = publish_legacy_recovery(&project, &lock_path, "candidate")?;
        let state = RecoveryState {
            format: RECOVERY_STATE_FORMAT.to_string(),
            previous_digest: lock_digest("original"),
            candidate_digest: "not-a-digest".to_string(),
        };
        cooldown_core::fs::atomic_write(state_path.as_std_path(), &serde_json::to_vec(&state)?)?;

        let error = test_recover(&project)
            .err()
            .ok_or_else(|| eyre::eyre!("malformed state was accepted"))?;

        std::assert_matches!(error, CoreError::LockUnreadable(_));
        assert_eq!(std::fs::read_to_string(&lock_path)?, "candidate");
        assert!(marker.exists());
        assert!(state_path.exists());
        Ok(())
    }

    #[test]
    fn read_side_pending_check_never_recovers_the_lock() -> eyre::Result<()> {
        let (_dir, project, lock_path) = setup();
        let (marker, _state_path) = publish_legacy_recovery(&project, &lock_path, "candidate")?;

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
