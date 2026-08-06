//! Resilient application of a whole-graph plan.
//!
//! A tool that re-resolves the whole graph in one native command (pnpm, cargo, go, deno, uv) applies
//! its candidates **atomically**: if a single candidate is unsatisfiable — an unpublished/yanked
//! version the manager cannot fetch, one side of a mutually-exclusive conflict, or a candidate set
//! that produces a stale lock — the whole command fails and the adapter's `apply` returns an error.
//! Reporting that as "every candidate is held" is wrong: one bad candidate would block every other.
//!
//! [`apply_resilient_with_observer`] wraps `apply` so that, on such a failure, it isolates the
//! offending candidates by **delta-debugging partitioning** (the ddmin idea) and applies a
//! deterministic satisfiable subset, reporting the candidates this bounded search rejects as held.
//! The oracle is the tool's own `apply` (async, and it mutates the project), so each trial restores
//! the caller-provided journal first; a generic binary-search/delta-debugging crate cannot drive that
//! side-effecting async oracle, so the small partitioning loop lives here.
//! The happy path — the whole set resolves — is a single `apply` with no extra cost, and the
//! per-package managers (npm/yarn/bun), which never fail atomically, always take it.

use std::collections::HashSet;

use cooldown_core::{
    ApplyObserver, ApplyReport, Change, PreparedMutation, ProjectMutationState, Result, SkipReason,
    Skipped, ToolWrite,
};

use super::change_key::{ChangeTargetKey, change_target_key};

#[derive(Debug)]
pub(crate) enum ApplyFailure {
    /// The native apply failed after its observed state was restored successfully.
    Failed(cooldown_core::CoreError),
    /// The native apply may have mutated the project, but its state could not be observed or
    /// restored safely.
    /// No later mutation may run.
    RestoreConflict(cooldown_core::CoreError),
}

impl ApplyFailure {
    #[cfg(test)]
    fn into_core_error(self) -> cooldown_core::CoreError {
        match self {
            ApplyFailure::Failed(error) | ApplyFailure::RestoreConflict(error) => error,
        }
    }
}

type ApplyResult<T> = Result<T, ApplyFailure>;

#[cfg(test)]
async fn apply_resilient(
    writer: &dyn ToolWrite,
    project: &cooldown_core::Project,
    plan: &cooldown_core::Plan,
) -> Result<ApplyReport> {
    apply_resilient_full(writer, project, plan)
        .await
        .map(|mutation| mutation.report)
}

#[cfg(test)]
async fn apply_resilient_full(
    writer: &dyn ToolWrite,
    project: &cooldown_core::Project,
    plan: &cooldown_core::Plan,
) -> Result<AppliedMutation> {
    let mutation = PreparedMutation::prepare(writer, project, plan).await?;
    apply_resilient_with_observer(writer, &mutation, &())
        .await
        .map_err(ApplyFailure::into_core_error)
}

/// A successful apply and the project state observed immediately afterwards for later drift checks.
#[derive(Debug)]
pub(crate) struct AppliedMutation {
    pub(crate) report: ApplyReport,
    pub(crate) expected: ProjectMutationState,
    /// Candidates the recovery dropped for a failure that is *not* a resolver rejection (a
    /// verification or stale-lock error, e.g. a peer contract the resolve broke without a
    /// culpable candidate). These are per-candidate errors the caller must report as such —
    /// reclassifying them as policy skips would hide a real defect behind "the resolver rejected
    /// this change" and a `0 errors` summary.
    pub(crate) rejected: Vec<RejectedChange>,
}

/// One candidate dropped by recovery together with the error its singleton trial produced.
#[derive(Debug)]
pub(crate) struct RejectedChange {
    pub(crate) change: Change,
    pub(crate) error: cooldown_core::CoreError,
}

async fn apply_checked(
    writer: &dyn ToolWrite,
    mutation: &PreparedMutation,
    observer: &dyn ApplyObserver,
) -> ApplyResult<cooldown_core::ApplyAttemptOutcome> {
    let attempt = writer
        .apply_with_observer(mutation, observer)
        .await
        .map_err(ApplyFailure::RestoreConflict)?;
    attempt
        .into_outcome_for(mutation)
        .map_err(ApplyFailure::RestoreConflict)
}

