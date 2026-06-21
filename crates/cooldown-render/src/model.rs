//! The serializable view model — the stable `--json` contract. One common envelope, with
//! command-specific `summary`, `items[]`, and a flattened command-specific top-level `meta`.
//!
//! Stability policy: SemVer-style — additive fields don't bump [`SCHEMA_VERSION`]; a
//! removal/retype/semantic change does. Consumers ignore unknown fields. The `status` and
//! `minAgeSource` enums are part of the contract.

use cooldown_core::{Diagnostic, MemberRef, SkipReason, Status, UpdateKind};
use serde::Serialize;

/// The JSON schema version. Bumped only on a removal/retype/semantic change.
pub const SCHEMA_VERSION: u32 = 1;

/// The one common envelope, identical in shape across tools and commands.
///
/// Every `--json` document is an `Envelope`. The three type parameters carry the
/// command-specific parts: `M` the flattened top-level [`meta`](Envelope::meta),
/// `S` the [`summary`](Envelope::summary) counts, and `I` the
/// [`items`](Envelope::items) element. The remaining fields are common to all
/// commands. Serialize one with [`to_json`](crate::to_json).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope<M: Serialize, S: Serialize, I: Serialize> {
    /// The contract version, always [`SCHEMA_VERSION`]. Serialized as `schemaVersion`.
    pub schema_version: u32,
    /// The command that produced this envelope, e.g. `"outdated"` or `"check"`.
    pub command: &'static str,
    /// Whether the run succeeded; mirrors the process exit code (`true` iff `0`).
    pub ok: bool,
    /// RFC3339 UTC timestamp marking when the envelope was generated.
    pub generated_at: String,
    /// Command-specific top-level fields (flattened): `scope`/`artifactScope` for check, etc.
    #[serde(flatten)]
    pub meta: M,
    /// The command-specific aggregate counts (e.g. [`OutdatedSummary`], [`CheckSummary`]).
    pub summary: S,
    /// The per-dependency rows (e.g. [`OutdatedItem`], [`CheckItem`]).
    pub items: Vec<I>,
    /// Non-fatal [`Diagnostic`]s that did not affect the exit code.
    pub warnings: Vec<Diagnostic>,
    /// Fatal or per-item [`Diagnostic`]s encountered during the run.
    pub errors: Vec<Diagnostic>,
}

impl<M: Serialize, S: Serialize, I: Serialize> Envelope<M, S, I> {
    /// Builds an envelope with empty `warnings`/`errors` and the current [`SCHEMA_VERSION`].
    ///
    /// Push diagnostics onto [`warnings`](Envelope::warnings) and
    /// [`errors`](Envelope::errors) afterwards if any were collected.
    pub fn new(
        command: &'static str,
        ok: bool,
        generated_at: String,
        meta: M,
        summary: S,
        items: Vec<I>,
    ) -> Self {
        Envelope {
            schema_version: SCHEMA_VERSION,
            command,
            ok,
            generated_at,
            meta,
            summary,
            items,
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }
}

/// The resolved window block on an item: `{ minAgeDays, source, clampedBy? }`. Days are
/// display-only float days; the boundary comparison is on the underlying instant.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    /// The resolved minimum age in (fractional) days the dependency must reach.
    pub min_age_days: f64,
    /// Which config layer decided the window, e.g. `"default"` or a rule selector.
    pub source: String,
    /// The selector of a stricter native/registry policy that clamped the window, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clamped_by: Option<String>,
}

/// The `latest` block on an [`OutdatedItem`]: the newest existing version and its age.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestInfo {
    /// The newest existing version string, adoptable or not.
    pub version: String,
    /// The RFC3339 publish timestamp of [`version`](LatestInfo::version), if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// The age of [`version`](LatestInfo::version) in (fractional) days, if its publish time is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_days: Option<f64>,
}

