//! The ports (traits) and the I/O-facing types that cross them.
//!
//! [`ToolRead`] is the read-side port the informational and gating use cases speak to: discovery,
//! dependency graphs, native policy, and lock-currency verification. [`ReleaseFetcher`] is the
//! registry-fetch port (classified releases and locked-release metadata), kept separate so the use
//! cases can only reach it through the run's release cache. [`ToolWrite`] is the mutation-side port
//! used only by commands that rewrite project state. [`PackageRegistry`] is the finer-grained port
//! each adapter is built from (constructor-injected, reusable and fakeable in unit tests).

use crate::error::{CoreError, Result};
use crate::model::{
    ApplyReport, ArtifactId, CandidateScope, Change, DepScope, Dependency, FetchContext,
    LockVerifyReport, Plan, Project, ProjectDetection, Release, ToolId, UpdateKind, VerifyReport,
    Version,
};
use crate::mutation::{AcceptedProjectState, ProjectMutationJournal, ProjectMutationState};
use crate::policy::{Origin, PolicyLayer, Rule, Selector, WindowSpec};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use jiff::{SignedDuration, Timestamp};
use std::sync::Arc;

/// What an adapter can express, so the conformance suite can capability-gate the right invariants.
///
/// Each field is an independent capability flag describing a feature an tool adapter
/// supports. The conformance suite reads these to decide which invariants apply: an tool
/// without pseudo-versions, for example, is never asked to classify one. The flags describe what
/// the adapter *can* do, never what policy *should* do — they carry no opinions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent tool capability flags; a bitflags/enum would obscure each named capability"
)]
pub struct Capabilities {
    /// The tool has commit-pinned pseudo-versions (Go).
    pub has_pseudo: bool,
    /// The tool has `+incompatible`-style adoptable-but-untagged releases (Go).
    pub has_incompatible: bool,
    /// The tool has mutable dist-tags (npm `latest`).
    pub has_dist_tags: bool,
    /// `sync` can write the resolved policy back into native config.
    pub can_sync: bool,
    /// Releases are artifact-granular (a universal lock with per-file upload times, e.g. uv).
    pub artifact_granular: bool,
}

/// The run's clock — the single source of the evaluation instant ("now").
///
/// Time is a port so the "now" boundary can be injected like any other dependency: production wires
/// a system clock, while tests and reproducible output (e.g. the README screenshots) wire a fixed
/// instant. The clock is sampled **once** at the start of a run and the resulting [`Timestamp`] is
/// threaded through the otherwise clock-free core, so every dependency in one run is judged against
/// the same "now" — sampling per call would let the instant drift mid-run. Implementations must be
/// `Send + Sync`.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Timestamp;
}

/// The read-side port the use cases speak to, implemented once per tool adapter.
///
/// An `ToolRead` reads native project state (its dependencies and native cooldown config) and
/// verifies that native lock state is current. It is deliberately mechanism-only: it never decides
/// the cooldown (the core does) and never builds a [`Rule`]/[`WindowSpec`] (window normalisation
/// happens once, in [`normalize_native`]).
///
/// The registry-fetch methods live on the separate [`ReleaseFetcher`] port, so code holding a
/// `dyn ToolRead` *cannot* fetch releases — and therefore cannot sidestep the run's release cache.
///
/// The trait is made object-safe via [`macro@async_trait`] so the use cases can hold a
/// `dyn ToolRead` and drive any tool uniformly. Implementations must be `Send + Sync`.
#[async_trait]
pub trait ToolRead: Send + Sync {
    /// Returns the stable identifier of this tool (e.g. Go, Cargo, uv).
    ///
    /// Used to label diagnostics and to route projects to the adapter that detected them.
    fn id(&self) -> ToolId;

    /// Returns the adapter's [`Capabilities`] — what it can express, not opinions.
    ///
    /// The conformance suite and use cases read these flags to capability-gate behaviour, so the
    /// returned value must accurately reflect the features this adapter actually supports.
    fn capabilities(&self) -> Capabilities;

    /// Declares how projects of this tool are detected below a scan root.
    ///
    /// The orchestrator performs one gitignore-aware, exclude-aware scan for both primary and
    /// validation-only markers.
    /// An adapter neither walks the tree nor decides `.gitignore` or exclude policy itself.
    fn project_detection(&self) -> ProjectDetection;

    /// Validates manifest roots found without the adapter's declared lockfile marker.
    ///
    /// The orchestrator calls this only for validation markers declared by
    /// [`Self::project_detection`].
    /// It lets an adapter audit shared discovery inputs once and reject unsupported alternate state
    /// without treating validation-only markers as detected projects.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) when a manifest selects project state outside the
    /// adapter's supported filesystem model.
    fn validate_manifests_without_lock(&self, roots: &[Utf8PathBuf]) -> Result<()> {
        if let Some(root) = roots.first() {
            return Err(CoreError::Config(format!(
                "tool `{}` declared a validation-only project marker at {root} without implementing its validator",
                self.id().as_str()
            )));
        }
        Ok(())
    }

    /// Classifies a version-to-version movement using the adapter's native version semantics.
    ///
    /// The core normally carries [`UpdateKind`] from registry release metadata. This hook is only for
    /// net rows synthesized after several lock movements collapse into one report row; adapters that
    /// cannot classify arbitrary version strings cheaply can return `None` and the caller will keep
    /// the original leg's kind.
    fn classify_update_kind(&self, _from: &str, _to: &str) -> Option<UpdateKind> {
        None
    }

