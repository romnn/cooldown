//! The core domain model. Versions are **opaque to the core**: Go pseudo-versions, `/vN` majors,
//! `+incompatible`, PEP 440 and semver share no parse rules, so the core never parses a version —
//! the tool hands back releases already classified, carrying an opaque ordering token and the
//! update-kind relative to the current pin.

use crate::duration::since;
use crate::policy::ResolvedWindow;
use camino::Utf8PathBuf;
use std::fmt;

/// Canonical display form of a version. The core treats this as opaque; it never parses it.
///
/// Go pseudo-versions, `/vN` majors, `+incompatible`, PEP 440 and semver share no parse
/// rules, so a `Version` is just the string an tool chose to display. Ordering and
/// same-major comparisons go through the opaque [`ReleaseOrder`] and [`MajorKey`] tokens
/// instead, never through this string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct Version(
    /// The verbatim display string, exactly as the tool produced it.
    pub String,
);

impl Version {
    /// Wraps a string in a [`Version`].
    ///
    /// The string is stored verbatim; the core never parses or normalises it.
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::Version;
    ///
    /// let v = Version::new("1.2.3");
    /// assert_eq!(v.as_str(), "1.2.3");
    /// ```
    pub fn new(s: impl Into<String>) -> Self {
        Version(s.into())
    }

    /// Returns the version's display string.
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::Version;
    ///
    /// assert_eq!(Version::new("v0.1.0").as_str(), "v0.1.0");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An opaque "same major?" token, compared for **equality only** — never ordered. `--major` gates
/// same-major vs cross-major jumps with this; the minor/patch distinction comes from
/// [`Release::kind_from_current`].
///
/// Two releases share a major when their `MajorKey`s are equal. Because the token is only
/// ever tested for equality, the tool is free to encode the major however it likes
/// (e.g. `"1"`, `"v2"`, the module path for a Go `/vN` major).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MajorKey(
    /// The opaque major identifier; only compared for equality.
    pub String,
);

/// An opaque total-order token, meaningful only **within one package**. The core sorts and compares
/// releases with this; it carries a `debug_assert` of sortedness at the port boundary.
///
/// Ordering follows the natural lexicographic ordering of the byte vector, which the
/// tool constructs so that "newer" sorts greater. Tokens from different packages are
/// not comparable in any meaningful way.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReleaseOrder(
    /// The opaque ordering bytes; sorted lexicographically, newest greatest.
    pub Vec<u8>,
);

/// An tool identifier, registered by its adapter. `Copy + 'static` so it threads cheaply.
///
/// The wrapped string is the stable tool name used in config (`[tool.<name>]`) and on the `--tool`
/// flag — cooldown is organized by the dependency tool it drives (`cargo`, `go`, `uv`), not the
/// language. See [`RECOGNIZED_TOOLS`] and [`tool_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToolId(
    /// The stable tool name, e.g. `"cargo"` or `"go"`.
    pub &'static str,
);

