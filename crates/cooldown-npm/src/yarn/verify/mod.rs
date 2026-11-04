//! Whether a Yarn Berry lock is current, as Yarn itself decides it.
//!
//! Yarn's own lockfile check is the verdict: `yarn install --immutable` regenerates the lock from
//! the manifests and configuration and fails with `YN0028` when a single byte would change.
//! That install also links, writes caches, rewrites an rc file for an older lock, and prunes a
//! project-local cache, so it runs on a strict copy of every input Yarn reads ([`stage`]), never
//! on the project itself.
//!
//! The run is offline: Yarn, Corepack, and git are each kept off the network, and the copy's
//! cache starts empty.
//! A current lock still reaches the verdict, because Yarn resolves every recorded package from
//! the lock and rebuilds local packages from the copy.
//! A lock that needs registry metadata for a descriptor it does not record reaches no verdict
//! and is unknown rather than stale.
//!
//! The verdict is read from the NDJSON report ([`report`]), never from the exit status, which is
//! 1 for every failure, including a fetch the offline run blocks after the lock was proven
//! current.
//! Only `YN0028` makes a lock stale, only reaching the fetch step makes it current, and
//! everything else, a configuration the copy cannot reproduce included, is unknown.

mod profile;
mod report;
mod settings;
mod stage;

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, unix))]
pub(super) use report::captures;
pub(super) use report::errors as report_errors;
pub(super) use stage::{InputDigest, Outputs};

use crate::manifest::workspaces::YarnEnvironment;
use crate::nodecmd::{EnvOverride, NodeCmd};
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, LockStatus, LockVerifyReport, Project, Result};
use profile::Profile;
use report::Verdict;
use stage::{Materialized, Planned, Staged};

/// The lockfile Berry reads and verification copies.
const LOCKFILE: &str = "yarn.lock";

/// The most error lines an unknown verdict quotes.
const QUOTED_ERRORS: usize = 4;

/// The most lock diff lines a stale verdict quotes.
const QUOTED_DIFF: usize = 12;

/// What verification reads from the process: Yarn's environment, and where the copy may go.
#[derive(Debug, Clone)]
pub(super) struct Context {
    /// The environment Yarn reads its configuration from.
    pub(super) environment: YarnEnvironment,
    /// The directories a copy may be created in, in order of preference.
    pub(super) bases: Vec<Utf8PathBuf>,
}

/// A verification's verdict, with the digest of the inputs it covers when it proved the lock
/// current.
#[derive(Debug)]
pub(super) struct Verification {
    pub(super) report: LockVerifyReport,
    pub(super) digest: Option<InputDigest>,
}

/// Verifies the Berry lock of `project` with Yarn, run by `driver` on a copy of the project.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] for a lock the Berry reader refuses,
/// [`CoreError::Filesystem`] when an input cannot be read or the copy cannot be written, and
/// [`CoreError::ToolSpawn`] when `yarn` cannot be spawned.
/// Every other reason not to trust a verdict is a [`LockStatus::Unknown`] report.
pub(super) async fn lock_current(
    driver: &NodeCmd,
    project: &Project,
    context: &Context,
) -> Result<Verification> {
    let lock_path = project.root.join(LOCKFILE);
    let lock_text = std::fs::read_to_string(&lock_path)
        .map_err(|err| CoreError::Filesystem(format!("{lock_path}: {err}")))?;
    let plan = match stage::plan(
        &project.root,
        &lock_text,
        &context.environment,
        &context.bases,
    )? {
        Planned::Ready(plan) => plan,
        Planned::Declined(reason) => return Ok(unknown(&project.root, &reason)),
    };
    let staged = match plan.materialize()? {
        Materialized::Ready(staged) => staged,
        Materialized::Declined(reason) => return Ok(unknown(&project.root, &reason)),
    };
    let Staged {
        scratch,
        copy_root,
        outputs,
        digest,
    } = *staged;
    let root = &project.root;
    let mut overrides = profile::base_overrides(&outputs);
    let probe = probe(driver, &copy_root, &overrides).await?;
    let probed = match probe.and_then(|version| Profile::of(&version)) {
        Ok(probed) => probed,
        Err(reason) => return Ok(unknown(root, &relabel(&reason, &copy_root))),
    };
    overrides.extend(probed.profile.overrides());
    let output = driver
        .output_overriding(&copy_root, &probed.profile.args(), &overrides)
        .await?;
    // Yarn has exited, so nothing reads the copy any more.
    drop(scratch);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let version = &probed.version;
    Ok(match report::classify(&stdout) {
        Verdict::Current => Verification {
            report: LockVerifyReport {
                status: LockStatus::Current,
                detail: format!(
                    "{LOCKFILE} is current (Yarn {version} found nothing to change in a copy of \
                     {root})"
                ),
            },
            digest: Some(digest),
        },
        Verdict::Stale { diff } => Verification {
            report: LockVerifyReport {
                status: LockStatus::Stale,
                detail: relabel(
                    &format!(
                        "{LOCKFILE} is stale in {root}: Yarn {version} would rewrite it \
                         (`yarn install --immutable` fails with YN0028):\n{}",
                        quoted(&diff, QUOTED_DIFF)
                    ),
                    &copy_root,
                ),
            },
            digest: None,
        },
        Verdict::Undecided { errors } => unknown(
            root,
            &relabel(
                &format!(
                    "Yarn {version} reached no verdict offline, so the lock may need a resolution \
                     only the registry can answer, which verification never reaches; run \
                     `yarn install`: {}",
                    quoted(&errors, QUOTED_ERRORS)
                ),
                &copy_root,
            ),
        ),
        Verdict::NoReport => unknown(
            root,
            &relabel(
                &format!(
                    "Yarn {version} printed no install report: {}",
                    first_lines(&format!("{stdout}\n{stderr}"), QUOTED_ERRORS)
                ),
                &copy_root,
            ),
        ),
    })
}