    /// Returns the **raw, unscoped** resolved dependencies for `project`.
    ///
    /// `scope` selects between direct-only dependencies and the full resolved graph, but this method
    /// applies no `--package` scoping and no `exclude` policy — the orchestrator owns those (it knows
    /// the run's config; an adapter must not). So the result still contains excluded/out-of-scope
    /// packages and their full [`members`](Dependency::members).
    ///
    /// Reporting commands must therefore read deps through the orchestrator's scoped path (which
    /// drops excluded members and out-of-scope packages), never this method directly. The only
    /// legitimate direct callers are whole-graph reads that intentionally need every dependency — the
    /// upgrade graph-violation check and the `explain` registry lookup — and they never surface
    /// `members`.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the manifest or lock cannot be read or parsed.
    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>>;

    /// Returns direct dependency-like requirements that exist only as manifest constraints, with no
    /// entry in the resolved lock graph. These are opt-in command inputs for flows that can evaluate
    /// and mutate a manifest floor directly (`outdated`/`upgrade`), not lock-gate inputs: commands
    /// that verify or fix the resolved graph read only [`dependencies`](ToolRead::dependencies).
    ///
    /// Each returned [`Dependency`] carries the requirement's lower-bound floor as its
    /// [`current`](Dependency::current), with empty `artifacts` and no graph floor/ceiling. The
    /// default is empty for tools whose actionable packages are all lock-backed.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the manifest cannot be read or parsed.
    async fn manifest_constraints(&self, _project: &Project) -> Result<Vec<Dependency>> {
        Ok(Vec::new())
    }

    /// Returns the tool's native cooldown config translated into the unified rule model.
    ///
    /// Each window is left RAW (see [`RawWindow`]) so the core normalises absolute-vs-rolling
    /// exactly once via [`normalize_native`]. Tools without a native cooldown concept (Go)
    /// return `None`.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the native config exists but cannot be parsed.
    async fn native_policy(&self, project: &Project) -> Result<Option<NativePolicyLayer>>;

    /// Verifies the lock is current relative to its manifest — the fail-closed `check` precondition.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) when the probe itself fails. A known stale lock or
    /// unsupported currency proof is reported in the [`LockVerifyReport`].
    async fn verify_lock_current(&self, project: &Project) -> Result<LockVerifyReport>;
}

/// The registry-fetch port: classified candidate releases and the locked release for a dependency.
///
/// Split out from [`ToolRead`] on purpose. These are the only methods that hit a registry, and the
/// application layer must route every call through the run-scoped release cache (for single-flight
/// dedup across a workspace and across `upgrade` fixpoint rounds) and the rate-limited HTTP client.
/// To make that non-optional, the use cases are never handed a `dyn ReleaseFetcher`: they hold a
/// [`ToolRead`] (which cannot fetch) and reach releases only through the cache. "Forgetting to
/// cache" is then a compile error, not a code-review catch.
///
/// Adapters are typically assembled on top of the finer-grained [`PackageRegistry`] port. The trait
/// is object-safe via [`macro@async_trait`]; implementations must be `Send + Sync`.
///
/// # Contract
///
/// [`releases`](ReleaseFetcher::releases) must return its candidates sorted ascending by release
/// order — see [`debug_assert_sorted`], which the core relies on.
#[async_trait]
pub trait ReleaseFetcher: Send + Sync {
    /// Returns the classified candidate releases for `dep`, sorted ascending by release order.
    ///
    /// Each candidate carries its order, `kind_from_current`, and publish times, resolved via the
    /// underlying [`PackageRegistry`]. `fetch` supplies the project and artifact scope so each
    /// candidate's publish instant follows the candidate invariant (for artifact-granular tools,
    /// the instant reflects the artifacts selected by `fetch`).
    /// `candidates` communicates which candidate set the command actually cares about, so adapters
    /// such as Go can skip cross-major discovery unless it is in scope.
    ///
    /// Implementations must return the slice sorted ascending by order — see
    /// [`debug_assert_sorted`], which the core relies on.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the registry lookup fails.
    async fn releases(
        &self,
        dep: &Dependency,
        fetch: &FetchContext<'_>,
        candidates: CandidateScope,
    ) -> Result<Vec<Release>>;

    /// Applies dependency-specific declared-bound classification to cached candidate releases.
    ///
    /// The application calls this on its private clone after the shared registry lookup. Adapters
    /// that support explicit manifest upper bounds must classify them here instead of in
    /// [`releases`](Self::releases), because two projects can share a package/version cache key
    /// while declaring different requirements.
    fn classify_declared_bound(&self, _dep: &Dependency, _releases: &mut [Release]) {}

