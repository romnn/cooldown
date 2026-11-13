//! Typed diagnostic observations emitted independently of console logging.
//!
//! The application subscribes to [`TARGET`] and owns persistence and run timestamps.
//! Without a subscriber these observations do no serialization or filesystem work.

use crate::{Dependency, FixVerdict, PinVerdict, Release, ResolveContext, Verdict};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

/// The dedicated tracing target for structured run evidence.
pub const TARGET: &str = "cooldown_run_trace";

/// Whether a recorder is listening in the current execution context.
#[must_use]
pub fn enabled() -> bool {
    tracing::enabled!(target: TARGET, tracing::Level::TRACE)
}

/// Emits one typed observation when a recorder is present.
pub fn emit(event: &Event<'_>) {
    if !enabled() {
        return;
    }
    match serde_json::to_string(event) {
        Ok(payload) => {
            let artifact = match event {
                Event::ArtifactStarted { artifact, .. }
                | Event::ArtifactChunk { artifact, .. }
                | Event::ArtifactFinished { artifact, .. } => Some(*artifact),
                _ => None,
            };
            let input_path = match event {
                Event::ArtifactStarted {
                    source: ArtifactSource::File { path, .. },
                    ..
                } => Some(*path),
                _ => None,
            };
            let sha256 = match event {
                Event::ArtifactStarted { sha256, .. } => *sha256,
                _ => None,
            };
            tracing::trace!(target: TARGET, trace_event = payload.as_str(), trace_artifact = artifact, trace_input_path = input_path, trace_artifact_sha256 = sha256, trace_artifact_finished = matches!(event, Event::ArtifactFinished { .. }));
        }
        Err(err) => {
            tracing::error!(target: TARGET, error = %err, "trace event serialization failed");
        }
    }
}

/// A process-local operation identity used to pair starts and finishes across concurrent work.
#[must_use]
pub fn operation_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The role of a native input in an adapter's evidence declaration.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputRole {
    /// Application or workspace dependency declarations.
    Manifest,
    /// A resolved lock or native graph document.
    Graph,
    /// Native resolver or cooldown configuration.
    Configuration,
    /// A local reference, patch, executable or other resolver input.
    ResolverInput,
}

/// One exact native input, declared by the adapter that reads it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceInput {
    /// The absolute native path, including external workspace references.
    pub path: camino::Utf8PathBuf,
    /// Why this input participates in resolution.
    pub role: InputRole,
}

/// What a native graph document can establish if its capture succeeds.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GraphCoverage {
    /// The document records resolved identities and their dependency bindings.
    Resolved,
    /// The document records pins but omits some or all dependency bindings.
    Inventory {
        /// The limitation of this native format.
        reason: String,
    },
}

/// A native graph source bound to an exact declared artifact, not a filename pattern.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphSource {
    /// The exact input path whose successful capture establishes this evidence.
    pub path: camino::Utf8PathBuf,
    /// The adapter's native format identifier.
    pub format: String,
    /// The fidelity of the original document, without a second graph parser.
    pub coverage: GraphCoverage,
}

/// Files and native graph evidence a read-side adapter can identify without resolving again.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    /// Exact inputs, including paths expected to be absent.
    pub inputs: Vec<EvidenceInput>,
    /// Native graph references, available only when their artifacts capture successfully.
    pub graphs: Vec<GraphSource>,
    /// Missing or declined input coverage, never silently represented as complete.
    pub limitations: Vec<String>,
    /// Ambient native configuration variables present during this snapshot.
    pub environment: Vec<EnvironmentOverride>,
}

/// A version-only invocation for the adapter's configured native executable.
#[derive(Debug, Clone)]
pub struct ToolVersionCommand {
    /// The executable selected by the adapter, including explicit overrides.
    pub program: String,
    /// Arguments that report the version without resolving a project.
    pub arguments: Vec<String>,
}

/// Which state of a project a leased snapshot describes.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotPhase {
    /// The initial observed state before the run changes any project.
    Initial,
    /// The state protected by the same lease as the associated mutation or refresh.
    BeforeMutation,
    /// Inputs read by an intermediate or staged operation.
    Observed,
    /// The final state while the operation still owns the project.
    Final,
}

