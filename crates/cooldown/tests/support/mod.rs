//! Shared, tool-agnostic harness for the temp-dir convergence integration tests.
//!
//! Each test creates a fresh temp dir, writes a minimal project fixture on the fly (a fresh temp
//! dir is the same deterministic starting state every run — no checked-in repos, no stale-lock
//! drift), seeds a starting lock with the ecosystem's own package manager, runs the real
//! `cooldown` binary against it, and parses the `--json` envelope.
//!
//! The fixtures pin the resolution clock to a fixed instant via the cooldown `--freeze <DATE>`
//! cutoff (an absolute exclude-newer), so the underlying resolver replays the registry's publish
//! history as of that instant — append-mostly in practice; an unpublish/yank of a fixture dep is
//! one live mutation that could shift it, and the npm family's mutable `latest` dist-tag is the
//! other, which frozen suites decouple via [`Fixture::tag_independent`]. Tests assert invariants
//! (convergence, no-silent-change, cross-command agreement), never hard-coded versions.
//!
//! Everything here is ecosystem-agnostic. [`Fixture`] only knows how to write files, run an
//! arbitrary tool (so each ecosystem can seed its own lock — `uv lock`, `cargo generate-lockfile`,
//! `go mod tidy`, `pnpm install --lockfile-only`, …), drive `cooldown`, and parse the returned
//! [`Envelope`]. The lock-diff helper [`changed_packages`] is parameterized by a [`PinParser`] so
//! each lock format plugs in its own pin extraction; [`toml_lock_pins`] covers the TOML
//! `[[package]]` shape shared by `uv.lock` and `Cargo.lock`. Adding a new ecosystem is "write a
//! fixture generator + a pin parser, then reuse the same invariant assertions on the returned
//! [`Envelope`]", not a rewrite.

#![allow(
    dead_code,
    reason = "the harness is shared across per-ecosystem integration test files; not every helper is used by every file, and not every ecosystem is covered yet"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use color_eyre::eyre;
use cooldown_adapter_util::{program_on_path, resolve_program};

const TOOL_ATTEMPTS: usize = 3;

/// A throwaway project tree under a temp dir, plus the means to run `cooldown` against it.
pub struct Fixture {
    dir: tempfile::TempDir,
    tag_independent: bool,
}

impl Fixture {
    /// Create an empty fixture rooted at a fresh temp dir.
    pub fn new() -> Self {
        let fixture = Self::new_non_git();
        // Anchor discovery when an ambient ancestor belongs to another Git repository.
        std::fs::create_dir(fixture.dir.path().join(".git")).expect("create fixture Git anchor");
        // The valid Git metadata gives mutation fixtures an external recovery trust domain.
        std::fs::write(
            fixture.dir.path().join(".git/HEAD"),
            "ref: refs/heads/main\n",
        )
        .expect("write fixture Git HEAD");
        fixture
    }