    /// Returns the currently-locked version of `dep` as a [`Release`].
    ///
    /// The returned release carries its `quality` (equal to `dep.current_quality`) and the publish
    /// instant of its locked artifacts. This is precisely what `check` evaluates for the pin.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the locked version cannot be read or its
    /// publish instant cannot be resolved.
    async fn locked_release(&self, dep: &Dependency, fetch: &FetchContext<'_>) -> Result<Release>;

    /// Derives the locked release from a candidate response when both registry operations are
    /// semantically equivalent.
    ///
    /// The default returns `None`: artifact- or project-sensitive registries may need a distinct
    /// lookup even when the candidate list contains the locked version. A registry adapter should
    /// opt in only when it can construct exactly the value [`locked_release`](Self::locked_release)
    /// would return, allowing the application cache to avoid the duplicate lookup.
    fn locked_release_from_candidates(
        &self,
        _dep: &Dependency,
        _releases: &[Release],
    ) -> Option<Release> {
        None
    }

    /// Whether this fetcher's results depend on the *asking project* (its lockfile, module graph, or
    /// resolved environment) rather than being a pure function of the package and version.
    ///
    /// The run-scoped release cache uses this to decide its key: a project-scoped fetcher is keyed
    /// per project, so two projects that share a `(package, version)` never serve each other's
    /// answer; a project-independent fetcher (a global registry index) is shared across the whole
    /// run. Defaults to `false` — correct for every registry-index adapter. Override to `true` when
    /// `releases`/`locked_release` read [`FetchContext::project`] or project-specific state beyond
    /// the declared bound handled by [`classify_declared_bound`](Self::classify_declared_bound)
    /// (e.g. Go's per-module `go list -m -versions`, uv's per-project locked artifact times).
    fn releases_are_project_scoped(&self) -> bool {
        false
    }
}

/// Observes candidate-level work hidden inside a tool's logical apply operation.
pub trait ApplyObserver: Send + Sync {
    /// Reports the candidate whose native resolver operation is about to start.
    ///
    /// Batch-oriented adapters may leave this at its no-op default. Adapters that expand one
    /// logical plan into sequential native commands should call it immediately before each command
    /// so an application can expose the otherwise-hidden current candidate.
    fn candidate_started(&self, _change: &Change) {}
}

impl ApplyObserver for () {}

/// How the application executes an adapter's resolver mutations.
#[derive(Clone, Copy)]
pub enum MutationExecution<'a> {
    /// Resolver trials run against the source project under the cooperative project lease.
    InPlace,
    /// The adapter prepares and publishes a faithful isolated project trial.
    Isolated(&'a dyn IsolatedMutationStrategy),
}

/// Prepares a tool-specific isolated project with a matching publication capability.
#[async_trait]
pub trait IsolatedMutationStrategy: Send + Sync {
    /// Stages every resolver input and output under the source project's captured coordination.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] when the source cannot be represented faithfully or changes while
    /// staging.
    async fn prepare(
        &self,
        source: &Project,
        coordination: &crate::fs::ProjectCoordination,
    ) -> Result<Box<dyn IsolatedMutation>>;
}

/// The source-visible state after publishing an accepted isolated trial.
#[derive(Debug)]
pub enum AcceptedPublication {
    /// Every accepted file is visible and no restart recovery remains pending.
    Published {
        /// Non-fatal durability or cleanup diagnostics.
        warnings: Vec<crate::Diagnostic>,
    },
    /// Every accepted file is visible, but a recovery marker still requires explicit cleanup.
    PublishedPendingRecovery {
        /// Non-fatal diagnostics recorded before marker cleanup became terminal.
        warnings: Vec<crate::Diagnostic>,
        /// The terminal recovery error that must be reported to the user.
        error: CoreError,
    },
}

/// How adapter-owned interrupted mutation state was settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDisposition {
    /// No interrupted mutation state was present.
    Unchanged,
    /// A completely published accepted candidate was retained.
    Accepted,
    /// A partial or speculative mutation was restored to its preimage.
    Restored,
    /// Only recovery artifacts remained and were consumed.
    CleanupOnly,
}

/// The settled recovery state and any non-fatal durability or cleanup diagnostics.
#[derive(Debug, Clone)]
pub struct MutationRecovery {
    /// The state selected by recovery.
    pub disposition: RecoveryDisposition,
    /// Non-fatal diagnostics produced after the visible state was settled.
    pub warnings: Vec<crate::Diagnostic>,
}

impl MutationRecovery {
    /// Constructs a recovery result without warnings.
    #[must_use]
    pub const fn settled(disposition: RecoveryDisposition) -> Self {
        MutationRecovery {
            disposition,
            warnings: Vec::new(),
        }
    }
}

/// A prepared isolated resolver trial tied to its accepted-state publisher.
#[async_trait]
pub trait IsolatedMutation: Send {
    /// The staged project that receives resolver mutations.
    fn project(&self) -> &Project;

    /// Captures the accepted outputs together with the source-input snapshot that authorized them.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] if an output is outside the publication set or cannot be captured.
    fn accepted_state(&self) -> Result<AcceptedProjectState>;

    /// Revalidates the trial's topology and source inputs, then publishes its accepted state.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] if the source changed or publication cannot complete safely.
    async fn publish(&self, accepted: &AcceptedProjectState) -> Result<AcceptedPublication>;
}

/// A project produced by an isolated mutation stage.
///
/// The private field prevents callers from wrapping an arbitrary [`Project`].
/// [`PreparedMutation::prepare_isolated`] separately checks that its tool family matches the
/// consuming writer and that the writer still declares isolated execution.
#[derive(Debug, Clone)]
pub struct IsolatedMutationProject {
    project: Project,
}

impl IsolatedMutationProject {
    /// The staged project represented by this capability.
    #[must_use]
    pub fn project(&self) -> &Project {
        &self.project
    }
}

impl dyn IsolatedMutation + '_ {
    /// Captures the staged-project capability needed by isolated-only mutation operations.
    #[must_use]
    pub fn mutation_project(&self) -> IsolatedMutationProject {
        IsolatedMutationProject {
            project: self.project().clone(),
        }
    }
}

/// The ownership state produced by one adapter apply attempt.
///
/// A finished resolver failure may still have rewritten files, so it carries the postimage an
/// outer journal may conditionally restore.
/// A pending-recovery outcome deliberately carries no postimage because adapter-owned recovery
/// evidence is the only authority allowed to restore that state.
#[derive(Debug)]
pub enum ApplyAttempt {
    /// The adapter released mutation ownership to the application.
    Finished {
        /// The adapter's apply result.
        report: Result<ApplyReport>,
        /// The write set captured immediately after the adapter finished mutating it.
        postimage: ProjectMutationState,
    },
    /// Adapter-owned recovery evidence still controls the project state.
    PendingRecovery {
        /// The reason recovery evidence remains authoritative.
        detail: String,
    },
}