/// The origin of one incrementally recorded artifact.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ArtifactSource<'a> {
    /// One exact adapter-declared input.
    File {
        /// The absolute native path.
        path: &'a str,
        /// The adapter's input classification.
        role: InputRole,
    },
    /// A registry POST request body, before it is sent or served from cache.
    HttpRequest {
        /// The endpoint receiving this body.
        url: &'a str,
    },
    /// The command's exact rendered standard output, including workspace-free results.
    Output,
    /// The exact body returned by the registry client, including cache results.
    Http {
        /// The requested URL or POST cache key.
        key: &'a str,
        /// HTTP status of the delivered document.
        status: u16,
        /// Whether the body came from the persistent HTTP cache.
        from_cache: bool,
        /// Whether the delivered cached body was stale.
        stale: bool,
    },
    /// A native process's complete standard output.
    Stdout {
        /// The matching command operation.
        operation: u64,
    },
    /// A native process's complete standard error.
    Stderr {
        /// The matching command operation.
        operation: u64,
    },
}

/// A bounded borrowed artifact chunk, retained without lossy text conversion.
#[derive(Serialize)]
#[serde(tag = "encoding", content = "data", rename_all = "snake_case")]
pub enum Content<'a> {
    /// A complete UTF-8 chunk.
    Utf8(&'a str),
    /// Original bytes when the chunk is not valid UTF-8.
    Bytes(&'a [u8]),
}

impl<'a> Content<'a> {
    /// Borrows the original bytes, using text only when it is lossless.
    #[must_use]
    pub fn from_bytes(bytes: &'a [u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(text) => Self::Utf8(text),
            Err(_) => Self::Bytes(bytes),
        }
    }
}

/// The terminal outcome of an artifact capture.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArtifactOutcome<'a> {
    /// All bytes were captured.
    Complete,
    /// The declared file did not exist.
    Missing,
    /// The file could not be opened or read.
    Failed {
        /// The actual I/O or inventory error.
        reason: &'a str,
    },
    /// The file changed while it was read, so it cannot establish a coherent input.
    Changed,
}

/// One environment override explicitly supplied to a native command.
#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentOverride {
    /// The variable name.
    pub name: String,
    /// The supplied value, or `None` when the command removes the variable.
    pub value: Option<String>,
}

/// The actual candidate ceilings enabled for an evaluation or probe.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateFilters {
    /// Whether the declared upper bound applies.
    pub declared_bound: bool,
    /// Whether the configured maximum major applies.
    pub max_major: bool,
    /// Whether the latest registry tag applies.
    pub latest_tag: bool,
}

/// A reason an available release was excluded before cooldown evaluation.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseFilter {
    /// The release is not newer than the locked version.
    CurrentOrOlder,
    /// The release's quality is incompatible with the current dependency.
    Quality,
    /// A cross-major move is outside this dependency's scope.
    Major,
    /// The release was yanked or withdrawn.
    Yanked,
    /// The release had not been published at the evaluation instant.
    NotPublished,
    /// An active third-party exact requirement caps the graph.
    GraphCeiling,
    /// A configured maximum major excludes the release.
    MaxMajor,
    /// The application's explicit declared bound excludes the release.
    DeclaredBound,
    /// The registry's latest tag excludes the release.
    DistTag,
    /// The adapter cannot classify the movement.
    Unclassified,
}

