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
    pub(crate) fn capture(result: Result<()>, journal: &ProjectMutationJournal) -> Result<Self> {
        Ok(OwnedStep {
            result,
            postimage: journal.capture_state()?,
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
    postimage: &ProjectMutationState,
) -> Result<()> {
    journal.restore_if_unchanged(postimage)
}

/// Chooses the lock-only driver command for one change, when the package manager supports one.
///
/// In `Auto` mode, an in-range target should move only the lock and preserve the author's range.
/// The range check happens before the command because a native lock-only pin need not validate that
/// the requested version still satisfies `package.json`.
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

/// The transaction that lands one candidate after cooldown captures its authorized manifest edits.
pub(crate) enum CandidateLanding {
    /// The command preserves manifests itself.
    ///
    /// The snapshot re-establishes the authorized state if a cutoff fallback first restores the
    /// candidate baseline.
    Direct {
        command: Vec<String>,
        authorized_manifests: ProjectMutationJournal,
    },
    /// The exact pin may save another range.
    ///
    /// Authorized bytes are restored and the lock is resynchronized before judging the result.
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
/// Eligibility depends on declarations rather than the files widening happened to modify.
/// A published-contract-only declaration deliberately has an empty write set but can still
/// authorize an exact lock move.
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
    // Without an exact preserving pin, shifting even an in-range declaration is the only way to
    // steer a bare relock to the planned target.
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
        // npm's resync can select the old lock again when every install range remains compatible.
        // Shift those ranges only when no declaration needed widening.
        // If one did, its edit already steers the resync and every compatible sibling stays intact.
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

/// Returns the manager's exact pin and any restore/resync transaction needed to keep authorized
/// manifest bytes authoritative.
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

/// Captures only manifests a change can touch.
///
/// The lock stays outside this snapshot so a pin can restore authorized manifests and resynchronize
/// the candidate lock.
fn manifest_snapshot(project: &Project, change: &Change) -> Result<ProjectMutationJournal> {
    ProjectMutationJournal::capture(&project.root, manifest::manifest_rels(&change.members))
}

/// Reports whether the target satisfies every range declared by its possible owning manifests.
///
/// An absent declaration returns `false`, making the caller rewrite rather than risk an inconsistent
/// lock.
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

/// Returns the root and workspace-member manifests that may declare this dependency.
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
    let first = OwnedStep::capture(first_result, candidate_journal)?;
    let (attempt, retried_without_cutoff) = match first.result {
        Ok(()) => (first, false),
        Err(error) => {
            let fallback = matches!(&error, CoreError::Tool { .. })
                .then(|| without_before(landing.command()))
                .flatten();
            if let Some(fallback) = fallback {
                // A baselined post-cutoff package can make npm's historical-tree resolve impossible.
                // Restore the baseline and reapply authorized manifests before retrying without it.
                restore_after_owned_step(candidate_journal, &first.postimage)?;
                landing
                    .authorized_manifests()
                    .restore_if_unchanged(&authorized_baseline)?;
                let fallback_result = command.run_candidate(&project.root, &fallback).await;
                (
                    OwnedStep::capture(fallback_result, candidate_journal)?,
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
            restore_after_owned_step(authorized_manifests, &authorized_postimage)?;
            let resync = if retried_without_cutoff {
                without_before(resync).unwrap_or_else(|| resync.clone())
            } else {
                resync.clone()
            };
            let result = command.run_candidate(&project.root, &resync).await;
            OwnedStep::capture(result, candidate_journal)
        }
        _ => Ok(attempt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::Npm;
    use crate::registry::NPM;
    use color_eyre::eyre;
    use cooldown_core::{MemberRef, PackageId, UpdateKind, Version};
    use std::sync::Mutex;

    fn change(name: &str, from: &str, to: &str) -> Change {
        Change {
            package: PackageId::new(Npm::ID, name, Some(NPM.to_string())),
            from: Version::new(from),
            to: Version::new(to),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        }
    }

    #[test]
    fn candidate_restore_refuses_drift_after_the_owned_command() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let relative = Utf8Path::new("package-lock.json");
        std::fs::write(root.join(relative), "original")?;
        let journal = ProjectMutationJournal::capture(root, [relative])?;
        std::fs::write(root.join(relative), "candidate")?;
        let postimage = journal.capture_state()?;
        std::fs::write(root.join(relative), "independent")?;

        let result = restore_after_owned_step(&journal, &postimage);

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        assert_eq!(std::fs::read_to_string(root.join(relative))?, "independent");
        Ok(())
    }

    struct CutoffFallbackCommand {
        authorized_manifest: String,
        pin_saves_manifest: bool,
        calls: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl CandidateCommand for CutoffFallbackCommand {
        async fn run_candidate(&self, root: &Utf8Path, args: &[String]) -> Result<()> {
            let call_index = {
                let mut calls = self.calls.lock().map_err(|_| {
                    CoreError::Filesystem("candidate command call log was poisoned".to_string())
                })?;
                let index = calls.len();
                calls.push(args.to_vec());
                index
            };
            let manifest = root.join("package.json");
            match call_index {
                0 => {
                    std::fs::write(&manifest, r#"{"range":"first-attempt-save"}"#)?;
                    Err(CoreError::Tool {
                        tool: "npm".to_string(),
                        termination: cooldown_core::ToolTermination::ExitCode(1),
                        stderr: "historical tree unavailable".to_string(),
                    })
                }
                1 => {
                    let live = std::fs::read_to_string(&manifest)?;
                    if live != self.authorized_manifest {
                        return Err(CoreError::LockConflict(format!(
                            "fallback saw `{live}` instead of the authorized manifest"
                        )));
                    }
                    if self.pin_saves_manifest {
                        std::fs::write(&manifest, r#"{"range":"fallback-pin-save"}"#)?;
                    }
                    Ok(())
                }
                2 => {
                    let live = std::fs::read_to_string(&manifest)?;
                    if live != self.authorized_manifest {
                        return Err(CoreError::LockConflict(format!(
                            "resync saw `{live}` instead of the authorized manifest"
                        )));
                    }
                    Ok(())
                }
                _ => Err(CoreError::LockConflict(
                    "candidate landing ran more commands than expected".to_string(),
                )),
            }
        }
    }

    fn retry_journals(
        root: &Utf8Path,
        authorized_manifest: &str,
    ) -> eyre::Result<(ProjectMutationJournal, ProjectMutationJournal)> {
        let relative = Utf8Path::new("package.json");
        std::fs::write(root.join(relative), r#"{"range":"baseline"}"#)?;
        let candidate = ProjectMutationJournal::capture(root, [relative])?;
        std::fs::write(root.join(relative), authorized_manifest)?;
        let authorized = ProjectMutationJournal::capture(root, [relative])?;
        Ok((candidate, authorized))
    }

    fn fallback_project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            manifest: root.join("package.json"),
            kind: Npm::ID,
            exclude_newer: None,
        }
    }

    #[tokio::test]
    async fn direct_cutoff_fallback_reapplies_the_authorized_manifest() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let authorized_manifest = r#"{"range":"authorized"}"#;
        let (candidate_journal, authorized_manifests) = retry_journals(root, authorized_manifest)?;
        let landing = CandidateLanding::Direct {
            command: vec!["install".to_string(), "--before=2026-01-01".to_string()],
            authorized_manifests,
        };
        let command = CutoffFallbackCommand {
            authorized_manifest: authorized_manifest.to_string(),
            pin_saves_manifest: false,
            calls: Mutex::new(Vec::new()),
        };

        let attempt = run_candidate_landing_with(
            &command,
            &fallback_project(root),
            &candidate_journal,
            &landing,
        )
        .await?;
        attempt.result?;

        assert_eq!(
            std::fs::read_to_string(root.join("package.json"))?,
            authorized_manifest
        );
        let calls = command
            .calls
            .lock()
            .map_err(|_| eyre::eyre!("candidate command call log was poisoned"))?;
        assert_eq!(calls.len(), 2);
        let fallback = calls
            .get(1)
            .ok_or_else(|| eyre::eyre!("fallback command was not recorded"))?;
        assert!(fallback.iter().all(|arg| !arg.starts_with("--before=")));
        Ok(())
    }

    #[tokio::test]
    async fn pin_cutoff_fallback_restores_the_final_attempt_before_resync() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let authorized_manifest = r#"{"range":"authorized"}"#;
        let (candidate_journal, authorized_manifests) = retry_journals(root, authorized_manifest)?;
        let landing = CandidateLanding::PinRestoreResync {
            pin: vec!["install".to_string(), "--before=2026-01-01".to_string()],
            authorized_manifests,
            resync: vec!["install".to_string(), "--before=2026-01-01".to_string()],
        };
        let command = CutoffFallbackCommand {
            authorized_manifest: authorized_manifest.to_string(),
            pin_saves_manifest: true,
            calls: Mutex::new(Vec::new()),
        };

        let attempt = run_candidate_landing_with(
            &command,
            &fallback_project(root),
            &candidate_journal,
            &landing,
        )
        .await?;
        attempt.result?;

        assert_eq!(
            std::fs::read_to_string(root.join("package.json"))?,
            authorized_manifest
        );
        let calls = command
            .calls
            .lock()
            .map_err(|_| eyre::eyre!("candidate command call log was poisoned"))?;
        assert_eq!(calls.len(), 3);
        let fallback_and_resync = calls
            .get(1..)
            .ok_or_else(|| eyre::eyre!("fallback commands were not recorded"))?;
        assert!(
            fallback_and_resync
                .iter()
                .flatten()
                .all(|arg| !arg.starts_with("--before="))
        );
        Ok(())
    }

    #[test]
    fn window_minutes_from_cutoff_handles_spans_and_absolute_instants() -> eyre::Result<()> {
        let now: jiff::Timestamp = "2024-08-15T00:00:00Z".parse()?;
        // The application renders an age window as a relative span; each maps directly to minutes.
        assert_eq!(
            window_minutes_from_cutoff(Some("14 days"), now),
            Some(14 * 24 * 60)
        );
        assert_eq!(
            window_minutes_from_cutoff(Some("1 day"), now),
            Some(24 * 60)
        );
        assert_eq!(
            window_minutes_from_cutoff(Some("36 hours"), now),
            Some(36 * 60)
        );
        assert_eq!(window_minutes_from_cutoff(Some("1 hour"), now), Some(60));
        // Sub-minute ages round up so the cooldown is never silently disabled.
        assert_eq!(window_minutes_from_cutoff(Some("90 seconds"), now), Some(2));
        assert_eq!(window_minutes_from_cutoff(Some("30 seconds"), now), Some(1));
        // An absolute freeze instant converts to `now - instant` minutes (14 days here).
        assert_eq!(
            window_minutes_from_cutoff(Some("2024-08-01T00:00:00Z"), now),
            Some(14 * 24 * 60)
        );
        // A future instant (or no cutoff) excludes nothing → None.
        assert_eq!(
            window_minutes_from_cutoff(Some("2024-09-01T00:00:00Z"), now),
            None
        );
        assert_eq!(window_minutes_from_cutoff(None, now), None);
        Ok(())
    }

    #[test]
    fn absolute_cutoff_from_project_realizes_relative_windows_for_npm() -> eyre::Result<()> {
        let now: jiff::Timestamp = "2024-08-15T12:34:56Z".parse()?;

        assert_eq!(
            absolute_cutoff_from_project(Some("14 days"), now).as_deref(),
            Some("2024-08-01T12:34:56Z")
        );
        assert_eq!(
            absolute_cutoff_from_project(Some("90 seconds"), now).as_deref(),
            Some("2024-08-15T12:33:26Z")
        );
        assert_eq!(
            absolute_cutoff_from_project(Some("2024-07-01T00:00:00Z"), now).as_deref(),
            Some("2024-07-01T00:00:00Z")
        );
        assert_eq!(
            absolute_cutoff_from_project(Some("0 days"), now).as_deref(),
            Some("2024-08-15T12:34:56Z")
        );
        assert_eq!(absolute_cutoff_from_project(None, now), None);
        Ok(())
    }

    #[test]
    fn cutoff_fallback_removes_only_the_before_argument() {
        let args = vec![
            "install".to_string(),
            "eslint@10.6.0".to_string(),
            "--before=2026-06-30T00:00:00Z".to_string(),
            "--package-lock-only".to_string(),
        ];

        assert_eq!(
            without_before(&args),
            Some(vec![
                "install".to_string(),
                "eslint@10.6.0".to_string(),
                "--package-lock-only".to_string(),
            ])
        );
        assert_eq!(without_before(&["install".to_string()]), None);
    }

    /// A [`Project`] rooted in a temporary directory whose `package.json` declares `nanoid`.
    struct DeclaredProject {
        /// Owns the temporary root directory; dropping it deletes the project on disk, so it must
        /// stay bound for as long as the project is used.
        guard: tempfile::TempDir,
        /// The project whose root and manifest live inside the temporary directory.
        project: Project,
    }

    fn project_declaring(spec: &str) -> eyre::Result<DeclaredProject> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            root.join("package.json"),
            format!(r#"{{ "dependencies": {{ "nanoid": "{spec}" }} }}"#),
        )?;
        let project = Project {
            root: root.clone(),
            kind: crate::lock::Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        Ok(DeclaredProject {
            guard: dir,
            project,
        })
    }

    #[test]
    fn pnpm_uses_lock_only_only_for_in_range_auto() -> eyre::Result<()> {
        let DeclaredProject {
            guard: _guard,
            project,
        } = project_declaring("^3.0.0")?;

        // In-range minor under Auto → lock-only `pnpm update --no-save` (the declared range stands).
        let in_range = change("nanoid", "3.1.0", "3.3.0");
        let args = lockonly_command::<crate::lock::Pnpm>(&project, &in_range, RewriteMode::Auto)?;
        assert_eq!(
            args,
            Some(vec![
                "update".to_string(),
                "nanoid@3.3.0".to_string(),
                "--lockfile-only".to_string(),
                "--no-save".to_string()
            ])
        );

        // Out-of-range and `--rewrite` both take the manifest-rewrite + relock path.
        let major = change("nanoid", "3.1.0", "5.0.0");
        assert!(
            lockonly_command::<crate::lock::Pnpm>(&project, &major, RewriteMode::Auto)?.is_none()
        );
        assert!(
            lockonly_command::<crate::lock::Pnpm>(&project, &in_range, RewriteMode::Always)?
                .is_none()
        );
        assert_eq!(
            crate::lock::Pnpm::relock_args(None),
            ["install", "--lockfile-only"]
        );
        Ok(())
    }

    #[test]
    fn relock_commands_refresh_locks_without_adding_dependencies() {
        assert_eq!(
            crate::lock::Npm::relock_args(None),
            ["install", "--package-lock-only", "--no-audit", "--no-fund"]
        );
        assert_eq!(
            crate::lock::Pnpm::relock_args(None),
            ["install", "--lockfile-only"]
        );
        assert_eq!(crate::lock::Yarn::relock_args(None), ["install"]);
        assert_eq!(crate::lock::Bun::relock_args(None), ["install"]);
    }

    #[test]
    fn npm_install_commands_apply_the_absolute_before_cutoff() -> eyre::Result<()> {
        let before = Some("2024-08-01T00:00:00Z");

        assert_eq!(
            Npm::relock_args(before),
            [
                "install",
                "--package-lock-only",
                "--no-audit",
                "--no-fund",
                "--before=2024-08-01T00:00:00Z"
            ]
        );
        assert_eq!(
            Npm::pinned_relock_args("eslint", "10.6.0", before)
                .ok_or_else(|| eyre::eyre!("npm did not provide an exact pin command"))?,
            [
                "install",
                "eslint@10.6.0",
                "--package-lock-only",
                "--no-audit",
                "--no-fund",
                "--before=2024-08-01T00:00:00Z"
            ]
        );
        assert_eq!(
            Npm::build_args(before),
            [
                "install",
                "--no-audit",
                "--no-fund",
                "--before=2024-08-01T00:00:00Z"
            ]
        );
        Ok(())
    }

    /// npm's save-capable exact pin is bracketed around the manifest bytes cooldown authorized,
    /// including when cooldown widened the range before taking the snapshot.
    #[test]
    fn npm_restores_the_authorized_widened_range_after_its_exact_pin() -> eyre::Result<()> {
        let DeclaredProject {
            guard: _guard,
            mut project,
        } = project_declaring("^3.0.0")?;
        project.exclude_newer = Some("2024-08-01T00:00:00Z".to_string());
        let change = change("nanoid", "3.1.0", "5.1.11");

        let landing = candidate_landing::<Npm>(&project, &change, RewriteMode::Auto)?
            .ok_or_else(|| eyre::eyre!("declared candidate had no landing transaction"))?;
        let CandidateLanding::PinRestoreResync {
            pin,
            authorized_manifests,
            resync,
        } = landing
        else {
            eyre::bail!("npm's save-capable pin was not bracketed")
        };
        assert_eq!(
            pin,
            [
                "install",
                "nanoid@5.1.11",
                "--package-lock-only",
                "--no-audit",
                "--no-fund",
                "--before=2024-08-01T00:00:00Z"
            ]
        );
        assert_eq!(
            resync,
            [
                "install",
                "--package-lock-only",
                "--no-audit",
                "--no-fund",
                "--before=2024-08-01T00:00:00Z"
            ]
        );
        assert!(
            std::fs::read_to_string(&project.manifest)?.contains(r#""nanoid": "^5.1.11""#),
            "cooldown applies the authorized range before npm runs"
        );

        std::fs::write(
            &project.manifest,
            r#"{ "dependencies": { "nanoid": "^5.1.11-overwritten" } }"#,
        )?;
        authorized_manifests.restore()?;
        assert_eq!(
            std::fs::read_to_string(&project.manifest)?,
            r#"{ "dependencies": { "nanoid": "^5.1.11" } }"#,
            "the snapshot restores cooldown's range, not the pre-widen or npm-authored bytes"
        );
        Ok(())
    }

    #[test]
    fn npm_authorizes_a_target_steering_member_edit_when_every_range_is_compatible()
    -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::create_dir_all(root.join("apps/app"))?;
        std::fs::write(
            root.join("package.json"),
            r#"{ "peerDependencies": { "chalk": ">=5.6.0 <5.7.0" } }"#,
        )?;
        std::fs::write(
            root.join("apps/app/package.json"),
            r#"{ "devDependencies": { "chalk": "^5.6.0" } }"#,
        )?;
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let mut change = change("chalk", "5.6.0", "5.6.2");
        change.members = vec![
            MemberRef {
                name: "root".into(),
                path: ".".into(),
            },
            MemberRef {
                name: "app".into(),
                path: "apps/app".into(),
            },
        ];

        let landing = candidate_landing::<Npm>(&project, &change, RewriteMode::Auto)?
            .ok_or_else(|| eyre::eyre!("declared candidate had no landing transaction"))?;

        let CandidateLanding::PinRestoreResync { pin, resync, .. } = landing else {
            eyre::bail!("npm's member-owned exact pin was not bracketed")
        };
        assert_eq!(
            pin,
            [
                "install",
                "chalk@5.6.2",
                "--package-lock-only",
                "--no-audit",
                "--no-fund",
                "--workspace=apps/app"
            ]
        );
        assert_eq!(
            resync,
            ["install", "--package-lock-only", "--no-audit", "--no-fund"]
        );
        assert!(
            std::fs::read_to_string(root.join("apps/app/package.json"))?
                .contains(r#""chalk": "^5.6.2""#),
            "the member edit keeps npm's restored lock on the exact target"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("package.json"))?,
            r#"{ "peerDependencies": { "chalk": ">=5.6.0 <5.7.0" } }"#,
            "the root's published contract remains byte-identical"
        );
        Ok(())
    }

    /// A declaration nothing may rewrite (a published `peerDependencies` contract) leaves an empty
    /// widen write set, and a plain relock cannot move a lock that still satisfies its range — so
    /// the landing is an EXACT manifest-preserving pin.
    /// It must be target-directed, not a
    /// "newest the range admits" update: under a broad `>=5 <7` range, a deliberate patch move
    /// (`--major` off) would overshoot the plan and be rejected, and a `fix` rollback could not move
    /// downward at all.
    #[test]
    fn npm_pins_a_peer_only_declaration_exactly_and_restores_the_manifest() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            root.join("package.json"),
            r#"{ "peerDependencies": { "chalk": ">=5.6.0 <5.7.0" } }"#,
        )?;
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let change = change("chalk", "5.6.0", "5.6.2");
        let declarations =
            manifest::declarations(&project.root, &change.members, &change.package.name)?;
        assert_eq!(
            (
                declarations.absent(),
                declarations.has_install(),
                declarations.peer
            ),
            (false, false, true),
            "a peer-only declaration is declared, but writable nowhere"
        );

        // The landing is the bracketed exact pin: the pin names the exact planned version, and the
        // plain resync recopies metadata from the restored manifest.
        let pin = preserving_pin::<Npm>(&project, &change, &[])
            .ok_or_else(|| eyre::eyre!("npm did not provide an exact pin"))?;
        match pin {
            crate::lock::PreservingPin::PinRestoreResync { pin, resync } => {
                assert_eq!(
                    pin,
                    [
                        "install",
                        "chalk@5.6.2",
                        "--package-lock-only",
                        "--no-audit",
                        "--no-fund"
                    ],
                    "the pin is target-directed, not a range-maximum update"
                );
                assert_eq!(
                    resync,
                    ["install", "--package-lock-only", "--no-audit", "--no-fund"],
                    "the resync recopies restored manifest metadata without retargeting the lock"
                );
            }
            crate::lock::PreservingPin::Direct(args) => {
                eyre::bail!("npm's exact pin unexpectedly preserved the range directly: {args:?}")
            }
        }

        // pnpm needs no bracketing: its exact pin already skips the manifest.
        std::assert_matches!(
            preserving_pin::<crate::lock::Pnpm>(&project, &change, &[]),
            Some(crate::lock::PreservingPin::Direct(_))
        );
        Ok(())
    }

    #[test]
    fn pnpm_lock_only_requires_all_declaring_manifests_to_accept_target() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::create_dir_all(root.join("apps/a"))?;
        std::fs::create_dir_all(root.join("apps/b"))?;
        std::fs::write(root.join("package.json"), r#"{ "name": "root" }"#)?;
        std::fs::write(
            root.join("apps/a/package.json"),
            r#"{ "dependencies": { "nanoid": "^3.0.0" } }"#,
        )?;
        std::fs::write(
            root.join("apps/b/package.json"),
            r#"{ "dependencies": { "nanoid": "^2.0.0" } }"#,
        )?;
        let project = Project {
            root: root.clone(),
            kind: crate::lock::Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let mut change = change("nanoid", "3.1.0", "3.3.0");
        change.members = vec![
            MemberRef {
                name: "a".into(),
                path: "apps/a".into(),
            },
            MemberRef {
                name: "b".into(),
                path: "apps/b".into(),
            },
        ];

        let args = lockonly_command::<crate::lock::Pnpm>(&project, &change, RewriteMode::Auto)?;

        assert!(args.is_none());
        Ok(())
    }

    #[test]
    fn npm_has_no_direct_lock_only_command() -> eyre::Result<()> {
        let DeclaredProject {
            guard: _guard,
            project,
        } = project_declaring("^3.0.0")?;
        let in_range = change("nanoid", "3.1.0", "3.3.0");
        assert!(lockonly_command::<Npm>(&project, &in_range, RewriteMode::Auto)?.is_none());
        Ok(())
    }
}
