//! Proves a Bun `bun.lock` current with Bun's own `bun install --frozen-lockfile --dry-run`.
//!
//! A frozen install refuses a lock that no longer satisfies the manifests, and `--dry-run` stops
//! it before it installs anything, so the probe runs live in the project.
//! Its verdict covers the lock only where the probe first establishes that Bun reads the lock and
//! the project cooldown reads, with its install cache outside the checkout:
//!
//! - Bun is a stable 1.4.5 or newer, the oldest release measured to leave the project alone;
//! - the project has a text `bun.lock`, which Bun reads ahead of a binary `bun.lockb`;
//! - no configuration Bun reads disables its install cache, since an uncached run writes into the
//!   project's `node_modules` ([`config`]);
//! - no enclosing directory declares workspaces, which Bun could run the project as a member of;
//! - neither the root manifest nor the lock carries `trustedDependencies` or
//!   `patchedDependencies`, and the lock records no local folder or link dependency, whose own
//!   manifest can change unseen.
//!
//! Bun's frozen check accepts some edits it does not compare, such as a member's version, a
//! removed member, or a new member named like a locked registry package.
//! Its exit 0 therefore proves the lock current only when the workspace members and every
//! member's declarations still match what the lock recorded, and no member lists
//! `trustedDependencies`, which frozen mode does not compare ([`workspace`]).
//!
//! Every Bun the probe runs keeps its cache and temporary files under cooldown's cache directory,
//! outside the checkout, ignores `BUN_OPTIONS`, and stays offline when cooldown does.

mod config;
mod workspace;

use crate::nodecmd::{EnvOverride, NodeCmd};
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    CoreError, LockStatus, LockVerifyReport, Result, ToolTermination, failure_detail,
};
use serde_json::Value;
use std::process::Output;

const LOCKFILE: &str = "bun.lock";

/// The oldest Bun whose frozen dry run is measured to leave the project byte for byte as it was.
const MIN_BUN: semver::Version = semver::Version::new(1, 4, 5);

/// Bun's error line for a lock that does not satisfy the manifests.
const FROZEN: &str = "error: lockfile had changes, but lockfile is frozen";

/// Asks Bun whether the `bun.lock` at `root` is current, with Bun's cache and temporary files kept
/// under `scratch`.
///
/// The lock is [`LockStatus::Stale`] only when Bun's sole error is its frozen-lockfile refusal.
/// A qualification the probe cannot establish is [`LockStatus::Unknown`].
///
/// # Errors
///
/// Returns [`CoreError::ToolSpawn`] if Bun cannot be spawned, [`CoreError::Tool`] for any other
/// Bun failure, and [`CoreError::Filesystem`] if the project root cannot be resolved or the
/// scratch directories cannot be created.
pub(crate) async fn lock_current(
    cmd: &NodeCmd,
    root: &Utf8Path,
    scratch: &Utf8Path,
    offline: bool,
) -> Result<LockVerifyReport> {
    let environment = config::Environment::from_process();
    lock_current_in(cmd, root, scratch, offline, &environment).await
}

