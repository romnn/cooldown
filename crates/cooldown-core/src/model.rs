//! The core domain model. Versions are **opaque to the core**: Go pseudo-versions, `/vN` majors,
//! `+incompatible`, PEP 440 and semver share no parse rules, so the core never parses a version —
//! the tool hands back releases already classified, carrying an opaque ordering token and the
//! update-kind relative to the current pin.

use crate::duration::since;
use crate::error::Diagnostic;
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
    /// The ecosystem's numeric major ordinal, or `None` when it cannot be classified.
    ///
    /// A configured `max-major` conservatively excludes releases without an ordinal. PEP 440
    /// epochs do not affect this number; adapters use the first release segment.
    pub major_number: Option<u64>,
    /// The update kind relative to the current pin, or `None` when not comparable (e.g. a
    /// commit pin).
    pub kind_from_current: Option<UpdateKind>,
    /// Whether this release falls outside [`Dependency::declared_bound`].
    ///
    /// Adapters set this with their native range matcher. It is always `false` when the dependency
    /// has no explicit declared upper bound.
    pub beyond_declared_bound: bool,
    /// Whether this release is ordered above the version the registry's mutable `latest` dist-tag
    /// names (npm-family only; see [`Capabilities::has_dist_tags`](crate::Capabilities)).
    ///
    /// The tag is the maintainer's own "this is current" pointer — `npm install <pkg>` resolves to
    /// it — so a stable release ordered above it (a premature or abandoned major the maintainer
    /// kept releasing below, e.g. a `17.0.0` published months before the `16.x` line continued) is
    /// not a normal adoption target. `evaluate` holds such releases out of the candidate set unless
    /// the current pin is itself beyond the tag (a project deliberately riding a `next` line) or
    /// [`honor_latest_tag`](crate::ResolveContext::honor_latest_tag) is off. Always `false` for
    /// registries without dist-tags, and for the tagged version itself and everything ordered at or
    /// below it.
    pub beyond_latest_tag: bool,
    /// The newest upload time over the selected artifacts, or `None` if any selected
    /// artifact's time is unknown.
    pub published_at: Option<jiff::Timestamp>,
    /// Whether the release has been yanked/withdrawn.
    pub yanked: bool,
    /// The quality classification the adapter assigned.
    pub quality: ReleaseQuality,
}

/// A dependency candidate to be evaluated. Most candidates are resolved lock entries. Adapters may
/// also expose manifest-only constraints through an explicit port method for commands that can
/// mutate that manifest floor directly; those candidates use the declared floor as `current`, have no
/// locked artifacts, and never participate in lock-gating commands.
///
/// `current_quality` lets `evaluate` apply the prerelease rule in the core. For lock-backed
/// dependencies, `current_quality == locked_release(dep, ctx).quality` (the adapter derives both from
/// the same lock entry). `graph_floor` is the lowest version the resolved graph permits (MVS floor /
/// a `=` pin), read from the lock.
#[derive(Debug, Clone)]
pub struct Dependency {
    /// The dependency's package identity.
    pub package: PackageId,
    /// The package's identity in the tool's advisory-database ecosystem
    /// ([`Capabilities::advisory_ecosystem`]), in the database's canonical spelling — or `None`
    /// when the dependency's resolved source cannot be proven to belong to that ecosystem.
    ///
    /// The feed query is case- and separator-sensitive, so the spelling must be the database's
    /// canonical form (the Python tools normalize to PEP 503, Swift lowercases its
    /// repository-URL identity; identity is correct for most tools).
    ///
    /// `None` withholds the identity entirely: the package is never sent to the feed and never
    /// matched against its advisories.
    /// Advisory data can only *loosen* policy (annotate rows, shorten windows), so a package from a
    /// private or alternate registry that merely shares a public package's name must not inherit
    /// the public package's advisories — OSV's `PyPI` ecosystem identifies pypi.org packages, not
    /// arbitrary Python indexes.
    /// The proof must be *positive*, in one of three forms: a per-package resolution record naming
    /// the public registry; a fully enumerable configuration surface shown clean (pip's
    /// requirements tree, rooted at the file and followed through its `-r`/`-c` includes); or the
    /// package manager itself confirming its effective routing at feed time
    /// ([`ToolRead::confirm_advisory_identities`](crate::ToolRead::confirm_advisory_identities) —
    /// npm's, pnpm's, and pip's `config list`).
    /// The absence of *unenumerable* configuration is never proof — npm's global and builtin
    /// layers, pip's interpreter-prefix site config behind a shim, Maven's parent poms — and
    /// configuration can only veto a grant, never substitute for one.
    /// The adapter is the only side that sees the lock's source records, so it decides here, at
    /// construction.
    pub advisory_identity: Option<String>,
    /// The currently-locked version, or the declared floor for an explicit manifest-only candidate.
    pub current: Version,
    /// The quality of `current`; for lock-backed dependencies this mirrors
    /// `locked_release(dep, ctx).quality`.
    pub current_quality: ReleaseQuality,
    /// Whether this is a direct dependency (as opposed to transitive).
    pub direct: bool,
    /// The locked artifacts for this dependency; empty for version-granular tools and manifest-only
    /// candidates.
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
    /// The verbatim declared requirement carrying an explicit `<` or `<=` upper bound.
    ///
    /// The core treats this as display-only opaque text. The declaring adapter parses it and marks
    /// each [`Release::beyond_declared_bound`]. Implicit caret and tilde ceilings are not recorded.
    pub declared_bound: Option<String>,
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
    /// The attributed constraint edges behind [`graph_floor`](Self::graph_floor) and
    /// [`graph_ceiling`](Self::graph_ceiling), when the adapter can name each requirer. Empty when
    /// attribution is unavailable; the collapsed fields then remain the only graph constraint and
    /// `fix` planning treats every hold as genuine. See [`GraphHoldEdge`].
    pub hold_edges: Vec<GraphHoldEdge>,
}

