//! Run-scoped replay of the lock-resolving Cargo commands an apply runs in a staged project.
//!
//! An apply's steps repeat Cargo commands against byte-identical inputs: a later pass retries a
//! pin from the lock an earlier pass restored, and a step Cargo completed is refused afterwards by
//! the plan-dependent [acceptance rule](crate::tool), which the inputs alone do not decide.
//! A command's native outcome is its exit status, its output and the lock it leaves behind, and it
//! depends only on the native inputs the [rejection memo](crate::rejection_memo) captures, the
//! target files Cargo discovers beside each manifest, the directory it runs in and its arguments.
//! So a repeated command replays its recorded outcome instead of running Cargo, and everything
//! after the command, the acceptance judgement included, runs on the replay exactly as on a run.
//!
//! The replay rests on the assumptions the rejection memo already makes for one run: the registry
//! index and the Cargo toolchain stay as they are.
//! Two more inputs vary within a run, so a command that depends on them always runs:
//!
//! - A positive publish age makes the resolver's choice depend on the time.
//! - Offline resolution chooses from the crates downloaded so far, which other processes change.
//!
//! Only staged copies take part, and only while every package Cargo loads lies in the copy's
//! private tree.
//! cooldown stages regular files there and writes nothing but the lock and manifests the key
//! captures, so the target files Cargo discovers stay as staged, and a package or target source
//! outside the tree, which anything may change, makes a command run instead.
//! An outcome is reused only in the directory it was recorded in, whose paths its output names.

use std::collections::{HashMap, VecDeque};
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, Project};

use crate::cargocmd::{
    Cargo, CommandLine, PackageKey, RESOLVE_ARGS, ResolvedGraph, as_strs,
    memoizable_resolver_rejection, update_precise_args, update_unlocking_args,
};
use crate::locked_reads::{LockedReader, LockedReads};
use crate::publish_age::{CargoEnvironment, parse_age};
use crate::rejection_memo::{LockSource, MemoKey, MemoOperation, RejectionMemo};

/// The recorded outcomes of the run, least recently used first.
#[derive(Debug)]
pub(crate) struct CommandOutcomes {
    store: Mutex<Store>,
    budget: Budget,
    /// The private tree of each registered staged project root.
    trees: Mutex<HashMap<Utf8PathBuf, Utf8PathBuf>>,
}

/// What [`CommandOutcomes`] keeps at most.
#[derive(Debug, Clone, Copy)]
struct Budget {
    /// The bytes all entries are charged together (see [`Recorded::charge`]).
    bytes: usize,
    entries: usize,
}

impl Budget {
    /// One `cargo metadata` output of a large workspace takes several megabytes, and one lock up
    /// to a few, so this keeps the outcomes of a batch's repeated pins without letting the graph
    /// outputs grow the run's memory unbounded.
    const DEFAULT: Self = Self {
        bytes: 128 << 20,
        entries: 1024,
    };

    /// What an entry is charged beyond its payload, so that outcomes without one still count.
    const ENTRY_OVERHEAD: usize = 1024;
}

impl Default for CommandOutcomes {
    fn default() -> Self {
        Self::with_budget(Budget::DEFAULT)
    }
}

#[derive(Debug, Default)]
struct Store {
    entries: VecDeque<Recorded>,
    charged: usize,
}

/// One recorded outcome and the inputs it was recorded from.
#[derive(Debug)]
struct Recorded {
    key: CommandKey,
    outcome: Arc<Outcome>,
}

impl Recorded {
    /// The bytes this entry holds, plus [`Budget::ENTRY_OVERHEAD`].
    fn charge(&self) -> usize {
        let Outcome { output, lock, .. } = self.outcome.as_ref();
        let lock = match lock {
            LockAfter::Unchanged => 0,
            LockAfter::Rewritten(bytes) => bytes.len(),
        };
        let arguments: usize = self.key.args.iter().map(String::len).sum();
        output.stdout.len()
            + output.stderr.len()
            + lock
            + arguments
            + self.key.root.as_str().len()
            + Budget::ENTRY_OVERHEAD
    }
}

/// What a command's outcome is a function of.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandKey {
    /// The directory Cargo runs in, which its output and diagnostics name.
    root: Utf8PathBuf,
    inputs: MemoKey,
    /// The arguments as spawned, the crates.io publish-age override included.
    args: Vec<String>,
}