/// The status of an `outdated` item.
///
/// Serialized in `snake_case`. This is the render-side mirror of
/// [`cooldown_core::Status`], plus an [`Error`](OutdatedStatus::Error) variant
/// for items whose evaluation failed. Convert from a core status with the
/// [`From<Status>`](OutdatedStatus::from) impl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutdatedStatus {
    /// No newer adoptable version exists.
    UpToDate,
    /// A newer version exists and has matured past its window.
    Adoptable,
    /// A newer version exists but is younger than its window.
    InCooldown,
    /// Exempted by an `allow` rule (or a pseudo/commit pin).
    Exempt,
    /// Pinned, so it will not move on its own: a commit pin or an exact manifest pin.
    Held,
    /// The currently-locked version is itself younger than its window.
    CurrentInCooldown,
    /// The relevant release has no known publish time.
    UnknownAge,
    /// Evaluation of this item failed; see the item's [`error`](OutdatedItem::error).
    Error,
}

impl From<Status> for OutdatedStatus {
    fn from(s: Status) -> Self {
        match s {
            Status::UpToDate => OutdatedStatus::UpToDate,
            Status::Adoptable => OutdatedStatus::Adoptable,
            Status::InCooldown => OutdatedStatus::InCooldown,
            Status::Exempt => OutdatedStatus::Exempt,
            Status::Held => OutdatedStatus::Held,
            Status::CurrentInCooldown => OutdatedStatus::CurrentInCooldown,
            Status::UnknownAge => OutdatedStatus::UnknownAge,
        }
    }
}

/// One row in an `outdated` report: a dependency and its newest adoptable/latest versions.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutdatedItem {
    /// The package name (tool-native identifier).
    pub name: String,
    /// The tool the package belongs to, e.g. `"go"`, `"cargo"`, or `"uv"`.
    pub tool: String,
    /// The project (manifest/workspace member) the dependency was found in.
    pub project: String,
    /// The registry the version data came from, omitted when not applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// Whether this is a direct dependency (as opposed to transitive).
    pub direct: bool,
    /// The currently-locked version.
    pub current: String,
    /// The workspace member package(s) that declare this dependency at this version (e.g. cargo
    /// member crates, pnpm/npm workspace packages). Empty when the source cannot be attributed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MemberRef>,
    /// The resolved cooldown [`Window`] applied to this dependency.
    pub window: Window,
    /// Age in (fractional) days of the shown upgrade candidate — the version whose
    /// [`window`](OutdatedItem::window) is shown. Omitted when there is no newer candidate (up to
    /// date, a commit pin) or its publish time is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_age_days: Option<f64>,
    /// The version the cooldown countdown refers to, when it is *not* the
    /// [`latest`](OutdatedItem::latest) version — e.g. under `--countdown soonest`, where an
    /// intermediate version matures before the newest one. Omitted when the countdown tracks the
    /// latest version (the default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_version: Option<String>,
    /// The verdict for this dependency.
    pub status: OutdatedStatus,
    /// The newest version that has matured past its window, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adoptable_target: Option<String>,
    /// The newest existing version and its age, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<LatestInfo>,
    /// The error that prevented evaluation, present iff `status` is [`OutdatedStatus::Error`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Diagnostic>,
}

/// The aggregate counts for an `outdated` report, keyed by [`OutdatedStatus`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutdatedSummary {
    /// The total number of dependencies evaluated.
    pub total: usize,
    /// The number with status [`OutdatedStatus::Adoptable`].
    pub adoptable: usize,
    /// The number with status [`OutdatedStatus::InCooldown`].
    pub in_cooldown: usize,
    /// The number with status [`OutdatedStatus::UpToDate`].
    pub up_to_date: usize,
    /// The number with status [`OutdatedStatus::Exempt`].
    pub exempt: usize,
    /// The number with status [`OutdatedStatus::Held`].
    pub held: usize,
    /// The number with status [`OutdatedStatus::UnknownAge`].
    pub unknown_age: usize,
    /// The number whose evaluation failed (status [`OutdatedStatus::Error`]).
    pub errors: usize,
}

/// The flattened top-level `meta` for `outdated`. The command has no extra top-level fields.
#[derive(Debug, Clone, Serialize)]
pub struct OutdatedMeta {}