    /// Create an empty fixture WITHOUT the Git anchor: a plain non-Git project directory, so
    /// cooldown's coordination namespace falls back to the repo-root `.cooldown/locks` directory
    /// instead of a Git common directory. This relies on the ambient temp dir having no Git
    /// ancestor of its own (true for standard system temp dirs) — with one, discovery would anchor
    /// there, which is exactly what [`Fixture::new`]'s stamp defends against. Tests that model a
    /// Git checkout (or run `git init` themselves) keep using [`Fixture::new`].
    pub fn new_non_git() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("cooldown-it-")
            .tempdir()
            .expect("create temp dir");
        Self {
            dir,
            tag_independent: false,
        }
    }

    /// Decouple every `cooldown` run from the registry's mutable `latest` dist-tag by passing
    /// `--no-respect-dist-tags`. The frozen npm-family suites opt in: their publish-history replay
    /// is stable, but the tag is live state a maintainer can move at any time, and only the
    /// dedicated dist-tag probe should couple to it.
    pub fn tag_independent(mut self) -> Self {
        self.tag_independent = true;
        self
    }

    /// The project root.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Write a file (creating parent dirs) relative to the project root.
    pub fn write(&self, rel: &str, contents: &str) -> &Self {
        let path = self.dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&path, contents).expect("write fixture file");
        self
    }

    /// Read a project-root-relative file to bytes (for byte-identical lock comparisons).
    pub fn read_bytes(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.dir.path().join(rel)).expect("read fixture file")
    }

    /// Run a raw command in the project root, returning its captured output. Used to drive the real
    /// package manager when seeding a starting lock (e.g. `uv lock --exclude-newer …`).
    pub fn run_tool(&self, program: &str, args: &[&str], envs: &[(&str, &str)]) -> CapturedOutput {
        self.run_tool_result(program, args, envs, None)
            .unwrap_or_else(|error| panic!("spawn {program}: {error}"))
    }

    fn run_tool_result(
        &self,
        program: &str,
        args: &[&str],
        envs: &[(&str, &str)],
        phase: Option<&str>,
    ) -> eyre::Result<CapturedOutput> {
        let executable = resolve_program(program);
        let mut attempt = 1;
        loop {
            let mut command = Command::new(&executable);
            command.current_dir(self.dir.path()).args(args);
            for (key, value) in envs {
                command.env(key, value);
            }
            let output = match phase {
                Some(phase) => traced_output(&mut command, phase)?,
                None => command.output()?,
            };
            let captured = CapturedOutput::from(program, args, output);
            if captured.status.success()
                || !captured.is_transient_tool_failure()
                || attempt == TOOL_ATTEMPTS
            {
                return Ok(captured);
            }
            eprintln!(
                "retrying transient tool failure ({}/{TOOL_ATTEMPTS}): {}",
                attempt + 1,
                captured.label
            );
            std::thread::sleep(Duration::from_millis(
                500 * u64::try_from(attempt).unwrap_or(1),
            ));
            attempt += 1;
        }
    }

    /// Runs a raw command while identifying its phase and retaining its captured diagnostics.
    pub fn run_tool_traced(
        &self,
        phase: &str,
        program: &str,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> eyre::Result<CapturedOutput> {
        eprintln!(
            "starting integration phase `{phase}`: {program} {}",
            args.join(" ")
        );
        let started = Instant::now();
        let captured = self.run_tool_result(program, args, envs, Some(phase))?;
        trace_captured_phase(phase, started, &captured);
        Ok(captured)
    }

    /// Run the built `cooldown` binary against the fixture with the given args, capturing output.
    /// `CARGO_BIN_EXE_cooldown` is injected by Cargo for integration tests.
    pub fn cooldown(&self, args: &[&str]) -> CapturedOutput {
        let exe = env!("CARGO_BIN_EXE_cooldown");
        let output = Command::new(exe)
            .current_dir(self.dir.path())
            .args(args)
            .args(self.tag_independent.then_some("--no-respect-dist-tags"))
            // Pin tool/dir explicitly so detection is deterministic regardless of ambient state.
            .arg("--dir")
            .arg(self.dir.path())
            .output()
            .expect("spawn cooldown binary");
        CapturedOutput::from("cooldown", args, output)
    }

    /// Runs cooldown while identifying its phase and retaining its captured diagnostics.
    pub fn cooldown_traced(&self, phase: &str, args: &[&str]) -> eyre::Result<CapturedOutput> {
        eprintln!(
            "starting integration phase `{phase}`: cooldown {}",
            args.join(" ")
        );
        let started = Instant::now();
        let exe = env!("CARGO_BIN_EXE_cooldown");
        let mut command = Command::new(exe);
        command
            .current_dir(self.dir.path())
            .args(args)
            .args(self.tag_independent.then_some("--no-respect-dist-tags"))
            .arg("--dir")
            .arg(self.dir.path());
        let captured = CapturedOutput::from("cooldown", args, traced_output(&mut command, phase)?);
        trace_captured_phase(phase, started, &captured);
        Ok(captured)
    }

    /// Run a `cooldown` subcommand with `--json` and parse the envelope. Panics with the captured
    /// stderr if the binary did not emit valid JSON, so a resolver/setup failure is legible.
    pub fn cooldown_json(&self, args: &[&str]) -> Envelope {
        let mut full: Vec<&str> = args.to_vec();
        full.push("--json");
        let captured = self.cooldown(&full);
        let value: serde_json::Value =
            serde_json::from_slice(&captured.stdout).unwrap_or_else(|err| {
                panic!(
                    "cooldown {args:?} did not emit JSON: {err}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    captured.stdout_str(),
                    captured.stderr_str(),
                )
            });
        Envelope { value }
    }

    /// Runs a JSON command while identifying its phase and retaining its captured diagnostics.
    pub fn cooldown_json_traced(&self, phase: &str, args: &[&str]) -> eyre::Result<Envelope> {
        let mut full: Vec<&str> = args.to_vec();
        full.push("--json");
        let captured = self.cooldown_traced(phase, &full)?;
        let value: serde_json::Value = serde_json::from_slice(&captured.stdout).map_err(|error| {
            eyre::eyre!(
                "cooldown {args:?} did not emit JSON: {error}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                captured.stdout_str(),
                captured.stderr_str(),
            )
        })?;
        Ok(Envelope { value })
    }
}

