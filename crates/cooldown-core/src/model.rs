//! The core domain model. Versions are **opaque to the core**: Go pseudo-versions, `/vN` majors,
//! `+incompatible`, PEP 440 and semver share no parse rules, so the core never parses a version —
//! the ecosystem hands back releases already classified, carrying an opaque ordering token and the
//! update-kind relative to the current pin.

use crate::policy::ResolvedWindow;
use camino::Utf8PathBuf;
use std::fmt;

/// Canonical display form of a version. The core treats this as opaque; it never parses it.
///
/// Go pseudo-versions, `/vN` majors, `+incompatible`, PEP 440 and semver share no parse
/// rules, so a `Version` is just the string an ecosystem chose to display. Ordering and
/// same-major comparisons go through the opaque [`ReleaseOrder`] and [`MajorKey`] tokens
/// instead, never through this string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct Version(
    /// The verbatim display string, exactly as the ecosystem produced it.
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
/// ever tested for equality, the ecosystem is free to encode the major however it likes
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
/// ecosystem constructs so that "newer" sorts greater. Tokens from different packages are
/// not comparable in any meaningful way.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReleaseOrder(
    /// The opaque ordering bytes; sorted lexicographically, newest greatest.
    pub Vec<u8>,
);

/// An ecosystem identifier, registered by its adapter. `Copy + 'static` so it threads cheaply.
///
/// The wrapped string is the stable language name used in config (`[lang.<name>]`) and on
/// the `--lang` flag; see [`RECOGNIZED_ECOSYSTEMS`] and [`ecosystem_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EcosystemId(
    /// The stable language name, e.g. `"rust"` or `"go"`.
    pub &'static str,
);

impl EcosystemId {
    /// Returns the ecosystem's stable language name.
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::EcosystemId;
    ///
    /// assert_eq!(EcosystemId("rust").as_str(), "rust");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

/// The ecosystems `cooldown` recognises in config (`[lang.<name>]`) and `--lang`, by their stable
/// language name. Pre-registering not-yet-implemented ones lets a shared org config mention them
/// without erroring, while a genuine typo (`[lang.golang]`) is still rejected.
pub const RECOGNIZED_ECOSYSTEMS: &[EcosystemId] = &[
    EcosystemId("go"),
    EcosystemId("rust"),
    EcosystemId("python"),
    EcosystemId("node"),
];

/// Resolve a language name to its canonical [`EcosystemId`], or `None` if unrecognised.
#[must_use]
pub fn ecosystem_id(name: &str) -> Option<EcosystemId> {
    RECOGNIZED_ECOSYSTEMS.iter().copied().find(|e| e.0 == name)
}

impl fmt::Display for EcosystemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl serde::Serialize for EcosystemId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0)
    }
}

/// A fully-qualified package identity: which ecosystem, the package name, and (optionally) the
/// registry/index it resolves from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackageId {
    /// The ecosystem the package belongs to.
    pub ecosystem: EcosystemId,
    /// The package name as it appears in the ecosystem's index.
    pub name: String,
    /// The registry/index the package resolves from (e.g. `crates.io`), or `None` for the
    /// ecosystem's default.
    pub registry: Option<String>,
}

impl PackageId {
    /// Assembles a [`PackageId`] from its ecosystem, name, and optional registry.
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::{EcosystemId, PackageId};
    ///
    /// let id = PackageId::new(EcosystemId("rust"), "serde", None);
    /// assert_eq!(id.name, "serde");
    /// assert_eq!(id.ecosystem.as_str(), "rust");
    /// assert!(id.registry.is_none());
    /// ```
    pub fn new(ecosystem: EcosystemId, name: impl Into<String>, registry: Option<String>) -> Self {
        PackageId {
            ecosystem,
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

/// A non-empty id for one locked artifact (e.g. a uv wheel/sdist). Version-granular ecosystems (Go,
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
    /// The locked artifacts for this dependency; empty for version-granular ecosystems.
    pub artifacts: Vec<ArtifactId>,
    /// The lowest version the resolved graph permits (MVS floor or a `=` pin), read from the
    /// lock; `None` when unconstrained.
    pub graph_floor: Option<Version>,
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
    /// Commit-pinned (a pseudo-version): no tagged version to compare against.
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

/// The aggregate verdict for a dependency over its candidate set.
#[derive(Debug, Clone)]
pub struct Verdict {
    /// The aggregate status over the candidate set.
    pub status: Status,
    /// The newest candidate that has matured past its window, if any.
    pub adoptable_target: Option<Version>,
    /// The newest existing version, adoptable or not.
    pub latest: Option<Version>,
    /// The per-candidate verdicts, newest first.
    pub candidates: Vec<Candidate>,
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

/// A detected project rooted at a manifest within one ecosystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The project's root directory.
    pub root: Utf8PathBuf,
    /// The ecosystem the project belongs to.
    pub kind: EcosystemId,
    /// The path to the project's manifest (e.g. `Cargo.toml`, `go.mod`).
    pub manifest: Utf8PathBuf,
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
}

/// A set of planned changes handed to an adapter's `apply`.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// The planned version changes.
    pub changes: Vec<Change>,
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
    /// The candidate was filtered out (e.g. requires `--major`).
    NotEligible,
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
            SkipReason::NotEligible => "candidate not eligible under the current candidate filter",
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateScope {
    /// Only same-major candidates are relevant (`--major` not set).
    CurrentMajorOnly,
    /// Cross-major candidates are relevant (`--major` set).
    AllowCrossMajor,
}

/// The platform/abi/python-version/markers a lock must satisfy. Version-granular ecosystems leave
/// this empty.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    /// The platform/abi/python-version/marker strings the lock must satisfy.
    pub markers: Vec<String>,
}

/// The context an adapter needs to fetch releases and locked metadata for the right artifacts.
#[derive(Debug, Clone)]
pub struct FetchContext<'a> {
    /// The project being evaluated.
    pub project: &'a Project,
    /// The environments the lock must satisfy; empty for version-granular ecosystems.
    pub environments: &'a [Environment],
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
