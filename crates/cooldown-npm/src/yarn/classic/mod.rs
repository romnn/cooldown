//! Exact `upgrade` and `fix` landings in a Yarn Classic project.
//!
//! Yarn Classic keeps the entry its lock records under a selector and resolves a selector it
//! lacks to the newest release the range admits, so no install alone lands a target below a
//! range's maximum.
//! A landing therefore runs in a copy of the project made as a Berry landing makes one
//! ([`verify::resolve_copy`]), in three steps:
//!
//! 1. every install declaration of the package in its declaring members is pinned to the exact
//!    target, and an install records the release's entry;
//! 2. the manifests are restored, and the starting lock gets that entry under each selector the
//!    declaring members request ([`classic_seed`]), a grouped key split so every other selector
//!    keeps its entry;
//! 3. a second install resolves what the target newly requires and prunes what nothing reaches.
//!
//! Yarn's configuration, environment, and credentials stay the user's: the copy keeps every rc
//! file where Yarn Classic looks for it.
//! Every Yarn process runs the admitted Yarn Classic itself, whatever a `yarn-path` names, skips
//! lifecycle scripts, nested installs' included, and writes file contents and directory entries
//! only in the copy's scratch directory, Yarn's own cache, and the devices Node opens
//! ([`confine`]).
//! A setting that sends such a write anywhere else fails the install, so the project gets neither
//! a `node_modules` folder nor `.yarn-integrity`, and no setting needs screening; where the writes
//! cannot be confined, or `YARN_CONFIG` names an rc file the copy cannot place, every candidate is
//! held before Yarn runs ([`declined`]).
//!
//! Each install runs with `--force` on a lock that starts with [`MARKER`], which Yarn drops
//! whenever it writes the lock, so a lock a setting such as `pure-lockfile` kept Yarn from writing
//! still carries it, and nothing is published from it.
//! The copy's lock is written back only once each declaring member binds the target in it, and
//! only while neither the lock nor any other input of the copy changed in the project meanwhile.

mod confine;
#[cfg(all(test, target_os = "linux"))]
mod tests;

pub(super) use confine::available as confinement;
#[cfg(test)]
mod unconfined_tests;

use super::executable;
use super::mutate::{Manifests, pin_in_copy, write};
use super::verify::{self, Copied, ResolveCopy};
use crate::apply::landing::{CandidateLanding, CandidateRunner, OwnedStep};
use crate::lock::{
    MemberResolution, WorkspaceDialect, WorkspaceManifests, Yarn, YarnBindings, classic_seed,
    classic_selector,
};
use crate::manifest::workspaces::YarnEnvironment;
use crate::nodecmd::{EnvOverride, NodeCmd};
use crate::tool::{member_paths, miss_detail};
use crate::version::{RangeMatch, range_match};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    Change, CoreError, Plan, Project, ProjectMutationFile, ProjectMutationJournal,
    ProjectMutationState, Result, SkipReason, Skipped, ToolTermination, failure_detail,
};

/// The lockfile Yarn Classic reads and the landing writes back.
const LOCKFILE: &str = "yarn.lock";

/// The comment each install's lock starts with, which Yarn drops whenever it writes the lock.
const MARKER: &str = "# cooldown: Yarn has not written this lock";

/// The install both steps run: the lock written even when Yarn finds it current, no lifecycle
/// script, and no prompt.
const INSTALL: [&str; 4] = [
    "install",
    "--force",
    "--ignore-scripts",
    "--non-interactive",
];

/// Why a landing holds every candidate before Yarn runs, or `None` when it lands them:
/// `confinement` says Yarn's writes cannot be confined here, or `YARN_CONFIG` names an rc file,
/// which Yarn reads relative to where it starts, a place the copy does not reproduce.
pub(super) fn declined(
    environment: &YarnEnvironment,
    confinement: std::result::Result<(), String>,
) -> Option<String> {
    if let Err(reason) = confinement {
        return Some(reason);
    }
    environment.variable("yarn_config").next().map(|_| {
        "cooldown lands no Yarn Classic version while `YARN_CONFIG` names an rc file, which the \
         copy it resolves in cannot reproduce"
            .to_string()
    })
}

/// The landing each candidate of a Classic apply runs.
pub(super) struct Landing<'a> {
    driver: &'a NodeCmd,
    /// Where copies are made, and the environment Yarn reads in them.
    context: verify::Context,
    /// Why every candidate is held before Yarn runs ([`declined`]), or `None`.
    declined: Option<String>,
    /// Yarn's default cache folder, which every Yarn run of the landing shares, or `None` for a
    /// cache in each copy's scratch directory ([`yarn_cache`]).
    cache: Option<Utf8PathBuf>,
}

