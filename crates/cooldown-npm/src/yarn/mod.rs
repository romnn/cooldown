//! The `yarn` adapter, which serves both Yarn lock formats under one tool.
//!
//! Yarn Classic (v1) and Yarn Berry (v2+) share the `yarn` tool, its `[tool.yarn]` config, and the
//! `yarn.lock` file name, but their locks, commands, and configuration differ.
//! [`YarnTool`] therefore keeps one backend per lock format and routes every call to the backend
//! the project's own lock names: the lock on disk for a read, and the lock the prepared journal
//! captured for a mutation, so no format is cached across projects or trials.
//!
//! A lock's format says how to read it, not which Yarn runs it.
//! Before a resolver runs, the executable the project declares (`packageManager`) and the one
//! `yarn --version` reaches in the project, through any `yarnPath` or Corepack, must be able to
//! run the lock without migrating it ([`executable`]), and the live lock must still be of the
//! format the run was judged on.
//! Berry projects are read only: their mutation is refused before any resolver could run, and
//! `sync` writes only their native age gate ([`age_gate`]).
//!
//! `check` verifies a Berry lock with Yarn itself, in a copy of the project ([`verify`]), and
//! confirms afterwards that none of the inputs it verified changed while the dependencies were
//! read.
//! An advisory identity a Berry lock grants survives only where Yarn's effective routing names
//! the public registry for the package ([`routing`]).

mod age_gate;
mod executable;
mod routing;
mod verify;

use crate::lock::{NodeLock, Yarn, YarnBerry, is_yarn_berry_lock};
use crate::manifest::workspaces::{YarnConfiguration, YarnEnvironment};
use crate::nodecmd::NodeCmd;
use crate::registry::NpmRegistry;
use crate::tool::NpmTool;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::fs::{ManifestFamily, ProjectCoordination};
use cooldown_core::{
    ApplyAttempt, ApplyObserver, ApplyReport, CandidateScope, Capabilities, CoreError, Declaration,
    DepScope, Dependency, Diagnostic, EdgeNormalizationReport, EdgePolicy, EdgeRebind,
    FetchContext, LockVerifyReport, MemberRef, MutationBaseline, MutationExecution,
    MutationRecovery, NativePolicyLayer, NestedOwnership, Plan, PreparedMutation, Project,
    ProjectMarker, ProjectMutationJournal, Release, ReleaseFetcher, ResolveInputs, ResolvedPolicy,
    Result, SyncReport, SyncScope, TargetFallback, Tool, ToolId, ToolRead, ToolWrite, UpdateKind,
    VerifyReport,
};
use cooldown_registry::SharedHttp;
use std::collections::HashMap;

/// The format a `yarn.lock` is written in, which picks the backend that reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockFormat {
    /// The Yarn Classic (v1) grammar.
    Classic,
    /// The Yarn Berry (v2+) YAML document, marked by its top-level `__metadata` entry.
    Berry,
}

impl LockFormat {
    /// The format of the lock body `content`.
    fn of(content: &str) -> Self {
        if is_yarn_berry_lock(content) {
            LockFormat::Berry
        } else {
            LockFormat::Classic
        }
    }

    /// The format of the lock on disk at `root`.
    ///
    /// A project without a lock has nothing Berry about it, so it is Classic, whose backend
    /// reports the missing lock wherever one is needed.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Filesystem`] when the lock exists but cannot be read.
    fn on_disk(root: &Utf8Path) -> Result<Self> {
        let path = root.join(Yarn::LOCKFILE);
        match std::fs::read_to_string(&path) {
            Ok(content) => Ok(LockFormat::of(&content)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(LockFormat::Classic),
            Err(err) => Err(CoreError::Filesystem(format!("{path}: {err}"))),
        }
    }

    /// The format of the lock `journal` captured, Classic when it captured none.
    fn captured(journal: &ProjectMutationJournal) -> Self {
        crate::peers::journaled_lock::<Yarn>(journal).map_or(LockFormat::Classic, LockFormat::of)
    }

    /// The format's name, as a diagnostic spells it.
    fn describe(self) -> &'static str {
        match self {
            LockFormat::Classic => "Yarn Classic (v1)",
            LockFormat::Berry => "Yarn Berry (v2+)",
        }
    }
}

