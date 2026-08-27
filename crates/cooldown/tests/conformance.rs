//! Network-free conformance tests: drive the `Workspace` use cases against a fake `Tool` with
//! canned data, asserting the universal invariants and the cross-cutting behaviours (the check
//! gate, baseline acknowledgement, and the upgrade trial-rollback that never commits a violating
//! lock).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test helpers (including the shared `support` fixtures) and the in-file fake adapter; unwrap/expect/panic on known-good fixtures is the intended immediate test failure (clippy.toml allows all three in tests)"
)]

// Reused only for its temp-dir project fixtures (`Fixture::new` / `Fixture::new_non_git`), so the
// coordination-namespace test below can drive the fake pipeline in both a Git-anchored and a
// genuinely non-Git project root.
mod support;

use async_trait::async_trait;
use camino::Utf8PathBuf;
use color_eyre::eyre;
use cooldown::app::{
    AdapterSet, Baseline, CheckStatus, Exit, OutdatedStatus, ProjectCtx, RunOpts, Workspace,
};
use cooldown_core::config::builtin_default_layer;
use cooldown_core::*;
use std::collections::HashMap;
use std::io::Write as _;
use std::sync::Arc;
use std::sync::Mutex;

const GO: ToolId = ToolId("go");

fn ts(s: &str) -> jiff::Timestamp {
    s.parse().unwrap()
}
fn now() -> jiff::Timestamp {
    ts("2026-06-17T00:00:00Z")
}

fn rel(v: &str, ord: u32, pub_at: Option<&str>, kind: Option<UpdateKind>) -> Release {
    Release {
        version: Version::new(v),
        order: ReleaseOrder(ord.to_be_bytes().to_vec()),
        major: MajorKey(String::new()),
        major_number: v
            .trim_start_matches('v')
            .split('.')
            .next()
            .and_then(|major| major.parse().ok()),
        kind_from_current: kind,
        beyond_declared_bound: false,
        beyond_latest_tag: false,
        published_at: pub_at.map(ts),
        yanked: false,
        quality: ReleaseQuality::Stable,
    }
}

fn dep(name: &str, current: &str, direct: bool) -> Dependency {
    Dependency {
        package: PackageId::new(GO, name, Some("proxy.example".into())),
        advisory_identity: Some(name.to_string()),
        current: Version::new(current),
        current_quality: ReleaseQuality::Stable,
        direct,
        artifacts: Vec::new(),
        graph_floor: None,
        graph_ceiling: None,
        declared_bound: None,
        members: Vec::new(),
        pinned: false,
        hold_edges: Vec::new(),
    }
}

/// An attributed floor edge for the circular-hold discount tests: `requirer`@`requirer_version`
/// floors the carrying dependency at `bound` via `requirement`.
fn floor_edge(
    requirer: &str,
    requirer_version: &str,
    requirement: &str,
    bound: &str,
) -> GraphHoldEdge {
    GraphHoldEdge {
        requirer: requirer.to_string(),
        requirer_version: Version::new(requirer_version),
        requirement: requirement.to_string(),
        bound: Version::new(bound),
        kind: GraphHoldKind::Floor,
    }
}

#[derive(Default)]
struct State {
    /// Simulates a re-lock having dragged in a fresh transitive.
    fresh_transitive_present: bool,
    /// Whether `apply` has already mutated the project once.
    apply_attempted: bool,
    /// Number of resolver apply attempts made during this run.
    apply_attempts: usize,
    /// Direct manifest constraints that have no resolved graph entry.
    manifest_constraints: Vec<Dependency>,
    /// Package versions pinned by a successful fake apply, surfaced by the next graph probe.
    applied_versions: HashMap<String, Version>,
    /// Release metadata failure armed only after the forward batch reaches the fake lock.
    fail_releases_after_apply_for: Option<String>,
    /// Make the fake mutate `fake.lock` so rollback can be asserted against real bytes.
    write_lock_on_apply: bool,
    /// Require the mutation lifecycle hook to run before dependency discovery.
    require_recovery_before_read: bool,
    /// Whether the mutation lifecycle hook has run for this project.
    recovery_completed: bool,
    /// Simulate an independent lock edit immediately before a graph-probe failure.
    drift_lock_before_graph_failure: bool,
    /// Delay an armed graph-probe failure until this apply attempt, when configured.
    fail_graph_after_attempt: Option<usize>,
    /// Make an outer two-file rollback restore its first file, then fail on its second file.
    #[cfg(unix)]
    force_partial_restore_failure: bool,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "test fixture toggles independent failure modes to exercise the workspace invariants"
)]
struct FakeEco {
    direct: Vec<Dependency>,
    transitive: Vec<Dependency>,
    fresh_transitive: Option<Dependency>,
    releases: HashMap<String, Vec<Release>>,
    locked: HashMap<String, Release>,
    inject_fresh_on_apply: bool,
    /// Net version changes the fake reports as having been forced on packages the plan did not name —
    /// the whole-graph re-resolve's collateral. Reflected into the report's `applied` set so the
    /// executor must surface them, mirroring how the uv adapter reports a forced non-candidate move
    /// from the full lock diff (never silent).
    collateral_on_apply: Vec<Change>,
    /// Edge-binding rebinds the fake reports from its apply — simulating the cargo adapter's
    /// edge-policy pass so the app layer's plumbing (report rows, counts) is conformance-tested
    /// without a real `Cargo.lock`.
    edge_rebinds_on_apply: Vec<EdgeRebind>,
    stale_lock: bool,
    fail_graph_after_apply: bool,
    fail_locked_release_after_apply_for: Option<String>,
    stale_lock_after_apply: bool,
    build_fails_after_apply: bool,
    /// A package whose advisory identity the fake's `confirm_advisory_identities` withholds —
    /// simulating an adapter's feed-time veto (npm's effective-registry query), so the
    /// conformance tests can prove the app confirms before identities are queried or matched.
    confirm_strips: Option<String>,
    state: Mutex<State>,
    root: Utf8PathBuf,
}

impl FakeEco {
    fn project(&self) -> Project {
        Project {
            root: self.root.clone(),
            kind: GO,
            manifest: self.root.join("go.mod"),
            exclude_newer: None,
        }
    }

    fn mutation_parts<'a>(
        &self,
        mutation: &'a PreparedMutation,
    ) -> Result<(&'a Project, &'a Plan, &'a ProjectMutationJournal)> {
        if matches!(self.mutation_execution(), MutationExecution::Isolated(_)) {
            mutation.isolated_parts_for(self)
        } else {
            mutation.parts_for(self)
        }
    }
}

struct FakeMutationStage {
    #[expect(dead_code, reason = "the field keeps the staged test project alive")]
    scratch: tempfile::TempDir,
    source: Project,
    staged: Project,
    preimage: ProjectMutationJournal,
    publish_pending_recovery: bool,
}

#[async_trait]
impl IsolatedMutationStrategy for FakeEco {
    async fn prepare(
        &self,
        source: &Project,
        _coordination: &cooldown_core::fs::ProjectCoordination,
    ) -> Result<Box<dyn IsolatedMutation>> {
        if source.root.join("fail-isolated-staging").exists() {
            return Err(CoreError::Filesystem(
                "injected isolated staging failure".to_string(),
            ));
        }
        let scratch = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(scratch.path().to_owned()).map_err(|path| {
            CoreError::PathEncoding(format!("non-UTF-8 test path: {}", path.display()))
        })?;
        std::fs::copy(source.root.join("Cargo.lock"), root.join("Cargo.lock"))?;
        let preimage =
            ProjectMutationJournal::capture(&source.root, [camino::Utf8Path::new("Cargo.lock")])?;
        let staged = Project {
            root: root.clone(),
            manifest: root.join("go.mod"),
            kind: source.kind,
            exclude_newer: source.exclude_newer.clone(),
        };
        Ok(Box::new(FakeMutationStage {
            scratch,
            source: source.clone(),
            staged,
            preimage,
            publish_pending_recovery: source.root.join("publish-pending-recovery").exists(),
        }))
    }
}

#[async_trait]
impl IsolatedMutation for FakeMutationStage {
    fn project(&self) -> &Project {
        &self.staged
    }

    fn accepted_state(&self) -> Result<AcceptedProjectState> {
        let candidate =
            cooldown_core::ProjectMutationState::capture(&self.staged.root, &self.preimage)?;
        AcceptedProjectState::new(
            self.preimage.clone(),
            candidate,
            ProjectInputSnapshot::default(),
        )
    }

    async fn publish(&self, accepted: &AcceptedProjectState) -> Result<AcceptedPublication> {
        accepted.install(&self.source.root)?;
        if self.publish_pending_recovery {
            return Ok(AcceptedPublication::PublishedPendingRecovery {
                warnings: Vec::new(),
                error: CoreError::PendingRecovery(
                    "fake accepted publication retained recovery evidence".to_string(),
                ),
            });
        }
        Ok(AcceptedPublication::Published {
            warnings: Vec::new(),
        })
    }
}

fn apply_versions(
    mut deps: Vec<Dependency>,
    versions: &HashMap<String, Version>,
) -> Vec<Dependency> {
    for dep in &mut deps {
        if let Some(version) = versions.get(&dep.package.name) {
            dep.current = version.clone();
        }
    }
    deps
}

#[async_trait]
impl ToolRead for FakeEco {
    fn id(&self) -> ToolId {
        GO
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: true,
            has_incompatible: true,
            // Lets the advisory conformance tests below exercise the feed; inert for every
            // other test because no other test's policy stack enables `[advisories]`.
            advisory_ecosystem: Some("Go"),
            ..Default::default()
        }
    }
    fn project_detection(&self) -> cooldown_core::ProjectDetection {
        cooldown_core::ProjectDetection::Primary(cooldown_core::ProjectMarker {
            lockfile: "fake.lock",
            manifest: "fake.toml",
            alternate_manifests: &[],
            workspace_root: true,
        })
    }
    async fn confirm_advisory_identities(&self, _project: &Project, deps: &mut [Dependency]) {
        if let Some(name) = &self.confirm_strips {
            for dep in deps.iter_mut() {
                if dep.package.name == *name {
                    dep.advisory_identity = None;
                }
            }
        }
    }
    async fn dependencies(&self, p: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let state = self.state.lock().unwrap();
        if state.require_recovery_before_read && !state.recovery_completed {
            return Err(CoreError::System(
                "dependency discovery ran before mutation recovery".to_string(),
            ));
        }
        if scope == DepScope::Graph
            && self.fail_graph_after_apply
            && state.apply_attempted
            && state
                .fail_graph_after_attempt
                .is_none_or(|attempt| state.apply_attempts >= attempt)
        {
            if state.drift_lock_before_graph_failure {
                std::fs::write(p.root.join("fake.lock"), b"external edit")?;
            }
            return Err(CoreError::Transient("post-apply graph probe failed".into()));
        }
        let mut out = apply_versions(self.direct.clone(), &state.applied_versions);
        if scope == DepScope::Graph {
            out.extend(apply_versions(
                self.transitive.clone(),
                &state.applied_versions,
            ));
            if state.fresh_transitive_present
                && let Some(ft) = &self.fresh_transitive
            {
                // Reflect any applied downgrade so an `upgrade` reconcile pass that rolls the
                // floated-up transitive back is visible on the next graph probe.
                out.extend(apply_versions(vec![ft.clone()], &state.applied_versions));
            }
        }
        Ok(out)
    }
    async fn manifest_constraints(&self, _p: &Project) -> Result<Vec<Dependency>> {
        let state = self
            .state
            .lock()
            .map_err(|_| CoreError::System("fake state mutex poisoned".to_string()))?;
        Ok(apply_versions(
            state.manifest_constraints.clone(),
            &state.applied_versions,
        ))
    }
    async fn native_policy(&self, _p: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }
    async fn verify_lock_current(&self, p: &Project) -> Result<LockVerifyReport> {
        if p.root.join("fail-final-lock-verification").exists() {
            return Err(CoreError::Transient(
                "injected final lock verification failure".into(),
            ));
        }
        let stale = self.stale_lock
            || (self.stale_lock_after_apply && self.state.lock().unwrap().apply_attempted);
        Ok(LockVerifyReport {
            status: if stale {
                LockStatus::Stale
            } else {
                LockStatus::Current
            },
            detail: if stale { "stale".into() } else { "tidy".into() },
        })
    }
}

#[async_trait]
impl ReleaseFetcher for FakeEco {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> Result<Vec<Release>> {
        let fail_after_apply = {
            let state = self.state.lock().unwrap();
            state.apply_attempted
                && state
                    .fail_releases_after_apply_for
                    .as_deref()
                    .is_some_and(|name| name == dep.package.name)
        };
        if fail_after_apply {
            #[cfg(unix)]
            if self.state.lock().unwrap().force_partial_restore_failure {
                use std::os::unix::fs::PermissionsExt as _;

                std::fs::set_permissions(
                    self.root.join("blocked"),
                    std::fs::Permissions::from_mode(0o500),
                )?;
            }
            return Err(CoreError::Transient(
                format!("reconcile release probe failed for {}", dep.package.name).into(),
            ));
        }
        Ok(self
            .releases
            .get(&dep.package.name)
            .cloned()
            .unwrap_or_default())
    }
    async fn locked_release(
        &self,
        dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
    ) -> Result<Release> {
        let applied = {
            let state = self.state.lock().unwrap();
            if state.apply_attempted
                && self
                    .fail_locked_release_after_apply_for
                    .as_deref()
                    .is_some_and(|name| name == dep.package.name)
            {
                return Err(CoreError::Transient(
                    format!(
                        "post-apply locked release probe failed for {}",
                        dep.package.name
                    )
                    .into(),
                ));
            }
            state.applied_versions.get(&dep.package.name).cloned()
        };
        if let Some(version) = applied {
            return self
                .releases
                .get(&dep.package.name)
                .and_then(|releases| releases.iter().find(|release| release.version == version))
                .cloned()
                .ok_or_else(|| CoreError::NotFound(dep.package.name.clone()));
        }
        self.locked
            .get(&dep.package.name)
            .cloned()
            .ok_or_else(|| CoreError::NotFound(dep.package.name.clone()))
    }
}

#[async_trait]
impl ToolWrite for FakeEco {
    fn mutation_tool(&self) -> ToolId {
        GO
    }

    fn supports_transitive_advance(&self) -> bool {
        // The fake engine applies whatever the plan names, so graph-wide upgrade planning is
        // exercised by default; a `no-transitive-advance` marker file models an engine that
        // cannot pin an undeclared package (the npm/yarn/bun per-package family).
        !self.root.join("no-transitive-advance").exists()
    }

    fn mutation_execution(&self) -> MutationExecution<'_> {
        if self.root.join("use-isolated-mutation").exists() {
            MutationExecution::Isolated(self)
        } else {
            MutationExecution::InPlace
        }
    }

    async fn ensure_no_pending_mutation(&self, _p: &Project) -> Result<()> {
        let state = self.state.lock().unwrap();
        if state.require_recovery_before_read && !state.recovery_completed {
            return Err(CoreError::StaleLock(
                "pending fake mutation; run `cooldown recover`".to_string(),
            ));
        }
        Ok(())
    }

    async fn recover_pending_mutation(
        &self,
        _p: &Project,
        _coordination: &cooldown_core::fs::ProjectCoordination,
    ) -> Result<MutationRecovery> {
        let mut state = self.state.lock().unwrap();
        let disposition = if state.require_recovery_before_read && !state.recovery_completed {
            RecoveryDisposition::Restored
        } else {
            RecoveryDisposition::Unchanged
        };
        state.recovery_completed = true;
        // The workspace consumes the adapter, so `recovery_completed` cannot be inspected after a
        // run; the marker file mirrors `build-invoked` as the externally observable record that
        // the recovery hook ran.
        std::fs::write(self.root.join("recovery-invoked"), b"")?;
        Ok(MutationRecovery::settled(disposition))
    }

    async fn lock_edge_snapshot(&self, p: &Project) -> Result<Option<Vec<u8>>> {
        let path = p.root.join("edge.snapshot");
        match std::fs::read(path) {
            Ok(contents) => Ok(Some(contents)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn mutation_journal(&self, p: &Project, _plan: &Plan) -> Result<ProjectMutationJournal> {
        #[cfg(unix)]
        if self.state.lock().unwrap().force_partial_restore_failure {
            return ProjectMutationJournal::capture(
                &p.root,
                [
                    camino::Utf8Path::new("restored/state"),
                    camino::Utf8Path::new("blocked/state"),
                ],
            );
        }
        if self.state.lock().unwrap().write_lock_on_apply {
            return ProjectMutationJournal::capture(&p.root, [camino::Utf8Path::new("fake.lock")]);
        }
        ProjectMutationJournal::capture(&p.root, std::iter::empty::<&camino::Utf8Path>())
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        let (p, plan, _) = self.mutation_parts(mutation)?;
        let mut state = self.state.lock().unwrap();
        state.apply_attempted = true;
        state.apply_attempts += 1;
        if self.root.join("use-isolated-mutation").exists() {
            if p.root == self.root {
                return Err(CoreError::System(
                    "isolated fake resolver received the source project".to_string(),
                ));
            }
            if std::fs::read_to_string(self.root.join("Cargo.lock"))? != "source lock" {
                return Err(CoreError::LockConflict(
                    "source lock changed before isolated fake resolution completed".to_string(),
                ));
            }
            std::fs::write(p.root.join("Cargo.lock"), "accepted lock")?;
        }
        let recorder = p.root.join("record-apply-batches");
        if recorder.exists() {
            let mut recorder = std::fs::OpenOptions::new().append(true).open(recorder)?;
            writeln!(
                recorder,
                "{}",
                plan.changes
                    .iter()
                    .map(|change| change.package.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            )?;
        }
        if state.write_lock_on_apply {
            std::fs::write(p.root.join("fake.lock"), b"mutated lock")?;
        }
        #[cfg(unix)]
        if state.force_partial_restore_failure {
            std::fs::write(p.root.join("restored/state"), b"candidate")?;
            std::fs::write(p.root.join("blocked/state"), b"candidate")?;
        }
        if self.inject_fresh_on_apply {
            state.fresh_transitive_present = true;
        }
        // The `hold-not-eligible` marker lists package names (one per line) the fake refuses to
        // move, reported as `NotEligible` skips — the catalog-managed / no-editable-requirement
        // shape the pnpm and npm adapters produce.
        let held_not_eligible: Vec<String> =
            std::fs::read_to_string(self.root.join("hold-not-eligible"))
                .map(|contents| contents.lines().map(str::to_string).collect())
                .unwrap_or_default();
        let mut applied = Vec::new();
        let mut skipped = Vec::new();
        for change in &plan.changes {
            if held_not_eligible.contains(&change.package.name) {
                skipped.push(Skipped {
                    change: change.clone(),
                    reason: SkipReason::NotEligible,
                    offending: Some(change.package.clone()),
                    detail: Some(format!(
                        "{} is catalog-managed; update the catalog entry to move it",
                        change.package.name
                    )),
                });
                continue;
            }
            state
                .applied_versions
                .insert(change.package.name.clone(), change.to.clone());
            applied.push(change.clone());
        }
        // A whole-graph re-resolve can force packages the plan did not name to move for consistency.
        // Reflect those collateral moves into both the applied report and the graph state, so the
        // executor sees — and must surface — them exactly as the uv adapter does from its lock diff.
        // The `collateral-once` marker scopes the float to the first re-resolve only: a later fix
        // round's narrow downgrade batch does not re-drag the collateral, so a multi-round `fix`
        // can converge instead of ping-ponging against the float forever.
        if !(state.apply_attempts > 1 && self.root.join("collateral-once").exists()) {
            for collateral in &self.collateral_on_apply {
                state
                    .applied_versions
                    .insert(collateral.package.name.clone(), collateral.to.clone());
                applied.push(collateral.clone());
            }
        }
        Ok(ApplyReport {
            applied,
            skipped,
            edge_rebinds: self.edge_rebinds_on_apply.clone(),
            warnings: p
                .root
                .join("warn-on-apply")
                .exists()
                .then(|| {
                    Diagnostic::new(
                        DiagnosticKind::Filesystem,
                        "committed correction durability is uncertain",
                    )
                })
                .into_iter()
                .collect(),
        })
    }

    async fn apply_with_observer(
        &self,
        mutation: &PreparedMutation,
        _observer: &dyn ApplyObserver,
    ) -> Result<ApplyAttempt> {
        let (p, _, journal) = self.mutation_parts(mutation)?;
        let report = self.apply(mutation).await;
        if p.root.join("pending-recovery-on-apply").exists() {
            return Ok(mutation.pending_recovery_attempt(
                "fake adapter retained authoritative recovery evidence".to_string(),
            ));
        }
        let postimage = journal.capture_state()?;
        mutation.finished_attempt(report, &postimage)
    }

    async fn build(&self, p: &Project) -> Result<VerifyReport> {
        std::fs::write(p.root.join("build-invoked"), b"")?;
        Ok(VerifyReport {
            ok: !(self.build_fails_after_apply && self.state.lock().unwrap().apply_attempted),
            detail: if self.build_fails_after_apply && self.state.lock().unwrap().apply_attempted {
                "build failed".into()
            } else {
                "ok".into()
            },
        })
    }

    async fn normalize_lock_edges(
        &self,
        mutation: &PreparedMutation,
        _policy: EdgePolicy,
        before: Option<&[u8]>,
        committed: &[EdgeRebind],
    ) -> Result<cooldown_core::EdgeNormalizationReport> {
        let (project, _, _) = self.mutation_parts(mutation)?;
        if project.root.join("fail-edge-normalization").exists() {
            return Err(CoreError::PendingRecovery(
                "fake edge normalization retained recovery evidence".to_string(),
            ));
        }
        if before.is_none() {
            return Ok(cooldown_core::EdgeNormalizationReport::default());
        }
        Ok(cooldown_core::EdgeNormalizationReport {
            rebinds: committed
                .iter()
                .filter(|rebind| {
                    matches!(
                        rebind.action,
                        EdgeBindingAction::Restored | EdgeBindingAction::Canonicalized
                    )
                })
                .cloned()
                .collect(),
            warnings: Vec::new(),
        })
    }
}

fn workspace(fake: FakeEco, baseline: Baseline) -> Workspace {
    workspace_with_layers(fake, baseline, vec![builtin_default_layer()])
}

fn workspace_with_layers(fake: FakeEco, baseline: Baseline, layers: Vec<PolicyLayer>) -> Workspace {
    let project = fake.project();
    let ctx = ProjectCtx {
        tool: GO,
        project,
        rel_path: Utf8PathBuf::from("."),
        policy: PolicyStack {
            layers,
            strict_native: false,
        },
        edge_policy: EdgePolicy::default(),
    };
    let mut adapters = AdapterSet::new();
    std::assert_matches!(
        adapters.register_target_verified_mutator(Arc::new(fake)),
        Ok(())
    );
    Workspace::new(
        adapters,
        vec![ctx],
        now(),
        baseline,
        Utf8PathBuf::from("."),
        Vec::new(),
    )
}

struct UnknownLockFake(FakeEco);

impl UnknownLockFake {
    fn project(&self) -> Project {
        self.0.project()
    }
}

#[async_trait]
impl ToolRead for UnknownLockFake {
    fn id(&self) -> ToolId {
        self.0.id()
    }

    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    fn project_detection(&self) -> cooldown_core::ProjectDetection {
        self.0.project_detection()
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        self.0.dependencies(project, scope).await
    }

    async fn native_policy(&self, project: &Project) -> Result<Option<NativePolicyLayer>> {
        self.0.native_policy(project).await
    }

    async fn verify_lock_current(&self, _project: &Project) -> Result<LockVerifyReport> {
        Ok(LockVerifyReport {
            status: LockStatus::Unknown,
            detail: "fake lock currency is unknown".into(),
        })
    }
}

#[async_trait]
impl ReleaseFetcher for UnknownLockFake {
    async fn releases(
        &self,
        dep: &Dependency,
        fetch: &FetchContext<'_>,
        candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        self.0.releases(dep, fetch, candidates).await
    }

    async fn locked_release(&self, dep: &Dependency, fetch: &FetchContext<'_>) -> Result<Release> {
        self.0.locked_release(dep, fetch).await
    }
}

#[async_trait]
impl ToolWrite for UnknownLockFake {
    fn mutation_tool(&self) -> ToolId {
        GO
    }

    async fn mutation_journal(&self, p: &Project, plan: &Plan) -> Result<ProjectMutationJournal> {
        self.0.mutation_journal(p, plan).await
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        self.0.apply(mutation).await
    }

    async fn build(&self, p: &Project) -> Result<VerifyReport> {
        self.0.build(p).await
    }
}

fn unknown_lock_workspace(fake: FakeEco, baseline: Baseline) -> Workspace {
    let fake = UnknownLockFake(fake);
    let project = fake.project();
    let ctx = ProjectCtx {
        tool: GO,
        project,
        rel_path: Utf8PathBuf::from("."),
        policy: PolicyStack {
            layers: vec![builtin_default_layer()],
            strict_native: false,
        },
        edge_policy: EdgePolicy::default(),
    };
    let mut adapters = AdapterSet::new();
    std::assert_matches!(
        adapters.register_target_verified_mutator(Arc::new(fake)),
        Ok(())
    );
    Workspace::new(
        adapters,
        vec![ctx],
        now(),
        baseline,
        Utf8PathBuf::from("."),
        Vec::new(),
    )
}

fn opts() -> RunOpts {
    RunOpts {
        concurrency: 4,
        ..Default::default()
    }
}

/// A fresh temp-dir project root for one fake ecosystem.
struct TmpRoot {
    /// Owns the temporary directory; dropping it deletes the tree.
    guard: tempfile::TempDir,
    /// The directory's UTF-8 path, handed to the fake as its project root.
    root: Utf8PathBuf,
}

fn tmp_root() -> TmpRoot {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    TmpRoot { guard: dir, root }
}

fn fake(
    root: Utf8PathBuf,
    direct: Vec<Dependency>,
    transitive: Vec<Dependency>,
    releases: HashMap<String, Vec<Release>>,
    locked: HashMap<String, Release>,
) -> FakeEco {
    FakeEco {
        direct,
        transitive,
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    }
}

#[tokio::test]
async fn mutation_recovery_precedes_dependency_discovery() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut adapter = fake(
        root.clone(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
    );
    adapter
        .state
        .get_mut()
        .expect("exclusive fake state")
        .require_recovery_before_read = true;

    let outcome = workspace(adapter, Baseline::default())
        .upgrade(&opts())
        .await;

    assert!(outcome.errors.is_empty());
    // An empty error list alone would also pass if the pipeline never reached discovery at all;
    // the marker proves the recovery hook genuinely ran (and therefore ran before the discovery
    // that would otherwise have errored).
    assert!(
        root.join("recovery-invoked").exists(),
        "the mutation recovery hook must run during a real upgrade"
    );
}

#[tokio::test]
async fn dry_run_refuses_pending_source_state_without_recovering_it() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut adapter = fake(
        root.clone(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
    );
    adapter
        .state
        .get_mut()
        .expect("exclusive fake state")
        .require_recovery_before_read = true;

    let outcome = workspace(adapter, Baseline::default())
        .upgrade(&RunOpts {
            dry_run: true,
            ..opts()
        })
        .await;

    assert_eq!(outcome.errors.len(), 1);
    // The refusal must be the adapter's own pending-mutation diagnostic — not some incidental
    // failure that happens to leave one error behind.
    let error = outcome.errors.first().expect("one refusal diagnostic");
    assert_eq!(error.kind, DiagnosticKind::StaleLock);
    assert!(
        error.message.contains("pending fake mutation"),
        "the refusal must surface the adapter's pending-mutation detail, got: {}",
        error.message
    );
    assert!(outcome.items.is_empty());
    // Refusing means refusing: the read-side dry run must never invoke the mutating recovery hook.
    assert!(
        !root.join("recovery-invoked").exists(),
        "--dry-run must not recover the pending state it refuses"
    );
}

/// The non-Git coordination fallback (docs/content/docs/security.md): a project with no Git
/// metadata anywhere above it must still coordinate its mutation lifecycle, rendezvousing in an
/// owner-private `.cooldown/locks` directory under the canonical project root instead of a Git
/// common directory. The Git-anchored default fixture is the in-test contrast: the same pipeline
/// leaves its lease state under `.git/cooldown/locks` and never creates `.cooldown`.
#[tokio::test]
async fn non_git_project_coordinates_under_repo_root_cooldown_locks() {
    let fixture = support::Fixture::new_non_git();
    let root =
        Utf8PathBuf::from_path_buf(fixture.root().to_path_buf()).expect("UTF-8 fixture root");
    assert!(
        !root.join(".git").exists(),
        "the opt-out fixture must carry no Git anchor"
    );

    let outcome = workspace(
        edge_reporting_fake(root.clone(), Vec::new()),
        Baseline::default(),
    )
    .upgrade(&opts())
    .await;

    assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);
    assert_eq!(outcome.summary.applied, 1);
    // Coordination hashes the canonical root (a symlinked temp dir resolves), so assert there.
    let canonical = Utf8PathBuf::from_path_buf(
        std::fs::canonicalize(fixture.root()).expect("canonicalize fixture root"),
    )
    .expect("UTF-8 canonical root");
    let locks = canonical.join(".cooldown/locks");
    assert!(
        locks.is_dir(),
        "a non-Git run must coordinate under the repo-root `.cooldown/locks` fallback"
    );
    assert!(
        locks.join(".maintenance.lock").is_file(),
        "the shared maintenance lock lives in the fallback namespace"
    );
    let project_lease = locks.join(format!(
        "{:016x}.lock",
        cooldown_core::fs::fnv1a_64(canonical.as_str())
    ));
    assert!(
        project_lease.is_file(),
        "the project lease must land in the fallback namespace: {project_lease}"
    );

    // Contrast: the default Git-anchored fixture rendezvouses in the Git common directory and
    // must not scatter a `.cooldown` directory into the working tree.
    let git_fixture = support::Fixture::new();
    let git_root =
        Utf8PathBuf::from_path_buf(git_fixture.root().to_path_buf()).expect("UTF-8 fixture root");
    let outcome = workspace(
        edge_reporting_fake(git_root, Vec::new()),
        Baseline::default(),
    )
    .upgrade(&opts())
    .await;
    assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);
    let git_canonical = Utf8PathBuf::from_path_buf(
        std::fs::canonicalize(git_fixture.root()).expect("canonicalize fixture root"),
    )
    .expect("UTF-8 canonical root");
    assert!(
        git_canonical.join(".git/cooldown/locks").is_dir(),
        "a Git-anchored run coordinates in the Git common directory"
    );
    assert!(
        !git_canonical.join(".cooldown").exists(),
        "a Git-anchored run must not create the non-Git fallback directory"
    );
}

#[tokio::test]
async fn isolated_staging_failure_preserves_completed_recovery_notice() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("Cargo.lock"), "source lock")?;
    std::fs::write(root.join("use-isolated-mutation"), "")?;
    std::fs::write(root.join("fail-isolated-staging"), "")?;
    let mut adapter = fake(root, Vec::new(), Vec::new(), HashMap::new(), HashMap::new());
    adapter
        .state
        .get_mut()
        .map_err(|_| eyre::eyre!("fake state mutex poisoned"))?
        .require_recovery_before_read = true;

    let outcome = workspace(adapter, Baseline::default())
        .upgrade(&opts())
        .await;

    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.kind == DiagnosticKind::Recovery)
    );
    assert!(
        outcome
            .errors
            .iter()
            .any(|error| error.message.contains("injected isolated staging failure"))
    );
    Ok(())
}

