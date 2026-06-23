//! Resilient application of a whole-graph plan.
//!
//! A tool that re-resolves the whole graph in one native command (pnpm, cargo, go, deno, uv) applies
//! its candidates **atomically**: if a single candidate is unsatisfiable — an unpublished/yanked
//! version the manager cannot fetch, or one side of a mutually-exclusive conflict — the whole command
//! fails and the adapter's `apply` returns an error. Reporting that as "every candidate is held" is
//! wrong: one bad candidate would block every other.
//!
//! [`apply_resilient`] wraps `apply` so that, on such a failure, it isolates the offending candidates
//! by **delta-debugging partitioning** (the ddmin idea) and applies the maximal satisfiable subset,
//! reporting only the genuine culprits as held. The oracle is the tool's own `apply` (async, and it
//! mutates the project), so each trial restores the caller-provided journal first; a generic
//! binary-search/delta-debugging crate cannot drive that side-effecting async oracle, so the small
//! partitioning loop lives here. The happy path — the whole set resolves — is a single `apply` with
//! no extra cost, and the per-package managers (npm/yarn/bun), which never fail atomically, always
//! take it.

use std::collections::HashSet;

use cooldown_core::{
    ApplyReport, Change, Plan, Project, ProjectMutationJournal, Result, SkipReason, Skipped,
    ToolWrite,
};

/// Apply `plan` via `writer`, recovering from an atomic joint-resolve failure by holding only the
/// candidates that actually make the set unsatisfiable.
///
/// Returns the committed [`ApplyReport`]: the applied changes (plus any the resolve genuinely held)
/// and, appended, every candidate the recovery dropped — so one unfetchable or conflicting candidate
/// never blocks the rest. A tool-spawn failure (the binary is missing) is not a candidate conflict
/// and propagates unchanged. `journal` is the caller's pre-apply snapshot; it is restored before each
/// recovery trial and before the final commit, so no partial widen or lock leaks.
pub(crate) async fn apply_resilient(
    writer: &dyn ToolWrite,
    project: &Project,
    plan: &Plan,
    journal: &ProjectMutationJournal,
) -> Result<ApplyReport> {
    match writer.apply(project, plan, journal).await {
        Ok(report) => return Ok(report),
        Err(err) if err.is_tool_spawn_failure() => return Err(err),
        // The set is unsatisfiable as a whole. Fall through to isolate the culprits and apply the rest.
        Err(_) => {}
    }

    let accepted =
        maximal_satisfiable_subset(writer, project, &plan.changes, plan.rewrite, journal).await?;
    // Each change in a plan is uniquely keyed by (name, target): the planner never emits two moves of
    // the same package to the same version (a multi-version dep keeps distinct targets per line). Keyed
    // by owned strings so the accepted subset can be moved into the commit plan below.
    let accepted_keys: HashSet<(String, String)> = accepted
        .iter()
        .map(|change| (change.package.name.clone(), change.to.as_str().to_string()))
        .collect();

    // Commit exactly the accepted subset (restore first so the failed full-set attempt leaves nothing).
    journal.restore(&project.root)?;
    let mut report = if accepted.is_empty() {
        ApplyReport::default()
    } else {
        let committed = Plan {
            changes: accepted,
            rewrite: plan.rewrite,
        };
        writer.apply(project, &committed, journal).await?
    };

    // Every candidate the subset excluded is held: the resolve could not place it.
    for change in &plan.changes {
        let key = (change.package.name.clone(), change.to.as_str().to_string());
        if !accepted_keys.contains(&key) {
            report.skipped.push(held(change));
        }
    }
    Ok(report)
}