/// The `yarn` driver every Yarn run of the adapter goes through: `cmd` with Corepack's auto-pin
/// disabled.
///
/// Corepack 0.31 and older write a `packageManager` field into a root manifest that declares none
/// whenever they run Yarn there, unless `COREPACK_ENABLE_AUTO_PIN` is `0`.
/// That write lands outside every rollback cooldown keeps: a mutation journals the root manifest
/// only when the plan edits it, and the version probe and `--build` journal nothing.
fn yarn_driver(cmd: NodeCmd) -> NodeCmd {
    cmd.with_env("COREPACK_ENABLE_AUTO_PIN", "0")
}

/// The Yarn adapter (`yarn.lock`), reading Yarn Classic (v1) and Yarn Berry (v2+) locks.
///
/// A Classic lock records no workspace member, so the members come from the root `workspaces`
/// field and each member's direct dependencies are attributed through the lock entry its own
/// manifest selector names, which protects its exact manifest pins like npm's (see [`Yarn`]).
///
/// A Berry lock records its workspaces, so each member's direct dependencies are attributed
/// through the lock's own record of the member, joined to the member's manifest, and read through
/// the current manifests and Yarn configuration (see `YarnBerry`).
/// `upgrade`, `fix`, and `--build` refuse a Berry project before running any resolver, while
/// `sync` writes its native age gate into the project's `.yarnrc.yml`.
///
/// Either way, a resolver runs only under a Yarn that reads the lock in its own format: a Berry
/// `yarn` beside a Classic lock would migrate it, so that combination is refused with what to do
/// instead.
pub struct YarnTool {
    classic: NpmTool<Yarn>,
    berry: NpmTool<YarnBerry>,
    /// The `yarn` binary whose version the executable check asks for, the same one the backends
    /// run, and the one that verifies a Berry lock and reports its routing.
    driver: NodeCmd,
    /// The directories a Berry verification may create its copy in, in order of preference.
    verify_bases: Vec<Utf8PathBuf>,
    /// The Yarn environment a test verifies under, in place of this process's.
    yarn_environment: Option<YarnEnvironment>,
    /// The digest of the inputs each project root's Berry lock was verified current against,
    /// until [`ToolRead::confirm_verified_inputs`] checks it.
    verified: tokio::sync::Mutex<HashMap<Utf8PathBuf, verify::InputDigest>>,
    /// Yarn's routing per project root, asked once per run, `None` cached for a project whose
    /// routing Yarn cannot establish.
    routing: tokio::sync::Mutex<HashMap<Utf8PathBuf, Option<routing::Routing>>>,
}

impl YarnTool {
    /// Creates the adapter from a configured [`NpmRegistry`], which both backends share.
    #[must_use]
    pub fn new(registry: NpmRegistry) -> Self {
        // The system's temporary directory serves a project outside the home directory, and
        // cooldown's cache directory one inside it, wherever the temporary directory lies.
        let verify_bases = [
            Some(std::env::temp_dir()),
            Some(registry.cache_dir().join("yarn-verify")),
        ]
        .into_iter()
        .flatten()
        .filter_map(|base| Utf8PathBuf::from_path_buf(base).ok())
        .collect();
        YarnTool {
            classic: NpmTool::new(registry.clone())
                .with_driver(yarn_driver(NodeCmd::new(Yarn::BIN))),
            berry: NpmTool::new(registry).with_driver(yarn_driver(NodeCmd::new(YarnBerry::BIN))),
            driver: yarn_driver(NodeCmd::new(Yarn::BIN)),
            verify_bases,
            yarn_environment: None,
            verified: tokio::sync::Mutex::new(HashMap::new()),
            routing: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`NpmRegistry`].
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        YarnTool::new(NpmRegistry::new(http))
    }