/// One active requirement edge through which the resolved graph constrains a [`Dependency`]: the
/// requiring package (a resolved node, identified by name and version) and the bound its
/// requirement imposes on this dependency's node.
///
/// The collapsed [`Dependency::graph_floor`] / [`Dependency::graph_ceiling`] keep the gate cheap
/// for `check`; these attributed edges exist so `fix` planning can tell *who* holds a violation.
/// A hold contributed only by requirers that are themselves too-fresh violations is circular — the
/// floor is conditioned on the very resolution being fixed — so the planner discounts those edges
/// and plans the whole co-moving family; a hold from a compliant requirer is genuine and is
/// reported with the requirer's name and requirement. Adapters that cannot attribute constraints
/// leave [`Dependency::hold_edges`] empty, which disables discounting and preserves the collapsed
/// behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphHoldEdge {
    /// The requiring package's name (a resolved non-member node in the graph).
    pub requirer: String,
    /// The requiring package's resolved version, used to match the requirer against the violation
    /// set (coexisting versions of one name are distinct nodes).
    pub requirer_version: Version,
    /// The verbatim declared requirement, for display in held-dependency warnings.
    pub requirement: String,
    /// The bound this edge imposes on the dependency's resolved node: for a floor edge, the lowest
    /// version the requirement admits; for a ceiling edge (an exact `=` pin), the pinned version,
    /// which adapters guarantee equals the dependency's resolved version.
    pub bound: Version,
    /// Which direction the bound constrains.
    pub kind: GraphHoldKind,
}

/// The direction a [`GraphHoldEdge`] constrains its dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphHoldKind {
    /// The edge's requirement imposes a lower bound (contributes to [`Dependency::graph_floor`]).
    Floor,
    /// The edge is an exact pin capping the node (contributes to [`Dependency::graph_ceiling`]).
    Ceiling,
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
    /// Held by a pin, resolved-graph ceiling, explicit manifest upper bound, or configured
    /// `max-major`, so it will not move automatically.
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
    /// The cooldown window resolved for this candidate — the security window when the advisory
    /// shorten mode applied (then [`ResolvedWindow::shortened_by`] names the advisory).
    pub window: ResolvedWindow,
    /// The verdict for this candidate.
    pub status: Status,
    /// The candidate's publish instant, threaded through for rendering (`ageDays`).
    pub published_at: Option<jiff::Timestamp>,
    /// Why this candidate is security-relevant (adopting it fixes an advisory affecting the
    /// current pin), or `None` for an ordinary candidate.
    ///
    /// Always `None` without an advisory feed.
    pub security: Option<crate::advisory::SecurityRelevance>,
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
    /// Why the headline is [`Status::Held`], or `None` for every other status.
    pub held_reason: Option<HeldReason>,
}