/// The native outcome of one command.
#[derive(Debug)]
pub(crate) struct Outcome {
    /// The command's output; a successful command's stdout is dropped when its caller needs only
    /// the status (see [`Outcome::stdout_kept`]).
    output: Output,
    stdout_kept: bool,
    lock: LockAfter,
    /// How long Cargo took, which a replay saves.
    elapsed: Duration,
}

/// The `Cargo.lock` a command left behind.
#[derive(Debug)]
enum LockAfter {
    /// The lock the command started from.
    Unchanged,
    /// The lock the command wrote in place of the one it started from.
    Rewritten(Vec<u8>),
}

/// Which part of an outcome a caller reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Needs {
    /// Success or failure; a failure's whole output is always kept.
    Status,
    /// The output of a successful command too.
    Stdout,
}

impl CommandOutcomes {
    fn with_budget(budget: Budget) -> Self {
        Self {
            store: Mutex::new(Store::default()),
            budget,
            trees: Mutex::new(HashMap::new()),
        }
    }

    /// Registers `staged` as a staged project root whose copy lives in the canonical directory
    /// `tree`, which only cooldown writes.
    pub(crate) fn register_staged(&self, staged: &Utf8Path, tree: &Utf8Path) {
        self.trees
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(staged.to_owned(), tree.to_owned());
    }

    /// The private tree of the staged project root `root`, if it was registered.
    fn tree(&self, root: &Utf8Path) -> Option<Utf8PathBuf> {
        self.trees
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .cloned()
    }

    fn get(&self, key: &CommandKey, needs: Needs) -> Option<Arc<Outcome>> {
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let position = store.entries.iter().position(|recorded| {
            recorded.key == *key && (needs == Needs::Status || recorded.outcome.stdout_kept)
        })?;
        let recorded = store.entries.remove(position)?;
        let outcome = Arc::clone(&recorded.outcome);
        store.entries.push_back(recorded);
        Some(outcome)
    }

    fn insert(&self, recorded: Recorded) {
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Store { entries, charged } = &mut *store;
        if let Some(position) = entries.iter().position(|kept| kept.key == recorded.key)
            && let Some(replaced) = entries.remove(position)
        {
            *charged -= replaced.charge();
        }
        *charged += recorded.charge();
        entries.push_back(recorded);
        while *charged > self.budget.bytes || entries.len() > self.budget.entries {
            let Some(evicted) = entries.pop_front() else {
                break;
            };
            *charged -= evicted.charge();
        }
    }
}

/// Runs lock-resolving Cargo commands through [`CommandOutcomes`].
#[derive(Clone, Copy)]
pub(crate) struct MemoizedCargo<'a> {
    pub(crate) cargo: &'a Cargo,
    pub(crate) memo: &'a RejectionMemo,
    pub(crate) outcomes: &'a CommandOutcomes,
    /// The run's locked reads, which a resolve that left the lock as it was feeds.
    pub(crate) reads: &'a LockedReads,
    /// Whether outcomes are recorded and replayed at all, which the rejection memo's switch
    /// decides.
    pub(crate) enabled: bool,
}

/// A command that ran or replayed, with what recording a fresh outcome takes.
struct Run {
    outcome: Arc<Outcome>,
    /// The key a fresh, reusable outcome is recorded under.
    record: Option<CommandKey>,
}

