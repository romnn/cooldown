//! Confining the writes of each Yarn Classic process a landing starts, with Linux Landlock.
//!
//! Reads, execution, and the network stay open, so Yarn reads the user's configuration and
//! credentials where they are; what is confined, to the places the landing names, is every
//! write Landlock ABI 3 (Linux 6.2) governs: writing or truncating a file's contents, and
//! creating, removing, renaming, or linking a directory entry.
//! Metadata, such as a file's permissions or owner, is no such write.
//! A configuration that sends such a write anywhere else, a cache folder, a modules folder, a
//! `cwd`, or an offline mirror, fails the install instead, so the landing needs to know no
//! setting.
//!
//! ABI 2 lets a file move between two writable directories, as Yarn's cache moves a download, and
//! ABI 3 adds truncation.
//! Elsewhere, and on a kernel without it, cooldown lands no Yarn Classic version.

use camino::Utf8Path;
use cooldown_core::{CoreError, Result};
use tokio::process::Command;

/// Why a landing holds a candidate where Yarn's writes cannot be confined.
const UNCONFINED: &str = "cooldown lands Yarn Classic versions only where it can confine Yarn's \
                          writes, which needs Linux Landlock";

/// Whether Yarn's writes can be confined here, or why not.
#[cfg(target_os = "linux")]
pub(in crate::yarn) fn available() -> std::result::Result<(), String> {
    linux::ruleset(&[])
        .map(drop)
        .map_err(|err| format!("{UNCONFINED} ABI 3 or later: {err}"))
}

/// Whether Yarn's writes can be confined here, or why not.
#[cfg(not(target_os = "linux"))]
pub(in crate::yarn) fn available() -> std::result::Result<(), String> {
    Err(UNCONFINED.to_string())
}

/// Confines the writes of the process `command` starts to the files below `writable`.
///
/// # Errors
///
/// Returns [`CoreError::System`] when the confinement cannot be prepared.
#[cfg(target_os = "linux")]
pub(super) fn confine(command: &mut Command, writable: &[&Utf8Path]) -> Result<()> {
    let ruleset = linux::ruleset(writable)
        .map_err(|err| CoreError::System(format!("{UNCONFINED}: {err}")))?;
    linux::restrict_before_exec(command, ruleset);
    Ok(())
}

/// Confines the writes of the process `command` starts to the files below `writable`.
///
/// # Errors
///
/// Returns [`CoreError::System`] always, since only Linux confines them.
#[cfg(not(target_os = "linux"))]
pub(super) fn confine(_command: &mut Command, _writable: &[&Utf8Path]) -> Result<()> {
    Err(CoreError::System(UNCONFINED.to_string()))
}

#[cfg(target_os = "linux")]
mod linux {
    use camino::Utf8Path;
    use landlock::{
        ABI, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreated,
        RulesetCreatedAttr, RulesetError, RulesetStatus, path_beneath_rules,
    };
    use tokio::process::Command;

    /// The ruleset that leaves a process writing only below `writable`, prepared in full, or why
    /// this kernel cannot enforce it.
    ///
    /// A path that cannot be opened grants nothing.
    pub(super) fn ruleset(writable: &[&Utf8Path]) -> Result<RulesetCreated, RulesetError> {
        let written = AccessFs::from_write(ABI::V3);
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(written)?
            .create()?
            .add_rules(path_beneath_rules(writable, written))
    }

    /// Enforces `ruleset` on the process `command` starts, between its fork and its exec.
    ///
    /// The ruleset is prepared in full beforehand, so the forked child only makes the two system
    /// calls that enforce it.
    #[expect(
        unsafe_code,
        reason = "Landlock confines a process only from inside it, and `pre_exec` is the one hook \
                  that runs inside the child before Yarn does"
    )]
    pub(super) fn restrict_before_exec(command: &mut Command, ruleset: RulesetCreated) {
        let mut ruleset = Some(ruleset);
        let restrict = move || {
            let ruleset = ruleset.take().ok_or(std::io::ErrorKind::PermissionDenied)?;
            match ruleset.restrict_self() {
                Ok(status) if status.ruleset == RulesetStatus::FullyEnforced => Ok(()),
                _ => Err(std::io::ErrorKind::PermissionDenied.into()),
            }
        };
        // SAFETY: the closure runs in the forked child, where only async-signal-safe work is
        // sound. It allocates nothing: `restrict_self` makes the `prctl` and
        // `landlock_restrict_self` system calls on a descriptor opened before the fork, and every
        // error it builds or returns is a bare error kind or OS error code.
        unsafe {
            command.pre_exec(restrict);
        }
    }
}