#[tokio::test]
async fn restore_conflict_stops_before_a_later_lock_batch() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("fake.lock"), b"original lock")?;
    std::fs::write(root.join("record-apply-batches"), b"")?;
    let runtime = dep("runtime", "1.0.0", true);
    let build_tool = dep("build-tool", "1.0.0", true);
    let mut adapter = fake(
        root.clone(),
        vec![runtime],
        Vec::new(),
        HashMap::from([
            (
                "runtime".to_string(),
                vec![
                    rel("1.0.0", 1, Some("2025-01-01T00:00:00Z"), None),
                    rel(
                        "1.1.0",
                        2,
                        Some("2025-02-01T00:00:00Z"),
                        Some(UpdateKind::Minor),
                    ),
                ],
            ),
            (
                "build-tool".to_string(),
                vec![
                    rel("1.0.0", 1, Some("2025-01-01T00:00:00Z"), None),
                    rel(
                        "1.1.0",
                        2,
                        Some("2025-02-01T00:00:00Z"),
                        Some(UpdateKind::Minor),
                    ),
                ],
            ),
        ]),
        HashMap::from([
            (
                "runtime".to_string(),
                rel("1.0.0", 1, Some("2025-01-01T00:00:00Z"), None),
            ),
            (
                "build-tool".to_string(),
                rel("1.0.0", 1, Some("2025-01-01T00:00:00Z"), None),
            ),
        ]),
    );
    adapter.fail_graph_after_apply = true;
    // The manifest-only batch mutates first, then a simulated editor changes its lock before the
    // failing graph probe asks cooldown to roll the batch back.
    let state = adapter
        .state
        .get_mut()
        .map_err(|_| eyre::eyre!("fake state mutex poisoned"))?;
    state.manifest_constraints = vec![build_tool];
    state.drift_lock_before_graph_failure = true;
    state.write_lock_on_apply = true;

    let outcome = workspace(adapter, Baseline::default())
        .upgrade(&opts())
        .await;

    // The editor's bytes survive and the later runtime lock batch never reaches the adapter.
    assert!(!outcome.errors.is_empty());
    assert_eq!(
        std::fs::read_to_string(root.join("record-apply-batches"))?,
        "build-tool\n"
    );
    assert_eq!(std::fs::read(root.join("fake.lock"))?, b"external edit");
    Ok(())
}

#[tokio::test]
async fn isolated_mutation_publishes_once_then_builds_the_source() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("Cargo.lock"), "source lock")?;
    std::fs::write(root.join("use-isolated-mutation"), "")?;
    let adapter = fake(
        root.clone(),
        vec![dep("a", "v1.0.0", true)],
        Vec::new(),
        HashMap::from([(
            "a".to_string(),
            vec![
                rel("v1.0.0", 1, Some("2025-01-01T00:00:00Z"), None),
                rel(
                    "v1.1.0",
                    2,
                    Some("2025-02-01T00:00:00Z"),
                    Some(UpdateKind::Minor),
                ),
            ],
        )]),
        HashMap::from([(
            "a".to_string(),
            rel("v1.0.0", 1, Some("2025-01-01T00:00:00Z"), None),
        )]),
    );
    let mut options = opts();
    options.build = true;

    let outcome = workspace(adapter, Baseline::default())
        .upgrade(&options)
        .await;

    assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);
    assert_eq!(
        std::fs::read_to_string(root.join("Cargo.lock"))?,
        "accepted lock"
    );
    assert!(root.join("build-invoked").exists());
    assert_eq!(outcome.summary.applied, 1);
    Ok(())
}

#[tokio::test]
async fn published_pending_recovery_keeps_applied_rows_and_suppresses_build() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("Cargo.lock"), "source lock")?;
    std::fs::write(root.join("use-isolated-mutation"), "")?;
    std::fs::write(root.join("publish-pending-recovery"), "")?;
    let adapter = fake(
        root.clone(),
        vec![dep("a", "v1.0.0", true)],
        Vec::new(),
        HashMap::from([(
            "a".to_string(),
            vec![
                rel("v1.0.0", 1, Some("2025-01-01T00:00:00Z"), None),
                rel(
                    "v1.1.0",
                    2,
                    Some("2025-02-01T00:00:00Z"),
                    Some(UpdateKind::Minor),
                ),
            ],
        )]),
        HashMap::from([(
            "a".to_string(),
            rel("v1.0.0", 1, Some("2025-01-01T00:00:00Z"), None),
        )]),
    );
    let mut options = opts();
    options.build = true;

    let outcome = workspace(adapter, Baseline::default())
        .upgrade(&options)
        .await;

    assert_eq!(
        std::fs::read_to_string(root.join("Cargo.lock"))?,
        "accepted lock"
    );
    assert!(!root.join("build-invoked").exists());
    assert_eq!(outcome.summary.applied, 1);
    assert!(
        outcome
            .errors
            .iter()
            .any(|error| error.kind == DiagnosticKind::PendingRecovery)
    );
    Ok(())
}

#[tokio::test]
async fn pending_adapter_recovery_prevents_outer_rollback_and_build() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("fake.lock"), b"original lock")?;
    std::fs::write(root.join("pending-recovery-on-apply"), b"")?;
    let mut adapter = fake(
        root.clone(),
        vec![dep("a", "v1.0.0", true)],
        Vec::new(),
        HashMap::from([(
            "a".to_string(),
            vec![
                rel("v1.0.0", 1, Some("2025-01-01T00:00:00Z"), None),
                rel(
                    "v1.1.0",
                    2,
                    Some("2025-02-01T00:00:00Z"),
                    Some(UpdateKind::Minor),
                ),
            ],
        )]),
        HashMap::from([(
            "a".to_string(),
            rel("v1.0.0", 1, Some("2025-01-01T00:00:00Z"), None),
        )]),
    );
    adapter
        .state
        .get_mut()
        .map_err(|_| eyre::eyre!("fake state mutex poisoned"))?
        .write_lock_on_apply = true;
    let mut options = opts();
    options.build = true;

    let outcome = workspace(adapter, Baseline::default())
        .upgrade(&options)
        .await;

    assert_eq!(std::fs::read(root.join("fake.lock"))?, b"mutated lock");
    assert!(!root.join("build-invoked").exists());
    assert_eq!(outcome.summary.applied, 0);
    assert!(
        outcome
            .errors
            .iter()
            .chain(outcome.items.iter().filter_map(|item| item.error.as_ref()))
            .any(|error| error.kind == DiagnosticKind::PendingRecovery)
    );
    Ok(())
}

#[tokio::test]
async fn edge_normalization_failure_stops_before_final_build() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("edge.snapshot"), b"run-start edge state")?;
    std::fs::write(root.join("fail-edge-normalization"), b"")?;
    let mut options = opts();
    options.build = true;

    let outcome = workspace(
        fake(
            root.clone(),
            Vec::new(),
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
        ),
        Baseline::default(),
    )
    .upgrade(&options)
    .await;

    assert!(!root.join("build-invoked").exists());
    assert!(
        outcome
            .errors
            .iter()
            .any(|error| error.kind == DiagnosticKind::PendingRecovery)
    );
    Ok(())
}

fn too_fresh_fix_releases() -> Vec<Release> {
    vec![
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
        rel(
            "v1.0.1",
            1,
            Some("2026-06-01T00:00:00Z"),
            Some(UpdateKind::Patch),
        ),
        rel(
            "v1.0.2",
            2,
            Some("2026-06-16T00:00:00Z"),
            Some(UpdateKind::Patch),
        ),
    ]
}

fn release_named(releases: &[Release], version: &str) -> Release {
    releases
        .iter()
        .find(|release| release.version == Version::new(version))
        .unwrap()
        .clone()
}

#[tokio::test]
async fn outdated_splits_adoptable_and_in_cooldown() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    // `a`: the newest (v1.2.0) is still cooling, but v1.1.0 has matured → adoptable (you can update).
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ), // mature
            rel(
                "v1.2.0",
                2,
                Some("2026-06-16T00:00:00Z"),
                Some(UpdateKind::Minor),
            ), // fresh
        ],
    );
    // `b`: the only newer version is fresh and nothing has matured → in cooldown (cannot update yet).
    releases.insert(
        "b".to_string(),
        vec![
            rel("v2.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v2.1.0",
                1,
                Some("2026-06-16T00:00:00Z"),
                Some(UpdateKind::Minor),
            ), // fresh
        ],
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true), dep("b", "v2.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.outdated(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.items.len(), 2);
    let a = out.items.iter().find(|i| i.name == "a").expect("a");
    let b = out.items.iter().find(|i| i.name == "b").expect("b");
    // `a` has a matured version, so it is adoptable even though its newest is still cooling.
    assert_eq!(a.status, OutdatedStatus::Adoptable);
    assert_eq!(a.adoptable_target.as_deref(), Some("v1.1.0"));
    assert_eq!(a.latest.as_ref().unwrap().version, "v1.2.0");
    assert_eq!(a.candidate_age_days, Some(1.0));
    // `b` has nothing matured, so it genuinely cannot update yet.
    assert_eq!(b.status, OutdatedStatus::InCooldown);
    assert_eq!(b.adoptable_target, None);
    assert_eq!(b.candidate_age_days, Some(1.0));
    assert_eq!(out.summary.adoptable, 1);
    assert_eq!(out.summary.in_cooldown, 1);
}

#[tokio::test]
async fn outdated_countdown_tracks_latest_or_soonest_maturing() {
    // The ruff scenario: locked at 0.15.15 with three newer patches under the default 7-day window
    // (now = 2026-06-17, cutoff 2026-06-10). 0.15.16 has matured (adoptable); 0.15.17 and 0.15.18 are
    // still cooling. 0.15.18 is the freshest (newest), but 0.15.17 unlocks three days sooner.
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "ruff".to_string(),
        vec![
            rel("0.15.15", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "0.15.16",
                1,
                Some("2026-06-05T00:00:00Z"),
                Some(UpdateKind::Patch),
            ), // matured
            rel(
                "0.15.17",
                2,
                Some("2026-06-13T00:00:00Z"),
                Some(UpdateKind::Patch),
            ), // cooling, matures soonest
            rel(
                "0.15.18",
                3,
                Some("2026-06-16T00:00:00Z"),
                Some(UpdateKind::Patch),
            ), // cooling, the newest
        ],
    );
    let make = || {
        workspace(
            fake(
                root.clone(),
                vec![dep("ruff", "0.15.15", true)],
                vec![],
                releases.clone(),
                HashMap::new(),
            ),
            Baseline::default(),
        )
    };

    // Latest horizon (explicit, now that `soonest` is the default): the Cooldown column tracks the
    // freshest version, 0.15.18 (age 1d). It needs no version label because that is exactly what the
    // Latest column already shows.
    let latest_opts = RunOpts {
        cooldown_horizon: CooldownHorizon::Latest,
        ..opts()
    };
    let latest = make().outdated(&latest_opts).await;
    let item = latest
        .items
        .iter()
        .find(|i| i.name == "ruff")
        .expect("ruff");
    assert_eq!(item.status, OutdatedStatus::Adoptable);
    assert_eq!(item.adoptable_target.as_deref(), Some("0.15.16"));
    assert_eq!(item.latest.as_ref().unwrap().version, "0.15.18");
    assert_eq!(item.candidate_age_days, Some(1.0));
    assert_eq!(item.cooldown_version, None);

    // Soonest horizon: the Cooldown column tracks 0.15.17 (age 4d) — the next version to mature —
    // while adoptable/latest are unchanged, because the choice is display-only. Because 0.15.17 is
    // not the latest version, it is labelled so the cell reads `4d/7d (0.15.17)`.
    let soonest_opts = RunOpts {
        cooldown_horizon: CooldownHorizon::Soonest,
        ..opts()
    };
    let soonest = make().outdated(&soonest_opts).await;
    let item = soonest
        .items
        .iter()
        .find(|i| i.name == "ruff")
        .expect("ruff");
    assert_eq!(item.status, OutdatedStatus::Adoptable);
    assert_eq!(item.adoptable_target.as_deref(), Some("0.15.16"));
    assert_eq!(item.latest.as_ref().unwrap().version, "0.15.18");
    assert_eq!(item.candidate_age_days, Some(4.0));
    assert_eq!(item.cooldown_version.as_deref(), Some("0.15.17"));
}

#[tokio::test]
async fn outdated_default_view_never_labels_even_with_an_unclassifiable_newest() {
    // Regression: the default (`latest`) view must never append a `(version)` label. Here the newest
    // eligible release 0.15.18 is unclassifiable (`kind_from_current = None`), so it is `verdict.latest`
    // yet never becomes a candidate; the shown candidate is the next one down, 0.15.17. The label is
    // suppressed by comparing the shown version against the newest *candidate* (not `verdict.latest`),
    // so the cell stays bare — byte-identical to the pre-feature output.
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "ruff".to_string(),
        vec![
            rel("0.15.15", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "0.15.16",
                1,
                Some("2026-06-05T00:00:00Z"),
                Some(UpdateKind::Patch),
            ), // adoptable
            rel(
                "0.15.17",
                2,
                Some("2026-06-13T00:00:00Z"),
                Some(UpdateKind::Patch),
            ), // cooling, the newest *candidate*
            rel("0.15.18", 3, Some("2026-06-16T00:00:00Z"), None), // newest eligible, but unclassifiable → skipped as a candidate
        ],
    );
    let ws = workspace(
        fake(
            root,
            vec![dep("ruff", "0.15.15", true)],
            vec![],
            releases,
            HashMap::new(),
        ),
        Baseline::default(),
    );
    let out = ws.outdated(&opts()).await;
    let item = out.items.iter().find(|i| i.name == "ruff").expect("ruff");
    // `latest` reports the unclassifiable newest; the cooldown tracks the newest candidate, unlabelled.
    assert_eq!(item.latest.as_ref().unwrap().version, "0.15.18");
    assert_eq!(item.candidate_age_days, Some(4.0));
    assert_eq!(
        item.cooldown_version, None,
        "the default view must not label the cooldown version"
    );
}

#[tokio::test]
async fn upgrade_dedupes_a_planned_transitive_with_its_mvs_collateral_while_fix_leaves_it() {
    // `upgrade` plans the whole resolved graph, so the indirect `t` is a first-class candidate for
    // its newest matured release — and the same move also arrives as adapter collateral when the
    // direct `a` bump drags it (MVS promotion). The two provenances must dedupe into one applied
    // row, never double-report. `fix` (downgrade-only) never advances anything.
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v2.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            // Newer and matured past the 7-day window (cutoff 2026-06-10).
            rel(
                "v2.0.1",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Patch),
            ),
        ],
    );
    releases.insert(
        "t".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            // The indirect's newer release, also matured; carried up by the `a` bump, not planned.
            rel(
                "v1.0.1",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Patch),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v2.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert(
        "t".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );

    // The indirect `t` is dragged from v1.0.0 to v1.0.1 by the whole-graph re-resolve when `a` moves.
    let collateral_t = Change {
        package: PackageId::new(GO, "t", Some("proxy.example".into())),
        from: Version::new("v1.0.0"),
        to: Version::new("v1.0.1"),
        kind: UpdateKind::Patch,
        downgrade: false,
        direct: false,
        members: Vec::new(),
    };

    let make = || {
        let mut eco = fake(
            root.clone(),
            vec![dep("a", "v2.0.0", true)],
            vec![dep("t", "v1.0.0", false)],
            releases.clone(),
            locked.clone(),
        );
        eco.collateral_on_apply = vec![collateral_t.clone()];
        workspace(eco, Baseline::default())
    };

    // `fix` never moves a dep forward — both pins are already matured, so nothing is touched.
    let fixed = make().fix(&opts()).await;
    assert_eq!(fixed.summary.applied, 0);
    assert!(fixed.items.is_empty());

    // `upgrade` plans both; `t`'s planned advance and its MVS-collateral report collapse into one
    // applied row (never silent, never doubled).
    let upgraded = make().upgrade(&opts()).await;
    assert_eq!(upgraded.summary.applied, 2);
    let a = upgraded
        .items
        .iter()
        .find(|item| item.name == "a")
        .expect("a advanced");
    assert_eq!(a.to, "v2.0.1");
    let t = upgraded
        .items
        .iter()
        .find(|item| item.name == "t")
        .expect("t carried forward as collateral");
    assert_eq!(t.to, "v1.0.1");
    assert!(!t.downgrade);
}