fn traced_output(command: &mut Command, phase: &str) -> eyre::Result<std::process::Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre::eyre!("traced command did not expose stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| eyre::eyre!("traced command did not expose stderr"))?;
    let (status, stdout, stderr) = std::thread::scope(|scope| -> eyre::Result<_> {
        let stdout = scope.spawn(|| capture_traced_stream(stdout, phase, "stdout"));
        let stderr = scope.spawn(|| capture_traced_stream(stderr, phase, "stderr"));
        let status = child.wait()?;
        let stdout = stdout
            .join()
            .map_err(|_| eyre::eyre!("traced stdout reader panicked"))??;
        let stderr = stderr
            .join()
            .map_err(|_| eyre::eyre!("traced stderr reader panicked"))??;
        Ok((status, stdout, stderr))
    })?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn capture_traced_stream(
    mut stream: impl std::io::Read,
    phase: &str,
    name: &str,
) -> std::io::Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Ok(captured);
        }
        let chunk = buffer
            .get(..read)
            .ok_or_else(|| std::io::Error::other("stream read exceeded its buffer"))?;
        captured.extend_from_slice(chunk);
        eprintln!("[{phase} {name}] {}", String::from_utf8_lossy(chunk));
    }
}

fn trace_captured_phase(phase: &str, started: Instant, captured: &CapturedOutput) {
    eprintln!(
        "completed integration phase `{phase}` in {:?}: status={:?}",
        started.elapsed(),
        captured.status.code(),
    );
}

