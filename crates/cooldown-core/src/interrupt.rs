//! Cooperative interruption for a run that may be mutating project files in place.
//!
//! A signal must not kill cooldown between a mutation and its rollback: an in-place adapter edits
//! the live manifest, lock, and native config during resolver trials, and only the ordinary
//! error paths restore them.
//! So the binary turns the first signal into a [`request`], which tells every running child
//! process tree to stop.
//! Every later spawn refuses, and the steps that would commit a result (accepting a trial,
//! publishing a staged project, writing native config) check [`ensure_not_requested`] first.
//! The resulting [`interrupted`] error then travels through the same rollback paths as any other
//! local fault.

use crate::CoreError;
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);
static CHILDREN: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

/// Requests that the run stop, and asks every registered child process group to terminate.
///
/// Returns `true` for the first request and `false` once one is already pending, so the signal
/// handler can treat a repeated signal as a demand to quit immediately.
pub fn request() -> bool {
    let first = !REQUESTED.swap(true, Ordering::SeqCst);
    for pid in children().iter() {
        terminate(*pid);
    }
    first
}

/// Kills every child process group still registered, for one that ignored the termination
/// [`request`].
pub fn kill_remaining() {
    for pid in children().iter() {
        kill(*pid);
    }
}

/// Whether an interruption has been requested.
#[must_use]
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// The error reported for `activity` (such as "accepting the batch") once the run has been
/// interrupted.
///
/// It is a [`CoreError::System`], a local-environment failure, so resilient apply propagates it
/// instead of isolating a candidate, and the run restores its baseline.
#[must_use]
pub fn interrupted(activity: &str) -> CoreError {
    CoreError::System(format!("interrupted by a signal while {activity}"))
}

/// Refuses to begin `activity` once an interruption has been requested.
///
/// # Errors
///
/// Returns the [`interrupted`] error when an interruption is pending.
pub fn ensure_not_requested(activity: &str) -> Result<(), CoreError> {
    if requested() {
        return Err(interrupted(activity));
    }
    Ok(())
}

/// Registers a running child process so a later [`request`] can terminate it.
///
/// On Unix the child must lead its own process group (`process_group(0)`), because the signal
/// goes to the whole group: a descendant that outlives the child would otherwise keep its output
/// pipes open, and waiting for them would stall the rollback.
/// The registration lasts until the returned guard drops, which the caller does once the child has
/// been waited for.
/// A child registered after the request was made is terminated immediately, which closes the race
/// between spawning it and the signal arriving.
#[must_use = "dropping the guard unregisters the child at once"]
pub fn register_child(pid: u32) -> ChildRegistration {
    children().insert(pid);
    if requested() {
        terminate(pid);
    }
    ChildRegistration { pid }
}

/// Keeps a child process registered for termination until dropped.
#[derive(Debug)]
pub struct ChildRegistration {
    pid: u32,
}

impl Drop for ChildRegistration {
    fn drop(&mut self) {
        children().remove(&self.pid);
    }
}

fn children() -> std::sync::MutexGuard<'static, BTreeSet<u32>> {
    // The set holds plain process ids, so a panic while it was locked cannot leave it inconsistent.
    CHILDREN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(unix)]
fn terminate(pid: u32) {
    signal(pid, rustix::process::Signal::TERM);
}

#[cfg(unix)]
fn kill(pid: u32) {
    signal(pid, rustix::process::Signal::KILL);
}

#[cfg(unix)]
fn signal(pid: u32, signal: rustix::process::Signal) {
    let Some(group) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    // The group may already be gone; the request stands either way.
    let _ = rustix::process::kill_process_group(group, signal);
}

// A console interrupt already reaches every process attached to the console, children included.
#[cfg(not(unix))]
fn terminate(_pid: u32) {}

#[cfg(not(unix))]
fn kill(_pid: u32) {}
