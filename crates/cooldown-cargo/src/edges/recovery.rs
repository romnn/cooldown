//! Owns restart-visible publication and live state transitions for Cargo project mutations.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::fs::DurableWriteError;
use cooldown_core::{
    AcceptedProjectState, CoreError, Diagnostic, DiagnosticKind, Project, ProjectMutationFile,
    ProjectMutationJournal, Result,
};
use sha2::{Digest, Sha256};
use std::io::Write;

const RECOVERY_FORMAT: &str = "cooldown-cargo-lock-recovery-v2";
const RECOVERY_STATE_FORMAT: &str = "cooldown-cargo-lock-recovery-state-v1";
const PROJECT_RECOVERY_FORMAT: &str = "cooldown-cargo-project-recovery-v1";
pub(crate) const RECOVERY_MARKER: &str = "Cargo.lock.cooldown-recovery";

/// A speculative lock correction whose recovery record remains authoritative until commit.
///
/// The transaction keeps the original bytes immutable on disk and rewrites only a small digest
/// record between isolation probes.
/// Every transition verifies the live lock against its expected bytes before replacing it or
/// deleting recovery state.
#[derive(Debug)]
pub(super) struct SpeculativeLockTransaction {
    lock_path: Utf8PathBuf,
    recovery_path: Utf8PathBuf,
    state_path: Utf8PathBuf,
    original_lock: String,
    current_lock: String,
    staged_lock: Option<String>,
    record: RecoveryRecord,
    state: RecoveryState,
}

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

impl SpeculativeLockTransaction {
    /// Starts a transaction with `candidate_lock` installed as its staged state.
    pub(super) fn begin(
        project: &Project,
        lock_path: &Utf8Path,
        original_lock: &str,
        candidate_lock: &str,
    ) -> Result<Self> {
        Self::begin_with_installer(
            project,
            lock_path,
            original_lock,
            candidate_lock,
            durable_write,
        )
    }

    fn begin_with_installer<F>(
        project: &Project,
        lock_path: &Utf8Path,
        original_lock: &str,
        candidate_lock: &str,
        install: F,
    ) -> Result<Self>
    where
        F: FnOnce(&std::path::Path, &[u8]) -> Result<()>,
    {
        ensure_lock_equals(
            lock_path,
            original_lock,
            "starting a speculative correction",
            None,
        )?;
        let recovery_path = recovery_path(lock_path);
        if !path_exists(&recovery_path)? {
            ensure_no_orphan_artifacts(lock_path)?;
        }
        let mut transaction = Self::publish_records(
            project,
            lock_path,
            recovery_path,
            original_lock,
            candidate_lock,
        )?;
        transaction.install_initial_candidate(original_lock, candidate_lock, install)?;
        Ok(transaction)
    }

    fn publish_records(
        project: &Project,
        lock_path: &Utf8Path,
        recovery_path: Utf8PathBuf,
        original_lock: &str,
        candidate_lock: &str,
    ) -> Result<Self> {
        let state_path = unique_state_path(lock_path);
        let state_file = state_path
            .file_name()
            .ok_or_else(|| CoreError::PathEncoding(format!("path has no file name: {state_path}")))?
            .to_string();
        let record = RecoveryRecord {
            format: RECOVERY_FORMAT.to_string(),
            project_root: canonical_project_root(project)?,
            original_digest: lock_digest(original_lock),
            state_file,
            original_lock: original_lock.to_string(),
        };
        let state = RecoveryState::new(original_lock, candidate_lock)?;
        if let Err(error) = publish_exclusive_json(&state_path, &state) {
            if matches!(&error, PublicationError::DurabilityUncertain(_))
                && let Err(cleanup) = remove_file(&state_path)
            {
                tracing::warn!(
                    path = %state_path,
                    error = %cleanup,
                    "could not remove unpublished Cargo.lock recovery state"
                );
            }
            return Err(CoreError::Filesystem(format!(
                "could not publish private Cargo.lock recovery state at {state_path}: {}",
                error.into_core_error(&state_path)
            )));
        }
        if let Err(error) = publish_exclusive_json(&recovery_path, &record) {
            return match error {
                PublicationError::NotPublished(error) => {
                    if let Err(cleanup_error) = remove_file(&state_path) {
                        tracing::warn!(
                            path = %state_path,
                            error = %cleanup_error,
                            "could not remove unpublished Cargo.lock recovery state"
                        );
                    }
                    Err(error)
                }
                uncertain @ PublicationError::DurabilityUncertain(_) => {
                    let error = uncertain.into_core_error(&recovery_path);
                    Err(pending_recovery(&recovery_path, &error))
                }
            };
        }

        Ok(SpeculativeLockTransaction {
            lock_path: lock_path.to_owned(),
            recovery_path,
            state_path,
            original_lock: original_lock.to_string(),
            current_lock: original_lock.to_string(),
            staged_lock: Some(candidate_lock.to_string()),
            record,
            state,
        })
    }

