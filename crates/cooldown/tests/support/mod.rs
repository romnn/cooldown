//! Shared, tool-agnostic harness for the temp-dir convergence integration tests.
//!
//! Each test creates a fresh temp dir, writes a minimal project fixture on the fly (a fresh temp
//! dir is the same deterministic starting state every run — no checked-in repos, no stale-lock
//! drift), seeds a starting lock with the ecosystem's own package manager, runs the real
//! `cooldown` binary against it, and parses the `--json` envelope.
//!
//! The fixtures pin the resolution clock to a fixed instant via the cooldown `--freeze <DATE>`
//! cutoff (an absolute exclude-newer), so the underlying resolver replays the registry's immutable
//! history and reproduces the same resolve forever. Tests assert invariants (convergence,
//! no-silent-change, cross-command agreement), never hard-coded versions.
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
            } else if let Some((path, version)) = go_require_pair(line) {
                pins.insert(path, version);
            }
        } else if line == "require (" {
            in_block = true;
        } else if let Some(rest) = line.strip_prefix("require ")
            && let Some((path, version)) = go_require_pair(rest.trim())
        {
            pins.insert(path, version);
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

/// Split a `module/path v1.2.3` line into `(path, version)`, requiring a `v`-prefixed second field.
fn go_require_pair(line: &str) -> Option<(String, String)> {
    let mut fields = line.split_whitespace();
    let path = fields.next()?;
    let version = fields.next()?;
    version
        .starts_with('v')
        .then(|| (path.to_string(), version.to_string()))
}

/// Extract the quoted value of a `key = "value"` line, if the line is exactly that field.
fn toml_field(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let inner = rest.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_owned())
}
