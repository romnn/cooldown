//! The pure policy core: domain model, the two decision functions
//! ([`evaluate`](evaluate::evaluate) and [`check_pin`](evaluate::check_pin)), the policy
//! [`resolve`](policy::resolve), the ports, and config parsing. No concrete I/O, no clock, no
//! version parsing — everything that decides "is this version too fresh?" lives here, once, for
//! every tool.

pub mod config;
pub mod duration;
pub mod error;
pub mod evaluate;
pub mod fs;
pub mod model;
pub mod policy;
pub mod ports;
pub mod redact;

pub use error::{CoreError, Diagnostic, DiagnosticKind, Result, ToolTermination, failure_detail};
pub use evaluate::{
    CeilingHold, CeilingReason, FixVerdict, ResolveContext, check_pin, evaluate,
    evaluate_ceiling_hold, evaluate_fix,
};
pub use model::*;
pub use policy::{
    ByKind, MaxMajorPick, Origin, PatternGlob, PolicyLayer, PolicyStack, Resolution, ResolveKind,
    ResolveQuery, ResolvedWindow, Rule, Selector, TraceStep, WindowSpec, exempt_package_globs,
    resolve, resolve_max_major, window_exclude_newer,
};
pub use ports::{
    AcceptedProjectState, AcceptedPublication, ApplyAttempt, ApplyObserver, Capabilities, Clock,
    IsolatedMutation, IsolatedMutationStrategy, MutationExecution, MutationRecovery,
    NativePolicyLayer, NativeRule, PackageRegistry, ProjectInputSnapshot, ProjectMutationFile,
    ProjectMutationJournal, ProjectMutationState, RawArtifact, RawRelease, RawWindow,
    RecoveryDisposition, ReleaseFetcher, ResolveInputs, ResolvedPolicy, SyncReport, SyncScope,
    Tool, ToolRead, ToolWrite, debug_assert_sorted, normalize_native,
};
