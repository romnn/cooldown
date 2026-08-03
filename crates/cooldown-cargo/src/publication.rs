//! Publishes accepted Cargo project states and recovers interrupted source transactions.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    AcceptedProjectState, AcceptedPublication, CoreError, Diagnostic, DiagnosticKind,
    MutationRecovery, Project, ProjectMutationFile, ProjectMutationJournal, RecoveryDisposition,
    Result,
};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;

const RECOVERY_FORMAT: &str = "cooldown-cargo-lock-recovery-v2";
const RECOVERY_STATE_FORMAT: &str = "cooldown-cargo-lock-recovery-state-v1";
const PROJECT_RECOVERY_FORMAT: &str = "cooldown-cargo-project-recovery-v1";
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
        ProjectMutationJournal::new(files)
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

/// Publishes one accepted Cargo project state under an owner-only whole-project recovery record.
pub(crate) fn publish_accepted(
    project: &Project,
    accepted: &AcceptedProjectState,
) -> Result<AcceptedPublication> {
    publish_accepted_with(project, accepted, AcceptedProjectState::install)
}

fn publish_accepted_with<F>(
    project: &Project,
    accepted: &AcceptedProjectState,
    install: F,
) -> Result<AcceptedPublication>
where
    F: FnOnce(&AcceptedProjectState, &Utf8Path) -> Result<()>,
{
    publish_accepted_with_marker(project, accepted, install, remove_transaction_marker)
}