impl ToolId {
    /// Returns the tool's stable tool name.
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::ToolId;
    ///
    /// assert_eq!(ToolId("cargo").as_str(), "cargo");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

/// The tools `cooldown` recognises in config (`[tool.<name>]`) and `--tool`, by the stable name of
/// the tool it drives. Each JavaScript package manager is its own tool (they pin different lockfile
/// formats), so `npm`, `pnpm`, `yarn`, and `bun` are all first-class, while a genuine typo
/// (`[tool.carg]`) is still rejected.
pub const RECOGNIZED_TOOLS: &[ToolId] = &[
    ToolId("cargo"),
    ToolId("go"),
    ToolId("uv"),
    ToolId("npm"),
    ToolId("pnpm"),
    ToolId("yarn"),
    ToolId("bun"),
    ToolId("deno"),
    ToolId("bundler"),
    ToolId("hex"),
    ToolId("maven"),
    ToolId("gradle"),
    ToolId("pip"),
    ToolId("poetry"),
    ToolId("conda"),
    ToolId("pixi"),
    ToolId("swift"),
];

/// Returns the canonical tool names as a comma-separated string for diagnostics.
#[must_use]
pub fn recognized_tool_names() -> String {
    RECOGNIZED_TOOLS
        .iter()
        .map(ToolId::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve a tool name (or a common alias) to its canonical [`ToolId`], or `None` if unrecognised.
/// Accepts the language name and sibling tools as aliases: `rust`/`crates` → cargo,
/// `python`/`pip`/`pypi` → uv, `golang` → go, and `node`/`js` → npm (the default JS manager).
#[must_use]
pub fn tool_id(name: &str) -> Option<ToolId> {
    let canonical = match name {
        "cargo" | "crates" | "rust" => "cargo",
        "go" | "golang" => "go",
        "uv" | "pypi" | "python" => "uv",
        "pip" => "pip",
        "poetry" => "poetry",
        "conda" | "mamba" | "micromamba" => "conda",
        "pixi" => "pixi",
        "swift" | "spm" | "swiftpm" => "swift",
        "npm" | "node" | "js" | "javascript" | "typescript" => "npm",
        "pnpm" => "pnpm",
        "yarn" => "yarn",
        "bun" => "bun",
        "deno" => "deno",
        "bundler" | "bundle" | "ruby" | "gem" | "rubygems" => "bundler",
        "hex" | "mix" | "elixir" => "hex",
        "maven" | "mvn" => "maven",
        "gradle" => "gradle",
        _ => return None,
    };
    RECOGNIZED_TOOLS.iter().copied().find(|e| e.0 == canonical)
}

impl fmt::Display for ToolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl serde::Serialize for ToolId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0)
    }
}

/// A fully-qualified package identity: which tool, the package name, and (optionally) the
/// registry/index it resolves from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackageId {
    /// The tool the package belongs to.
    pub tool: ToolId,
    /// The package name as it appears in the tool's index.
    pub name: String,
    /// The registry/index the package resolves from (e.g. `crates.io`), or `None` for the
    /// tool's default.
    pub registry: Option<String>,
}

impl PackageId {
    /// Assembles a [`PackageId`] from its tool, name, and optional registry.
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::{ToolId, PackageId};
    ///
    /// let id = PackageId::new(ToolId("cargo"), "serde", None);
    /// assert_eq!(id.name, "serde");
    /// assert_eq!(id.tool.as_str(), "cargo");
    /// assert!(id.registry.is_none());
    /// ```
    pub fn new(tool: ToolId, name: impl Into<String>, registry: Option<String>) -> Self {
        PackageId {
            tool,
            name: name.into(),
            registry,
        }
    }
}

/// The quality classification an adapter assigns each release. `Incompatible` (Go `+incompatible`)
/// is adoptable; `Prerelease` is excluded unless the current pin is itself a prerelease; `Pseudo`
/// (a commit pin) is `Held` in `outdated` and exempt in `check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseQuality {
    /// A normal tagged release; the usual adoption target.
    Stable,
    /// A prerelease (alpha/beta/rc); excluded unless the current pin is itself a prerelease.
    Prerelease,
    /// A commit pin (Go pseudo-version); [`Status::Held`] in `outdated` and exempt in `check`.
    Pseudo,
    /// A Go `+incompatible` release; adoptable, treated as stable-like.
    Incompatible,
}

impl ReleaseQuality {
    /// Returns `true` for the "real release" qualities adoption normally targets.
    ///
    /// [`Stable`](ReleaseQuality::Stable) and [`Incompatible`](ReleaseQuality::Incompatible)
    /// are stable-like; [`Prerelease`](ReleaseQuality::Prerelease) and
    /// [`Pseudo`](ReleaseQuality::Pseudo) are not.
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::ReleaseQuality;
    ///
    /// assert!(ReleaseQuality::Stable.is_stable_like());
    /// assert!(ReleaseQuality::Incompatible.is_stable_like());
    /// assert!(!ReleaseQuality::Prerelease.is_stable_like());
    /// ```
    #[must_use]
    pub fn is_stable_like(self) -> bool {
        matches!(self, ReleaseQuality::Stable | ReleaseQuality::Incompatible)
    }
}

