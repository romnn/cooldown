//! The application use cases. A [`Workspace`] bundles the detected adapters, per-project layered
//! policy, and a single `now` snapshotted once for the whole run (consistency over freshness — two
//! deps evaluated 30s apart must use the same boundary).
//!
//! Policy is **per project**: the shared layers (default, global, explicit `--config`, env, CLI)
//! are common, but the native layer and the repo cascade (root → this project's dir) are scoped to
//! each project, so sibling projects never leak policy into one another.

mod advisories;
pub mod baseline;
mod change_key;
mod check;
mod clock;
mod explain;
pub(crate) mod lock;
mod outdated;
mod progress;
mod project_copy;
mod read;
mod recover;
mod release_cache;
mod resilient_apply;
mod sync;
mod upgrade;
mod workspace;

pub use baseline::Baseline;
pub use clock::{Clock, FixedClock, SystemClock};
pub use cooldown_render::{
    AdvisoryConfigInfo, BuildInfo, CheckItem, CheckMeta, CheckStatus, CheckSummary, ConfigItem,
    ConfigSummary, EffectiveInfo, ExplainDeclaration, ExplainMeta, ExplainStep, LatestInfo,
    OutdatedItem, OutdatedStatus, OutdatedSummary, SecurityInfo, SkippedInfo, UpgradeEdgeInfo,
    UpgradeItem, UpgradeMeta, UpgradeSummary, Window,
};
pub use progress::Progress;
pub use recover::{RecoveryItem, RecoveryOutcome, RecoveryStatus, RecoverySummary};
pub use sync::{SyncItem, SyncOutcome, SyncStatus, SyncSummary};
pub use workspace::{
    AdapterSet, AdvisoryFailureMode, Exit, MemberExcludes, ProjectCtx, RunOpts, RunScope,
    TransitiveGate, Workspace,
};

pub(crate) use recover::{RecoveryTarget, recover_targets};
pub(crate) use workspace::{
    FetchedRelease, LockReportAction, age_days, diag_from_error, empty_selection_diagnostic,
    lock_report_outcome, recovery_diagnostics, render_window, round2, security_info,
};