/// The policy or declaration that prevents a held dependency from moving automatically.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldReason {
    /// An exact `==` or `=` manifest pin.
    ExactPin,
    /// A commit pin or pseudo-version.
    CommitPin,
    /// A requirer's exact pin caps the resolved graph.
    GraphCeiling,
    /// The manifest's verbatim explicit upper-bound requirement.
    DeclaredBound(String),
    /// The inclusive configured numeric major ceiling.
    MaxMajor(u64),
    /// The registry's `latest` dist-tag caps adoption at this version; the releases above it (the
    /// current tag points below them, whatever other tag they carry) are held.
    DistTag(String),
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
    /// The cooldown window resolved for the locked release — the security window when the
    /// locked version is itself an advisory's fix and the shorten mode applied.
    pub window: ResolvedWindow,
    /// Whether the resolved graph forces this (too-fresh) version (MVS floor / `=` pin).
    pub graph_held: bool,
    /// The graph-imposed floor version, when one is responsible for the hold.
    pub graph_floor: Option<Version>,
    /// The locked release's publish instant, threaded for rendering.
    pub published_at: Option<jiff::Timestamp>,
    /// Why this pin is security-relevant (the locked version is an advisory's fix version), or
    /// `None` for an ordinary pin.
    ///
    /// Always `None` without an advisory feed.
    pub security: Option<crate::advisory::SecurityRelevance>,
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
/// Where an adapter supports lock-only updates, [`Auto`](RewriteMode::Auto) leaves an in-range
/// constraint untouched and may widen an implicit caret/tilde ceiling for an opted-in major update.
/// An explicit `<`/`<=` comparator holds in this mode. [`Always`](RewriteMode::Always) rewrites every
/// adopted target and is the deliberate escape hatch for crossing an explicit upper bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteMode {
    /// Preserve an in-range constraint where possible and widen only implicit ceilings when
    /// required. Author-written `<`/`<=` upper bounds remain absolute in this default mode.
    #[default]
    Auto,
    /// Always rewrite the manifest constraint, including when crossing an explicit upper bound.
    Always,
}

/// How an adapter treats addressable resolved lock **edge bindings** after a whole-graph re-resolve.
///
/// A lock records not only which package versions exist but which coexisting version each
/// dependent's edge is *bound* to (cargo's `dependencies = ["uuid 0.8.2"]` entries).
/// When a dependent's declared range admits several locked versions (e.g. diesel's
/// `uuid = ">=0.7, <2.0"` with both `0.8.2` and `1.x` in the lock), an incremental re-resolve can
/// silently rebind such an edge between them — a build-affecting change (`rustc` receives the other
/// copy as `--extern`) that is invisible at the per-version level and passes the tool's own lock
/// verification.
/// This policy decides what the adapter does about it.
/// Currently enforced by the cargo adapter; adapters without ambiguous edge bindings ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgePolicy {
    /// Restore an addressable, unambiguous crates.io edge the re-resolve rebound between two
    /// still-coexisting versions when its earlier binding still satisfies the active requirement.
    /// This is the default.
    #[default]
    Preserve,
    /// Bind each addressable, unambiguous crates.io edge to the **highest** locked version satisfying
    /// the dependent's active requirement, preferring candidates whose declared `rust-version` is
    /// workspace-compatible.
    ///
    /// This adapter-owned normalization matches a from-scratch resolve in the common case and also
    /// heals eligible bad bindings that predate the run.
    Canonicalize,
    /// Leave every binding exactly as the resolver produced it.
    /// Unplanned rebinds are still *reported* (never silent), just not corrected.
    None,
}