/// The update kind of a candidate relative to the current pin. `Copy + Eq`, deliberately **no
/// `Ord`** — kinds are categories, not a scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateKind {
    /// A cross-major jump (different [`MajorKey`]).
    Major,
    /// A same-major change that is not a patch.
    Minor,
    /// A same-major patch-level change.
    Patch,
}

/// A non-empty id for one locked artifact (e.g. a uv wheel/sdist). Version-granular tools (Go,
/// crates.io) leave `Dependency::artifacts` empty.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ArtifactId(
    /// The non-empty artifact identifier (e.g. a wheel/sdist filename).
    pub String,
);

/// A classified release. The `published_at` aggregate is the newest upload over the *selected*
/// artifacts (environment-relevant, else all), but `None` if **any** selected artifact's time is
/// unknown — conservative: a partially-known release is never treated as mature.
#[derive(Debug, Clone)]
pub struct Release {
    /// The release's display version.
    pub version: Version,
    /// The opaque ordering token used to sort releases within the package.
    pub order: ReleaseOrder,
    /// The opaque same-major token, compared for equality with the current pin's.
    pub major: MajorKey,
    /// The update kind relative to the current pin, or `None` when not comparable (e.g. a
    /// commit pin).
    pub kind_from_current: Option<UpdateKind>,
    /// The newest upload time over the selected artifacts, or `None` if any selected
    /// artifact's time is unknown.
    pub published_at: Option<jiff::Timestamp>,
    /// Whether the release has been yanked/withdrawn.
    pub yanked: bool,
    /// The quality classification the adapter assigned.
    pub quality: ReleaseQuality,
}

/// A resolved dependency to be evaluated. `current_quality` lets `evaluate` apply the prerelease
/// rule in the core. INVARIANT: `current_quality == locked_release(dep, ctx).quality` (the adapter
/// derives both from the same lock entry). `graph_floor` is the lowest version the resolved graph
/// permits (MVS floor / a `=` pin), read from the lock.
#[derive(Debug, Clone)]
pub struct Dependency {
    /// The dependency's package identity.
    pub package: PackageId,
    /// The currently-locked version.
    pub current: Version,
    /// The quality of the currently-locked release; mirrors `locked_release(dep, ctx).quality`.
    pub current_quality: ReleaseQuality,
    /// Whether this is a direct dependency (as opposed to transitive).
    pub direct: bool,
    /// The locked artifacts for this dependency; empty for version-granular tools.
    pub artifacts: Vec<ArtifactId>,
    /// The lowest version the resolved graph permits (MVS floor or a `=` pin), read from the
    /// lock; `None` when unconstrained.
    pub graph_floor: Option<Version>,
    /// The highest version the resolved graph permits — symmetric to [`graph_floor`](Self::graph_floor).
    /// Set when a *requirer* pins this dependency exactly (e.g. another package's `protobuf==6.33.5`
    /// caps a transitive `protobuf`), so it cannot be upgraded past this version even though newer
    /// releases exist; `evaluate` then reports it [`Status::Held`]. `None` means unbounded above (the
    /// common case — most deps can move up). A direct manifest pin is captured by
    /// [`pinned`](Self::pinned) instead; this field is for the *transitive* ceiling the graph imposes.
    ///
    /// Invariant every adapter upholds: when set, this equals [`current`](Self::current) — an exact
    /// `==`/`=` pin forces the dependency to resolve to exactly that version, so an *active* ceiling is
    /// always the resolved version (adapters confirm the pin matches `current` before recording it).
    /// `evaluate` relies on this: a ceiling above the fetched releases would be silently uncapped, and
    /// `check_pin` treats a ceiling at the locked version as graph-held in both directions.
    pub graph_ceiling: Option<Version>,
    /// The workspace member package(s) that declare this dependency at this resolved version — e.g.
    /// cargo member crates, pnpm/npm workspace packages, the uv project itself. Reports attribute the
    /// dependency to these packages (by name, or by path under `--paths`). Empty when the adapter
    /// cannot attribute a source (a transitive dep, or a tool without per-member data); the
    /// presentation then leaves the column blank.
    pub members: Vec<MemberRef>,
    /// The dependency is exact-pinned in the manifest (`==x.y.z`, cargo `=x.y.z`, a bare npm
    /// version), so it will not move without editing the manifest. Such a pin is still evaluated for
    /// context when newer candidates exist (so `adoptable_target` can show the newest matured
    /// version), but then its headline status is [`Status::Held`] and `upgrade` will not mutate it.
    /// The `outdated --hide-pinned` flag only filters these rows from the human table; `check` gates
    /// a pinned dep's age like any other pin.
    pub pinned: bool,
}

