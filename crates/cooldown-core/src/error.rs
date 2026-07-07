//! The core error type. Adapter-internal errors (`reqwest`, `io`, `serde`) convert into
//! [`CoreError`] at the port boundary via `From`; non-fatal apply skips are `Ok` data, never `Err`.

use std::fmt;
use std::process::{ExitStatus, Output};

/// A [`Result`](std::result::Result) specialized to [`CoreError`].
///
/// Defaulting the error parameter to [`CoreError`] lets functions in the core
/// write `Result<T>` while still permitting `Result<T, OtherError>` where a
/// different error type is needed.
pub type Result<T, E = CoreError> = std::result::Result<T, E>;

/// A boxed, thread-safe error used to carry an opaque transient cause.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// How an external tool process terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolTermination {
    /// The process exited with a numeric exit code.
    ExitCode(i32),
    /// The process was terminated by a signal.
    Signal(i32),
    /// The process terminated without an exit code or signal detail.
    Unknown,
}

impl ToolTermination {
    /// Convert an OS [`ExitStatus`] into the typed termination state cooldown reports.
    #[must_use]
    pub fn from_exit_status(status: ExitStatus) -> Self {
        if let Some(code) = status.code() {
            return ToolTermination::ExitCode(code);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;

            if let Some(signal) = status.signal() {
                return ToolTermination::Signal(signal);
            }
        }
        ToolTermination::Unknown
    }
}

impl fmt::Display for ToolTermination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolTermination::ExitCode(code) => write!(f, "exit code {code}"),
            ToolTermination::Signal(signal) => write!(f, "signal {signal}"),
            ToolTermination::Unknown => f.write_str("unknown termination"),
        }
    }
}

/// The canonical human-readable detail for a non-zero tool exit.
///
/// Package managers split their diagnostics inconsistently — some write the actionable error to
/// stderr, others to stdout — so every tool driver surfaces both streams: stderr, then stdout when
/// both carry text, falling back to a synthesised termination note when the process wrote nothing.
/// This gives [`CoreError::is_local_environment_failure`] the same detail regardless of stream.
#[must_use]
pub fn failure_detail(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match (stderr.is_empty(), stdout.is_empty()) {
        (false, false) => format!("{stderr}\n{stdout}"),
        (false, true) => stderr,
        (true, false) => stdout,
        (true, true) => format!(
            "package manager exited with {}",
            ToolTermination::from_exit_status(out.status)
        ),
    }
}

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

    /// An external tool (`go`, `cargo`, `uv`) exited non-zero after being spawned.
    #[error("tool `{tool}` failed with {termination}: {stderr}")]
    Tool {
        /// The name of the invoked tool (for example `go`, `cargo`, or `uv`).
        tool: String,
        /// The way the process terminated.
        termination: ToolTermination,
        /// The captured failure detail: the tool's standard-error output, or — when a driver merges
        /// the streams — stderr followed by stdout, or a synthesised termination note when the
        /// process wrote nothing. Named for the common case; not guaranteed to be stderr alone.
        stderr: String,
    },

    /// An external tool (`go`, `cargo`, `uv`) could not be spawned at all.
    #[error("failed to spawn tool `{tool}`: {detail}")]
    ToolSpawn {
        /// The name of the invoked tool (for example `go`, `cargo`, or `uv`).
        tool: String,
        /// The attempted invocation and OS error text.
        detail: String,
    },

    /// Invalid configuration or command input: a malformed `cooldown.toml`, a bad duration or
    /// glob, or a disallowed flag combination. A *usage* error — the user must fix their input.
    #[error("config error: {0}")]
    Config(String),

    /// An upstream **registry** payload could not be parsed (a Go `.info`/`@latest` document, a
    /// `PyPI` JSON response, or a crates.io sparse-index line). An environment-level data fault.
    #[error("parse error: {0}")]
    Parse(String),

    /// The resolved dependency graph could not be read: a malformed `uv.lock`, or unparsable
    /// `go list`/`cargo metadata` output. Distinct from [`CoreError::StaleLock`] (which is present
    /// but out of date) — here the lock data itself is unreadable.
    #[error("unreadable lock: {0}")]
    LockUnreadable(String),

    /// A lockfile/manifest is stale relative to its source, or absent, making evaluation unsound.
    #[error("stale or absent lock: {0}")]
    StaleLock(String),

    /// A local filesystem read/write/create/remove failure.
    #[error("filesystem error: {0}")]
    Filesystem(String),

    /// A local path could not be represented in cooldown's UTF-8/path model.
    #[error("path encoding error: {0}")]
    PathEncoding(String),

    /// A local JSON/TOML serialization step failed.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// A project-level filesystem lock is already held by another cooldown process.
    #[error("lock conflict: {0}")]
    LockConflict(String),

    /// A non-transient local runtime/environment setup step failed.
    #[error("system error: {0}")]
    System(String),

    /// The cache or registry was consulted in `--offline` mode and missed.
    #[error("offline cache miss: {0}")]
    OfflineMiss(String),
}

