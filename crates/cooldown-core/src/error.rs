//! The core error type. Adapter-internal errors (`reqwest`, `io`, `serde`) convert into
//! [`CoreError`] at the port boundary via `From`; non-fatal apply skips are `Ok` data, never `Err`.

use std::fmt;

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

/// A boxed, thread-safe error used to carry an opaque transient cause.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Errors raised at or below the port boundary.
///
/// The variants separate *classes* of failure so the app can decide retry/exit behaviour without
/// matching on error strings. [`CoreError::is_transient`] is the retry classifier, kept distinct
/// from the `Display` text.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// The requested package/version/module was not found upstream (a 404/410 equivalent).
    #[error("not found: {0}")]
    NotFound(String),

    /// A transient failure (network blip, 5xx, 429) that a retry might fix.
    #[error("transient error: {0}")]
    Transient(#[source] BoxError),

    /// An external tool (`go`, `cargo`, `uv`) exited non-zero.
    #[error("tool `{tool}` failed with status {status}: {stderr}")]
    Tool {
        tool: String,
        status: i32,
        stderr: String,
    },

    /// Invalid configuration or command input: a malformed `cooldown.toml`, a bad duration or
    /// glob, or a disallowed flag combination. A *usage* error — the user must fix their input.
    #[error("config error: {0}")]
    Config(String),

    /// An upstream **registry** payload could not be parsed (a Go `.info`/`@latest` document, a
    /// PyPI JSON response, or a crates.io sparse-index line). An environment-level data fault.
    #[error("parse error: {0}")]
    Parse(String),

    /// The resolved dependency graph could not be read: a malformed `uv.lock`, or unparseable
    /// `go list`/`cargo metadata` output. Distinct from [`CoreError::StaleLock`] (which is present
    /// but out of date) — here the lock data itself is unreadable.
    #[error("unreadable lock: {0}")]
    LockUnreadable(String),

    /// A lockfile/manifest is stale relative to its source, or absent, making evaluation unsound.
    #[error("stale or absent lock: {0}")]
    StaleLock(String),

    /// An I/O failure not attributable to a single dependency.
    #[error("io error: {0}")]
    Io(String),

    /// The cache or registry was consulted in `--offline` mode and missed.
    #[error("offline cache miss: {0}")]
    OfflineMiss(String),
}

impl CoreError {
    /// Whether retrying the operation could plausibly succeed. Network/5xx/429 are transient; a
    /// `NotFound`, a config or parse error, or a non-zero tool exit are not.
    pub fn is_transient(&self) -> bool {
        matches!(self, CoreError::Transient(_))
    }

    /// Convenience constructor for a transient cause from any error type.
    pub fn transient(e: impl Into<BoxError>) -> Self {
        CoreError::Transient(e.into())
    }

    /// The structured kind for the JSON `Diagnostic.kind` field. Each error class maps to exactly
    /// one kind, so a consumer never has to disambiguate by message string.
    pub fn diagnostic_kind(&self) -> DiagnosticKind {
        match self {
            CoreError::NotFound(_) => DiagnosticKind::NotFound,
            CoreError::Transient(_) | CoreError::OfflineMiss(_) => DiagnosticKind::Transient,
            CoreError::Tool { .. } => DiagnosticKind::ToolFailed,
            CoreError::Config(_) => DiagnosticKind::Config,
            CoreError::Parse(_) => DiagnosticKind::Parse,
            CoreError::LockUnreadable(_) => DiagnosticKind::LockfileUnreadable,
            CoreError::StaleLock(_) => DiagnosticKind::StaleLock,
            CoreError::Io(_) => DiagnosticKind::Transient,
        }
    }
}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        CoreError::Io(e.to_string())
    }
}

/// A structured diagnostic surfaced in the JSON envelope's `warnings`/`errors` arrays and on the
/// TTY. `kind` and `message` are always set; the rest are populated when applicable so a consumer
/// can map a diagnostic back to a baseline key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ecosystem: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// The closed set of diagnostic kinds. Part of the JSON contract (`schemaVersion` bumps on change).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    Transient,
    NotFound,
    UnknownAge,
    StricterNative,
    Yanked,
    StaleLock,
    ToolFailed,
    /// A local lockfile/manifest or resolved-graph dump could not be read.
    LockfileUnreadable,
    /// Invalid configuration or command input (the user must fix it).
    Config,
    /// An upstream registry payload could not be parsed.
    Parse,
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DiagnosticKind::Transient => "transient",
            DiagnosticKind::NotFound => "not_found",
            DiagnosticKind::UnknownAge => "unknown_age",
            DiagnosticKind::StricterNative => "stricter_native",
            DiagnosticKind::Yanked => "yanked",
            DiagnosticKind::StaleLock => "stale_lock",
            DiagnosticKind::ToolFailed => "tool_failed",
            DiagnosticKind::LockfileUnreadable => "lockfile_unreadable",
            DiagnosticKind::Config => "config",
            DiagnosticKind::Parse => "parse",
        };
        f.write_str(s)
    }
}

impl Diagnostic {
    /// Start a diagnostic with just the required fields; chain the `with_*` setters for the rest.
    pub fn new(kind: DiagnosticKind, message: impl Into<String>) -> Self {
        Diagnostic {
            kind,
            message: message.into(),
            ecosystem: None,
            project: None,
            package: None,
            version: None,
            registry: None,
            tool: None,
            path: None,
        }
    }

    pub fn with_ecosystem(mut self, e: impl Into<String>) -> Self {
        self.ecosystem = Some(e.into());
        self
    }
    pub fn with_project(mut self, p: impl Into<String>) -> Self {
        self.project = Some(p.into());
        self
    }
    pub fn with_package(mut self, p: impl Into<String>) -> Self {
        self.package = Some(p.into());
        self
    }
    pub fn with_version(mut self, v: impl Into<String>) -> Self {
        self.version = Some(v.into());
        self
    }
    pub fn with_registry(mut self, r: impl Into<String>) -> Self {
        self.registry = Some(r.into());
        self
    }
    pub fn with_tool(mut self, t: impl Into<String>) -> Self {
        self.tool = Some(t.into());
        self
    }
    pub fn with_path(mut self, p: impl Into<String>) -> Self {
        self.path = Some(p.into());
        self
    }
}