    fn install_initial_candidate<F>(
        &mut self,
        original_lock: &str,
        candidate_lock: &str,
        install: F,
    ) -> Result<()>
    where
        F: FnOnce(&std::path::Path, &[u8]) -> Result<()>,
    {
        if let Err(error) = ensure_lock_equals(
            &self.lock_path,
            original_lock,
            "installing the first speculative correction",
            Some(&self.recovery_path),
        ) {
            return Err(pending_recovery(&self.recovery_path, &error));
        }
        if let Err(error) = install(self.lock_path.as_std_path(), candidate_lock.as_bytes()) {
            match lock_equals(&self.lock_path, original_lock) {
                Ok(true) => {
                    self.staged_lock = None;
                    return match self.remove_record() {
                        Ok(CommitOutcome::Committed) => Err(error),
                        Ok(CommitOutcome::DurabilityUncertain(durability)) => {
                            let failure = CoreError::Filesystem(format!(
                                "candidate installation failed: {error}; recovery-marker durability is uncertain: {durability}; outer rollback was refused"
                            ));
                            Err(pending_recovery(&self.recovery_path, &failure))
                        }
                        Err(cleanup) => Err(pending_recovery(&self.recovery_path, &cleanup)),
                    };
                }
                Ok(false) => {}
                Err(check) => {
                    let failure = CoreError::Filesystem(format!(
                        "candidate installation failed: {error}; live lock inspection also failed: {check}"
                    ));
                    return Err(pending_recovery(&self.recovery_path, &failure));
                }
            }
            return Err(pending_recovery(&self.recovery_path, &error));
        }
        Ok(())
    }

    /// Installs another candidate from the last accepted or rejected state.
    pub(super) fn stage(&mut self, candidate_lock: &str) -> Result<()> {
        if self.staged_lock.is_some() {
            return Err(CoreError::System(
                "cannot stage a Cargo.lock candidate while another is awaiting a decision"
                    .to_string(),
            ));
        }
        ensure_lock_equals(
            &self.lock_path,
            &self.current_lock,
            "preparing a speculative correction",
            Some(&self.recovery_path),
        )?;
        let state = RecoveryState::new(&self.current_lock, candidate_lock)?;
        let contents = serde_json::to_vec(&state)
            .map_err(|error| CoreError::Serialization(error.to_string()))?;
        match cooldown_core::fs::atomic_write_durable(self.state_path.as_std_path(), &contents) {
            Ok(()) => self.state = state,
            Err(DurableWriteError::NotCommitted(error)) => return Err(error),
            Err(DurableWriteError::DurabilityUncertain(error)) => {
                self.state = state;
                return Err(DurableWriteError::DurabilityUncertain(error)
                    .into_core_error(self.state_path.as_std_path()));
            }
        }
        ensure_lock_equals(
            &self.lock_path,
            &self.current_lock,
            "installing a speculative correction",
            Some(&self.recovery_path),
        )?;
        self.staged_lock = Some(candidate_lock.to_string());
        match cooldown_core::fs::atomic_write_durable(
            self.lock_path.as_std_path(),
            candidate_lock.as_bytes(),
        ) {
            Ok(()) => {}
            Err(DurableWriteError::NotCommitted(error)) => {
                if lock_equals(&self.lock_path, &self.current_lock)? {
                    self.staged_lock = None;
                }
                return Err(error);
            }
            Err(error @ DurableWriteError::DurabilityUncertain(_)) => {
                return Err(error.into_core_error(self.lock_path.as_std_path()));
            }
        }
        Ok(())
    }

    /// Accepts the staged bytes only when they are still the verified live lock.
    pub(super) fn accept(&mut self) -> Result<()> {
        let candidate = self.staged_lock.as_ref().ok_or_else(|| {
            CoreError::System("no staged Cargo.lock candidate to accept".to_string())
        })?;
        ensure_lock_equals(
            &self.lock_path,
            candidate,
            "accepting the verified correction",
            Some(&self.recovery_path),
        )?;
        self.current_lock.clone_from(candidate);
        self.staged_lock = None;
        Ok(())
    }

