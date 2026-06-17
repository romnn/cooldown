//! The ports (traits) and the I/O-facing types that cross them.
//!
//! [`Ecosystem`] is the one port the use cases speak to: it reads state, yields classified
//! releases, and executes changes — it never decides the cooldown (the core does) and never builds
//! a `Rule`/`WindowSpec` (window normalisation happens once, in [`normalize_native`]).
//! [`PackageRegistry`] is the finer-grained port each adapter is built from (constructor-injected,
//! reusable and fakeable in unit tests).

use crate::error::Result;
use crate::model::{
    ApplyReport, ArtifactId, DepScope, Dependency, EcosystemId, Plan, Project, Release,
    TargetContext, VerifyReport, Version,
};
use crate::policy::{Origin, PolicyLayer, Rule, Selector, WindowSpec};
use async_trait::async_trait;
use camino::Utf8Path;
use jiff::{SignedDuration, Timestamp};

/// What an adapter can express, so the conformance suite can capability-gate the right invariants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// The ecosystem has commit-pinned pseudo-versions (Go).
    pub has_pseudo: bool,
    /// The ecosystem has `+incompatible`-style adoptable-but-untagged releases (Go).
    pub has_incompatible: bool,
    /// The ecosystem has mutable dist-tags (npm `latest`).
    pub has_dist_tags: bool,
    /// `sync` can write the resolved policy back into native config.
    pub can_sync: bool,
    /// Releases are artifact-granular (a universal lock with per-file upload times, e.g. uv).
    pub artifact_granular: bool,
}

/// The one port the use cases speak to. Object-safe via `async_trait` so we can hold
/// `Box<dyn Ecosystem>`.
#[async_trait]
pub trait Ecosystem: Send + Sync {
    fn id(&self) -> EcosystemId;

    /// Capabilities, not opinions.
    fn capabilities(&self) -> Capabilities;

    /// Detect the projects of this ecosystem rooted under `root`.
    async fn detect(&self, root: &Utf8Path) -> Result<Vec<Project>>;

    /// The dependencies in scope (direct only, or the full resolved graph).
    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>>;

    /// Classified candidate releases (order + `kind_from_current` + publish times) via the
    /// registry. `ctx` supplies the project + target environment + artifact scope so each
    /// candidate's publish instant follows the candidate invariant.
    async fn releases(&self, dep: &Dependency, ctx: &TargetContext<'_>) -> Result<Vec<Release>>;

    /// The CURRENTLY-LOCKED version as a `Release`: its `quality` (== `dep.current_quality`) and
    /// the publish instant of its locked artifacts. This is what `check` evaluates for the pin.
    async fn locked_release(&self, dep: &Dependency, ctx: &TargetContext<'_>) -> Result<Release>;

    /// Native cooldown config translated into the unified rule model, each window left RAW so the
    /// core normalises absolute-vs-rolling exactly once. `Go => None`.
    async fn native_policy(&self, project: &Project) -> Result<Option<NativePolicyLayer>>;

    /// Apply the plan and report applied/skipped. Mechanics only (rewrites, MVS, resolver); **no
    /// intra-plan rollback** — the app drives trials/rollback. Skips are `Ok` data.
    async fn apply(&self, project: &Project, plan: &Plan) -> Result<ApplyReport>;

    /// OPT-IN compile/sync after re-locking (`--build`). `apply` already guarantees a consistent,
    /// resolvable lock; this is the expensive extra confidence step.
    async fn build(&self, project: &Project) -> Result<VerifyReport>;

    /// Verify the lock is current relative to its manifest (the fail-closed `check` precondition).
    async fn verify_lock_current(&self, project: &Project) -> Result<VerifyReport>;

    /// A snapshot token for the project's lock state, used by `upgrade` to restore after a trial.
    async fn snapshot_lock(&self, project: &Project) -> Result<LockSnapshot>;

    /// Restore a previously-taken lock snapshot.
    async fn restore_lock(&self, project: &Project, snapshot: &LockSnapshot) -> Result<()>;

    /// Write the resolved policy down into native config (`sync`; opt-in, post-MVP).
    async fn write_native(
        &self,
        _project: &Project,
        _policy: &ResolvedPolicy,
    ) -> Result<SyncReport> {
        Ok(SyncReport::Unsupported)
    }
}

/// An opaque snapshot of a project's lockfiles, taken before an `upgrade` trial so it can be
/// restored if the trial introduces a too-fresh transitive.
#[derive(Debug, Clone, Default)]
pub struct LockSnapshot {
    /// `(relative path, bytes)` for each lock-relevant file the adapter wants to be able to restore.
    pub files: Vec<(camino::Utf8PathBuf, Vec<u8>)>,
}

/// Native cooldown config, in the adapter's own structural terms.
#[derive(Debug, Clone)]
pub struct NativePolicyLayer {
    pub rules: Vec<NativeRule>,
}

/// One native rule: a selector and a raw (un-normalised) window.
#[derive(Debug, Clone)]
pub struct NativeRule {
    pub selector: Selector,
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

/// Convert a [`NativePolicyLayer`] into a normal [`PolicyLayer`] at [`Origin::Native`], exactly
/// once, per rule by selector.
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

/// The finer-grained registry port each adapter is built from.
#[async_trait]
pub trait PackageRegistry: Send + Sync {
    /// All known releases for a package, each carrying per-artifact upload times.
    async fn releases(&self, package: &crate::model::PackageId) -> Result<Vec<RawRelease>>;

    /// Publish instant of the LOCKED pin: for artifact-granular ecosystems the NEWEST of the given
    /// artifacts, but `None` if ANY of them has an unknown time (conservative → `UnknownAge`);
    /// version-level otherwise.
    async fn published_at(
        &self,
        pkg: &crate::model::PackageId,
        version: &Version,
        artifacts: &[ArtifactId],
    ) -> Result<Option<Timestamp>>;
}

/// A release as the registry reports it, before classification.
#[derive(Debug, Clone)]
pub struct RawRelease {
    pub version: Version,
    pub published_at: Option<Timestamp>,
    pub yanked: bool,
    /// Empty for version-granular ecosystems; populated (PyPI) for artifact-granular ones.
    pub artifacts: Vec<RawArtifact>,
}

/// One artifact within a release (a uv wheel/sdist), with its own upload time (or `None`).
#[derive(Debug, Clone)]
pub struct RawArtifact {
    pub id: ArtifactId,
    pub published_at: Option<Timestamp>,
    pub markers: Vec<String>,
}

/// The resolved policy handed to `write_native` for `sync` (post-MVP; minimal for now).
#[derive(Debug, Clone)]
pub struct ResolvedPolicy {
    pub default_window: Option<WindowSpec>,
}

/// The outcome of a `sync`/`write_native` (post-MVP).
#[derive(Debug, Clone)]
pub enum SyncReport {
    Unsupported,
    Written { path: camino::Utf8PathBuf },
    Deferred { tool: String },
}

/// A small helper for adapters: assert their `releases` output is sorted ascending by order, in
/// debug builds (the core relies on it).
pub fn debug_assert_sorted(releases: &[Release]) {
    debug_assert!(
        releases.windows(2).all(|w| w[0].order <= w[1].order),
        "adapter must return releases sorted ascending by ReleaseOrder"
    );
}

/// Re-export so adapters can refer to a project path type without importing camino directly.
pub type PathRef<'a> = &'a Utf8Path;