impl CoreError {
    /// Whether retrying the operation could plausibly succeed. Network/5xx/429 are transient; a
    /// `NotFound`, a config or parse error, or a non-zero tool exit are not.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, CoreError::Transient(_))
    }

    /// Whether this is a tool-spawn failure rather than a non-zero exit from a spawned tool.
    #[must_use]
    pub fn is_tool_spawn_failure(&self) -> bool {
        matches!(self, CoreError::ToolSpawn { .. })
    }

    /// Whether this error reflects a broken **local environment** rather than the resolver reporting
    /// the requested dependency graph unsatisfiable.
    ///
    /// Resilient apply uses this to decide whether an `apply` failure is a per-candidate resolver
    /// conflict — isolate the culprit and apply the rest — or a whole-environment fault that must
    /// propagate, so a missing binary, full disk, read-only tree, or corrupt package-manager store is
    /// never misreported as "every candidate held".
    ///
    /// The structured variants (a tool that could not be spawned, or a filesystem/lock/serialization
    /// fault cooldown itself raised) are an exact, locale-independent signal. A [`CoreError::Tool`]
    /// carries only the subprocess's own free-form failure detail, which cooldown cannot introspect
    /// structurally, so that case falls back to a best-effort match against well-known
    /// broken-environment phrases — necessarily incomplete and locale-sensitive, but the only signal
    /// a subprocess exposes. A plain non-zero exit whose detail does not name a broken environment
    /// stays a resolver conflict, so a single unfetchable/conflicting candidate is still isolated.
    #[must_use]
    pub fn is_local_environment_failure(&self) -> bool {
        match self {
            CoreError::ToolSpawn { .. }
            | CoreError::Filesystem(_)
            | CoreError::LockUnreadable(_)
            | CoreError::PathEncoding(_)
            | CoreError::Serialization(_)
            | CoreError::System(_)
            | CoreError::LockConflict(_) => true,
            CoreError::Tool { stderr, .. } => detail_indicates_broken_environment(stderr),
            _ => false,
        }
    }

    /// Convenience constructor for a transient cause from any error type.
    pub fn transient(e: impl Into<BoxError>) -> Self {
        CoreError::Transient(e.into())
    }

    /// The structured kind for the JSON `Diagnostic.kind` field. Each error class maps to exactly
    /// one kind, so a consumer never has to disambiguate by message string.
    #[must_use]
    pub fn diagnostic_kind(&self) -> DiagnosticKind {
        match self {
            CoreError::NotFound(_) => DiagnosticKind::NotFound,
            CoreError::Transient(_) | CoreError::OfflineMiss(_) => DiagnosticKind::Transient,
            CoreError::Tool { .. } => DiagnosticKind::ToolFailed,
            CoreError::ToolSpawn { .. } => DiagnosticKind::ToolSpawnFailed,
            CoreError::Config(_) => DiagnosticKind::Config,
            CoreError::Parse(_) => DiagnosticKind::Parse,
            CoreError::LockUnreadable(_) => DiagnosticKind::LockfileUnreadable,
            CoreError::StaleLock(_) => DiagnosticKind::StaleLock,
            CoreError::Filesystem(_) => DiagnosticKind::Filesystem,
            CoreError::PathEncoding(_) => DiagnosticKind::PathEncoding,
            CoreError::Serialization(_) => DiagnosticKind::Serialization,
            CoreError::LockConflict(_) => DiagnosticKind::LockConflict,
            CoreError::System(_) => DiagnosticKind::System,
        }
    }
}

/// Best-effort match of a spawned tool's failure detail against well-known broken-environment
/// phrases, for the [`CoreError::Tool`] case where no structured signal is available. Incomplete and
/// locale-sensitive by nature — a subprocess exposes only free-form text, and localized OS messages
/// will not match; the structured [`CoreError`] variants are the reliable signal, this is the
/// fallback for failures that only surface as a non-zero tool exit. `"permission denied"`
/// deliberately also matches auth failures (git's `Permission denied (publickey)`, registries that
/// phrase a 403 that way): missing credentials are an environment fault, and aborting with the real
/// message beats misreporting the candidate as a resolver conflict.
fn detail_indicates_broken_environment(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "unable to open database file",
        "read-only file system",
        "permission denied",
        "no space left on device",
        "disk quota exceeded",
        "database is locked",
        "eacces",
        "enospc",
        "erofs",
        "sqlite_cantopen",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        CoreError::Filesystem(e.to_string())
    }
}