    /// Enables package-document revalidation for version-adopting runs, as
    /// [`NpmTool::with_listing_revalidation`] does for each backend.
    #[must_use]
    pub fn with_listing_revalidation(self, revalidate: bool) -> Self {
        YarnTool {
            classic: self.classic.with_listing_revalidation(revalidate),
            berry: self.berry.with_listing_revalidation(revalidate),
            ..self
        }
    }

    /// Drives both backends and the executable check with the binary `bin`, so a test can
    /// substitute a scripted fake for `yarn`.
    ///
    /// The fake starts from Corepack's auto-pin switched on, the default of Corepack 0.31 and
    /// older, so a `COREPACK_ENABLE_AUTO_PIN=0` the developer's shell exports cannot hide a driver
    /// that leaves it on.
    #[cfg(all(test, unix))]
    fn with_driver(self, bin: &str) -> Self {
        let fake = || yarn_driver(NodeCmd::with_bin(bin).with_env("COREPACK_ENABLE_AUTO_PIN", "1"));
        YarnTool {
            classic: self.classic.with_driver(fake()),
            berry: self.berry.with_driver(fake()),
            driver: fake(),
            ..self
        }
    }

    /// Verifies Berry locks under `environment`, with copies created in `bases`, so a test
    /// controls the home directory and the scratch location.
    #[cfg(all(test, unix))]
    fn with_verification(self, environment: YarnEnvironment, bases: Vec<Utf8PathBuf>) -> Self {
        YarnTool {
            yarn_environment: Some(environment),
            verify_bases: bases,
            ..self
        }
    }

    /// The environment Yarn reads its configuration from.
    fn yarn_environment(&self) -> YarnEnvironment {
        self.yarn_environment
            .clone()
            .unwrap_or_else(YarnEnvironment::from_process)
    }

    fn verify_context(&self) -> verify::Context {
        verify::Context {
            environment: self.yarn_environment(),
            bases: self.verify_bases.clone(),
        }
    }

    /// Verifies `project`'s Berry lock and records the digest of the inputs a current verdict
    /// covers.
    async fn verify_berry_lock(&self, project: &Project) -> Result<LockVerifyReport> {
        let verification =
            verify::lock_current(&self.driver, project, &self.verify_context()).await?;
        let mut verified = self.verified.lock().await;
        match verification.digest {
            Some(digest) => verified.insert(project.root.clone(), digest),
            None => verified.remove(&project.root),
        };
        Ok(verification.report)
    }

    /// Withholds every provisional identity of a Berry project that Yarn's routing does not send
    /// to the public registry, and all of them when the routing cannot be established.
    async fn confirm_berry_identities(&self, project: &Project, deps: &mut [Dependency]) {
        if deps.iter().all(|dep| dep.advisory_identity.is_none()) {
            return;
        }
        let routing = self.berry_routing(&project.root).await;
        for dep in deps {
            let public = routing
                .as_ref()
                .is_some_and(|routing| !routing.reroutes(&dep.package.name));
            if !public {
                dep.advisory_identity = None;
            }
        }
    }

    /// Yarn's routing for the Berry project at `root`, asked once and memoized.
    ///
    /// A configured plugin may add resolvers or fetchers of its own, and an rc file Yarn would
    /// refuse configures nothing provable, so either establishes no routing without asking.
    async fn berry_routing(&self, root: &Utf8Path) -> Option<routing::Routing> {
        let mut cache = self.routing.lock().await;
        if let Some(cached) = cache.get(root) {
            return cached.clone();
        }
        let routing = match YarnConfiguration::read(root, &self.yarn_environment()) {
            Ok(_) => routing::query(&self.driver, root).await,
            Err(_) => None,
        };
        cache.insert(root.to_owned(), routing.clone());
        routing
    }

    /// The backend that reads locks of `format`.
    fn backend(&self, format: LockFormat) -> &dyn Tool {
        match format {
            LockFormat::Classic => &self.classic,
            LockFormat::Berry => &self.berry,
        }
    }

    /// The backend for the lock on disk at `project`'s root.
    fn backend_for(&self, project: &Project) -> Result<&dyn Tool> {
        Ok(self.backend(LockFormat::on_disk(&project.root)?))
    }

