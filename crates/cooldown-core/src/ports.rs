//! The ports (traits) and the I/O-facing types that cross them.
//!
//! [`ToolRead`] is the read-side port the informational and gating use cases speak to: discovery,
//! dependency graphs, native policy, and lock-currency verification. [`ReleaseFetcher`] is the
//! registry-fetch port (classified releases and locked-release metadata), kept separate so the use
//! cases can only reach it through the run's release cache. [`ToolWrite`] is the mutation-side port
//! used only by commands that rewrite project state. [`PackageRegistry`] is the finer-grained port
//! each adapter is built from (constructor-injected, reusable and fakeable in unit tests).

use crate::error::Result;
use crate::model::{
    ApplyReport, ArtifactId, CandidateScope, DepScope, Dependency, FetchContext, LockVerifyReport,
    Plan, Project, ProjectMarker, Release, ToolId, UpdateKind, VerifyReport, Version,
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

    /// Detects the projects of this tool rooted under `root`.
    ///
    /// Declares the filesystem [marker](ProjectMarker) that identifies this tool's project
    /// roots. The orchestrator performs a single gitignore-aware, exclude-aware scan from it, so an
    /// adapter neither walks the tree nor decides `.gitignore`/exclude policy itself — that concern
    /// lives in one agnostic place and is enforced by this interface.
    fn project_marker(&self) -> ProjectMarker;

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

    /// Whether this fetcher's results depend on the *asking project* (its lockfile, module graph, or
    /// resolved environment) rather than being a pure function of the package and version.
    ///
    /// The run-scoped release cache uses this to decide its key: a project-scoped fetcher is keyed
    /// per project, so two projects that share a `(package, version)` never serve each other's
    /// answer; a project-independent fetcher (a global registry index) is shared across the whole
    /// run. Defaults to `false` — correct for every registry-index adapter. Override to `true` when
    /// `releases`/`locked_release` read [`FetchContext::project`] or per-project [`Dependency`] state
    /// (e.g. Go's per-module `go list -m -versions`, uv's per-project locked artifact times).
    fn releases_are_project_scoped(&self) -> bool {
        false
    }
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
    /// the application layer captures a [`ProjectMutationJournal`] before calling `apply`, restores
    /// it if the trial is rejected, and verifies the resulting graph before committing/reporting a
    /// planned change as applied. An adapter should still return only changes it believes reached
    /// their exact [`Change::to`](crate::model::Change::to) target, plus any collateral lock diff it
    /// can derive from the before/after lock state. Skips are reported as `Ok` data in the
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
    /// Called **once per repo**, not per project, so concurrent project upgrades never race on the
    /// shared file. The default returns [`SyncReport::Unsupported`]; only [`SyncScope::Repo`]
    /// adapters override it. As with [`write_native`](ToolWrite::write_native), `dry_run` must report
    /// what it *would* do without touching any file.
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

    /// The files this adapter's mutating resolve reads, so the throwaway [`ProjectCopy`] probe (used by
    /// `outdated`'s blocked-verification and `--dry-run`) copies ONLY these — never the full source/data
    /// tree.
    ///
    /// The probe copies a project into a tempdir to run the real resolver without touching the real
    /// lock. Copying everything is catastrophic in a large monorepo (gigabytes of assets, model
    /// weights, and build data into a tempdir that is often tmpfs/RAM), and the resolver needs none of
    /// it — only the dependency metadata in manifests, lockfiles, and workspace/registry config. The
    /// default [`ResolveInputs::DEFAULT`] is the union of those across every supported manager (a
    /// basename a tool never produces simply never matches), with no source files. Cargo and Go
    /// override it to add their source extension, because those resolvers validate crate/module targets
    /// and the import graph against the actual source — an empty `src/` would make the resolve error.
    fn resolve_inputs(&self) -> ResolveInputs {
        ResolveInputs::DEFAULT
    }
}

/// The files a tool's mutating resolve actually reads — manifests, lockfiles, workspace/registry
/// config, and selected source — so the application can stage a throwaway project copy without
/// cloning the whole tree. Inputs are matched by exact basename, explicit project-relative path
/// prefix, or opted-in source extension.
#[derive(Debug, Clone, Copy)]
pub struct ResolveInputs {
    /// Exact file basenames to copy wherever they appear in the tree.
    pub filenames: &'static [&'static str],
    /// Project-relative path prefixes to copy, for config files whose basename is too generic or
    /// whose resolver support files live below an otherwise-pruned dot directory.
    pub path_prefixes: &'static [&'static str],
    /// File extensions (without the leading dot) to copy as source. Cargo (`rs`) and Go (`go`)
    /// validate their targets/import graph against the real source, so the probe must include it;
    /// declaration-only resolvers (pnpm, uv, deno) leave this empty.
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
    /// repo via [`ToolWrite::write_repo_native`], so concurrent project upgrades never race on it.
    Repo,
}

/// Convenience bound for concrete adapters that implement all three ports.
///
/// Application registration is still explicit: a concrete adapter may be registered as read/fetch
/// only, or as a mutator too, so implementing [`ToolWrite`] does not accidentally make a new adapter
/// writable at runtime.
pub trait Tool: ToolRead + ReleaseFetcher + ToolWrite {}

impl<T> Tool for T where T: ToolRead + ReleaseFetcher + ToolWrite {}

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
