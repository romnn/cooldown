//! Proves an npm `package-lock.json` current with npm's own `npm ci --dry-run`.
//!
//! `npm ci` refuses a lock that no longer satisfies the manifests, and `--dry-run` stops it before
//! it installs anything.
//! Its verdict covers the lock only where the probe first establishes that npm reads the lock and
//! the project cooldown reads:
//!
//! - npm is 10.5.1 or newer, since older releases remove `node_modules` despite `--dry-run`;
//! - no `npm-shrinkwrap.json` sits beside the lock, which npm would verify instead;
//! - npm resolves the project to its own root rather than an enclosing workspace, and no
//!   configured `workspace` filter narrows the run to some members.
//!
//! npm compares only versions, so a declaration whose source changed at the same version passes.
//! Its exit 0 therefore proves the lock current only when every local package the lock records
//! still declares what the lock recorded for it, and no `overrides` entry names a source other
//! than the registry, which the lock records nowhere.
//!
//! Every npm the probe runs keeps its cache and logs under cooldown's cache directory, outside the
//! checkout, and stays offline when cooldown does ([`Isolated`]).

use crate::nodecmd::{EnvOverride, NodeCmd};
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    CoreError, LockStatus, LockVerifyReport, Result, ToolTermination, failure_detail,
};
use serde_json::{Map, Value};
use std::process::Output;

const LOCKFILE: &str = "package-lock.json";

/// The lock npm prefers over `package-lock.json` when both exist.
const SHRINKWRAP: &str = "npm-shrinkwrap.json";

/// The oldest npm whose `ci --dry-run` leaves an existing `node_modules` alone.
const MIN_NPM: semver::Version = semver::Version::new(10, 5, 1);

/// The output settings every probe command runs under, pinned so the user's configuration cannot
/// reshape the error report the verdict is read from.
///
/// `workspaces=null` drops a configured `workspaces` switch, while a configured `workspace`
/// filter still applies and is caught by `npm prefix` instead.
/// `--prefix` is left alone because it changes where npm looks for its global configuration.
const PINNED_FLAGS: [&str; 6] = [
    "--loglevel=error",
    "--color=false",
    "--heading=npm",
    "--usage=false",
    "--json=false",
    "--workspaces=null",
];

/// npm's message for a lock that does not satisfy the manifests, in npm 10.5.1 through 11.
const OUT_OF_SYNC: &str = "`npm ci` can only install packages when your package.json and \
                           package-lock.json or npm-shrinkwrap.json are in sync. Please update \
                           your lock file with `npm install` before continuing.";

/// The first line of npm's message for a project without a lock, in npm 10.5.1 through 11.
const NO_LOCK: &str = "The `npm ci` command can only install with an existing package-lock.json or";

/// The install fields npm records for every local package in the lock.
const DECLARATION_FIELDS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