fn publish_accepted_with_marker<F, R>(
    project: &Project,
    accepted: &AcceptedProjectState,
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
    accepted.validate_source(&project.root)?;
    let lock_path = project.root.join("Cargo.lock");
    let recovery_path = recovery_path(&lock_path);
    ensure_no_orphan_artifacts(&lock_path)?;
    let record = ProjectRecoveryRecord::new(project, accepted)?;
    record.validate(project, &recovery_path)?;
    let publication = match publish_exclusive_json(&recovery_path, &record) {
        Ok(outcome) => outcome,
        Err(PublicationError::NotPublished(error)) => return Err(error),
        Err(error @ PublicationError::DurabilityUncertain(_)) => {
            let error = error.into_core_error(&recovery_path);
            return Err(pending_recovery(&recovery_path, &error));
        }
    };
    if let Err(error) = install(accepted, &project.root) {
        return Err(pending_recovery(&recovery_path, &error));
    }
    let marker = match remove_marker(&recovery_path) {
        Ok(marker) => marker,
        Err(error) => {
            return Ok(AcceptedPublication::PublishedPendingRecovery {
                warnings: Vec::new(),
                error: pending_recovery(&recovery_path, &error),
            });
        }
    };
    let mut warnings = match marker {
        CommitOutcome::Committed => Vec::new(),
        CommitOutcome::DurabilityUncertain(error) => vec![Diagnostic::new(
            DiagnosticKind::Filesystem,
            format!(
                "accepted Cargo project state is visible, but recovery-marker removal durability is uncertain: {error}"
            ),
        )],
    };
    retry_private_cleanup(publication, &mut warnings, remove_file);
    Ok(AcceptedPublication::Published { warnings })
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
pub(crate) fn recover_pending(project: &Project) -> Result<MutationRecovery> {
    let lock_path = project.root.join("Cargo.lock");
    let recovery_path = recovery_path(&lock_path);
    if !path_exists(&recovery_path)? {
        ensure_no_orphan_artifacts(&lock_path)?;
        return Ok(MutationRecovery::settled(RecoveryDisposition::Unchanged));
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
    let mut recovery = MutationRecovery::settled(if restored {
        RecoveryDisposition::Restored
    } else {
        RecoveryDisposition::CleanupOnly
    });
    if let CommitOutcome::DurabilityUncertain(error) = remove_transaction_marker(&recovery_path)? {
        recovery.warnings.push(Diagnostic::new(
            DiagnosticKind::Filesystem,
            format!(
                "recovery settled Cargo.lock, but marker-removal durability is uncertain at {recovery_path}: {error}"
            ),
        ));
    }
    if let Err(error) = remove_file(&state_path) {
        recovery.warnings.push(Diagnostic::new(
            DiagnosticKind::Filesystem,
            format!("recovery settled Cargo.lock, but could not remove state artifact {state_path}: {error}"),
        ));
    }
    Ok(recovery)
}

fn recovery_record_format(path: &Utf8Path) -> Result<String> {
    let value: serde_json::Value = read_json(path)?;
    value
        .get("format")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| untrusted_record(path))
}

fn recover_project_publication(
    project: &Project,
    recovery_path: &Utf8Path,
) -> Result<MutationRecovery> {
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
    let disposition = if saw_original && saw_candidate {
        original.restore_if_unchanged(&project.root, &live)?;
        RecoveryDisposition::Restored
    } else if saw_candidate {
        RecoveryDisposition::Accepted
    } else {
        RecoveryDisposition::CleanupOnly
    };
    cleanup_linked_publication_files(recovery_path)?;
    let mut recovery = MutationRecovery::settled(disposition);
    match remove_transaction_marker(recovery_path)? {
        CommitOutcome::Committed => {}
        CommitOutcome::DurabilityUncertain(error) => {
            recovery.warnings.push(Diagnostic::new(
                DiagnosticKind::Filesystem,
                format!(
                    "project recovery settled the visible files, but marker-removal durability is uncertain at {recovery_path}: {error}"
                ),
            ));
        }
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
    Ok(open_recovery_artifact(path)?.is_some())
}

fn open_recovery_artifact(path: &Utf8Path) -> Result<Option<File>> {
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
    Ok(Some(file))
}

fn publish_exclusive_json<T: serde::Serialize>(
    path: &Utf8Path,
    value: &T,
) -> Result<PublicationOutcome, PublicationError> {
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
) -> Result<PublicationOutcome, PublicationError>
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
    let file = open_recovery_artifact(path)?.ok_or_else(|| {
        CoreError::LockUnreadable(format!(
            "Cargo.lock recovery artifact disappeared at {path}; left all files untouched"
        ))
    })?;
    serde_json::from_reader(file).map_err(|error| {
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
        let project = project(&root);
        let lock_path = root.join("Cargo.lock");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"original").expect("write lock");
        (dir, project, lock_path)
    }

    fn accepted_project_state(
        project: &Project,
        paths: &[(&str, &str)],
    ) -> eyre::Result<AcceptedProjectState> {
        let candidate = tempfile::tempdir()?;
        let candidate_root = Utf8PathBuf::from_path_buf(candidate.path().to_owned())
            .map_err(|path| eyre::eyre!("non-UTF-8 candidate path: {path:?}"))?;
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
        let original = ProjectMutationJournal::new(files)?;
        let candidate = original.capture_state(&candidate_root)?;
        Ok(AcceptedProjectState::new(
            original,
            candidate,
            cooldown_core::ProjectInputSnapshot::default(),
        )?)
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
        publish_exclusive_json(&marker, &record)
            .map_err(|error| eyre::eyre!("publish legacy marker: {error:?}"))?;
        std::fs::write(lock_path, current)?;
        Ok((marker, state_path))
    }

    #[test]
    fn accepted_project_state_publishes_once_and_consumes_its_record() -> eyre::Result<()> {
        let (_directory, project, lock_path) = setup();
        let accepted = accepted_project_state(&project, &[("Cargo.lock", "accepted")])?;

        let publication = publish_accepted(&project, &accepted)?;

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

        let error = publish_accepted(&project, &accepted)
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
        publish_exclusive_json(&marker, &record)
            .map_err(|error| eyre::eyre!("publish marker: {error:?}"))?;
        std::fs::write(&lock_path, "accepted lock")?;

        std::assert_matches!(
            recover_pending(&project)?.disposition,
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

        let error = publish_accepted_with(&project, &accepted, |_accepted, root| {
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
            recover_pending(&project)?.disposition,
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
            recover_pending(&project)?.disposition,
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
        publish_exclusive_json(&marker, &record)
            .map_err(|error| eyre::eyre!("publish marker: {error:?}"))?;
        accepted.install(&project.root)?;

        std::assert_matches!(
            recover_pending(&project)?.disposition,
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
        publish_exclusive_json(&marker, &record)
            .map_err(|error| eyre::eyre!("publish marker: {error:?}"))?;
        accepted.install(&project.root)?;
        std::fs::write(&manifest_path, "external manifest")?;

        std::assert_matches!(recover_pending(&project), Err(CoreError::LockUnreadable(_)));
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
            recover_pending(&project)?.disposition,
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

        let error = recover_pending(&project)
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
            return Err(eyre::eyre!("publication did not retain its private name"));
        };

        for path in [&marker, &path] {
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

        let error = recover_pending(&project).expect_err("reject marker");

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

        let error = recover_pending(&project)
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
        std::assert_matches!(recover_pending(&project), Err(CoreError::LockUnreadable(_)));
        assert_eq!(std::fs::read_to_string(lock_path)?, "original");
        assert!(std::fs::symlink_metadata(marker)?.file_type().is_symlink());
        Ok(())
    }
}