/// A project, plan, and rollback journal validated as one mutation authority.
///
/// Callers prepare this value through [`PreparedMutation::prepare`] and may derive only checked
/// plan subsets from it.
/// Its private fields prevent a plan from being paired with another project's journal at a
/// mutation boundary.
/// Dispatch is bound to a tool family, not to one concrete adapter instance.
#[derive(Debug, Clone)]
pub struct PreparedMutation {
    project: Project,
    plan: Plan,
    journal: Arc<ProjectMutationJournal>,
    tool: ToolId,
    execution: PreparedMutationExecution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedMutationExecution {
    InPlace,
    Isolated,
}

impl PreparedMutation {
    /// Captures the adapter's write set and binds it to `project` and `plan`.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] when the writer requires isolated execution, cannot capture the
    /// write set, or produces a journal that does not authorize the supplied project.
    pub async fn prepare(writer: &dyn ToolWrite, project: &Project, plan: &Plan) -> Result<Self> {
        if !matches!(writer.mutation_execution(), MutationExecution::InPlace) {
            return Err(CoreError::LockConflict(format!(
                "{} mutations require an adapter-prepared isolated project",
                writer.mutation_tool().as_str()
            )));
        }
        Self::prepare_for(writer, project, plan, PreparedMutationExecution::InPlace).await
    }

    /// Captures and binds an operation for an adapter-created isolated project.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] when the adapter cannot capture the write set or the resulting
    /// journal does not authorize the staged project.
    pub async fn prepare_isolated(
        writer: &dyn ToolWrite,
        project: &IsolatedMutationProject,
        plan: &Plan,
    ) -> Result<Self> {
        if !matches!(writer.mutation_execution(), MutationExecution::Isolated(_)) {
            return Err(CoreError::LockConflict(
                "isolated project capability does not belong to an isolated mutation adapter"
                    .to_string(),
            ));
        }
        Self::prepare_for(
            writer,
            project.project(),
            plan,
            PreparedMutationExecution::Isolated,
        )
        .await
    }

    async fn prepare_for(
        writer: &dyn ToolWrite,
        project: &Project,
        plan: &Plan,
        execution: PreparedMutationExecution,
    ) -> Result<Self> {
        let tool = writer.mutation_tool();
        if project.kind != tool {
            return Err(CoreError::LockConflict(format!(
                "{} mutation authority cannot prepare a {} project",
                tool.as_str(),
                project.kind.as_str()
            )));
        }
        let journal = writer.mutation_journal(project, plan).await?;
        journal.validate_project(&project.root)?;
        Ok(PreparedMutation {
            project: project.clone(),
            plan: plan.clone(),
            journal: Arc::new(journal),
            tool,
            execution,
        })
    }

    /// Revalidates an in-place tool-family operation and its write set for one dispatch.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the writer's tool family or execution mode differs,
    /// or when the project identity or writable topology changed after preparation.
    pub fn parts_for(
        &self,
        writer: &(impl ToolWrite + ?Sized),
    ) -> Result<(&Project, &Plan, &ProjectMutationJournal)> {
        if self.execution != PreparedMutationExecution::InPlace
            || !matches!(writer.mutation_execution(), MutationExecution::InPlace)
        {
            return Err(CoreError::LockConflict(
                "operation requires in-place mutation execution".to_string(),
            ));
        }
        self.validated_parts_for(writer)
    }

    fn validated_parts_for(
        &self,
        writer: &(impl ToolWrite + ?Sized),
    ) -> Result<(&Project, &Plan, &ProjectMutationJournal)> {
        let tool = writer.mutation_tool();
        if tool != self.tool {
            return Err(CoreError::LockConflict(format!(
                "{} tool family cannot dispatch an operation prepared for {}",
                tool.as_str(),
                self.tool.as_str()
            )));
        }
        self.journal.validate_project(&self.project.root)?;
        Ok((&self.project, &self.plan, self.journal.as_ref()))
    }

    /// The plan carried by this prepared operation.
    #[must_use]
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// The project-bound rollback journal for this operation.
    #[must_use]
    pub fn journal(&self) -> &ProjectMutationJournal {
        self.journal.as_ref()
    }

    /// Revalidates and exposes an operation whose adapter requires isolated mutation execution.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the writer's tool family or execution mode differs,
    /// or when the prepared project's mutation authority is no longer valid.
    pub fn isolated_parts_for(
        &self,
        writer: &(impl ToolWrite + ?Sized),
    ) -> Result<(&Project, &Plan, &ProjectMutationJournal)> {
        if self.execution != PreparedMutationExecution::Isolated
            || !matches!(writer.mutation_execution(), MutationExecution::Isolated(_))
        {
            return Err(CoreError::LockConflict(
                "operation requires an adapter-prepared isolated mutation project".to_string(),
            ));
        }
        self.validated_parts_for(writer)
    }