/// [`lock_current`] with Bun's user-level configuration located through `environment`.
async fn lock_current_in(
    cmd: &NodeCmd,
    root: &Utf8Path,
    scratch: &Utf8Path,
    offline: bool,
    environment: &config::Environment,
) -> Result<LockVerifyReport> {
    let canonical =
        canonical(root).map_err(|err| CoreError::Filesystem(format!("{root}: {err}")))?;

    // Qualify the lock, the configuration, and the manifests before Bun runs
    let Ok(content) = std::fs::read_to_string(canonical.join(LOCKFILE)) else {
        return Ok(unknown(&format!(
            "no readable text {LOCKFILE} exists, and a binary bun.lockb is not verified"
        )));
    };
    let Ok(lock) = crate::lock::decode_bun(&content) else {
        return Ok(unknown(&format!("{LOCKFILE} cannot be read as JSONC")));
    };
    let Some(manifest) = read_manifest(&canonical.join("package.json")) else {
        return Ok(unknown("package.json cannot be read as JSON"));
    };
    let qualification = config::cache_disabled(&canonical, environment)
        .or_else(|| enclosing_workspace(&canonical))
        .or_else(|| unsupported_feature(&manifest, &lock));
    if let Some(reason) = qualification {
        return Ok(unknown(&reason));
    }
    let unproven = workspace::unproven_by_bun(&canonical, &manifest, &lock);
    // Each directory Bun writes to is qualified itself, since either may be a link into the
    // project.
    let (Some(cache), Some(tmp)) = (
        outside(&canonical, &scratch.join("cache")),
        outside(&canonical, &scratch.join("tmp")),
    ) else {
        return Ok(unknown(&format!(
            "cooldown's cache directory {scratch} leads into the project, where Bun would write \
             its cache"
        )));
    };
    let bun = Isolated::new(cmd, &canonical, &cache, &tmp)?;

    // Qualify the executable
    let revision = bun.run(&["--revision"]).await?;
    if !revision.status.success() {
        return Err(bun.failure(&revision));
    }
    let revision = String::from_utf8_lossy(&revision.stdout);
    if !supports_frozen_dry_run(&revision) {
        return Ok(unknown(&format!(
            "it requires a stable Bun >= {MIN_BUN} to verify {LOCKFILE} without touching the \
             project, and Bun is {}",
            revision.trim()
        )));
    }

    // Run Bun's own verification
    let mut args = vec![
        "install",
        "--frozen-lockfile",
        "--dry-run",
        "--ignore-scripts",
        "--no-progress",
    ];
    if offline {
        args.push("--offline");
    }
    let out = bun.run(&args).await?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    match verdict(ToolTermination::from_exit_status(out.status), &stderr) {
        Verdict::Passed => {
            // A lock rewritten or removed while Bun ran was never the one compared.
            let unchanged = std::fs::read_to_string(canonical.join(LOCKFILE))
                .is_ok_and(|after| after == content);
            Ok(if !unchanged {
                unknown(&format!("{LOCKFILE} changed while Bun verified it"))
            } else if let Some(reason) = unproven {
                unknown(&reason)
            } else {
                LockVerifyReport {
                    status: LockStatus::Current,
                    detail: format!("{LOCKFILE} is current"),
                }
            })
        }
        Verdict::Diagnosed => Ok(unknown(&format!(
            "Bun reported a problem with {LOCKFILE} yet exited successfully: {}",
            failure_detail(&out)
        ))),
        Verdict::Stale => Ok(LockVerifyReport {
            status: LockStatus::Stale,
            detail: format!("{LOCKFILE} is stale in {root}: {}", failure_detail(&out)),
        }),
        Verdict::Failed => Err(bun.failure(&out)),
    }
}

/// Bun run in the project root with its cache and temporary files under cooldown's cache
/// directory, outside the checkout.
struct Isolated<'a> {
    cmd: &'a NodeCmd,
    /// The canonical project root, which is also the run's `PWD`.
    root: &'a Utf8Path,
    env: Vec<EnvOverride>,
}

impl<'a> Isolated<'a> {
    /// Runs `cmd` in the canonical `root`, creating its `cache` and `tmp` directories, both
    /// resolved outside the project.
    ///
    /// `BUN_OPTIONS` is cleared because it injects arguments such as `--cwd` or `--config`.
    /// `PWD` names the canonical root because Bun's Unix lookups walk the ancestors of `$PWD`'s
    /// spelling rather than of the working directory.
    fn new(cmd: &'a NodeCmd, root: &'a Utf8Path, cache: &Utf8Path, tmp: &Utf8Path) -> Result<Self> {
        for directory in [cache, tmp] {
            std::fs::create_dir_all(directory)
                .map_err(|err| CoreError::Filesystem(format!("{directory}: {err}")))?;
        }
        let env = [
            ("BUN_INSTALL_CACHE_DIR", cache.as_str()),
            ("BUN_TMPDIR", tmp.as_str()),
            ("TMPDIR", tmp.as_str()),
            ("TMP", tmp.as_str()),
            ("TEMP", tmp.as_str()),
            ("BUN_CONFIG_SKIP_LOAD_LOCKFILE", "0"),
            ("BUN_OPTIONS", ""),
            ("PWD", root.as_str()),
        ]
        .into_iter()
        .map(|(name, value)| EnvOverride {
            name,
            value: value.into(),
        })
        .collect();
        Ok(Isolated { cmd, root, env })
    }

