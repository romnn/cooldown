//! Owns durable and live state transitions for speculative `Cargo.lock` corrections.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, Project, Result};
use sha2::{Digest, Sha256};
use std::io::Write;

const RECOVERY_FORMAT: &str = "cooldown-cargo-lock-recovery-v2";
const RECOVERY_STATE_FORMAT: &str = "cooldown-cargo-lock-recovery-state-v1";

/// A speculative lock correction whose durable record remains authoritative until commit.
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
            cooldown_core::fs::atomic_write,
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
        write_exclusive_json(&state_path, &state)?;
        if let Err(error) = write_exclusive_json(&recovery_path, &record) {
            let _ = remove_file(&state_path);
            return Err(error);
        }

        let mut transaction = SpeculativeLockTransaction {
            lock_path: lock_path.to_owned(),
            recovery_path,
            state_path,
            original_lock: original_lock.to_string(),
            current_lock: original_lock.to_string(),
            staged_lock: Some(candidate_lock.to_string()),
            record,
            state,
        };
        ensure_lock_equals(
            &transaction.lock_path,
            original_lock,
            "installing the first speculative correction",
            Some(&transaction.recovery_path),
        )?;
        if let Err(error) = install(lock_path.as_std_path(), candidate_lock.as_bytes()) {
            if lock_equals(lock_path, original_lock)? {
                transaction.staged_lock = None;
                transaction.remove_record()?;
            }
            return Err(error);
        }
        Ok(transaction)
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
        cooldown_core::fs::atomic_write(self.state_path.as_std_path(), &contents)?;
        self.state = state;
        ensure_lock_equals(
            &self.lock_path,
            &self.current_lock,
            "installing a speculative correction",
            Some(&self.recovery_path),
        )?;
        self.staged_lock = Some(candidate_lock.to_string());
        if let Err(error) =
            cooldown_core::fs::atomic_write(self.lock_path.as_std_path(), candidate_lock.as_bytes())
        {
            if lock_equals(&self.lock_path, &self.current_lock)? {
                self.staged_lock = None;
            }
            return Err(error);
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
        cooldown_core::fs::atomic_write(
            self.lock_path.as_std_path(),
            self.current_lock.as_bytes(),
        )?;
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
    pub(super) fn commit(&mut self) -> Result<()> {
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
    pub(super) fn rollback(&mut self) -> Result<()> {
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
            cooldown_core::fs::atomic_write(
                self.lock_path.as_std_path(),
                self.original_lock.as_bytes(),
            )?;
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

    fn remove_record(&self) -> Result<()> {
        if read_json::<RecoveryRecord>(&self.recovery_path)? != self.record {
            return Err(untrusted_record(&self.recovery_path));
        }
        if read_json::<RecoveryState>(&self.state_path)? != self.state {
            return Err(untrusted_record(&self.state_path));
        }
        remove_file(&self.recovery_path)?;
        if let Err(error) = remove_file(&self.state_path) {
            tracing::warn!(
                path = %self.state_path,
                error = %error,
                "could not remove completed Cargo.lock recovery state"
            );
        }
        Ok(())
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
        return Ok(false);
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
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), record.original_lock.as_bytes())?;
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
    lock_path.with_extension("lock.cooldown-recovery")
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

fn write_exclusive_json<T: serde::Serialize>(path: &Utf8Path, value: &T) -> Result<()> {
    let contents =
        serde_json::to_vec(value).map_err(|error| CoreError::Serialization(error.to_string()))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                CoreError::LockConflict(format!(
                    "pending Cargo.lock transaction state already exists at {path}"
                ))
            } else {
                error.into()
            }
        })?;
    if let Err(error) = file.write_all(&contents).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
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
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn untrusted_record(path: &Utf8Path) -> CoreError {
    CoreError::LockUnreadable(format!(
        "untrusted Cargo.lock recovery record at {path}; left all files untouched"
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