/// An observation at the boundary where a decision or external effect occurs.
#[derive(Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Event<'a> {
    /// The actual process invocation and host, before workspace preparation.
    RunStarted {
        /// The invoked subcommand.
        command: &'a str,
        /// The cooldown version producing the trace.
        version: &'a str,
        /// The host operating system.
        os: &'a str,
        /// The host architecture.
        arch: &'a str,
        /// The process's original working directory.
        directory: &'a str,
    },
    /// Invocation details follow the compact run header so metadata remains cheap to inspect.
    Invocation {
        /// The original argument vector.
        arguments: &'a [String],
        /// Process-boundary configuration and cooldown environment overrides.
        environment: &'a [EnvironmentOverride],
        /// The absolute project selection, including an explicit directory option.
        selected_directory: &'a str,
    },
    /// The repository boundary chosen by workspace discovery.
    Workspace {
        /// The discovered repository root.
        repository: &'a str,
    },
    /// A configuration file actually loaded while preparing the run.
    Configuration {
        /// The absolute file path.
        path: &'a str,
        /// The exact source before parsing or any sync operation.
        content: &'a str,
    },
    /// The discovered project and its actual effective policy/evaluation clock.
    Project {
        /// The absolute project directory.
        project: &'a str,
        /// The selected ecosystem adapter.
        tool: &'a str,
        /// The immutable policy evaluation instant, independent of wall-clock event times.
        evaluation_at: jiff::Timestamp,
        /// The effective policy as a typed configuration report.
        policy: &'a crate::PolicyStack,
    },
    /// A leased snapshot declares its exact input closure before acquisition starts.
    SnapshotStarted {
        /// The identity shared by its artifacts and terminal record.
        snapshot: u64,
        /// The original or staged project directory.
        project: &'a str,
        /// The owning ecosystem adapter.
        tool: &'a str,
        /// Which protected project state is represented.
        phase: SnapshotPhase,
        /// The adapter-owned native evidence declaration.
        evidence: &'a Evidence,
    },
    /// The snapshot's input enumeration and acquisition has finished.
    SnapshotFinished {
        /// The matching snapshot identity.
        snapshot: u64,
    },
    /// An artifact starts before any bytes are read or copied.
    ArtifactStarted {
        /// Its unique identity within the run.
        artifact: u64,
        /// Its snapshot, or none for process streams.
        snapshot: Option<u64>,
        /// The input or process stream this artifact represents.
        source: ArtifactSource<'a>,
        /// Expected byte length when known.
        size: Option<u64>,
        /// The lowercase hex SHA-256 of the complete content, when it is known before the first
        /// chunk.
        ///
        /// A recorder that has already written identical content in the same run may record this
        /// artifact as a reference to that copy instead of repeating its chunks.
        #[serde(skip_serializing_if = "Option::is_none")]
        sha256: Option<&'a str>,
    },
    /// One bounded artifact chunk, available even if later capture fails.
    ArtifactChunk {
        /// The matching artifact identity.
        artifact: u64,
        /// The byte offset in the original artifact.
        offset: u64,
        /// Original text or bytes, without a whole-artifact buffer.
        content: Content<'a>,
    },
    /// An artifact's terminal state; only complete captures establish available graph evidence.
    ArtifactFinished {
        /// The matching artifact identity.
        artifact: u64,
        /// The number of original bytes recorded.
        bytes: u64,
        /// Whether all source bytes were captured coherently.
        outcome: ArtifactOutcome<'a>,
    },
    /// The unscoped dependencies the adapter actually returned to the orchestrator.
    Dependencies {
        /// The original or staged project root.
        project: &'a str,
        /// The ecosystem adapter.
        tool: &'a str,
        /// The full adapter view before package/member filtering.
        dependencies: &'a [Dependency],
    },
    /// The result of a native version-only query.
    ToolVersion {
        /// The project directory used to resolve toolchain/config overrides.
        project: &'a str,
        /// The configured executable path.
        program: &'a str,
        /// The query's output, or an explanation of why no version was obtained.
        result: &'a serde_json::Value,
    },
    /// A native command starts, before spawn can fail or the child can hang.
    CommandStarted {
        /// The identity shared with its terminal event.
        operation: u64,
        /// The configured tool name.
        tool: &'a str,
        /// The executable actually passed to the operating system.
        program: &'a str,
        /// The exact argument vector.
        arguments: &'a [String],
        /// The command's working directory.
        directory: Option<&'a str>,
        /// Explicit command environment overrides, excluding inherited ambient variables.
        environment: &'a [EnvironmentOverride],
    },
    /// A native command ends, fails to spawn, or is interrupted.
    CommandFinished {
        /// The matching start identity.
        operation: u64,
        /// Elapsed monotonic microseconds.
        duration_us: u64,
        /// The process exit code, absent on a signal or spawn failure.
        exit_code: Option<i32>,
        /// The process's actual code or signal, absent on a spawn/wait failure.
        termination: Option<crate::ToolTermination>,
        /// A spawn/wait/interruption failure, when present.
        error: Option<&'a str>,
    },
    /// An actual core policy evaluation and its full decision inputs/output.
    Evaluation {
        /// The project identity used by policy selectors.
        project: &'a str,
        /// The dependency being judged, including graph and declared constraints.
        dependency: &'a Dependency,
        /// All available classified release evidence.
        releases: &'a [Release],
        /// The actual major/bound/tag context used to evaluate the release set.
        context: &'a ResolveContext<'a>,
        /// The fixed evaluation clock used by policy.
        evaluation_at: jiff::Timestamp,
        /// The actual typed core verdict.
        decision: Decision<'a>,
        /// Actual candidate ceiling mask; absent for locked-pin and fix evaluations.
        filters: Option<CandidateFilters>,
    },
    /// The field-by-field derivation of a policy window.
    PolicyResolved {
        /// The exact selector query.
        query: &'a crate::ResolveQuery<'a>,
        /// The fixed policy evaluation clock.
        evaluation_at: jiff::Timestamp,
        /// The selected window and every contributing or discarded rule.
        resolution: &'a crate::Resolution,
    },
    /// A release was rejected by an actual candidate filter.
    ReleaseFiltered {
        /// The project identity used by the decision.
        project: &'a str,
        /// The package being evaluated.
        dependency: &'a Dependency,
        /// The rejected release.
        release: &'a Release,
        /// The filter that rejected it.
        reason: ReleaseFilter,
    },
    /// A typed state transition in a resolver attempt.
    Mutation {
        /// The original or staged project directory.
        project: &'a str,
        /// The closed transition and its actual decision data.
        transition: Mutation<'a>,
    },
    /// The same structured result printed by the command, including all diagnostics.
    Report {
        /// The complete report envelope.
        report: &'a serde_json::Value,
    },
    /// A terminal run record; its absence identifies an incomplete recording.
    RunFinished {
        /// The process's actual exit status.
        exit_code: u8,
        /// Whether cooperative interruption was requested.
        interrupted: bool,
        /// A preparation or dispatch error, absent for ordinary report outcomes.
        error: Option<&'a str>,
    },
}