/// Captured stdout/stderr/status of a subprocess run.
pub struct CapturedOutput {
    pub label: String,
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CapturedOutput {
    fn from(program: &str, args: &[&str], output: std::process::Output) -> Self {
        Self {
            label: format!("{program} {}", args.join(" ")),
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }

    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    fn is_transient_tool_failure(&self) -> bool {
        if self.status.success() {
            return false;
        }
        let haystack = format!("{}\n{}", self.stdout_str(), self.stderr_str()).to_ascii_lowercase();
        [
            "connection reset",
            "ssl connect error",
            "timed out",
            "timeout",
            "temporary failure",
            "could not resolve host",
            "network is unreachable",
            "socket hang up",
            "econnreset",
            "etimedout",
            "too many requests",
            "http 429",
            "http 502",
            "http 503",
            "http 504",
            "failed to download",
            "download of config.json failed",
        ]
        .iter()
        .any(|needle| haystack.contains(needle))
    }

    /// Assert the command exited successfully, surfacing stderr on failure.
    pub fn expect_success(self) -> Self {
        assert!(
            self.status.success(),
            "command failed ({}): status={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.label,
            self.status.code(),
            self.stdout_str(),
            self.stderr_str(),
        );
        self
    }

    /// Returns successful output or an error containing both captured streams.
    pub fn require_success(self) -> eyre::Result<Self> {
        if self.status.success() {
            return Ok(self);
        }
        Err(eyre::eyre!(
            "command failed ({}): status={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.label,
            self.status.code(),
            self.stdout_str(),
            self.stderr_str(),
        ))
    }
}

/// The `from -> to` versions of one reported change row, extracted by
/// [`Envelope::change_for`]/[`Envelope::changes_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeVersions {
    /// The version the row moves from.
    pub from: String,
    /// The version the row moves to.
    pub to: String,
}

impl ChangeVersions {
    pub fn new(from: &str, to: &str) -> Self {
        ChangeVersions {
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }
}

/// One lock-edge row of an `upgrade`/`fix` report (an item carrying an `edge` block), extracted by
/// [`Envelope::edge_rows`].
#[derive(Debug)]
pub struct EdgeRow {
    /// The dependency whose binding moved (the row's package column).
    pub dependency: String,
    /// The dependent package whose edge it is.
    pub dependent: String,
    /// The dependent package's source-bearing lock identity, when present.
    pub dependent_source: Option<String>,
    /// The policy outcome: `restored`, `canonicalized`, `rebound`, `held`, or `unaddressable`.
    pub action: String,
    /// Whether this binding outcome is present in the committed lock.
    pub applied: bool,
    /// The binding version before the move (for `held`, the binding that stays in place).
    pub from: String,
    /// The binding version after the move (for `held`, the withheld correction target).
    pub to: String,
}

#[derive(serde::Deserialize)]
struct RawEdgeRow {
    name: String,
    applied: bool,
    from: String,
    to: String,
    edge: RawEdgeInfo,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEdgeInfo {
    dependent: String,
    dependent_source: Option<String>,
    action: String,
}

/// A parsed `cooldown --json` envelope with typed accessors for the fields the invariants check.
#[derive(Debug)]
pub struct Envelope {
    value: serde_json::Value,
}

impl Envelope {
    fn items(&self) -> &[serde_json::Value] {
        self.value
            .get("items")
            .and_then(serde_json::Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    fn summary_u64(&self, key: &str) -> u64 {
        self.value
            .get("summary")
            .and_then(|summary| summary.get(key))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| panic!("missing summary.{key} in {}", self.value))
    }

    pub fn ok(&self) -> bool {
        self.value
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .expect("envelope.ok")
    }

    /// `meta.lockStatus` (flattened at the top level): `current`/`stale`/`unknown` after a real
    /// mutation, `None` under `--dry-run`.
    pub fn lock_status(&self) -> Option<&str> {
        self.value
            .get("lockStatus")
            .and_then(serde_json::Value::as_str)
    }

    pub fn summary_applied(&self) -> u64 {
        self.summary_u64("applied")
    }

    pub fn summary_skipped(&self) -> u64 {
        self.summary_u64("skipped")
    }

    pub fn summary_errors(&self) -> u64 {
        // `outdated`/`check` and `upgrade`/`fix` both carry an `errors` count in summary.
        self.summary_u64("errors")
    }

    pub fn summary_violations(&self) -> u64 {
        self.summary_u64("violations")
    }

    pub fn summary_edges_corrected(&self) -> u64 {
        self.summary_u64("edgesCorrected")
    }

    pub fn summary_edges_rebound(&self) -> u64 {
        self.summary_u64("edgesRebound")
    }

    pub fn summary_edges_held(&self) -> u64 {
        self.summary_u64("edgesHeld")
    }

    pub fn summary_edges_unaddressable(&self) -> u64 {
        self.summary_u64("edgesUnaddressable")
    }

    /// `meta.applied` (flattened at the top level): whether any reported mutation is committed.
    pub fn meta_applied(&self) -> bool {
        self.value
            .get("applied")
            .and_then(serde_json::Value::as_bool)
            .expect("envelope.applied")
    }

    /// The number of *direct* dependencies `check` flagged as a cooldown violation (`direct == true`
    /// and `status == "violation"`). A direct violation is always reducible by `fix`, so it must be
    /// zero after a `fix` converges; graph-held transitive violations may remain (the resolver pins
    /// them and `fix` cannot roll them back). Used by the cargo `fix` invariant.
    pub fn summary_direct_violations(&self) -> u64 {
        let count = self
            .items()
            .iter()
            .filter(|item| {
                item.get("direct").and_then(serde_json::Value::as_bool) == Some(true)
                    && item.get("status").and_then(serde_json::Value::as_str) == Some("violation")
            })
            .count();
        u64::try_from(count).unwrap_or(u64::MAX)
    }

    /// Names of items the mutation actually moved (`applied == true`).
    pub fn applied_names(&self) -> BTreeSet<String> {
        self.filter_names(|item| {
            item.get("applied")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
    }

    /// Names of applied changes reported as version downgrades.
    pub fn downgraded_names(&self) -> BTreeSet<String> {
        self.filter_names(|item| {
            item.get("applied")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                && item
                    .get("downgrade")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
        })
    }

    /// Names of items `upgrade`/`fix` held back because the whole-graph re-resolve rejected the move
    /// (skipped with reason `resolver_conflict`). This is the "held" set the agreement invariants
    /// compare against `outdated`'s `blocked` set.
    pub fn held_conflict_names(&self) -> BTreeSet<String> {
        self.filter_names(|item| {
            let not_applied = !item
                .get("applied")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let reason = item
                .get("skipped")
                .and_then(|skipped| skipped.get("reason"))
                .and_then(serde_json::Value::as_str);
            not_applied && reason == Some("resolver_conflict")
        })
    }

    pub fn skipped_reasons_for(&self, name: &str) -> BTreeSet<String> {
        self.items()
            .iter()
            .filter(|item| item.get("name").and_then(serde_json::Value::as_str) == Some(name))
            .filter_map(|item| {
                item.get("skipped")
                    .and_then(|skipped| skipped.get("reason"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }

    /// The named item's skip detail line (serialized as `skipped.message`), when present.
    pub fn skip_detail_for(&self, name: &str) -> Option<String> {
        self.items().iter().find_map(|item| {
            if item.get("name").and_then(serde_json::Value::as_str) != Some(name) {
                return None;
            }
            item.get("skipped")
                .and_then(|skipped| skipped.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
    }

    pub fn skipped_reasons(&self) -> BTreeSet<String> {
        self.items()
            .iter()
            .filter_map(|item| {
                item.get("skipped")
                    .and_then(|skipped| skipped.get("reason"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }

    /// Names of `outdated` items with the given status string (e.g. `"blocked"`, `"adoptable"`).
    pub fn outdated_with_status(&self, status: &str) -> BTreeSet<String> {
        self.filter_names(|item| {
            item.get("status").and_then(serde_json::Value::as_str) == Some(status)
        })
    }

    /// The named top-level string field of the first item with this `name` (e.g. `"blockedBy"`,
    /// `"adoptableTarget"`, `"current"`), when present.
    pub fn item_field_str(&self, name: &str, field: &str) -> Option<String> {
        self.items().iter().find_map(|item| {
            if item.get("name").and_then(serde_json::Value::as_str) != Some(name) {
                return None;
            }
            item.get(field)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
    }

    /// The `security` block of the first item with this `name`, when present — the
    /// advisory-feed annotation (`fixes`, `severity`, `source`, `applied`).
    pub fn security_for(&self, name: &str) -> Option<serde_json::Value> {
        self.items().iter().find_map(|item| {
            if item.get("name").and_then(serde_json::Value::as_str) != Some(name) {
                return None;
            }
            item.get("security").cloned()
        })
    }

    /// The `window.shortenedBy` advisory id of the first item with this `name`, when present —
    /// set only when the advisory security window replaced the ordinary one.
    pub fn shortened_by_for(&self, name: &str) -> Option<String> {
        self.items().iter().find_map(|item| {
            if item.get("name").and_then(serde_json::Value::as_str) != Some(name) {
                return None;
            }
            item.get("window")?
                .get("shortenedBy")?
                .as_str()
                .map(str::to_owned)
        })
    }

    /// `summary.securityRelevant` of a `check` envelope.
    pub fn summary_security_relevant(&self) -> u64 {
        self.summary_u64("securityRelevant")
    }

    /// The `latest.version` of the named `outdated` item, when present.
    pub fn latest_version_for(&self, name: &str) -> Option<String> {
        self.items().iter().find_map(|item| {
            if item.get("name").and_then(serde_json::Value::as_str) != Some(name) {
                return None;
            }
            item.get("latest")?
                .get("version")?
                .as_str()
                .map(str::to_owned)
        })
    }

    /// The skip's `offending` package for the named item, when present.
    pub fn skipped_offending_for(&self, name: &str) -> Option<String> {
        self.items().iter().find_map(|item| {
            if item.get("name").and_then(serde_json::Value::as_str) != Some(name) {
                return None;
            }
            item.get("skipped")?
                .get("offending")?
                .as_str()
                .map(str::to_owned)
        })
    }

    /// The `from -> to` change reported for a named item, if present.
    pub fn change_for(&self, name: &str) -> Option<ChangeVersions> {
        self.items().iter().find_map(|item| {
            if item.get("name").and_then(serde_json::Value::as_str) != Some(name) {
                return None;
            }
            Some(ChangeVersions {
                from: item.get("from")?.as_str()?.to_owned(),
                to: item.get("to")?.as_str()?.to_owned(),
            })
        })
    }

    /// Every `from -> to` row reported for a package, preserving report order.
    pub fn changes_for(&self, name: &str) -> Vec<ChangeVersions> {
        self.items()
            .iter()
            .filter(|item| item.get("name").and_then(serde_json::Value::as_str) == Some(name))
            .filter_map(|item| {
                Some(ChangeVersions {
                    from: item.get("from")?.as_str()?.to_owned(),
                    to: item.get("to")?.as_str()?.to_owned(),
                })
            })
            .collect()
    }

    /// The lock-edge rows of an `upgrade`/`fix` report, in report order.
    /// Empty when no edge binding moved (or the tool has no edge policy).
    pub fn edge_rows(&self) -> Vec<EdgeRow> {
        self.items()
            .iter()
            .filter(|item| item.get("edge").is_some())
            .map(|item| {
                let raw: RawEdgeRow = serde_json::from_value(item.clone())
                    .expect("edge-bearing upgrade row matches the JSON schema");
                EdgeRow {
                    dependency: raw.name,
                    dependent: raw.edge.dependent,
                    dependent_source: raw.edge.dependent_source,
                    action: raw.edge.action,
                    applied: raw.applied,
                    from: raw.from,
                    to: raw.to,
                }
            })
            .collect()
    }

    pub fn warning_kinds(&self) -> BTreeSet<String> {
        self.diagnostic_values("warnings", "kind")
    }

    pub fn error_kinds(&self) -> BTreeSet<String> {
        self.diagnostic_values("errors", "kind")
    }

    pub fn error_messages(&self) -> BTreeSet<String> {
        self.diagnostic_values("errors", "message")
    }

    pub fn warning_paths(&self) -> BTreeSet<String> {
        self.diagnostic_values("warnings", "path")
    }

    pub fn error_paths(&self) -> BTreeSet<String> {
        self.diagnostic_values("errors", "path")
    }

    fn diagnostic_values(&self, section: &str, key: &str) -> BTreeSet<String> {
        self.value
            .get(section)
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|diagnostic| {
                diagnostic
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }

    fn filter_names(&self, pred: impl Fn(&serde_json::Value) -> bool) -> BTreeSet<String> {
        self.items()
            .iter()
            .filter(|item| pred(item))
            .filter_map(|item| {
                item.get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }
}

/// Whether a tool binary is resolvable on `PATH`. Local integration tests skip with a clear
/// message when their package manager is absent; CI treats the same condition as a setup failure.
pub fn tool_on_path(tool: &str) -> bool {
    program_on_path(tool).is_some()
}

/// Emit a skip notice and return early from a local test body when its real package manager is not
/// installed. CI provisions every required tool, so a missing binary there is an infrastructure
/// failure rather than a passing skipped test.
#[macro_export]
macro_rules! skip_if_missing {
    ($tool:expr, $return:expr) => {
        let tool = $tool;
        if !$crate::support::tool_on_path(tool) {
            assert!(
                std::env::var_os("CI").is_none(),
                "`{tool}` not found on PATH even though CI must provision every integration tool"
            );
            eprintln!(
                "skipping: `{}` not found on PATH (provision it via the repo `mise.toml`)",
                tool
            );
            return $return;
        }
    };
    ($tool:expr) => {
        $crate::skip_if_missing!($tool, ());
    };
}

/// Extracts the `name -> pinned version` map from a lock file's raw bytes. Each lock format has its
/// own shape, so each ecosystem supplies a parser (e.g. [`toml_lock_pins`] for `uv.lock` /
/// `Cargo.lock`). Used by [`changed_packages`] to diff two locks without depending on the format.
pub type PinParser = fn(&[u8]) -> BTreeMap<String, String>;

/// The set of packages whose pinned version *moved* between two lock files — i.e. present in both
/// locks at a different version, parsed via the ecosystem's `pins` strategy.
///
/// Packages that leave or join the graph are deliberately excluded: a removal/addition is a
/// graph-shape consequence of a reported direct move (e.g. a dependency dropping one of its own
/// deps removes that transitive package from the lock), not a silent *version* change. The
/// invariant under test is that no surviving package's version moves without appearing in the
/// report.
pub fn changed_packages(before: &[u8], after: &[u8], pins: PinParser) -> BTreeSet<String> {
    let before_pins = pins(before);
    let after_pins = pins(after);
    let mut changed = BTreeSet::new();
    for (name, before_version) in &before_pins {
        if let Some(after_version) = after_pins.get(name)
            && before_version != after_version
        {
            changed.insert(name.clone());
        }
    }
    changed
}

/// A [`PinParser`] for TOML lock files that emit each package as a `[[package]]` block with a
/// `name = "…"` line followed (within the same block) by a `version = "…"` line. This is the shape
/// of both `uv.lock` and `Cargo.lock`, so both ecosystems share it. A line-based scan is enough to
/// detect every moved pin without a TOML dependency in the test.
pub fn toml_lock_pins(lock: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(lock);
    let mut pins = BTreeMap::new();
    let mut current_name: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(value) = toml_field(trimmed, "name") {
            current_name = Some(value);
        } else if let Some(value) = toml_field(trimmed, "version")
            && let Some(name) = current_name.take()
        {
            pins.insert(name, value);
        }
    }
    pins
}

/// A [`PinParser`] for `go.mod`: the `module path → version` map of every `require` directive,
/// handling both the single-line form (`require path v1.2.3`) and the grouped `require ( … )` block.
/// A trailing `// indirect` (or any `//` comment) is stripped. This is the `go.mod` analogue of
/// [`toml_lock_pins`], so the Go convergence test can diff two `go.mod` files via [`changed_packages`]
/// without a TOML dependency.
pub fn go_mod_pins(go_mod: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(go_mod);
    let mut pins = BTreeMap::new();
    let mut in_block = false;
    for raw in text.lines() {
        let line = match raw.find("//") {
            Some(index) => &raw[..index],
            None => raw,
        }
        .trim();
        if line.is_empty() {
            continue;
        }
        if in_block {
            if line == ")" {
                in_block = false;
            } else if let Some(require) = go_require_pair(line) {
                pins.insert(require.path, require.version);
            }
        } else if line == "require (" {
            in_block = true;
        } else if let Some(rest) = line.strip_prefix("require ")
            && let Some(require) = go_require_pair(rest.trim())
        {
            pins.insert(require.path, require.version);
        }
    }
    pins
}

/// A [`PinParser`] for `pnpm-lock.yaml`: the `name -> newest pinned version` map of every key in the
/// top-level `packages:` section. Each key is `name@version` (scoped names keep their leading `@`),
/// optionally followed by a `(peer@x)` peer-disambiguation suffix that is stripped. A name can appear
/// at several versions (duplicate graph copies); the newest is kept so a moved direct declaration is
/// not masked by a stale transitive copy — matching the adapter's own `locked_versions`. This is the
/// `pnpm-lock.yaml` analogue of [`toml_lock_pins`], so the pnpm convergence test can diff two locks
/// via [`changed_packages`] without a YAML dependency.
pub fn pnpm_lock_pins(lock: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(lock);
    let mut pins: BTreeMap<String, String> = BTreeMap::new();
    let mut in_packages = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(stripped) = line.strip_prefix("  ") {
            if !in_packages || stripped.starts_with(' ') {
                continue; // outside the section, or a nested field of a package entry
            }
            let key = stripped
                .trim()
                .trim_end_matches(':')
                .trim_matches('\'')
                .trim_matches('"');
            // Drop the `(peer@x)` suffix pnpm appends to disambiguate peer resolutions.
            let key = key.split('(').next().unwrap_or(key);
            let Some(at) = key.rfind('@').filter(|&index| index > 0) else {
                continue;
            };
            let (name, version) = (key[..at].to_string(), key[at + 1..].to_string());
            match pins.entry(name) {
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    if pnpm_version_gt(&version, slot.get()) {
                        *slot.get_mut() = version;
                    }
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(version);
                }
            }
        } else {
            in_packages = line.starts_with("packages:");
        }
    }
    pins
}

/// A coarse "is `a` a newer version than `b`" for keeping the newest duplicate copy in
/// [`pnpm_lock_pins`]. Compares dot-separated numeric components left to right; a non-numeric
/// component (a prerelease tag) compares as lower. Exact precision is not needed — the diff only asks
/// whether a name's newest copy *changed*, and both locks parse identically.
fn pnpm_version_gt(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<i64> {
        v.split(['.', '-', '+'])
            .map(|component| component.parse::<i64>().unwrap_or(-1))
            .collect()
    };
    parts(a) > parts(b)
}

/// A [`PinParser`] for `deno.lock`: the `name -> newest pinned version` map unioning the top-level
/// `jsr` and `npm` objects, each keyed `name@version` (scoped names keep their leading `@`). A name
/// can appear at several versions (duplicate graph copies); the newest is kept so a moved direct
/// declaration is not masked by a stale transitive copy — matching the adapter's own `locked_versions`.
/// `deno.lock` is JSON, so this parses it with `serde_json` (already a test dependency) rather than a
/// line scan, the `deno.lock` analogue of [`pnpm_lock_pins`].
pub fn deno_lock_pins(lock: &[u8]) -> BTreeMap<String, String> {
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(lock) else {
        return BTreeMap::new();
    };
    let mut pins: BTreeMap<String, String> = BTreeMap::new();
    for section in ["jsr", "npm"] {
        let Some(obj) = doc.get(section).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for key in obj.keys() {
            let Some(at) = key.rfind('@').filter(|&index| index > 0) else {
                continue;
            };
            let (name, version) = (key[..at].to_string(), key[at + 1..].to_string());
            match pins.entry(name) {
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    if pnpm_version_gt(&version, slot.get()) {
                        *slot.get_mut() = version;
                    }
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(version);
                }
            }
        }
    }
    pins
}

/// One `go.mod` require line split into its parts.
struct GoRequire {
    /// The required module path.
    path: String,
    /// The `v`-prefixed pinned version.
    version: String,
}

/// Split a `module/path v1.2.3` line, requiring a `v`-prefixed second field.
fn go_require_pair(line: &str) -> Option<GoRequire> {
    let mut fields = line.split_whitespace();
    let path = fields.next()?;
    let version = fields.next()?;
    version.starts_with('v').then(|| GoRequire {
        path: path.to_string(),
        version: version.to_string(),
    })
}

/// Extract the quoted value of a `key = "value"` line, if the line is exactly that field.
fn toml_field(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let inner = rest.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_owned())
}
