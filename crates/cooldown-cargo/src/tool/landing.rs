//! Warm starts of an apply from where an earlier apply of the run left the whole-graph resolve.
//!
//! The upgrade executor tries one project several times from the same restored baseline: the
//! initial trial, lower-target fallbacks that keep every accepted change, and a final replay of
//! the accepted set.
//! Each trial's whole-graph resolve would otherwise land the accepted changes again from scratch,
//! at the cost of hundreds of Cargo resolves in a large workspace.
//! A landing records where an earlier resolve ended, so a later apply from the same inputs whose
//! plan carries every change that landed starts there and works only on the rest.
//!
//! A landing is reused only when the state it records is one the later plan could have reached
//! itself:
//!
//! - The apply starts from byte-identical native inputs under the same environment.
//! - The rewrite mode is the same, and under `Always`, which widens every planned change up front,
//!   the later plan carries every change the earlier one attempted.
//! - Every change whose lock slot the landing moved is planned again, identically.
//! - The landed lock keeps the [acceptance rule](super::acceptance) against the start for the
//!   later plan.
//!
//! Everything after the resolve (the edge phase, the guards, the report) runs on the warm state
//! as it would on a cold one.

use super::acceptance::ResolveBaseline;
use crate::lockfile::CargoLock;
use crate::rejection_memo::MemoKey;
use camino::Utf8PathBuf;
use cooldown_core::{Change, GeneratedMembers, ProjectMutationJournal, RewriteMode};
use std::collections::{BTreeSet, VecDeque};
use std::sync::Mutex;

/// How many landings a run keeps; a fallback search alternates between few starting points.
const CAPACITY: usize = 8;

/// Where one whole-graph resolve ended.
struct Landing {
    start: MemoKey,
    rewrite: RewriteMode,
    attempted: Vec<Change>,
    /// The planned changes whose lock slot the resolve moved.
    touched: Vec<Change>,
    /// Every file of the write set whose contents the resolve changed, with those contents.
    files: Vec<(Utf8PathBuf, String)>,
    lock: CargoLock,
}

/// The files of a landing a later apply starts from.
pub(super) struct WarmStart {
    pub(super) files: Vec<(Utf8PathBuf, String)>,
    pub(super) touched: usize,
}

/// The landings of the current run, newest last.
#[derive(Default)]
pub(super) struct LandingMemo {
    landings: Mutex<VecDeque<Landing>>,
}

/// The state a finished resolve left, recorded by [`LandingMemo::remember`].
pub(super) struct Finished<'a> {
    pub(super) start: MemoKey,
    pub(super) rewrite: RewriteMode,
    pub(super) plan: &'a [Change],
    pub(super) start_lock: &'a CargoLock,
    pub(super) lock: CargoLock,
    pub(super) files: Vec<(Utf8PathBuf, String)>,
}

impl LandingMemo {
    /// The landing with the most moved changes that `plan` may start from, if any.
    pub(super) fn recall(
        &self,
        start: &MemoKey,
        start_lock: &CargoLock,
        rewrite: RewriteMode,
        plan: &[Change],
        journal: &ProjectMutationJournal,
        generated: &GeneratedMembers,
    ) -> Option<WarmStart> {
        let baseline = ResolveBaseline::capture(start_lock, generated);
        let landings = self
            .landings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        landings
            .iter()
            .filter(|landing| {
                landing.start == *start
                    && landing.rewrite == rewrite
                    && !landing.touched.is_empty()
                    && (!matches!(rewrite, RewriteMode::Always)
                        || landing.attempted.iter().all(|change| plan.contains(change)))
                    && landing.touched.iter().all(|change| plan.contains(change))
                    && landing
                        .files
                        .iter()
                        .all(|(path, _)| journal.files().iter().any(|file| file.path() == path))
                    && baseline.violation(&landing.lock, generated, plan).is_none()
            })
            .max_by_key(|landing| landing.touched.len())
            .map(|landing| WarmStart {
                files: landing.files.clone(),
                touched: landing.touched.len(),
            })
    }

    /// Records where a resolve that started from `start` ended.
    pub(super) fn remember(&self, finished: Finished<'_>) {
        let touched: Vec<Change> = finished
            .plan
            .iter()
            .filter(|change| moved(finished.start_lock, &finished.lock, change))
            .cloned()
            .collect();
        if touched.is_empty() {
            return;
        }
        let mut landings = self
            .landings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        landings.retain(|landing| {
            !(landing.start == finished.start
                && landing.rewrite == finished.rewrite
                && landing.files == finished.files)
        });
        if landings.len() == CAPACITY {
            landings.pop_front();
        }
        landings.push_back(Landing {
            start: finished.start,
            rewrite: finished.rewrite,
            attempted: finished.plan.to_vec(),
            touched,
            files: finished.files,
            lock: finished.lock,
        });
    }
}

/// Whether the resolve moved any locked copy of the crate `change` plans, from any source.
///
/// Comparing every copy rather than the planned slot keeps a landing from carrying a move of the
/// crate the later plan does not make, whichever registry or line it happened in.
fn moved(start: &CargoLock, after: &CargoLock, change: &Change) -> bool {
    let copies = |lock: &CargoLock| -> BTreeSet<(Option<String>, Option<String>)> {
        lock.package
            .iter()
            .filter(|package| package.name == change.package.name)
            .map(|package| (package.source.clone(), package.version.clone()))
            .collect()
    };
    copies(start) != copies(after)
}