impl<'a> Landing<'a> {
    /// A landing for a project whose source lies at `source_root`, or `None` when that is unknown.
    ///
    /// Only a landing that runs Yarn needs a cache, and Yarn's default one only where it is judged
    /// against the source: the root of a copy, such as a preview's, says nothing about it.
    pub(super) fn new(
        driver: &'a NodeCmd,
        context: verify::Context,
        declined: Option<String>,
        source_root: Option<&Utf8Path>,
    ) -> Self {
        let cache = match (&declined, source_root) {
            (None, Some(root)) => yarn_cache(&context.environment, root),
            _ => None,
        };
        Landing {
            driver,
            context,
            declined,
            cache,
        }
    }
}

#[async_trait]
impl CandidateRunner for Landing<'_> {
    /// A candidate is held where the landing declines every one ([`declined`]), when a declaring
    /// member requests no range that admits the target, and when seeding its selectors would move
    /// a binding the candidate does not own ([`conflict`]).
    fn hold(
        &self,
        project: &Project,
        plan: &Plan,
        change: &Change,
        lock: Option<&str>,
    ) -> Result<Option<Skipped>> {
        let skip = |detail: String| {
            Some(Skipped {
                change: change.clone(),
                reason: SkipReason::NotEligible,
                offending: Some(change.package.clone()),
                detail: Some(detail),
            })
        };
        if let Some(reason) = &self.declined {
            return Ok(skip(reason.clone()));
        }
        let Some(lock) = lock else {
            return Ok(skip(format!(
                "{LOCKFILE} is missing, and cooldown lands a Yarn Classic target only in a lock \
                 Yarn wrote"
            )));
        };
        let workspace = WorkspaceManifests::read(&project.root, WorkspaceDialect::Classic);
        Ok(match selectors(&workspace, change) {
            Ok(selectors) => conflict(lock, &workspace, plan, change, &selectors).and_then(skip),
            Err(reason) => skip(reason),
        })
    }

    /// Yarn's own errors, or the reason cooldown refused the landing, are what a reader needs to
    /// see why the candidate stayed where it was.
    fn skip_detail(&self, error: &CoreError) -> Option<String> {
        match error {
            CoreError::Tool { stderr, .. } => Some(format!("Yarn refused the landing: {stderr}")),
            CoreError::UnacceptableResolve(detail) => Some(detail.clone()),
            _ => None,
        }
    }

    /// The landing publishes the lock through the `authorized` write set, which it leaves holding
    /// exactly what the landing wrote, as its postimage.
    async fn land(
        &self,
        project: &Project,
        change: &Change,
        _candidate_journal: &ProjectMutationJournal,
        landing: &CandidateLanding,
        authorized: &ProjectMutationState,
    ) -> Result<OwnedStep> {
        if !matches!(landing, CandidateLanding::Seeded) {
            return Err(CoreError::System(
                "a Yarn Classic landing seeds its selectors; it runs no other transaction".into(),
            ));
        }
        let mut owned = authorized.clone();
        let result = self.seed_target(project, change, &mut owned).await;
        Ok(OwnedStep {
            result,
            postimage: owned,
        })
    }
}

