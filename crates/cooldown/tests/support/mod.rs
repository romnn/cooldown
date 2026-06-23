//! Shared harness for the temp-dir convergence integration tests.
//!
//! Each test creates a fresh temp dir, writes a minimal project fixture on the fly (a fresh temp
//! dir is the same deterministic starting state every run — no checked-in repos, no stale-lock
//! drift), runs the real `cooldown` binary against it, and parses the `--json` envelope.
//!
//! The fixtures pin the resolution clock to a fixed instant via the cooldown `--freeze <DATE>`
//! cutoff (an absolute exclude-newer), so the underlying resolver replays PyPI's immutable history
//! and reproduces the same resolve forever. Tests assert invariants (convergence, no-silent-change,
//! cross-command agreement), never hard-coded versions.
//!
//! The harness is intentionally ecosystem-agnostic: [`Fixture`] only knows how to write files and
//! drive `cooldown`. Adding cargo/go/pnpm coverage later is "write a fixture generator + reuse the
//! same invariant assertions on the returned [`Envelope`]", not a rewrite.

#![allow(
    dead_code,
    reason = "the harness is shared across per-ecosystem integration test files; not every helper is used by every file, and only uv is covered today"
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A throwaway project tree under a temp dir, plus the means to run `cooldown` against it.
pub struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    /// Create an empty fixture rooted at a fresh temp dir.
    pub fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("cooldown-it-")
            .tempdir()
            .expect("create temp dir");
        Self { dir }
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
        let mut command = Command::new(program);
        command.current_dir(self.dir.path()).args(args);
        for (key, value) in envs {
            command.env(key, value);
        }
        let output = command
            .output()
            .unwrap_or_else(|err| panic!("spawn {program}: {err}"));
        CapturedOutput::from(program, args, output)
    }

    /// Run the built `cooldown` binary against the fixture with the given args, capturing output.
    /// `CARGO_BIN_EXE_cooldown` is injected by Cargo for integration tests.
    pub fn cooldown(&self, args: &[&str]) -> CapturedOutput {
        let exe = env!("CARGO_BIN_EXE_cooldown");
        let output = Command::new(exe)
            .current_dir(self.dir.path())
            .args(args)
            // Pin tool/dir explicitly so detection is deterministic regardless of ambient state.
            .arg("--dir")
            .arg(self.dir.path())
            .output()
            .expect("spawn cooldown binary");
        CapturedOutput::from("cooldown", args, output)
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

    /// Assert the command exited successfully, surfacing stderr on failure.
    pub fn expect_success(self) -> Self {
        assert!(
            self.status.success(),
            "command failed ({}): status={:?}\n--- stderr ---\n{}",
            self.label,
            self.status.code(),
            self.stderr_str(),
        );
        self
    }
}

/// A parsed `cooldown --json` envelope with typed accessors for the fields the invariants check.
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

    /// `meta.lockVerified` (flattened at the top level): `Some(bool)` after a real mutation, `None`
    /// under `--dry-run`.
    pub fn lock_verified(&self) -> Option<bool> {
        self.value
            .get("lockVerified")
            .and_then(serde_json::Value::as_bool)
    }

    pub fn summary_applied(&self) -> u64 {
        self.summary_u64("applied")
    }

    pub fn summary_errors(&self) -> u64 {
        // `outdated`/`check` and `upgrade`/`fix` both carry an `errors` count in summary.
        self.summary_u64("errors")
    }

    pub fn summary_violations(&self) -> u64 {
        self.summary_u64("violations")
    }

    /// Names of items the mutation actually moved (`applied == true`).
    pub fn applied_names(&self) -> BTreeSet<String> {
        self.filter_names(|item| {
            item.get("applied")
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

    /// Names of `outdated` items with the given status string (e.g. `"blocked"`, `"adoptable"`).
    pub fn outdated_with_status(&self, status: &str) -> BTreeSet<String> {
        self.filter_names(|item| {
            item.get("status").and_then(serde_json::Value::as_str) == Some(status)
        })
    }

    /// The `from -> to` change reported for a named item, if present.
    pub fn change_for(&self, name: &str) -> Option<(String, String)> {
        self.items().iter().find_map(|item| {
            if item.get("name").and_then(serde_json::Value::as_str) != Some(name) {
                return None;
            }
            let from = item.get("from")?.as_str()?.to_owned();
            let to = item.get("to")?.as_str()?.to_owned();
            Some((from, to))
        })
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

/// Whether a tool binary is resolvable on `PATH`. Integration tests skip (with a clear message)
/// when their package manager is absent, so the fast unit run on a bare machine is unaffected.
pub fn tool_on_path(tool: &str) -> bool {
    which(tool).is_some()
}

fn which(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(tool);
        candidate.is_file().then_some(candidate)
    })
}

/// Emit a skip notice and return early from a test body. Pairs with the `#[ignore]` gate: when the
/// suite is opted into (`--run-ignored all`) but the tool is missing, the test prints why instead
/// of failing.
#[macro_export]
macro_rules! skip_if_missing {
    ($tool:expr) => {
        if !$crate::support::tool_on_path($tool) {
            eprintln!(
                "skipping: `{}` not found on PATH (provision it via the repo `mise.toml`)",
                $tool
            );
            return;
        }
    };
}