#[tokio::test]
async fn upgrade_advances_a_matured_in_range_transitive_no_direct_pin_drags() {
    // The quinn-proto/dompurify class: a transitive's newer matured release exists, but no direct
    // candidate drags it, and a resolver promotes only the minimum a requirement forces — without
    // graph-wide planning it sits unadopted forever.
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![rel("v2.0.0", 0, Some("2026-01-01T00:00:00Z"), None)],
    );
    releases.insert(
        "t".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.0.1",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Patch),
            ),
        ],
    );
    // A transitive capped by an exact requirer pin (graph ceiling at its current version): the
    // resolver would reject the advance, so it must not even be planned.
    releases.insert(
        "capped".to_string(),
        vec![
            rel("v3.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v3.0.1",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Patch),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v2.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert(
        "t".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert(
        "capped".to_string(),
        rel("v3.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let mut capped = dep("capped", "v3.0.0", false);
    capped.graph_ceiling = Some(Version::new("v3.0.0"));
    let make = || {
        workspace(
            fake(
                root.clone(),
                vec![dep("a", "v2.0.0", true)],
                vec![dep("t", "v1.0.0", false), capped.clone()],
                releases.clone(),
                locked.clone(),
            ),
            Baseline::default(),
        )
    };

    let out = make().upgrade(&opts()).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 1, "only the free transitive advances");
    let t = out
        .items
        .iter()
        .find(|item| item.name == "t")
        .expect("the transitive is planned and applied on its own");
    assert_eq!(t.to, "v1.0.1");
    assert!(!t.direct);
    assert!(
        !out.items
            .iter()
            .any(|item| item.name == "capped" && item.applied),
        "a ceiling-capped transitive is not advanced"
    );

    // `--transitive hide` narrows upgrade planning to direct deps: nothing to do here.
    let mut hide = opts();
    hide.transitive_mode = cooldown::app::TransitiveGate::Hide;
    let out = make().upgrade(&hide).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert!(out.items.is_empty());

    // An engine that cannot pin an undeclared package (`supports_transitive_advance` false, the
    // npm/yarn/bun family) keeps direct-only upgrade planning even under the default gate —
    // planning the candidate would only produce a not-eligible skip per run.
    std::fs::write(root.join("no-transitive-advance"), b"").expect("write capability marker");
    let out = make().upgrade(&opts()).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert!(
        out.items.is_empty(),
        "a capability-less engine must not plan transitives: {:?}",
        out.items
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>()
    );
    std::fs::remove_file(root.join("no-transitive-advance")).expect("remove capability marker");
}

#[tokio::test]
async fn outdated_transitive_scopes_in_indirect_deps() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    // Both a direct and a transitive dep have a matured newer version → both are adoptable.
    for name in ["a", "t"] {
        releases.insert(
            name.to_string(),
            vec![
                rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
                rel(
                    "v1.1.0",
                    1,
                    Some("2026-06-01T00:00:00Z"),
                    Some(UpdateKind::Minor),
                ),
            ],
        );
    }
    let make = || {
        let fake = FakeEco {
            direct: vec![dep("a", "v1.0.0", true)],
            transitive: vec![dep("t", "v1.0.0", false)],
            fresh_transitive: None,
            releases: releases.clone(),
            locked: HashMap::new(),
            inject_fresh_on_apply: false,
            collateral_on_apply: Vec::new(),
            edge_rebinds_on_apply: Vec::new(),
            stale_lock: false,
            fail_graph_after_apply: false,
            fail_locked_release_after_apply_for: None,
            stale_lock_after_apply: false,
            build_fails_after_apply: false,
            confirm_strips: None,
            state: Mutex::new(State::default()),
            root: root.clone(),
        };
        workspace(fake, Baseline::default())
    };

    // Default: direct-only — the transitive dep is not in the report.
    let out = make().outdated(&opts()).await;
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0].name, "a");

    // `--transitive`: the indirect dep is scoped in too.
    let mut transitive = opts();
    transitive.transitive = true;
    let out = make().outdated(&transitive).await;
    assert_eq!(out.items.len(), 2);
    assert!(out.items.iter().any(|item| item.name == "t"));
}

#[tokio::test]
async fn per_tool_exclude_prunes_workspace_member_dependencies() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut kept = dep("kept", "v1.0.0", true);
    kept.members = vec![MemberRef {
        name: "kept-app".into(),
        path: "apps/kept".into(),
    }];
    let mut dropped = dep("dropped", "v1.0.0", true);
    dropped.members = vec![MemberRef {
        name: "dropped-app".into(),
        path: "apps/dropped".into(),
    }];
    let mut releases = HashMap::new();
    for name in ["kept", "dropped"] {
        releases.insert(
            name.to_string(),
            vec![
                rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
                rel(
                    "v1.1.0",
                    1,
                    Some("2026-06-01T00:00:00Z"),
                    Some(UpdateKind::Minor),
                ),
            ],
        );
    }
    let fake = FakeEco {
        direct: vec![kept, dropped],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::default(),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let mut opts = opts();
    opts.exclude_folders_by_tool
        .insert(GO.as_str().to_string(), vec!["apps/dropped".to_string()]);

    let out = ws.outdated(&opts).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(
        out.items
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["kept"]
    );
    assert_eq!(out.items[0].members[0].path, "apps/kept");
}

#[tokio::test]
async fn per_tool_exclude_packages_prunes_workspace_member_dependencies() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut kept = dep("kept", "v1.0.0", true);
    kept.members = vec![MemberRef {
        name: "@app/kept".into(),
        path: "apps/kept".into(),
    }];
    let mut dropped = dep("dropped", "v1.0.0", true);
    dropped.members = vec![MemberRef {
        name: "@internal/dropped".into(),
        path: "apps/dropped".into(),
    }];
    let mut releases = HashMap::new();
    for name in ["kept", "dropped"] {
        releases.insert(
            name.to_string(),
            vec![
                rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
                rel(
                    "v1.1.0",
                    1,
                    Some("2026-06-01T00:00:00Z"),
                    Some(UpdateKind::Minor),
                ),
            ],
        );
    }
    let fake = FakeEco {
        direct: vec![kept, dropped],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::default(),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let mut opts = opts();
    // `@internal/*` matches the member's package NAME (`@internal/dropped`); it does NOT match the
    // member's path (`apps/dropped`), so this proves exclusion is name-based, not path-based. Keyed
    // by the canonical tool id.
    opts.exclude_packages_by_tool
        .insert(GO.as_str().to_string(), vec!["@internal/*".to_string()]);

    let out = ws.outdated(&opts).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(
        out.items
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["kept"]
    );
    assert_eq!(out.items[0].members[0].name, "@app/kept");
}

#[tokio::test]
async fn global_exclude_packages_prunes_workspace_member_dependencies() {
    // Coverage for the global/command `opts.exclude_packages` branch (set from `[global]`/
    // `[<command>]` or `--exclude-packages`), distinct from the per-tool map: `dependencies_in_scope`
    // seeds its package matcher from `opts.exclude_packages` before extending with the per-tool list.
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut kept = dep("kept", "v1.0.0", true);
    kept.members = vec![MemberRef {
        name: "@app/kept".into(),
        path: "apps/kept".into(),
    }];
    let mut dropped = dep("dropped", "v1.0.0", true);
    dropped.members = vec![MemberRef {
        name: "@internal/dropped".into(),
        path: "apps/dropped".into(),
    }];
    let mut releases = HashMap::new();
    for name in ["kept", "dropped"] {
        releases.insert(
            name.to_string(),
            vec![
                rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
                rel(
                    "v1.1.0",
                    1,
                    Some("2026-06-01T00:00:00Z"),
                    Some(UpdateKind::Minor),
                ),
            ],
        );
    }
    let fake = FakeEco {
        direct: vec![kept, dropped],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::default(),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let mut opts = opts();
    opts.exclude_packages = vec!["@internal/*".to_string()];

    let out = ws.outdated(&opts).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(
        out.items
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["kept"]
    );
}

#[tokio::test]
async fn check_flags_fresh_transitive_and_baseline_acknowledges() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    ); // mature
    locked.insert(
        "t".to_string(),
        rel("v0.5.0", 0, Some("2026-06-16T00:00:00Z"), None),
    ); // fresh → violation

    let make = || FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![dep("t", "v0.5.0", false)],
        fresh_transitive: None,
        releases: HashMap::new(),
        locked: locked.clone(),
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root: root.clone(),
    };

    // Without a baseline → the fresh transitive is a violation, exit 1.
    let ws = workspace(make(), Baseline::default());
    let out = ws.check(&opts()).await;
    assert_eq!(out.exit, Exit::Policy);
    assert_eq!(out.summary.violations, 1);
    assert_eq!(out.summary.checked, 2);
    assert_eq!(out.summary.direct, 1);
    assert_eq!(out.items[0].name, "t");
    assert_eq!(out.items[0].status, CheckStatus::Violation);

    // With an exact-scope baseline entry → acknowledged, exit 0.
    let baseline = Baseline {
        entries: vec![cooldown::app::baseline::AckEntry {
            tool: "go".into(),
            project: ".".into(),
            package: "t".into(),
            version: "v0.5.0".into(),
            registry: Some("proxy.example".into()),
            published_at: None,
            window_days: Some(7.0),
            reason: None,
            until: None,
        }],
    };
    let ws = workspace(make(), baseline);
    let out = ws.check(&opts()).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.violations, 0);
    assert_eq!(out.summary.acknowledged, 1);
}

#[tokio::test]
async fn check_transitive_allow_and_hide_modes() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    ); // direct, mature
    locked.insert(
        "t".to_string(),
        rel("v0.5.0", 0, Some("2026-06-16T00:00:00Z"), None),
    ); // transitive, fresh → would be a violation

    let make = || FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![dep("t", "v0.5.0", false)],
        fresh_transitive: None,
        releases: HashMap::new(),
        locked: locked.clone(),
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root: root.clone(),
    };

    // `--transitive allow`: the fresh transitive is still evaluated and reported, but as a non-fatal
    // `allowed` finding (distinct from a baselined `acknowledged`), so the gate passes.
    let mut allow = opts();
    allow.transitive_mode = cooldown::app::TransitiveGate::Allow;
    let out = workspace(make(), Baseline::default()).check(&allow).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.violations, 0);
    assert_eq!(out.summary.allowed, 1);
    assert_eq!(out.summary.acknowledged, 0);
    assert_eq!(out.summary.checked, 2, "the transitive is still evaluated");
    let allowed_item = out
        .items
        .iter()
        .find(|item| item.name == "t")
        .expect("the fresh transitive is reported");
    assert_eq!(allowed_item.status, CheckStatus::Allowed);

    // `--transitive hide`: the transitive is not evaluated at all (direct-only), gate passes.
    let mut hide = opts();
    hide.transitive_mode = cooldown::app::TransitiveGate::Hide;
    let out = workspace(make(), Baseline::default()).check(&hide).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.violations, 0);
    assert_eq!(out.summary.allowed, 0);
    assert_eq!(out.summary.acknowledged, 0);
    assert_eq!(out.summary.checked, 1, "only the direct dep is evaluated");
}

#[tokio::test]
async fn check_fails_closed_on_stale_lock() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases: HashMap::new(),
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: true,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.check(&opts()).await;
    assert_eq!(out.exit, Exit::Environment);
    assert_eq!(out.errors.len(), 1);
    assert_eq!(out.errors[0].kind, DiagnosticKind::StaleLock);

    let mut allowed = opts();
    allowed.allow_stale_lock = true;
    let out = ws.check(&allowed).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.checked, 0);
    assert_eq!(out.summary.skipped_stale_projects, 1);
    assert!(
        out.warnings
            .iter()
            .any(|warning| warning.message.contains("evaluation was skipped"))
    );
}

#[tokio::test]
async fn upgrade_applies_clean_change() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("warn-on-apply"), b"")?;
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.upgrade(&opts()).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 1);
    assert!(out.items[0].applied);
    assert_eq!(out.items[0].to, "v1.1.0");
    assert!(
        out.warnings
            .iter()
            .any(|warning| warning.message == "committed correction durability is uncertain")
    );
    Ok(())
}

#[tokio::test]
async fn upgrade_warns_when_final_lock_currency_is_unknown() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = unknown_lock_workspace(fake, Baseline::default());
    let out = ws.upgrade(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.summary.errors, 0);
    assert_eq!(out.meta.lock_status, Some(LockStatus::Unknown));
    assert!(
        out.warnings
            .iter()
            .any(|diag| diag.kind == DiagnosticKind::LockUnknown)
    );
}

#[tokio::test]
async fn upgrade_honors_allow_stale_lock_after_apply() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: true,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root: root.clone(),
    };
    let ws = workspace(fake, Baseline::default());
    let mut opts = opts();
    opts.allow_stale_lock = true;
    let out = ws.upgrade(&opts).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.summary.errors, 0);
    assert_eq!(out.meta.lock_status, Some(LockStatus::Stale));
    assert!(
        out.warnings
            .iter()
            .any(|diag| diag.kind == DiagnosticKind::StaleLock)
    );
}

#[tokio::test]
async fn upgrade_surfaces_a_forced_non_candidate_downgrade_never_silent() {
    // The reviewer fixture in adapter terms: upgrading the planned candidate `a` forces the
    // whole-graph re-resolve to move `b` — a package the plan never named — backward for consistency
    // (`b` is not a cooldown candidate; it is dragged along). That collateral move is part of the
    // committed lock, so it MUST appear in the report. The silent-non-candidate-downgrade bug was
    // exactly this move being applied to the lock yet omitted from the report, which let it ping-pong
    // back on the next run. Here the forced `b` downgrade is asserted to surface as its own applied row.
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    // `b` only has its current and an older release; the resolve forces it to the older one.
    releases.insert(
        "b".to_string(),
        vec![
            rel("v2.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel("v2.1.0", 1, Some("2026-06-01T00:00:00Z"), None),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert(
        "b".to_string(),
        rel("v2.1.0", 1, Some("2026-06-01T00:00:00Z"), None),
    );
    let forced_b_downgrade = Change {
        package: PackageId::new(GO, "b", Some("proxy.example".into())),
        from: Version::new("v2.1.0"),
        to: Version::new("v2.0.0"),
        kind: UpdateKind::Minor,
        downgrade: true,
        direct: false,
        members: Vec::new(),
    };
    let fake = FakeEco {
        // `b` is a transitive in the graph but never planned (its newest is already current, so the
        // upgrade planner does not move it); only the resolve's consistency requirement moves it.
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![dep("b", "v2.1.0", false)],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        collateral_on_apply: vec![forced_b_downgrade],
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.upgrade(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    // Both the planned `a` upgrade and the forced `b` downgrade are reported — nothing silent.
    let a = out
        .items
        .iter()
        .find(|item| item.name == "a")
        .expect("a row");
    assert!(a.applied);
    assert_eq!(a.to, "v1.1.0");
    let b = out
        .items
        .iter()
        .find(|item| item.name == "b")
        .expect("the forced non-candidate downgrade must be reported, never silent");
    assert!(b.applied);
    assert_eq!(b.from, "v2.1.0");
    assert_eq!(b.to, "v2.0.0");
    assert!(b.downgrade);
}

fn diesel_rebind(dependent_source: Option<String>, action: EdgeBindingAction) -> EdgeRebind {
    EdgeRebind {
        dependent: "diesel".to_string(),
        dependent_version: Version::new("2.3.11"),
        dependent_source,
        dependency: PackageId::new(GO, "uuid", Some("proxy.example".into())),
        from: Version::new("v0.8.2"),
        to: Version::new("v1.24.0"),
        action,
        detail: None,
    }
}

fn edge_reporting_fake(root: Utf8PathBuf, edge_rebinds_on_apply: Vec<EdgeRebind>) -> FakeEco {
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    }
}

fn assert_source_distinct_restorations(items: &[cooldown::app::UpgradeItem]) {
    let restored: Vec<_> = items.iter().filter(|item| item.name == "uuid").collect();
    assert_eq!(
        restored.len(),
        2,
        "source-distinct twins must not deduplicate"
    );
    let dependent_sources: std::collections::BTreeSet<_> = restored
        .iter()
        .map(|item| {
            item.edge
                .as_ref()
                .and_then(|edge| edge.dependent_source.as_deref())
        })
        .collect();
    assert_eq!(
        dependent_sources,
        std::collections::BTreeSet::from([
            Some("registry+https://github.com/rust-lang/crates.io-index"),
            Some("git+https://example.com/diesel#REDACTED"),
        ])
    );
    for restored in restored {
        assert!(
            restored.edge.is_some(),
            "restored row must carry an edge block"
        );
        let Some(edge) = restored.edge.as_ref() else {
            continue;
        };
        assert_eq!(edge.dependent, "diesel");
        assert_eq!(edge.dependent_version, "2.3.11");
        assert_eq!(edge.action, EdgeBindingAction::Restored);
        assert_eq!(restored.from, "v0.8.2");
        assert_eq!(restored.to, "v1.24.0");
        assert!(restored.applied, "a committed edge correction was applied");
        assert!(restored.skipped.is_none() && restored.error.is_none());
    }
}

fn incomplete_edge_fake(root: Utf8PathBuf, action: EdgeBindingAction) -> FakeEco {
    let mut rebind = diesel_rebind(None, action);
    rebind.detail =
        Some("rebinding away from uuid v0.8.2 would orphan its last lock reference".to_string());
    edge_reporting_fake(root, vec![rebind])
}

#[tokio::test]
async fn invalid_edge_outcomes_fail_at_the_apply_boundary() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let fake = edge_reporting_fake(root, vec![diesel_rebind(None, EdgeBindingAction::Held)]);

    let out = workspace(fake, Baseline::default()).upgrade(&opts()).await;

    assert_eq!(out.exit, Exit::Environment);
    assert_eq!(out.summary.errors, 1);
    assert!(out.items.iter().all(|item| item.edge.is_none()));
}

#[tokio::test]
async fn upgrade_surfaces_adapter_edge_rebinds_as_rows_beside_the_version_counts() {
    // An adapter's apply can report lock-edge binding moves beside the version changes.
    // Each rebind must surface as its own report row — carrying the dependent and the policy outcome
    // — while the version counts remain separate.
    let TmpRoot { guard: _g, root } = tmp_root();
    let fake = edge_reporting_fake(
        root,
        vec![
            diesel_rebind(
                Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
                EdgeBindingAction::Restored,
            ),
            diesel_rebind(
                Some("git+https://example.com/diesel#abcdef".to_string()),
                EdgeBindingAction::Restored,
            ),
            EdgeRebind {
                dependent: "arboard".to_string(),
                dependent_version: Version::new("3.6.1"),
                dependent_source: None,
                dependency: PackageId::new(GO, "windows-sys", Some("proxy.example".into())),
                from: Version::new("v0.60.2"),
                to: Version::new("v0.52.0"),
                action: EdgeBindingAction::Rebound,
                detail: None,
            },
        ],
    );
    let out = workspace(fake, Baseline::default()).upgrade(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.summary.skipped, 0);
    assert_eq!(out.summary.errors, 0);
    assert_eq!(out.summary.edges_corrected, 2);
    assert_eq!(out.summary.edges_rebound, 1);
    assert_eq!(out.summary.edges_held, 0);
    assert_eq!(out.summary.edges_unaddressable, 0);
    assert_source_distinct_restorations(&out.items);

    let rebound = out
        .items
        .iter()
        .find(|item| item.name == "windows-sys")
        .expect("the uncorrected rebind must surface too — never silent");
    assert_eq!(
        rebound.edge.as_ref().expect("edge block").action,
        EdgeBindingAction::Rebound
    );
    assert!(
        rebound.applied,
        "the resolver-produced binding was committed"
    );

    // Edge rows sort after the applied rows: footnotes to the version changes above them.
    // Unwrap both positions first: `None < Some(_)` holds for Options, so comparing them raw
    // would pass vacuously with the applied row missing entirely.
    let a_position = out
        .items
        .iter()
        .position(|item| item.name == "a")
        .expect("the applied version row for `a` must be present");
    let uuid_position = out
        .items
        .iter()
        .position(|item| item.name == "uuid")
        .expect("an edge rebind row for `uuid` must be present");
    assert!(a_position < uuid_position);
}

#[tokio::test]
async fn edge_report_redacts_source_secrets_at_the_app_boundary() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut rebind = diesel_rebind(
        Some(
            "git+https://token@example.com/diesel?branch=next&access_token=hidden#abcdef"
                .to_string(),
        ),
        EdgeBindingAction::Rebound,
    );
    rebind.dependency.registry =
        Some("git+https://user:secret@example.com/uuid?signature=signed#abcdef".into());
    rebind.detail = Some(
        "the binding moved to git+https://another-secret@example.com/uuid?token=hidden#abcdef"
            .to_string(),
    );
    let out = workspace(edge_reporting_fake(root, vec![rebind]), Baseline::default())
        .upgrade(&opts())
        .await;
    let row = out
        .items
        .iter()
        .find(|item| item.name == "uuid")
        .ok_or_else(|| eyre::eyre!("missing edge report row"))?;
    let edge = row
        .edge
        .as_ref()
        .ok_or_else(|| eyre::eyre!("missing edge report detail"))?;

    assert_eq!(
        edge.dependent_source.as_deref(),
        Some("git+https://example.com/diesel?branch=next&access_token=REDACTED#REDACTED")
    );
    assert_eq!(
        row.registry.as_deref(),
        Some("git+https://example.com/uuid?signature=REDACTED#REDACTED")
    );
    assert_eq!(
        edge.detail.as_deref(),
        Some("the binding moved to git+https://example.com/uuid?token=REDACTED#REDACTED")
    );
    Ok(())
}

#[tokio::test]
async fn upgrade_fails_strict_when_an_edge_policy_is_incomplete() {
    // A withheld target or an unaddressable corrective-policy move makes the mutation incomplete
    // under `--strict`; without `--strict` either remains a truthful row with its reason.
    let TmpRoot { guard: _g, root } = tmp_root();
    let ws = workspace(
        incomplete_edge_fake(root, EdgeBindingAction::Held),
        Baseline::default(),
    );
    let out = ws.upgrade(&opts()).await;
    assert_eq!(
        out.exit,
        Exit::Ok,
        "without --strict a held edge only reports"
    );
    assert_eq!(out.summary.edges_held, 1);
    assert_eq!(out.summary.edges_corrected, 0);
    assert_eq!(out.summary.edges_unaddressable, 0);
    let held = out
        .items
        .iter()
        .find(|item| item.edge.is_some())
        .expect("held row");
    assert!(!held.applied, "a withheld target was not committed");

    let TmpRoot {
        guard: _g2,
        root: strict_root,
    } = tmp_root();
    let strict_ws = workspace(
        incomplete_edge_fake(strict_root, EdgeBindingAction::Held),
        Baseline::default(),
    );
    let strict_out = strict_ws
        .upgrade(&RunOpts {
            strict: true,
            ..opts()
        })
        .await;
    assert_eq!(
        strict_out.exit,
        Exit::Policy,
        "a withheld correction is an incomplete mutation under --strict"
    );

    let TmpRoot {
        guard: _g3,
        root: unaddressable_root,
    } = tmp_root();
    let unaddressable_ws = workspace(
        incomplete_edge_fake(unaddressable_root, EdgeBindingAction::Unaddressable),
        Baseline::default(),
    );
    let unaddressable_out = unaddressable_ws
        .upgrade(&RunOpts {
            strict: true,
            ..opts()
        })
        .await;
    assert_eq!(unaddressable_out.exit, Exit::Policy);
    assert_eq!(unaddressable_out.summary.edges_held, 0);
    assert_eq!(unaddressable_out.summary.edges_unaddressable, 1);
    let unaddressable = unaddressable_out
        .items
        .iter()
        .find(|item| item.edge.is_some())
        .expect("unaddressable row");
    assert!(
        unaddressable.applied,
        "the resolver's observed edge move was committed even though policy could not address it"
    );
}

#[tokio::test]
async fn final_edge_audit_replaces_superseded_batch_history() {
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("edge.snapshot"), b"run start").expect("edge snapshot");
    let mut held = diesel_rebind(None, EdgeBindingAction::Held);
    held.detail = Some("temporary verification failure".to_string());
    let corrected = diesel_rebind(None, EdgeBindingAction::Canonicalized);
    let fake = edge_reporting_fake(root, vec![held, corrected]);

    let out = workspace(fake, Baseline::default())
        .upgrade(&RunOpts {
            strict: true,
            ..opts()
        })
        .await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.edges_held, 0);
    assert_eq!(out.summary.edges_corrected, 1);
    let edges: Vec<_> = out
        .items
        .iter()
        .filter_map(|item| item.edge.as_ref())
        .collect();
    std::assert_matches!(edges.as_slice(),
    [edge] if edge.action == EdgeBindingAction::Canonicalized);
}

/// Releases for package "a": the current v1.0.0 plus a long-matured cross-major v2.0.0. `kind =
/// Major` makes v2.0.0 ineligible under a default (major-off) run yet adoptable under `--major`.
fn a_v1_and_matured_v2() -> Vec<Release> {
    vec![
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
        rel(
            "v2.0.0",
            1,
            Some("2026-01-15T00:00:00Z"),
            Some(UpdateKind::Major),
        ),
    ]
}

/// A fixture for package "a" locked at v1.0.0 with `a_releases`, placed as a direct dep (`direct`)
/// or a transitive one. Mirrors the dogfooding `fs4`/`toml_edit` case where `outdated` shows a
/// cross-major update but a default `upgrade` skips it.
fn major_update_fake(root: camino::Utf8PathBuf, direct: bool, a_releases: Vec<Release>) -> FakeEco {
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), a_releases);
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let (direct, transitive) = if direct {
        (vec![dep("a", "v1.0.0", true)], vec![])
    } else {
        (vec![], vec![dep("a", "v1.0.0", false)])
    };
    FakeEco {
        direct,
        transitive,
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    }
}

