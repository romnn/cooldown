//! Owns restart-visible and live state transitions for speculative `Cargo.lock` corrections.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::fs::DurableWriteError;
use cooldown_core::{CoreError, Project, Result};
use sha2::{Digest, Sha256};
use std::io::Write;

const RECOVERY_FORMAT: &str = "cooldown-cargo-lock-recovery-v2";
const RECOVERY_STATE_FORMAT: &str = "cooldown-cargo-lock-recovery-state-v1";
pub(crate) const RECOVERY_MARKER: &str = "Cargo.lock.cooldown-recovery";

/// A speculative lock correction whose recovery record remains authoritative until commit.
///
/// The transaction keeps the original bytes immutable on disk and rewrites only a small digest
/// record between isolation probes. Every transition verifies the live lock against its expected
/// bytes before replacing it or deleting recovery state.
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
            clean_orphan_artifacts(project, lock_path, None)?;
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

    /// Restores the run-start lock from any recognized live transaction state.
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
        clean_orphan_states(&self.lock_path, Some(&self.record.state_file))?;
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

/// Restores a validated interrupted transaction while the caller holds exclusive project access.
pub(crate) fn recover_pending(project: &Project) -> Result<bool> {
    let lock_path = project.root.join("Cargo.lock");
    let recovery_path = recovery_path(&lock_path);
    if !path_exists(&recovery_path)? {
        return Ok(clean_orphan_artifacts(project, &lock_path, None)? > 0);
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
    clean_orphan_artifacts(project, &lock_path, None)?;
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

fn clean_orphan_artifacts(
    project: &Project,
    lock_path: &Utf8Path,
    referenced: Option<&str>,
) -> Result<usize> {
    let states = clean_orphan_states(lock_path, referenced)?;
    let publications = clean_orphan_publications(project, lock_path)?;
    Ok(states + publications)
}

fn clean_orphan_publications(project: &Project, lock_path: &Utf8Path) -> Result<usize> {
    let parent = lock_path.parent().unwrap_or_else(|| Utf8Path::new(""));
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(target) = publication_target(&name) else {
            continue;
        };
        let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let path = parent.join(&name);
        let valid = if target == RECOVERY_MARKER {
            read_json::<RecoveryRecord>(&path)
                .and_then(|record| record.validate(project, lock_path, &path))
                .is_ok()
        } else if validated_state_path(lock_path, target).is_ok() {
            read_json::<RecoveryState>(&path)
                .and_then(|state| state.validate(&path))
                .is_ok()
        } else {
            false
        };
        if valid {
            remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
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

fn clean_orphan_states(lock_path: &Utf8Path, referenced: Option<&str>) -> Result<usize> {
    let parent = lock_path.parent().unwrap_or_else(|| Utf8Path::new(""));
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if referenced == Some(name.as_str()) {
            continue;
        }
        let Ok(path) = validated_state_path(lock_path, &name) else {
            continue;
        };
        let Ok(contents) = std::fs::read(&path) else {
            continue;
        };
        let Ok(state) = serde_json::from_slice::<RecoveryState>(&contents) else {
            continue;
        };
        if state.validate(&path).is_err() {
            continue;
        }
        remove_file(&path)?;
        removed += 1;
    }
    Ok(removed)
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
    fn recovery_cleans_a_valid_unreferenced_state_file() {
        let (_dir, project, lock_path) = setup();
        let state_path = unique_state_path(&lock_path);
        let state = RecoveryState::new("original", "candidate").expect("build state");
        publish_exclusive_json(&state_path, &state).expect("publish orphan state");

        assert!(recover_pending(&project).expect("clean orphan state"));
        assert!(!state_path.exists());
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "original"
        );
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
    fn validated_orphan_publications_are_collected() -> color_eyre::Result<()> {
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

        assert_eq!(clean_orphan_publications(&project, &lock_path)?, 1);
        assert!(publication_temps(&lock_path)?.is_empty());
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