/// Apply `plan` via `writer`, recovering from an atomic joint-resolve failure by holding only the
/// candidates that actually make the set unsatisfiable.
///
/// Returns the committed [`ApplyReport`]: the applied changes (plus any the resolve genuinely held)
/// and, appended, every candidate the recovery dropped — so one unfetchable or conflicting
/// candidate never blocks the rest.
/// A local environment failure such as a missing binary, read-only tree, or full disk is not a
/// candidate conflict and propagates unchanged.
/// The prepared operation's journal is restored before each recovery trial and the final commit,
/// so no partial widen or lock leaks.
/// Its project identity and writable topology are revalidated before every adapter attempt.
/// Native candidate work is forwarded to `observer` across the first attempt and recovery trials.
pub(crate) async fn apply_resilient_with_observer(
    writer: &dyn ToolWrite,
    mutation: &PreparedMutation,
    observer: &dyn ApplyObserver,
) -> ApplyResult<AppliedMutation> {
    let plan = mutation.plan();
    let journal = mutation.journal();
    let first = apply_checked(writer, mutation, observer).await?;
    let (first_report, first_state) = match first {
        cooldown_core::ApplyAttemptOutcome::Finished { report, postimage } => (report, postimage),
        cooldown_core::ApplyAttemptOutcome::PendingRecovery { detail } => {
            return Err(ApplyFailure::RestoreConflict(
                cooldown_core::CoreError::PendingRecovery(detail),
            ));
        }
    };
    match first_report {
        Ok(report) => {
            return Ok(AppliedMutation {
                report,
                expected: first_state,
                rejected: Vec::new(),
            });
        }
        Err(error) if is_mutation_state_conflict(&error) => {
            return Err(ApplyFailure::RestoreConflict(error));
        }
        // A broken local environment (missing binary, full disk, read-only tree, corrupt store, an
        // unreadable lock) is not a per-candidate conflict — propagate it rather than bisecting the
        // plan and misreporting every candidate as held.
        Err(error) if error.is_local_environment_failure() => {
            journal
                .restore_if_unchanged(&first_state)
                .map_err(ApplyFailure::RestoreConflict)?;
            return Err(ApplyFailure::Failed(error));
        }
        // The set is unsatisfiable as a whole. Fall through to isolate the culprits and apply the rest.
        Err(_) => {}
    }

    let (accepted, dropped, trial_state) =
        verified_satisfiable_subset(writer, mutation, first_state, observer).await?;
    // Direct workspace members can emit sibling changes that share `(name, registry, target)`.
    // Include the sorted direct-member set so recovery never hides an excluded sibling behind an
    // accepted one. Transitive members remain attribution context, not distinct editable targets.
    let accepted_keys: HashSet<ChangeTargetKey> = accepted.iter().map(change_target_key).collect();

    // Commit exactly the accepted subset (restore first so the failed full-set attempt leaves nothing).
    journal
        .restore_if_unchanged(&trial_state)
        .map_err(ApplyFailure::RestoreConflict)?;
    let (mut report, expected) = if accepted.is_empty() {
        (
            ApplyReport::default(),
            journal
                .capture_state()
                .map_err(ApplyFailure::RestoreConflict)?,
        )
    } else {
        let committed = mutation
            .subset(accepted)
            .map_err(ApplyFailure::RestoreConflict)?;
        let result = apply_checked(writer, &committed, observer).await?;
        let (report, expected) = match result {
            cooldown_core::ApplyAttemptOutcome::Finished { report, postimage } => {
                (report, postimage)
            }
            cooldown_core::ApplyAttemptOutcome::PendingRecovery { detail } => {
                return Err(ApplyFailure::RestoreConflict(
                    cooldown_core::CoreError::PendingRecovery(detail),
                ));
            }
        };
        match report {
            Ok(report) => (report, expected),
            Err(error) if is_mutation_state_conflict(&error) => {
                return Err(ApplyFailure::RestoreConflict(error));
            }
            Err(error) => {
                journal
                    .restore_if_unchanged(&expected)
                    .map_err(ApplyFailure::RestoreConflict)?;
                return Err(ApplyFailure::Failed(error));
            }
        }
    };

    // Every candidate the subset excluded is accounted for, split by *why* its singleton trial
    // failed: a tool rejection (the resolver could not place it) stays a held skip carrying the
    // tool's own first stderr line, while any other failure — verification, stale lock — is a
    // per-candidate error the caller reports as such, never a policy skip.
    let mut rejection_errors: std::collections::HashMap<ChangeTargetKey, cooldown_core::CoreError> =
        dropped
            .into_iter()
            .map(|(change, error)| (change_target_key(&change), error))
            .collect();
    let mut rejected = Vec::new();
    for change in &plan.changes {
        let key = change_target_key(change);
        if accepted_keys.contains(&key) {
            continue;
        }
        match rejection_errors.remove(&key) {
            Some(error @ cooldown_core::CoreError::Tool { .. }) => {
                report.skipped.push(held(change, Some(&error)));
            }
            Some(error) => rejected.push(RejectedChange {
                change: change.clone(),
                error,
            }),
            // Defensive: partitioning descends every excluded change to a singleton, so an
            // unrecorded exclusion should not happen; keep the generic held row rather than lose it.
            None => report.skipped.push(held(change, None)),
        }
    }
    Ok(AppliedMutation {
        report,
        expected,
        rejected,
    })
}