macro_rules! edge_binding_actions {
    ($( $(#[$attr:meta])* $variant:ident = $wire:literal, )+) => {
        /// What the adapter's edge policy did or observed about one lock edge.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
        pub enum EdgeBindingAction {
            $( $(#[$attr])* #[serde(rename = $wire)] $variant, )+
        }

        impl EdgeBindingAction {
            /// Every variant, in declaration order.
            pub const ALL: &'static [EdgeBindingAction] =
                &[ $( EdgeBindingAction::$variant, )+ ];

            /// Returns the serialized wire token for this action.
            #[must_use]
            pub fn wire_value(self) -> &'static str {
                match self {
                    $( EdgeBindingAction::$variant => $wire, )+
                }
            }

            /// Whether a row with this action describes state committed to the project.
            #[must_use]
            pub const fn is_applied(self) -> bool {
                match self {
                    EdgeBindingAction::Restored
                    | EdgeBindingAction::Canonicalized
                    | EdgeBindingAction::Rebound
                    | EdgeBindingAction::Unaddressable => true,
                    EdgeBindingAction::Held => false,
                }
            }

            /// Whether a row with this action must explain its policy limitation.
            #[must_use]
            pub const fn requires_detail(self) -> bool {
                match self {
                    EdgeBindingAction::Held | EdgeBindingAction::Unaddressable => true,
                    EdgeBindingAction::Restored
                    | EdgeBindingAction::Canonicalized
                    | EdgeBindingAction::Rebound => false,
                }
            }
        }
    };
}

edge_binding_actions! {
    /// The re-resolve rebound the edge; [`EdgePolicy::Preserve`] restored the earlier binding.
    Restored = "restored",
    /// [`EdgePolicy::Canonicalize`] wrote the canonical binding.
    Canonicalized = "canonicalized",
    /// The resolver-produced binding was allowed and committed.
    Rebound = "rebound",
    /// A concrete corrective target was withheld; [`EdgeRebind::to`] carries that target.
    Held = "held",
    /// A binding moved, but the lock does not identify the requirement precisely enough to correct
    /// it safely; [`EdgeRebind::detail`] explains the limitation.
    Unaddressable = "unaddressable",
}

/// A policy-relevant lock edge that enforcement corrected, withheld a correction for, or observed
/// rebound.
///
/// For [`Restored`](EdgeBindingAction::Restored)/[`Canonicalized`](EdgeBindingAction::Canonicalized)
/// `to` is the binding the committed lock ends with and `from` the resolver-produced binding it
/// superseded; for [`Rebound`](EdgeBindingAction::Rebound) and
/// [`Unaddressable`](EdgeBindingAction::Unaddressable), `from` is the pre-apply binding and `to` the
/// committed one; for [`Held`](EdgeBindingAction::Held) the committed binding **stays at** `from`
/// and `to` is the correction that was withheld.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRebind {
    /// The dependent package whose edge moved (e.g. `diesel`).
    pub dependent: String,
    /// The dependent's resolved version (a dependent can coexist at several versions).
    pub dependent_version: Version,
    /// The dependent's package source, absent for path and workspace packages.
    pub dependent_source: Option<String>,
    /// The dependency the edge points at (e.g. `uuid`).
    pub dependency: PackageId,
    /// The superseded binding version (or, for [`Held`](EdgeBindingAction::Held), the binding that
    /// remains in place).
    pub from: Version,
    /// The binding version the committed lock ends with (or, for
    /// [`Held`](EdgeBindingAction::Held), the withheld correction target).
    pub to: Version,
    /// What the policy did (or observed) about the rebind.
    pub action: EdgeBindingAction,
    /// Source-transition or policy-limitation context.
    /// Required for [`Held`](EdgeBindingAction::Held) and
    /// [`Unaddressable`](EdgeBindingAction::Unaddressable); observed source moves may also carry it.
    pub detail: Option<String>,
}

impl EdgeRebind {
    /// Validates the action-specific invariants required by the report contract.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Serialization`](crate::CoreError::Serialization) when a required detail
    /// is absent or any provided detail is empty.
    pub fn validate(&self) -> crate::Result<()> {
        let detail = self
            .detail
            .as_deref()
            .filter(|detail| !detail.trim().is_empty());
        if self.detail.is_some() && detail.is_none() {
            return Err(crate::CoreError::Serialization(format!(
                "edge action `{}` has an empty detail",
                self.action.wire_value()
            )));
        }
        if self.action.requires_detail() && detail.is_none() {
            return Err(crate::CoreError::Serialization(format!(
                "edge action `{}` requires a detail",
                self.action.wire_value()
            )));
        }
        Ok(())
    }
}

/// A resolved package version already rejected by the active policy before an apply trial begins.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BaselineViolation {
    /// The complete package identity, including its registry or source.
    pub package: PackageId,
    /// The exact version present in the starting graph.
    pub version: Version,
}

/// A set of planned changes handed to an adapter's `apply`.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// The planned version changes.
    pub changes: Vec<Change>,
    /// How adapters should treat manifest constraints when applying these changes (the `--rewrite`
    /// flag).
    /// Defaults to [`RewriteMode::Auto`].
    pub rewrite: RewriteMode,
    /// How the adapter treats resolved lock edge bindings after the re-resolve (the
    /// `--cargo-edge-policy` flag / `[tool.cargo] edge-policy` config key).
    /// Defaults to [`EdgePolicy::Preserve`].
    pub edge_policy: EdgePolicy,
    /// Policy violations already present before this trial.
    ///
    /// Adapters may authorize these exact starting versions while resolving a repair, but must not
    /// treat the set as permission for newly selected versions to bypass the policy.
    pub baseline_violations: Vec<BaselineViolation>,
    /// The workspace members the run's `exclude-folders`/`exclude-packages` policy dropped from
    /// every dependency's attribution — outside the request, not merely unattributed.
    ///
    /// An adapter must neither move, pin, nor rewrite a declaration these members own, and must not
    /// count their declarations as workspace evidence: a pnpm importer the user excluded cannot
    /// veto an update in an included one by declaring the package on another line.
    /// Empty when nothing is excluded or the adapter attributes no members.
    pub excluded_members: Vec<MemberRef>,
}

