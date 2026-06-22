//! The pure policy core: domain model, the two decision functions
//! ([`evaluate`](evaluate::evaluate) and [`check_pin`](evaluate::check_pin)), the policy
//! [`resolve`](policy::resolve)r, the ports, and config parsing. No concrete I/O, no clock, no
//! version parsing — everything that decides "is this version too fresh?" lives here, once, for
//! every tool.

pub mod config;
pub mod duration;
pub mod error;
pub mod evaluate;
pub mod model;
pub mod policy;
pub mod ports;

pub use error::{CoreError, Diagnostic, DiagnosticKind, Result, ToolTermination};
pub use evaluate::{FixVerdict, ResolveContext, check_pin, evaluate, evaluate_fix};
pub use model::*;
pub use policy::{
    ByKind, Origin, PatternGlob, PolicyLayer, PolicyStack, Resolution, ResolveKind, ResolveQuery,
    ResolvedWindow, Rule, Selector, TraceStep, WindowSpec, resolve,
};
pub use ports::{
    Capabilities, Clock, NativePolicyLayer, NativeRule, PackageRegistry, ProjectMutationFile,
    ProjectMutationJournal, RawArtifact, RawRelease, RawWindow, ReleaseFetcher, ResolvedPolicy,
    SyncReport, Tool, ToolRead, ToolWrite, debug_assert_sorted, normalize_native,
};