/// A structured diagnostic surfaced in the JSON envelope's `warnings`/`errors` arrays and on the
/// TTY. `kind` and `message` are always set; the rest are populated when applicable so a consumer
/// can map a diagnostic back to a baseline key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    /// The structured class of the diagnostic. Always set.
    pub kind: DiagnosticKind,
    /// The human-readable description shown on the TTY and serialized in the
    /// JSON envelope. Always set.
    pub message: String,
    /// The tool the diagnostic originates from (for example `go`, `cargo`, or
    /// `uv`) — both the ecosystem it is package-specific to and the external
    /// binary it stems from, which are one and the same.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The project (workspace member or manifest) the diagnostic applies to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The package or module name the diagnostic applies to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// The package version the diagnostic applies to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The upstream registry consulted (for example a crates.io or `PyPI`
    /// endpoint), when relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// The filesystem path involved (for example a lockfile or manifest), when
    /// relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// The closed set of diagnostic kinds. Part of the JSON contract (`schemaVersion` bumps on change).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    /// A transient failure (network blip, 5xx, 429, or an offline cache miss)
    /// that a retry might resolve.
    Transient,
    /// The requested package, version, or module was not found upstream.
    NotFound,
    /// The release age of a version could not be determined, so the cooldown
    /// policy could not be evaluated for it.
    UnknownAge,
    /// A native tool constraint is stricter than the configured cooldown,
    /// so the tool's own rule governs instead.
    StricterNative,
    /// The version under consideration has been yanked upstream.
    Yanked,
    /// A lockfile or manifest is stale relative to its source, or absent.
    StaleLock,
    /// Lock currency could not be determined by the adapter.
    LockUnknown,
    /// An external tool (`go`, `cargo`, `uv`) exited non-zero after being spawned.
    ToolFailed,
    /// An external tool (`go`, `cargo`, `uv`) could not be spawned at all.
    ToolSpawnFailed,
    /// A local lockfile/manifest or resolved-graph dump could not be read.
    LockfileUnreadable,
    /// A local filesystem operation failed.
    Filesystem,
    /// A local path could not be represented in cooldown's UTF-8/path model.
    PathEncoding,
    /// A local JSON/TOML serialization step failed.
    Serialization,
    /// Another cooldown process already holds the project mutation lock.
    LockConflict,
    /// A non-transient local runtime/environment setup step failed.
    System,
    /// Invalid configuration or command input (the user must fix it).
    Config,
    /// An upstream registry payload could not be parsed.
    Parse,
    /// A cooldown violation `fix` left in place: an exact pin (without `--downgrade-pinned`), or a
    /// dependency with no older version matured enough to downgrade to. Informational — the user
    /// chooses how to act (manual downgrade, `--downgrade-pinned`, `baseline`, or wait).
    Held,
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
            DiagnosticKind::LockUnknown => "lock_unknown",
            DiagnosticKind::ToolFailed => "tool_failed",
            DiagnosticKind::ToolSpawnFailed => "tool_spawn_failed",
            DiagnosticKind::LockfileUnreadable => "lockfile_unreadable",
            DiagnosticKind::Filesystem => "filesystem",
            DiagnosticKind::PathEncoding => "path_encoding",
            DiagnosticKind::Serialization => "serialization",
            DiagnosticKind::LockConflict => "lock_conflict",
            DiagnosticKind::System => "system",
            DiagnosticKind::Config => "config",
            DiagnosticKind::Parse => "parse",
            DiagnosticKind::Held => "held",
        };
        f.write_str(s)
    }
}

impl Diagnostic {
    /// Starts a diagnostic with only the required [`kind`](Diagnostic::kind)
    /// and [`message`](Diagnostic::message) fields.
    ///
    /// This is the entry point of the builder chain: every optional field starts
    /// as `None` and is filled in by chaining the `with_*` setters such as
    /// [`Diagnostic::with_package`] or [`Diagnostic::with_version`].
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::{Diagnostic, DiagnosticKind};
    ///
    /// let diag = Diagnostic::new(DiagnosticKind::Yanked, "version was yanked")
    ///     .with_tool("cargo")
    ///     .with_package("serde")
    ///     .with_version("1.0.0");
    ///
    /// assert_eq!(diag.kind, DiagnosticKind::Yanked);
    /// assert_eq!(diag.package.as_deref(), Some("serde"));
    /// ```
    pub fn new(kind: DiagnosticKind, message: impl Into<String>) -> Self {
        Diagnostic {
            kind,
            message: message.into(),
            tool: None,
            project: None,
            package: None,
            version: None,
            registry: None,
            path: None,
        }
    }