/// A deterministic subset of `changes` that `apply` can resolve together, found by
/// delta-debugging partitioning.
///
/// `accepted` is grown from the empty set and always stays satisfiable. A work-list of candidate
/// groups is seeded with the two halves of the full set (the full set is already known to fail, so
/// testing it again is skipped). For each group, `accepted + group` is trialled: if it resolves, the
/// group folds into `accepted`; a singleton group that still fails is an irreducible culprit and is
/// dropped; a larger failing group is split in half and re-queued.
/// The partition search is bounded by its tree of groups, but it does not establish that no larger
/// non-contiguous subset would pass.
async fn verified_satisfiable_subset(
    writer: &dyn ToolWrite,
    mutation: &PreparedMutation,
    mut current_state: ProjectMutationState,
    observer: &dyn ApplyObserver,
) -> ApplyResult<(
    Vec<Change>,
    Vec<(Change, cooldown_core::CoreError)>,
    ProjectMutationState,
)> {
    let plan = mutation.plan();
    let journal = mutation.journal();
    let mut accepted: Vec<Change> = Vec::new();
    let mut dropped: Vec<(Change, cooldown_core::CoreError)> = Vec::new();
    let mut work: Vec<Vec<Change>> = Vec::new();
    push_halves(&mut work, plan.changes.clone());

    while let Some(group) = work.pop() {
        journal
            .restore_if_unchanged(&current_state)
            .map_err(ApplyFailure::RestoreConflict)?;
        let trial = mutation
            .subset(accepted.iter().chain(group.iter()).cloned().collect())
            .map_err(ApplyFailure::RestoreConflict)?;
        let result = apply_checked(writer, &trial, observer).await?;
        let report = match result {
            cooldown_core::ApplyAttemptOutcome::Finished { report, postimage } => {
                current_state = postimage;
                report
            }
            cooldown_core::ApplyAttemptOutcome::PendingRecovery { detail } => {
                return Err(ApplyFailure::RestoreConflict(
                    cooldown_core::CoreError::PendingRecovery(detail),
                ));
            }
        };
        match report {
            Ok(_) => accepted.extend(group),
            Err(error) if is_mutation_state_conflict(&error) => {
                return Err(ApplyFailure::RestoreConflict(error));
            }
            // A broken local environment surfacing mid-recovery must propagate, not be charged to
            // whichever candidates happen to be in this trial group.
            Err(error) if error.is_local_environment_failure() => {
                journal
                    .restore_if_unchanged(&current_state)
                    .map_err(ApplyFailure::RestoreConflict)?;
                return Err(ApplyFailure::Failed(error));
            }
            // The group cannot join `accepted`: split it, or drop it if it is a single culprit —
            // keeping the culprit's own error so the caller can report *why* it was dropped.
            Err(_) if group.len() > 1 => push_halves(&mut work, group),
            Err(error) => {
                if let Some(change) = group.into_iter().next() {
                    dropped.push((change, error));
                }
            }
        }
    }
    Ok((accepted, dropped, current_state))
}

fn is_mutation_state_conflict(error: &cooldown_core::CoreError) -> bool {
    matches!(
        error,
        cooldown_core::CoreError::LockConflict(_)
            | cooldown_core::CoreError::LockUnreadable(_)
            | cooldown_core::CoreError::PendingRecovery(_)
    )
}

/// Split `group` in two and push the halves so the left half is processed first (LIFO work-list).
/// `split_off` consumes `group` without cloning; an empty half is not pushed.
fn push_halves(work: &mut Vec<Vec<Change>>, mut group: Vec<Change>) {
    let right = group.split_off(group.len() / 2);
    if !right.is_empty() {
        work.push(right);
    }
    if !group.is_empty() {
        work.push(group);
    }
}

/// A held skip for a candidate the resolve could not place. It blames itself (the generic "resolver
/// rejected this change" form), matching what each adapter emitted when it marked the whole batch
/// held — elaborated with the tool's own first stderr line when the singleton trial's rejection is
/// available, since that line carries the fact the generic message lacks (which requirement, held
/// by whom).
fn held(change: &Change, rejection: Option<&cooldown_core::CoreError>) -> Skipped {
    Skipped {
        change: change.clone(),
        reason: SkipReason::ResolverConflict,
        offending: Some(change.package.clone()),
        detail: rejection.and_then(rejection_detail),
    }
}