    /// Derives a retry operation containing only changes authorized by this operation.
    ///
    /// All non-change plan policy remains identical to the prepared operation.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when `changes` contains a change absent from the
    /// prepared plan or repeats one more times than the prepared plan does.
    pub fn subset(&self, changes: Vec<Change>) -> Result<Self> {
        let mut available = self.plan.changes.clone();
        for change in &changes {
            let Some(position) = available.iter().position(|candidate| candidate == change) else {
                return Err(CoreError::LockConflict(format!(
                    "retry plan contains unauthorized mutation for {}",
                    change.package.name
                )));
            };
            available.remove(position);
        }
        let mut plan = self.plan.clone();
        plan.changes = changes;
        Ok(PreparedMutation {
            project: self.project.clone(),
            plan,
            journal: Arc::clone(&self.journal),
            tool: self.tool,
            execution: self.execution,
        })
    }
}

/// The mutation-side port for tools that can rewrite project state.
///
/// Read-only commands depend only on [`ToolRead`]. Commands such as `upgrade` and `sync` opt
/// into this narrower side explicitly so they are the only call sites coupled to rollback/build
/// mechanics.
#[async_trait]
pub trait ToolWrite: Send + Sync {
    /// Returns the stable tool-family identifier authorized to consume prepared mutations.
    fn mutation_tool(&self) -> ToolId;

    /// Selects whether resolver trials mutate the source project or a disposable copy.
    fn mutation_execution(&self) -> MutationExecution<'_> {
        MutationExecution::InPlace
    }

    /// Captures the current contents of only the files `plan` may mutate.
    ///
    /// The returned [`ProjectMutationJournal`] is the rollback token the application layer restores
    /// if the trial is rejected or if `apply` fails after mutating files.
    /// The journal is scoped to this exact `project` and `plan`, so adapters should capture the
    /// smallest file set they may rewrite under `project.root`.
    /// [`PreparedMutation::prepare`] binds the journal to the project and plan before handing the
    /// resulting capability to [`apply`](ToolWrite::apply).
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the relevant local files cannot be read.
    async fn mutation_journal(
        &self,
        project: &Project,
        plan: &Plan,
    ) -> Result<ProjectMutationJournal>;

    /// Applies a prepared operation and reports what was applied or skipped.
    ///
    /// Mechanics only (manifest rewrites, MVS, resolver runs).
    /// **Whole-plan** rollback belongs to
    /// the application layer: it captures a [`ProjectMutationJournal`] before calling `apply`,
    /// restores it if the trial is rejected, and verifies the resulting graph before
    /// committing/reporting a planned change as applied. An adapter MAY additionally restore that
    /// same journal internally to reject individual candidates whose landing it can prove wrong
    /// (the npm-family peer verification does), provided every rejected candidate is reported as
    /// a skip and the tree it returns is consistent — never a partially-applied state the caller
    /// cannot see. An adapter should still return only changes it believes reached their exact
    /// [`Change::to`](crate::model::Change::to) target, plus any collateral lock diff it can
    /// derive from the before/after lock state. Skips are reported as `Ok` data in the
    /// [`ApplyReport`], not errors.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`](crate::CoreError::LockConflict) when the prepared
    /// project's identity or writable topology changed.
    /// Returns another [`CoreError`](crate::CoreError) if the manifest cannot be rewritten or
    /// re-locking fails.
    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport>;

    /// Applies a prepared operation while reporting sequential native candidate work through
    /// `observer`.
    ///
    /// The default delegates to [`apply`](ToolWrite::apply), which is correct for adapters that run
    /// one native command for the whole plan. An adapter that internally invokes its resolver once
    /// per candidate should override this method, notify the observer before each invocation, and
    /// capture the returned postimage immediately after its final owned write.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the journal postimage cannot be captured.
    /// The adapter's own apply error remains inside [`ApplyAttempt::Finished`] so the caller
    /// receives the postimage even after a failed resolver mutates files.
    async fn apply_with_observer(
        &self,
        mutation: &PreparedMutation,
        _observer: &dyn ApplyObserver,
    ) -> Result<ApplyAttempt> {
        let report = self.apply(mutation).await;
        let postimage = mutation.journal().capture_state()?;
        Ok(ApplyAttempt::Finished { report, postimage })
    }

    /// Opt-in compile/sync after re-locking (the `--build` step).
    ///
    /// [`apply`](ToolWrite::apply) already guarantees a consistent, resolvable lock; this is the
    /// expensive extra confidence step that actually builds or syncs the project.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the build/sync invocation itself fails to run;
    /// a failed build is reported in the [`VerifyReport`].
    async fn build(&self, project: &Project) -> Result<VerifyReport>;

    /// Refreshes the lockfile before a read-only command evaluates it.
    ///
    /// This is opt-in for commands such as `check --lock` and `outdated --lock`: the caller has
    /// explicitly allowed a pre-read lockfile mutation so the following graph read can rely on the
    /// package manager's own resolver instead of merely probing a pre-existing lock. Adapters that
    /// cannot refresh locks independently return `None`.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the package manager cannot be spawned. A resolver
    /// failure is represented as a non-current [`LockVerifyReport`].
    async fn refresh_lock(&self, _project: &Project) -> Result<Option<LockVerifyReport>> {
        Ok(None)
    }

    /// Whether [`refresh_lock`](ToolWrite::refresh_lock) can perform a standalone lock refresh.
    ///
    /// The application uses this to avoid taking a project mutation lock and printing refresh
    /// progress for adapters whose default implementation is a no-op.
    fn supports_lock_refresh(&self) -> bool {
        false
    }

    /// Whether a successful [`apply`](ToolWrite::apply) run proves the adapter's lockfile is current.
    ///
    /// Some adapters cannot independently verify an arbitrary existing lock, but their mutating path
    /// delegates to the package manager's own lock refresh command. After that command succeeds, the
    /// lock is current for this run even if `check` must still fail closed on a pre-existing lock.
    fn successful_apply_proves_lock_current(&self) -> bool {
        false
    }

