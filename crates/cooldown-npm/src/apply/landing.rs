//! One-candidate manifest authorization, resolver execution, and cutoff fallback.

use crate::lock::NodeLock;
use crate::manifest;
use crate::nodecmd::NodeCmd;
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    Change, CoreError, Project, ProjectMutationJournal, ProjectMutationState, Result, RewriteMode,
};

/// The result and exact write-set state observed when an adapter-owned command returned.
pub(crate) struct OwnedStep {
    pub(crate) result: Result<()>,
    pub(crate) postimage: ProjectMutationState,
}

impl OwnedStep {
    pub(crate) fn capture(
        result: Result<()>,
        journal: &ProjectMutationJournal,
        root: &Utf8Path,
    ) -> Result<Self> {
        Ok(OwnedStep {
            result,
            postimage: journal.capture_state(root)?,
        })
    }
}

#[async_trait]
pub(crate) trait CandidateCommand {
    async fn run_candidate(&self, root: &Utf8Path, args: &[String]) -> Result<()>;
}

#[async_trait]
impl CandidateCommand for NodeCmd {
    async fn run_candidate(&self, root: &Utf8Path, args: &[String]) -> Result<()> {
        self.run(root, args).await
    }
}

/// Restores an adapter-owned step only while its exact postimage remains live.
pub(crate) fn restore_after_owned_step(
    journal: &ProjectMutationJournal,
    root: &Utf8Path,
    postimage: &ProjectMutationState,
) -> Result<()> {
    journal.restore_if_unchanged(root, postimage)
}

pub(crate) fn lockonly_command<L: NodeLock>(
    project: &Project,
    change: &Change,
    mode: RewriteMode,
) -> Result<Option<Vec<String>>> {
    let name = &change.package.name;
    let version = change.to.as_str();
    if mode == RewriteMode::Auto
        && let Some(lockonly) = L::lockonly_update_args(name, version)
        && target_in_declared_range(project, change)?
    {
        return Ok(Some(lockonly));
    }
    Ok(None)
}

/// The transaction that lands one candidate after its authorized manifest edits are captured.
pub(crate) enum CandidateLanding {
    /// The command preserves manifests itself.
    Direct {
        command: Vec<String>,
        authorized_manifests: ProjectMutationJournal,
    },
    /// The exact pin saves another range, so authorized bytes are restored before resynchronizing.
    PinRestoreResync {
        pin: Vec<String>,
        authorized_manifests: ProjectMutationJournal,
        resync: Vec<String>,
    },
}

impl CandidateLanding {
    fn command(&self) -> &[String] {
        match self {
            CandidateLanding::Direct { command, .. } => command,
            CandidateLanding::PinRestoreResync { pin, .. } => pin,
        }
    }

    fn authorized_manifests(&self) -> &ProjectMutationJournal {
        match self {
            CandidateLanding::Direct {
                authorized_manifests,
                ..
            }
            | CandidateLanding::PinRestoreResync {
                authorized_manifests,
                ..
            } => authorized_manifests,
        }
    }
}

/// Plans the manifest authorization and resolver command for one candidate.
///
/// `None` means no manifest declares the dependency, so adding a root dependency is not authorized.
///
/// # Errors
///
/// Returns a [`CoreError`] when declarations cannot be inspected or authorized edits fail.
pub(crate) fn candidate_landing<L: NodeLock>(
    project: &Project,
    change: &Change,
    mode: RewriteMode,
) -> Result<Option<CandidateLanding>> {
    if let Some(args) = lockonly_command::<L>(project, change, mode)? {
        return Ok(Some(CandidateLanding::Direct {
            command: args,
            authorized_manifests: manifest_snapshot(project, change)?,
        }));
    }
    let declarations =
        manifest::declarations(&project.root, &change.members, &change.package.name)?;
    if declarations.absent() {
        return Ok(None);
    }
    let preserving_pin = preserving_pin::<L>(project, change, declarations.install_workspaces());
    let manifest_mode = if mode == RewriteMode::Auto && preserving_pin.is_none() {
        RewriteMode::Always
    } else {
        mode
    };
    let rewrite = manifest::widen_constraints(
        &project.root,
        &change.members,
        &change.package.name,
        change.to.as_str(),
        manifest_mode,
    )?;
    if mode == RewriteMode::Auto
        && rewrite.modified.is_empty()
        && declarations.has_install()
        && matches!(
            &preserving_pin,
            Some(crate::lock::PreservingPin::PinRestoreResync { .. })
        )
    {
        // npm's resync can select the old lock when every install range remains compatible.
        // Shift the ranges only when no declaration already needed widening to steer that resync.
        manifest::widen_constraints(
            &project.root,
            &change.members,
            &change.package.name,
            change.to.as_str(),
            RewriteMode::Always,
        )?;
    }
    let authorized_manifests = manifest_snapshot(project, change)?;
    let landing = match preserving_pin {
        Some(crate::lock::PreservingPin::Direct(command)) => CandidateLanding::Direct {
            command,
            authorized_manifests,
        },
        Some(crate::lock::PreservingPin::PinRestoreResync { pin, resync }) => {
            CandidateLanding::PinRestoreResync {
                pin,
                authorized_manifests,
                resync,
            }
        }
        None => {
            let before = absolute_cutoff_from_project(
                project.exclude_newer.as_deref(),
                jiff::Timestamp::now(),
            );
            CandidateLanding::Direct {
                command: L::relock_args(before.as_deref()),
                authorized_manifests,
            }
        }
    };
    Ok(Some(landing))
}

