//! The ports (traits) and the I/O-facing types that cross them.
//!
//! [`ToolRead`] is the read-side port the informational and gating use cases speak to:
//! discovery, dependency graphs, classified releases, locked-release metadata, native policy, and
//! lock-currency verification. [`ToolWrite`] is the mutation-side port used only by commands
//! that rewrite project state. [`PackageRegistry`] is the finer-grained port each adapter is built
//! from (constructor-injected, reusable and fakeable in unit tests).

use crate::error::Result;
use crate::model::{
    ApplyReport, ArtifactId, CandidateScope, DepScope, Dependency, FetchContext, Plan, Project,
    ProjectMarker, Release, ToolId, VerifyReport, Version,
};
use crate::policy::{Origin, PolicyLayer, Rule, Selector, WindowSpec};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use jiff::{SignedDuration, Timestamp};

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

/// The read-side port the use cases speak to, implemented once per tool adapter.
///
/// An `ToolRead` reads native project state, yields classified [`Release`]s, and verifies
/// that native lock state is current. It is deliberately mechanism-only: it never decides the
/// cooldown (the core does) and never builds a [`Rule`]/[`WindowSpec`] (window normalisation
/// happens once, in [`normalize_native`]). Adapters are typically assembled on top of the
/// finer-grained [`PackageRegistry`] port.
///
/// The trait is made object-safe via [`macro@async_trait`] so the use cases can hold a
/// `dyn ToolRead` and drive any tool uniformly. Implementations must be `Send + Sync`.
///
/// # Contract
///
/// Implementors must uphold the invariants documented on each method. In particular, [`releases`]
/// must return candidates sorted ascending by release order (see [`debug_assert_sorted`]).
///
/// [`releases`]: ToolRead::releases
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

    /// Detects the projects of this tool rooted under `root`.
    ///
    /// Declares the filesystem [marker](ProjectMarker) that identifies this tool's project
    /// roots. The orchestrator performs a single gitignore-aware, exclude-aware scan from it, so an
    /// adapter neither walks the tree nor decides `.gitignore`/exclude policy itself — that concern
    /// lives in one agnostic place and is enforced by this interface.
    fn project_marker(&self) -> ProjectMarker;

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
    /// artifact-granular tools, the instant reflects the artifacts selected by `fetch`).
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
    /// Returns a [`CoreError`](crate::CoreError) if currency cannot be determined; a stale lock is
    /// reported in the [`VerifyReport`].
    async fn verify_lock_current(&self, project: &Project) -> Result<VerifyReport>;
}

/// The mutation-side port for tools that can rewrite project state.
///
/// Read-only commands depend only on [`ToolRead`]. Commands such as `upgrade` and `sync` opt
/// into this narrower side explicitly so they are the only call sites coupled to rollback/build
/// mechanics.
#[async_trait]
pub trait ToolWrite: Send + Sync {
    /// Captures the current contents of only the files `plan` may mutate.
    ///
    /// The returned [`ProjectMutationJournal`] is the rollback token the application layer restores
    /// if the trial is rejected or if `apply` fails after mutating files. The journal is scoped to
    /// this exact `plan`, so adapters should capture the smallest file set they may rewrite. The
    /// same journal is then handed back to [`apply`](ToolWrite::apply), so adapters may also
    /// treat it as the precomputed write-set for the trial.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the relevant local files cannot be read.
    async fn mutation_journal(
        &self,
        project: &Project,
        plan: &Plan,
    ) -> Result<ProjectMutationJournal>;

    /// Applies `plan` to `project` and reports what was applied or skipped.
    ///
    /// Mechanics only (manifest rewrites, MVS, resolver runs); there is **no intra-plan rollback** —
    /// the application layer captures a [`ProjectMutationJournal`] before calling `apply` and
    /// restores it if the trial is rejected. Skips are reported as `Ok` data in the
    /// [`ApplyReport`], not errors.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the manifest cannot be rewritten or re-locking
    /// fails.
    async fn apply(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport>;

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

/// Convenience bound for concrete adapters that implement both the read-side and write-side ports.
pub trait Tool: ToolRead + ToolWrite {}

impl<T> Tool for T where T: ToolRead + ToolWrite {}

/// The pre-change contents of the files a planned mutation may rewrite.
#[derive(Debug, Clone, Default)]
pub struct ProjectMutationJournal {
    /// The captured file entries the application layer can restore on rollback.
    pub files: Vec<ProjectMutationFile>,
}

/// One file entry recorded in a [`ProjectMutationJournal`].
#[derive(Debug, Clone)]
pub struct ProjectMutationFile {
    /// The path relative to the project root.
    pub path: Utf8PathBuf,
    /// The captured bytes when the file existed, or `None` when it was absent and must be removed
    /// on restore.
    pub contents: Option<Vec<u8>>,
}

impl ProjectMutationJournal {
    /// Capture the current contents of one project-relative file.
    ///
    /// Missing files are recorded as `None`, which tells [`restore`](Self::restore) to remove them
    /// if a trial created them.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the file exists but cannot be read.
    pub fn capture_file(root: &Utf8Path, rel: &Utf8Path) -> Result<ProjectMutationFile> {
        let path = root.join(rel);
        let contents = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        Ok(ProjectMutationFile {
            path: rel.to_owned(),
            contents,
        })
    }

    /// Restore every captured file entry under `root`.
    ///
    /// Entries whose captured contents are `None` are removed if they now exist.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if a file cannot be written back or removed.
    pub fn restore(&self, root: &Utf8Path) -> Result<()> {
        for file in &self.files {
            let path = root.join(&file.path);
            match &file.contents {
                Some(bytes) => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(path, bytes)?;
                }
                None => match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                },
            }
        }
        Ok(())
    }
}

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
    /// Writing was deferred to an external `tool` rather than performed in-process.
    Deferred {
        /// Name of the external tool the write was deferred to.
        tool: String,
    },
}

/// Asserts, in debug builds, that an adapter's [`releases`](ToolRead::releases) output is sorted
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