    /// Refuses a read when adapter-owned interrupted mutation state is pending.
    ///
    /// The application calls this while holding shared project access.
    /// Implementations must not recover or otherwise mutate project state through this read-side
    /// hook.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) describing the pending transaction.
    async fn ensure_no_pending_mutation(&self, _project: &Project) -> Result<()> {
        Ok(())
    }

    /// Recovers adapter-owned state left by an interrupted mutation.
    ///
    /// The application invokes this at the start of a mutation lifecycle while holding the
    /// project's exclusive mutation lock represented by `coordination`.
    /// Read-side adapter methods must never perform recovery; they should instead report pending
    /// state without modifying the project.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if pending state cannot be validated or recovered.
    async fn recover_pending_mutation(
        &self,
        _project: &Project,
        _coordination: &crate::fs::ProjectCoordination,
    ) -> Result<MutationRecovery> {
        Ok(MutationRecovery::settled(RecoveryDisposition::Unchanged))
    }

    /// Captures the adapter-owned lock state needed to report edge bindings across a whole run.
    ///
    /// The application treats a returned snapshot as evidence that
    /// [`normalize_lock_edges`](ToolWrite::normalize_lock_edges) can produce an authoritative final
    /// edge report.
    /// Adapters without ambiguous lock edges return [`None`].
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the adapter's edge-bearing lock state cannot be
    /// read.
    async fn lock_edge_snapshot(&self, _project: &Project) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Enforces the final lock **edge-binding** policy and reports its net relationship changes.
    ///
    /// `before` is the adapter snapshot captured before the run's first mutation.
    /// `committed` carries per-batch edge evidence whose corrective provenance may not be derivable
    /// from that snapshot when a target package was introduced during the run.
    /// Adapters reconcile both with the final saved lock, discarding superseded attempts.
    /// A healing policy such as
    /// [`EdgePolicy::Canonicalize`](crate::EdgePolicy) still runs when no version change landed.
    /// Adapters without ambiguous edge bindings keep the default no-op.
    /// A mutating implementation must require [`PreparedMutation::isolated_parts_for`] before
    /// touching the staged project.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the package manager cannot be spawned, the
    /// lock cannot be read, or the metadata the policy's requirement checks need cannot be fetched
    /// — a failed prerequisite must surface as the project error it is, never as a successful
    /// no-op.
    /// A correction that fails the adapter's own lock verification is rolled back and reported as
    /// held, not an error.
    async fn normalize_lock_edges(
        &self,
        _mutation: &PreparedMutation,
        _policy: crate::EdgePolicy,
        _before: Option<&[u8]>,
        _committed: &[crate::EdgeRebind],
    ) -> Result<crate::EdgeNormalizationReport> {
        Ok(crate::EdgeNormalizationReport::default())
    }

    /// Writes the resolved policy down into native config (the `sync` operation; opt-in, post-MVP).
    ///
    /// The default implementation returns [`SyncReport::Unsupported`]; adapters that can sync
    /// override it to write the [`ResolvedPolicy`] into their native cooldown config.
    ///
    /// When `dry_run` is set the adapter must compute and report what it *would* do
    /// ([`SyncReport::Written`] vs [`SyncReport::Unchanged`]) without touching any file.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the native config cannot be written.
    async fn write_native(
        &self,
        _project: &Project,
        _policy: &ResolvedPolicy,
        _dry_run: bool,
    ) -> Result<SyncReport> {
        Ok(SyncReport::Unsupported)
    }

    /// Where this adapter's native cooldown config lives, which decides how `sync` drives it.
    ///
    /// The default [`SyncScope::None`] is correct for tools without any native cooldown concept
    /// (Go, Cargo): `sync` writes nothing for them. Adapters whose native config is per-project
    /// override to [`SyncScope::Project`] (and implement [`write_native`](ToolWrite::write_native));
    /// adapters whose native config is a single repo-level file override to [`SyncScope::Repo`] (and
    /// implement [`write_repo_native`](ToolWrite::write_repo_native)).
    fn sync_scope(&self) -> SyncScope {
        SyncScope::None
    }

    /// Writes the resolved repo-wide policy into a single repo-level native config file (the `sync`
    /// operation for [`SyncScope::Repo`] adapters, e.g. uv's root `uv.toml`).
    ///
    /// Called **once per repo**, not per project.
    /// The application holds access to every project that consumes the shared file for the duration
    /// of the call.
    /// The default returns [`SyncReport::Unsupported`]; only [`SyncScope::Repo`] adapters override
    /// it.
    /// As with
    /// [`write_native`](ToolWrite::write_native), `dry_run` must report what it *would* do without
    /// touching any file.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the repo-level native config cannot be written.
    async fn write_repo_native(
        &self,
        _repo_root: &Utf8Path,
        _policy: &ResolvedPolicy,
        _dry_run: bool,
    ) -> Result<SyncReport> {
        Ok(SyncReport::Unsupported)
    }

    /// The files this adapter's resolver preview reads from a heuristic throwaway project copy.
    ///
    /// Only in-place adapters use this generic preview path.
    /// It excludes bulk source and data that can make monorepo copies prohibitively expensive.
    /// The default [`ResolveInputs::DEFAULT`] is the union of dependency metadata across supported
    /// managers, with no source files.
    /// Adapters may add source extensions needed by their preview resolver.
    /// An adapter that publishes isolated mutations instead supplies an
    /// [`IsolatedMutationStrategy`] with its own authoritative read set and topology.
    fn resolve_inputs(&self) -> ResolveInputs {
        ResolveInputs::DEFAULT
    }
}