    /// Rejects the staged bytes and restores the last accepted state after checking for drift.
    pub(super) fn reject(&mut self) -> Result<()> {
        let candidate = self.staged_lock.as_ref().ok_or_else(|| {
            CoreError::System("no staged Cargo.lock candidate to reject".to_string())
        })?;
        ensure_lock_equals(
            &self.lock_path,
            candidate,
            "rejecting an unverified correction",
            Some(&self.recovery_path),
        )?;
        match cooldown_core::fs::atomic_write_durable(
            self.lock_path.as_std_path(),
            self.current_lock.as_bytes(),
        ) {
            Ok(()) => {}
            Err(DurableWriteError::NotCommitted(error)) => return Err(error),
            Err(error @ DurableWriteError::DurabilityUncertain(_)) => {
                self.staged_lock = None;
                return Err(error.into_core_error(self.lock_path.as_std_path()));
            }
        }
        ensure_lock_equals(
            &self.lock_path,
            &self.current_lock,
            "restoring the prior correction state",
            Some(&self.recovery_path),
        )?;
        self.staged_lock = None;
        Ok(())
    }

    /// Commits the last accepted state and consumes its recovery marker.
    pub(super) fn commit(&mut self) -> Result<CommitOutcome> {
        if self.staged_lock.is_some() {
            return Err(CoreError::System(
                "cannot commit a Cargo.lock transaction with an undecided candidate".to_string(),
            ));
        }
        ensure_lock_equals(
            &self.lock_path,
            &self.current_lock,
            "committing the verified correction",
            Some(&self.recovery_path),
        )?;
        self.remove_record()
    }

    /// Restores the transaction-start lock from any recognized live transaction state.
    pub(super) fn rollback(&mut self) -> Result<CommitOutcome> {
        let expected = self
            .staged_lock
            .as_deref()
            .unwrap_or(self.current_lock.as_str());
        ensure_lock_equals(
            &self.lock_path,
            expected,
            "rolling back the correction transaction",
            Some(&self.recovery_path),
        )?;
        if expected != self.original_lock {
            durable_write(self.lock_path.as_std_path(), self.original_lock.as_bytes())?;
        }
        ensure_lock_equals(
            &self.lock_path,
            &self.original_lock,
            "finishing correction rollback",
            Some(&self.recovery_path),
        )?;
        self.current_lock.clone_from(&self.original_lock);
        self.staged_lock = None;
        self.remove_record()
    }

    fn remove_record(&self) -> Result<CommitOutcome> {
        if read_json::<RecoveryRecord>(&self.recovery_path)? != self.record {
            return Err(untrusted_record(&self.recovery_path));
        }
        if read_json::<RecoveryState>(&self.state_path)? != self.state {
            return Err(untrusted_record(&self.state_path));
        }
        let outcome = remove_transaction_marker(&self.recovery_path)?;
        if let CommitOutcome::DurabilityUncertain(_) = outcome {
            return Ok(outcome);
        }
        if let Err(error) = remove_file(&self.state_path) {
            tracing::warn!(
                path = %self.state_path,
                error = %error,
                "could not remove completed Cargo.lock recovery state"
            );
        }
        Ok(CommitOutcome::Committed)
    }
}

impl RecoveryState {
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
        Ok(ProjectMutationJournal { files })
    }
}