    /// Runs Bun with `args` and returns its output whatever its exit status.
    async fn run(&self, args: &[&str]) -> Result<Output> {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
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

/// `path` with every symbolic link resolved, spelled without Windows' verbatim prefix.
fn canonical(path: &Utf8Path) -> std::io::Result<Utf8PathBuf> {
    let resolved = dunce::canonicalize(path)?;
    Utf8PathBuf::from_path_buf(resolved)
        .map_err(|path| std::io::Error::other(format!("{} is not valid UTF-8", path.display())))
}

/// `directory` resolved through the symbolic links of its existing ancestors, itself included, or
/// `None` when it lies inside the canonical `root` or cannot be resolved.
///
/// cooldown accepts a cache directory inside the project, and creating a directory there would
/// itself change the project.
fn outside(root: &Utf8Path, directory: &Utf8Path) -> Option<Utf8PathBuf> {
    for ancestor in directory.ancestors() {
        match canonical(ancestor) {
            Ok(resolved) => {
                let resolved = resolved.join(directory.strip_prefix(ancestor).ok()?);
                return (!resolved.starts_with(root)).then_some(resolved);
            }
            // An entry that exists without resolving, such as a dangling link, could lead anywhere.
            Err(_) if std::fs::symlink_metadata(ancestor).is_ok() => return None,
            Err(_) => {}
        }
    }
    None
}

/// Whether the Bun that printed `revision` for `--revision` is a stable release no older than
/// [`MIN_BUN`].
///
/// Bun prints its version with the commit as build metadata (`1.4.5+a7733435c`).
fn supports_frozen_dry_run(revision: &str) -> bool {
    semver::Version::parse(revision.trim()).is_ok_and(|version| {
        version.pre.is_empty()
            && semver::Version::new(version.major, version.minor, version.patch) >= MIN_BUN
    })
}

fn read_manifest(path: &Utf8Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Why an ancestor of the canonical `root` could make Bun run the project as a workspace member,
/// if one could.
///
/// Bun selects a workspace root by walking upward, so any enclosing `package.json` that declares
/// workspaces, or that cannot be read as JSON, leaves the run's scope unproven.
fn enclosing_workspace(root: &Utf8Path) -> Option<String> {
    root.ancestors().skip(1).find_map(|directory| {
        let manifest = directory.join("package.json");
        std::fs::symlink_metadata(&manifest).ok()?;
        read_manifest(&manifest)
            .is_none_or(|document| document.get("workspaces").is_some())
            .then(|| format!("{manifest} may make Bun run the project as a workspace member"))
    })
}

/// Why the root `manifest` or the `lock` uses a feature whose inputs Bun's frozen check does not
/// compare, if either does.
fn unsupported_feature(manifest: &Value, lock: &Value) -> Option<String> {
    for (document, file) in [(manifest, "package.json"), (lock, LOCKFILE)] {
        if document.get("trustedDependencies").is_some() {
            return Some(format!("{file} lists `trustedDependencies`"));
        }
        let patched = document.get("patchedDependencies");
        if patched.is_some_and(|patched| patched.as_object().is_none_or(|map| !map.is_empty())) {
            return Some(format!("{file} lists `patchedDependencies`"));
        }
    }
    let packages = lock.get("packages").and_then(Value::as_object)?;
    packages.values().find_map(|entry| {
        let identity = crate::lock::bun_identity(Some(entry))?;
        let crate::lock::BunIdentity { reference, .. } = crate::lock::BunIdentity::parse(identity)?;
        (reference.starts_with("file:") || reference.starts_with("link:")).then(|| {
            format!(
                "{LOCKFILE} records the local dependency `{identity}`, whose own manifest Bun \
                 does not compare"
            )
        })
    })
}

/// What one frozen dry run's exit and standard error say about the lock.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Bun exited 0 without an error or lock diagnostic.
    Passed,
    /// Bun exited 0 but reported an error or that it did not use the lock.
    Diagnosed,
    /// Bun's frozen-lockfile refusal was its only error.
    Stale,
    /// Any other failure.
    Failed,
}

/// Reads a frozen dry run's `termination` and `stderr`.
///
/// The refusal counts only as a standalone line, after ANSI colors and carriage returns are
/// stripped, beside no other `error:` line.
/// Bun ignores a lock it cannot load and then refuses the frozen install, so its lock-load
/// diagnostics disqualify both the refusal and a success.
fn verdict(termination: ToolTermination, stderr: &str) -> Verdict {
    let lines: Vec<String> = stderr
        .lines()
        .map(|line| crate::lock::strip_ansi(line).trim().to_string())
        .collect();
    let errors: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|line| line.starts_with("error:"))
        .collect();
    let lock_diagnosed = lines.iter().any(|line| {
        line.starts_with("warn: Ignoring lockfile") || line.contains("failed to parse lockfile")
    });
    match termination {
        ToolTermination::ExitCode(0) if errors.is_empty() && !lock_diagnosed => Verdict::Passed,
        ToolTermination::ExitCode(0) => Verdict::Diagnosed,
        ToolTermination::ExitCode(1) if errors == [FROZEN] && !lock_diagnosed => Verdict::Stale,
        _ => Verdict::Failed,
    }
}

#[cfg(test)]
mod tests;