impl Landing<'_> {
    /// Lands `change` in a copy of `project` and publishes the copy's lock through `owned`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Tool`] when Yarn fails, and [`CoreError::UnacceptableResolve`] when an
    /// install leaves the lock unwritten, the pinned one records no entry for the target, or a
    /// declaring member does not bind the target after the seeded one, each leaving the project
    /// untouched; [`CoreError::LockConflict`] when an input of the copy changed in the project
    /// while the copy resolved; and [`CoreError::System`] or [`CoreError::LockUnreadable`] when
    /// the Yarn found is no Yarn Classic, the project cannot be copied, or its lock cannot be
    /// seeded.
    async fn seed_target(
        &self,
        project: &Project,
        change: &Change,
        owned: &mut ProjectMutationState,
    ) -> Result<()> {
        let name = change.package.name.as_str();
        let to = change.to.as_str();
        let workspace = WorkspaceManifests::read(&project.root, WorkspaceDialect::Classic);
        let selectors = selectors(&workspace, change).map_err(CoreError::UnacceptableResolve)?;
        let lock_path = Utf8Path::new(LOCKFILE);
        // The publish below refuses to write once any file differs from `owned`, so this is the
        // lock the landing starts from.
        let started = owned
            .files()
            .iter()
            .find(|file| file.path() == lock_path)
            .and_then(ProjectMutationFile::contents)
            .and_then(|lock| std::str::from_utf8(lock).ok())
            .ok_or_else(|| {
                let path = project.root.join(lock_path);
                CoreError::LockUnreadable(format!("{path} is missing or not UTF-8"))
            })?
            .to_owned();
        let copy = match verify::resolve_copy(project, &started, &self.context)? {
            Copied::Ready(copy) => copy,
            Copied::Declined(reason) => {
                return Err(CoreError::System(format!(
                    "cooldown cannot copy {root} to resolve in: {reason}",
                    root = project.root
                )));
            }
        };
        let version = self.yarn(&copy, &["--version"]).await?;
        if !version.trim().starts_with("1.") {
            return Err(executable::classic_lock_refused(&format!(
                "`{yarn}` runs Yarn {version}",
                yarn = self.driver.program(),
                version = version.trim()
            )));
        }

        // Pinned install, which records the target's entry
        let manifests = Manifests::take(&copy.copy_root)?;
        pin_in_copy(&copy.copy_root, change)?;
        let pinned_lock = self.install(&copy, &started).await?;
        let pinned = format!("{name}@{to}");
        let entry = classic_seed::entry(&pinned_lock, &pinned).ok_or_else(|| {
            CoreError::UnacceptableResolve(format!("Yarn recorded no entry for {pinned}"))
        })?;
        manifests.restore()?;

        // Seeded install, which resolves what the target requires
        let seeded = classic_seed::seed(&started, &selectors, &entry).ok_or_else(|| {
            CoreError::LockUnreadable(format!(
                "{LOCKFILE} holds an entry Yarn Classic's reader does not read"
            ))
        })?;
        let settled = self.install(&copy, &seeded).await?;
        let reached = YarnBindings::read(&settled, &workspace).is_some_and(|bindings| {
            member_paths(change)
                .into_iter()
                .all(|member| bindings.resolution(member, name) == MemberResolution::Resolved(to))
        });
        if !reached {
            let detail = miss_detail::<Yarn>(&settled, Some(&workspace), change);
            return Err(CoreError::UnacceptableResolve(detail));
        }

        // The copy resolved the project as it was when it was made; an input that changed since
        // then is someone else's change, which the landing must neither overwrite nor ignore.
        if verify::live_digest(project, &self.context)?.as_ref() != Some(&copy.digest) {
            return Err(CoreError::LockConflict(format!(
                "the inputs of {LOCKFILE} changed while cooldown resolved {pinned} in a copy, so \
                 the lock was left as it is; re-run once they have settled"
            )));
        }
        owned.write_owned(lock_path, settled.as_bytes())
    }

    /// Runs Yarn Classic's install in `copy` on `lock`, marked as unwritten ([`MARKER`]), and
    /// returns the lock Yarn wrote.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Tool`] when the install fails, and [`CoreError::UnacceptableResolve`]
    /// when it exits without writing the lock.
    async fn install(&self, copy: &ResolveCopy, lock: &str) -> Result<String> {
        let path = copy.copy_root.join(LOCKFILE);
        // Yarn reads a byte-order mark only at the very start.
        let (mark, body) = match lock.strip_prefix('\u{feff}') {
            Some(body) => ("\u{feff}", body),
            None => ("", lock),
        };
        write(&path, format!("{mark}{MARKER}\n{body}").as_bytes())?;
        self.yarn(copy, &INSTALL).await?;
        match std::fs::read_to_string(&path) {
            Ok(written) if !written.contains(MARKER) => Ok(written),
            _ => Err(CoreError::UnacceptableResolve(format!(
                "Yarn exited without writing {LOCKFILE}, as a setting such as `pure-lockfile` \
                 makes it, so the landing left the lock as it was"
            ))),
        }
    }

    /// Runs the admitted Yarn Classic itself with `args` in `copy`, whatever a `yarn-path` names,
    /// with lifecycle scripts off for every install it starts, its temporary files in the scratch
    /// directory, its cache in the landing's cache folder or the scratch directory, and its writes
    /// confined to those and the devices Node opens ([`confine`]), and returns its standard
    /// output.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Tool`] with Yarn's output, its paths relative to the project, when
    /// Yarn fails, [`CoreError::ToolSpawn`] when it cannot be spawned, and
    /// [`CoreError::System`] when its writes cannot be confined.
    async fn yarn(&self, copy: &ResolveCopy, args: &[&str]) -> Result<String> {
        let scratch = copy.scratch();
        let temporary = scratch.join("tmp");
        std::fs::create_dir_all(&temporary)?;
        let mut writable = vec![
            scratch,
            Utf8Path::new("/dev/null"),
            Utf8Path::new("/dev/tty"),
        ];
        let mut overrides = vec![
            EnvOverride {
                name: "YARN_IGNORE_PATH",
                value: "1".into(),
            },
            // The git fetcher's nested install takes no `--ignore-scripts` from the outer one.
            EnvOverride {
                name: "YARN_IGNORE_SCRIPTS",
                value: "true".into(),
            },
            EnvOverride {
                name: "TMPDIR",
                value: temporary.into_string().into(),
            },
        ];
        let cache = self.cache.clone().unwrap_or_else(|| scratch.join("cache"));
        writable.push(&cache);
        overrides.push(EnvOverride {
            name: "YARN_CACHE_FOLDER",
            value: cache.clone().into_string().into(),
        });
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        let program = self.driver.program();
        let mut command = self.driver.command(&copy.copy_root, &args, &overrides);
        confine::confine(&mut command, &writable)?;
        let output = cooldown_adapter_util::supervised_output(&mut command, program, || {
            format!("{program} {}", args.join(" "))
        })
        .await?;
        if !output.status.success() {
            return Err(CoreError::Tool {
                tool: program.to_string(),
                termination: ToolTermination::from_exit_status(output.status),
                stderr: verify::relabel(&failure_detail(&output), &copy.copy_root),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Yarn Classic's default cache folder on Linux, `$XDG_CACHE_HOME/yarn` or `~/.cache/yarn`,
/// resolved through every symbolic link and created when missing, or `None` when a landing must
/// not write there: it cannot be resolved, or it lies inside the source project at `root` or
/// holds it.
///
/// The folder is created only below a directory that resolves outside the project.
fn yarn_cache(environment: &YarnEnvironment, root: &Utf8Path) -> Option<Utf8PathBuf> {
    let caches = match &environment.xdg_cache_home {
        Some(caches) => caches.clone(),
        None => environment.home.as_ref()?.join(".cache"),
    };
    let root = root.canonicalize_utf8().ok()?;
    let clear = |path: &Utf8Path| !path.starts_with(&root) && !root.starts_with(path);
    let cache = caches.canonicalize_utf8().ok()?.join("yarn");
    if !clear(&cache) {
        return None;
    }
    std::fs::create_dir_all(&cache).ok()?;
    let cache = cache.canonicalize_utf8().ok()?;
    clear(&cache).then_some(cache)
}

/// The selectors `change`'s declaring members request, as the lock keys them (`chalk@^4.0.0`), or
/// why no landing can bind them to the target.
///
/// Only a range that admits the target qualifies, since Yarn reuses whatever entry seeding gives a
/// selector, a tag or a protocol reference included.
fn selectors(
    workspace: &WorkspaceManifests,
    change: &Change,
) -> std::result::Result<Vec<String>, String> {
    let name = change.package.name.as_str();
    let to = change.to.as_str();
    if let Some(reason) = workspace.unavailable_reason() {
        return Err(format!(
            "cooldown cannot read which selector each member requests for {name}: {reason}"
        ));
    }
    let mut selectors = Vec::new();
    for member in member_paths(change) {
        let Some(selector) = classic_selector(workspace, member, name) else {
            return Err(format!("{member} declares no install range of {name}"));
        };
        if range_match(selector, to) != RangeMatch::Matches {
            return Err(format!(
                "{member} declares {name} as `{selector}`, no range that admits {to}"
            ));
        }
        let selector = format!("{name}@{selector}");
        if !selectors.contains(&selector) {
            selectors.push(selector);
        }
    }
    Ok(selectors)
}

/// Why seeding `selectors` into `lock` would move a binding `change` does not own, or `None`.
///
/// A Classic selector names one entry for the whole workspace: one that binds another copy than
/// the candidate's carries that copy's other dependents along, and one an excluded member requests
/// moves that member too.
fn conflict(
    lock: &str,
    workspace: &WorkspaceManifests,
    plan: &Plan,
    change: &Change,
    selectors: &[String],
) -> Option<String> {
    let name = change.package.name.as_str();
    let Some(bindings) = YarnBindings::read(lock, workspace) else {
        return Some(format!(
            "cooldown cannot read which copy of {name} each member's selector binds"
        ));
    };
    for member in member_paths(change) {
        if let Some(version) = bindings.bound_version(member, name)
            && version != change.from.as_str()
            && version != change.to.as_str()
        {
            return Some(format!(
                "{member}'s selector binds {name}@{version}, a copy this landing does not own"
            ));
        }
    }
    plan.excluded_members.iter().find_map(|excluded| {
        let selector = classic_selector(workspace, &excluded.path, name)?;
        selectors.contains(&format!("{name}@{selector}")).then(|| {
            format!(
                "the excluded member {path} requests the same `{name}@{selector}`, which Yarn \
                     binds once for the whole workspace",
                path = excluded.path
            )
        })
    })
}