/// Declares [`SkipReason`] together with its [`ALL`](SkipReason::ALL) enumeration and per-variant
/// wire tokens in a single invocation: each variant's serde `rename`, its `ALL` entry, and its
/// [`wire_value`](SkipReason::wire_value) all expand from the same `$wire` literal, so the
/// serialized token, the schema token, and the enumeration structurally cannot disagree.
macro_rules! skip_reasons {
    ($( $(#[$attr:meta])* $variant:ident = $wire:literal, )+) => {
        /// Why a planned change was not applied. Skips are `Ok` data, not `Err`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
        pub enum SkipReason {
            $( $(#[$attr])* #[serde(rename = $wire)] $variant, )+
        }

        impl SkipReason {
            /// Every variant, in declaration order. An enumeration consumer (the report JSON
            /// schema's `reason` enum, the tests that walk every reason) can never miss one:
            /// a variant exists exactly when it appears here.
            pub const ALL: &'static [SkipReason] = &[ $( SkipReason::$variant, )+ ];

            /// The wire token this reason serializes as (`"needs_major"`, `"peer_held"`, …).
            /// Identical to serde's output by construction, exposed so the JSON schema can
            /// enumerate the closed set without serializing.
            #[must_use]
            pub fn wire_value(self) -> &'static str {
                match self {
                    $( SkipReason::$variant => $wire, )+
                }
            }
        }
    };
}

skip_reasons! {
    /// The graph requires this version newer (MVS floor / `=` pin) — cannot downgrade.
    GraphHeld = "graph_held",
    /// Applying it would drag a too-fresh, non-acknowledged transitive into the lock.
    TransitiveInCooldown = "transitive_in_cooldown",
    /// The resolver/MVS rejected the change.
    ResolverConflict = "resolver_conflict",
    /// The dependency has no editable version requirement to retarget — it is transitive-only or a
    /// path/git source — so `upgrade` cannot move it by rewriting a constraint.
    NotEligible = "not_eligible",
    /// An adoptable update crosses a major boundary and `--major` was not set; re-run with `--major`
    /// (per `--package`) to take it. It counts as a skip (the report breaks out how many such rows
    /// need `--major`), but unlike a real skip it never fails a `--strict` run — you chose not to
    /// take it, the run did not fail to.
    NeedsMajor = "needs_major",
    /// An explicit manifest upper bound holds the dependency below the newer major.
    DeclaredBoundHeld = "declared_bound_held",
    /// A configured package `max-major` holds the dependency below the candidate.
    MaxMajorHeld = "max_major_held",
    /// The registry's `latest` dist-tag currently points below the newer release, so adopting it
    /// would move past what a plain install resolves to today. Like
    /// [`NeedsMajor`](SkipReason::NeedsMajor) this is conservative-correct — the maintainer's own
    /// tag says the newer release is not current — so it never fails a `--strict` run.
    DistTagHeld = "dist_tag_held",
    /// A resolved dependent's declared peer range excludes the target (e.g. `fumadocs-mdx`
    /// peer-requires `fumadocs-core@^16` while the target is `17.0.0`), so landing it would break
    /// the peer contract even where the native resolver only warns. The offending dependent is
    /// named on the [`Skipped`] row.
    PeerHeld = "peer_held",
    /// The workspace declares the dependency under ranges that do not all admit the target (one
    /// member on `@types/node@^22` while the target is `25`), so it is range-floated (each importer
    /// kept on its own line) rather than pinned to one target; `--rewrite` widens the ranges and
    /// converges it.
    /// The same reason holds a name the plan targets at several versions at once (a `^22` line and
    /// a `^25` line each advancing within itself), which one joint pin cannot land whatever the
    /// ranges admit; there only `--major`, admitting one line for every importer, converges it.
    /// Like [`NeedsMajor`](SkipReason::NeedsMajor) this is conservative-correct, not a failed
    /// upgrade, so it never fails a `--strict` run.
    MultiVersionHeld = "multi_version_held",
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
            SkipReason::DeclaredBoundHeld => {
                "declared upper bound holds this below the newer major; pass --rewrite to cross and rewrite it"
            }
            SkipReason::MaxMajorHeld => {
                "config max-major ceiling holds this; raise it in cooldown.toml to adopt"
            }
            SkipReason::DistTagHeld => {
                "the registry's current latest dist-tag points below this newer release; set respect-dist-tags = false to adopt it"
            }
            SkipReason::PeerHeld => {
                "a resolved dependent's peer dependency range excludes this target"
            }
            SkipReason::MultiVersionHeld => {
                "declared at multiple versions across the workspace; kept on its own line"
            }
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
    /// An adapter-supplied elaboration of the skip carrying facts only the adapter knows (e.g. the
    /// dependent's verbatim peer range that excludes the target). Reports prefer it over the
    /// generic [`SkipReason::message`] when present.
    pub detail: Option<String>,
}

/// The outcome of an `apply`: what changed and what was skipped. Skips are non-fatal data.
#[derive(Debug, Clone, Default)]
pub struct ApplyReport {
    /// The changes that were applied.
    pub applied: Vec<Change>,
    /// The changes that were skipped, with reasons.
    pub skipped: Vec<Skipped>,
    /// Lock edges whose binding moved between coexisting versions, with what the
    /// [`EdgePolicy`] did about each.
    /// Empty for adapters without ambiguous edge bindings.
    pub edge_rebinds: Vec<EdgeRebind>,
    /// Non-fatal adapter warnings about a mutation that is already visible and must still be
    /// reported as committed.
    pub warnings: Vec<Diagnostic>,
}

/// The authoritative final edge audit plus non-fatal durability warnings.
#[derive(Debug, Clone, Default)]
pub struct EdgeNormalizationReport {
    /// Final lock-edge outcomes after reconciling run-level observation and batch provenance.
    pub rebinds: Vec<EdgeRebind>,
    /// Warnings raised after a verified correction crossed its visible commit point.
    pub warnings: Vec<Diagnostic>,
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

/// The primary filesystem markers that directly identify a tool's project root.
///
/// Adapters carry this inside [`ProjectDetection`] rather than scanning themselves.
/// The orchestrator owns gitignore-aware and exclude-aware traversal, so an adapter cannot bypass
/// shared detection policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectMarker {
    /// The lock/manifest filename whose presence marks a project root (e.g. `"Cargo.lock"`).
    pub lockfile: &'static str,
    /// The primary manifest filename recorded on the detected [`Project`] (e.g. `"Cargo.toml"`).
    pub manifest: &'static str,
    /// Alternate manifest names for tools that accept more than one root config filename.
    pub alternate_manifests: &'static [&'static str],
    /// When `true`, a marked root's descendants are not also reported — a workspace root already
    /// owns its members (Cargo/uv). When `false`, every match is its own project (Go multi-module).
    /// A dropped descendant gets one appeal: the adapter's
    /// [`nested_lockfile_root_escapes`](crate::ToolRead::nested_lockfile_root_escapes) can
    /// recognize it as a workspace root of its own that the enclosing workspace only excludes.
    pub workspace_root: bool,
}

/// The complete filesystem-marker specification for one adapter's project discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectDetection {
    /// A primary marker directly identifies every project root.
    Primary(ProjectMarker),
    /// A second marker identifies roots that must be validated but not automatically accepted.
    PrimaryWithValidation {
        /// The marker that directly identifies project roots.
        primary: ProjectMarker,
        /// The validation-only filename inspected during the same repository traversal.
        validation_marker: &'static str,
    },
}

impl ProjectDetection {
    /// Returns the primary marker that directly identifies project roots.
    #[must_use]
    pub fn primary(self) -> ProjectMarker {
        match self {
            ProjectDetection::Primary(marker)
            | ProjectDetection::PrimaryWithValidation {
                primary: marker, ..
            } => marker,
        }
    }

    /// Returns the optional validation-only marker scanned alongside the primary marker.
    #[must_use]
    pub fn validation_marker(self) -> Option<&'static str> {
        match self {
            ProjectDetection::Primary(_) => None,
            ProjectDetection::PrimaryWithValidation {
                validation_marker, ..
            } => Some(validation_marker),
        }
    }
}

