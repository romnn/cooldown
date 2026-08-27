//! Shared dependency policy, adapter contracts, and portable mutation lifecycle.
//!
//! The crate owns the domain model, policy resolution, decision functions, and filesystem state
//! used to validate or restore accepted project mutations.
//! Concrete package-manager commands, network clients, clocks, and ecosystem-specific version
//! parsing remain outside the core.

pub mod advisory;
pub mod config;
pub mod duration;
pub mod error;
pub mod evaluate;
pub mod fs;
pub mod model;
pub mod mutation;
pub mod policy;
pub mod ports;
pub mod redact;

pub use advisory::{
    Advisory, AdvisoryContext, AdvisoryFetch, AdvisoryId, AdvisoryMode, AdvisoryPolicy,
    AdvisorySeverity, AdvisorySource, AdvisorySourceId, AdvisorySourceKind, AffectedRange,
    PackageAdvisories, RawAdvisory, RawAffectedRange, RawRangeEvent, ResolvedAdvisoryPolicy,
    SecurityRelevance, apply_security_window, classify_advisory, resolve_advisory_policy,
};
pub use error::{CoreError, Diagnostic, DiagnosticKind, Result, ToolTermination, failure_detail};
pub use evaluate::{
    CeilingHold, CeilingReason, FixVerdict, ResolveContext, advisories_reintroduced_by, check_pin,
    check_pin_advised, evaluate, evaluate_advised, evaluate_ceiling_hold,
    evaluate_ceiling_hold_advised, evaluate_fix, evaluate_fix_advised,
};
pub use model::*;
pub use mutation::{
    AcceptedProjectState, ProjectInputSnapshot, ProjectMutationFile, ProjectMutationJournal,
    ProjectMutationState,
};
pub use policy::{
    ByKind, MaxMajorPick, Origin, PatternGlob, PolicyLayer, PolicyStack, Resolution, ResolveKind,
    ResolveQuery, ResolvedWindow, Rule, Selector, TraceStep, WindowSpec, exempt_package_globs,
    resolve, resolve_max_major, window_exclude_newer,
};
pub use ports::{
    AcceptedPublication, ApplyAttempt, ApplyAttemptOutcome, ApplyObserver, Capabilities, Clock,
    IsolatedMutation, IsolatedMutationProject, IsolatedMutationStrategy, MutationExecution,
    MutationRecovery, NativePolicyLayer, NativeRule, PackageRegistry, PreparedMutation,
    RawArtifact, RawRelease, RawWindow, RecoveryDisposition, ReleaseFetcher, ResolveInputs,
    ResolvedPolicy, SyncReport, SyncScope, Tool, ToolRead, ToolWrite, debug_assert_sorted,
    normalize_native,
};