    /// The backend for `project` where the call has no error to report: the Classic one when the
    /// lock cannot be read, since both answer these calls alike.
    fn backend_or_classic(&self, project: &Project) -> &dyn Tool {
        LockFormat::on_disk(&project.root).map_or(&self.classic, |format| self.backend(format))
    }

    /// The backend that may run a resolver in `root` for the lock it was judged on, read as
    /// `judged`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::System`] for a Berry lock, whose mutation is not supported, and when
    /// the Yarn that would run in `root` cannot run a Classic lock without migrating it.
    /// Returns [`CoreError::LockConflict`] when the live lock is no longer of the judged format,
    /// leaving it as it is, ahead of any refusal of the Yarn executable.
    async fn admitted_backend(&self, root: &Utf8Path, judged: LockFormat) -> Result<&dyn Tool> {
        let probed = match judged {
            LockFormat::Classic => executable::ensure_classic_executable(&self.driver, root).await,
            LockFormat::Berry => Ok(()),
        };
        // The version probe runs Yarn, and anything else may write the lock meanwhile, so the
        // format the mutation was judged on is confirmed right before a backend writes: the
        // Classic backend would otherwise rewrite a Berry lock that replaced the judged one.
        // The probe's own refusal waits for that check, because a caller restores the captured
        // lock after a refusal but never after a conflict, and restoring would put the judged
        // Classic lock back over the Berry one.
        let live = LockFormat::on_disk(root)?;
        if live != judged {
            return Err(CoreError::LockConflict(format!(
                "yarn.lock changed from a {judged} to a {live} lockfile before cooldown ran Yarn, \
                 so it was left as it is; re-run once the lock has settled",
                judged = judged.describe(),
                live = live.describe()
            )));
        }
        probed?;
        match judged {
            LockFormat::Classic => Ok(&self.classic),
            LockFormat::Berry => Err(berry_mutation_unsupported()),
        }
    }

    /// The backend that may apply `mutation`, judged on the lock its journal captured.
    async fn applying_backend(&self, mutation: &PreparedMutation) -> Result<&dyn Tool> {
        let (project, _, journal) = mutation.parts_for(self)?;
        self.admitted_backend(&project.root, LockFormat::captured(journal))
            .await
    }
}

/// The refusal of every Berry mutation, which cooldown does not support yet.
fn berry_mutation_unsupported() -> CoreError {
    CoreError::System(
        "yarn.lock is a Yarn Berry (v2+) lockfile, which cooldown reads but cannot change yet: \
         `upgrade`, `fix`, and `--build` do not run for a Yarn Berry project"
            .into(),
    )
}

#[async_trait]
impl ToolRead for YarnTool {
    fn id(&self) -> ToolId {
        Yarn::ID
    }

    fn capabilities(&self) -> Capabilities {
        self.classic.capabilities()
    }

    fn project_marker(&self) -> ProjectMarker {
        self.classic.project_marker()
    }

    fn lease_family(&self, project: &Project) -> ManifestFamily {
        self.classic.lease_family(project)
    }

    fn nested_ownership(
        &self,
        primary: &[Utf8PathBuf],
        nested: &[Utf8PathBuf],
    ) -> Vec<NestedOwnership> {
        self.classic.nested_ownership(primary, nested)
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        self.classic.classify_update_kind(from, to)
    }

    fn advisory_version(&self, version: &str) -> String {
        self.classic.advisory_version(version)
    }