    /// Sets the [`tool`](Diagnostic::tool) field and returns `self` for chaining.
    #[must_use]
    pub fn with_tool(mut self, e: impl Into<String>) -> Self {
        self.tool = Some(e.into());
        self
    }
    /// Sets the [`project`](Diagnostic::project) field and returns `self` for chaining.
    #[must_use]
    pub fn with_project(mut self, p: impl Into<String>) -> Self {
        self.project = Some(p.into());
        self
    }
    /// Sets the [`package`](Diagnostic::package) field and returns `self` for chaining.
    #[must_use]
    pub fn with_package(mut self, p: impl Into<String>) -> Self {
        self.package = Some(p.into());
        self
    }
    /// Sets the [`version`](Diagnostic::version) field and returns `self` for chaining.
    #[must_use]
    pub fn with_version(mut self, v: impl Into<String>) -> Self {
        self.version = Some(v.into());
        self
    }
    /// Sets the [`registry`](Diagnostic::registry) field and returns `self` for chaining.
    #[must_use]
    pub fn with_registry(mut self, r: impl Into<String>) -> Self {
        self.registry = Some(r.into());
        self
    }
    /// Sets the [`path`](Diagnostic::path) field and returns `self` for chaining.
    #[must_use]
    pub fn with_path(mut self, p: impl Into<String>) -> Self {
        self.path = Some(p.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_exit_and_spawn_map_to_distinct_diagnostic_kinds() {
        let exited = CoreError::Tool {
            tool: "cargo".into(),
            termination: ToolTermination::ExitCode(101),
            stderr: "failed".into(),
        };
        let spawn = CoreError::ToolSpawn {
            tool: "cargo".into(),
            detail: "spawn failed".into(),
        };

        assert_eq!(exited.diagnostic_kind(), DiagnosticKind::ToolFailed);
        assert_eq!(spawn.diagnostic_kind(), DiagnosticKind::ToolSpawnFailed);
        assert_eq!(DiagnosticKind::ToolFailed.to_string(), "tool_failed");
        assert_eq!(
            DiagnosticKind::ToolSpawnFailed.to_string(),
            "tool_spawn_failed"
        );
    }

    #[test]
    fn local_environment_failures_are_classified_separately() {
        let sqlite = CoreError::Tool {
            tool: "pnpm".into(),
            termination: ToolTermination::ExitCode(1),
            stderr: "pnpm: unable to open database file".into(),
        };
        let readonly = CoreError::Tool {
            tool: "pnpm".into(),
            termination: ToolTermination::ExitCode(1),
            stderr: "Read-only file system (os error 30)".into(),
        };
        let disk_full = CoreError::Tool {
            tool: "cargo".into(),
            termination: ToolTermination::ExitCode(101),
            stderr: "error: failed to write Cargo.lock: No space left on device".into(),
        };
        // Auth failures are deliberately environmental: credentials are machine state, not a
        // property of the candidate set.
        let git_auth = CoreError::Tool {
            tool: "cargo".into(),
            termination: ToolTermination::ExitCode(101),
            stderr: "git@github.com: Permission denied (publickey).".into(),
        };
        let resolver = CoreError::Tool {
            tool: "pnpm".into(),
            termination: ToolTermination::ExitCode(1),
            stderr: "No matching version found for colors@999.0.0".into(),
        };

        // Structured local faults are an exact, locale-independent signal — no string match needed.
        assert!(
            CoreError::ToolSpawn {
                tool: "pnpm".into(),
                detail: "binary missing".into(),
            }
            .is_local_environment_failure()
        );
        assert!(CoreError::Filesystem("disk full".into()).is_local_environment_failure());
        assert!(CoreError::LockUnreadable("corrupt lock".into()).is_local_environment_failure());
        // A spawned tool whose only signal is stderr text is matched best-effort.
        assert!(sqlite.is_local_environment_failure());
        assert!(readonly.is_local_environment_failure());
        assert!(disk_full.is_local_environment_failure());
        assert!(git_auth.is_local_environment_failure());
        // A genuine resolver rejection and a plain not-found stay isolatable (bisected, not fatal).
        assert!(!resolver.is_local_environment_failure());
        assert!(!CoreError::StaleLock("lock is stale".into()).is_local_environment_failure());
        assert!(!CoreError::NotFound("colors@999.0.0".into()).is_local_environment_failure());
    }

    #[test]
    fn local_failures_map_to_distinct_diagnostic_kinds() {
        assert_eq!(
            CoreError::Filesystem("disk full".into()).diagnostic_kind(),
            DiagnosticKind::Filesystem
        );
        assert_eq!(
            CoreError::PathEncoding("bad path".into()).diagnostic_kind(),
            DiagnosticKind::PathEncoding
        );
        assert_eq!(
            CoreError::Serialization("bad json".into()).diagnostic_kind(),
            DiagnosticKind::Serialization
        );
        assert_eq!(
            CoreError::LockConflict("locked".into()).diagnostic_kind(),
            DiagnosticKind::LockConflict
        );
        assert_eq!(
            CoreError::System("bad env".into()).diagnostic_kind(),
            DiagnosticKind::System
        );
    }
}