/// A copy of a project, made as a verification makes one ([`stage`]), for Yarn's resolver to
/// change in place of the project.
#[derive(Debug)]
pub(super) struct ResolveCopy {
    #[expect(
        dead_code,
        reason = "the scratch directory lives as long as the copy, which dropping it deletes"
    )]
    scratch: stage::Scratch,
    /// The project's position in the copy, where Yarn runs.
    pub(super) copy_root: Utf8PathBuf,
    /// The folders Yarn writes outside the project, redirected into the scratch directory.
    pub(super) outputs: Outputs,
    /// The digest of the project's inputs the copy holds, which [`live_digest`] gives again only
    /// while none of them changed.
    pub(super) digest: InputDigest,
}

/// Whether a project could be copied for Yarn's resolver, and the copy if it could.
pub(super) enum Copied {
    Ready(Box<ResolveCopy>),
    /// Why the copy could not reproduce what Yarn reads for the project.
    Declined(String),
}

/// Copies `project`, whose `yarn.lock` holds `lock`, for Yarn's resolver.
///
/// The copy holds every input a verification copies, its rc files mirrored where Yarn looks for
/// them, so Yarn resolves in it what it would resolve in the project.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] for a lock the Berry reader refuses, and
/// [`CoreError::Filesystem`] when an input cannot be read or the copy cannot be written.
pub(super) fn resolve_copy(project: &Project, lock: &str, context: &Context) -> Result<Copied> {
    let plan = match stage::plan(&project.root, lock, &context.environment, &context.bases)? {
        Planned::Ready(plan) => plan,
        Planned::Declined(reason) => return Ok(Copied::Declined(reason)),
    };
    Ok(match plan.materialize()? {
        Materialized::Ready(staged) => {
            let Staged {
                scratch,
                copy_root,
                outputs,
                digest,
            } = *staged;
            Copied::Ready(Box::new(ResolveCopy {
                scratch,
                copy_root,
                outputs,
                digest,
            }))
        }
        Materialized::Declined(reason) => Copied::Declined(reason),
    })
}

/// Yarn 4's verdict on the lock `copy` holds, from the same offline immutable install a
/// verification runs: `Ok` when the lock is current for the copy's inputs, or the verdict's
/// detail when it is stale or undecided.
///
/// # Errors
///
/// Returns [`CoreError::ToolSpawn`] when `yarn` cannot be spawned.
pub(super) async fn copy_current(
    driver: &NodeCmd,
    copy: &ResolveCopy,
) -> Result<std::result::Result<(), String>> {
    let mut overrides = profile::base_overrides(&copy.outputs);
    overrides.extend(Profile::Yarn4.overrides());
    let output = driver
        .output_overriding(&copy.copy_root, &Profile::Yarn4.args(), &overrides)
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = match report::classify(&stdout) {
        Verdict::Current => return Ok(Ok(())),
        Verdict::Stale { diff } => format!(
            "Yarn would still rewrite it:\n{}",
            quoted(&diff, QUOTED_DIFF)
        ),
        Verdict::Undecided { errors } => format!(
            "Yarn reached no verdict on it: {}",
            quoted(&errors, QUOTED_ERRORS)
        ),
        Verdict::NoReport => format!(
            "Yarn printed no install report: {}",
            first_lines(
                &format!("{stdout}\n{}", String::from_utf8_lossy(&output.stderr)),
                QUOTED_ERRORS
            )
        ),
    };
    Ok(Err(relabel(&detail, &copy.copy_root)))
}