#[tokio::test]
async fn upgrade_surfaces_adoptable_major_update_held_back_by_default() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let ws = workspace(
        major_update_fake(root, true, a_v1_and_matured_v2()),
        Baseline::default(),
    );
    let out = ws.upgrade(&opts()).await;
    // Nothing is applied; the held-back cross-major counts as a skip (a `skipped` row whose Result is
    // `needs --major`)…
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.skipped, 1);
    // …recorded as a held-back item the user can act on with `--major`.
    let held: Vec<_> = out
        .items
        .iter()
        .filter(|it| {
            it.skipped
                .as_ref()
                .is_some_and(|s| s.reason == SkipReason::NeedsMajor)
        })
        .collect();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].name, "a");
    assert_eq!(held[0].from, "v1.0.0");
    assert_eq!(held[0].to, "v2.0.0");
}

#[tokio::test]
async fn upgrade_major_adopts_the_update_instead_of_hinting() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let ws = workspace(
        major_update_fake(root, true, a_v1_and_matured_v2()),
        Baseline::default(),
    );
    let out = ws
        .upgrade(&RunOpts {
            allow_major: true,
            ..opts()
        })
        .await;
    // With `--major` the same update is adopted, not held back — so no `needs --major` item.
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.items[0].to, "v2.0.0");
    assert!(
        !out.items.iter().any(|it| {
            it.skipped
                .as_ref()
                .is_some_and(|s| s.reason == SkipReason::NeedsMajor)
        }),
        "no held-back item when --major adopts the update"
    );
}

#[tokio::test]
async fn upgrade_reports_a_matured_target_held_by_a_declared_bound() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = a_v1_and_matured_v2();
    releases[1].beyond_declared_bound = true;
    let mut fake = major_update_fake(root, true, releases);
    fake.direct[0].declared_bound = Some(">=1, <2".to_string());
    let out = workspace(fake, Baseline::default())
        .upgrade(&RunOpts {
            allow_major: true,
            strict: true,
            ..opts()
        })
        .await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.skipped, 1);
    let held = out.items.first().expect("bound-held row");
    assert_eq!(held.to, "v2.0.0");
    assert_eq!(
        held.skipped.as_ref().map(|skip| skip.reason),
        Some(SkipReason::DeclaredBoundHeld)
    );
    assert!(!out.items.iter().any(|item| {
        item.skipped
            .as_ref()
            .is_some_and(|skip| skip.reason == SkipReason::NeedsMajor)
    }));
}

#[tokio::test]
async fn upgrade_applies_an_in_bound_update_and_reports_the_bound_held_major() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = vec![
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
        rel(
            "v1.1.0",
            1,
            Some("2026-01-10T00:00:00Z"),
            Some(UpdateKind::Minor),
        ),
        rel(
            "v2.0.0",
            2,
            Some("2026-01-15T00:00:00Z"),
            Some(UpdateKind::Major),
        ),
    ];
    releases[2].beyond_declared_bound = true;
    let mut fake = major_update_fake(root, true, releases);
    fake.direct[0].declared_bound = Some(">=1, <2".to_string());
    let out = workspace(fake, Baseline::default())
        .upgrade(&RunOpts {
            allow_major: true,
            ..opts()
        })
        .await;

    assert!(
        out.items
            .iter()
            .any(|item| item.applied && item.to == "v1.1.0")
    );
    assert!(out.items.iter().any(|item| {
        item.to == "v2.0.0"
            && item
                .skipped
                .as_ref()
                .is_some_and(|skip| skip.reason == SkipReason::DeclaredBoundHeld)
    }));
}

#[tokio::test]
async fn upgrade_does_not_report_a_bound_without_a_matured_target_beyond_it() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = a_v1_and_matured_v2();
    releases[1].published_at = Some(ts("2026-06-16T00:00:00Z"));
    releases[1].beyond_declared_bound = true;
    let mut fake = major_update_fake(root, true, releases);
    fake.direct[0].declared_bound = Some("<2".to_string());
    let out = workspace(fake, Baseline::default())
        .upgrade(&RunOpts {
            allow_major: true,
            ..opts()
        })
        .await;

    assert!(out.items.is_empty());
    assert_eq!(out.summary.skipped, 0);
}

#[tokio::test]
async fn upgrade_rewrite_crosses_a_declared_bound() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = a_v1_and_matured_v2();
    releases[1].beyond_declared_bound = true;
    let mut fake = major_update_fake(root, true, releases);
    fake.direct[0].declared_bound = Some("<2".to_string());
    let out = workspace(fake, Baseline::default())
        .upgrade(&RunOpts {
            allow_major: true,
            rewrite: RewriteMode::Always,
            ..opts()
        })
        .await;

    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.items[0].to, "v2.0.0");
    assert!(out.items[0].skipped.is_none());
}

#[tokio::test]
async fn upgrade_reports_a_matured_target_held_by_max_major() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let fake = major_update_fake(root, true, a_v1_and_matured_v2());
    let mut ceiling = PolicyLayer::new(Origin::Repo("cooldown.toml".into()));
    let mut rule = Rule::new(Selector::Package {
        glob: PatternGlob::new("a").expect("glob"),
        tool: Some(GO),
    });
    rule.max_major = Some(1);
    ceiling.rules.push(rule);
    let out = workspace_with_layers(
        fake,
        Baseline::default(),
        vec![builtin_default_layer(), ceiling],
    )
    .upgrade(&RunOpts {
        allow_major: true,
        strict: true,
        ..opts()
    })
    .await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.skipped, 1);
    assert_eq!(out.items[0].to, "v2.0.0");
    assert_eq!(
        out.items[0].skipped.as_ref().map(|skip| skip.reason),
        Some(SkipReason::MaxMajorHeld)
    );
}

/// The releases of `a_v1_and_matured_v2` with the v2 major marked beyond the registry's `latest`
/// dist-tag — the fumadocs-core shape (a matured, stable major the registry's current `latest`
/// tag points below).
fn a_v1_and_matured_v2_above_the_tag() -> Vec<Release> {
    a_v1_and_matured_v2()
        .into_iter()
        .map(|mut release| {
            release.beyond_latest_tag = release.version == Version::new("v2.0.0");
            release
        })
        .collect()
}

#[tokio::test]
async fn upgrade_reports_a_matured_target_held_by_the_dist_tag() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let fake = major_update_fake(root, true, a_v1_and_matured_v2_above_the_tag());
    let out = workspace(fake, Baseline::default())
        .upgrade(&RunOpts {
            allow_major: true,
            strict: true,
            ..opts()
        })
        .await;

    // A dist-tag hold is conservative-correct — the maintainer's own tag says v2 is not current —
    // so it is reported, does not fail `--strict`, and nothing is applied.
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.skipped, 1);
    assert_eq!(out.items[0].to, "v2.0.0");
    assert_eq!(
        out.items[0].skipped.as_ref().map(|skip| skip.reason),
        Some(SkipReason::DistTagHeld)
    );
}

#[tokio::test]
async fn upgrade_strict_passes_over_a_not_eligible_hold() {
    // The pnpm-catalog shape: the adapter holds the candidate `NotEligible` because cooldown has no
    // editable requirement to retarget (the pin lives in the catalog definition). The dep merely
    // stays on its already-matured older version — a permanent, conservative-correct hold that
    // `--strict` must not fail, or a catalog-using repo could never pass `upgrade --strict`.
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("hold-not-eligible"), "a\n").unwrap();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let ws = workspace(
        fake(
            root.clone(),
            vec![dep("a", "v1.0.0", true)],
            Vec::new(),
            releases,
            locked,
        ),
        Baseline::default(),
    );

    let out = ws
        .upgrade(&RunOpts {
            strict: true,
            ..opts()
        })
        .await;

    assert_eq!(
        out.exit,
        Exit::Ok,
        "a NotEligible hold is a statement that cooldown cannot act, not an incomplete upgrade"
    );
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.skipped, 1);
    assert_eq!(
        out.items[0].skipped.as_ref().map(|skip| skip.reason),
        Some(SkipReason::NotEligible)
    );
}

#[tokio::test]
async fn fix_strict_still_fails_on_a_not_eligible_hold() {
    // Under `fix` the same NotEligible hold leaves a live cooldown violation in the graph — the
    // too-fresh pin stays — so strict keeps failing, consistent with fix's other unfixable holds
    // (graph-held, no matured older release).
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("hold-not-eligible"), "a\n").unwrap();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    let ws = workspace(
        fake(
            root.clone(),
            vec![dep("a", "v1.0.2", true)],
            Vec::new(),
            releases,
            locked,
        ),
        Baseline::default(),
    );

    let out = ws
        .fix(&RunOpts {
            strict: true,
            ..opts()
        })
        .await;

    assert_eq!(out.exit, Exit::Policy);
    assert_eq!(out.summary.applied, 0);
    assert_eq!(
        out.items[0].skipped.as_ref().map(|skip| skip.reason),
        Some(SkipReason::NotEligible)
    );
}

#[tokio::test]
async fn upgrade_ignoring_dist_tags_crosses_the_tag() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let fake = major_update_fake(root, true, a_v1_and_matured_v2_above_the_tag());
    let out = workspace(fake, Baseline::default())
        .upgrade(&RunOpts {
            allow_major: true,
            ignore_dist_tags: true,
            ..opts()
        })
        .await;

    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.items[0].to, "v2.0.0");
    assert!(out.items[0].skipped.is_none());
}

#[tokio::test]
async fn outdated_holds_a_major_above_the_tag_with_the_dist_tag_named() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let fake = major_update_fake(root, true, a_v1_and_matured_v2_above_the_tag());
    let out = workspace(fake, Baseline::default())
        .outdated(&RunOpts {
            allow_major: true,
            ..opts()
        })
        .await;

    let row = out.items.first().expect("the held row");
    assert_eq!(row.status, OutdatedStatus::Held);
    // The hold names the version the registry's `latest` tag recommends (the current pin here), and
    // the newest existing version stays visible as context.
    assert_eq!(
        row.held_by.as_ref().map(|held| held.reason().clone()),
        Some(HeldReason::DistTag("v1.0.0".to_string()))
    );
    assert_eq!(
        row.latest.as_ref().map(|latest| latest.version.as_str()),
        Some("v2.0.0")
    );
    assert!(row.adoptable_target.is_none());
}

#[tokio::test]
async fn upgrade_major_crosses_a_direct_but_not_a_transitive() {
    // `--major` rewrites a *direct* dep's manifest constraint across a major boundary, but a
    // transitive dep has no editable constraint and the resolver would reject an independent
    // cross-major bump — so it is capped to its current major. Tool-agnostic: proven on the fake
    // adapter, so it holds for every tool that reports `direct` correctly.
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), a_v1_and_matured_v2());
    releases.insert("t".to_string(), a_v1_and_matured_v2());
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert(
        "t".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );

    let ws = workspace(
        fake(
            root,
            vec![dep("a", "v1.0.0", true)],
            vec![dep("t", "v1.0.0", false)],
            releases,
            locked,
        ),
        Baseline::default(),
    );
    let out = ws
        .upgrade(&RunOpts {
            allow_major: true,
            ..opts()
        })
        .await;

    // The direct dep crosses the major boundary.
    let a = out
        .items
        .iter()
        .find(|it| it.name == "a")
        .expect("a planned");
    assert_eq!(a.to, "v2.0.0");
    assert!(a.applied);
    // The transitive dep is not carried across the major — it produces no item at all.
    assert!(
        !out.items.iter().any(|it| it.name == "t"),
        "a transitive must not be cross-major'd under --major: {:?}",
        out.items
    );
}

#[tokio::test]
async fn upgrade_does_not_hint_a_transitive_major_update() {
    // Only a directly-declared dep can be adopted by `--major` (it rewrites a manifest constraint),
    // so a transitive cross-major must never be hinted — `cooldown upgrade --major -p <transitive>`
    // would do nothing. The dep is in scope (graph) but `dep.direct` is false.
    let TmpRoot { guard: _g, root } = tmp_root();
    let ws = workspace(
        major_update_fake(root, false, a_v1_and_matured_v2()),
        Baseline::default(),
    );
    let out = ws.upgrade(&opts()).await;
    assert!(
        !out.items.iter().any(|it| {
            it.skipped
                .as_ref()
                .is_some_and(|s| s.reason == SkipReason::NeedsMajor)
        }),
        "a transitive major update must not be flagged"
    );
}

#[tokio::test]
async fn upgrade_applies_the_in_range_update_and_still_hints_the_major() {
    // A dep with both a matured in-range minor (v1.1.0) and a matured cross-major (v2.0.0): the
    // default run adopts the minor and still surfaces the major as a separate hint. The hint's `to`
    // is the major (not the just-applied minor) — the `!=` guard keeps them distinct.
    let TmpRoot { guard: _g, root } = tmp_root();
    let releases = vec![
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
        rel(
            "v1.1.0",
            1,
            Some("2026-01-10T00:00:00Z"),
            Some(UpdateKind::Minor),
        ),
        rel(
            "v2.0.0",
            2,
            Some("2026-01-15T00:00:00Z"),
            Some(UpdateKind::Major),
        ),
    ];
    let ws = workspace(major_update_fake(root, true, releases), Baseline::default());
    let out = ws.upgrade(&opts()).await;
    assert_eq!(out.summary.applied, 1);
    assert!(out.items.iter().any(|it| it.applied && it.to == "v1.1.0"));
    // The major is still flagged as `needs --major` (to = the major, not the just-applied minor),
    // and is informational — not counted in the skipped tally.
    let held: Vec<_> = out
        .items
        .iter()
        .filter(|it| {
            it.skipped
                .as_ref()
                .is_some_and(|s| s.reason == SkipReason::NeedsMajor)
        })
        .collect();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].from, "v1.0.0");
    assert_eq!(held[0].to, "v2.0.0");
    // The held-back major counts as a skip (the renderer breaks out the "need --major" subset).
    assert_eq!(out.summary.skipped, 1);
}

#[tokio::test]
async fn fix_downgrades_too_fresh_direct_to_newest_matured() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    let ws = workspace(
        fake(
            root,
            vec![dep("a", "v1.0.2", true)],
            Vec::new(),
            releases,
            locked,
        ),
        Baseline::default(),
    );

    let out = ws.fix(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.summary.skipped, 0);
    assert!(out.warnings.is_empty());
    assert_eq!(out.items[0].name, "a");
    assert_eq!(out.items[0].from, "v1.0.2");
    assert_eq!(out.items[0].to, "v1.0.1");
    assert!(out.items[0].applied);
    // The rollback is flagged a downgrade (so the report says "downgraded", not "upgraded").
    assert!(out.items[0].downgrade);
}

#[tokio::test]
async fn fix_downgrades_two_simultaneously_too_fresh_deps_in_one_batch() {
    // Both direct deps are younger than the window, so a single `fix` round plans both and applies
    // them as one batch (the joint, ceiling-constrained resolve). Each is a legitimate downgrade of a
    // too-fresh dep; neither must be reported as a conflict because the other also moved backward —
    // the regression the earlier collateral-downgrade guard introduced, where a fix that matured two
    // deps together was rolled back as a false conflict.
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    releases.insert("b".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    locked.insert("b".to_string(), release_named(&package_releases, "v1.0.2"));
    let ws = workspace(
        fake(
            root,
            vec![dep("a", "v1.0.2", true), dep("b", "v1.0.2", true)],
            Vec::new(),
            releases,
            locked,
        ),
        Baseline::default(),
    );

    let out = ws.fix(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 2);
    assert_eq!(out.summary.skipped, 0);
    assert!(out.warnings.is_empty());
    for name in ["a", "b"] {
        let item = out.items.iter().find(|item| item.name == name).expect(name);
        assert_eq!(item.from, "v1.0.2");
        assert_eq!(item.to, "v1.0.1");
        assert!(item.applied);
        assert!(item.downgrade);
    }
}

#[tokio::test]
async fn fix_warns_and_leaves_exact_pin_unless_opted_in() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    let mut pinned = dep("a", "v1.0.2", true);
    pinned.pinned = true;
    let ws = workspace(
        fake(root, vec![pinned], Vec::new(), releases, locked),
        Baseline::default(),
    );

    let out = ws.fix(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert!(out.items.is_empty());
    assert_eq!(out.warnings.len(), 1);
    assert_eq!(out.warnings[0].kind, DiagnosticKind::Held);
    assert!(out.warnings[0].message.contains("--downgrade-pinned"));
}

#[tokio::test]
async fn fix_strict_fails_when_a_violation_is_left_unresolved() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    let mut pinned = dep("a", "v1.0.2", true);
    pinned.pinned = true;
    let ws = workspace(
        fake(root, vec![pinned], Vec::new(), releases, locked),
        Baseline::default(),
    );
    let mut opts = opts();
    opts.strict = true;

    let out = ws.fix(&opts).await;

    assert_eq!(out.exit, Exit::Policy);
    assert_eq!(out.summary.applied, 0);
    assert!(out.items.is_empty());
    assert_eq!(out.warnings.len(), 1);
    assert_eq!(out.warnings[0].kind, DiagnosticKind::Held);
}

#[tokio::test]
async fn fix_downgrades_exact_pin_when_opted_in() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    let mut pinned = dep("a", "v1.0.2", true);
    pinned.pinned = true;
    let ws = workspace(
        fake(root, vec![pinned], Vec::new(), releases, locked),
        Baseline::default(),
    );
    let mut opts = opts();
    opts.downgrade_pinned = true;

    let out = ws.fix(&opts).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.items[0].to, "v1.0.1");
}

#[tokio::test]
async fn fix_warns_and_leaves_graph_held_violation() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    let mut held = dep("a", "v1.0.2", true);
    held.graph_floor = Some(Version::new("v1.0.2"));
    let ws = workspace(
        fake(root, vec![held], Vec::new(), releases, locked),
        Baseline::default(),
    );

    let out = ws.fix(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert!(out.items.is_empty());
    assert_eq!(out.warnings.len(), 1);
    assert_eq!(out.warnings[0].kind, DiagnosticKind::Held);
    assert!(out.warnings[0].message.contains("resolved graph requires"));
}

/// The circular-hold discount through the `fix` planning entry: two violations flooring each
/// other at their fresh versions are a family holding itself in place — the hold is conditioned
/// on the resolution being fixed, so both must be planned and downgraded, where the collapsed
/// floors alone would warn each member graph-held (the previous test's shape, member by member).
#[tokio::test]
async fn fix_plans_a_mutually_held_family_through_the_discount() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    releases.insert("b".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    locked.insert("b".to_string(), release_named(&package_releases, "v1.0.2"));
    let mut a = dep("a", "v1.0.2", true);
    a.graph_floor = Some(Version::new("v1.0.2"));
    a.hold_edges = vec![floor_edge("b", "v1.0.2", "~1.0.2", "v1.0.2")];
    let mut b = dep("b", "v1.0.2", true);
    b.graph_floor = Some(Version::new("v1.0.2"));
    b.hold_edges = vec![floor_edge("a", "v1.0.2", "~1.0.2", "v1.0.2")];
    let ws = workspace(
        fake(root, vec![a, b], Vec::new(), releases, locked),
        Baseline::default(),
    );

    let out = ws.fix(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 2, "both family members move");
    assert!(
        out.warnings.is_empty(),
        "no member may be reported graph-held: {:?}",
        out.warnings
    );
    for name in ["a", "b"] {
        let item = out.items.iter().find(|item| item.name == name).expect(name);
        assert_eq!(
            item.to, "v1.0.1",
            "{name} matures to the newest matured release"
        );
    }
}

/// Release line for the discount/advisory seam: the locked `v1.0.2` and its predecessor `v1.0.1`
/// both carry the advisory's patch, and `v1.0.1` sits in the gap between the two windows — older
/// than the 1-day security window, younger than the ordinary 7-day one. Which of them a rollback
/// can reach is therefore decided entirely by which window the verdict was judged under.
fn advised_fix_releases() -> Vec<Release> {
    vec![
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
        rel(
            "v1.0.1",
            1,
            Some("2026-06-14T00:00:00Z"),
            Some(UpdateKind::Patch),
        ),
        rel(
            "v1.0.2",
            2,
            Some("2026-06-16T12:00:00Z"),
            Some(UpdateKind::Patch),
        ),
    ]
}