    async fn confirm_advisory_identities(&self, project: &Project, deps: &mut [Dependency]) {
        match LockFormat::on_disk(&project.root) {
            Ok(LockFormat::Berry) => self.confirm_berry_identities(project, deps).await,
            _ => {
                self.classic
                    .confirm_advisory_identities(project, deps)
                    .await;
            }
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        self.backend_for(project)?
            .dependencies(project, scope)
            .await
    }

    async fn manifest_notices(&self, project: &Project) -> Result<Vec<Diagnostic>> {
        self.backend_for(project)?.manifest_notices(project).await
    }

    async fn declarations(&self, project: &Project, name: &str) -> Result<Vec<Declaration>> {
        self.backend_for(project)?.declarations(project, name).await
    }

    async fn rescope_members(
        &self,
        project: &Project,
        deps: &mut [Dependency],
        excluded: &[MemberRef],
    ) -> Result<()> {
        self.backend_for(project)?
            .rescope_members(project, deps, excluded)
            .await
    }

    async fn manifest_constraints(&self, project: &Project) -> Result<Vec<Dependency>> {
        self.backend_for(project)?
            .manifest_constraints(project)
            .await
    }

    async fn native_policy(&self, project: &Project) -> Result<Option<NativePolicyLayer>> {
        self.backend_for(project)?.native_policy(project).await
    }

    async fn verify_lock_current(&self, project: &Project) -> Result<LockVerifyReport> {
        match LockFormat::on_disk(&project.root)? {
            LockFormat::Classic => self.classic.verify_lock_current(project).await,
            LockFormat::Berry => self.verify_berry_lock(project).await,
        }
    }

    /// A Berry lock verified current is confirmed against its inputs as they are now: planning
    /// the copy again must give the digest the verification recorded.
    /// An input that changed, one that appeared where the copy looks for inputs, or a lock that
    /// is no longer Berry means the dependencies just read may not be the ones Yarn verified.
    /// A change undone before this second read goes unnoticed, the same window pnpm's
    /// verify-then-read check leaves.
    async fn confirm_verified_inputs(&self, project: &Project) -> Result<()> {
        let Some(recorded) = self.verified.lock().await.remove(&project.root) else {
            return Ok(());
        };
        let unchanged = LockFormat::on_disk(&project.root)? == LockFormat::Berry
            && verify::live_digest(project, &self.verify_context())?.as_ref() == Some(&recorded);
        if unchanged {
            Ok(())
        } else {
            Err(CoreError::LockConflict(format!(
                "the inputs of yarn.lock in {} changed while cooldown verified them; re-run once \
                 they have settled",
                project.root
            )))
        }
    }

    async fn lock_inputs(&self, project: &Project) -> Result<Vec<Utf8PathBuf>> {
        self.backend_for(project)?.lock_inputs(project).await
    }
}

/// Registry lookups depend on the package alone, never on the lock format, so the Classic
/// backend answers them for every project.
#[async_trait]
impl ReleaseFetcher for YarnTool {
    async fn releases(
        &self,
        dep: &Dependency,
        fetch: &FetchContext<'_>,
        candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        self.classic.releases(dep, fetch, candidates).await
    }

    fn classify_declared_bound(&self, dep: &Dependency, releases: &mut [Release]) {
        self.classic.classify_declared_bound(dep, releases);
    }

    async fn locked_release(&self, dep: &Dependency, fetch: &FetchContext<'_>) -> Result<Release> {
        self.classic.locked_release(dep, fetch).await
    }

    fn locked_release_from_candidates(
        &self,
        dep: &Dependency,
        releases: &[Release],
    ) -> Option<Release> {
        self.classic.locked_release_from_candidates(dep, releases)
    }

    fn releases_are_project_scoped(&self) -> bool {
        self.classic.releases_are_project_scoped()
    }
}

#[async_trait]
impl ToolWrite for YarnTool {
    fn mutation_tool(&self) -> ToolId {
        Yarn::ID
    }

    fn mutation_execution(&self) -> MutationExecution<'_> {
        self.classic.mutation_execution()
    }

    fn guards_duplicate_copies(&self) -> bool {
        self.classic.guards_duplicate_copies()
    }

    fn mutation_installs(&self) -> bool {
        self.classic.mutation_installs()
    }

    fn supports_transitive_advance(&self) -> bool {
        self.classic.supports_transitive_advance()
    }

    fn target_fallback(&self) -> TargetFallback {
        self.classic.target_fallback()
    }

    fn external_resolve_roots(&self, project: &Project) -> Vec<Utf8PathBuf> {
        self.backend_or_classic(project)
            .external_resolve_roots(project)
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        self.backend_for(project)?
            .mutation_journal(project, plan)
            .await
    }