/// Asks npm whether the `package-lock.json` at `root` is current, with npm's cache and logs kept
/// under `scratch`.
///
/// The lock is [`LockStatus::Stale`] only when npm refuses it as out of sync with the manifests,
/// or as missing while no lock exists.
/// A qualification the probe cannot establish is [`LockStatus::Unknown`].
///
/// # Errors
///
/// Returns [`CoreError::ToolSpawn`] if npm cannot be spawned, [`CoreError::Tool`] for any other
/// npm failure, [`CoreError::Filesystem`] if the lock cannot be read, and the lock reader's error
/// for a lock it refuses.
pub(crate) async fn lock_current(
    cmd: &NodeCmd,
    root: &Utf8Path,
    scratch: &Utf8Path,
    offline: bool,
) -> Result<LockVerifyReport> {
    let npm = Isolated::new(cmd, root, scratch, offline);

    // Qualify the lock, the executable, and the scope
    if !matches!(
        std::fs::symlink_metadata(root.join(SHRINKWRAP)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound
    ) {
        return Ok(unknown(&format!(
            "{SHRINKWRAP} exists, and npm verifies it instead of {LOCKFILE}"
        )));
    }
    let version = npm.run(&pinned(&["--version"])).await?;
    if !version.status.success() {
        return Err(npm.failure(&version));
    }
    let version = String::from_utf8_lossy(&version.stdout);
    if !supports_dry_run(&version) {
        return Ok(unknown(&format!(
            "it requires npm >= {MIN_NPM} to verify {LOCKFILE} without touching node_modules, \
             and npm is {}",
            version.trim()
        )));
    }
    let prefix = npm.run(&pinned(&["prefix"])).await?;
    if !prefix.status.success() {
        if error_code(&failure_detail(&prefix)) == Some("ENOWORKSPACES") {
            return Ok(unknown(
                "a configured npm `workspace` filter would narrow `npm ci` to some members",
            ));
        }
        return Err(npm.failure(&prefix));
    }
    let prefix = Utf8PathBuf::from(String::from_utf8_lossy(&prefix.stdout).trim());
    if !same_directory(&prefix, root) {
        return Ok(unknown(&format!(
            "npm runs the project at {prefix}, not at {root}"
        )));
    }

    // Read what the lock records, which npm's exit 0 does not compare
    let lock = read_lock(&root.join(LOCKFILE))?;
    let unproven = match &lock {
        Some(content) => unproven_by_npm(root, &crate::lock::npm_lock_packages(content)?),
        None => None,
    };

    // Run npm's own verification
    let out = npm
        .run(&pinned(&[
            "ci",
            "--dry-run",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
        ]))
        .await?;
    if out.status.success() {
        return Ok(match (&lock, unproven) {
            // A lock written after it was read, such as by a concurrent install, was never compared.
            (None, _) => unknown(&format!("{LOCKFILE} appeared while npm verified it")),
            (Some(_), Some(reason)) => unknown(&reason),
            (Some(_), None) => LockVerifyReport {
                status: LockStatus::Current,
                detail: format!("{LOCKFILE} is current"),
            },
        });
    }
    let refusal = if ToolTermination::from_exit_status(out.status).is_abnormal() {
        None
    } else {
        ci_refusal(&failure_detail(&out))
    };
    match refusal {
        Some(Refusal::OutOfSync(message)) => Ok(stale(root, &message)),
        Some(Refusal::NoLock(message)) if lock.is_none() => Ok(stale(root, &message)),
        _ => Err(npm.failure(&out)),
    }
}

/// `args` followed by [`PINNED_FLAGS`].
fn pinned<'a>(args: &[&'a str]) -> Vec<&'a str> {
    args.iter().chain(&PINNED_FLAGS).copied().collect()
}

/// npm run in a project with its cache and logs under cooldown's cache directory, outside the
/// checkout, and offline when cooldown is, so a project `.npmrc` cannot make it write there.
pub(crate) struct Isolated<'a> {
    cmd: &'a NodeCmd,
    root: &'a Utf8Path,
    flags: Vec<String>,
    /// The cache and logs locations and the offline switch again, for an npm whose `npm.cmd` runs
    /// a helper before it forwards the flags.
    env: Vec<EnvOverride>,
}

impl<'a> Isolated<'a> {
    /// Runs `cmd` in `root` with its cache and logs under `scratch`.
    pub(crate) fn new(
        cmd: &'a NodeCmd,
        root: &'a Utf8Path,
        scratch: &Utf8Path,
        offline: bool,
    ) -> Self {
        let cache = scratch.join("cache");
        let logs = scratch.join("logs");
        let mut flags = vec![format!("--cache={cache}"), format!("--logs-dir={logs}")];
        let mut env = vec![
            EnvOverride {
                name: "npm_config_cache",
                value: cache.into_string().into(),
            },
            EnvOverride {
                name: "npm_config_logs_dir",
                value: logs.into_string().into(),
            },
        ];
        if offline {
            flags.push("--offline".to_string());
            env.push(EnvOverride {
                name: "npm_config_offline",
                value: "true".into(),
            });
            // An empty list allows git no transport, so a git dependency cannot reach the network.
            env.push(EnvOverride {
                name: "GIT_ALLOW_PROTOCOL",
                value: "".into(),
            });
        }
        Isolated {
            cmd,
            root,
            flags,
            env,
        }
    }

