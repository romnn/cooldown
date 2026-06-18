//! The ports (traits) and the I/O-facing types that cross them.
//!
//! [`Ecosystem`] is the one port the use cases speak to: it reads state, yields classified
//! releases, and executes changes — it never decides the cooldown (the core does) and never builds
//! a `Rule`/`WindowSpec` (window normalisation happens once, in [`normalize_native`]).
//! [`PackageRegistry`] is the finer-grained port each adapter is built from (constructor-injected,
//! reusable and fakeable in unit tests).

use crate::error::Result;
use crate::model::{
    ApplyReport, ArtifactId, CandidateScope, DepScope, Dependency, EcosystemId, FetchContext, Plan,
    Project, Release, VerifyReport, Version,
};
use crate::policy::{Origin, PolicyLayer, Rule, Selector, WindowSpec};
use async_trait::async_trait;
use camino::Utf8Path;
use jiff::{SignedDuration, Timestamp};

/// What an adapter can express, so the conformance suite can capability-gate the right invariants.
///
/// Each field is an independent capability flag describing a feature an [`Ecosystem`] adapter
/// supports. The conformance suite reads these to decide which invariants apply: an ecosystem
/// without pseudo-versions, for example, is never asked to classify one. The flags describe what
/// the adapter *can* do, never what policy *should* do — they carry no opinions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent ecosystem capability flags; a bitflags/enum would obscure each named capability"
)]
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

/// The single port the use cases speak to, implemented once per ecosystem adapter.
///
/// An `Ecosystem` reads native project state, yields classified [`Release`]s, and executes plans.
/// It is deliberately mechanism-only: it never decides the cooldown (the core does) and never
/// builds a [`Rule`]/[`WindowSpec`] (window normalisation happens once, in [`normalize_native`]).
/// Adapters are typically assembled on top of the finer-grained [`PackageRegistry`] port.
///
/// The trait is made object-safe via [`macro@async_trait`] so the use cases can hold a
/// `Box<dyn Ecosystem>` and drive any ecosystem uniformly. Implementations must be `Send + Sync`.
///
/// # Contract
///
/// Implementors must uphold the invariants documented on each method. In particular, [`releases`]
/// must return candidates sorted ascending by release order (see [`debug_assert_sorted`]), and
/// [`apply`] must perform no intra-plan rollback — the application layer drives trials and
/// rollback using [`snapshot_lock`]/[`restore_lock`].
///
/// [`releases`]: Ecosystem::releases
/// [`apply`]: Ecosystem::apply
/// [`snapshot_lock`]: Ecosystem::snapshot_lock
/// [`restore_lock`]: Ecosystem::restore_lock
#[async_trait]
pub trait Ecosystem: Send + Sync {
    /// Returns the stable identifier of this ecosystem (e.g. Go, Cargo, uv).
    ///
    /// Used to label diagnostics and to route projects to the adapter that detected them.
    fn id(&self) -> EcosystemId;

    /// Returns the adapter's [`Capabilities`] — what it can express, not opinions.
    ///
    /// The conformance suite and use cases read these flags to capability-gate behaviour, so the
    /// returned value must accurately reflect the features this adapter actually supports.
    fn capabilities(&self) -> Capabilities;

    /// Detects the projects of this ecosystem rooted under `root`.
    ///
    /// Returns one [`Project`] per discovered manifest/lock pair under `root`, or an empty vector
    /// if this ecosystem is not present. Must not fail merely because no projects are found.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the filesystem cannot be walked or a manifest
    /// is present but malformed.
    async fn detect(&self, root: &Utf8Path) -> Result<Vec<Project>>;

    /// Returns the dependencies in scope for `project`.
    ///
    /// `scope` selects between direct-only dependencies and the full resolved graph. The returned
    /// list is what the core evaluates against policy.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the manifest or lock cannot be read or parsed.
    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>>;

    /// Returns the classified candidate releases for `dep`, sorted ascending by release order.
    ///
    /// Each candidate carries its order, `kind_from_current`, and publish times, resolved via the
    /// underlying [`PackageRegistry`]. `fetch` supplies the project, target environment, and
    /// artifact scope so each candidate's publish instant follows the candidate invariant (for
    /// artifact-granular ecosystems, the instant reflects the artifacts selected by `fetch`).
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