    async fn mutation_baseline(&self, project: &Project, plan: &Plan) -> Result<MutationBaseline> {
        self.backend_for(project)?
            .mutation_baseline(project, plan)
            .await
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        self.applying_backend(mutation).await?.apply(mutation).await
    }

    async fn apply_with_observer(
        &self,
        mutation: &PreparedMutation,
        observer: &dyn ApplyObserver,
    ) -> Result<ApplyAttempt> {
        match self.applying_backend(mutation).await {
            Ok(backend) => backend.apply_with_observer(mutation, observer).await,
            // A refused apply wrote nothing, and it reports like any failed apply, with the
            // postimage the caller restores from.
            Err(err) => {
                let postimage = mutation.journal().capture_state()?;
                mutation.finished_attempt(Err(err), &postimage)
            }
        }
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        let format = LockFormat::on_disk(&project.root)?;
        self.admitted_backend(&project.root, format)
            .await?
            .build(project)
            .await
    }

    async fn refresh_lock(&self, project: &Project) -> Result<Option<LockVerifyReport>> {
        self.backend_for(project)?.refresh_lock(project).await
    }

    async fn refresh_conflicted_lock(&self, project: &Project) -> Result<Option<LockVerifyReport>> {
        self.backend_for(project)?
            .refresh_conflicted_lock(project)
            .await
    }

    fn supports_lock_refresh(&self) -> bool {
        self.classic.supports_lock_refresh()
    }

    fn successful_apply_proves_lock_current(&self) -> bool {
        self.classic.successful_apply_proves_lock_current()
    }

    async fn ensure_no_pending_mutation(&self, project: &Project) -> Result<()> {
        self.backend_for(project)?
            .ensure_no_pending_mutation(project)
            .await
    }

    async fn recover_pending_mutation(
        &self,
        project: &Project,
        coordination: &ProjectCoordination,
    ) -> Result<MutationRecovery> {
        self.backend_for(project)?
            .recover_pending_mutation(project, coordination)
            .await
    }

    async fn lock_edge_snapshot(&self, project: &Project) -> Result<Option<Vec<u8>>> {
        self.backend_for(project)?.lock_edge_snapshot(project).await
    }

    async fn normalize_lock_edges(
        &self,
        mutation: &PreparedMutation,
        policy: EdgePolicy,
        before: Option<&[u8]>,
        committed: &[EdgeRebind],
    ) -> Result<EdgeNormalizationReport> {
        self.backend(LockFormat::captured(mutation.journal()))
            .normalize_lock_edges(mutation, policy, before, committed)
            .await
    }

    /// Only a Berry project has a native exemption list to admit young locked versions into, and
    /// only when its declared Yarn has the age gate at all.
    fn requires_sync_admission_evaluation(&self, project: &Project) -> bool {
        LockFormat::on_disk(&project.root).is_ok_and(|format| format == LockFormat::Berry)
            && !age_gate::declares_ungated_yarn(&project.root)
    }

    async fn write_native(
        &self,
        project: &Project,
        policy: &ResolvedPolicy,
        dry_run: bool,
    ) -> Result<SyncReport> {
        match LockFormat::on_disk(&project.root)? {
            LockFormat::Classic => self.classic.write_native(project, policy, dry_run).await,
            LockFormat::Berry => {
                let environment = age_gate::SyncEnvironment::from_process();
                age_gate::write(&self.driver, project, policy, &environment, dry_run).await
            }
        }
    }

    /// Yarn Berry's age gate lives in each project's own rc file, while a Classic project reports
    /// that it has none.
    fn sync_scope(&self) -> SyncScope {
        SyncScope::Project
    }

    async fn write_repo_native(
        &self,
        repo_root: &Utf8Path,
        policy: &ResolvedPolicy,
        dry_run: bool,
    ) -> Result<SyncReport> {
        self.classic
            .write_repo_native(repo_root, policy, dry_run)
            .await
    }

    fn resolve_inputs(&self) -> ResolveInputs {
        self.classic.resolve_inputs()
    }
}

#[cfg(test)]
mod tests;