/// The circular-hold discount and the security window meet here, and the seam between them is the
/// re-judged verdict: discounting a family's mutual floors forces `fix` to evaluate each member a
/// second time, and that second pass must carry the same advisory context as the first.
///
/// Both members are locked on `v1.0.2`, a patched release too fresh even for the shortened window,
/// so both are violations that hold each other at their fresh versions. Under the security window
/// the rollback reaches `v1.0.1` — the other patched release, and itself compliant once judged
/// under that same window. Judged under the ordinary 7-day window instead, `v1.0.1` is invisible
/// and the rollback falls through to `v1.0.0`, the one release the advisory still affects: losing
/// the context on the second pass would quietly downgrade a security fix into a vulnerable
/// version.
#[tokio::test]
async fn fix_discounting_a_family_keeps_the_security_window_on_the_re_judged_verdict() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = advised_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    releases.insert("b".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    locked.insert("b".to_string(), release_named(&package_releases, "v1.0.2"));
    let mut a = dep("a", "v1.0.2", true);
    a.graph_floor = Some(Version::new("v1.0.2"));
    a.hold_edges = vec![floor_edge("b", "v1.0.2", "~1.0.2", "v1.0.2")];
    let mut b = dep("b", "v1.0.2", true);
    b.graph_floor = Some(Version::new("v1.0.2"));
    b.hold_edges = vec![floor_edge("a", "v1.0.2", "~1.0.2", "v1.0.2")];
    let feed = Arc::new(StaticAdvisories {
        advisories: vec![
            (
                "a".to_string(),
                ghsa_fixed_by_either("GHSA-A", "v1.0.1", "v1.0.2"),
            ),
            (
                "b".to_string(),
                ghsa_fixed_by_either("GHSA-B", "v1.0.1", "v1.0.2"),
            ),
        ],
        stale: false,
        unreachable: false,
    });
    let ws = workspace_with_layers(
        fake(root, vec![a, b], Vec::new(), releases, locked),
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(feed);

    let out = ws.fix(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    assert_eq!(
        out.summary.applied, 2,
        "both family members move: {:?}",
        out.items
    );
    assert!(
        out.warnings.is_empty(),
        "no member may be reported graph-held: {:?}",
        out.warnings
    );
    for name in ["a", "b"] {
        let item = out.items.iter().find(|item| item.name == name).expect(name);
        assert_eq!(
            item.to, "v1.0.1",
            "{name} rolls back to the other patched release, not past the advisory's fix"
        );
    }
}

/// A genuine hold through the `fix` planning entry: the floor comes from a *compliant* requirer
/// (absent from the violation set), so the violation stays in place and the warning names that
/// requirer and its requirement instead of the generic graph-held text.
#[tokio::test]
async fn fix_warning_names_the_compliant_holder() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    let mut held = dep("a", "v1.0.2", true);
    held.graph_floor = Some(Version::new("v1.0.2"));
    held.hold_edges = vec![floor_edge("anchor", "v2.0.0", "^1.0.2", "v1.0.2")];
    let ws = workspace(
        fake(root, vec![held], Vec::new(), releases, locked),
        Baseline::default(),
    );

    let out = ws.fix(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert!(out.items.is_empty());
    assert_eq!(out.warnings.len(), 1);
    assert_eq!(out.warnings[0].kind, DiagnosticKind::Held);
    assert!(
        out.warnings[0]
            .message
            .contains("anchor v2.0.0 requires `^1.0.2`"),
        "the warning names the compliant holder: {}",
        out.warnings[0].message
    );
}

/// The discount set holds only *movable* violations: a transitive `--transitive allow` leaves in
/// place keeps flooring its dependents exactly like a compliant requirer — discounting it would
/// plan changes whose only landing path is moving the very node the flag promised to keep.
#[tokio::test]
async fn fix_transitive_allow_keeps_the_allow_gated_hold() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("d".to_string(), package_releases.clone());
    releases.insert("t".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("d".to_string(), release_named(&package_releases, "v1.0.2"));
    locked.insert("t".to_string(), release_named(&package_releases, "v1.0.2"));
    // Direct violation `d` is held at its fresh version by transitive violation `t`.
    let mut held = dep("d", "v1.0.2", true);
    held.graph_floor = Some(Version::new("v1.0.2"));
    held.hold_edges = vec![floor_edge("t", "v1.0.2", "~1.0.2", "v1.0.2")];
    let ws = workspace(
        fake(
            root,
            vec![held],
            vec![dep("t", "v1.0.2", false)],
            releases,
            locked,
        ),
        Baseline::default(),
    );
    let mut allow = opts();
    allow.transitive_mode = cooldown::app::TransitiveGate::Allow;

    let out = ws.fix(&allow).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0, "nothing may move: {:?}", out.items);
    // `t` is reported left-in-place by the flag; `d` stays held — and the hold is genuine now
    // (its requirer is excluded from the discount), so the warning names `t`.
    assert_eq!(out.warnings.len(), 2);
    assert!(
        out.warnings.iter().any(|warning| warning
            .message
            .contains("left in place by --transitive allow")),
        "{:?}",
        out.warnings
    );
    assert!(
        out.warnings
            .iter()
            .any(|warning| warning.message.contains("t v1.0.2 requires `~1.0.2`")),
        "{:?}",
        out.warnings
    );
}

/// The other direction of the movable-violations rule: a violation with no matured older release
/// stays in the discount set — whether it can move is the resolver's call, and the fresh-era-only
/// crate that simply drops out once its family moves (the `icu_locale_fallback` shape) depends on
/// its edges being discounted.
#[tokio::test]
async fn fix_discounts_a_hold_from_an_untargetable_violation() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    // `x` exists only at its fresh version: a violation with no matured target.
    releases.insert(
        "x".to_string(),
        vec![rel("v1.0.2", 0, Some("2026-06-16T00:00:00Z"), None)],
    );
    releases.insert("y".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert(
        "x".to_string(),
        rel("v1.0.2", 0, Some("2026-06-16T00:00:00Z"), None),
    );
    locked.insert("y".to_string(), release_named(&package_releases, "v1.0.2"));
    let mut held = dep("y", "v1.0.2", true);
    held.graph_floor = Some(Version::new("v1.0.2"));
    held.hold_edges = vec![floor_edge("x", "v1.0.2", "~1.0.2", "v1.0.2")];
    let ws = workspace(
        fake(
            root,
            vec![dep("x", "v1.0.2", true), held],
            Vec::new(),
            releases,
            locked,
        ),
        Baseline::default(),
    );

    let out = ws.fix(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(
        out.summary.applied, 1,
        "the held violation moves: {:?}",
        out.items
    );
    let unblocked = out.items.iter().find(|item| item.name == "y").expect("y");
    assert_eq!(unblocked.to, "v1.0.1");
    // `x` itself remains: no matured release exists for it.
    assert!(
        out.warnings
            .iter()
            .any(|warning| warning.message.contains("no older version has matured")),
        "{:?}",
        out.warnings
    );
}

/// The pinned half of the movable-violations rule: a pin left alone without `--downgrade-pinned`
/// keeps flooring its dependents like a compliant requirer (the held dep's warning names it),
/// while `--downgrade-pinned` puts the pin into the discount set and both violations plan.
#[tokio::test]
async fn fix_pinned_hold_follows_the_downgrade_pinned_flag() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("p".to_string(), package_releases.clone());
    releases.insert("v".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("p".to_string(), release_named(&package_releases, "v1.0.2"));
    locked.insert("v".to_string(), release_named(&package_releases, "v1.0.2"));
    let make = || {
        let mut pinned = dep("p", "v1.0.2", true);
        pinned.pinned = true;
        let mut held = dep("v", "v1.0.2", true);
        held.graph_floor = Some(Version::new("v1.0.2"));
        held.hold_edges = vec![floor_edge("p", "v1.0.2", "~1.0.2", "v1.0.2")];
        workspace(
            fake(
                root.clone(),
                vec![pinned, held],
                Vec::new(),
                releases.clone(),
                locked.clone(),
            ),
            Baseline::default(),
        )
    };

    // Default: the pin is not movable, so its floor on `v` is a genuine hold — nothing moves,
    // `p` warns as pinned, and `v`'s warning names `p` as the holder.
    let out = make().fix(&opts()).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0, "{:?}", out.items);
    assert_eq!(out.warnings.len(), 2);
    assert!(
        out.warnings
            .iter()
            .any(|warning| warning.message.contains("rerun with --downgrade-pinned")),
        "{:?}",
        out.warnings
    );
    assert!(
        out.warnings
            .iter()
            .any(|warning| warning.message.contains("p v1.0.2 requires `~1.0.2`")),
        "{:?}",
        out.warnings
    );

    // --downgrade-pinned: the pin joins the discount set, unblocking `v`; both mature.
    let mut with_pinned = opts();
    with_pinned.downgrade_pinned = true;
    let out = make().fix(&with_pinned).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 2, "{:?}", out.items);
    for name in ["p", "v"] {
        let item = out.items.iter().find(|item| item.name == name).expect(name);
        assert_eq!(item.to, "v1.0.1");
    }
}

/// The round-budget backstop: when the final round still applied changes (and its re-lock floated
/// in fresh work), the exhaustion exit must not report a silent clean success — one plan-only pass
/// surfaces the residual violation as a "rerun fix" warning that marks the run incomplete
/// (`Exit::Policy` under `--strict`).
#[tokio::test]
async fn fix_round_budget_exhaustion_warns_about_residual_violations() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("d".to_string(), package_releases.clone());
    releases.insert("t".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("d".to_string(), release_named(&package_releases, "v1.0.2"));
    locked.insert("t".to_string(), release_named(&package_releases, "v1.0.2"));
    // The re-lock of round 1's downgrade floats in a fresh, reconcilable transitive — work that
    // would need round 2, which the budget denies.
    let mut floated = dep("t", "v1.0.2", false);
    floated.graph_floor = Some(Version::new("v1.0.0"));
    let fake = FakeEco {
        direct: vec![dep("d", "v1.0.2", true)],
        transitive: vec![],
        fresh_transitive: Some(floated),
        releases,
        locked,
        inject_fresh_on_apply: true,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let mut strict = opts();
    strict.strict = true;
    strict.fix_round_budget = Some(1);

    let out = workspace(fake, Baseline::default()).fix(&strict).await;

    assert_eq!(
        out.summary.applied, 1,
        "round 1's downgrade lands: {:?}",
        out.items
    );
    assert!(
        out.warnings.iter().any(|warning| {
            warning.message.contains(
                "t@v1.0.2 is still younger than its cooldown after 1 fix rounds; rerun fix",
            )
        }),
        "{:?}",
        out.warnings
    );
    assert_eq!(
        out.exit,
        Exit::Policy,
        "the exhausted budget marks a strict run incomplete"
    );
}

#[tokio::test]
async fn fix_downgrades_transitive_deps_by_default_with_modes_to_relax() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("t".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert(
        "b".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert("t".to_string(), release_named(&package_releases, "v1.0.2"));

    // A fresh workspace per case: the fake records applied versions across a `fix` run.
    let make = || {
        workspace(
            fake(
                root.clone(),
                vec![dep("b", "v1.0.0", true)],
                vec![dep("t", "v1.0.2", false)],
                releases.clone(),
                locked.clone(),
            ),
            Baseline::default(),
        )
    };

    // Default (Enforce): the whole resolved graph is fixed, so the too-fresh transitive `t` is
    // downgraded to its newest matured version — no opt-in needed.
    let out = make().fix(&opts()).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.items[0].name, "t");
    assert_eq!(out.items[0].to, "v1.0.1");

    // `--transitive hide`: direct-only, so the transitive is neither evaluated nor touched.
    let mut hide = opts();
    hide.transitive_mode = cooldown::app::TransitiveGate::Hide;
    let out = make().fix(&hide).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert!(out.items.is_empty());

    // `--transitive allow`: the transitive is evaluated and reported, but left in place; only direct
    // deps would be downgraded.
    let mut allow = opts();
    allow.transitive_mode = cooldown::app::TransitiveGate::Allow;
    let out = make().fix(&allow).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert!(
        out.warnings.iter().any(|warning| warning
            .message
            .contains("left in place by --transitive allow")),
        "the allowed transitive is reported"
    );
}

#[tokio::test]
async fn fix_collapses_a_round_one_float_and_its_round_two_downgrade_into_one_net_row() {
    // Round 1 downgrades the too-fresh direct `a`; the joint re-resolve floats the transitive `t`
    // up to a fresh v2.0.0 as collateral. Round 2's re-plan then schedules the `t` downgrade to
    // its newest matured release. The fixpoint exit must collapse `t`'s two chronological legs
    // (v1.0.0 → v2.0.0, then v2.0.0 → v1.0.5) into the single net row — this drives the real
    // `fix_to_fixpoint` wiring, not the collapse helper in isolation.
    let TmpRoot { guard: _g, root } = tmp_root();
    // The float happens only on the first re-resolve; the round-2 downgrade must not re-drag it.
    std::fs::write(root.join("collateral-once"), b"").unwrap();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), too_fresh_fix_releases());
    releases.insert(
        "t".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.0.5",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Patch),
            ),
            // Fresh: the round-1 collateral parks `t` here, making it round 2's fix candidate.
            rel(
                "v2.0.0",
                2,
                Some("2026-06-16T00:00:00Z"),
                Some(UpdateKind::Major),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        release_named(&too_fresh_fix_releases(), "v1.0.2"),
    );
    locked.insert(
        "t".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let mut floated = dep("t", "v1.0.0", false);
    // A graph floor below the floated version proves the violation reducible, so the conservative
    // fix gate lets round 2 mature it down instead of rolling round 1 back.
    floated.graph_floor = Some(Version::new("v1.0.0"));
    let mut eco = fake(
        root.clone(),
        vec![dep("a", "v1.0.2", true)],
        vec![floated],
        releases,
        locked,
    );
    eco.collateral_on_apply = vec![Change {
        package: PackageId::new(GO, "t", Some("proxy.example".into())),
        from: Version::new("v1.0.0"),
        to: Version::new("v2.0.0"),
        kind: UpdateKind::Major,
        downgrade: false,
        direct: false,
        members: Vec::new(),
    }];

    let out = workspace(eco, Baseline::default()).fix(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    let a = out
        .items
        .iter()
        .find(|item| item.name == "a")
        .expect("a downgraded");
    assert_eq!((a.from.as_str(), a.to.as_str()), ("v1.0.2", "v1.0.1"));
    assert!(a.downgrade);
    let t_rows: Vec<_> = out.items.iter().filter(|item| item.name == "t").collect();
    assert_eq!(
        t_rows.len(),
        1,
        "the collateral float and its round-2 downgrade must collapse into one net row, got: {t_rows:?}"
    );
    assert_eq!(
        (t_rows[0].from.as_str(), t_rows[0].to.as_str()),
        ("v1.0.0", "v1.0.5")
    );
    assert!(
        !t_rows[0].downgrade,
        "the net move (v1.0.0 → v1.0.5) is forward"
    );
}

#[tokio::test]
async fn upgrade_rolls_back_when_change_introduces_fresh_transitive() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("warn-on-apply"), b"")?;
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    // The fresh transitive's locked release is younger than the window.
    locked.insert(
        "t".to_string(),
        rel("v0.5.0", 0, Some("2026-06-16T00:00:00Z"), None),
    );

    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: Some(dep("t", "v0.5.0", false)),
        releases,
        locked,
        inject_fresh_on_apply: true, // applying the change drags in `t`
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.upgrade(&opts()).await;

    // The optimistic upgrade keeps the lock and tries to mature the floated-up transitive down, but
    // `t` has no older release to fall back to, so the reconcile pass cannot clear it. The final gate
    // restores the pre-lock snapshot and reports the change held, naming `t` — never committing a
    // graph `check` would reject.
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.skipped, 1);
    let sk = out
        .items
        .first()
        .and_then(|item| item.skipped.as_ref())
        .ok_or_else(|| eyre::eyre!("expected a skipped upgrade row"))?;
    assert_eq!(sk.reason, SkipReason::TransitiveInCooldown);
    assert_eq!(sk.offending.as_deref(), Some("t from proxy.example"));
    assert!(
        out.warnings
            .iter()
            .all(|warning| warning.message != "committed correction durability is uncertain")
    );
    Ok(())
}

#[tokio::test]
async fn upgrade_keeps_earlier_singleton_holds_when_later_selection_aborts() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    for name in ["a", "b"] {
        releases.insert(
            name.to_string(),
            vec![
                rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
                rel(
                    "v1.1.0",
                    1,
                    Some("2026-06-01T00:00:00Z"),
                    Some(UpdateKind::Minor),
                ),
            ],
        );
    }
    let mut locked = HashMap::new();
    for name in ["a", "b"] {
        locked.insert(
            name.to_string(),
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
        );
    }
    locked.insert(
        "t".to_string(),
        rel("v0.5.0", 0, Some("2026-06-16T00:00:00Z"), None),
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true), dep("b", "v1.0.0", true)],
        transitive: Vec::new(),
        fresh_transitive: Some(dep("t", "v0.5.0", false)),
        releases,
        locked,
        inject_fresh_on_apply: true,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: true,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State {
            fail_graph_after_attempt: Some(3),
            ..State::default()
        }),
        root,
    };

    let outcome = workspace(fake, Baseline::default()).upgrade(&opts()).await;

    assert_eq!(outcome.exit, Exit::Environment);
    assert_eq!(outcome.summary.applied, 0);
    assert_eq!(outcome.summary.skipped, 1);
    assert_eq!(outcome.summary.errors, 1);
    let held = outcome
        .items
        .iter()
        .find(|item| item.skipped.is_some())
        .ok_or_else(|| eyre::eyre!("earlier rejected singleton was not reported"))?;
    assert_eq!(held.name, "a");
    assert_eq!(
        held.skipped.as_ref().map(|skipped| skipped.reason),
        Some(SkipReason::TransitiveInCooldown)
    );
    Ok(())
}

#[tokio::test]
async fn upgrade_restores_once_when_reconcile_metadata_fetch_fails() {
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("fake.lock"), b"baseline lock").expect("seed fake lock");
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let t_releases = too_fresh_fix_releases();
    releases.insert("t".to_string(), t_releases.clone());
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert("t".to_string(), release_named(&t_releases, "v1.0.2"));
    locked.insert(
        "x".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );

    let mut floated = dep("t", "v1.0.2", false);
    floated.graph_floor = Some(Version::new("v1.0.0"));

    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![dep("x", "v1.0.0", false)],
        fresh_transitive: Some(floated),
        releases,
        locked,
        inject_fresh_on_apply: true,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State {
            // `t` enters the graph only after apply (the injected fresh transitive), so its
            // release probe is the one fetch reconciliation performs cold — graph-wide upgrade
            // planning has already fetched (and the run cache retains) every pre-apply package.
            fail_releases_after_apply_for: Some("t".to_string()),
            write_lock_on_apply: true,
            ..State::default()
        }),
        root: root.clone(),
    };
    let out = workspace(fake, Baseline::default()).upgrade(&opts()).await;

    assert_eq!(out.exit, Exit::Environment);
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.skipped, 0);
    assert_eq!(out.summary.errors, 1);
    assert!(out.items.is_empty());
    assert_eq!(
        out.errors.len(),
        1,
        "the provisional error must not leak twice"
    );
    assert_eq!(out.errors[0].kind, DiagnosticKind::Transient);
    assert_eq!(out.errors[0].package.as_deref(), Some("t"));
    assert!(
        out.errors[0]
            .message
            .contains("reconcile release probe failed for t")
    );
    assert_eq!(
        std::fs::read(root.join("fake.lock")).expect("read restored fake lock"),
        b"baseline lock"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn partial_outer_restore_reports_no_applied_rows() -> eyre::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::create_dir_all(root.join("restored"))?;
    std::fs::create_dir_all(root.join("blocked"))?;
    std::fs::write(root.join("restored/state"), b"original first")?;
    std::fs::write(root.join("blocked/state"), b"original second")?;

    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let t_releases = too_fresh_fix_releases();
    releases.insert("t".to_string(), t_releases.clone());
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert("t".to_string(), release_named(&t_releases, "v1.0.2"));
    locked.insert(
        "x".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let mut floated = dep("t", "v1.0.2", false);
    floated.graph_floor = Some(Version::new("v1.0.0"));
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![dep("x", "v1.0.0", false)],
        fresh_transitive: Some(floated),
        releases,
        locked,
        inject_fresh_on_apply: true,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State {
            // Like the reconcile-fetch test above: only the injected fresh transitive `t` is
            // fetch-cold after apply, now that graph-wide upgrade planning pre-fetches (and the
            // run cache retains) every pre-apply package.
            fail_releases_after_apply_for: Some("t".to_string()),
            force_partial_restore_failure: true,
            ..State::default()
        }),
        root: root.clone(),
    };

    let outcome = workspace(fake, Baseline::default()).upgrade(&opts()).await;
    std::fs::set_permissions(root.join("blocked"), std::fs::Permissions::from_mode(0o700))?;

    // The rollback restored only its first file, so neither previously verified row is authoritative.
    assert_eq!(
        std::fs::read(root.join("restored/state"))?,
        b"original first"
    );
    assert_eq!(std::fs::read(root.join("blocked/state"))?, b"candidate");
    assert_eq!(outcome.exit, Exit::Environment);
    assert_eq!(outcome.summary.applied, 0);
    assert!(!outcome.meta.applied);
    assert!(outcome.items.iter().all(|item| !item.applied));
    assert!(
        outcome
            .errors
            .iter()
            .any(|error| error.message.contains("mutation state is indeterminate"))
    );
    Ok(())
}

#[tokio::test]
async fn upgrade_reconciles_a_floated_up_transitive_instead_of_rolling_back() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let t_releases = too_fresh_fix_releases(); // v1.0.1 matured, v1.0.2 too fresh
    releases.insert("t".to_string(), t_releases.clone());

    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert("t".to_string(), release_named(&t_releases, "v1.0.2"));

    // Upgrading `a` floats `t` up to a too-fresh v1.0.2, but the graph still permits a lower version
    // (floor v1.0.0), so the transitive is *reconcilable* rather than forced.
    let mut floated = dep("t", "v1.0.2", false);
    floated.graph_floor = Some(Version::new("v1.0.0"));

    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: Some(floated),
        releases,
        locked,
        inject_fresh_on_apply: true, // applying the `a` upgrade drags in `t`
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let out = workspace(fake, Baseline::default()).upgrade(&opts()).await;

    // The forward move is kept (not rolled back) and the floated-up transitive is reconciled down to
    // its newest matured version — one `upgrade` leaves a gate-clean lock, no separate `fix` needed.
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 2);
    let upgraded = out.items.iter().find(|item| item.name == "a").expect("a");
    assert_eq!(upgraded.to, "v1.1.0");
    let reconciled = out.items.iter().find(|item| item.name == "t").expect("t");
    assert_eq!(reconciled.to, "v1.0.1");
}

/// The circular-hold discount through the reconcile entry (`reconcile_to_fixpoint`): the floated
/// transitive `t` arms the reconcile, whose planning also sees the pre-existing violation `u` —
/// held at its fresh version only by `t`, itself in the violation set. The discount plans `u`
/// beside `t`; with collapsed floors alone `u` would stay held while only `t` reconciled.
#[tokio::test]
async fn upgrade_reconcile_discounts_a_hold_from_a_co_violating_requirer() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let t_releases = too_fresh_fix_releases(); // v1.0.1 matured, v1.0.2 too fresh
    releases.insert("t".to_string(), t_releases.clone());
    releases.insert("u".to_string(), t_releases.clone());

    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert("t".to_string(), release_named(&t_releases, "v1.0.2"));
    locked.insert("u".to_string(), release_named(&t_releases, "v1.0.2"));

    // The floated transitive is reconcilable in the gate's collapsed view (floor below current).
    let mut floated = dep("t", "v1.0.2", false);
    floated.graph_floor = Some(Version::new("v1.0.0"));
    // The pre-existing violation is held at its fresh version, but only by `t` — a co-violating
    // requirer the reconcile's planning discounts.
    let mut held = dep("u", "v1.0.2", false);
    held.graph_floor = Some(Version::new("v1.0.2"));
    held.hold_edges = vec![floor_edge("t", "v1.0.2", "~1.0.2", "v1.0.2")];

    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![held],
        fresh_transitive: Some(floated),
        releases,
        locked,
        inject_fresh_on_apply: true, // applying the `a` upgrade drags in `t`
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let out = workspace(fake, Baseline::default()).upgrade(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(
        out.summary.applied, 3,
        "forward move plus both reconciled violations"
    );
    let upgraded = out.items.iter().find(|item| item.name == "a").expect("a");
    assert_eq!(upgraded.to, "v1.1.0");
    let reconciled = out.items.iter().find(|item| item.name == "t").expect("t");
    assert_eq!(reconciled.to, "v1.0.1");
    let unblocked = out.items.iter().find(|item| item.name == "u").expect("u");
    assert_eq!(
        unblocked.to, "v1.0.1",
        "the discount plans the violation its co-violating requirer held"
    );
}

#[tokio::test]
async fn upgrade_attempts_reconcile_without_a_known_floor_prediction() {
    // The optimistic upgrade gate does not roll back solely because the first graph probe lacks a
    // per-node floor prediction. It keeps the forward move, lets the adapter try the reconcile
    // downgrade, and rolls back only if the final graph still has a newly introduced violation.
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let t_releases = too_fresh_fix_releases(); // v1.0.1 matured, v1.0.2 too fresh
    releases.insert("t".to_string(), t_releases.clone());

    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert("t".to_string(), release_named(&t_releases, "v1.0.2"));

    // No `graph_floor` is set on the floated-up transitive — the old pessimistic gate would have
    // treated it as irreducible before giving the writer a chance to place a valid downgrade.
    let floated = dep("t", "v1.0.2", false);
    assert!(floated.graph_floor.is_none());

    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: Some(floated),
        releases,
        locked,
        inject_fresh_on_apply: true,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let out = workspace(fake, Baseline::default()).upgrade(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 2);
    let upgraded = out.items.iter().find(|item| item.name == "a").expect("a");
    assert_eq!(upgraded.to, "v1.1.0");
    let reconciled = out.items.iter().find(|item| item.name == "t").expect("t");
    assert_eq!(reconciled.to, "v1.0.1");
}

#[tokio::test]
async fn upgrade_checks_full_graph_even_when_package_filtered() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert(
        "t".to_string(),
        rel("v0.5.0", 0, Some("2026-06-16T00:00:00Z"), None),
    );

    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: Some(dep("t", "v0.5.0", false)),
        releases,
        locked,
        inject_fresh_on_apply: true,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let mut opts = opts();
    opts.package = vec![PatternGlob::new("a").expect("valid glob")];

    let out = ws.upgrade(&opts).await;

    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.skipped, 1);
    let skipped = out.items[0].skipped.as_ref().expect("skip recorded");
    assert_eq!(skipped.reason, SkipReason::TransitiveInCooldown);
    assert_eq!(skipped.offending.as_deref(), Some("t from proxy.example"));
}

#[tokio::test]
async fn upgrade_keeps_an_unrelated_change_when_a_pre_existing_violation_merely_floats() {
    // The repo is already dirty: `t@v0.5.0` is an unacknowledged too-fresh transitive before the run
    // (a standing `check` violation). Upgrading unrelated `a` re-resolves `t` to another fresh version
    // `v0.6.0` that the reconcile pass cannot mature down. Because `t` was ALREADY violating, floating
    // it is not a newly-introduced offender — the optimistic gate must keep the valid `a` upgrade
    // rather than roll it back (one dirty version line stayed one dirty version line).
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    // Both `t` versions are too fresh and there is no matured older release, so reconcile cannot fix it.
    releases.insert(
        "t".to_string(),
        vec![
            rel("v0.5.0", 0, Some("2026-06-16T00:00:00Z"), None),
            rel("v0.6.0", 1, Some("2026-06-18T00:00:00Z"), None),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert(
        "t".to_string(),
        rel("v0.5.0", 0, Some("2026-06-16T00:00:00Z"), None),
    );

    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: Some(dep("t", "v0.5.0", false)),
        releases,
        locked,
        inject_fresh_on_apply: true,
        // The re-resolve floats the already-violating `t` from v0.5.0 to v0.6.0 (still too fresh).
        collateral_on_apply: vec![Change {
            package: PackageId::new(GO, "t", None),
            from: Version::new("v0.5.0"),
            to: Version::new("v0.6.0"),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: false,
            members: Vec::new(),
        }],
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        // `t@v0.5.0` is a violation already present in the baseline graph, before any apply.
        confirm_strips: None,
        state: Mutex::new(State {
            fresh_transitive_present: true,
            ..State::default()
        }),
        root,
    };
    let out = workspace(fake, Baseline::default()).upgrade(&opts()).await;

    // The unrelated `a` upgrade is kept (not rolled back); only `t` is reported as a leftover violation.
    let a = out
        .items
        .iter()
        .find(|item| item.name == "a")
        .expect("a row");
    assert!(
        a.applied,
        "the valid `a` upgrade survives a pre-existing violation"
    );
    assert_eq!(a.to, "v1.1.0");
    assert!(
        out.items
            .iter()
            .all(|item| item.skipped.as_ref().map(|s| s.reason)
                != Some(SkipReason::TransitiveInCooldown)),
        "nothing is rolled back as TransitiveInCooldown for a pre-existing violation"
    );
}

#[tokio::test]
async fn upgrade_fails_closed_when_post_apply_validation_errors() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: true,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.upgrade(&opts()).await;

    assert_eq!(out.exit, Exit::Environment);
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.errors, 1);
    assert!(out.items[0].error.is_some());
}