    /// Returns the ecosystem's native cooldown config translated into the unified rule model.
    ///
    /// Each window is left RAW (see [`RawWindow`]) so the core normalises absolute-vs-rolling
    /// exactly once via [`normalize_native`]. Ecosystems without a native cooldown concept (Go)
    /// return `None`.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the native config exists but cannot be parsed.
    async fn native_policy(&self, project: &Project) -> Result<Option<NativePolicyLayer>>;

    /// Applies `plan` to `project` and reports what was applied or skipped.
    ///
    /// Mechanics only (manifest rewrites, MVS, resolver runs); there is **no intra-plan rollback** —
    /// the application layer drives trials and rollback via [`snapshot_lock`](Ecosystem::snapshot_lock)
    /// and [`restore_lock`](Ecosystem::restore_lock). Skips are reported as `Ok` data in the
    /// [`ApplyReport`], not errors.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the manifest cannot be rewritten or re-locking
    /// fails.
    async fn apply(&self, project: &Project, plan: &Plan) -> Result<ApplyReport>;

    /// Opt-in compile/sync after re-locking (the `--build` step).
    ///
    /// [`apply`](Ecosystem::apply) already guarantees a consistent, resolvable lock; this is the
    /// expensive extra confidence step that actually builds or syncs the project.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the build/sync invocation itself fails to run;
    /// a failed build is reported in the [`VerifyReport`].
    async fn build(&self, project: &Project) -> Result<VerifyReport>;

    /// Verifies the lock is current relative to its manifest — the fail-closed `check` precondition.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if currency cannot be determined; a stale lock is
    /// reported in the [`VerifyReport`].
    async fn verify_lock_current(&self, project: &Project) -> Result<VerifyReport>;

    /// Captures an opaque [`LockSnapshot`] of the project's lock state.
    ///
    /// Used by `upgrade` to restore the lock after a trial via
    /// [`restore_lock`](Ecosystem::restore_lock).
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the lock-relevant files cannot be read.
    async fn snapshot_lock(&self, project: &Project) -> Result<LockSnapshot>;

    /// Restores a previously-taken [`LockSnapshot`], returning the project's lock to that state.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the snapshotted files cannot be written back.
    async fn restore_lock(&self, project: &Project, snapshot: &LockSnapshot) -> Result<()>;

    /// Writes the resolved policy down into native config (the `sync` operation; opt-in, post-MVP).
    ///
    /// The default implementation returns [`SyncReport::Unsupported`]; adapters that can sync
    /// override it to write the [`ResolvedPolicy`] into their native cooldown config.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the native config cannot be written.
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
///
/// Produced by [`Ecosystem::native_policy`] and consumed by [`normalize_native`], which converts
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
/// [`Ecosystem`] adapter free of window-normalisation logic.
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

/// The finer-grained registry port each [`Ecosystem`] adapter is built from.
///
/// Where [`Ecosystem`] speaks in terms of projects and classified releases, a `PackageRegistry`
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
    /// For artifact-granular ecosystems this is the NEWEST upload time among the given `artifacts`,
    /// but `None` if ANY of them has an unknown time — a conservative choice that the core maps to
    /// `UnknownAge`. For version-granular ecosystems it is the version-level publish instant and
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
    /// The per-artifact breakdown: empty for version-granular ecosystems; populated (`PyPI`) for
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

/// The resolved policy handed to [`Ecosystem::write_native`] for `sync` (post-MVP; minimal for now).
#[derive(Debug, Clone)]
pub struct ResolvedPolicy {
    /// The default cooldown window to write into native config, if any.
    pub default_window: Option<WindowSpec>,
}

/// The outcome of a `sync`/[`Ecosystem::write_native`] (post-MVP).
#[derive(Debug, Clone)]
pub enum SyncReport {
    /// The adapter cannot sync; nothing was written. This is the default `write_native` result.
    Unsupported,
    /// The resolved policy was written to native config at `path`.
    Written {
        /// Path of the native config file that was written.
        path: camino::Utf8PathBuf,
    },
    /// Writing was deferred to an external `tool` rather than performed in-process.
    Deferred {
        /// Name of the external tool the write was deferred to.
        tool: String,
    },
}

/// Asserts, in debug builds, that an adapter's [`releases`](Ecosystem::releases) output is sorted
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