pub(crate) fn preserving_pin<L: NodeLock>(
    project: &Project,
    change: &Change,
    workspaces: &[String],
) -> Option<crate::lock::PreservingPin> {
    let before =
        absolute_cutoff_from_project(project.exclude_newer.as_deref(), jiff::Timestamp::now());
    L::preserving_pin(
        &change.package.name,
        change.to.as_str(),
        before.as_deref(),
        workspaces,
    )
}

fn manifest_snapshot(project: &Project, change: &Change) -> Result<ProjectMutationJournal> {
    let mut files = Vec::new();
    for rel in manifest::manifest_rels(&change.members) {
        files.push(ProjectMutationJournal::capture_file(&project.root, &rel)?);
    }
    ProjectMutationJournal::new(files)
}

pub(crate) fn target_in_declared_range(project: &Project, change: &Change) -> Result<bool> {
    let mut found = false;
    for manifest in candidate_manifests(project, change) {
        if let Some(range) = manifest::declared_range(&manifest, &change.package.name)? {
            found = true;
            if !version::version_in_range(&range, change.to.as_str()) {
                return Ok(false);
            }
        }
    }
    Ok(found)
}

fn candidate_manifests(project: &Project, change: &Change) -> Vec<Utf8PathBuf> {
    manifest::manifest_rels(&change.members)
        .into_iter()
        .map(|rel| project.root.join(rel))
        .collect()
}

/// Converts a project cutoff into pnpm's rolling `minimumReleaseAge` minute count.
pub(crate) fn window_minutes_from_cutoff(
    cutoff: Option<&str>,
    now: jiff::Timestamp,
) -> Option<i64> {
    let cutoff = cutoff?.trim();
    if let Some((count, unit)) = cutoff.split_once(' ')
        && let Ok(count) = count.parse::<i64>()
    {
        let minutes = match unit.trim_end_matches('s') {
            "day" => count.checked_mul(24 * 60)?,
            "hour" => count.checked_mul(60)?,
            "minute" => count,
            "second" => count.checked_add(59)? / 60,
            _ => return None,
        };
        return (minutes > 0).then_some(minutes);
    }
    let instant: jiff::Timestamp = cutoff.parse().ok()?;
    let minutes = now.duration_since(instant).as_secs() / 60;
    (minutes > 0).then_some(minutes)
}

/// Converts a stable project cutoff into the absolute instant npm's `--before` option requires.
pub(crate) fn absolute_cutoff_from_project(
    cutoff: Option<&str>,
    now: jiff::Timestamp,
) -> Option<String> {
    let cutoff = cutoff?.trim();
    if let Ok(instant) = cutoff.parse::<jiff::Timestamp>() {
        return Some(instant.to_string());
    }
    let duration = cooldown_core::duration::parse_duration(cutoff).ok()?;
    now.checked_sub(duration)
        .ok()
        .map(|instant| instant.to_string())
}

/// Removes a command's `--before=` cutoff for one historical-tree fallback.
pub(crate) fn without_before(args: &[String]) -> Option<Vec<String>> {
    let filtered: Vec<String> = args
        .iter()
        .filter(|arg| !arg.starts_with("--before="))
        .cloned()
        .collect();
    (filtered.len() != args.len()).then_some(filtered)
}

pub(crate) async fn run_candidate_landing_with<C: CandidateCommand>(
    command: &C,
    project: &Project,
    candidate_journal: &ProjectMutationJournal,
    landing: &CandidateLanding,
) -> Result<OwnedStep> {
    let authorized_baseline = candidate_journal.state_for(landing.authorized_manifests())?;
    let first_result = command
        .run_candidate(&project.root, landing.command())
        .await;
    let first = OwnedStep::capture(first_result, candidate_journal, &project.root)?;
    let (attempt, retried_without_cutoff) = match first.result {
        Ok(()) => (first, false),
        Err(error) => {
            let fallback = matches!(&error, CoreError::Tool { .. })
                .then(|| without_before(landing.command()))
                .flatten();
            if let Some(fallback) = fallback {
                // A baselined post-cutoff package can make npm's historical-tree resolve impossible.
                // Restore the baseline and reapply authorized manifests before retrying without it.
                restore_after_owned_step(candidate_journal, &project.root, &first.postimage)?;
                landing
                    .authorized_manifests()
                    .restore_if_unchanged(&project.root, &authorized_baseline)?;
                let fallback_result = command.run_candidate(&project.root, &fallback).await;
                (
                    OwnedStep::capture(fallback_result, candidate_journal, &project.root)?,
                    true,
                )
            } else {
                (
                    OwnedStep {
                        result: Err(error),
                        postimage: first.postimage,
                    },
                    false,
                )
            }
        }
    };
    match (&attempt.result, landing) {
        (
            Ok(()),
            CandidateLanding::PinRestoreResync {
                authorized_manifests,
                resync,
                ..
            },
        ) => {
            let authorized_postimage = attempt.postimage.state_for(authorized_manifests)?;
            restore_after_owned_step(authorized_manifests, &project.root, &authorized_postimage)?;
            let resync = if retried_without_cutoff {
                without_before(resync).unwrap_or_else(|| resync.clone())
            } else {
                resync.clone()
            };
            let result = command.run_candidate(&project.root, &resync).await;
            OwnedStep::capture(result, candidate_journal, &project.root)
        }
        _ => Ok(attempt),
    }
}