#[tokio::test]
async fn upgrade_fails_closed_when_post_apply_locked_release_errors() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: Some("a".into()),
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.upgrade(&opts()).await;

    assert_eq!(out.exit, Exit::Environment);
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.errors, 1);
    assert!(out.items[0].error.is_some());
}

#[tokio::test]
async fn upgrade_stops_before_build_when_the_final_lock_is_stale() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: true,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root: root.clone(),
    };
    let ws = workspace(fake, Baseline::default());
    let mut opts = opts();
    opts.build = true;
    let out = ws.upgrade(&opts).await;

    assert_eq!(out.exit, Exit::Environment);
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.summary.errors, 1);
    assert!(
        out.errors
            .iter()
            .any(|d| d.kind == DiagnosticKind::StaleLock)
    );
    assert!(!root.join("build-invoked").exists());
}

#[tokio::test]
async fn upgrade_reports_a_failed_requested_build() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: true,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root: root.clone(),
    };
    let ws = workspace(fake, Baseline::default());
    let mut opts = opts();
    opts.build = true;

    let out = ws.upgrade(&opts).await;

    assert_eq!(out.exit, Exit::Environment);
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.summary.errors, 1);
    assert!(root.join("build-invoked").exists());
    assert!(
        out.errors
            .iter()
            .any(|error| error.message == "build failed")
    );
}

#[tokio::test]
async fn upgrade_stops_before_build_when_final_lock_verification_errors() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("fail-final-lock-verification"), "")?;
    let adapter = fake(
        root.clone(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
    );
    let mut options = opts();
    options.build = true;

    let outcome = workspace(adapter, Baseline::default())
        .upgrade(&options)
        .await;

    assert_eq!(outcome.exit, Exit::Environment);
    assert_eq!(outcome.summary.errors, 1);
    assert!(!root.join("build-invoked").exists());
    Ok(())
}

#[tokio::test]
async fn explain_traces_the_default_window() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases: HashMap::new(),
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.explain("a", &opts()).await?;
    assert_eq!(out.exit, Exit::Ok);
    assert!((out.meta.effective.min_age_days - 7.0).abs() < 1e-9);
    assert_eq!(out.meta.effective.decided_by, "default");
    assert!(out.steps.iter().any(|s| s.applied && s.field == "default"));
    Ok(())
}

#[tokio::test]
async fn explain_refuses_pending_mutation_state() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut adapter = fake(
        root,
        vec![dep("a", "v1.0.0", true)],
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
    );
    adapter
        .state
        .get_mut()
        .map_err(|_| eyre::eyre!("fake state mutex poisoned"))?
        .require_recovery_before_read = true;

    let result = workspace(adapter, Baseline::default())
        .explain("a", &opts())
        .await;

    std::assert_matches!(result, Err(CoreError::StaleLock(_)));
    Ok(())
}

/// `explain` resolves the package's registry from the dependency graph, so a `[registry."…"]`
/// rule is applied (it would be silently skipped if explain resolved with no registry).
#[tokio::test]
async fn explain_applies_registry_scoped_rule() -> eyre::Result<()> {
    let TmpRoot { guard: _g, root } = tmp_root();
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)], // dep `a` is published from registry "proxy.example"
        transitive: vec![],
        fresh_transitive: None,
        releases: HashMap::new(),
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        collateral_on_apply: Vec::new(),
        edge_rebinds_on_apply: Vec::new(),
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        confirm_strips: None,
        state: Mutex::new(State::default()),
        root,
    };

    // A repo layer with a registry-scoped 30d window — above the 7d default.
    let mut repo = PolicyLayer::new(Origin::Repo(Utf8PathBuf::from("cooldown.toml")));
    let mut rule = Rule::new(Selector::Registry("proxy.example".into()));
    rule.window = ByKind::scalar(WindowSpec::MinAge(jiff::SignedDuration::from_hours(
        24 * 30,
    )));
    repo.rules.push(rule);

    let project = fake.project();
    let ctx = ProjectCtx {
        tool: GO,
        project,
        rel_path: Utf8PathBuf::from("."),
        policy: PolicyStack {
            layers: vec![builtin_default_layer(), repo],
            strict_native: false,
        },
        edge_policy: EdgePolicy::default(),
    };
    let mut adapters = AdapterSet::new();
    std::assert_matches!(
        adapters.register_target_verified_mutator(Arc::new(fake)),
        Ok(())
    );
    let ws = Workspace::new(
        adapters,
        vec![ctx],
        now(),
        Baseline::default(),
        Utf8PathBuf::from("."),
        Vec::new(),
    );

    let out = ws.explain("a", &opts()).await?;
    assert_eq!(out.exit, Exit::Ok);
    // The resolved registry is surfaced and the registry rule (30d) beats the 7d default.
    assert_eq!(out.meta.registry.as_deref(), Some("proxy.example"));
    assert!((out.meta.effective.min_age_days - 30.0).abs() < 1e-9);
    assert_eq!(
        out.meta.effective.decided_by,
        "repo:cooldown.toml:registry=proxy.example"
    );
    assert!(
        out.steps
            .iter()
            .any(|s| s.applied && s.selector.as_deref() == Some("registry=proxy.example"))
    );
    Ok(())
}

/// A minimal repo-scoped fake tool used to assert `sync`'s `SyncScope::Repo` dispatch: it counts
/// `write_repo_native` calls so a multi-project run can prove the shared file is written exactly
/// once, and tracks whether the value was already written so a second run reports `Unchanged`.
struct RepoScopedFake {
    root: Utf8PathBuf,
    repo_writes: Arc<Mutex<usize>>,
    recoveries: Arc<Mutex<Vec<Utf8PathBuf>>>,
    already_written: Mutex<bool>,
}

const REPO_TOOL: ToolId = ToolId("repotool");

impl RepoScopedFake {
    fn project(&self, rel: &str) -> Project {
        let root = self.root.join(rel);
        Project {
            root: root.clone(),
            kind: REPO_TOOL,
            manifest: root.join("pyproject.toml"),
            exclude_newer: None,
        }
    }
}

#[async_trait]
impl ToolRead for RepoScopedFake {
    fn id(&self) -> ToolId {
        REPO_TOOL
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can_sync: true,
            ..Default::default()
        }
    }
    fn project_detection(&self) -> cooldown_core::ProjectDetection {
        cooldown_core::ProjectDetection::Primary(cooldown_core::ProjectMarker {
            lockfile: "repo.lock",
            manifest: "pyproject.toml",
            alternate_manifests: &[],
            workspace_root: false,
        })
    }
    async fn dependencies(&self, _p: &Project, _scope: DepScope) -> Result<Vec<Dependency>> {
        Ok(Vec::new())
    }
    async fn native_policy(&self, _p: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }
    async fn verify_lock_current(&self, _p: &Project) -> Result<LockVerifyReport> {
        Ok(LockVerifyReport {
            status: LockStatus::Current,
            detail: "ok".into(),
        })
    }
}

#[async_trait]
impl ReleaseFetcher for RepoScopedFake {
    async fn releases(
        &self,
        _dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> Result<Vec<Release>> {
        Ok(Vec::new())
    }
    async fn locked_release(
        &self,
        dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
    ) -> Result<Release> {
        Err(CoreError::NotFound(dep.package.name.clone()))
    }
}

#[async_trait]
impl ToolWrite for RepoScopedFake {
    fn mutation_tool(&self) -> ToolId {
        REPO_TOOL
    }

    async fn recover_pending_mutation(
        &self,
        project: &Project,
        _coordination: &cooldown_core::fs::ProjectCoordination,
    ) -> Result<MutationRecovery> {
        self.recoveries.lock().unwrap().push(project.root.clone());
        let marker = project.root.join("restore-on-recovery");
        let disposition = match std::fs::remove_file(marker) {
            Ok(()) => RecoveryDisposition::Restored,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                RecoveryDisposition::Unchanged
            }
            Err(error) => return Err(error.into()),
        };
        Ok(MutationRecovery::settled(disposition))
    }

    async fn mutation_journal(&self, p: &Project, _plan: &Plan) -> Result<ProjectMutationJournal> {
        ProjectMutationJournal::capture(&p.root, std::iter::empty::<&camino::Utf8Path>())
    }
    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        mutation.parts_for(self)?;
        Ok(ApplyReport::default())
    }
    async fn build(&self, _p: &Project) -> Result<VerifyReport> {
        Ok(VerifyReport {
            ok: true,
            detail: "ok".into(),
        })
    }
    fn sync_scope(&self) -> SyncScope {
        SyncScope::Repo
    }
    async fn write_repo_native(
        &self,
        repo_root: &camino::Utf8Path,
        _policy: &ResolvedPolicy,
        _dry_run: bool,
    ) -> Result<SyncReport> {
        *self.repo_writes.lock().unwrap() += 1;
        let path = repo_root.join("uv.toml");
        let mut written = self.already_written.lock().unwrap();
        if *written {
            Ok(SyncReport::Unchanged { path })
        } else {
            *written = true;
            Ok(SyncReport::Written { path })
        }
    }
}

#[tokio::test]
async fn sync_repo_scope_writes_once_for_many_projects_and_is_idempotent() -> eyre::Result<()> {
    let TmpRoot { guard: _dir, root } = tmp_root();
    std::fs::create_dir_all(root.join("a"))?;
    std::fs::create_dir_all(root.join("b"))?;
    let repo_writes = Arc::new(Mutex::new(0usize));
    let recoveries = Arc::new(Mutex::new(Vec::new()));
    let fake = RepoScopedFake {
        root: root.clone(),
        repo_writes: Arc::clone(&repo_writes),
        recoveries: Arc::clone(&recoveries),
        already_written: Mutex::new(false),
    };
    // Two projects of the same repo-scoped tool must still trigger a single repo write.
    let contexts = ["a", "b"]
        .into_iter()
        .map(|rel| ProjectCtx {
            tool: REPO_TOOL,
            project: fake.project(rel),
            rel_path: Utf8PathBuf::from(rel),
            policy: PolicyStack {
                layers: vec![builtin_default_layer()],
                strict_native: false,
            },
            edge_policy: EdgePolicy::default(),
        })
        .collect::<Vec<_>>();
    let mut adapters = AdapterSet::new();
    std::assert_matches!(
        adapters.register_target_verified_mutator(Arc::new(fake)),
        Ok(())
    );
    let ws = Workspace::new(
        adapters,
        contexts,
        now(),
        Baseline::default(),
        root.clone(),
        vec![builtin_default_layer()],
    );

    let out = ws
        .sync(&RunOpts {
            source_dir: Some(root.join("a")),
            ..opts()
        })
        .await;
    // Exactly one repo write and one item (labelled "." for the repo root), not one per project.
    assert_eq!(*repo_writes.lock().unwrap(), 1);
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0].project, ".");
    assert_eq!(out.items[0].status, cooldown::app::SyncStatus::Written);
    assert_eq!(out.summary.written, 1);
    assert_eq!(
        *recoveries.lock().unwrap(),
        vec![root.join("a"), root.join("b")]
    );
    // The default 7d window renders as the relative span uv re-evaluates each run.
    assert_eq!(out.items[0].window.as_deref(), Some("7d"));

    // A second sync against the now-current repo file reports unchanged, and still writes once more
    // only to compare (the adapter's own idempotence covers the no-op file write).
    let again = ws.sync(&opts()).await;
    assert_eq!(again.items.len(), 1);
    assert_eq!(again.items[0].status, cooldown::app::SyncStatus::Unchanged);
    assert_eq!(again.summary.unchanged, 1);
    Ok(())
}

#[tokio::test]
async fn repo_sync_preserves_earlier_recovery_notice_when_later_access_fails() -> eyre::Result<()> {
    let TmpRoot { guard: _dir, root } = tmp_root();
    std::fs::create_dir_all(root.join("a"))?;
    std::fs::write(root.join("a/restore-on-recovery"), "")?;
    let repo_writes = Arc::new(Mutex::new(0usize));
    let recoveries = Arc::new(Mutex::new(Vec::new()));
    let fake = RepoScopedFake {
        root: root.clone(),
        repo_writes: Arc::clone(&repo_writes),
        recoveries: Arc::clone(&recoveries),
        already_written: Mutex::new(false),
    };
    let contexts = ["a", "b"]
        .into_iter()
        .map(|rel| ProjectCtx {
            tool: REPO_TOOL,
            project: fake.project(rel),
            rel_path: Utf8PathBuf::from(rel),
            policy: PolicyStack {
                layers: vec![builtin_default_layer()],
                strict_native: false,
            },
            edge_policy: EdgePolicy::default(),
        })
        .collect::<Vec<_>>();
    let mut adapters = AdapterSet::new();
    std::assert_matches!(
        adapters.register_target_verified_mutator(Arc::new(fake)),
        Ok(())
    );
    let workspace = Workspace::new(
        adapters,
        contexts,
        now(),
        Baseline::default(),
        root.clone(),
        vec![builtin_default_layer()],
    );

    let outcome = workspace.sync(&opts()).await;

    assert_eq!(*repo_writes.lock().unwrap(), 0);
    assert_eq!(*recoveries.lock().unwrap(), vec![root.join("a")]);
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.kind == DiagnosticKind::Recovery)
    );
    assert_eq!(outcome.summary.errors, 1);
    Ok(())
}

/// A minimal project-scoped fake tool used to assert `sync`'s `SyncScope::Project` dispatch: it
/// counts `write_native` calls so a multi-project run can prove the per-project file is written once
/// per project. Guards the regression where a tool overrides `write_native` but forgets `sync_scope`,
/// which silently defaults to `SyncScope::None` and stops `sync` writing anything.
struct ProjectScopedFake {
    root: Utf8PathBuf,
    native_writes: Arc<Mutex<Vec<String>>>,
}

const PROJECT_TOOL: ToolId = ToolId("projecttool");

impl ProjectScopedFake {
    fn project(&self, rel: &str) -> Project {
        let root = self.root.join(rel);
        Project {
            root: root.clone(),
            kind: PROJECT_TOOL,
            manifest: root.join("pyproject.toml"),
            exclude_newer: None,
        }
    }
}

#[async_trait]
impl ToolRead for ProjectScopedFake {
    fn id(&self) -> ToolId {
        PROJECT_TOOL
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can_sync: true,
            ..Default::default()
        }
    }
    fn project_detection(&self) -> cooldown_core::ProjectDetection {
        cooldown_core::ProjectDetection::Primary(cooldown_core::ProjectMarker {
            lockfile: "project.lock",
            manifest: "pyproject.toml",
            alternate_manifests: &[],
            workspace_root: false,
        })
    }
    async fn dependencies(&self, _p: &Project, _scope: DepScope) -> Result<Vec<Dependency>> {
        Ok(Vec::new())
    }
    async fn native_policy(&self, _p: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }
    async fn verify_lock_current(&self, _p: &Project) -> Result<LockVerifyReport> {
        Ok(LockVerifyReport {
            status: LockStatus::Current,
            detail: "ok".into(),
        })
    }
}

#[async_trait]
impl ReleaseFetcher for ProjectScopedFake {
    async fn releases(
        &self,
        _dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> Result<Vec<Release>> {
        Ok(Vec::new())
    }
    async fn locked_release(
        &self,
        dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
    ) -> Result<Release> {
        Err(CoreError::NotFound(dep.package.name.clone()))
    }
}

#[async_trait]
impl ToolWrite for ProjectScopedFake {
    fn mutation_tool(&self) -> ToolId {
        PROJECT_TOOL
    }

    async fn mutation_journal(&self, p: &Project, _plan: &Plan) -> Result<ProjectMutationJournal> {
        ProjectMutationJournal::capture(&p.root, std::iter::empty::<&camino::Utf8Path>())
    }
    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        mutation.parts_for(self)?;
        Ok(ApplyReport::default())
    }
    async fn build(&self, _p: &Project) -> Result<VerifyReport> {
        Ok(VerifyReport {
            ok: true,
            detail: "ok".into(),
        })
    }
    fn sync_scope(&self) -> SyncScope {
        SyncScope::Project
    }
    async fn write_native(
        &self,
        project: &Project,
        _policy: &ResolvedPolicy,
        _dry_run: bool,
    ) -> Result<SyncReport> {
        let path = project.root.join("project.toml");
        self.native_writes.lock().unwrap().push(path.to_string());
        Ok(SyncReport::Written { path })
    }
}