/// The largest subset of `changes` that `apply` can resolve together, found by delta-debugging
/// partitioning.
///
/// `accepted` is grown from the empty set and always stays satisfiable. A work-list of candidate
/// groups is seeded with the two halves of the full set (the full set is already known to fail, so
/// testing it again is skipped). For each group, `accepted + group` is trialled: if it resolves, the
/// group folds into `accepted`; a singleton group that still fails is an irreducible culprit and is
/// dropped; a larger failing group is split in half and re-queued. This isolates per-candidate culprits
/// in O(log n) trials and resolves conflicts-with-accepted by dropping the conflicting side.
async fn maximal_satisfiable_subset(
    writer: &dyn ToolWrite,
    project: &Project,
    changes: &[Change],
    rewrite: cooldown_core::RewriteMode,
    journal: &ProjectMutationJournal,
) -> Result<Vec<Change>> {
    let mut accepted: Vec<Change> = Vec::new();
    let mut work: Vec<Vec<Change>> = Vec::new();
    push_halves(&mut work, changes.to_vec());

    while let Some(group) = work.pop() {
        journal.restore(&project.root)?;
        let trial = Plan {
            changes: accepted.iter().chain(group.iter()).cloned().collect(),
            rewrite,
        };
        match writer.apply(project, &trial, journal).await {
            Ok(_) => accepted.extend(group),
            Err(err) if err.is_tool_spawn_failure() => return Err(err),
            // The group cannot join `accepted`: split it, or drop it if it is a single culprit.
            Err(_) if group.len() > 1 => push_halves(&mut work, group),
            Err(_) => {}
        }
    }
    Ok(accepted)
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
/// rejected this change" form), matching what each adapter emitted when it marked the whole batch held.
fn held(change: &Change) -> Skipped {
    Skipped {
        change: change.clone(),
        reason: SkipReason::ResolverConflict,
        offending: Some(change.package.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::apply_resilient;
    use async_trait::async_trait;
    use cooldown_core::{
        ApplyReport, Change, CoreError, PackageId, Plan, Project, ProjectMutationJournal, Result,
        RewriteMode, ToolId, ToolTermination, ToolWrite, UpdateKind, VerifyReport, Version,
    };
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TOOL: ToolId = ToolId("mock");

    /// Decides whether a trial plan (by package names) resolves — the mock's satisfiability oracle.
    type ResolveOracle = Box<dyn Fn(&[String]) -> bool + Send + Sync>;

    fn change(name: &str) -> Change {
        Change {
            package: PackageId::new(TOOL, name.to_string(), None),
            from: Version::new("1.0.0"),
            to: Version::new("2.0.0"),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        }
    }

    fn plan(names: &[&str]) -> Plan {
        Plan {
            changes: names.iter().map(|name| change(name)).collect(),
            rewrite: RewriteMode::Auto,
        }
    }

    fn temp_project() -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let manifest = root.join("manifest");
        let project = Project {
            root,
            kind: TOOL,
            manifest,
            exclude_newer: None,
        };
        (dir, project)
    }

    /// A `ToolWrite` whose joint resolve (`apply`) is unsatisfiable iff the plan contains any package
    /// in `unsatisfiable_with` AND the `requires` rule (a conflict): the closure decides per plan.
    /// `apply` reports every change applied on success; the journal is empty so restore is a no-op.
    /// `apply_calls` counts invocations so a test can assert the happy path adds no overhead.
    struct MockWriter {
        resolves: ResolveOracle,
        spawn_fails: bool,
        apply_calls: AtomicUsize,
    }

    impl MockWriter {
        fn new(resolves: impl Fn(&[String]) -> bool + Send + Sync + 'static) -> Self {
            MockWriter {
                resolves: Box::new(resolves),
                spawn_fails: false,
                apply_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ToolWrite for MockWriter {
        async fn mutation_journal(
            &self,
            _project: &Project,
            _plan: &Plan,
        ) -> Result<ProjectMutationJournal> {
            Ok(ProjectMutationJournal::default())
        }

        async fn apply(
            &self,
            _project: &Project,
            plan: &Plan,
            _journal: &ProjectMutationJournal,
        ) -> Result<ApplyReport> {
            self.apply_calls.fetch_add(1, Ordering::SeqCst);
            if self.spawn_fails {
                return Err(CoreError::ToolSpawn {
                    tool: "mock".into(),
                    detail: "binary missing".into(),
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
                    skipped: Vec::new(),
                })
            } else {
                Err(CoreError::Tool {
                    tool: "mock".into(),
                    termination: ToolTermination::ExitCode(1),
                    stderr: "unsatisfiable".into(),
                })
            }
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

    #[tokio::test]
    async fn happy_path_is_a_single_apply_with_no_bisection() {
        // Everything resolves: exactly one `apply`, every change applied, nothing held.
        let writer = MockWriter::new(|_| true);
        let (_dir, project) = temp_project();
        let plan = plan(&["a", "b", "c"]);
        let journal = writer.mutation_journal(&project, &plan).await.unwrap();

        let report = apply_resilient(&writer, &project, &plan, &journal)
            .await
            .unwrap();

        assert_eq!(names(&report.applied), names(&plan.changes));
        assert!(report.skipped.is_empty());
        assert_eq!(writer.apply_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn one_unfetchable_candidate_is_isolated_and_the_rest_apply() {
        // `colors` is unfetchable (the resolve fails whenever it is present); every other candidate
        // must still be applied — the exact "one bad version blocks everything" bug this guards.
        let writer = MockWriter::new(|names| !names.iter().any(|n| n == "colors"));
        let (_dir, project) = temp_project();
        let plan = plan(&["a", "b", "colors", "c", "d"]);
        let journal = writer.mutation_journal(&project, &plan).await.unwrap();

        let report = apply_resilient(&writer, &project, &plan, &journal)
            .await
            .unwrap();

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
    async fn multiple_unfetchable_candidates_are_all_isolated() {
        let writer =
            MockWriter::new(|names| !names.iter().any(|n| n == "colors" || n == "left-pad"));
        let (_dir, project) = temp_project();
        let plan = plan(&["a", "colors", "b", "left-pad", "c"]);
        let journal = writer.mutation_journal(&project, &plan).await.unwrap();

        let report = apply_resilient(&writer, &project, &plan, &journal)
            .await
            .unwrap();

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
    async fn a_mutually_exclusive_pair_keeps_one_side() {
        // `x` and `y` cannot coexist (the resolve fails iff BOTH are present), but each is fine alone.
        // Recovery must keep one and hold the other — never block both.
        let writer = MockWriter::new(|names| {
            !(names.iter().any(|n| n == "x") && names.iter().any(|n| n == "y"))
        });
        let (_dir, project) = temp_project();
        let plan = plan(&["a", "x", "b", "y", "c"]);
        let journal = writer.mutation_journal(&project, &plan).await.unwrap();

        let report = apply_resilient(&writer, &project, &plan, &journal)
            .await
            .unwrap();

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
        let (_dir, project) = temp_project();
        let plan = plan(&["a", "b", "c"]);
        let journal = writer.mutation_journal(&project, &plan).await.unwrap();

        let report = apply_resilient(&writer, &project, &plan, &journal)
            .await
            .unwrap();

        assert!(report.applied.is_empty());
        assert_eq!(skipped_names(&report), names(&plan.changes));
    }

    #[tokio::test]
    async fn a_spawn_failure_propagates_without_bisecting() {
        let mut writer = MockWriter::new(|_| true);
        writer.spawn_fails = true;
        let (_dir, project) = temp_project();
        let plan = plan(&["a", "b"]);
        let journal = writer.mutation_journal(&project, &plan).await.unwrap();

        let result = apply_resilient(&writer, &project, &plan, &journal).await;

        assert!(
            result.is_err(),
            "a missing binary must propagate, not bisect"
        );
        // Exactly one apply: the first attempt errored as a spawn failure, no recovery trials.
        assert_eq!(writer.apply_calls.load(Ordering::SeqCst), 1);
    }
}
