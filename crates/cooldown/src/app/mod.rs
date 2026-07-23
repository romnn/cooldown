//! The application use cases. A [`Workspace`] bundles the detected adapters, per-project layered
//! policy, and a single `now` snapshotted once for the whole run (consistency over freshness — two
//! deps evaluated 30s apart must use the same boundary).
//!
//! Policy is **per project**: the shared layers (default, global, explicit `--config`, env, CLI)
//! are common, but the native layer and the repo cascade (root → this project's dir) are scoped to
//! each project, so sibling projects never leak policy into one another.

pub mod baseline;
mod change_key;
mod check;
mod clock;
mod explain;
mod lock;
mod outdated;
mod progress;
mod project_copy;
mod read;
mod release_cache;
mod resilient_apply;
mod sync;
mod upgrade;
mod workspace;

pub use baseline::Baseline;
pub use clock::{Clock, FixedClock, SystemClock};
pub use cooldown_render::{
    BuildInfo, CheckItem, CheckMeta, CheckStatus, CheckSummary, ConfigItem, ConfigSummary,
    EffectiveInfo, ExplainMeta, ExplainStep, LatestInfo, OutdatedItem, OutdatedStatus,
    OutdatedSummary, SkippedInfo, UpgradeItem, UpgradeMeta, UpgradeSummary, Window,
};
pub use progress::Progress;
pub use sync::{SyncItem, SyncOutcome, SyncStatus, SyncSummary};
pub use workspace::{AdapterSet, Exit, ProjectCtx, RunOpts, TransitiveGate, Workspace};

pub(crate) use workspace::{
    LockReportAction, age_days, diag_from_error, lock_report_outcome, render_window, round2,
};