/// A workspace member that declares a dependency: its package `name` and its `path` relative to the
/// project/workspace root. Reports show `name` by default and `path` under `--paths`. The root is
/// recorded as `.` (rendered `./`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MemberRef {
    /// The package/crate name (e.g. `@airtype/admin`, `airtype-acl-api`).
    pub name: String,
    /// The member's directory relative to the workspace root (the root is `.`).
    pub path: String,
}

/// The status of a dependency or pin. Note **graph-held is not a status**: it is a `graph_held`
/// flag on a [`Status::CurrentInCooldown`] violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// No newer adoptable version exists.
    UpToDate,
    /// A newer version exists and has matured past its window.
    Adoptable,
    /// A newer version exists but is younger than its window.
    InCooldown,
    /// Exempted by an `allow` rule (or, in `check`, a pseudo/commit pin).
    Exempt,
    /// Pinned, so it will not move on its own: a commit pin (pseudo-version, no tagged version to
    /// compare against) or an exact `==`/`=` manifest pin.
    Held,
    /// The currently-locked version is itself younger than its window (the `check` violation).
    CurrentInCooldown,
    /// The relevant release has no known publish time.
    UnknownAge,
}

/// The per-candidate verdict. The decision is per candidate — a patch can be adoptable while a
/// major still cools.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The candidate version.
    pub version: Version,
    /// The update kind relative to the current pin.
    pub kind: UpdateKind,
    /// The cooldown window resolved for this candidate.
    pub window: ResolvedWindow,
    /// The verdict for this candidate.
    pub status: Status,
    /// The candidate's publish instant, threaded through for rendering (`ageDays`).
    pub published_at: Option<jiff::Timestamp>,
}

impl Candidate {
    /// How long this candidate must still wait to mature past its window at `now`: the gap between
    /// its publish instant and the window [`cutoff`](ResolvedWindow::cutoff). Positive while the
    /// candidate is cooling, non-positive once it has matured. `None` when the publish time is
    /// unknown — an [`UnknownAge`](Status::UnknownAge) candidate never matures, so it has no
    /// countdown. Used to order cooling candidates for [`CooldownHorizon::Soonest`].
    fn time_to_mature(&self, now: jiff::Timestamp) -> Option<jiff::SignedDuration> {
        self.published_at
            .map(|published| since(published, self.window.cutoff(now)))
    }
}

/// Which still-cooling upgrade the `outdated` report's cooldown countdown tracks when more than one
/// newer version exists. Both variants leave the *decision* untouched — what is adoptable and the
/// headline [`Status`] are unchanged; they only choose which candidate's `age/window` the report
/// surfaces in its cooldown column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CooldownHorizon {
    /// Track the newest eligible candidate — "how long until the latest version matures".
    Latest,
    /// Track the still-cooling candidate that matures first — "how long until the *next* upgrade
    /// unlocks". When an intermediate version clears its window days before the newest release does,
    /// this surfaces that nearer date instead. Falls back to the newest candidate when nothing is
    /// currently cooling. The default: the soonest unlock is the more actionable countdown.
    #[default]
    Soonest,
}

/// The aggregate verdict for a dependency over its candidate set.
#[derive(Debug, Clone)]
pub struct Verdict {
    /// The aggregate status over the candidate set.
    pub status: Status,
    /// The newest candidate that has matured past its window, if any.
    pub adoptable_target: Option<Version>,
    /// The newest existing version, adoptable or not.
    pub latest: Option<Version>,
    /// The per-candidate verdicts in ascending release order; the newest candidate is last.
    pub candidates: Vec<Candidate>,
}