impl MemoizedCargo<'_> {
    /// Resolves `project`'s whole graph like [`Cargo::metadata`], writing the lock it settles on.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Cargo::metadata`].
    pub(crate) async fn metadata(&self, project: &Project) -> Result<ResolvedGraph, CoreError> {
        let run = self.run(project, &RESOLVE_ARGS, Needs::Stdout).await?;
        let graph = self.graph(project, &run);
        self.record(run, Needs::Stdout);
        graph
    }

    /// Resolves `project`'s whole graph like [`Self::metadata`], for a caller that only needs the
    /// lock it settles on.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Cargo::metadata`].
    pub(crate) async fn resolve(&self, project: &Project) -> Result<(), CoreError> {
        let run = self.run(project, &RESOLVE_ARGS, Needs::Status).await?;
        // Only an output that parsed is recorded without its stdout, so its replay skips the parse.
        let parsed = if run.outcome.stdout_kept {
            self.graph(project, &run).map(|_| ())
        } else {
            Ok(())
        };
        // An output that does not parse keeps its stdout, so its replay fails the same way.
        let needs = if parsed.is_ok() {
            Needs::Status
        } else {
            Needs::Stdout
        };
        self.record(run, needs);
        parsed
    }

    /// Pins the crates.io `name@from` node to exactly `to` via `cargo update -p <spec>
    /// --precise <to>`, with the spec source-qualified as the crates.io index.
    ///
    /// The qualified spec cannot address a same-name package from another registry, and
    /// `@<from>` disambiguates a crate resolved at multiple versions.
    /// One spec per invocation, never several: cargo accepts multiple `-p` specs alongside
    /// `--precise` but silently applies the pin to only the first spec and exits 0 (observed on
    /// cargo 1.96), so batching crates that share a target version into one call loses every pin
    /// but the first.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `cargo` cannot be spawned, or [`CoreError::Tool`] if the
    /// update is rejected (e.g. a `=`-pin or resolver conflict blocks the precise move, or `from`
    /// no longer names a locked node).
    /// A rejection is the caller's signal that the candidate stays where the resolver placed it.
    pub(crate) async fn update_precise_crates_io(
        &self,
        project: &Project,
        name: &str,
        from: &str,
        to: &str,
    ) -> Result<(), CoreError> {
        self.update_precise_crates_io_unlocking(project, name, from, to, &[])
            .await
    }

    /// Pins the crates.io `name@from` node to exactly `to` like
    /// [`update_precise_crates_io`](Self::update_precise_crates_io), with each crates.io node in
    /// `unlocked` also released for the same resolve.
    ///
    /// Cargo applies `--precise` to the first spec only and re-resolves every other named node
    /// freely, which is what lets a pin land when its target needs a version of another locked
    /// crate (pest 2.9.3 requires `psm = "=0.1.24"` while another dependent holds psm 0.1.32).
    /// The caller has to verify the outcome: an unlocked node may land anywhere its requirements
    /// admit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`update_precise_crates_io`](Self::update_precise_crates_io).
    pub(crate) async fn update_precise_crates_io_unlocking(
        &self,
        project: &Project,
        name: &str,
        from: &str,
        to: &str,
        unlocked: &[PackageKey],
    ) -> Result<(), CoreError> {
        self.update(project, &update_precise_args(name, from, to, unlocked))
            .await
    }

    /// Releases and re-resolves `unlocked` like [`Cargo::update_unlocking`].
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Cargo::update_unlocking`].
    pub(crate) async fn update_unlocking(
        &self,
        project: &Project,
        unlocked: &[PackageKey],
    ) -> Result<(), CoreError> {
        self.update(project, &update_unlocking_args(unlocked)).await
    }

    async fn update(&self, project: &Project, args: &[String]) -> Result<(), CoreError> {
        let run = self.run(project, &as_strs(args), Needs::Status).await?;
        let updated = self.cargo.stdout_of(&run.outcome.output).map(|_| ());
        self.record(run, Needs::Status);
        updated
    }

    fn graph(&self, project: &Project, run: &Run) -> Result<ResolvedGraph, CoreError> {
        let stdout = self.cargo.stdout_of(&run.outcome.output)?;
        Cargo::parse_graph(&stdout, &project.generated_members)
    }

    /// Replays the outcome recorded for this command's inputs, or runs Cargo.
    async fn run(&self, project: &Project, args: &[&str], needs: Needs) -> Result<Run, CoreError> {
        let command = self.cargo.command_line(&project.root, args)?;
        let key = self.key(project, &command);
        if let Some(pending) = &key
            && let Some(outcome) = self.outcomes.get(&pending.key, needs)
        {
            // A replay has no subprocess to perform the supervisor's interruption check.
            cooldown_core::interrupt::ensure_not_requested("replaying a cargo command")?;
            if let LockAfter::Rewritten(lock) = &outcome.lock {
                std::fs::write(lock_path(&project.root), lock)?;
            }
            tracing::debug!(
                args = ?command.args,
                dir = %project.root,
                saved_ms = outcome.elapsed.as_millis(),
                "cargo command replayed"
            );
            self.share_unchanged_resolve(project, args, &pending.key.inputs, &outcome);
            return Ok(Run {
                outcome,
                record: None,
            });
        }
        let started = Instant::now();
        let output = self.cargo.spawn(&project.root, &command.args).await?;
        let elapsed = started.elapsed();
        let settled = key.and_then(|key| self.settle(project, key, &output));
        let (lock, record) = match settled {
            Some(Settled { lock, key }) => (lock, Some(key)),
            None => (LockAfter::Unchanged, None),
        };
        let outcome = Arc::new(Outcome {
            output,
            stdout_kept: true,
            lock,
            elapsed,
        });
        if let Some(key) = &record {
            self.share_unchanged_resolve(project, args, &key.inputs, &outcome);
        }
        Ok(Run { outcome, record })
    }

    /// Keeps the output of a whole-graph resolve that left the lock as it was among the run's
    /// locked reads, so the locked reads of the same inputs that follow need not run Cargo.
    ///
    /// Cargo writes no lock when the resolve serializes to the lines it read, and a locked read of
    /// the same inputs then succeeds with the same output.
    /// The converse does not hold, so a kept locked read never stands in for a resolve: Cargo also
    /// accepts a locked read whose lock lines differ from the serialized resolve as long as both
    /// parse to the same resolve, and an unlocked resolve rewrites such a lock.
    ///
    /// Only an outcome with a key is shared, so only in a staged copy whose packages and target
    /// sources all lie in its private tree, where the target files Cargo discovers stay as staged.
    fn share_unchanged_resolve(
        &self,
        project: &Project,
        args: &[&str],
        inputs: &MemoKey,
        outcome: &Outcome,
    ) {
        let unchanged = matches!(outcome.lock, LockAfter::Unchanged);
        let succeeded = outcome.output.status.success();
        if args != RESOLVE_ARGS || !unchanged || !succeeded || !outcome.stdout_kept {
            return;
        }
        let reader = LockedReader {
            cargo: self.cargo,
            memo: self.memo,
            reads: self.reads,
        };
        reader.keep_settled(
            project,
            inputs.clone().with_operation(MemoOperation::LockedRead),
            &String::from_utf8_lossy(&outcome.output.stdout),
        );
    }

    /// Records a fresh outcome, dropping a successful command's stdout unless `needs` reads it.
    fn record(&self, run: Run, needs: Needs) {
        let Run { outcome, record } = run;
        let Some(key) = record else {
            return;
        };
        let Ok(mut outcome) = Arc::try_unwrap(outcome) else {
            return;
        };
        if needs == Needs::Status && outcome.output.status.success() {
            outcome.output.stdout = Vec::new();
            outcome.stdout_kept = false;
        }
        self.outcomes.insert(Recorded {
            key,
            outcome: Arc::new(outcome),
        });
    }

    /// The key of a command about to run in `project`, or `None` when its outcome is not
    /// reusable.
    fn key(&self, project: &Project, command: &CommandLine) -> Option<PendingKey> {
        if !self.enabled {
            return None;
        }
        let tree = self.outcomes.tree(&project.root)?;
        if !stable_resolution(
            &project.root,
            &CargoEnvironment::current(),
            command.crates_io_age.as_deref(),
        ) {
            return None;
        }
        let lock = read_lock(&project.root).ok()?;
        let inputs = self.inputs(project, &tree, lock.clone())?;
        Some(PendingKey {
            key: CommandKey {
                root: project.root.clone(),
                inputs,
                args: command.args.clone(),
            },
            tree,
            lock,
        })
    }

    /// The key of `project`'s native inputs with `lock` as its lock, or `None` when they cannot
    /// all be captured or Cargo would load a package or target source outside `tree`.
    fn inputs(&self, project: &Project, tree: &Utf8Path, lock: Option<Vec<u8>>) -> Option<MemoKey> {
        let inputs = self
            .memo
            .command_inputs(project, LockSource::Contents(lock))
            .inspect_err(|err| tracing::debug!(%err, "cargo command memo input capture bypassed"))
            .ok()?;
        if !inputs.key.is_cacheable() {
            return None;
        }
        let private = inputs.manifests.iter().all(|manifest| {
            within(&manifest.directory, tree)
                && declared_targets(&manifest.directory, &manifest.document)
                    .iter()
                    .all(|target| within(target, tree))
        });
        if !private {
            tracing::debug!(
                dir = %project.root,
                "cargo command memo bypassed because a package or target source lies outside the staged copy"
            );
            return None;
        }
        Some(inputs.key)
    }

    /// The lock a command left behind and the key to record it under, when its outcome is
    /// reusable: a success or a resolver refusal, after which every input but the lock stands as
    /// it was keyed.
    fn settle(&self, project: &Project, pending: PendingKey, output: &Output) -> Option<Settled> {
        let reproducible =
            output.status.success() || memoizable_resolver_rejection(&self.cargo.failure(output));
        if !reproducible {
            return None;
        }
        let PendingKey {
            key,
            tree,
            lock: before,
        } = pending;
        let after = read_lock(&project.root).ok()?;
        // The inputs are captured again with the lock the command started from, so the lock the
        // command itself wrote is the only change they tolerate.
        if self.inputs(project, &tree, before.clone())? != key.inputs {
            return None;
        }
        let lock = if after == before {
            LockAfter::Unchanged
        } else {
            // Cargo never deletes the lock it resolves; an outcome that did is not replayed.
            LockAfter::Rewritten(after?)
        };
        Some(Settled { lock, key })
    }
}

/// A key computed before a command ran, with the private tree it was checked against and the lock
/// it was computed from.
struct PendingKey {
    key: CommandKey,
    tree: Utf8PathBuf,
    lock: Option<Vec<u8>>,
}

/// A fresh outcome's lock and the key it is recorded under.
struct Settled {
    lock: LockAfter,
    key: CommandKey,
}

/// Whether `path`, with every symbolic link resolved, lies in `tree`; `false` when it does not
/// exist.
fn within(path: &Utf8Path, tree: &Utf8Path) -> bool {
    std::fs::canonicalize(path)
        .ok()
        .and_then(|path| Utf8PathBuf::from_path_buf(path).ok())
        .is_some_and(|path| path.starts_with(tree))
}

/// The path a target table of a manifest names, if any.
fn path_of(table: &toml::Value) -> Option<&str> {
    table.get("path").and_then(toml::Value::as_str)
}

/// The target sources the manifest `document` in `directory` names by path.
///
/// A target named without a path is inferred from the package directory itself.
fn declared_targets(directory: &Utf8Path, document: &toml::Value) -> Vec<Utf8PathBuf> {
    let mut paths: Vec<&str> = Vec::new();
    paths.extend(document.get("lib").and_then(path_of));
    for kind in ["bin", "example", "test", "bench"] {
        let tables = document.get(kind).and_then(toml::Value::as_array);
        paths.extend(tables.into_iter().flatten().filter_map(path_of));
    }
    match document
        .get("package")
        .and_then(|package| package.get("build"))
    {
        Some(toml::Value::String(build)) => paths.push(build),
        Some(toml::Value::Array(builds)) => {
            paths.extend(builds.iter().filter_map(toml::Value::as_str));
        }
        _ => {}
    }
    paths.into_iter().map(|path| directory.join(path)).collect()
}

fn lock_path(root: &Utf8Path) -> Utf8PathBuf {
    root.join("Cargo.lock")
}

/// The lock in `root`, `None` when there is none.
fn read_lock(root: &Utf8Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(lock_path(root)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Whether a command run in `dir` resolves the same whenever it runs: Cargo is online and no
/// positive publish age reaches any registry.
///
/// `crates_io_age` is the crates.io age the command line passes, which Cargo ranks above every
/// configured one.
/// `resolver.incompatible-publish-age` is not consulted, so a configured positive age always makes
/// a command run.
/// A setting that cannot be read counts as a dependence, which only costs the replay.
fn stable_resolution(dir: &Utf8Path, env: &CargoEnvironment, crates_io_age: Option<&str>) -> bool {
    if env
        .offline
        .as_deref()
        .is_some_and(|offline| offline != "false")
    {
        return false;
    }
    let mut ages: Vec<String> = crates_io_age.iter().map(|&age| age.to_owned()).collect();
    ages.extend(env.global_age.iter().cloned());
    ages.extend(
        // The command line's crates.io age does not override `CARGO_REGISTRIES_CRATES_IO_*`
        // safely: the variable also names a registry called `crates_io`.
        env.registry_ages.iter().cloned(),
    );
    for path in env.config_files(dir) {
        let Some(config) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        else {
            return false;
        };
        let offline = config.get("net").and_then(|net| net.get("offline"));
        if offline.is_some_and(|offline| offline != &toml::Value::Boolean(false)) {
            return false;
        }
        let global = config
            .get("registry")
            .and_then(|registry| registry.get("global-min-publish-age"));
        let registries = config
            .get("registries")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flatten()
            .filter(|(name, _)| crates_io_age.is_none() || *name != "crates-io")
            .filter_map(|(_, registry)| registry.get("min-publish-age"));
        for age in global.into_iter().chain(registries) {
            let Some(age) = age.as_str() else {
                return false;
            };
            ages.push(age.to_owned());
        }
    }
    ages.iter()
        .all(|age| parse_age(age).is_ok_and(|age| age.is_zero()))
}

#[cfg(all(test, unix))]
mod tests;