impl ProjectRecoveryFile {
    fn new(original: &ProjectMutationFile, candidate: &ProjectMutationFile) -> Result<Self> {
        let original_contents = utf8_contents(original)?;
        let candidate_contents = utf8_contents(candidate)?;
        Ok(ProjectRecoveryFile {
            path: original.path.to_string(),
            original: original_contents,
            candidate: candidate_contents,
            original_permissions: RecoveryPermissions::new(original.permissions.as_ref()),
            candidate_permissions: RecoveryPermissions::new(candidate.permissions.as_ref()),
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
        Ok(ProjectMutationFile {
            path: Utf8PathBuf::from(&self.path),
            contents: self
                .original
                .as_ref()
                .map(|value| value.as_bytes().to_vec()),
            permissions: self.original_permissions.to_permissions(root)?,
        })
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
        live.path == Utf8Path::new(&self.path)
            && live.contents.as_deref() == contents.map(str::as_bytes)
            && permissions.matches(live.permissions.as_ref())
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
        allow(
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
    file.contents
        .as_ref()
        .map(|contents| {
            String::from_utf8(contents.clone()).map_err(|error| {
                CoreError::Serialization(format!(
                    "Cargo mutation output {} is not UTF-8: {error}",
                    file.path
                ))
            })
        })
        .transpose()
}

/// Publishes one accepted Cargo project state under a durable whole-project recovery record.
pub(crate) fn publish_accepted(
    project: &Project,
    accepted: &AcceptedProjectState,
) -> Result<Vec<Diagnostic>> {
    publish_accepted_with(project, accepted, AcceptedProjectState::install)
}

fn publish_accepted_with<F>(
    project: &Project,
    accepted: &AcceptedProjectState,
    install: F,
) -> Result<Vec<Diagnostic>>
where
    F: FnOnce(&AcceptedProjectState, &Utf8Path) -> Result<()>,
{
    if accepted.changed_files().next().is_none() {
        return Ok(Vec::new());
    }
    accepted.validate_source(&project.root)?;
    let lock_path = project.root.join("Cargo.lock");
    let recovery_path = recovery_path(&lock_path);
    ensure_no_orphan_artifacts(&lock_path)?;
    let record = ProjectRecoveryRecord::new(project, accepted)?;
    record.validate(project, &recovery_path)?;
    match publish_exclusive_json(&recovery_path, &record) {
        Ok(()) => {}
        Err(PublicationError::NotPublished(error)) => return Err(error),
        Err(error @ PublicationError::DurabilityUncertain(_)) => {
            let error = error.into_core_error(&recovery_path);
            return Err(pending_recovery(&recovery_path, &error));
        }
    }
    if let Err(error) = install(accepted, &project.root) {
        return Err(pending_recovery(&recovery_path, &error));
    }
    match remove_transaction_marker(&recovery_path)? {
        CommitOutcome::Committed => Ok(Vec::new()),
        CommitOutcome::DurabilityUncertain(error) => Ok(vec![Diagnostic::new(
            DiagnosticKind::Filesystem,
            format!(
                "accepted Cargo project state is visible, but recovery-marker removal durability is uncertain: {error}"
            ),
        )]),
    }
}

/// Restores a validated interrupted transaction while the caller holds exclusive project access.
pub(crate) fn recover_pending(project: &Project) -> Result<bool> {
    let lock_path = project.root.join("Cargo.lock");
    let recovery_path = recovery_path(&lock_path);
    if !path_exists(&recovery_path)? {
        ensure_no_orphan_artifacts(&lock_path)?;
        return Ok(false);
    }
    let format = recovery_record_format(&recovery_path)?;
    if format == PROJECT_RECOVERY_FORMAT {
        return recover_project_publication(project, &recovery_path);
    }
    if format != RECOVERY_FORMAT {
        return Err(untrusted_record(&recovery_path));
    }
    let record: RecoveryRecord = read_json(&recovery_path)?;
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
    if current != record.original_lock {
        durable_write(lock_path.as_std_path(), record.original_lock.as_bytes())?;
    }
    ensure_lock_equals(
        &lock_path,
        &record.original_lock,
        "recovering the interrupted correction",
        Some(&recovery_path),
    )?;
    remove_file(&recovery_path)?;
    if let Err(error) = remove_file(&state_path) {
        tracing::warn!(
            path = %state_path,
            error = %error,
            "could not remove recovered Cargo.lock state"
        );
    }
    Ok(true)
}

fn recovery_record_format(path: &Utf8Path) -> Result<String> {
    let value: serde_json::Value = read_json(path)?;
    value
        .get("format")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| untrusted_record(path))
}

fn recover_project_publication(project: &Project, recovery_path: &Utf8Path) -> Result<bool> {
    let record: ProjectRecoveryRecord = read_json(recovery_path)?;
    record.validate(project, recovery_path)?;
    let original = record.original_journal(&project.root)?;
    let live = original.capture_state(&project.root)?;
    let mut saw_original = false;
    let mut saw_candidate = false;
    for (recorded, live) in record.files.iter().zip(live.files()) {
        if !recorded.is_changed() {
            if !recorded.matches_original(live) {
                return Err(CoreError::LockUnreadable(format!(
                    "{} changed after the accepted project trial at {recovery_path}; left all files untouched",
                    project.root.join(&live.path)
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
                project.root.join(&live.path)
            )));
        }
    }
    if saw_original && saw_candidate {
        original.restore_if_unchanged(&project.root, &live)?;
    }
    match remove_transaction_marker(recovery_path)? {
        CommitOutcome::Committed => {}
        CommitOutcome::DurabilityUncertain(error) => {
            tracing::warn!(
                path = %recovery_path,
                error = %error,
                "project recovery completed, but marker-removal durability is uncertain"
            );
        }
    }
    Ok(true)
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
    Ok(())
}

fn recovery_path(lock_path: &Utf8Path) -> Utf8PathBuf {
    lock_path.with_file_name(RECOVERY_MARKER)
}

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
    let state_component = Utf8Path::new(state_file);
    let lock_file = lock_path
        .file_name()
        .ok_or_else(|| CoreError::PathEncoding(format!("path has no file name: {lock_path}")))?;
    let prefix = format!("{lock_file}.cooldown-recovery-");
    let valid_name = state_component
        .parent()
        .is_some_and(|parent| parent.as_str().is_empty())
        && state_file.starts_with(&prefix)
        && state_component.extension() == Some("state");
    if !valid_name {
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
    format!("{:x}", Sha256::digest(text.as_bytes()))
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
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn publish_exclusive_json<T: serde::Serialize>(
    path: &Utf8Path,
    value: &T,
) -> std::result::Result<(), PublicationError> {
    #[cfg(unix)]
    let sync_parent = sync_parent_directory;
    #[cfg(not(unix))]
    let sync_parent = |_path: &Utf8Path| Ok(());
    publish_exclusive_json_with(path, value, sync_parent, remove_file)
}

fn publish_exclusive_json_with<T, S, C>(
    path: &Utf8Path,
    value: &T,
    sync_parent: S,
    cleanup_private: C,
) -> std::result::Result<(), PublicationError>
where
    T: serde::Serialize,
    S: FnOnce(&Utf8Path) -> Result<()>,
    C: FnOnce(&Utf8Path) -> Result<()>,
{
    let contents = serde_json::to_vec(value).map_err(|error| {
        PublicationError::NotPublished(CoreError::Serialization(error.to_string()))
    })?;
    let temp =
        create_synced_private_file(path, &contents).map_err(PublicationError::NotPublished)?;
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
    }
    Ok(())
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
        let orphan_state = validated_state_path(lock_path, &name).is_ok();
        let orphan_publication = publication_target(&name).is_some();
        if orphan_state || orphan_publication {
            return Err(CoreError::LockUnreadable(format!(
                "unreferenced Cargo.lock recovery artifact at {}; left it untouched; inspect and remove it explicitly",
                parent.join(name)
            )));
        }
    }
    Ok(())
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

fn read_json<T: serde::de::DeserializeOwned>(path: &Utf8Path) -> Result<T> {
    let contents = std::fs::read(path)?;
    serde_json::from_slice(&contents).map_err(|error| {
        CoreError::LockUnreadable(format!(
            "invalid Cargo.lock recovery record at {path}: {error}; left all files untouched"
        ))
    })
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

fn remove_file_with<S>(path: &Utf8Path, sync_parent: S) -> std::result::Result<(), RemovalError>
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
        let project = project(&root);
        let lock_path = root.join("Cargo.lock");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"original").expect("write lock");
        (dir, project, lock_path)
    }

    fn accepted_project_state(
        project: &Project,
        paths: &[(&str, &str)],
    ) -> color_eyre::Result<AcceptedProjectState> {
        let candidate = tempfile::tempdir()?;
        let candidate_root = Utf8PathBuf::from_path_buf(candidate.path().to_owned())
            .map_err(|path| color_eyre::eyre::eyre!("non-UTF-8 candidate path: {path:?}"))?;
        let mut files = Vec::new();
        for (path, contents) in paths {
            let relative = Utf8Path::new(path);
            files.push(ProjectMutationJournal::capture_file(
                &project.root,
                relative,
            )?);
            let target = candidate_root.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(target, contents)?;
        }
        let original = ProjectMutationJournal { files };
        let candidate = original.capture_state(&candidate_root)?;
        Ok(AcceptedProjectState::new(original, candidate)?)
    }

    #[test]
    fn accepted_project_state_publishes_once_and_consumes_its_record() -> color_eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;

        let warnings = publish_accepted(&project, &accepted)?;

        assert!(warnings.is_empty());
        assert_eq!(std::fs::read_to_string(&lock_path)?, "accepted");
        assert!(!recovery_path(&lock_path).exists());
        Ok(())
    }

    #[test]
    fn project_publication_refuses_source_drift_before_creating_a_record() -> color_eyre::Result<()>
    {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;
        std::fs::write(&lock_path, "external")?;

        let error = publish_accepted(&project, &accepted)
            .err()
            .ok_or_else(|| color_eyre::eyre::eyre!("drifted publication succeeded"))?;

        assert!(matches!(error, CoreError::LockConflict(_)));
        assert_eq!(std::fs::read_to_string(&lock_path)?, "external");
        assert!(!recovery_path(&lock_path).exists());
        Ok(())
    }

    #[test]
    fn recovery_restores_a_mixed_whole_project_publication() -> color_eyre::Result<()> {
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
        publish_exclusive_json(&marker, &record)
            .map_err(|error| color_eyre::eyre::eyre!("publish marker: {error:?}"))?;
        std::fs::write(&lock_path, "accepted lock")?;

        assert!(recover_pending(&project)?);
        assert_eq!(std::fs::read_to_string(&lock_path)?, "original");
        assert_eq!(
            std::fs::read_to_string(&manifest_path)?,
            "original manifest"
        );
        assert!(!marker.exists());
        Ok(())
    }

    #[test]
    fn failed_publication_keeps_one_receipt_that_recovers_the_preimage() -> color_eyre::Result<()> {
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

        let error = publish_accepted_with(&project, &accepted, |_accepted, root| {
            std::fs::write(root.join("Cargo.lock"), "accepted lock")?;
            Err(CoreError::Filesystem(
                "injected publication failure".to_string(),
            ))
        })
        .err()
        .ok_or_else(|| color_eyre::eyre::eyre!("injected publication unexpectedly succeeded"))?;

        assert!(matches!(error, CoreError::PendingRecovery(_)));
        assert!(recovery_path(&lock_path).exists());
        assert!(recover_pending(&project)?);
        assert_eq!(std::fs::read_to_string(&lock_path)?, "original");
        assert_eq!(
            std::fs::read_to_string(&manifest_path)?,
            "original manifest"
        );
        assert!(!recovery_path(&lock_path).exists());
        Ok(())
    }

    #[test]
    fn recovery_accepts_a_fully_published_project() -> color_eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;
        let marker = recovery_path(&lock_path);
        let record = ProjectRecoveryRecord::new(&project, &accepted)?;
        publish_exclusive_json(&marker, &record)
            .map_err(|error| color_eyre::eyre::eyre!("publish marker: {error:?}"))?;
        accepted.install(&project.root)?;

        assert!(recover_pending(&project)?);
        assert_eq!(std::fs::read_to_string(&lock_path)?, "accepted");
        assert!(!marker.exists());
        Ok(())
    }

