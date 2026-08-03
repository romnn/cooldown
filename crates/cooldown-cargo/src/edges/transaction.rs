//! Owns reversible edge-correction probes inside a disposable Cargo project.

use camino::Utf8Path;
use cooldown_core::{CoreError, Project, Result};

/// A reversible lock correction whose entire lifetime stays inside a disposable project copy.
pub(super) struct SpeculativeLockTransaction {
    lock_path: camino::Utf8PathBuf,
    original_lock: String,
    current_lock: String,
    staged_lock: Option<String>,
}

impl SpeculativeLockTransaction {
    /// Starts with `candidate_lock` installed as the pending decision.
    pub(super) fn begin(
        _project: &Project,
        lock_path: &Utf8Path,
        original_lock: &str,
        candidate_lock: &str,
    ) -> Result<Self> {
        ensure_lock_equals(
            lock_path,
            original_lock,
            "starting an edge-correction probe",
        )?;
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), candidate_lock.as_bytes())?;
        ensure_lock_equals(
            lock_path,
            candidate_lock,
            "staging an edge-correction probe",
        )?;
        Ok(SpeculativeLockTransaction {
            lock_path: lock_path.to_owned(),
            original_lock: original_lock.to_string(),
            current_lock: original_lock.to_string(),
            staged_lock: Some(candidate_lock.to_string()),
        })
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
            "preparing an edge-correction probe",
        )?;
        cooldown_core::fs::atomic_write(self.lock_path.as_std_path(), candidate_lock.as_bytes())?;
        ensure_lock_equals(
            &self.lock_path,
            candidate_lock,
            "staging an edge-correction probe",
        )?;
        self.staged_lock = Some(candidate_lock.to_string());
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
            "accepting the verified edge correction",
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
            "rejecting an unverified edge correction",
        )?;
        cooldown_core::fs::atomic_write(
            self.lock_path.as_std_path(),
            self.current_lock.as_bytes(),
        )?;
        ensure_lock_equals(
            &self.lock_path,
            &self.current_lock,
            "restoring the prior edge-correction state",
        )?;
        self.staged_lock = None;
        Ok(())
    }

    /// Verifies that no pending decision remains in the disposable copy.
    pub(super) fn commit(&mut self) -> Result<()> {
        if self.staged_lock.is_some() {
            return Err(CoreError::System(
                "cannot commit a Cargo.lock transaction with an undecided candidate".to_string(),
            ));
        }
        ensure_lock_equals(
            &self.lock_path,
            &self.current_lock,
            "committing the verified edge correction",
        )?;
        Ok(())
    }

    /// Restores the transaction-start lock after checking the live candidate for drift.
    pub(super) fn rollback(&mut self) -> Result<()> {
        let expected = self
            .staged_lock
            .as_deref()
            .unwrap_or(self.current_lock.as_str());
        ensure_lock_equals(
            &self.lock_path,
            expected,
            "rolling back the edge-correction probe",
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
            "finishing edge-correction rollback",
        )?;
        self.current_lock.clone_from(&self.original_lock);
        self.staged_lock = None;
        Ok(())
    }
}

fn ensure_lock_equals(lock_path: &Utf8Path, expected: &str, transition: &str) -> Result<()> {
    if std::fs::read_to_string(lock_path)? == expected {
        return Ok(());
    }
    Err(CoreError::LockConflict(format!(
        "Cargo.lock changed independently while {transition}; left the disposable project untouched"
    )))
}