    /// Runs npm with `args` and returns its output whatever its exit status.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if npm cannot be spawned.
    pub(crate) async fn run(&self, args: &[&str]) -> Result<Output> {
        let args: Vec<String> = args
            .iter()
            .map(ToString::to_string)
            .chain(self.flags.iter().cloned())
            .collect();
        self.cmd
            .output_overriding(self.root, &args, &self.env)
            .await
    }

    fn failure(&self, out: &Output) -> CoreError {
        CoreError::Tool {
            tool: self.cmd.program().to_string(),
            termination: ToolTermination::from_exit_status(out.status),
            stderr: failure_detail(out),
        }
    }
}

fn unknown(reason: &str) -> LockVerifyReport {
    LockVerifyReport {
        status: LockStatus::Unknown,
        detail: format!("{LOCKFILE} currency cannot be verified: {reason}"),
    }
}

/// The stale verdict, naming the project ahead of npm's own message so a repository with several
/// locks knows which one npm refused.
fn stale(root: &Utf8Path, message: &str) -> LockVerifyReport {
    LockVerifyReport {
        status: LockStatus::Stale,
        detail: format!("{LOCKFILE} is stale in {root}: {message}"),
    }
}

/// Whether the npm that printed `version` for `--version` runs `ci --dry-run` without removing
/// `node_modules`.
fn supports_dry_run(version: &str) -> bool {
    semver::Version::parse(version.trim()).is_ok_and(|version| version >= MIN_NPM)
}

/// Whether `prefix` and `root` name the same directory, through any symbolic link.
fn same_directory(prefix: &Utf8Path, root: &Utf8Path) -> bool {
    match (std::fs::canonicalize(prefix), std::fs::canonicalize(root)) {
        (Ok(prefix), Ok(root)) => prefix == root,
        _ => false,
    }
}

/// The lock's content, or `None` when no file exists at `path`.
///
/// Only a missing entry is an absent lock: an unreadable file or a dangling symbolic link is an
/// error, so npm's missing-lock refusal never reads as stale while some lock is there.
fn read_lock(path: &Utf8Path) -> Result<Option<String>> {
    let filesystem = |err: std::io::Error| CoreError::Filesystem(format!("{path}: {err}"));
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(filesystem(err)),
        Ok(_) => std::fs::read_to_string(path).map(Some).map_err(filesystem),
    }
}

/// The lines of npm's error report in `output`, without the heading npm 10 (`npm ERR!`) or npm 11
/// (`npm error`) prints before each.
fn error_lines(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter_map(|line| {
            ["npm error", "npm ERR!"].iter().find_map(|heading| {
                let rest = line.strip_prefix(heading)?;
                (rest.is_empty() || rest.starts_with(' ')).then_some(rest.trim())
            })
        })
        .collect()
}

/// The code npm's error report in `output` opens with.
fn error_code(output: &str) -> Option<&str> {
    error_lines(output).first()?.strip_prefix("code ")
}

/// npm's refusal of the lock in a failed `npm ci`.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// The lock does not satisfy the manifests, with npm's message and the mismatches it lists.
    OutOfSync(String),
    /// There is no lock, with npm's message.
    NoLock(String),
}

/// The refusal npm's error report in `output` states, if it is one.
///
/// The report must open with the `EUSAGE` code, and its first paragraph must be one of npm's two
/// messages, so neither message quoted elsewhere, such as in a dependency's name, counts.
fn ci_refusal(output: &str) -> Option<Refusal> {
    let lines = error_lines(output);
    let (code, report) = lines.split_first()?;
    if *code != "code EUSAGE" {
        return None;
    }
    let mut paragraphs = report
        .split(|line| line.is_empty())
        .filter(|paragraph| !paragraph.is_empty());
    let message = paragraphs.next()?;
    if *message == [OUT_OF_SYNC] {
        let mismatches = paragraphs.next().unwrap_or_default();
        Some(Refusal::OutOfSync(
            message
                .iter()
                .chain(mismatches)
                .copied()
                .collect::<Vec<_>>()
                .join(" "),
        ))
    } else if message.first() == Some(&NO_LOCK) {
        Some(Refusal::NoLock(message.join(" ")))
    } else {
        None
    }
}