/// The first non-empty stderr line of a tool rejection, bounded for the report table. `None` for
/// non-tool errors and empty stderr, falling back to the generic reason message. The line reaches
/// the TTY and JSON reports verbatim, so embedded URLs are stripped of credentials here, at
/// construction.
fn rejection_detail(error: &cooldown_core::CoreError) -> Option<String> {
    let cooldown_core::CoreError::Tool { stderr, .. } = error else {
        return None;
    };
    // Prefer the tool's own failure sentence — cargo/uv/pnpm prefix it `error`, uv's resolver
    // conflicts prefix it `×` ("× No solution found when resolving dependencies") — over the
    // progress noise around it ("Updating crates.io index", "Using CPython 3.12.13"), which says
    // nothing about the rejection. The fallback still skips the known chatter prefixes so a
    // stderr with no marked sentence yields its first substantive line, not the banner.
    let noise = |line: &&str| {
        [
            "Using CPython",
            "Creating virtual environment",
            "Resolved ",
            "Audited ",
            "Updating ",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
    };
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("error") || line.starts_with('×'))
        .or_else(|| {
            stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !noise(line))
        })
        .or_else(|| stderr.lines().map(str::trim).find(|line| !line.is_empty()))?;
    // Resolver errors quote registry URLs, which can embed credentials on private registries;
    // redact before the cap bounds the (possibly lengthened) line.
    let mut line = cooldown_core::redact::url_secrets(line);
    // Char-boundary-safe cap: stderr is tool-controlled and can be arbitrarily long.
    if let Some((cut, _)) = line.char_indices().nth(200) {
        line.truncate(cut);
        line.push('…');
    }
    Some(format!("the resolver rejected this change: {line}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejection_detail_prefers_the_failure_sentence_over_progress_chatter() {
        // uv prints environment banners before its verdict, and its no-solution sentence carries
        // a `×` marker with no `error:` prefix — the row detail must surface the verdict, never
        // "Using CPython 3.12.13".
        let conflict = CoreError::Tool {
            tool: "uv".into(),
            termination: ToolTermination::ExitCode(1),
            stderr: "Using CPython 3.12.13\n  × No solution found when resolving dependencies:\n"
                .into(),
        };
        let detail = super::rejection_detail(&conflict).expect("tool stderr yields a detail");
        assert!(
            detail.contains("× No solution found"),
            "the resolver verdict is the detail: {detail}"
        );
        assert!(!detail.contains("Using CPython"));

        // With neither an `error:` nor a `×` sentence, the first substantive line wins over the
        // banner; a banner-only stderr still yields the banner rather than nothing.
        let odd = CoreError::Tool {
            tool: "uv".into(),
            termination: ToolTermination::ExitCode(2),
            stderr: "Using CPython 3.12.13\nfailed to fetch metadata\n".into(),
        };
        let detail = super::rejection_detail(&odd).expect("substantive line yields a detail");
        assert!(detail.contains("failed to fetch metadata"), "{detail}");
        let banner_only = CoreError::Tool {
            tool: "uv".into(),
            termination: ToolTermination::ExitCode(2),
            stderr: "Using CPython 3.12.13\n".into(),
        };
        let detail = super::rejection_detail(&banner_only).expect("banner-only still reports");
        assert!(detail.contains("Using CPython 3.12.13"));
    }

    use super::{
        ApplyFailure, Skipped, apply_resilient, apply_resilient_full, apply_resilient_with_observer,
    };
    use async_trait::async_trait;
    use camino::Utf8Path;
    use color_eyre::eyre;
    use cooldown_core::{
        ApplyObserver, ApplyReport, Change, CoreError, PackageId, Plan, PreparedMutation, Project,
        ProjectMutationJournal, Result, RewriteMode, ToolId, ToolTermination, ToolWrite,
        UpdateKind, VerifyReport, Version,
    };
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TOOL: ToolId = ToolId("mock");

    /// Decides whether a trial plan (by package names) resolves — the mock's satisfiability oracle.
    type ResolveOracle = Box<dyn Fn(&[String]) -> bool + Send + Sync>;

    fn change(name: &str) -> Change {
        change_from(name, "1.0.0", "2.0.0")
    }

    fn change_from(name: &str, from: &str, to: &str) -> Change {
        Change {
            package: PackageId::new(TOOL, name.to_string(), None),
            from: Version::new(from),
            to: Version::new(to),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        }
    }

    fn member(name: &str, path: &str) -> cooldown_core::MemberRef {
        cooldown_core::MemberRef {
            name: name.to_string(),
            path: path.to_string(),
        }
    }

    fn plan(names: &[&str]) -> Plan {
        Plan {
            changes: names.iter().map(|name| change(name)).collect(),
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        }
    }

    /// A [`Project`] rooted in its own temporary directory.
    struct TempProject {
        /// Owns the temporary directory; dropping it deletes the project tree.
        directory: tempfile::TempDir,
        project: Project,
    }

    fn temp_project() -> TempProject {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let manifest = root.join("manifest");
        let project = Project {
            root,
            kind: TOOL,
            manifest,
            exclude_newer: None,
        };
        TempProject {
            directory: dir,
            project,
        }
    }

    /// A local fault the mock injects before consulting the resolve oracle, so a test can assert it
    /// propagates as-is rather than being bisected into a held report.
    enum Fault {
        Spawn,
        Environment,
        Filesystem,
    }

    #[derive(Clone, Copy)]
    enum RejectError {
        Tool,
        StaleLock,
    }

    /// A `ToolWrite` whose joint resolve (`apply`) is unsatisfiable iff the plan contains any package
    /// in `unsatisfiable_with` AND the `requires` rule (a conflict): the closure decides per plan.
    /// `apply` reports every change applied on success; the journal is empty so restore is a no-op.
    /// `apply_calls` counts invocations so a test can assert the happy path adds no overhead.
    struct MockWriter {
        tool: ToolId,
        resolves: ResolveOracle,
        fault: Option<Fault>,
        reject_error: RejectError,
        apply_calls: AtomicUsize,
    }

    impl MockWriter {
        fn new(resolves: impl Fn(&[String]) -> bool + Send + Sync + 'static) -> Self {
            MockWriter {
                tool: TOOL,
                resolves: Box::new(resolves),
                fault: None,
                reject_error: RejectError::Tool,
                apply_calls: AtomicUsize::new(0),
            }
        }

        fn stale_lock(resolves: impl Fn(&[String]) -> bool + Send + Sync + 'static) -> Self {
            MockWriter {
                reject_error: RejectError::StaleLock,
                ..MockWriter::new(resolves)
            }
        }

        fn with_tool(mut self, tool: ToolId) -> Self {
            self.tool = tool;
            self
        }

        fn apply_calls(&self) -> usize {
            self.apply_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ToolWrite for MockWriter {
        fn mutation_tool(&self) -> ToolId {
            self.tool
        }

        async fn mutation_journal(
            &self,
            project: &Project,
            _plan: &Plan,
        ) -> Result<ProjectMutationJournal> {
            ProjectMutationJournal::capture(&project.root, [Utf8Path::new("requirements.txt")])
        }

        async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
            let (_, plan, _) = mutation.parts_for(self)?;
            self.apply_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(fault) = &self.fault {
                return Err(match fault {
                    Fault::Spawn => CoreError::ToolSpawn {
                        tool: "mock".into(),
                        detail: "binary missing".into(),
                    },
                    Fault::Environment => CoreError::Tool {
                        tool: "pnpm".into(),
                        termination: ToolTermination::ExitCode(1),
                        stderr: "pnpm: unable to open database file".into(),
                    },
                    Fault::Filesystem => {
                        CoreError::Filesystem("manifest is read-only (os error 30)".into())
                    }
                });
            }
            let names: Vec<String> = plan
                .changes
                .iter()
                .map(|change| change.package.name.clone())
                .collect();
            if (self.resolves)(&names) {
                Ok(ApplyReport {
                    applied: plan.changes.clone(),
                    ..ApplyReport::default()
                })
            } else {
                Err(match self.reject_error {
                    RejectError::Tool => CoreError::Tool {
                        tool: "mock".into(),
                        termination: ToolTermination::ExitCode(1),
                        stderr: "unsatisfiable".into(),
                    },
                    RejectError::StaleLock => CoreError::StaleLock(
                        "pnpm-lock.yaml importer apps/admin dependency vite: version 7.3.5 does not satisfy range ^6".into(),
                    ),
                })
            }
        }

        async fn apply_with_observer(
            &self,
            mutation: &PreparedMutation,
            observer: &dyn ApplyObserver,
        ) -> Result<cooldown_core::ApplyAttempt> {
            let (_, plan, journal) = mutation.parts_for(self)?;
            for change in &plan.changes {
                observer.candidate_started(change);
            }
            let report = self.apply(mutation).await;
            let postimage = journal.capture_state()?;
            mutation.finished_attempt(report, &postimage)
        }

        async fn build(&self, _project: &Project) -> Result<VerifyReport> {
            Ok(VerifyReport {
                ok: true,
                detail: String::new(),
            })
        }
    }

    fn names(changes: &[Change]) -> BTreeSet<String> {
        changes
            .iter()
            .map(|change| change.package.name.clone())
            .collect()
    }

    fn skipped_names(report: &ApplyReport) -> BTreeSet<String> {
        report
            .skipped
            .iter()
            .map(|skipped| skipped.change.package.name.clone())
            .collect()
    }

    fn skipped_from_versions(report: &ApplyReport) -> BTreeSet<String> {
        report
            .skipped
            .iter()
            .map(|skipped| skipped.change.from.to_string())
            .collect()
    }

    #[tokio::test]
    async fn happy_path_is_a_single_apply_with_no_bisection() {
        // Everything resolves: exactly one `apply`, every change applied, nothing held.
        let writer = MockWriter::new(|_| true);
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let plan = plan(&["a", "b", "c"]);
        let report = apply_resilient(&writer, &project, &plan).await.unwrap();

        assert_eq!(names(&report.applied), names(&plan.changes));
        assert!(report.skipped.is_empty());
        assert_eq!(writer.apply_calls(), 1);
    }

    #[tokio::test]
    async fn prepared_operation_rejects_another_adapter() -> eyre::Result<()> {
        let writer = MockWriter::new(|_| true);
        let other = MockWriter::new(|_| true).with_tool(ToolId("other"));
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        std::fs::write(project.root.join("requirements.txt"), "original")?;
        let plan = plan(&["a"]);
        let mutation = PreparedMutation::prepare(&writer, &project, &plan).await?;

        let result = other.apply(&mutation).await;

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        assert_eq!(other.apply_calls(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn prepared_operation_rejects_an_unauthorized_retry_subset() -> eyre::Result<()> {
        let writer = MockWriter::new(|_| true);
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        std::fs::write(project.root.join("requirements.txt"), "original")?;
        let mutation = PreparedMutation::prepare(&writer, &project, &plan(&["a"])).await?;

        let result = mutation.subset(vec![change("b")]);

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        Ok(())
    }

    #[tokio::test]
    async fn prepared_operation_rejects_another_projects_postimage() -> eyre::Result<()> {
        let writer = MockWriter::new(|_| true);
        let TempProject {
            directory: _first_directory,
            project: first,
        } = temp_project();
        let TempProject {
            directory: _second_directory,
            project: second,
        } = temp_project();
        std::fs::write(first.root.join("requirements.txt"), "first")?;
        std::fs::write(second.root.join("requirements.txt"), "second")?;
        let plan = plan(&["a"]);
        let first_mutation = PreparedMutation::prepare(&writer, &first, &plan).await?;
        let second_mutation = PreparedMutation::prepare(&writer, &second, &plan).await?;
        let postimage = first_mutation.journal().capture_state()?;
        let attempt = first_mutation.finished_attempt(Ok(ApplyReport::default()), &postimage)?;

        let result = attempt.into_outcome_for(&second_mutation);

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn retargeted_file_symlink_stops_an_in_place_adapter_before_dispatch() -> eyre::Result<()>
    {
        use std::os::unix::fs::symlink;

        let writer = MockWriter::new(|_| true);
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let external = tempfile::tempdir()?;
        let external = camino::Utf8PathBuf::from_path_buf(external.path().to_owned())
            .map_err(|path| eyre::eyre!("path is not UTF-8: {}", path.display()))?;
        let relative = Utf8Path::new("requirements.txt");
        std::fs::write(project.root.join(relative), "original")?;
        std::fs::write(external.join(relative), "external")?;
        let plan = plan(&["a"]);
        let mutation = PreparedMutation::prepare(&writer, &project, &plan).await?;
        std::fs::remove_file(project.root.join(relative))?;
        symlink(external.join(relative), project.root.join(relative))?;

        let result = apply_resilient_with_observer(&writer, &mutation, &()).await;

        std::assert_matches!(
            result,
            Err(ApplyFailure::RestoreConflict(CoreError::LockConflict(_)))
        );
        assert_eq!(writer.apply_calls(), 0);
        assert_eq!(
            std::fs::read_to_string(external.join(relative))?,
            "external"
        );
        Ok(())
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn new_hard_link_stops_an_in_place_adapter_before_dispatch() -> eyre::Result<()> {
        let writer = MockWriter::new(|_| true);
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let external = tempfile::tempdir()?;
        let external = camino::Utf8PathBuf::from_path_buf(external.path().to_owned())
            .map_err(|path| eyre::eyre!("path is not UTF-8: {}", path.display()))?;
        let relative = Utf8Path::new("requirements.txt");
        std::fs::write(project.root.join(relative), "original")?;
        let plan = plan(&["a"]);
        let mutation = PreparedMutation::prepare(&writer, &project, &plan).await?;
        std::fs::hard_link(project.root.join(relative), external.join(relative))?;

        let result = apply_resilient_with_observer(&writer, &mutation, &()).await;

        std::assert_matches!(
            result,
            Err(ApplyFailure::RestoreConflict(CoreError::LockConflict(_)))
        );
        assert_eq!(writer.apply_calls(), 0);
        assert_eq!(
            std::fs::read_to_string(external.join(relative))?,
            "original"
        );
        Ok(())
    }

    #[tokio::test]
    async fn observed_apply_forwards_every_native_candidate() -> eyre::Result<()> {
        struct Counter(AtomicUsize);

        impl ApplyObserver for Counter {
            fn candidate_started(&self, _change: &Change) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let writer = MockWriter::new(|_| true);
        let observer = Counter(AtomicUsize::new(0));
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let plan = plan(&["a", "b", "c"]);
        let mutation = PreparedMutation::prepare(&writer, &project, &plan).await?;

        apply_resilient_with_observer(&writer, &mutation, &observer)
            .await
            .map_err(ApplyFailure::into_core_error)?;

        assert_eq!(observer.0.load(Ordering::SeqCst), 3);
        Ok(())
    }

    #[tokio::test]
    async fn one_unfetchable_candidate_is_isolated_and_the_rest_apply() {
        // `colors` is unfetchable (the resolve fails whenever it is present); every other candidate
        // must still be applied — the exact "one bad version blocks everything" bug this guards.
        let writer = MockWriter::new(|names| !names.iter().any(|n| n == "colors"));
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let plan = plan(&["a", "b", "colors", "c", "d"]);
        let report = apply_resilient(&writer, &project, &plan).await.unwrap();

        assert_eq!(
            names(&report.applied),
            ["a", "b", "c", "d"]
                .iter()
                .map(ToString::to_string)
                .collect()
        );
        assert_eq!(
            skipped_names(&report),
            std::iter::once("colors".to_string()).collect()
        );
    }

    #[tokio::test]
    async fn stale_lock_candidate_is_isolated_and_reported_as_an_error_not_a_skip() {
        // A stale lock from a trial resolve is a property of that candidate set, not the local machine.
        // The recovery oracle must bisect it just like an unsatisfiable resolve, so one pnpm importer
        // mismatch does not turn the whole workspace into held candidates — but the dropped
        // candidate itself is a verification failure, not a resolver rejection: it must surface as
        // a per-candidate error carrying the original message, never as a "resolver rejected this
        // change" skip in a report claiming 0 errors.
        let writer = MockWriter::stale_lock(|names| !names.iter().any(|name| name == "vite"));
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let plan = plan(&["a", "vite", "b"]);
        let mutation = apply_resilient_full(&writer, &project, &plan)
            .await
            .unwrap();

        assert_eq!(
            names(&mutation.report.applied),
            ["a", "b"].iter().map(ToString::to_string).collect()
        );
        assert!(
            skipped_names(&mutation.report).is_empty(),
            "a verification failure must not be reclassified as a policy skip"
        );
        let rejected: Vec<&str> = mutation
            .rejected
            .iter()
            .map(|rejection| rejection.change.package.name.as_str())
            .collect();
        assert_eq!(rejected, ["vite"]);
        assert!(
            matches!(
                mutation.rejected.first().map(|rejection| &rejection.error),
                Some(cooldown_core::CoreError::StaleLock(detail))
                    if detail.contains("does not satisfy range")
            ),
            "the trial's own error (and its message) must be preserved"
        );
        assert!(
            writer.apply_calls() > 1,
            "stale lock must be bisected, not propagated as a local fault"
        );
    }

    #[tokio::test]
    async fn tool_rejected_candidate_keeps_its_stderr_in_the_held_skip_detail() {
        // A genuine resolver rejection stays a held skip — but no longer a bare one: the tool's
        // own first stderr line (which requirement, held by whom) rides along as the detail.
        let writer = MockWriter::new(|names| !names.iter().any(|name| name == "colors"));
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let plan = plan(&["a", "colors", "b"]);
        let report = apply_resilient(&writer, &project, &plan).await.unwrap();

        let held: Vec<&Skipped> = report
            .skipped
            .iter()
            .filter(|skipped| skipped.change.package.name == "colors")
            .collect();
        assert_eq!(held.len(), 1);
        assert_eq!(
            held.first().and_then(|skipped| skipped.detail.as_deref()),
            Some("the resolver rejected this change: unsatisfiable"),
        );
    }

    #[tokio::test]
    async fn multiple_unfetchable_candidates_are_all_isolated() {
        let writer =
            MockWriter::new(|names| !names.iter().any(|n| n == "colors" || n == "left-pad"));
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let plan = plan(&["a", "colors", "b", "left-pad", "c"]);
        let report = apply_resilient(&writer, &project, &plan).await.unwrap();

        assert_eq!(
            names(&report.applied),
            ["a", "b", "c"].iter().map(ToString::to_string).collect()
        );
        assert_eq!(
            skipped_names(&report),
            ["colors", "left-pad"]
                .iter()
                .map(ToString::to_string)
                .collect()
        );
    }

    #[tokio::test]
    async fn target_collision_reports_excluded_direct_sibling_as_held() {
        // Two workspace members can bump the same package to the same target from different current
        // lines. If recovery accepts one member and excludes the other, the excluded sibling must not
        // disappear behind a `(name, to)` key collision.
        let writer = MockWriter::new(|names| {
            !names
                .iter()
                .any(|name| name == "nix" && names.iter().filter(|n| *n == "nix").count() > 1)
        });
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let mut app_a = change_from("nix", "0.28.0", "0.31.3");
        app_a.members = vec![member("app-a", "crates/app-a")];
        let mut app_b = change_from("nix", "0.30.0", "0.31.3");
        app_b.members = vec![member("app-b", "crates/app-b")];
        let plan = Plan {
            changes: vec![app_a, app_b],
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        };
        let report = apply_resilient(&writer, &project, &plan).await.unwrap();

        assert_eq!(report.applied.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(
            skipped_from_versions(&report),
            std::iter::once("0.30.0".to_string()).collect()
        );
    }

    #[tokio::test]
    async fn earlier_resolving_half_is_not_displaced_by_a_later_conflicting_candidate() {
        // The left half (`a`, `b`) resolves together. The right half contains one candidate (`r`) that
        // is valid alone but conflicts with the accepted left half, plus one irreducible failure
        // (`bad`). A recovery scheduler that isolates `bad` before accepting the known-good left half
        // can wrongly keep `r` and hold both `a` and `b`; a real-workspace run surfaced exactly that
        // as many false `blocked` rows. The left half must keep priority so the larger satisfiable
        // subset lands.
        let writer = MockWriter::new(|names| {
            let contains_bad = names.iter().any(|name| name == "bad");
            let contains_r = names.iter().any(|name| name == "r");
            let contains_left = names.iter().any(|name| name == "a" || name == "b");
            !(contains_bad || contains_r && contains_left)
        });
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let plan = plan(&["a", "b", "r", "bad"]);
        let report = apply_resilient(&writer, &project, &plan).await.unwrap();

        assert_eq!(
            names(&report.applied),
            ["a", "b"].into_iter().map(String::from).collect()
        );
        assert_eq!(
            skipped_names(&report),
            ["bad", "r"].into_iter().map(String::from).collect()
        );
    }

    #[tokio::test]
    async fn a_mutually_exclusive_pair_keeps_one_side() {
        // `x` and `y` cannot coexist (the resolve fails iff BOTH are present), but each is fine alone.
        // Recovery must keep one and hold the other — never block both.
        let writer = MockWriter::new(|names| {
            !(names.iter().any(|n| n == "x") && names.iter().any(|n| n == "y"))
        });
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let plan = plan(&["a", "x", "b", "y", "c"]);
        let report = apply_resilient(&writer, &project, &plan).await.unwrap();

        // a, b, c always apply; exactly one of x/y is held.
        let applied = names(&report.applied);
        assert!(["a", "b", "c"].iter().all(|n| applied.contains(*n)));
        let held = skipped_names(&report);
        assert_eq!(
            held.len(),
            1,
            "exactly one of the conflicting pair is held: {held:?}"
        );
        assert!(held.contains("x") || held.contains("y"));
        // The committed set is satisfiable: it does not contain both x and y.
        assert!(!(applied.contains("x") && applied.contains("y")));
    }

    #[tokio::test]
    async fn everything_unsatisfiable_holds_all_and_applies_none() {
        let writer = MockWriter::new(|_| false);
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let plan = plan(&["a", "b", "c"]);
        let report = apply_resilient(&writer, &project, &plan).await.unwrap();

        assert!(report.applied.is_empty());
        assert_eq!(skipped_names(&report), names(&plan.changes));
    }

    /// Drive `writer` (pre-configured with one injected fault) through `apply_resilient` and assert
    /// the error propagates verbatim in exactly one apply — no bisection.
    ///
    /// `apply` returns the injected fault before it consults the resolve oracle, so
    /// `apply_calls() == 1` proves recovery never ran.
    async fn assert_propagates_without_bisecting(
        writer: MockWriter,
        expected: impl Fn(&CoreError) -> bool,
    ) {
        let TempProject {
            directory: _directory,
            project,
        } = temp_project();
        let plan = plan(&["a", "b"]);
        let result = apply_resilient(&writer, &project, &plan).await;

        std::assert_matches!(&result, Err(err) if expected(err));
        assert_eq!(writer.apply_calls(), 1);
    }

    #[tokio::test]
    async fn a_spawn_failure_propagates_without_bisecting() {
        let mut writer = MockWriter::new(|_| true);
        writer.fault = Some(Fault::Spawn);
        assert_propagates_without_bisecting(writer, |err| {
            matches!(err, CoreError::ToolSpawn { .. })
        })
        .await;
    }

    #[tokio::test]
    async fn a_tool_environment_failure_propagates_without_bisecting() {
        let mut writer = MockWriter::new(|_| {
            panic!("environment failure should not consult the resolve oracle")
        });
        writer.fault = Some(Fault::Environment);
        assert_propagates_without_bisecting(writer, |err| {
            matches!(err, CoreError::Tool { stderr, .. } if stderr.contains("unable to open database file"))
        })
        .await;
    }

    #[tokio::test]
    async fn a_structured_local_failure_propagates_without_bisecting() {
        // A typed local fault (a read-only manifest surfacing as `CoreError::Filesystem`) is not a
        // resolver-graph decision, so it must propagate rather than be bisected into an all-held report.
        let mut writer =
            MockWriter::new(|_| panic!("a filesystem fault should not consult the resolve oracle"));
        writer.fault = Some(Fault::Filesystem);
        assert_propagates_without_bisecting(
            writer,
            |err| matches!(err, CoreError::Filesystem(detail) if detail.contains("read-only")),
        )
        .await;
    }
}