#[tokio::test]
async fn sync_project_scope_writes_native_per_project() -> eyre::Result<()> {
    let TmpRoot { guard: _dir, root } = tmp_root();
    std::fs::create_dir_all(root.join("a"))?;
    std::fs::create_dir_all(root.join("b"))?;
    let native_writes = Arc::new(Mutex::new(Vec::new()));
    let fake = ProjectScopedFake {
        root: root.clone(),
        native_writes: Arc::clone(&native_writes),
    };
    // Two in-scope projects of the same project-scoped tool must each get a `write_native`, so a tool
    // that overrides `write_native` but forgets `sync_scope` (defaulting to `None`) is caught.
    let contexts = ["a", "b"]
        .into_iter()
        .map(|rel| ProjectCtx {
            tool: PROJECT_TOOL,
            project: fake.project(rel),
            rel_path: Utf8PathBuf::from(rel),
            policy: PolicyStack {
                layers: vec![builtin_default_layer()],
                strict_native: false,
            },
            edge_policy: EdgePolicy::default(),
        })
        .collect::<Vec<_>>();
    let mut adapters = AdapterSet::new();
    std::assert_matches!(
        adapters.register_target_verified_mutator(Arc::new(fake)),
        Ok(())
    );
    let ws = Workspace::new(
        adapters,
        contexts,
        now(),
        Baseline::default(),
        root.clone(),
        vec![builtin_default_layer()],
    );

    let out = ws.sync(&opts()).await;
    // One `write_native` per project (two), and one written item per project.
    assert_eq!(native_writes.lock().unwrap().len(), 2);
    assert_eq!(out.items.len(), 2);
    assert!(
        out.items
            .iter()
            .all(|item| item.status == cooldown::app::SyncStatus::Written)
    );
    assert_eq!(out.summary.written, 2);
    assert_eq!(
        out.items
            .iter()
            .map(|item| item.project.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    Ok(())
}

/// A fake whose whole-graph `apply` HOLDS the `typer` candidate (the resolve cannot place
/// `typer@0.26.7` because `huggingface-hub` requires `typer<0.26.0`) and lands nothing, exactly as
/// the real uv resolve does in the `download-huggingface-dataset-snapshot` repro. Every `apply`
/// writes a sentinel file into the project root it is handed, so a dry-run (which must run against a
/// throwaway copy) leaves the real root's sentinel absent while a real run creates it — proving the
/// mutation landed only on the copy.
struct HeldConflictFake {
    root: Utf8PathBuf,
}

const HELD_TOOL: ToolId = ToolId("helduv");

impl HeldConflictFake {
    fn project(&self) -> Project {
        Project {
            root: self.root.clone(),
            kind: HELD_TOOL,
            manifest: self.root.join("pyproject.toml"),
            exclude_newer: None,
        }
    }

    fn typer_releases() -> Vec<Release> {
        vec![
            // Long-matured under the 7d default (now = 2026-06-17), so the locked pin is not itself a
            // cooldown violation and the newer 0.26.7 is an adoptable forward move the planner takes.
            rel("0.25.1", 0, Some("2026-01-10T00:00:00Z"), None),
            rel(
                "0.26.7",
                1,
                Some("2026-02-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ]
    }
}

#[async_trait]
impl ToolRead for HeldConflictFake {
    fn id(&self) -> ToolId {
        HELD_TOOL
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    fn project_detection(&self) -> cooldown_core::ProjectDetection {
        cooldown_core::ProjectDetection::Primary(cooldown_core::ProjectMarker {
            lockfile: "uv.lock",
            manifest: "pyproject.toml",
            alternate_manifests: &[],
            workspace_root: true,
        })
    }
    async fn dependencies(&self, _p: &Project, _scope: DepScope) -> Result<Vec<Dependency>> {
        Ok(vec![dep("typer", "0.25.1", true)])
    }
    async fn native_policy(&self, _p: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }
    async fn verify_lock_current(&self, _p: &Project) -> Result<LockVerifyReport> {
        Ok(LockVerifyReport {
            status: LockStatus::Current,
            detail: "ok".into(),
        })
    }
}

#[async_trait]
impl ReleaseFetcher for HeldConflictFake {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> Result<Vec<Release>> {
        if dep.package.name == "typer" {
            Ok(Self::typer_releases())
        } else {
            Ok(Vec::new())
        }
    }
    async fn locked_release(
        &self,
        dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
    ) -> Result<Release> {
        if dep.package.name == "typer" {
            Ok(release_named(&Self::typer_releases(), "0.25.1"))
        } else {
            Err(CoreError::NotFound(dep.package.name.clone()))
        }
    }
}

#[async_trait]
impl ToolWrite for HeldConflictFake {
    fn mutation_tool(&self) -> ToolId {
        HELD_TOOL
    }

    async fn mutation_journal(&self, p: &Project, _plan: &Plan) -> Result<ProjectMutationJournal> {
        ProjectMutationJournal::capture(&p.root, std::iter::empty::<&camino::Utf8Path>())
    }
    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        let (p, plan, _) = mutation.parts_for(self)?;
        // Any apply touches the tree it is handed — a dry-run must hand us the copy, never the real
        // root, so this sentinel never appears in the real project.
        std::fs::write(p.root.join("applied.sentinel"), "applied").unwrap();
        // The whole-graph resolve cannot land typer's candidate: huggingface-hub requires
        // typer<0.26.0, so it is HELD, naming the blocker — exactly upgrade's reported skip.
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            if change.package.name == "typer" {
                report.skipped.push(Skipped {
                    change: change.clone(),
                    reason: SkipReason::ResolverConflict,
                    offending: Some(PackageId::new(
                        HELD_TOOL,
                        "huggingface-hub".to_string(),
                        Some("proxy.example".into()),
                    )),
                    detail: None,
                });
            } else {
                report.applied.push(change.clone());
            }
        }
        Ok(report)
    }
    async fn build(&self, _p: &Project) -> Result<VerifyReport> {
        Ok(VerifyReport {
            ok: true,
            detail: "ok".into(),
        })
    }
}

fn held_conflict_workspace(root: Utf8PathBuf) -> Workspace {
    let fake = HeldConflictFake { root: root.clone() };
    let ctx = ProjectCtx {
        tool: HELD_TOOL,
        project: fake.project(),
        rel_path: Utf8PathBuf::from("."),
        policy: PolicyStack {
            layers: vec![builtin_default_layer()],
            strict_native: false,
        },
        edge_policy: EdgePolicy::default(),
    };
    let mut adapters = AdapterSet::new();
    std::assert_matches!(
        adapters.register_target_verified_mutator(Arc::new(fake)),
        Ok(())
    );
    Workspace::new(
        adapters,
        vec![ctx],
        now(),
        Baseline::default(),
        root,
        Vec::new(),
    )
}

/// One held/blocked candidate as a run reports it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct HeldCandidate {
    /// The candidate package that came back skipped.
    package: String,
    /// The blocker the resolve named, when it named one.
    offending: Option<String>,
}

/// The held/blocked set a run reports: every typer-or-other candidate that came back skipped, with
/// the blocker the resolve named. Shared by the real and dry-run assertions so the two are compared
/// by the exact same extraction.
fn held_set(items: &[cooldown::app::UpgradeItem]) -> Vec<HeldCandidate> {
    let mut held: Vec<HeldCandidate> = items
        .iter()
        .filter_map(|item| {
            item.skipped.as_ref().map(|skipped| HeldCandidate {
                package: item.name.clone(),
                offending: skipped.offending.clone(),
            })
        })
        .collect();
    held.sort();
    held
}

#[tokio::test]
async fn dry_run_reports_the_truly_held_candidate_matching_the_real_run() {
    // The repro: typer's matured 0.26.7 is adoptable in isolation, but the whole-graph resolve HOLDS
    // it because huggingface-hub requires typer<0.26.0. A `--dry-run` must preview the TRUE outcome —
    // typer HELD (not `planned`/applied) — by running the same resolve against a throwaway copy, so it
    // agrees with the real run exactly.
    let TmpRoot {
        guard: _real_dir,
        root: real_root,
    } = tmp_root();
    let real_sentinel = real_root.join("applied.sentinel");

    let dry = held_conflict_workspace(real_root.clone())
        .upgrade(&RunOpts {
            dry_run: true,
            ..opts()
        })
        .await;

    // typer is reported HELD, naming its blocker — never a phantom `planned`/applied row.
    let typer = dry
        .items
        .iter()
        .find(|item| item.name == "typer")
        .expect("typer row");
    assert!(
        !typer.applied,
        "dry-run must not promise an upgrade the real run holds"
    );
    let skipped = typer
        .skipped
        .as_ref()
        .expect("typer must be held (skipped), not planned");
    assert_eq!(skipped.reason, SkipReason::ResolverConflict);
    assert_eq!(
        skipped.offending.as_deref(),
        Some("huggingface-hub from proxy.example")
    );
    assert_eq!(dry.summary.applied, 0);
    assert_eq!(dry.summary.skipped, 1);
    // A dry-run never persists: the apply ran against the copy, so the real root has no sentinel and
    // the lock is reported untouched.
    assert!(
        !real_sentinel.as_std_path().exists(),
        "dry-run mutated the real project root"
    );
    assert_eq!(dry.meta.lock_status, None);

    // The real run produces the identical held/blocked set — the two agree by construction.
    let TmpRoot {
        guard: _real_dir2,
        root: real_root2,
    } = tmp_root();
    let real = held_conflict_workspace(real_root2.clone())
        .upgrade(&opts())
        .await;
    assert_eq!(
        held_set(&dry.items),
        held_set(&real.items),
        "dry-run's held set must equal the real run's"
    );
    assert_eq!(
        held_set(&real.items),
        vec![HeldCandidate {
            package: "typer".to_string(),
            offending: Some("huggingface-hub from proxy.example".to_string()),
        }],
    );
    // The real run did mutate its own (separate) root — proving the sentinel mechanism actually fires
    // when the apply is against the real tree, so the dry-run's absent sentinel is meaningful.
    assert!(
        real_root2.join("applied.sentinel").as_std_path().exists(),
        "the real run must mutate the real root",
    );
}

#[tokio::test]
async fn dry_run_leaves_the_real_lock_and_manifest_byte_identical() {
    // Beyond "no sentinel": the real on-disk manifest and lock must be byte-for-byte unchanged after a
    // dry-run, since the whole resolve ran against a throwaway copy.
    let TmpRoot { guard: _dir, root } = tmp_root();
    let manifest = root.join("pyproject.toml");
    let lock = root.join("uv.lock");
    std::fs::write(
        &manifest,
        "[project]\nname = \"snap\"\nversion = \"0.1.0\"\ndependencies = [\"typer==0.25.1\"]\n",
    )
    .unwrap();
    std::fs::write(&lock, "version = 1\nrevision = 3\n# typer 0.25.1\n").unwrap();

    let digest = |path: &camino::Utf8Path| -> Vec<u8> {
        use std::hash::{Hash, Hasher};
        let bytes = std::fs::read(path).unwrap();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hasher);
        // Pair the content hash with the raw length so a collision cannot mask a real change.
        let mut out = hasher.finish().to_be_bytes().to_vec();
        out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        out
    };
    let manifest_before = digest(&manifest);
    let lock_before = digest(&lock);

    let out = held_conflict_workspace(root.clone())
        .upgrade(&RunOpts {
            dry_run: true,
            ..opts()
        })
        .await;
    // The dry-run still computed the real held outcome…
    assert_eq!(out.summary.skipped, 1);
    assert_eq!(out.summary.applied, 0);
    // …without touching the real manifest or lock.
    assert_eq!(
        digest(&manifest),
        manifest_before,
        "dry-run modified the real manifest",
    );
    assert_eq!(digest(&lock), lock_before, "dry-run modified the real lock");
}

/// A feed that answers the first query and fails from then on, counting the queries it saw.
///
/// The shape a top-up hits when the feed goes down after the initial fetch: every later round
/// would ask again, so this is what proves the executor stops asking.
struct FlakyAdvisories {
    advisories: Vec<(String, RawAdvisory)>,
    queries: Mutex<usize>,
}

impl FlakyAdvisories {
    fn queries(&self) -> usize {
        *self.queries.lock().expect("query count")
    }
}

#[async_trait]
impl AdvisorySource for FlakyAdvisories {
    fn id(&self) -> AdvisorySourceId {
        AdvisorySourceId("osv")
    }

    async fn advisories(&self, _ecosystem: &str, packages: &[String]) -> Result<AdvisoryFetch> {
        let mut queries = self.queries.lock().expect("query count");
        *queries += 1;
        if *queries > 1 {
            return Err(CoreError::transient("advisory feed down".to_string()));
        }
        Ok(AdvisoryFetch {
            packages: self
                .advisories
                .iter()
                .filter(|(package, _)| packages.contains(package))
                .map(|(package, advisory)| PackageAdvisories {
                    package: package.clone(),
                    advisories: vec![advisory.clone()],
                })
                .collect(),
            stale: false,
        })
    }
}

/// A canned advisory feed for the conformance tests: static per-package advisories, an optional
/// stale marker, or a hard failure — the app must treat all three exactly like a real OSV
/// client.
struct StaticAdvisories {
    advisories: Vec<(String, RawAdvisory)>,
    stale: bool,
    unreachable: bool,
}

#[async_trait]
impl AdvisorySource for StaticAdvisories {
    fn id(&self) -> AdvisorySourceId {
        AdvisorySourceId("osv")
    }

    async fn advisories(&self, _ecosystem: &str, packages: &[String]) -> Result<AdvisoryFetch> {
        if self.unreachable {
            return Err(CoreError::transient("advisory feed down".to_string()));
        }
        Ok(AdvisoryFetch {
            packages: self
                .advisories
                .iter()
                .filter(|(package, _)| packages.contains(package))
                .map(|(package, advisory)| PackageAdvisories {
                    package: package.clone(),
                    advisories: vec![advisory.clone()],
                })
                .collect(),
            stale: self.stale,
        })
    }
}

/// A High advisory affecting everything below `fixed`, with `fixed` as its one fix version.
///
/// Spelled the way every real OSV document does — an `introduced` opens the range, even when it
/// is the "from the beginning" sentinel — because evaluation starts *not* vulnerable, so a range
/// that only closes describes nothing.
fn ghsa_fixed_by(id: &str, fixed: &str) -> RawAdvisory {
    RawAdvisory {
        id: id.to_string(),
        aliases: Vec::new(),
        severity: AdvisorySeverity::High,
        withdrawn: false,
        summary: "conformance advisory".to_string(),
        ranges: vec![RawAffectedRange {
            events: vec![
                cooldown_core::RawRangeEvent::Introduced("0".to_string()),
                cooldown_core::RawRangeEvent::Fixed(fixed.to_string()),
            ],
        }],
        affected_versions: Vec::new(),
        fixes: vec![fixed.to_string()],
    }
}

/// An advisory patched in `fixed` whose patch also ships in the later `also_fixed` release — the
/// shape OSV produces when one fix lands on several maintained release lines, so the feed names
/// more than one exact fix version. Only versions below `fixed` are affected.
fn ghsa_fixed_by_either(id: &str, fixed: &str, also_fixed: &str) -> RawAdvisory {
    RawAdvisory {
        fixes: vec![fixed.to_string(), also_fixed.to_string()],
        ..ghsa_fixed_by(id, fixed)
    }
}

/// A repo policy layer enabling the feed, optionally selecting the shorten mode (with a 1-day
/// security window).
fn advisory_layer(mode: Option<AdvisoryMode>) -> PolicyLayer {
    let mut layer = PolicyLayer::new(Origin::Repo(Utf8PathBuf::from("cooldown.toml")));
    layer.advisories = Some(AdvisoryPolicy {
        enabled: Some(true),
        mode,
        min_age: mode
            .filter(|mode| *mode == AdvisoryMode::Shorten)
            .map(|_| jiff::SignedDuration::from_hours(24)),
        ..AdvisoryPolicy::default()
    });
    layer
}

fn advisory_layers(mode: Option<AdvisoryMode>) -> Vec<PolicyLayer> {
    vec![
        cooldown_core::config::builtin_default_layer(),
        advisory_layer(mode),
    ]
}

/// The dep-a fixture the advisory tests share: locked at a mature `v1.0.0`, with a 2-day-old
/// `v1.1.0` (inside the 7d default window, past a 1d security window) that fixes `GHSA-CONF`.
fn advisory_fake(root: Utf8PathBuf) -> FakeEco {
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-15T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    fake(
        root,
        vec![dep("a", "v1.0.0", true)],
        Vec::new(),
        releases,
        locked,
    )
}

fn advisory_feed() -> Arc<StaticAdvisories> {
    Arc::new(StaticAdvisories {
        advisories: vec![("a".to_string(), ghsa_fixed_by("GHSA-CONF", "v1.1.0"))],
        stale: false,
        unreachable: false,
    })
}

/// Flag mode end to end: `outdated` annotates the fixing candidate without changing any verdict
/// — the row reads the same as an unadvised run plus the security block.
#[tokio::test]
async fn advisory_flag_mode_annotates_outdated_without_changing_the_verdict() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let unadvised = workspace(advisory_fake(root.clone()), Baseline::default())
        .outdated(&opts())
        .await;

    let ws = workspace_with_layers(
        advisory_fake(root.clone()),
        Baseline::default(),
        advisory_layers(None),
    )
    .with_advisory_source(advisory_feed());
    let advised = ws.outdated(&opts()).await;

    assert_eq!(advised.exit, Exit::Ok);
    let plain = &unadvised.items[0];
    let flagged = &advised.items[0];
    assert_eq!(flagged.status, plain.status, "flag mode changes no verdict");
    assert_eq!(flagged.status, OutdatedStatus::InCooldown);
    assert_eq!(flagged.adoptable_target, plain.adoptable_target);
    let security = flagged.security.as_ref().expect("annotated");
    assert_eq!(security.fixes, vec!["GHSA-CONF".to_string()]);
    assert_eq!(security.severity, AdvisorySeverity::High);
    assert!(!security.applied);
    assert!(plain.security.is_none(), "no feed, no annotation");
}

/// The shorten mode across the whole mutation pipeline — the load-bearing consistency: the
/// planner adopts the fix through the 1-day security window, and the post-apply residual gate
/// (advised from the same feed) accepts the now-young pin instead of rolling back the very
/// change the planner just made.
#[tokio::test]
async fn advisory_shorten_mode_upgrades_the_fix_and_the_residual_gate_keeps_it() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let ws = workspace_with_layers(
        advisory_fake(root.clone()),
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(advisory_feed());
    let out = ws.upgrade(&opts()).await;

    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    assert_eq!(out.summary.applied, 1, "items: {:?}", out.items);
    let item = out
        .items
        .iter()
        .find(|item| item.name == "a")
        .expect("the planned fix");
    assert!(item.applied, "rolled back: {:?}", item.skipped);
    assert_eq!(item.to, "v1.1.0");
    // The row explains itself: a fast-tracked adoption must not read like a routine bump.
    let security = item.security.as_ref().expect("security provenance");
    assert_eq!(security.version, "v1.1.0");
    assert_eq!(security.fixes, vec!["GHSA-CONF".to_string()]);
    assert!(security.applied, "the security window is why it landed");

    // Without the shorten mode the same fixture plans nothing: v1.1.0 is inside the 7d window.
    let flag_ws = workspace_with_layers(
        advisory_fake(root.clone()),
        Baseline::default(),
        advisory_layers(None),
    )
    .with_advisory_source(advisory_feed());
    let flag_out = flag_ws.upgrade(&opts()).await;
    assert_eq!(flag_out.summary.applied, 0, "flag mode must not fast-track");
}

/// A young cross-major security fix (2 days old, matured only under the 1-day security window)
/// with `a` locked at `v1.0.0`: the fix `v2.0.0` fixes `GHSA-CONF`.
fn young_major_fix(root: Utf8PathBuf) -> (FakeEco, Arc<StaticAdvisories>) {
    let fake = major_update_fake(
        root,
        true,
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v2.0.0",
                1,
                Some("2026-06-15T00:00:00Z"),
                Some(UpdateKind::Major),
            ),
        ],
    );
    let feed = Arc::new(StaticAdvisories {
        advisories: vec![("a".to_string(), ghsa_fixed_by("GHSA-CONF", "v2.0.0"))],
        stale: false,
        unreachable: false,
    });
    (fake, feed)
}

/// A `needs --major` hold keeps its security provenance: the held row says the target fixes the
/// advisory (and that the security window is why it matured), instead of reading as a routine
/// skip — the probe runs advised precisely so this hold surfaces at all.
#[tokio::test]
async fn a_held_back_major_security_fix_keeps_its_provenance() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let (fake, feed) = young_major_fix(root);
    let out = workspace_with_layers(
        fake,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(feed)
    .upgrade(&opts())
    .await;

    assert_eq!(out.summary.applied, 0);
    let held = out
        .items
        .iter()
        .find(|item| {
            item.skipped
                .as_ref()
                .is_some_and(|skip| skip.reason == SkipReason::NeedsMajor)
        })
        .expect("the held-back major row");
    assert_eq!(held.to, "v2.0.0");
    let security = held.security.as_ref().expect("security provenance");
    assert_eq!(security.fixes, vec!["GHSA-CONF".to_string()]);
    assert!(security.applied, "matured only under the security window");
}

/// A ceiling hold (here a declared manifest bound) keeps its security provenance the same way —
/// the ceiling recorder is a separate implementation from the `needs --major` one.
#[tokio::test]
async fn a_bound_held_security_fix_keeps_its_provenance() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let (mut fake, feed) = young_major_fix(root);
    if let Some(release) = fake
        .releases
        .get_mut("a")
        .and_then(|releases| releases.get_mut(1))
    {
        release.beyond_declared_bound = true;
    }
    if let Some(dep) = fake.direct.first_mut() {
        dep.declared_bound = Some(">=1, <2".to_string());
    }
    let out = workspace_with_layers(
        fake,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(feed)
    .upgrade(&RunOpts {
        allow_major: true,
        ..opts()
    })
    .await;

    assert_eq!(out.summary.applied, 0);
    let held = out
        .items
        .iter()
        .find(|item| {
            item.skipped
                .as_ref()
                .is_some_and(|skip| skip.reason == SkipReason::DeclaredBoundHeld)
        })
        .expect("the bound-held row");
    assert_eq!(held.to, "v2.0.0");
    let security = held.security.as_ref().expect("security provenance");
    assert_eq!(security.fixes, vec!["GHSA-CONF".to_string()]);
    assert!(security.applied, "matured only under the security window");
}

/// A package whose resolved source is not provably in the advisory ecosystem (a private
/// registry's namesake of a public package) carries no advisory identity: it is never queried,
/// never annotated, and never earns the security window — the public package's advisories are
/// about a different package. The narrowed coverage is said out loud, once.
#[tokio::test]
async fn an_unproven_source_gets_no_advisory_identity() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let private = |root: Utf8PathBuf| {
        let mut eco = advisory_fake(root);
        for dep in &mut eco.direct {
            dep.advisory_identity = None;
        }
        eco
    };

    // Shorten mode: the same fixture whose public form fast-tracks the fix
    // (see `advisory_shorten_mode_upgrades_the_fix_and_the_residual_gate_keeps_it`)
    // must plan nothing when the package's origin is unproven.
    let out = workspace_with_layers(
        private(root.clone()),
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(advisory_feed())
    .upgrade(&opts())
    .await;
    assert_eq!(
        out.summary.applied, 0,
        "a public namesake's advisory must not fast-track a private package: {:?}",
        out.items
    );
    assert!(
        out.warnings.iter().any(|warning| {
            warning.kind == DiagnosticKind::AdvisoryEcosystemUnsupported
                && warning.message.contains("advisory ecosystem")
        }),
        "withheld coverage must be loud: {:?}",
        out.warnings
    );

    // Flag mode: no annotation either — the advisory names a different package.
    let advised = workspace_with_layers(private(root), Baseline::default(), advisory_layers(None))
        .with_advisory_source(advisory_feed())
        .outdated(&opts())
        .await;
    assert!(
        advised.items[0].security.is_none(),
        "an unproven source must not inherit the public package's annotation"
    );
}

/// The read-side plumbing of feed-time confirmation: an identity the adapter withholds when
/// `outdated` consults the feed (npm's effective-registry veto) is neither queried nor matched
/// — the row loses its annotation and the withheld coverage warns — however confidently
/// `dependencies()` granted it.
#[tokio::test]
async fn outdated_confirms_identities_with_the_adapter_before_the_feed() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut eco = advisory_fake(root);
    eco.confirm_strips = Some("a".to_string());
    let ws = workspace_with_layers(eco, Baseline::default(), advisory_layers(None))
        .with_advisory_source(advisory_feed());
    let out = ws.outdated(&opts()).await;

    assert!(
        out.items[0].security.is_none(),
        "a feed-time-withheld identity is never matched: {:?}",
        out.items[0].security
    );
    assert!(
        out.warnings
            .iter()
            .any(|warning| warning.kind == DiagnosticKind::AdvisoryEcosystemUnsupported),
        "the withheld coverage is loud: {:?}",
        out.warnings
    );
}

/// The upgrade-side plumbing of the same confirmation: without it, the shorten mode adopts the
/// young fix on the strength of an identity the adapter would have withheld at feed time.
#[tokio::test]
async fn upgrade_confirms_identities_with_the_adapter_before_the_feed() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut eco = advisory_fake(root);
    eco.confirm_strips = Some("a".to_string());
    let ws = workspace_with_layers(
        eco,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(advisory_feed());
    let out = ws.upgrade(&opts()).await;

    assert_eq!(
        out.summary.applied, 0,
        "no security window without a confirmed identity: {:?}",
        out.items
    );
    assert!(
        out.warnings
            .iter()
            .any(|warning| warning.kind == DiagnosticKind::AdvisoryEcosystemUnsupported),
        "the withheld coverage is loud: {:?}",
        out.warnings
    );
}

/// The collateral-provenance fixture: a matured routine `a` bump planned under `-p a`, with the
/// transitive `t` outside the planning scope entirely — so any movement of `t` is purely the
/// resolver's, claimed by no planned row and probed by no planner.
fn collateral_fixture(root: Utf8PathBuf, t_releases: Vec<Release>, t_locked: Release) -> FakeEco {
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-01-02T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let t_current = t_locked.version.clone();
    releases.insert("t".to_string(), t_releases);
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert("t".to_string(), t_locked);
    fake(
        root,
        vec![dep("a", "v1.0.0", true)],
        vec![dep("t", t_current.as_str(), false)],
        releases,
        locked,
    )
}

fn collateral_move(from: &str, to: &str, kind: UpdateKind, downgrade: bool) -> Change {
    Change {
        package: PackageId::new(GO, "t", Some("proxy.example".into())),
        from: Version::new(from),
        to: Version::new(to),
        kind,
        downgrade,
        direct: false,
        members: Vec::new(),
    }
}

/// A resolver-induced collateral change that lands an exact security fix must carry the same
/// provenance a planned row would: `t` is outside the planning scope, but the joint resolve
/// drags it onto the young fix admitted only by the security window — and that applied row must
/// not read as a routine move.
#[tokio::test]
async fn a_collateral_security_fix_keeps_its_provenance() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut eco = collateral_fixture(
        root,
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            // Two days old: inside the 7d default window, past the 1d security window.
            rel(
                "v2.0.0",
                1,
                Some("2026-06-15T00:00:00Z"),
                Some(UpdateKind::Major),
            ),
        ],
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    eco.collateral_on_apply = vec![collateral_move(
        "v1.0.0",
        "v2.0.0",
        UpdateKind::Major,
        false,
    )];
    let feed = Arc::new(StaticAdvisories {
        advisories: vec![("t".to_string(), ghsa_fixed_by("GHSA-COLL", "v2.0.0"))],
        stale: false,
        unreachable: false,
    });
    let out = workspace_with_layers(
        eco,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(feed)
    .upgrade(&RunOpts {
        package: vec![PatternGlob::new("a").expect("glob")],
        ..opts()
    })
    .await;

    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    let collateral_row = out
        .items
        .iter()
        .find(|item| item.name == "t" && item.applied)
        .expect("the collateral applied row");
    assert_eq!(collateral_row.to, "v2.0.0");
    let security = collateral_row
        .security
        .as_ref()
        .expect("a collateral exact fix must not read as a routine move");
    assert_eq!(security.fixes, vec!["GHSA-COLL".to_string()]);
    assert_eq!(security.version, "v2.0.0");
    assert!(security.applied, "the security window admits the young pin");
}

/// A collateral downgrade off an advisory's exact fix version must not commit in silence: the
/// landing version is old and matured, so the residual gate passes it — the promised rollback
/// warning is the only signal left.
#[tokio::test]
async fn a_collateral_rollback_off_a_fix_warns() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut eco = collateral_fixture(
        root,
        vec![
            rel("v1.0.1", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.0.2",
                1,
                Some("2026-01-02T00:00:00Z"),
                Some(UpdateKind::Patch),
            ),
        ],
        rel("v1.0.2", 1, Some("2026-01-02T00:00:00Z"), None),
    );
    eco.collateral_on_apply = vec![collateral_move("v1.0.2", "v1.0.1", UpdateKind::Patch, true)];
    let feed = Arc::new(StaticAdvisories {
        advisories: vec![("t".to_string(), ghsa_fixed_by("GHSA-COLL", "v1.0.2"))],
        stale: false,
        unreachable: false,
    });
    let out = workspace_with_layers(
        eco,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(feed)
    .upgrade(&RunOpts {
        package: vec![PatternGlob::new("a").expect("glob")],
        ..opts()
    })
    .await;

    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    let collateral_row = out
        .items
        .iter()
        .find(|item| item.name == "t" && item.applied)
        .expect("the collateral applied row");
    assert_eq!(collateral_row.to, "v1.0.1");
    assert!(collateral_row.downgrade);
    assert!(
        collateral_row.security.is_none(),
        "the landing version fixes nothing"
    );
    assert!(
        out.warnings.iter().any(|warning| {
            warning.kind == DiagnosticKind::AdvisoryRollback
                && warning.message.contains("GHSA-COLL")
                && warning.message.contains("rolled t back")
        }),
        "the rollback off the fix must warn: {:?}",
        out.warnings
    );
}

/// A *forward* collateral move can abandon a fix too: OSV ranges may reintroduce a
/// vulnerability, so an advisory fixed at `v1.0.2` can list `v3.1.0` as affected again. Both
/// versions here are old and matured, so the residual gate passes the landing pin as routine —
/// the direction-neutral fix-loss warning is the only signal left.
#[tokio::test]
async fn a_collateral_forward_move_onto_a_reintroduced_advisory_warns() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut eco = collateral_fixture(
        root,
        vec![
            rel("v1.0.2", 0, Some("2026-01-02T00:00:00Z"), None),
            rel(
                "v3.1.0",
                1,
                Some("2026-01-03T00:00:00Z"),
                Some(UpdateKind::Major),
            ),
        ],
        rel("v1.0.2", 0, Some("2026-01-02T00:00:00Z"), None),
    );
    eco.collateral_on_apply = vec![collateral_move(
        "v1.0.2",
        "v3.1.0",
        UpdateKind::Major,
        false,
    )];
    let mut advisory = ghsa_fixed_by("GHSA-COLL", "v1.0.2");
    advisory.affected_versions = vec!["v3.1.0".to_string()];
    let feed = Arc::new(StaticAdvisories {
        advisories: vec![("t".to_string(), advisory)],
        stale: false,
        unreachable: false,
    });
    let out = workspace_with_layers(
        eco,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(feed)
    .upgrade(&RunOpts {
        package: vec![PatternGlob::new("a").expect("glob")],
        ..opts()
    })
    .await;

    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    assert!(
        out.warnings.iter().any(|warning| {
            warning.kind == DiagnosticKind::AdvisoryRollback
                && warning.message.contains("moved t from v1.0.2")
                && warning.message.contains("GHSA-COLL")
                && warning.message.contains("affected again")
        }),
        "the forward move off the fix must warn: {:?}",
        out.warnings
    );
}