impl Verdict {
    /// The candidate whose cooldown countdown the `outdated` report should display under `horizon`.
    ///
    /// [`Latest`](CooldownHorizon::Latest) is the newest candidate (the last one).
    /// [`Soonest`](CooldownHorizon::Soonest) is the still-[`InCooldown`](Status::InCooldown)
    /// candidate that will mature first — useful when an intermediate version clears its window days
    /// before the newest release does — and falls back to the newest candidate when none are
    /// currently cooling. `None` only when there is no candidate at all (up to date, or a commit
    /// pin). The choice is presentation-only: it never affects `adoptable_target`, `latest`, or
    /// `status`.
    #[must_use]
    pub fn cooldown_candidate(
        &self,
        horizon: CooldownHorizon,
        now: jiff::Timestamp,
    ) -> Option<&Candidate> {
        match horizon {
            CooldownHorizon::Latest => self.candidates.last(),
            CooldownHorizon::Soonest => self
                .candidates
                .iter()
                .filter(|candidate| candidate.status == Status::InCooldown)
                .filter_map(|candidate| Some((candidate, candidate.time_to_mature(now)?)))
                .min_by_key(|&(_, remaining)| remaining)
                .map(|(candidate, _)| candidate)
                .or_else(|| self.candidates.last()),
        }
    }
}

/// The verdict over the currently-locked release (the `check` gate). `graph_held`/`graph_floor`
/// annotate a violation the resolved graph forces, so it can be baselined deliberately rather than
/// silently passed.
#[derive(Debug, Clone)]
pub struct PinVerdict {
    /// The verdict over the currently-locked release.
    pub status: Status,
    /// The cooldown window resolved for the locked release.
    pub window: ResolvedWindow,
    /// Whether the resolved graph forces this (too-fresh) version (MVS floor / `=` pin).
    pub graph_held: bool,
    /// The graph-imposed floor version, when one is responsible for the hold.
    pub graph_floor: Option<Version>,
    /// The locked release's publish instant, threaded for rendering.
    pub published_at: Option<jiff::Timestamp>,
}

/// A detected project rooted at a manifest within one tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The project's root directory.
    pub root: Utf8PathBuf,
    /// The tool the project belongs to.
    pub kind: ToolId,
    /// The path to the project's manifest (e.g. `Cargo.toml`, `go.mod`).
    pub manifest: Utf8PathBuf,
    /// The resolution window for this project as a publish-time cutoff string, populated by the
    /// application from the resolved policy. Tools whose resolver honors such a cutoff (uv's
    /// `--exclude-newer` / `UV_EXCLUDE_NEWER`) pass it so the lock resolves against cooldown's *own*
    /// window rather than whatever the tool or environment defaults to. It is a *relative* span
    /// (`"14 days"`) for an age window, or an absolute RFC3339 instant for a freeze — relative so a
    /// re-check stays stable across runs (an absolute `now - window` would drift every run and report
    /// the lock perpetually stale). `None` only when there is no effective window: detection (no
    /// policy resolved yet), or a `Latest`/zero-age window with no binding floor. The application fills
    /// it in once policy is resolved.
    pub exclude_newer: Option<String>,
}

/// What slice of the dependency set a command evaluates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepScope {
    /// Only direct dependencies (a fast path).
    Direct,
    /// The full resolved lockfile graph (direct + transitive) — the default for `check`.
    Graph,
}

/// A single planned version change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The package being changed.
    pub package: PackageId,
    /// The version being replaced.
    pub from: Version,
    /// The version being adopted.
    pub to: Version,
    /// The update kind of the change.
    pub kind: UpdateKind,
    /// Whether this change *lowers* the version — a cooldown rollback to the newest matured version
    /// rather than a forward move. Drives the report's "downgraded" vs "upgraded" status word, so an
    /// `upgrade` that rolls a too-fresh pin back is not mislabelled "upgraded".
    pub downgrade: bool,
    /// Whether the changed dependency is declared directly by a workspace member (vs. pulled in
    /// transitively). Carried so reports can attribute a transitive change as "via …".
    pub direct: bool,
    /// The workspace member package(s) that declare this dependency (direct) or that reach it through
    /// the graph (transitive), for source attribution in reports (see [`Dependency::members`]).
    pub members: Vec<MemberRef>,
}