/// The status of a `check` finding. Passing (mature/exempt) deps are not findings; they are counted
/// in `summary` instead.
///
/// Serialized in `snake_case`. Derived from a core [`cooldown_core::Status`] by
/// [`check_status_of`](crate::tty::check_status_of), which returns `None` for
/// passing dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The locked version is younger than its window — the gate failed for this dependency.
    Violation,
    /// A violation that has been acknowledged (baselined) and so does not fail the gate.
    Acknowledged,
    /// A too-fresh transitive dependency permitted by `check --transitive allow`: reported but
    /// non-fatal, and distinct from a baselined acknowledgment so the two stay auditable apart.
    Allowed,
    /// The relevant release has no known publish time, so maturity could not be decided.
    UnknownAge,
    /// Evaluation of this dependency failed; see the item's [`error`](CheckItem::error).
    Error,
}

/// One finding in a `check` report: a dependency that did not pass the cooldown gate cleanly.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckItem {
    /// The package name (tool-native identifier).
    pub name: String,
    /// The tool the package belongs to, e.g. `"go"`, `"cargo"`, or `"uv"`.
    pub tool: String,
    /// The project (manifest/workspace member) the dependency was found in.
    pub project: String,
    /// The workspace member package(s) that declare this dependency. Empty when not attributable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MemberRef>,
    /// The registry the version data came from, omitted when not applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// Whether this is a direct dependency (as opposed to transitive).
    pub direct: bool,
    /// The currently-locked version that was checked.
    pub current: String,
    /// The RFC3339 publish timestamp of [`current`](CheckItem::current), if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// The age of [`current`](CheckItem::current) in (fractional) days, if its publish time is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_days: Option<f64>,
    /// The resolved cooldown [`Window`] applied to this dependency.
    pub window: Window,
    /// The finding's status.
    pub status: CheckStatus,
    /// Whether the resolved graph forces this (too-fresh) version (MVS floor / `=` pin).
    pub graph_held: bool,
    /// The lowest version the resolved graph permits, when [`graph_held`](CheckItem::graph_held).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_floor: Option<String>,
    /// The error that prevented evaluation, present iff `status` is [`CheckStatus::Error`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Diagnostic>,
}

/// The aggregate counts for a `check` report.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckSummary {
    /// The total number of dependencies checked.
    pub checked: usize,
    /// How many of [`checked`](CheckSummary::checked) are direct dependencies.
    pub direct: usize,
    /// The number exempted by an `allow` rule or a pseudo/commit pin (passing, not findings).
    pub exempt: usize,
    /// The number of violations that were acknowledged (baselined).
    pub acknowledged: usize,
    /// The number of too-fresh transitive deps permitted by `check --transitive allow`
    /// (status [`CheckStatus::Allowed`]; reported, non-fatal).
    pub allowed: usize,
    /// The number with status [`CheckStatus::UnknownAge`].
    pub unknown_age: usize,
    /// The number whose evaluation failed (status [`CheckStatus::Error`]).
    pub errors: usize,
    /// The number of unacknowledged violations (status [`CheckStatus::Violation`]).
    pub violations: usize,
}

/// The flattened top-level `meta` for `check`: the scope of the run.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckMeta {
    /// The graph scope: `"lockfile-graph"` (the full resolved graph) or `"direct-only"`.
    pub scope: String,
    /// The artifact scope: `"environment"` (relevant artifacts only) or `"all"`.
    pub artifact_scope: String,
}

/// Why a planned mutation was not applied. Mirrors a [`cooldown_core::SkipReason`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedInfo {
    /// The structured reason the change was skipped.
    pub reason: SkipReason,
    /// A human-readable explanation of the skip.
    pub message: String,
    /// The offending dependency that forced the skip (e.g. a too-fresh transitive), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offending: Option<String>,
}

/// One row in an `upgrade` or `fix` report: a planned version change and its outcome.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeItem {
    /// The package name (tool-native identifier).
    pub name: String,
    /// The tool the package belongs to, e.g. `"go"`, `"cargo"`, or `"uv"`.
    pub tool: String,
    /// The project (manifest/workspace member) the dependency was found in.
    pub project: String,
    /// Whether the dependency is declared directly by a workspace member; `false` means transitive
    /// (the report attributes it as "via …").
    pub direct: bool,
    /// The workspace member package(s) that declare this dependency (direct) or reach it through the
    /// graph (transitive). Empty when not attributable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MemberRef>,
    /// The registry the version data came from, omitted when not applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// The version being changed from (the current pin).
    pub from: String,
    /// The version being changed to (the planned target).
    pub to: String,
    /// The update kind of the change relative to the current pin.
    pub kind: UpdateKind,
    /// Whether the change was actually written to the manifest/lock.
    pub applied: bool,
    /// Why the change was not applied, present iff [`applied`](UpgradeItem::applied) is `false` and no error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SkippedInfo>,
    /// The error that prevented the change, if one occurred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Diagnostic>,
}