/// Every file and directory a verification of `project` copies from the source, so a copy that
/// holds them has what Yarn reads for the project: its manifests, each `file:` folder whole, each
/// rc file between the project and the copy's boundary, and the rest [`stage`] names.
///
/// A project the copy cannot reproduce, or whose lock cannot be read, has none to name.
pub(super) fn copied_sources(project: &Project, context: &Context) -> Vec<Utf8PathBuf> {
    let Ok(lock) = std::fs::read_to_string(project.root.join(LOCKFILE)) else {
        return Vec::new();
    };
    match stage::plan(&project.root, &lock, &context.environment, &context.bases) {
        Ok(Planned::Ready(plan)) => plan.copied_sources(),
        Ok(Planned::Declined(_)) | Err(_) => Vec::new(),
    }
}

/// The digest of the inputs a verification of `project` would copy now, or `None` when the
/// project would no longer be verified at all or a screened input changed while it was read.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] or [`CoreError::Filesystem`] when the inputs cannot be
/// read.
pub(super) fn live_digest(project: &Project, context: &Context) -> Result<Option<InputDigest>> {
    let lock_path = project.root.join(LOCKFILE);
    let lock_text = std::fs::read_to_string(&lock_path)
        .map_err(|err| CoreError::Filesystem(format!("{lock_path}: {err}")))?;
    match stage::plan(
        &project.root,
        &lock_text,
        &context.environment,
        &context.bases,
    )? {
        Planned::Ready(plan) => plan.digest(),
        Planned::Declined(_) => Ok(None),
    }
}

/// The version `yarn --version` reports in the copy, or why it reports none.
///
/// # Errors
///
/// Returns [`CoreError::ToolSpawn`] when `yarn` cannot be spawned.
async fn probe(
    driver: &NodeCmd,
    copy_root: &Utf8Path,
    overrides: &[EnvOverride],
) -> Result<std::result::Result<String, String>> {
    let output = driver
        .output_overriding(copy_root, &["--version".to_string()], overrides)
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        return Ok(Ok(stdout.trim().to_string()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(Err(format!(
        "`yarn --version` failed in the verification copy, which keeps Corepack offline: {}",
        first_lines(&format!("{stdout}\n{stderr}"), QUOTED_ERRORS)
    )))
}

/// The report of a lock whose currency verification could not establish, for `reason`.
fn unknown(root: &Utf8Path, reason: &str) -> Verification {
    Verification {
        report: LockVerifyReport {
            status: LockStatus::Unknown,
            detail: format!("{LOCKFILE} currency in {root} is unknown: {reason}"),
        },
        digest: None,
    }
}

/// `text` with each path inside the copy spelled relative to the project, as every reason names
/// a project file, so a diagnostic never names the throwaway copy.
///
/// Yarn may print a path with either separator on Windows, so both are stripped after the copy's
/// root, and the root itself reads `.`.
pub(super) fn relabel(text: &str, copy_root: &Utf8Path) -> String {
    let root = copy_root.as_str();
    text.replace(&format!("{root}/"), "")
        .replace(&format!("{root}\\"), "")
        .replace(root, ".")
}

/// The first `limit` of `lines`, one per line, with a count of the rest.
fn quoted(lines: &[String], limit: usize) -> String {
    let mut quoted: Vec<String> = lines.iter().take(limit).cloned().collect();
    if lines.len() > limit {
        quoted.push(format!("… and {} more", lines.len() - limit));
    }
    quoted.join("\n")
}

/// The first `limit` non-empty lines of `text`, without terminal escapes, joined into one line.
fn first_lines(text: &str, limit: usize) -> String {
    let text = without_escapes(text);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(limit)
        .collect();
    if lines.is_empty() {
        "it printed nothing".to_string()
    } else {
        lines.join(" | ")
    }
}

/// `text` without its terminal escape sequences: an escape followed by `[`, parameters, and a
/// final letter, which Yarn's usage errors carry whenever their library decides to colour them.
fn without_escapes(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(next) = chars.next() {
        if next != '\u{1b}' {
            plain.push(next);
            continue;
        }
        if chars.next() == Some('[') {
            for parameter in chars.by_ref() {
                if parameter.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    plain
}