/// Files copied into a generic resolver preview without cloning the whole project tree.
///
/// Inputs are matched by exact basename, explicit project-relative path prefix, or opted-in source
/// extension.
#[derive(Debug, Clone, Copy)]
pub struct ResolveInputs {
    /// Exact file basenames to copy wherever they appear in the tree.
    pub filenames: &'static [&'static str],
    /// Project-relative path prefixes to copy, for config files whose basename is too generic or
    /// whose resolver support files live below an otherwise-pruned dot directory.
    pub path_prefixes: &'static [&'static str],
    /// File extensions (without the leading dot) to copy as source.
    /// Go (`go`) and adapters with executable manifests validate resolver input against source, so
    /// their generic preview must include it; declaration-only resolvers leave this empty.
    pub source_extensions: &'static [&'static str],
}

impl ResolveInputs {
    /// Every manifest, lockfile, and workspace/registry-config basename across all supported managers.
    /// Copying the union is safe — a basename a given tool never produces simply never matches — and
    /// keeps [`ProjectCopy`](crate) tool-agnostic while still excluding all source and data.
    pub const FILENAMES: &'static [&'static str] = &[
        // npm family
        "package.json",
        "package-lock.json",
        "npm-shrinkwrap.json",
        "pnpm-lock.yaml",
        "pnpm-workspace.yaml",
        ".npmrc",
        ".pnpmfile.cjs",
        "yarn.lock",
        ".yarnrc",
        ".yarnrc.yml",
        "bun.lock",
        "bun.lockb",
        // deno
        "deno.json",
        "deno.jsonc",
        "deno.lock",
        // python / uv
        "pyproject.toml",
        "uv.lock",
        "uv.toml",
        ".python-version",
        "requirements.txt",
        "requirements.in",
        "setup.py",
        "setup.cfg",
        "Pipfile",
        "Pipfile.lock",
        "poetry.lock",
        // cargo
        "Cargo.toml",
        "Cargo.lock",
        // go
        "go.mod",
        "go.sum",
        "go.work",
        "go.work.sum",
        // ruby
        "Gemfile",
        "Gemfile.lock",
        // elixir / hex
        "mix.exs",
        "mix.lock",
        // maven
        "pom.xml",
        // swift
        "Package.swift",
        "Package.resolved",
        // conda
        "environment.yml",
        "environment.yaml",
    ];

    /// Resolver config paths that live below otherwise-pruned dot directories or have generic names.
    pub const PATH_PREFIXES: &'static [&'static str] = &[
        ".cargo/config",
        ".cargo/config.toml",
        ".swiftpm/configuration/registries.json",
        ".yarn/releases",
        ".yarn/plugins",
    ];

    /// The declaration-only default: every known manifest/lock/config basename, no source files.
    pub const DEFAULT: ResolveInputs = ResolveInputs {
        filenames: Self::FILENAMES,
        path_prefixes: Self::PATH_PREFIXES,
        source_extensions: &[],
    };
}

/// Where a tool's native cooldown config lives, which decides how `sync` drives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncScope {
    /// No native cooldown config at all (e.g. Go, Cargo); `sync` writes nothing.
    None,
    /// Native config lives in each project's own manifest; `sync` writes it per project via
    /// [`ToolWrite::write_native`].
    Project,
    /// A single repo-level native file (e.g. uv's root `uv.toml`); `sync` writes it exactly once per
    /// repo via [`ToolWrite::write_repo_native`].
    Repo,
}

/// Convenience bound for concrete adapters that implement all three ports.
///
/// Application registration is still explicit: a concrete adapter may be registered as read/fetch
/// only, or as a mutator too, so implementing [`ToolWrite`] does not accidentally make a new adapter
/// writable at runtime.
pub trait Tool: ToolRead + ReleaseFetcher + ToolWrite {}

impl<T> Tool for T where T: ToolRead + ReleaseFetcher + ToolWrite {}

/// Native cooldown config, in the adapter's own structural terms.
///
/// Produced by [`ToolRead::native_policy`] and consumed by [`normalize_native`], which converts
/// it into a unified [`PolicyLayer`] at [`Origin::Native`].
#[derive(Debug, Clone)]
pub struct NativePolicyLayer {
    /// The native rules, each pairing a [`Selector`] with a still-[`RawWindow`].
    pub rules: Vec<NativeRule>,
}

/// One native rule: a selector and a raw (un-normalised) window.
#[derive(Debug, Clone)]
pub struct NativeRule {
    /// What this rule matches (a package, a group, or everything).
    pub selector: Selector,
    /// The cooldown window, kept raw so the core normalises it exactly once.
    pub window: RawWindow,
}

/// A native window before normalisation — kept raw so the core converts absolute-vs-rolling once.
#[derive(Debug, Clone)]
pub enum RawWindow {
    /// e.g. uv `exclude-newer = "2026-06-01"`.
    AbsoluteDate(Timestamp),
    /// e.g. pnpm `minimumReleaseAge` minutes, uv `exclude-newer = "14 days"`.
    RelativeDuration(SignedDuration),
    /// e.g. uv `exclude-newer-package = false` — a per-package exemption.
    OptOut,
}