/// The context an adapter needs to fetch releases and locked metadata for the right artifacts.
#[derive(Debug, Clone)]
pub struct FetchContext<'a> {
    /// The project being evaluated.
    pub project: &'a Project,
    /// Which artifacts to gate.
    pub artifacts: ArtifactScope,
}

/// The outcome of a lock-currency probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LockStatus {
    /// The lock is current relative to the manifest/config the tool reads.
    Current,
    /// The lock is known to be stale or absent.
    Stale,
    /// The adapter cannot currently prove whether the lock is current.
    Unknown,
}

/// The result of a lock-currency verification step.
#[derive(Debug, Clone)]
pub struct LockVerifyReport {
    /// The lock-currency outcome.
    pub status: LockStatus,
    /// Human-readable detail describing the probe result.
    pub detail: String,
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
        for (i, a) in SkipReason::ALL.iter().enumerate() {
            for b in &SkipReason::ALL[i + 1..] {
                assert_ne!(
                    a.message(),
                    b.message(),
                    "skip-reason messages must be distinct"
                );
            }
        }
    }

    /// Serde's token and `wire_value` expand from the same macro literal, so they cannot disagree
    /// by construction; the property left to check is that `ALL` enumerates distinct wire tokens
    /// (two variants sharing one `$wire` would alias on the wire without a compile error).
    #[test]
    fn skip_reason_wire_tokens_are_distinct() {
        use super::SkipReason;
        let mut seen = std::collections::BTreeSet::new();
        for reason in SkipReason::ALL {
            assert!(
                seen.insert(reason.wire_value()),
                "ALL must list every wire token once"
            );
        }
    }

    /// The enum, serde token, and `wire_value` expand from one macro literal, so the remaining
    /// property to check is that every variant has a distinct wire token.
    #[test]
    fn edge_binding_action_wire_tokens_match_serde() {
        use super::EdgeBindingAction;
        let mut seen = std::collections::BTreeSet::new();
        for action in EdgeBindingAction::ALL {
            let serialized = toml::Value::try_from(action).ok();
            assert_eq!(
                serialized,
                Some(toml::Value::String(action.wire_value().to_string())),
                "wire_value must equal serde's token"
            );
            assert!(
                seen.insert(action.wire_value()),
                "ALL must list every wire token once"
            );
        }
    }

    #[test]
    fn edge_binding_action_report_invariants_are_exhaustive() {
        use super::EdgeBindingAction;

        for action in EdgeBindingAction::ALL {
            assert_eq!(
                action.is_applied(),
                !matches!(action, EdgeBindingAction::Held)
            );
            assert_eq!(
                action.requires_detail(),
                matches!(
                    action,
                    EdgeBindingAction::Held | EdgeBindingAction::Unaddressable
                )
            );
        }
    }

    #[test]
    fn edge_rebind_validation_requires_non_empty_policy_reasons() {
        use super::{EdgeBindingAction, EdgeRebind, PackageId, ToolId, Version};

        let mut rebind = EdgeRebind {
            dependent: "consumer".to_string(),
            dependent_version: Version::new("1.0.0"),
            dependent_source: None,
            dependency: PackageId::new(ToolId("cargo"), "dependency", None),
            from: Version::new("1.0.0"),
            to: Version::new("2.0.0"),
            action: EdgeBindingAction::Held,
            detail: None,
        };

        assert!(rebind.validate().is_err());
        rebind.detail = Some("   ".to_string());
        assert!(rebind.validate().is_err());
        rebind.detail = Some("candidate would orphan a lock block".to_string());
        assert!(rebind.validate().is_ok());
        rebind.action = EdgeBindingAction::Rebound;
        rebind.detail = None;
        assert!(rebind.validate().is_ok());
    }
}