/// How `apply` should treat a manifest's declared version constraint when adopting a new version.
///
/// The two modes differ only when the new version already satisfies the existing constraint (an
/// in-range minor/patch, or a major under an open `>=` bound): [`Auto`](RewriteMode::Auto) leaves
/// the constraint untouched and moves only the lock, while [`Always`](RewriteMode::Always) rewrites
/// the constraint to track the adopted version. When the target falls *outside* the constraint
/// (e.g. a cross-major bump past a caret range), both modes must rewrite — it is the only way to
/// adopt it — so the lock-only path is reserved for genuinely in-range moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteMode {
    /// Move the lock within the existing constraint; rewrite the manifest constraint only when the
    /// target version lies outside it. The default — it keeps declared ranges as loose as the
    /// author left them.
    #[default]
    Auto,
    /// Always rewrite the manifest constraint to the adopted version, even for an in-range move.
    Always,
}

/// A set of planned changes handed to an adapter's `apply`.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// The planned version changes.
    pub changes: Vec<Change>,
    /// How adapters should treat manifest constraints when applying these changes (the `--rewrite`
    /// flag). Defaults to [`RewriteMode::Auto`].
    pub rewrite: RewriteMode,
}

/// Why a planned change was not applied. Skips are `Ok` data, not `Err`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The graph requires this version newer (MVS floor / `=` pin) — cannot downgrade.
    GraphHeld,
    /// Applying it would drag a too-fresh, non-acknowledged transitive into the lock.
    TransitiveInCooldown,
    /// The resolver/MVS rejected the change.
    ResolverConflict,
    /// The dependency has no editable version requirement to retarget — it is transitive-only or a
    /// path/git source — so `upgrade` cannot move it by rewriting a constraint.
    NotEligible,
    /// An adoptable update crosses a major boundary and `--major` was not set; re-run with `--major`
    /// (per `--package`) to take it. It counts as a skip (the report breaks out how many such rows
    /// need `--major`), but unlike a real skip it never fails a `--strict` run — you chose not to
    /// take it, the run did not fail to.
    NeedsMajor,
}

impl SkipReason {
    /// Returns a human-readable explanation of the skip reason.
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::SkipReason;
    ///
    /// assert_eq!(
    ///     SkipReason::ResolverConflict.message(),
    ///     "the resolver rejected this change",
    /// );
    /// ```
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            SkipReason::GraphHeld => "graph requires this version newer; cannot downgrade",
            SkipReason::TransitiveInCooldown => {
                "would introduce a transitive dependency younger than its window"
            }
            SkipReason::ResolverConflict => "the resolver rejected this change",
            SkipReason::NotEligible => {
                "no editable requirement to change (transitive-only or path/git dependency)"
            }
            SkipReason::NeedsMajor => "needs --major to adopt",
        }
    }
}

/// A change that was not applied, with the reason and any offending package.
#[derive(Debug, Clone)]
pub struct Skipped {
    /// The change that was not applied.
    pub change: Change,
    /// Why it was skipped.
    pub reason: SkipReason,
    /// The package responsible for the skip (e.g. the too-fresh transitive), when known.
    pub offending: Option<PackageId>,
}

/// The outcome of an `apply`: what changed and what was skipped. Skips are non-fatal data.
#[derive(Debug, Clone, Default)]
pub struct ApplyReport {
    /// The changes that were applied.
    pub applied: Vec<Change>,
    /// The changes that were skipped, with reasons.
    pub skipped: Vec<Skipped>,
}

/// Whether to gate only environment-relevant artifacts or every recorded artifact (`--all-artifacts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactScope {
    /// Gate only environment-relevant artifacts.
    Environment,
    /// Gate every recorded artifact (`--all-artifacts`).
    All,
}