/// The anti-noise half of direction-neutral fix loss: a forward move past a fix boundary onto a
/// version the advisory does *not* list stays silent — past the boundary the graph is
/// ordinarily still fixed, and warning there would fire on every routine upgrade of a package
/// that ever adopted a security fix.
#[tokio::test]
async fn a_collateral_forward_move_past_a_fix_stays_silent() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut eco = collateral_fixture(
        root,
        vec![
            rel("v1.0.2", 0, Some("2026-01-02T00:00:00Z"), None),
            rel(
                "v1.0.4",
                1,
                Some("2026-01-03T00:00:00Z"),
                Some(UpdateKind::Patch),
            ),
        ],
        rel("v1.0.2", 0, Some("2026-01-02T00:00:00Z"), None),
    );
    eco.collateral_on_apply = vec![collateral_move(
        "v1.0.2",
        "v1.0.4",
        UpdateKind::Patch,
        false,
    )];
    let feed = Arc::new(StaticAdvisories {
        advisories: vec![("t".to_string(), ghsa_fixed_by("GHSA-COLL", "v1.0.2"))],
        stale: false,
        unreachable: false,
    });
    let out = workspace_with_layers(
        eco,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(feed)
    .upgrade(&RunOpts {
        package: vec![PatternGlob::new("a").expect("glob")],
        ..opts()
    })
    .await;

    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    assert!(
        !out.warnings
            .iter()
            .any(|warning| warning.kind == DiagnosticKind::AdvisoryRollback),
        "a still-fixed landing version must not warn: {:?}",
        out.warnings
    );
}

/// The common OSV shape expresses a reintroduction as *ranges alone* — `introduced 0`,
/// `fixed v1.0.2`, `introduced v3.0.0` — with no enumerated affected versions. The gate's
/// single-release classification cannot order those boundaries, so the commit-time capture
/// must re-classify the landing package against its full release list and still warn.
#[tokio::test]
async fn a_collateral_forward_move_onto_a_range_only_reintroduction_warns() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut eco = collateral_fixture(
        root,
        vec![
            rel("v1.0.2", 0, Some("2026-01-02T00:00:00Z"), None),
            rel(
                "v3.0.0",
                1,
                Some("2026-01-03T00:00:00Z"),
                Some(UpdateKind::Major),
            ),
            rel(
                "v3.1.0",
                2,
                Some("2026-01-04T00:00:00Z"),
                Some(UpdateKind::Major),
            ),
        ],
        rel("v1.0.2", 0, Some("2026-01-02T00:00:00Z"), None),
    );
    eco.collateral_on_apply = vec![collateral_move(
        "v1.0.2",
        "v3.1.0",
        UpdateKind::Major,
        false,
    )];
    let mut advisory = ghsa_fixed_by("GHSA-COLL", "v1.0.2");
    advisory.ranges = vec![RawAffectedRange {
        events: vec![
            cooldown_core::RawRangeEvent::Introduced("0".to_string()),
            cooldown_core::RawRangeEvent::Fixed("v1.0.2".to_string()),
            cooldown_core::RawRangeEvent::Introduced("v3.0.0".to_string()),
        ],
    }];
    assert!(
        advisory.affected_versions.is_empty(),
        "the reintroduction must be provable from the range alone"
    );
    let feed = Arc::new(StaticAdvisories {
        advisories: vec![("t".to_string(), advisory)],
        stale: false,
        unreachable: false,
    });
    let out = workspace_with_layers(
        eco,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(feed)
    .upgrade(&RunOpts {
        package: vec![PatternGlob::new("a").expect("glob")],
        ..opts()
    })
    .await;

    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    assert!(
        out.warnings.iter().any(|warning| {
            warning.kind == DiagnosticKind::AdvisoryRollback
                && warning.message.contains("moved t from v1.0.2")
                && warning.message.contains("GHSA-COLL")
                && warning.message.contains("affected again")
        }),
        "a range-only reintroduction must warn like an enumerated one: {:?}",
        out.warnings
    );
}

/// The planned twin of the range-only case: the newest matured candidate lands inside a
/// reintroduced range the advisory expresses without enumerating a single version.
#[tokio::test]
async fn a_planned_move_onto_a_range_only_reintroduction_warns() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.2", 0, Some("2026-01-02T00:00:00Z"), None),
            rel(
                "v1.5.0",
                1,
                Some("2026-01-03T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
            rel(
                "v1.9.0",
                2,
                Some("2026-01-04T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.2", 0, Some("2026-01-02T00:00:00Z"), None),
    );
    let eco = fake(
        root,
        vec![dep("a", "v1.0.2", true)],
        Vec::new(),
        releases,
        locked,
    );
    let mut advisory = ghsa_fixed_by("GHSA-CONF", "v1.0.2");
    advisory.ranges = vec![RawAffectedRange {
        events: vec![
            cooldown_core::RawRangeEvent::Introduced("0".to_string()),
            cooldown_core::RawRangeEvent::Fixed("v1.0.2".to_string()),
            cooldown_core::RawRangeEvent::Introduced("v1.5.0".to_string()),
        ],
    }];
    let ws = workspace_with_layers(
        eco,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(Arc::new(StaticAdvisories {
        advisories: vec![("a".to_string(), advisory)],
        stale: false,
        unreachable: false,
    }));
    let out = ws.upgrade(&opts()).await;

    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    let row = out
        .items
        .iter()
        .find(|item| item.name == "a" && item.applied)
        .expect("the planned applied row");
    assert_eq!(row.to, "v1.9.0");
    assert!(
        out.warnings.iter().any(|warning| {
            warning.kind == DiagnosticKind::AdvisoryRollback
                && warning.message.contains("moved a from v1.0.2")
                && warning.message.contains("GHSA-CONF")
                && warning.message.contains("affected again")
        }),
        "the planned range-only reintroduction must warn: {:?}",
        out.warnings
    );
}

/// A *planned* move abandons a fix the same way a collateral one does — the planner prefers
/// fixes but is deliberately not a vulnerability gate, so the newest matured candidate wins
/// even when the advisory lists it as affected again — and planned rows previously carried no
/// fix-loss check at all.
#[tokio::test]
async fn a_planned_move_onto_a_reintroduced_advisory_warns() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.2", 0, Some("2026-01-02T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-01-03T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.2", 0, Some("2026-01-02T00:00:00Z"), None),
    );
    let eco = fake(
        root,
        vec![dep("a", "v1.0.2", true)],
        Vec::new(),
        releases,
        locked,
    );
    let mut advisory = ghsa_fixed_by("GHSA-CONF", "v1.0.2");
    advisory.affected_versions = vec!["v1.1.0".to_string()];
    let ws = workspace_with_layers(
        eco,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(Arc::new(StaticAdvisories {
        advisories: vec![("a".to_string(), advisory)],
        stale: false,
        unreachable: false,
    }));
    let out = ws.upgrade(&opts()).await;

    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    let row = out
        .items
        .iter()
        .find(|item| item.name == "a" && item.applied)
        .expect("the planned applied row");
    assert_eq!(row.to, "v1.1.0");
    assert!(
        out.warnings.iter().any(|warning| {
            warning.kind == DiagnosticKind::AdvisoryRollback
                && warning.message.contains("moved a from v1.0.2")
                && warning.message.contains("GHSA-CONF")
        }),
        "the planned move off the fix must warn: {:?}",
        out.warnings
    );
}

/// A project whose matured `a` upgrade drags a fresh transitive `t` into the graph on apply —
/// the re-lock shape that introduces a package no initial advisory fetch could have asked about.
///
/// `t` is locked at `v0.5.0`, two days old: inside the 7d default window, past a 1d security
/// window.
fn relock_introduces_fresh_transitive(root: Utf8PathBuf) -> FakeEco {
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert(
        "t".to_string(),
        rel("v0.5.0", 0, Some("2026-06-15T00:00:00Z"), None),
    );
    FakeEco {
        fresh_transitive: Some(dep("t", "v0.5.0", false)),
        inject_fresh_on_apply: true,
        ..fake(
            root,
            vec![dep("a", "v1.0.0", true)],
            Vec::new(),
            releases,
            locked,
        )
    }
}

/// `fix` rolls a security bump younger than even the security window back to a version the same
/// advisory marks affected — and says so.
///
/// The verdict is deliberately unchanged: `fix` exists to make `check` pass, and the feed is not
/// a vulnerability gate (see the module docs).
/// Undoing a security fix *silently* is the part that would be indefensible, so the run names
/// the advisory and points at `baseline`.
#[tokio::test]
async fn fix_warns_when_a_rollback_re_enters_an_advisory() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let ws = workspace_with_layers(
        rollback_reentry_fake(root),
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(advisory_feed());
    let out = ws.fix(&opts()).await;

    let rolled_back = out
        .items
        .iter()
        .find(|item| item.name == "a")
        .expect("the planned rollback");
    assert_eq!(rolled_back.to, "v1.0.0", "the verdict is unchanged");
    assert!(
        out.warnings.iter().any(|warning| {
            warning.message.contains("re-enters GHSA-CONF") && warning.message.contains("baseline")
        }),
        "the rollback must name what it re-enters: {:?}",
        out.warnings
    );
}

/// Locked on `a@v1.1.0`, the half-day-old fix for `GHSA-CONF` — the pin `fix` wants to roll
/// back to the affected `v1.0.0`.
fn rollback_reentry_fake(root: Utf8PathBuf) -> FakeEco {
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            // The fix, merged half a day ago: younger than even the 1d security window.
            rel(
                "v1.1.0",
                1,
                Some("2026-06-16T12:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.1.0", 1, Some("2026-06-16T12:00:00Z"), None),
    );
    fake(
        root,
        vec![dep("a", "v1.1.0", true)],
        Vec::new(),
        releases,
        locked,
    )
}

/// A rollback warning may only survive if the rollback landed: when the batch is rolled back
/// after apply (here the post-apply graph probe fails, restoring the journal), the note parked
/// for it goes down with the batch instead of claiming a rollback that never happened.
#[tokio::test]
async fn a_rejected_rollback_takes_its_advisory_warning_down_with_it() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut fake = rollback_reentry_fake(root);
    fake.fail_graph_after_apply = true;
    let ws = workspace_with_layers(
        fake,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(advisory_feed());
    let out = ws.fix(&opts()).await;

    let rejected = out
        .items
        .iter()
        .find(|item| item.name == "a")
        .expect("the failed rollback row");
    assert!(
        rejected.error.is_some(),
        "the failed batch must be reported"
    );
    assert!(
        out.warnings
            .iter()
            .all(|warning| !warning.message.contains("GHSA-CONF")),
        "no rollback happened, so nothing was re-entered: {:?}",
        out.warnings
    );
}

/// `fix --dry-run` reports the re-entry in the conditional: nothing rolled back, so the
/// warning says what *would* happen, matching the mood of the dry run's own rows.
#[tokio::test]
async fn a_dry_run_rollback_warning_speaks_in_the_conditional() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let ws = workspace_with_layers(
        rollback_reentry_fake(root),
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(advisory_feed());
    let out = ws
        .fix(&RunOpts {
            dry_run: true,
            ..opts()
        })
        .await;

    assert!(
        out.warnings.iter().any(|warning| {
            warning
                .message
                .contains("would roll it back to v1.0.0, re-entering GHSA-CONF")
        }),
        "the dry run must speak in the conditional: {:?}",
        out.warnings
    );
}

/// `outdated` verifies adoptable rows with a real upgrade preview against a throwaway copy —
/// and publishes the preview's warnings.
/// A reconcile rollback inside that preview must therefore report like a dry run's: nothing of
/// the user's was rolled back, so the advisory warning speaks in the conditional.
#[tokio::test]
async fn an_outdated_preview_rollback_warning_speaks_in_the_conditional() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let t_releases = too_fresh_fix_releases();
    releases.insert("t".to_string(), t_releases.clone());
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert("t".to_string(), release_named(&t_releases, "v1.0.2"));
    // Reconcilable rather than forced, so the preview's trial rolls the floated `t` back —
    // re-entering the advisory its fresh pin fixes — instead of holding `a`.
    let mut floated = dep("t", "v1.0.2", false);
    floated.graph_floor = Some(Version::new("v1.0.0"));
    let fake = FakeEco {
        fresh_transitive: Some(floated),
        inject_fresh_on_apply: true,
        ..fake(
            root,
            vec![dep("a", "v1.0.0", true)],
            Vec::new(),
            releases,
            locked,
        )
    };
    let feed = Arc::new(StaticAdvisories {
        advisories: vec![("t".to_string(), ghsa_fixed_by("GHSA-T", "v1.0.2"))],
        stale: false,
        unreachable: false,
    });
    let ws = workspace_with_layers(fake, Baseline::default(), advisory_layers(None))
        .with_advisory_source(feed);
    let out = ws.outdated(&opts()).await;

    assert!(
        out.warnings.iter().any(|warning| {
            warning
                .message
                .contains("would roll it back to v1.0.1, re-entering GHSA-T")
        }),
        "the read-only run must speak in the conditional: {:?}",
        out.warnings
    );
    assert!(
        out.warnings
            .iter()
            .all(|warning| !warning.message.contains("re-enters GHSA-T")),
        "no active-voice rollback claim in a read-only run: {:?}",
        out.warnings
    );
}

/// A real run that terminates before the final lock verification (here: a failed edge
/// normalization) has an absent lock status — the same state a dry run reports.
/// The report carries the explicit dry-run flag so the two stay distinguishable: the committed
/// security fast-track must still render as adopted, not as a would-be.
#[tokio::test]
async fn a_terminated_real_run_is_not_reported_as_a_dry_run() {
    let TmpRoot { guard: _g, root } = tmp_root();
    std::fs::write(root.join("edge.snapshot"), b"run-start edge state").expect("seed edges");
    std::fs::write(root.join("fail-edge-normalization"), b"").expect("seed the failure marker");
    let ws = workspace_with_layers(
        advisory_fake(root.clone()),
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(advisory_feed());
    let out = ws.upgrade(&opts()).await;

    assert!(
        !out.errors.is_empty(),
        "the failed normalization must be reported"
    );
    let item = out
        .items
        .iter()
        .find(|item| item.name == "a")
        .expect("the fast-tracked adoption");
    assert!(item.applied, "the batch committed before the termination");
    assert!(item.security.as_ref().is_some_and(|s| s.applied));
    assert!(!out.meta.dry_run, "a real run, however it ended");
    assert!(
        out.meta.lock_status.is_none(),
        "terminated before the final verification"
    );
}

/// A top-up that fails is reported once, not once per fixpoint round: the feed that just failed
/// will fail the same way next round, so the executor stops asking about those packages.
///
/// The reconcile shape is what makes this observable — the gate runs again after the reconcile
/// pass, and `t` is still absent from the snapshot each time.
#[tokio::test]
async fn a_failed_top_up_is_reported_once_across_rounds() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let t_releases = too_fresh_fix_releases();
    releases.insert("t".to_string(), t_releases.clone());
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert("t".to_string(), release_named(&t_releases, "v1.0.2"));
    // Reconcilable rather than forced, so the run keeps going after the first gate instead of
    // rolling back and stopping.
    let mut floated = dep("t", "v1.0.2", false);
    floated.graph_floor = Some(Version::new("v1.0.0"));
    let fake = FakeEco {
        fresh_transitive: Some(floated),
        inject_fresh_on_apply: true,
        ..fake(
            root,
            vec![dep("a", "v1.0.0", true)],
            Vec::new(),
            releases,
            locked,
        )
    };

    let feed = Arc::new(FlakyAdvisories {
        advisories: vec![("a".to_string(), ghsa_fixed_by("GHSA-CONF", "v1.1.0"))],
        queries: Mutex::new(0),
    });
    let ws = workspace_with_layers(
        fake,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(Arc::<FlakyAdvisories>::clone(&feed) as Arc<dyn AdvisorySource>);
    let out = ws.upgrade(&opts()).await;

    let unavailable = out
        .warnings
        .iter()
        .filter(|warning| warning.kind == DiagnosticKind::AdvisorySourceUnavailable)
        .count();
    assert_eq!(unavailable, 1, "warnings: {:?}", out.warnings);
    assert_eq!(
        feed.queries(),
        2,
        "the initial fetch plus one top-up attempt, never one per round"
    );
}

/// An enabled feed with no source wired at all is reported by the mutating commands too, not
/// only by `check`: the run consulted no feed, so it must not look like an ordinary unadvised
/// upgrade.
#[tokio::test]
async fn upgrade_reports_an_enabled_feed_with_no_wired_source() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let ws = workspace_with_layers(
        advisory_fake(root),
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    );
    let out = ws.upgrade(&opts()).await;
    assert!(
        out.warnings
            .iter()
            .any(|warning| warning.kind == DiagnosticKind::AdvisorySourceUnavailable),
        "warnings: {:?}",
        out.warnings
    );
}

/// A re-lock introduces a package the initial fetch never asked about, so the snapshot grows to
/// cover it and the residual gate judges the new pin *advised*.
///
/// Its locked version is a security fix, so the security window applies and the batch stands.
/// Unadvised, that same young pin reads as an ordinary cooldown violation: the gate rolls the
/// upgrade back, and reconciliation then hunts for an older matured release — the very version
/// the fix superseded.
#[tokio::test]
async fn advisory_snapshot_grows_to_cover_a_relock_introduced_fix() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let fixture = || relock_introduces_fresh_transitive(root.clone());
    let feed = || {
        Arc::new(StaticAdvisories {
            advisories: vec![("t".to_string(), ghsa_fixed_by("GHSA-T", "v0.5.0"))],
            stale: false,
            unreachable: false,
        })
    };

    let ws = workspace_with_layers(
        fixture(),
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(feed());
    let out = ws.upgrade(&opts()).await;
    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    assert_eq!(
        out.summary.applied, 1,
        "the introduced fix must not gate the batch: {:?}",
        out.items
    );

    // Flag mode annotates but never shortens, so the same graph rolls back — the difference is
    // the security window applied to a package only the re-lock revealed.
    let flag_ws = workspace_with_layers(fixture(), Baseline::default(), advisory_layers(None))
        .with_advisory_source(feed());
    let flag_out = flag_ws.upgrade(&opts()).await;
    assert_eq!(flag_out.summary.applied, 0);
    assert_eq!(
        flag_out
            .items
            .first()
            .and_then(|item| item.skipped.as_ref())
            .map(|skipped| skipped.reason),
        Some(SkipReason::TransitiveInCooldown)
    );
}

/// A re-lock can also introduce a package whose source is *not* provably in the ecosystem.
/// Withholding its identity is correct — it must not inherit the public namesake's advisories,
/// so the security window never applies and the young pin gates the batch — but the narrowed
/// coverage must be said out loud, exactly once, not swallowed by the top-up's "nothing
/// queryable" early return.
#[tokio::test]
async fn a_relock_introduced_unproven_package_narrows_coverage_loudly() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let mut eco = relock_introduces_fresh_transitive(root);
    if let Some(dep) = eco.fresh_transitive.as_mut() {
        dep.advisory_identity = None;
    }
    let ws = workspace_with_layers(
        eco,
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(Arc::new(StaticAdvisories {
        advisories: vec![("t".to_string(), ghsa_fixed_by("GHSA-T", "v0.5.0"))],
        stale: false,
        unreachable: false,
    }));
    let out = ws.upgrade(&opts()).await;

    assert_eq!(
        out.summary.applied, 0,
        "an unproven source earns no security window, so the young pin gates the batch: {:?}",
        out.items
    );
    let narrowed: Vec<_> = out
        .warnings
        .iter()
        .filter(|warning| warning.kind == DiagnosticKind::AdvisoryEcosystemUnsupported)
        .collect();
    assert_eq!(
        narrowed.len(),
        1,
        "the narrowing is reported exactly once, not once per gate pass: {:?}",
        out.warnings
    );
    assert!(
        narrowed[0].message.contains("newly resolved"),
        "the report names the re-lock as the origin: {}",
        narrowed[0].message
    );
}

/// The check side of the shorten mode: a freshly locked fix version passes the gate through the
/// security window (and is tallied), where the same pin fails unadvised — "merging a security
/// bump stops failing the next gate run".
#[tokio::test]
async fn advisory_shorten_mode_passes_a_fresh_security_fix_pin_in_check() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let locked_at_fix = || {
        let mut locked = HashMap::new();
        locked.insert(
            "a".to_string(),
            rel("v1.1.0", 1, Some("2026-06-15T00:00:00Z"), None),
        );
        fake(
            root.clone(),
            vec![dep("a", "v1.1.0", true)],
            Vec::new(),
            HashMap::new(),
            locked,
        )
    };

    let unadvised = workspace(locked_at_fix(), Baseline::default())
        .check(&opts())
        .await;
    assert_eq!(unadvised.exit, Exit::Policy, "2d-old pin fails the 7d gate");

    let ws = workspace_with_layers(
        locked_at_fix(),
        Baseline::default(),
        advisory_layers(Some(AdvisoryMode::Shorten)),
    )
    .with_advisory_source(advisory_feed());
    let advised = ws.check(&opts()).await;
    assert_eq!(advised.exit, Exit::Ok, "the fix passes the security window");
    assert_eq!(advised.summary.violations, 0);
    assert_eq!(advised.summary.security_relevant, 1);
}

/// An unreachable feed fails open — `check` still passes and warns — unless the gate insists
/// via `--fail-on-advisory-source`, which makes it an environment error like a registry outage.
#[tokio::test]
async fn advisory_feed_outage_fails_open_unless_the_gate_insists() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let down = || {
        Arc::new(StaticAdvisories {
            advisories: Vec::new(),
            stale: false,
            unreachable: true,
        })
    };

    let ws = workspace_with_layers(
        advisory_fake(root.clone()),
        Baseline::default(),
        advisory_layers(None),
    )
    .with_advisory_source(down());
    let out = ws.check(&opts()).await;
    assert_eq!(out.exit, Exit::Ok, "fail-open: the stricter window stands");
    assert!(
        out.warnings
            .iter()
            .any(|warning| warning.kind == DiagnosticKind::AdvisorySourceUnavailable),
        "the outage must be loud: {:?}",
        out.warnings
    );

    let ws = workspace_with_layers(
        advisory_fake(root.clone()),
        Baseline::default(),
        advisory_layers(None),
    )
    .with_advisory_source(down());
    let out = ws
        .check(&RunOpts {
            advisory_failure: cooldown::app::AdvisoryFailureMode::Error,
            ..opts()
        })
        .await;
    assert_eq!(out.exit, Exit::Environment, "the gate refuses to certify");
}

/// The poisoned-feed bound, end to end through `outdated`: a repo `bypass-floor` lifts only the
/// repo layer's own floor, so a separate org (global) floor still clamps the security window and
/// the fix stays in cooldown.
/// Only a bypass in the org layer itself fast-tracks it.
#[tokio::test]
async fn advisory_bypass_floor_lifts_only_its_own_layers_floor() {
    let TmpRoot { guard: _g, root } = tmp_root();
    let floor_layer = |origin: Origin, bypass: bool| {
        let mut layer = PolicyLayer::new(origin);
        let mut rule = cooldown_core::Rule::new(cooldown_core::Selector::Default);
        rule.floor = Some(jiff::SignedDuration::from_hours(24 * 7));
        layer.rules.push(rule);
        if bypass {
            layer.advisories = Some(AdvisoryPolicy {
                bypass_floor: Some(true),
                ..AdvisoryPolicy::default()
            });
        }
        layer
    };
    let status_of = |layers: Vec<PolicyLayer>| {
        let root = root.clone();
        async move {
            let ws = workspace_with_layers(advisory_fake(root), Baseline::default(), layers)
                .with_advisory_source(advisory_feed());
            let out = ws.outdated(&opts()).await;
            out.items
                .iter()
                .find(|item| item.name == "a")
                .map(|item| item.status)
                .expect("the dep-a row")
        }
    };

    // A repo bypass beside the repo `[advisories]` table cannot lift the global floor.
    let mut repo = advisory_layer(Some(AdvisoryMode::Shorten));
    repo.advisories = repo.advisories.map(|advisories| AdvisoryPolicy {
        bypass_floor: Some(true),
        ..advisories
    });
    let clamped = status_of(vec![
        cooldown_core::config::builtin_default_layer(),
        floor_layer(Origin::Global, false),
        repo,
    ])
    .await;
    assert_eq!(
        clamped,
        OutdatedStatus::InCooldown,
        "the org floor survives a repo bypass"
    );

    // Declared in the floor's own layer, the same floor is lifted for the security window.
    let lifted = status_of(vec![
        cooldown_core::config::builtin_default_layer(),
        floor_layer(Origin::Global, true),
        advisory_layer(Some(AdvisoryMode::Shorten)),
    ])
    .await;
    assert_eq!(lifted, OutdatedStatus::Adoptable);
}