/// Why npm's exit 0 would not prove the lock in `packages` current, if it would not.
///
/// npm records every local package it installs, the root and each workspace member or `file:`
/// target, with its install declarations, and the root with its `workspaces` patterns.
/// Each must still match the package's `package.json`.
fn unproven_by_npm(root: &Utf8Path, packages: &Map<String, Value>) -> Option<String> {
    if !packages.contains_key("") {
        return Some(format!(
            "{LOCKFILE} records no root package; run `npm install`"
        ));
    }
    for (key, recorded) in packages.iter().filter(|(key, _)| is_local_package(key)) {
        let manifest = if key.is_empty() {
            "package.json".to_string()
        } else {
            format!("{key}/package.json")
        };
        let Some(declared) = read_manifest(&root.join(&manifest)) else {
            return Some(format!("{manifest} cannot be read as JSON"));
        };
        let patterns_changed =
            key.is_empty() && workspace_patterns(&declared) != workspace_patterns(recorded);
        if patterns_changed || !same_declarations(&declared, recorded) {
            return Some(format!(
                "{manifest} declares other dependencies than {LOCKFILE} recorded for it; run \
                 `npm install`"
            ));
        }
        // npm reads `overrides` from the root manifest alone.
        let overridden = key
            .is_empty()
            .then(|| {
                declared
                    .get("overrides")
                    .and_then(|overrides| non_registry_override(overrides, &declared))
            })
            .flatten();
        if let Some(spec) = overridden {
            return Some(format!(
                "package.json overrides a dependency with `{spec}`, a source {LOCKFILE} does \
                 not record"
            ));
        }
    }
    None
}

/// Whether a `packages` key names a local package, the root's empty key or a directory outside
/// every `node_modules`.
fn is_local_package(key: &str) -> bool {
    !key.split('/').any(|segment| segment == "node_modules")
}

fn read_manifest(path: &Utf8Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The root's `workspaces` patterns as npm records them, omitted when there are none.
fn workspace_patterns(package: &Value) -> Option<&Value> {
    package.get("workspaces").filter(|patterns| {
        !patterns.is_null() && patterns.as_array().is_none_or(|list| !list.is_empty())
    })
}

/// Whether the `manifest` of a local package declares what its lock entry `recorded` holds, as
/// npm records it: an absent field as an empty one, and a name `optionalDependencies` declares
/// left out of `dependencies`.
fn same_declarations(manifest: &Value, recorded: &Value) -> bool {
    let field = |package: &Value, name: &str| {
        package
            .get(name)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    let optional = field(manifest, "optionalDependencies");
    DECLARATION_FIELDS.iter().all(|name| {
        let mut declared = field(manifest, name);
        if *name == "dependencies" {
            declared.retain(|dependency, _| !optional.contains_key(dependency));
        }
        declared == field(recorded, name)
    })
}

/// The first `overrides` value that names another source than a registry range, rendered as
/// written.
///
/// Such an override can change a package's source at the same version, which npm's check of the
/// lock does not see.
/// A `$name` reference stands for the root `manifest`'s own declaration of `name`, which must
/// then be a range itself.
fn non_registry_override(overrides: &Value, manifest: &Value) -> Option<String> {
    match overrides {
        Value::String(spec) => {
            let resolved = match spec.strip_prefix('$') {
                Some(name) => referenced_spec(manifest, name),
                None => Some(spec.as_str()),
            };
            (!resolved.is_some_and(crate::version::is_range)).then(|| spec.clone())
        }
        Value::Object(nested) => nested
            .values()
            .find_map(|nested| non_registry_override(nested, manifest)),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// The specifier an override's `$name` reference resolves to: the root `manifest`'s first
/// non-empty declaration of `name`, in the field order npm's `Edge` looks it up in.
fn referenced_spec<'a>(manifest: &'a Value, name: &str) -> Option<&'a str> {
    [
        "devDependencies",
        "optionalDependencies",
        "dependencies",
        "peerDependencies",
    ]
    .iter()
    .find_map(|field| {
        manifest
            .get(field)?
            .get(name)?
            .as_str()
            .filter(|spec| !spec.is_empty())
    })
}

#[cfg(test)]
mod tests;