/// Converts a [`NativePolicyLayer`] into a normal [`PolicyLayer`] at [`Origin::Native`].
///
/// This is where the absolute-vs-rolling decision is made — exactly once, per rule, by selector —
/// so that the rest of the core sees only normalised [`WindowSpec`]s. A [`RawWindow::OptOut`]
/// becomes an allowing rule rather than a window. Performing this conversion here keeps every
/// [`Tool`] adapter free of window-normalisation logic.
#[must_use]
pub fn normalize_native(native: NativePolicyLayer) -> PolicyLayer {
    let mut layer = PolicyLayer::new(Origin::Native);
    for nr in native.rules {
        let mut rule = Rule::new(nr.selector);
        match nr.window {
            RawWindow::RelativeDuration(d) => rule.window.default = Some(WindowSpec::MinAge(d)),
            RawWindow::AbsoluteDate(t) => rule.window.default = Some(WindowSpec::Freeze(t)),
            RawWindow::OptOut => rule.allow = true,
        }
        layer.rules.push(rule);
    }
    layer
}

/// The finer-grained registry port each [`Tool`] adapter is built from.
///
/// Where [`Tool`] speaks in terms of projects and classified releases, a `PackageRegistry`
/// answers raw questions about a single package: what versions exist and when each was published.
/// It is constructor-injected into adapters, which makes it reusable across adapters and easy to
/// fake in unit tests. Implementations must be `Send + Sync`.
#[async_trait]
pub trait PackageRegistry: Send + Sync {
    /// Returns all known releases for `package`, each carrying per-artifact upload times.
    ///
    /// The returned [`RawRelease`]s are unclassified — ordering and `kind_from_current` are the
    /// adapter's job once it has the project's current pin.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the registry is unreachable or its response
    /// cannot be parsed.
    async fn releases(&self, package: &crate::model::PackageId) -> Result<Vec<RawRelease>>;

    /// Returns the publish instant of the locked pin, or `None` if it is unknown.
    ///
    /// For artifact-granular tools this is the NEWEST upload time among the given `artifacts`,
    /// but `None` if ANY of them has an unknown time — a conservative choice that the core maps to
    /// `UnknownAge`. For version-granular tools it is the version-level publish instant and
    /// `artifacts` is ignored.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the registry lookup fails.
    async fn published_at(
        &self,
        pkg: &crate::model::PackageId,
        version: &Version,
        artifacts: &[ArtifactId],
    ) -> Result<Option<Timestamp>>;
}

/// A release as the [`PackageRegistry`] reports it, before classification.
#[derive(Debug, Clone)]
pub struct RawRelease {
    /// The version this release publishes.
    pub version: Version,
    /// The version-level publish instant, or `None` if the registry does not report one.
    pub published_at: Option<Timestamp>,
    /// Whether the registry has yanked/retracted this release.
    pub yanked: bool,
    /// The per-artifact breakdown: empty for version-granular tools; populated (`PyPI`) for
    /// artifact-granular ones.
    pub artifacts: Vec<RawArtifact>,
}

/// One artifact within a release (a uv wheel/sdist), with its own upload time (or `None`).
#[derive(Debug, Clone)]
pub struct RawArtifact {
    /// Identifies this artifact within its release.
    pub id: ArtifactId,
    /// This artifact's own upload instant, or `None` if the registry does not report one.
    pub published_at: Option<Timestamp>,
    /// The environment markers gating this artifact (e.g. platform/Python-version constraints),
    /// used to select the artifacts relevant to a target environment.
    pub markers: Vec<String>,
}

/// The resolved policy handed to [`ToolWrite::write_native`] for `sync` (post-MVP; minimal for now).
#[derive(Debug, Clone)]
pub struct ResolvedPolicy {
    /// The default cooldown window to write into native config, if any.
    pub default_window: Option<WindowSpec>,
    /// Package selector globs the policy exempts from the cooldown (cooldown.toml `latest`/`allow`),
    /// to bake into a native per-package exemption list (pnpm's `minimumReleaseAgeExclude`). Empty for
    /// tools without such a native knob, which simply ignore it.
    pub exempt_packages: Vec<String>,
}

/// The outcome of a `sync`/[`ToolWrite::write_native`] (post-MVP).
#[derive(Debug, Clone)]
pub enum SyncReport {
    /// The adapter cannot sync; nothing was written. This is the default `write_native` result.
    Unsupported,
    /// The resolved policy was written to native config at `path`.
    Written {
        /// Path of the native config file that was written.
        path: camino::Utf8PathBuf,
    },
    /// The native config at `path` already matched the policy; nothing was rewritten.
    Unchanged {
        /// Path of the native config file that was already in sync.
        path: camino::Utf8PathBuf,
    },
    /// Writing was deferred to an external `tool` rather than performed in-process.
    Deferred {
        /// Name of the external tool the write was deferred to.
        tool: String,
    },
}

/// Asserts, in debug builds, that an adapter's [`releases`](ReleaseFetcher::releases) output is sorted
/// ascending by release order.
///
/// The core relies on this ordering invariant, so adapters should call this on the slice they are
/// about to return. The check is a [`debug_assert!`] and compiles to nothing in release builds.
///
/// # Panics
///
/// Panics in debug builds if `releases` is not sorted ascending by [`Release::order`].
pub fn debug_assert_sorted(releases: &[Release]) {
    // Compare adjacent pairs via zipped iterators rather than `windows(2)` + indexing, so there is
    // no slice indexing that could panic and trip `clippy::indexing_slicing`.
    debug_assert!(
        releases
            .iter()
            .zip(releases.iter().skip(1))
            .all(|(prev, next)| prev.order <= next.order),
        "adapter must return releases sorted ascending by ReleaseOrder"
    );
}

/// Re-export so adapters can refer to a project path type without importing camino directly.
pub type PathRef<'a> = &'a Utf8Path;
