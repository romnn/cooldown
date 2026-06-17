//! The core domain model. Versions are **opaque to the core**: Go pseudo-versions, `/vN` majors,
//! `+incompatible`, PEP 440 and semver share no parse rules, so the core never parses a version —
//! the ecosystem hands back releases already classified, carrying an opaque ordering token and the
//! update-kind relative to the current pin.

use crate::policy::ResolvedWindow;
use camino::Utf8PathBuf;
use std::fmt;

/// Canonical display form of a version. The core treats this as opaque; it never parses it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct Version(pub String);

impl Version {
    pub fn new(s: impl Into<String>) -> Self {
        Version(s.into())
    }
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MajorKey(pub String);

/// An opaque total-order token, meaningful only **within one package**. The core sorts and compares
/// releases with this; it carries a `debug_assert` of sortedness at the port boundary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReleaseOrder(pub Vec<u8>);

/// An ecosystem identifier, registered by its adapter. `Copy + 'static` so it threads cheaply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EcosystemId(pub &'static str);

impl EcosystemId {
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
    pub ecosystem: EcosystemId,
    pub name: String,
    pub registry: Option<String>,
}

impl PackageId {
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
    Stable,
    Prerelease,
    Pseudo,
    Incompatible,
}

impl ReleaseQuality {
    /// Stable and `+incompatible` are the "real release" qualities adoption normally targets.
    pub fn is_stable_like(self) -> bool {
        matches!(self, ReleaseQuality::Stable | ReleaseQuality::Incompatible)
    }
}

/// The update kind of a candidate relative to the current pin. `Copy + Eq`, deliberately **no
/// `Ord`** — kinds are categories, not a scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateKind {
    Major,
    Minor,
    Patch,
}

/// A non-empty id for one locked artifact (e.g. a uv wheel/sdist). Version-granular ecosystems (Go,
/// crates.io) leave `Dependency::artifacts` empty.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ArtifactId(pub String);

/// A classified release. The `published_at` aggregate is the newest upload over the *selected*
/// artifacts (environment-relevant, else all), but `None` if **any** selected artifact's time is
/// unknown — conservative: a partially-known release is never treated as mature.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    pub order: ReleaseOrder,
    pub major: MajorKey,
    pub kind_from_current: Option<UpdateKind>,
    pub published_at: Option<jiff::Timestamp>,
    pub yanked: bool,
    pub quality: ReleaseQuality,
}

/// A resolved dependency to be evaluated. `current_quality` lets `evaluate` apply the prerelease
/// rule in the core. INVARIANT: `current_quality == locked_release(dep, ctx).quality` (the adapter
/// derives both from the same lock entry). `graph_floor` is the lowest version the resolved graph
/// permits (MVS floor / a `=` pin), read from the lock.
#[derive(Debug, Clone)]
pub struct Dependency {
    pub package: PackageId,
    pub current: Version,
    pub current_quality: ReleaseQuality,
    pub direct: bool,
    pub artifacts: Vec<ArtifactId>,
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
    pub version: Version,
    pub kind: UpdateKind,
    pub window: ResolvedWindow,
    pub status: Status,
    /// The candidate's publish instant, threaded through for rendering (`ageDays`).
    pub published_at: Option<jiff::Timestamp>,
}

/// The aggregate verdict for a dependency over its candidate set.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub status: Status,
    pub adoptable_target: Option<Version>,
    pub latest: Option<Version>,
    pub candidates: Vec<Candidate>,
}

/// The verdict over the currently-locked release (the `check` gate). `graph_held`/`graph_floor`
/// annotate a violation the resolved graph forces, so it can be baselined deliberately rather than
/// silently passed.
#[derive(Debug, Clone)]
pub struct PinVerdict {
    pub status: Status,
    pub window: ResolvedWindow,
    pub graph_held: bool,
    pub graph_floor: Option<Version>,
    /// The locked release's publish instant, threaded for rendering.
    pub published_at: Option<jiff::Timestamp>,
}

/// A detected project rooted at a manifest within one ecosystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub root: Utf8PathBuf,
    pub kind: EcosystemId,
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
    pub package: PackageId,
    pub from: Version,
    pub to: Version,
    pub kind: UpdateKind,
}

/// A set of planned changes handed to an adapter's `apply`.
#[derive(Debug, Clone, Default)]
pub struct Plan {
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
    pub change: Change,
    pub reason: SkipReason,
    /// The package responsible for the skip (e.g. the too-fresh transitive), when known.
    pub offending: Option<PackageId>,
}

/// The outcome of an `apply`: what changed and what was skipped. Skips are non-fatal data.
#[derive(Debug, Clone, Default)]
pub struct ApplyReport {
    pub applied: Vec<Change>,
    pub skipped: Vec<Skipped>,
}

/// Whether to gate only environment-relevant artifacts or every recorded artifact (`--all-artifacts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactScope {
    Environment,
    All,
}

/// The platform/abi/python-version/markers a lock must satisfy. Version-granular ecosystems leave
/// this empty.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    pub markers: Vec<String>,
}

/// The context an adapter needs to fetch releases and locked metadata for the right artifacts.
#[derive(Debug, Clone)]
pub struct TargetContext<'a> {
    pub project: &'a Project,
    pub environments: &'a [Environment],
    pub artifacts: ArtifactScope,
}

/// The result of an opt-in `build`/`sync` verification step.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub ok: bool,
    pub detail: String,
}
