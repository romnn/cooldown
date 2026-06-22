#![allow(
    missing_docs,
    reason = "application DTOs intentionally mirror cooldown-render; duplicating every field doc here would create a second divergent contract description"
)]

use cooldown_core::{Diagnostic, MemberRef, SkipReason, Status, UpdateKind};

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub min_age_days: f64,
    pub source: String,
    pub clamped_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LatestInfo {
    pub version: String,
    pub published_at: Option<String>,
    pub age_days: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutdatedStatus {
    UpToDate,
    Adoptable,
    /// A newer version has matured past its window and would otherwise be adoptable, but the
    /// whole-graph upgrade resolve cannot land it: another requirement in the graph conflicts with
    /// it. `upgrade` reports the same dependency as `held`, so `outdated` and `upgrade` agree.
    Blocked,
    InCooldown,
    Exempt,
    Held,
    CurrentInCooldown,
    UnknownAge,
    Error,
}

impl From<Status> for OutdatedStatus {
    fn from(status: Status) -> Self {
        match status {
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

impl OutdatedStatus {
    /// Ordering key for the report: things needing attention first, the ready-to-adopt updates
    /// last (so the actionable "what's still cooling / stuck" rows lead).
    #[must_use]
    pub(crate) fn sort_rank(self) -> u8 {
        match self {
            OutdatedStatus::Error => 0,
            OutdatedStatus::UnknownAge => 1,
            OutdatedStatus::Held => 2,
            OutdatedStatus::Blocked => 3,
            OutdatedStatus::CurrentInCooldown => 4,
            OutdatedStatus::InCooldown => 5,
            OutdatedStatus::Exempt => 6,
            OutdatedStatus::UpToDate => 7,
            OutdatedStatus::Adoptable => 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutdatedItem {
    pub name: String,
    pub tool: String,
    pub project: String,
    pub registry: Option<String>,
    pub direct: bool,
    pub current: String,
    /// The workspace member package(s) declaring this dependency (e.g. cargo member crates, pnpm
    /// workspace packages); empty when the source cannot be attributed.
    pub members: Vec<MemberRef>,
    pub window: Window,
    /// Age in (fractional) days of the shown upgrade candidate — the version whose cooldown
    /// [`window`](OutdatedItem::window) is shown. `None` when there is no newer candidate (up to
    /// date, a commit pin) or its publish time is unknown.
    pub candidate_age_days: Option<f64>,
    /// The version the cooldown countdown refers to, when it is *not* the
    /// [`latest`](OutdatedItem::latest) version — e.g. under `--countdown soonest`, where an
    /// intermediate version matures before the newest one. `None` when the countdown tracks the
    /// latest version (the default), so no extra label is needed.
    pub cooldown_version: Option<String>,
    pub status: OutdatedStatus,
    pub adoptable_target: Option<String>,
    /// When [`status`](OutdatedItem::status) is [`OutdatedStatus::Blocked`], the name of the package
    /// whose requirement holds the matured [`adoptable_target`](OutdatedItem::adoptable_target) out of
    /// the resolved graph. `None` for every other status, or when no single blocker can be named.
    pub blocked_by: Option<String>,
    pub latest: Option<LatestInfo>,
    pub error: Option<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutdatedSummary {
    pub total: usize,
    pub adoptable: usize,
    pub blocked: usize,
    pub in_cooldown: usize,
    pub up_to_date: usize,
    pub exempt: usize,
    pub held: usize,
    pub unknown_age: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckMeta {
    pub scope: String,
    pub artifact_scope: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Violation,
    /// A violation acknowledged in the baseline file, so it does not fail the gate.
    Acknowledged,
    /// A too-fresh transitive dep permitted by `check --transitive allow` — reported but non-fatal,
    /// and distinct from a baselined `Acknowledged` so the difference is auditable.
    Allowed,
    UnknownAge,
    Error,
}

impl CheckStatus {
    #[must_use]
    pub fn from_pin_status(status: Status, acknowledged: bool) -> Option<Self> {
        if acknowledged {
            return Some(CheckStatus::Acknowledged);
        }
        match status {
            Status::CurrentInCooldown => Some(CheckStatus::Violation),
            Status::UnknownAge => Some(CheckStatus::UnknownAge),
            Status::UpToDate
            | Status::Exempt
            | Status::Adoptable
            | Status::InCooldown
            | Status::Held => None,
        }
    }

    /// Ordering key for the report: gate failures first, the acknowledged (benign) rows last.
    #[must_use]
    pub(crate) fn sort_rank(self) -> u8 {
        match self {
            CheckStatus::Violation => 0,
            CheckStatus::Error => 1,
            CheckStatus::UnknownAge => 2,
            CheckStatus::Acknowledged => 3,
            CheckStatus::Allowed => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckItem {
    pub name: String,
    pub tool: String,
    pub project: String,
    pub members: Vec<MemberRef>,
    pub registry: Option<String>,
    pub direct: bool,
    pub current: String,
    pub published_at: Option<String>,
    pub age_days: Option<f64>,
    pub window: Window,
    pub status: CheckStatus,
    pub graph_held: bool,
    pub graph_floor: Option<String>,
    pub error: Option<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckSummary {
    pub checked: usize,
    pub direct: usize,
    pub exempt: usize,
    pub acknowledged: usize,
    /// Too-fresh transitive deps permitted by `check --transitive allow` (reported, non-fatal).
    pub allowed: usize,
    pub unknown_age: usize,
    pub errors: usize,
    pub violations: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedInfo {
    pub reason: SkipReason,
    pub message: String,
    pub offending: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpgradeItem {
    pub name: String,
    pub tool: String,
    pub project: String,
    /// Whether this dependency is declared directly by a workspace member; `false` means it was
    /// pulled in transitively (reports attribute it as "via …").
    pub direct: bool,
    /// Whether the change lowers the version (a cooldown rollback) rather than raising it.
    pub downgrade: bool,
    pub members: Vec<MemberRef>,
    pub registry: Option<String>,
    pub from: String,
    pub to: String,
    pub kind: UpdateKind,
    pub applied: bool,
    pub skipped: Option<SkippedInfo>,
    pub error: Option<Diagnostic>,
}

impl UpgradeItem {
    /// Ordering key for the report — errored/skipped changes first, the applied (succeeded) ones
    /// last; `--dry-run` items are all `planned`, so they fall back to name order. Mirrors the
    /// status precedence the renderer uses (applied > skipped > error > planned).
    #[must_use]
    pub(crate) fn sort_rank(&self) -> u8 {
        if let Some(skip) = &self.skipped {
            // Real skips lead; a held-back `needs --major` is informational (not a failure), so it
            // sorts after them, before planned and the applied (succeeded) rows.
            if skip.reason == SkipReason::NeedsMajor {
                2
            } else {
                1
            }
        } else if self.error.is_some() {
            0
        } else if self.applied {
            4
        } else {
            3
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeSummary {
    pub applied: usize,
    pub skipped: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInfo {
    pub requested: bool,
    pub ok: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeMeta {
    pub applied: bool,
    pub lock_verified: Option<bool>,
    pub build: BuildInfo,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainStep {
    pub layer: String,
    pub field: String,
    pub selector: Option<String>,
    pub min_age_days: Option<f64>,
    pub applied: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveInfo {
    pub min_age_days: f64,
    pub decided_by: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainMeta {
    pub project: String,
    pub registry: Option<String>,
    pub effective: EffectiveInfo,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigItem {
    pub project: String,
    pub tool: String,
    pub effective_default_min_age_days: f64,
    pub source: String,
    pub strict_native: bool,
    pub layers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSummary {
    pub projects: usize,
}