    #[test]
    fn recovery_refuses_drift_in_an_unchanged_tracked_manifest() -> color_eyre::Result<()> {
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
        publish_exclusive_json(&marker, &record)
            .map_err(|error| color_eyre::eyre::eyre!("publish marker: {error:?}"))?;
        accepted.install(&project.root)?;
        std::fs::write(&manifest_path, "external manifest")?;

        assert!(matches!(
            recover_pending(&project),
            Err(CoreError::LockUnreadable(_))
        ));
        assert_eq!(std::fs::read_to_string(&lock_path)?, "accepted");
        assert_eq!(
            std::fs::read_to_string(&manifest_path)?,
            "external manifest"
        );
        assert!(marker.exists());
        Ok(())
    }

    #[test]
    fn whole_project_record_rejects_inconsistent_permissions() -> color_eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;
        let mut record = ProjectRecoveryRecord::new(&project, &accepted)?;
        let file = record
            .files
            .first_mut()
            .ok_or_else(|| color_eyre::eyre::eyre!("record omitted its changed lock"))?;
        file.original_permissions.present = false;

        assert!(matches!(
            record.validate(&project, &recovery_path(&lock_path)),
            Err(CoreError::LockUnreadable(_))
        ));
        Ok(())
    }

    #[test]
    fn recovery_restores_only_a_recorded_transaction_state() {
        let (_dir, project, lock_path) = setup();
        let transaction =
            SpeculativeLockTransaction::begin(&project, &lock_path, "original", "candidate")
                .expect("begin transaction");

        assert!(recover_pending(&project).expect("recover lock"));
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "original"
        );
        assert!(!transaction.recovery_path.exists());
        assert!(!transaction.state_path.exists());
    }

    #[test]
    fn successful_verification_refuses_external_drift() {
        let (_dir, project, lock_path) = setup();
        let mut transaction =
            SpeculativeLockTransaction::begin(&project, &lock_path, "original", "candidate")
                .expect("begin transaction");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"external")
            .expect("write external lock");

        let error = transaction.accept().expect_err("reject drift");

        assert!(matches!(error, CoreError::LockUnreadable(_)));
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "external"
        );
        assert!(transaction.recovery_path.exists());
        assert!(transaction.state_path.exists());
    }

    #[test]
    fn unsuccessful_verification_refuses_external_drift() {
        let (_dir, project, lock_path) = setup();
        let mut transaction =
            SpeculativeLockTransaction::begin(&project, &lock_path, "original", "candidate")
                .expect("begin transaction");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"external")
            .expect("write external lock");

        let error = transaction.reject().expect_err("reject drift");

        assert!(matches!(error, CoreError::LockUnreadable(_)));
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "external"
        );
        assert!(transaction.recovery_path.exists());
        assert!(transaction.state_path.exists());
    }

    #[test]
    fn commit_and_rollback_both_check_the_live_lock() {
        for transition in ["commit", "rollback"] {
            let (_dir, project, lock_path) = setup();
            let mut transaction =
                SpeculativeLockTransaction::begin(&project, &lock_path, "original", "candidate")
                    .expect("begin transaction");
            transaction.accept().expect("accept candidate");
            cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"external")
                .expect("write external lock");

            let error = if transition == "commit" {
                transaction.commit().expect_err("reject commit drift")
            } else {
                transaction.rollback().expect_err("reject rollback drift")
            };

            assert!(matches!(error, CoreError::LockUnreadable(_)));
            assert_eq!(
                std::fs::read_to_string(&lock_path).expect("read lock"),
                "external"
            );
            assert!(transaction.recovery_path.exists());
        }
    }

    #[test]
    fn commit_does_not_delete_a_replaced_recovery_record() {
        let (_dir, project, lock_path) = setup();
        let mut transaction =
            SpeculativeLockTransaction::begin(&project, &lock_path, "original", "candidate")
                .expect("begin transaction");
        transaction.accept().expect("accept candidate");
        cooldown_core::fs::atomic_write(transaction.recovery_path.as_std_path(), b"user data")
            .expect("replace marker");

        let error = transaction.commit().expect_err("reject replaced marker");

        assert!(matches!(error, CoreError::LockUnreadable(_)));
        assert_eq!(
            std::fs::read_to_string(&transaction.recovery_path).expect("read marker"),
            "user data"
        );
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "candidate"
        );
    }

    #[test]
    fn initial_candidate_failure_cleans_an_unneeded_record() {
        let (_dir, project, lock_path) = setup();
        let marker = recovery_path(&lock_path);

        let error = SpeculativeLockTransaction::begin_with_installer(
            &project,
            &lock_path,
            "original",
            "candidate",
            |_path, _contents| Err(CoreError::Filesystem("injected write failure".to_string())),
        )
        .expect_err("candidate write fails");

        assert!(matches!(error, CoreError::Filesystem(_)));
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "original"
        );
        assert!(!marker.exists());
        let entries: Vec<_> = std::fs::read_dir(project.root.as_std_path())
            .expect("read project")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !entries
                .iter()
                .any(|name| Utf8Path::new(name).extension() == Some("state"))
        );
    }

    #[test]
    fn recovery_record_stores_original_bytes_once() {
        let (_dir, project, lock_path) = setup();
        let transaction =
            SpeculativeLockTransaction::begin(&project, &lock_path, "original", "candidate")
                .expect("begin transaction");
        let marker: RecoveryRecord = read_json(&transaction.recovery_path).expect("read marker");
        let state: RecoveryState = read_json(&transaction.state_path).expect("read state");

        assert_eq!(marker.original_lock, "original");
        assert_eq!(state.previous_digest, lock_digest("original"));
        assert_eq!(state.candidate_digest, lock_digest("candidate"));
    }

    #[test]
    fn recovery_reports_a_valid_unreferenced_state_file_without_deleting_it()
    -> color_eyre::Result<()> {
        let (_dir, project, lock_path) = setup();
        let state_path = unique_state_path(&lock_path);
        let state = RecoveryState::new("original", "candidate")?;
        publish_exclusive_json(&state_path, &state)
            .map_err(|error| color_eyre::eyre::eyre!("publish orphan state: {error:?}"))?;

        let error = recover_pending(&project)
            .err()
            .ok_or_else(|| color_eyre::eyre::eyre!("orphan state was not reported"))?;

        assert!(matches!(error, CoreError::LockUnreadable(_)));
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

        assert!(matches!(
            error,
            PublicationError::NotPublished(CoreError::LockConflict(_))
        ));
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read marker"),
            "user data"
        );
    }

    #[test]
    fn publication_reports_uncertain_durability_without_removing_evidence() -> color_eyre::Result<()>
    {
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
        assert!(matches!(
            result,
            Err(PublicationError::DurabilityUncertain(_))
        ));
        assert!(marker.exists());
        assert!(!publication_temps(&lock_path)?.is_empty());
        Ok(())
    }

    #[test]
    fn publication_cleanup_failure_does_not_hide_a_committed_publication() -> color_eyre::Result<()>
    {
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
        assert!(result.is_ok());
        assert!(marker.exists());
        assert!(!publication_temps(&lock_path)?.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_artifacts_are_owner_only() -> color_eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, project, lock_path) = setup();
        let transaction =
            SpeculativeLockTransaction::begin(&project, &lock_path, "original", "candidate")?;

        for path in [&transaction.recovery_path, &transaction.state_path] {
            assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }

    #[test]
    fn orphan_publications_are_reported_without_deletion() -> color_eyre::Result<()> {
        let (_dir, project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        let state_path = unique_state_path(&lock_path);
        let record = RecoveryRecord {
            format: RECOVERY_FORMAT.to_string(),
            project_root: canonical_project_root(&project)?,
            original_digest: lock_digest("original"),
            state_file: state_path
                .file_name()
                .ok_or_else(|| color_eyre::eyre::eyre!("state path has no file name"))?
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
        .map_err(|error| color_eyre::eyre::eyre!("publish recovery marker: {error:?}"))?;
        std::fs::remove_file(&marker)?;
        assert_eq!(publication_temps(&lock_path)?.len(), 1);

        let error = ensure_no_orphan_artifacts(&lock_path)
            .err()
            .ok_or_else(|| color_eyre::eyre::eyre!("orphan publication was not reported"))?;

        assert!(matches!(error, CoreError::LockUnreadable(_)));
        assert_eq!(publication_temps(&lock_path)?.len(), 1);
        Ok(())
    }

    #[test]
    fn removal_reports_uncertain_durability_after_unlink() -> color_eyre::Result<()> {
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
        assert!(matches!(result, Err(RemovalError::DurabilityUncertain(_))));
        assert!(!marker.exists());
        Ok(())
    }

    #[test]
    fn marker_sync_failure_becomes_a_committed_warning() -> color_eyre::Result<()> {
        let (_dir, _project, lock_path) = setup();
        let marker = recovery_path(&lock_path);
        std::fs::write(&marker, b"recovery evidence")?;

        let outcome = remove_transaction_marker_with(&marker, |_path| {
            Err(CoreError::Filesystem(
                "injected directory sync failure".to_string(),
            ))
        })?;
        let CommitOutcome::DurabilityUncertain(error) = outcome else {
            return Err(color_eyre::eyre::eyre!(
                "post-unlink sync failure was not reported as committed"
            ));
        };

        assert!(error.to_string().contains("syncing its directory failed"));
        assert!(!marker.exists());
        Ok(())
    }

    fn publication_temps(lock_path: &Utf8Path) -> color_eyre::Result<Vec<std::fs::DirEntry>> {
        let parent = lock_path
            .parent()
            .ok_or_else(|| color_eyre::eyre::eyre!("lock path has no parent: {lock_path}"))?;
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

        let error = recover_pending(&project).expect_err("reject marker");

        assert!(matches!(error, CoreError::LockUnreadable(_)));
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
    fn malformed_state_digest_leaves_the_transaction_untouched() {
        let (_dir, project, lock_path) = setup();
        let transaction =
            SpeculativeLockTransaction::begin(&project, &lock_path, "original", "candidate")
                .expect("begin transaction");
        let state = RecoveryState {
            format: RECOVERY_STATE_FORMAT.to_string(),
            previous_digest: lock_digest("original"),
            candidate_digest: "not-a-digest".to_string(),
        };
        cooldown_core::fs::atomic_write(
            transaction.state_path.as_std_path(),
            &serde_json::to_vec(&state).expect("serialize state"),
        )
        .expect("write state");

        let error = recover_pending(&project).expect_err("reject malformed state");

        assert!(matches!(error, CoreError::LockUnreadable(_)));
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "candidate"
        );
        assert!(transaction.recovery_path.exists());
        assert!(transaction.state_path.exists());
    }

    #[test]
    fn read_side_pending_check_never_recovers_the_lock() {
        let (_dir, project, lock_path) = setup();
        let transaction =
            SpeculativeLockTransaction::begin(&project, &lock_path, "original", "candidate")
                .expect("begin transaction");

        let error = ensure_no_pending(&project).expect_err("pending transaction");

        assert!(matches!(error, CoreError::StaleLock(_)));
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "candidate"
        );
        assert!(transaction.recovery_path.exists());
    }
}