impl Event<'_> {
    /// Emits this observation to the current run's recorder, when present.
    pub fn emit(&self) {
        emit(self);
    }
}

/// Converts an operation duration to a portable JSON integer without truncating it.
#[must_use]
pub fn duration_us(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// The actual result of one kind of policy evaluation.
#[derive(Serialize)]
#[serde(tag = "kind", content = "verdict", rename_all = "snake_case")]
pub enum Decision<'a> {
    /// A forward candidate-set evaluation.
    Upgrade(&'a Verdict),
    /// A locked-release age gate.
    Pin(&'a PinVerdict),
    /// A downgrade/remediation evaluation.
    Fix(&'a FixVerdict),
}

/// Closed resolver/mutation transitions, correlated through their enclosing operation scope.
#[derive(Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Mutation<'a> {
    /// A logical trial starts with its actual planned moves.
    TrialStarted {
        /// The moves tried together.
        changes: &'a [crate::Change],
    },
    /// The native resolver returned success; outer graph verification may still roll this trial back.
    Accepted {
        /// The moves the native resolver reports as applied.
        changes: &'a [crate::Change],
    },
    /// A trial failed acceptance or native resolution.
    Rejected {
        /// The trial's exact rejection reason.
        reason: &'a str,
    },
    /// An older or alternative target is selected for a retry.
    Fallback {
        /// The actual alternative movement.
        change: &'a crate::Change,
    },
    /// Restoration of a protected baseline starts.
    RollbackStarted,
    /// Restoration completed successfully.
    RolledBack,
    /// Source drift or another failure prevented restoration.
    RollbackFailed {
        /// The actual restoration failure.
        reason: &'a str,
    },
}

/// Emits already-owned bytes as bounded artifact records without cloning the stream.
pub fn artifact(source: ArtifactSource<'_>, bytes: &[u8]) {
    if !enabled() {
        return;
    }
    let artifact = operation_id();
    let sha256 = hex_sha256(bytes);
    Event::ArtifactStarted {
        artifact,
        snapshot: None,
        source,
        size: Some(bytes.len() as u64),
        sha256: Some(&sha256),
    }
    .emit();
    let mut offset = 0;
    for chunk in bytes.chunks(64 * 1024) {
        Event::ArtifactChunk {
            artifact,
            offset,
            content: Content::from_bytes(chunk),
        }
        .emit();
        offset += chunk.len() as u64;
    }
    Event::ArtifactFinished {
        artifact,
        bytes: offset,
        outcome: ArtifactOutcome::Complete,
    }
    .emit();
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(bytes)
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .filter_map(|nibble| char::from_digit(u32::from(nibble), 16))
        .collect()
}