/// Whether release discovery should stay within the current major line or also probe cross-major
/// candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateScope {
    /// Only same-major candidates are relevant (`--major` not set).
    CurrentMajorOnly,
    /// Cross-major candidates are relevant (`--major` set).
    AllowCrossMajor,
}

/// How an tool's project roots are recognized on disk.
///
/// Adapters *declare* this rather than scanning themselves: the orchestrator runs one
/// gitignore-aware, exclude-aware walk per tool from these markers, so detection policy
/// (`.gitignore` honoring, the exclude list) is owned in one agnostic place and an adapter cannot
/// bypass it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectMarker {
    /// The lock/manifest filename whose presence marks a project root (e.g. `"Cargo.lock"`).
    pub lockfile: &'static str,
    /// The manifest filename recorded on the detected [`Project`] (e.g. `"Cargo.toml"`).
    pub manifest: &'static str,
    /// When `true`, a marked root's descendants are not also reported — a workspace root already
    /// owns its members (Cargo/uv). When `false`, every match is its own project (Go multi-module).
    pub workspace_root: bool,
}

/// The context an adapter needs to fetch releases and locked metadata for the right artifacts.
#[derive(Debug, Clone)]
pub struct FetchContext<'a> {
    /// The project being evaluated.
    pub project: &'a Project,
    /// Which artifacts to gate.
    pub artifacts: ArtifactScope,
}

/// The result of an opt-in `build`/`sync` verification step.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Whether the verification step succeeded.
    pub ok: bool,
    /// Human-readable detail (e.g. the build output or failure reason).
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::tool_id;

    #[test]
    fn tool_id_resolves_canonical_tools_and_aliases() {
        for (input, canonical) in [
            ("cargo", "cargo"),
            ("rust", "cargo"),
            ("crates", "cargo"),
            ("go", "go"),
            ("golang", "go"),
            // Python: uv, pip, and Poetry are each their own tool; `python` defaults to uv.
            ("uv", "uv"),
            ("python", "uv"),
            ("pip", "pip"),
            ("poetry", "poetry"),
            ("conda", "conda"),
            ("mamba", "conda"),
            ("pixi", "pixi"),
            // Each JS package manager is its own canonical tool; `node`/`js` alias to npm.
            ("npm", "npm"),
            ("node", "npm"),
            ("js", "npm"),
            ("pnpm", "pnpm"),
            ("yarn", "yarn"),
            ("bun", "bun"),
            ("deno", "deno"),
            // Ruby, Elixir, Java, and Swift.
            ("bundler", "bundler"),
            ("ruby", "bundler"),
            ("hex", "hex"),
            ("mix", "hex"),
            ("maven", "maven"),
            ("mvn", "maven"),
            ("gradle", "gradle"),
            ("swift", "swift"),
            ("spm", "swift"),
        ] {
            assert_eq!(
                tool_id(input).expect("known tool").as_str(),
                canonical,
                "tool `{input}`"
            );
        }
        assert!(tool_id("carg").is_none(), "a typo is rejected");
    }

    #[test]
    fn skip_reason_messages_are_accurate_and_distinct() {
        use super::SkipReason;
        // `NotEligible` describes a missing requirement (a `=`-pinned/transitive-only dep `upgrade`
        // cannot retarget), not the candidate filter — the old wording mislabelled a graph-pinned
        // transitive like `generic-array` whose `cargo update --precise` was rejected.
        let not_eligible = SkipReason::NotEligible.message();
        assert!(
            not_eligible.contains("no editable requirement"),
            "NotEligible should name the missing requirement, got: {not_eligible}"
        );
        assert!(
            !not_eligible.contains("candidate filter"),
            "NotEligible must not blame the candidate filter, got: {not_eligible}"
        );
        // ResolverConflict's exact wording is already pinned by the `message()` doctest; here we
        // only assert every reason has a *distinct* message so two skips never read identically.
        let all = [
            SkipReason::GraphHeld,
            SkipReason::TransitiveInCooldown,
            SkipReason::ResolverConflict,
            SkipReason::NotEligible,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(
                    a.message(),
                    b.message(),
                    "skip-reason messages must be distinct"
                );
            }
        }
    }
}