/// The aggregate counts for an `upgrade` or `fix` report.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeSummary {
    /// The number of changes that were applied.
    pub applied: usize,
    /// The number of planned changes that were skipped.
    pub skipped: usize,
    /// The number of changes that errored.
    pub errors: usize,
}

/// The post-mutation build result reported in [`UpgradeMeta`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    /// Whether a build was requested (e.g. via `--build`).
    pub requested: bool,
    /// The build outcome: `Some(true)`/`Some(false)` once run, `None` if not run.
    pub ok: Option<bool>,
}

/// The flattened top-level `meta` for `upgrade` or `fix`: apply/lock/build outcomes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeMeta {
    /// Whether any change was applied (`false` for a dry run).
    pub applied: bool,
    /// Re-lock result; `null` for `--dry-run` (which never mutates).
    pub lock_verified: Option<bool>,
    /// The post-mutation build result.
    pub build: BuildInfo,
}

/// One step in an `explain` derivation: a config layer's contribution to the resolved window.
///
/// The steps form an ordered trace, lowest-precedence first; the last
/// [`applied`](ExplainStep::applied) step is the one that decided the effective
/// window (see [`EffectiveInfo`]).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainStep {
    /// The config layer, e.g. `"default"`, `"workspace"`, or `"project"`.
    pub layer: String,
    /// The field within the layer that set the value, e.g. `"minAge"`.
    pub field: String,
    /// The selector (glob/rule key) that matched this dependency, if the step came from a rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// The minimum age in (fractional) days this step contributes, if it sets one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_age_days: Option<f64>,
    /// Whether this step won (contributed to the effective window) or was overridden.
    pub applied: bool,
    /// A human-readable note on what the step did or why it was/wasn't applied.
    pub note: String,
}

/// The resolved effective window in an `explain` report, after all layers are applied.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveInfo {
    /// The effective minimum age in (fractional) days.
    pub min_age_days: f64,
    /// A description of which layer/field decided the effective value.
    pub decided_by: String,
}

/// The flattened top-level `meta` for `explain`: the subject and its effective window.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainMeta {
    /// The project the explained dependency belongs to.
    pub project: String,
    /// The registry in effect for the dependency, omitted when not applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// The resolved effective window.
    pub effective: EffectiveInfo,
}

/// The `summary` for `explain`. The command has no aggregate counts.
#[derive(Debug, Clone, Serialize)]
pub struct ExplainSummary {}

/// One row in a `config` report: the resolved default policy for one project.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigItem {
    /// The project root relative to the repo root.
    pub project: String,
    /// The tool the project belongs to.
    pub tool: String,
    /// The resolved default cooldown window in fractional days.
    pub effective_default_min_age_days: f64,
    /// Which layer/field decided the resolved default window.
    pub source: String,
    /// Whether `strict-native` is enabled for this project's policy stack.
    pub strict_native: bool,
    /// The project policy layers, lowest authority first.
    pub layers: Vec<String>,
}

/// The aggregate counts for a `config` report.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSummary {
    /// The number of projects included in the resolved config report.
    pub projects: usize,
}

/// The flattened top-level `meta` for `config`. The command has no extra top-level fields.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigMeta {}

/// One acknowledged baseline entry written by `cooldown baseline`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineItem {
    /// The tool token.
    pub tool: String,
    /// The project path relative to the repo root.
    pub project: String,
    /// The package name.
    pub package: String,
    /// The acknowledged version.
    pub version: String,
    /// The registry the package resolves to, omitted when not applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
}

/// The aggregate counts for a `baseline` report.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineSummary {
    /// The number of acknowledged entries after the write.
    pub acknowledged: usize,
    /// The number of stale entries pruned by this run.
    pub pruned: usize,
}

/// The flattened top-level `meta` for `baseline`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineMeta {
    /// The path of the baseline file to write.
    pub path: String,
    /// Whether the command computed the would-be baseline without writing it.
    pub dry_run: bool,
}
