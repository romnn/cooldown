//! The serializable view model — the stable `--json` contract. One common envelope, with
//! command-specific `summary`, `items[]`, and a flattened command-specific top-level `meta`.
//!
//! Stability policy: SemVer-style — additive fields don't bump [`SCHEMA_VERSION`]; a
//! removal/retype/semantic change does. Consumers ignore unknown fields. The `status` and
//! `minAgeSource` enums are part of the contract.

use cooldown_core::{Diagnostic, SkipReason, Status, UpdateKind};
use serde::Serialize;

/// The JSON schema version. Bumped only on a removal/retype/semantic change.
pub const SCHEMA_VERSION: u32 = 1;

/// The one common envelope, identical in shape across ecosystems and commands.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope<M: Serialize, S: Serialize, I: Serialize> {
    pub schema_version: u32,
    pub command: &'static str,
    pub ok: bool,
    /// RFC3339 UTC timestamp.
    pub generated_at: String,
    /// Command-specific top-level fields (flattened): `scope`/`artifactScope` for check, etc.
    #[serde(flatten)]
    pub meta: M,
    pub summary: S,
    pub items: Vec<I>,
    pub warnings: Vec<Diagnostic>,
    pub errors: Vec<Diagnostic>,
}

impl<M: Serialize, S: Serialize, I: Serialize> Envelope<M, S, I> {
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
    pub min_age_days: f64,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clamped_by: Option<String>,
}

/// The `latest` block on an outdated item.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestInfo {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_days: Option<f64>,
}

/// The status of an `outdated` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutdatedStatus {
    UpToDate,
    Adoptable,
    InCooldown,
    Exempt,
    Held,
    CurrentInCooldown,
    UnknownAge,
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutdatedItem {
    pub name: String,
    pub ecosystem: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    pub direct: bool,
    pub current: String,
    pub window: Window,
    pub status: OutdatedStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adoptable_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<LatestInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Diagnostic>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutdatedSummary {
    pub total: usize,
    pub adoptable: usize,
    pub in_cooldown: usize,
    pub up_to_date: usize,
    pub exempt: usize,
    pub held: usize,
    pub unknown_age: usize,
    pub errors: usize,
}

/// `outdated` has no extra top-level fields.
#[derive(Debug, Clone, Serialize)]
pub struct OutdatedMeta {}

/// The status of a `check` finding. Passing (mature/exempt) deps are not findings; they are counted
/// in `summary` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Violation,
    Acknowledged,
    UnknownAge,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckItem {
    pub name: String,
    pub ecosystem: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    pub direct: bool,
    pub current: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_days: Option<f64>,
    pub window: Window,
    pub status: CheckStatus,
    pub graph_held: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_floor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Diagnostic>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckSummary {
    pub checked: usize,
    pub direct: usize,
    pub exempt: usize,
    pub acknowledged: usize,
    pub unknown_age: usize,
    pub errors: usize,
    pub violations: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckMeta {
    /// `lockfile-graph` | `direct-only`.
    pub scope: String,
    /// `environment` | `all`.
    pub artifact_scope: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedInfo {
    pub reason: SkipReason,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offending: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeItem {
    pub name: String,
    pub ecosystem: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    pub from: String,
    pub to: String,
    pub kind: UpdateKind,
    pub applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SkippedInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Diagnostic>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeSummary {
    pub applied: usize,
    pub skipped: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub requested: bool,
    pub ok: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeMeta {
    pub applied: bool,
    /// Re-lock result; `null` for `--dry-run` (which never mutates).
    pub lock_verified: Option<bool>,
    pub build: BuildInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainStep {
    pub layer: String,
    pub field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_age_days: Option<f64>,
    pub applied: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveInfo {
    pub min_age_days: f64,
    pub decided_by: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainMeta {
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    pub effective: EffectiveInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExplainSummary {}
