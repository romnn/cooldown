//! Exact `upgrade` and `fix` landings in a Yarn Berry project.
//!
//! Berry lands a candidate in two lock-only installs (`yarn install --mode=update-lockfile`)
//! around a descriptor seeding ([`berry_seed`]):
//!
//! 1. every install declaration of the package in its declaring members is pinned to the exact
//!    target, so Yarn resolves the release and records its metadata and checksum;
//! 2. the declarations are restored to the bytes cooldown authorized, and the lock's key for the
//!    release is given each declared descriptor, which Yarn would otherwise bind to the old copy
//!    or resolve afresh to the range's maximum;
//! 3. a second install rewrites the lock in Yarn's canonical form, reusing the seeded bindings.
//!
//! Yarn writes much more than the lock, even with linking skipped: it reformats every workspace
//! manifest, writes its install state, and adds archives to a project-local cache.
//! So both installs run in a copy of the project made as verification makes one
//! ([`verify::resolve_copy`]), every manifest of the copy restored after each, and Yarn's own
//! offline immutable install then proves the settled lock current in the copy.
//! Only then is the copy's lock written back, while every input the copy holds is still as it was
//! copied.
//! The project receives no temporary pin and no other byte from Yarn, and the candidate's own
//! journal rolls the lock back.
//! Yarn's global folder, which holds its metadata cache and archive mirror, stays the user's, so
//! warm caches serve an offline landing, unless a package tree encloses it.
//! Such a folder may lie in the project, or, for a preview copy, in the project it copied, which
//! the copy cannot see: it moves into the scratch.
//!
//! Yarn's native age gate (`npmMinimalAgeGate`, from Yarn 4.10) filters every release it
//! resolves afresh, while a locked release is reused without one.
//! The pinned install therefore approves the exact target, which cooldown's own policy chose,
//! beside every entry Yarn's configuration already approves, and raises the gate to cooldown's
//! project window when that is stricter, so a transitive the target newly pulls in honours the
//! window or the freeze.
//! A transitive the gate quarantines fails the install, and the candidate with it.
//! A Yarn that gates each configured scope by that scope's own gate (Yarn 4.17 on) ignores the
//! raised global gate there, so a landing that newly resolves a release in a scope gated below
//! the window fails too, since cooldown often learns no publish time from a scope's registry.
//!
//! [`berry_seed`]: crate::lock::berry_seed

#[cfg(all(test, unix))]
mod tests;

use super::verify::{self, Copied, ResolveCopy};
use crate::apply::landing::{CandidateLanding, CandidateRunner, OwnedStep};
use crate::lock::berry_seed::{self, Seeding};
use crate::lock::{NameVersion, NodeLock as _, YarnBerry};
use crate::manifest;
use crate::nodecmd::{EnvOverride, NodeCmd};
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_core::{
    Change, CoreError, Plan, Project, ProjectMutationJournal, ProjectMutationState, Result,
    SkipReason, Skipped, ToolTermination, failure_detail,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;

/// The lockfile Berry reads and the landing writes back.
const LOCKFILE: &str = "yarn.lock";

/// The manifest fields Yarn's workspace resolver installs for the member itself, merged into the
/// one descriptor per name its lock records.
const INSTALL_FIELDS: [&str; 3] = ["dependencies", "devDependencies", "optionalDependencies"];

/// The first Yarn whose configuration has the age gate settings, which every earlier one refuses.
const FIRST_GATED: semver::Version = semver::Version::new(4, 10, 0);

/// The most error lines a failed install's detail quotes.
const QUOTED_ERRORS: usize = 6;

/// The landing each candidate of a Berry apply runs, with what the apply established once.
pub(super) struct Landing<'a> {
    pub(super) driver: &'a NodeCmd,
    /// Where copies are made, and the environment Yarn reads in them.
    pub(super) context: verify::Context,
    pub(super) settings: Settings,
}

/// The settings of Yarn's effective configuration in a project that a landing works around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Settings {
    /// Yarn's effective age gate, or `None` for a Yarn without one.
    pub(super) gate: Option<NativeGate>,
    /// Whether the landing keeps Yarn's global folder, which holds its metadata cache and mirror,
    /// in its copy's scratch directory, because the folder lies in a package tree
    /// ([`in_package_tree`]).
    pub(super) isolate_global: bool,
}

/// Yarn's effective age gate in a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NativeGate {
    /// Every `npmPreapprovedPackages` entry.
    pub(super) preapproved: Vec<String>,
    /// The global `npmMinimalAgeGate`, in minutes.
    pub(super) minutes: u64,
    /// The `npmMinimalAgeGate` of each configured scope, by scope name without its `@`, for a
    /// Yarn that gates a configured scope by its own gate instead of the global one.
    pub(super) scopes: BTreeMap<String, u64>,
}

/// The first Yarn that gates every configured scope by that scope's own `npmMinimalAgeGate`.
const FIRST_SCOPED: semver::Version = semver::Version::new(4, 17, 0);

/// One setting as `yarn config --json` reports it.
///
/// A map setting such as `npmScopes` reads as null when no source sets it and as an empty object
/// otherwise, whatever it holds, so it is read with `yarn config get` instead.
#[derive(serde::Deserialize)]
struct Reported {
    key: String,
    #[serde(default)]
    effective: serde_json::Value,
}

impl Settings {
    /// Yarn `version`'s effective settings in the project at `root`, from `yarn config --json`
    /// and, for a Yarn that gates scopes by their own gates, `yarn config get npmScopes --json`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::System`] when Yarn cannot report its settings, or approves an entry
    /// containing a comma, which the approval the landing passes through the environment cannot
    /// spell; returns [`CoreError::ToolSpawn`] when `yarn` cannot be spawned.
    pub(super) async fn query(
        driver: &NodeCmd,
        root: &Utf8Path,
        version: &semver::Version,
    ) -> Result<Self> {
        let refused = |detail: &str| {
            CoreError::System(format!(
                "cooldown cannot read Yarn's configuration in {root}, which a landing works \
                 around: {detail}"
            ))
        };
        let args = ["config", "--json"].map(ToString::to_string);
        let stdout = driver.stdout(root, &args).await.map_err(|err| match err {
            CoreError::Tool { stderr, .. } => refused(&stderr),
            other => other,
        })?;
        let reported: BTreeMap<String, Reported> = stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Reported>(line).ok())
            .map(|setting| (setting.key.clone(), setting))
            .collect();
        let setting = |key: &str| {
            reported
                .get(key)
                .ok_or_else(|| refused(&format!("it reports no `{key}`")))
        };
        let isolate_global = setting("globalFolder")?
            .effective
            .as_str()
            .is_none_or(|folder| in_package_tree(root, Utf8Path::new(folder)));
        let release = semver::Version::new(version.major, version.minor, version.patch);
        let gate = if release < FIRST_GATED {
            None
        } else {
            let preapproved: Vec<String> =
                serde_json::from_value(setting("npmPreapprovedPackages")?.effective.clone())
                    .map_err(|err| refused(&err.to_string()))?;
            if let Some(entry) = preapproved.iter().find(|entry| entry.contains(',')) {
                return Err(CoreError::System(format!(
                    "Yarn approves `{entry}` in npmPreapprovedPackages, whose `,` the approval of \
                     a landed target cannot carry, so cooldown does not change this lock; spell \
                     the entry without a comma"
                )));
            }
            let minutes = setting("npmMinimalAgeGate")?
                .effective
                .as_u64()
                .ok_or_else(|| refused("its npmMinimalAgeGate is no number of minutes"))?;
            let scopes = if release < FIRST_SCOPED {
                BTreeMap::new()
            } else {
                let args = ["config", "get", "npmScopes", "--json"].map(ToString::to_string);
                let stdout = driver.stdout(root, &args).await.map_err(|err| match err {
                    CoreError::Tool { stderr, .. } => refused(&stderr),
                    other => other,
                })?;
                scope_gates(stdout.trim())
                    .ok_or_else(|| refused("a scope's npmMinimalAgeGate is no number of minutes"))?
            };
            Some(NativeGate {
                preapproved,
                minutes,
                scopes,
            })
        };
        Ok(Settings {
            gate,
            isolate_global,
        })
    }
}

impl NativeGate {
    /// The first release other than the `target` that `settled` resolves and `started` does not,
    /// in a scope whose own gate is below the window's `floor` in minutes, or `None`.
    ///
    /// A Yarn that gates each configured scope by its own `npmMinimalAgeGate` ignores the raised
    /// global gate there, and a scope's releases often come from a registry that tells cooldown
    /// no publish time, so no later gate would catch a release Yarn picked too young.
    /// The target is cooldown's own choice, which its policy already judged.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockUnreadable`] when either lock cannot be read.
    fn ungated_release(
        &self,
        started: &str,
        settled: &str,
        target: &str,
        floor: u64,
    ) -> Result<Option<String>> {
        if self.scopes.values().all(|minutes| *minutes >= floor) {
            return Ok(None);
        }
        let release = |entry: NameVersion| format!("{}@{}", entry.name, entry.version);
        let before: HashSet<String> = YarnBerry::parse(started)?
            .into_iter()
            .map(release)
            .collect();
        Ok(YarnBerry::parse(settled)?
            .into_iter()
            .map(release)
            .filter(|release| release != target && !before.contains(release))
            .find(|release| {
                release
                    .strip_prefix('@')
                    .and_then(|scoped| scoped.split_once('/'))
                    .and_then(|(scope, _)| self.scopes.get(scope))
                    .is_some_and(|minutes| *minutes < floor)
            }))
    }
}

/// The gate of each scope in `yarn config get npmScopes --json`'s output, or `None` when the
/// output or a scope's gate is no number.
///
/// Yarn prints `undefined` when no source sets the map.
fn scope_gates(output: &str) -> Option<BTreeMap<String, u64>> {
    if output == "undefined" {
        return Some(BTreeMap::new());
    }
    let scopes: serde_json::Map<String, serde_json::Value> = serde_json::from_str(output).ok()?;
    scopes
        .iter()
        .map(|(scope, settings)| {
            let gate = settings.get(GATE_KEY)?.as_u64()?;
            Some((scope.clone(), gate))
        })
        .collect()
}

/// The age gate setting, global and per scope.
const GATE_KEY: &str = "npmMinimalAgeGate";

/// Whether the folder `path` lies at or below the project `root` or any directory holding a
/// `package.json`, as spelled or once canonical.
///
/// A preview runs in a copy whose root is not the project's, so the project it copied is found
/// by its manifest: a folder inside it has an ancestor holding one, whether an rc file names it,
/// a variable such as `XDG_DATA_HOME` leads Yarn's default there, or the project is the home
/// directory itself.
/// A folder some unrelated package tree encloses is kept out of the copy just the same, which
/// costs only a cold cache.
fn in_package_tree(root: &Utf8Path, path: &Utf8Path) -> bool {
    let canonical = |path: &Utf8Path| {
        dunce::canonicalize(path)
            .ok()
            .and_then(|path| camino::Utf8PathBuf::from_path_buf(path).ok())
    };
    // A folder Yarn has not created yet is judged by its nearest existing ancestor.
    let resolved = path.ancestors().find_map(|ancestor| {
        canonical(ancestor).map(|canonical| {
            let rest = path.strip_prefix(ancestor).unwrap_or(Utf8Path::new(""));
            canonical.join(rest)
        })
    });
    let enclosed = |path: &Utf8Path| {
        path.starts_with(root)
            || canonical(root).is_some_and(|root| path.starts_with(root))
            || path
                .ancestors()
                .any(|ancestor| ancestor.join("package.json").is_file())
    };
    enclosed(path) || resolved.is_some_and(|resolved| enclosed(&resolved))
}

#[async_trait]
impl CandidateRunner for Landing<'_> {
    /// A candidate is held when a declaration has no descriptor seeding can bind to the target,
    /// or when binding its descriptors would move a copy the candidate does not own
    /// ([`berry_seed::conflicts`]).
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
        let ranges = match declared_ranges(&project.root, change)? {
            Declared::Ranges(ranges) => ranges,
            Declared::Held(reason) => return Ok(skip(reason)),
        };
        let Some(lock) = lock else {
            return Ok(skip(format!(
                "{LOCKFILE} is missing, and cooldown lands a Yarn Berry target only in a lock Yarn \
                 proved current"
            )));
        };
        let excluded: HashSet<String> = plan
            .excluded_members
            .iter()
            .map(|member| member.path.clone())
            .collect();
        let seeding = Seeding {
            name: &change.package.name,
            version: change.to.as_str(),
            ranges: &ranges,
        };
        let conflicts = berry_seed::conflicts(lock, &seeding, change.from.as_str(), &excluded)?;
        if conflicts.is_empty() {
            Ok(None)
        } else {
            Ok(skip(format!(
                "landing {name}@{to} would move what it does not own: {}",
                conflicts.join("; "),
                name = change.package.name,
                to = change.to
            )))
        }
    }

    /// Yarn's own errors, or the reason cooldown refused the settled lock, are what a reader
    /// needs to see why the candidate stayed where it was.
    fn skip_detail(&self, error: &CoreError) -> Option<String> {
        match error {
            CoreError::Tool { stderr, .. } => Some(format!("Yarn refused the landing: {stderr}")),
            CoreError::UnacceptableResolve(detail) => Some(detail.clone()),
            _ => None,
        }
    }

    /// A landing that fails has written nothing, so its postimage is the `authorized` state
    /// cooldown's own edits left: restoring it never reaches over a change someone else made to
    /// the project meanwhile, which the restore then refuses as a conflict.
    async fn land(
        &self,
        project: &Project,
        change: &Change,
        candidate_journal: &ProjectMutationJournal,
        landing: &CandidateLanding,
        authorized: &ProjectMutationState,
    ) -> Result<OwnedStep> {
        if !matches!(landing, CandidateLanding::Seeded) {
            return Err(CoreError::System(
                "a Yarn Berry landing seeds its descriptors; it runs no other transaction".into(),
            ));
        }
        match self.seed_target(project, change).await {
            Ok(()) => OwnedStep::capture(Ok(()), candidate_journal),
            Err(err) => Ok(OwnedStep {
                result: Err(err),
                postimage: authorized.clone(),
            }),
        }
    }
}

impl Landing<'_> {
    /// Lands `change` in a copy of `project` and writes the copy's lock back.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Tool`] when either install fails and
    /// [`CoreError::UnacceptableResolve`] when Yarn does not prove the settled lock current or
    /// newly resolved a release under a scope gate below cooldown's window, each leaving the
    /// project untouched; [`CoreError::LockConflict`] when an input of the copy
    /// changed in the project while the copy resolved; and [`CoreError::System`] or
    /// [`CoreError::LockUnreadable`] when the project cannot be copied or the pinned lock cannot
    /// be seeded.
    async fn seed_target(&self, project: &Project, change: &Change) -> Result<()> {
        let name = change.package.name.as_str();
        let to = change.to.as_str();
        let ranges = match declared_ranges(&project.root, change)? {
            Declared::Ranges(ranges) => ranges,
            Declared::Held(reason) => return Err(CoreError::System(reason)),
        };
        let lock_path = project.root.join(LOCKFILE);
        let started = read(&lock_path)?;
        let copy = match verify::resolve_copy(project, &started, &self.context)? {
            Copied::Ready(copy) => copy,
            Copied::Declined(reason) => {
                return Err(CoreError::System(format!(
                    "cooldown cannot copy {root} to resolve in: {reason}",
                    root = project.root
                )));
            }
        };
        // Yarn rewrites every manifest it reads, so each install is followed by a restore of the
        // bytes cooldown authorized, which the next step must read.
        let manifests = Manifests::take(&copy.copy_root)?;
        pin_in_copy(&copy.copy_root, change)?;
        let copy_lock = copy.copy_root.join(LOCKFILE);
        write(
            &copy_lock,
            berry_seed::without_local_checksums(&started)?.as_bytes(),
        )?;
        let floor = floor_minutes(project.exclude_newer.as_deref(), jiff::Timestamp::now());
        let overrides = self.overrides(&copy, floor, name, to);
        self.install(project, &copy, &overrides).await?;
        manifests.restore()?;
        let seeding = Seeding {
            name,
            version: to,
            ranges: &ranges,
        };
        let seeded = berry_seed::seed(&read(&copy_lock)?, &seeding)?;
        write(&copy_lock, seeded.as_bytes())?;
        self.install(project, &copy, &overrides).await?;
        manifests.restore()?;
        if let Err(detail) = verify::copy_current(self.driver, &copy, &project.root).await? {
            return Err(CoreError::UnacceptableResolve(format!(
                "Yarn does not prove the lock that lands {name}@{to} current: {detail}"
            )));
        }
        let settled = read(&copy_lock)?;
        let ungated = match (&self.settings.gate, floor) {
            (Some(gate), Some(floor)) => {
                gate.ungated_release(&started, &settled, &format!("{name}@{to}"), floor)?
            }
            _ => None,
        };
        if let Some(release) = ungated {
            return Err(CoreError::UnacceptableResolve(format!(
                "landing {name}@{to} newly resolves {release}, whose scope Yarn gates by less than \
                 cooldown's window, which cooldown cannot hold that scope's releases to"
            )));
        }
        // The copy resolved the project as it was when it was made; an input that changed since
        // then is someone else's change, which the landing must neither overwrite nor ignore.
        if verify::live_digest(project, &self.context)?.as_ref() != Some(&copy.digest) {
            return Err(CoreError::LockConflict(format!(
                "the inputs of {lock_path} changed while cooldown resolved {name}@{to} in a copy, \
                 so the lock was left as it is; re-run once they have settled"
            )));
        }
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), settled.as_bytes())
    }

    /// Runs Yarn's lock-only install in `copy` under `overrides`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Tool`] with Yarn's errors, spelled against the project, when the
    /// install fails, and [`CoreError::ToolSpawn`] when `yarn` cannot be spawned.
    async fn install(
        &self,
        project: &Project,
        copy: &ResolveCopy,
        overrides: &[EnvOverride],
    ) -> Result<()> {
        let args = ["install", "--mode=update-lockfile", "--json"].map(ToString::to_string);
        let output = self
            .driver
            .output_overriding(&copy.copy_root, &args, overrides)
            .await?;
        if output.status.success() {
            return Ok(());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let errors = verify::report_errors(&stdout);
        let detail = if errors.is_empty() {
            failure_detail(&output)
        } else {
            let mut quoted: Vec<String> = errors.iter().take(QUOTED_ERRORS).cloned().collect();
            if errors.len() > QUOTED_ERRORS {
                quoted.push(format!("… and {} more", errors.len() - QUOTED_ERRORS));
            }
            quoted.join("\n")
        };
        Err(CoreError::Tool {
            tool: "yarn".to_string(),
            termination: ToolTermination::from_exit_status(output.status),
            stderr: verify::relabel(&detail, &copy.copy_root, &project.root),
        })
    }

    /// The settings both installs of a landing of `name@to` run under in `copy`, with the
    /// window's `floor` in minutes.
    ///
    /// Yarn's network and offline mode stay the user's, and so does its global folder outside
    /// every package tree, so its metadata cache and mirror serve a landing as they serve any
    /// install.
    /// Everything else Yarn writes outside the project goes into the copy's scratch directory, the
    /// cache included, so no archive reaches a cache the project keeps.
    fn overrides(
        &self,
        copy: &ResolveCopy,
        floor: Option<u64>,
        name: &str,
        to: &str,
    ) -> Vec<EnvOverride> {
        let outputs = &copy.outputs;
        let mut overrides = vec![
            set("YARN_ENABLE_TELEMETRY", "0"),
            set("YARN_ENABLE_COLORS", "0"),
            // Yarn 4 turns hardened mode on for a public pull request, which refreshes every
            // locked package from the registry instead of reusing the verified lock.
            set("YARN_ENABLE_HARDENED_MODE", "0"),
            set("YARN_ENABLE_GLOBAL_CACHE", "0"),
            set("YARN_ENABLE_IMMUTABLE_CACHE", "0"),
            set("YARN_CACHE_FOLDER", outputs.cache.as_str()),
            set("YARN_INSTALL_STATE_PATH", outputs.install_state.as_str()),
            set("YARN_VIRTUAL_FOLDER", outputs.virtual_folder.as_str()),
            set("YARN_PNP_UNPLUGGED_FOLDER", outputs.unplugged.as_str()),
        ];
        if self.settings.isolate_global {
            overrides.push(set("YARN_GLOBAL_FOLDER", outputs.global.as_str()));
        }
        if let Some(gate) = &self.settings.gate {
            let mut approved = gate.preapproved.clone();
            approved.push(format!("{name}@{to}"));
            overrides.push(set("YARN_NPM_PREAPPROVED_PACKAGES", &approved.join(",")));
            if let Some(floor) = floor
                && floor > gate.minutes
            {
                overrides.push(set("YARN_NPM_MINIMAL_AGE_GATE", &floor.to_string()));
            }
        }
        overrides
    }
}

/// The age, in whole minutes rounded up, below which cooldown's project window `cutoff` refuses
/// a release at `now`, or `None` when the window refuses none.
fn floor_minutes(cutoff: Option<&str>, now: jiff::Timestamp) -> Option<u64> {
    let instant: jiff::Timestamp =
        crate::apply::landing::absolute_cutoff_from_project(cutoff, now)?
            .parse()
            .ok()?;
    let seconds = now.duration_since(instant).as_secs();
    let seconds = u64::try_from(seconds).ok().filter(|seconds| *seconds > 0)?;
    Some(seconds.div_ceil(60))
}

/// What the declaring members of a change declare it as, after cooldown's authorized edits.
enum Declared {
    /// The descriptor ranges seeding binds to the target, one per distinct range.
    Ranges(Vec<String>),
    /// Why no landing can bind the declarations to the target.
    Held(String),
}

/// The descriptor ranges every install declaration of `change`'s package in its declaring
/// members resolves under ([`berry_seed::seed_range`]), read from the manifests under `root`.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`] or [`CoreError::Parse`] when a manifest cannot be read.
fn declared_ranges(root: &Utf8Path, change: &Change) -> Result<Declared> {
    let name = change.package.name.as_str();
    let to = change.to.as_str();
    let mut ranges = BTreeSet::new();
    for rel in manifest::manifest_rels(&change.members) {
        let path = root.join(&rel);
        let document = manifest_document(&path)?;
        let mut member: BTreeSet<String> = BTreeSet::new();
        for (field, specifier) in install_declarations(&document, name) {
            match berry_seed::seed_range(specifier, to) {
                Ok(range) => {
                    member.insert(range);
                }
                Err(reason) => {
                    return Ok(Declared::Held(format!(
                        "the declaration of {name} in {field} of {manifest} cannot be landed \
                         exactly: {reason}",
                        manifest = manifest::display_rel(&rel)
                    )));
                }
            }
        }
        if member.len() > 1 {
            return Ok(Declared::Held(format!(
                "{manifest} declares {name} at several ranges, of which Yarn binds only the last \
                 field's",
                manifest = manifest::display_rel(&rel)
            )));
        }
        ranges.extend(member);
    }
    if ranges.is_empty() {
        return Ok(Declared::Held(format!(
            "no declaring member installs {name} itself, so no declaration of it binds a copy"
        )));
    }
    Ok(Declared::Ranges(ranges.into_iter().collect()))
}

/// Every manifest of a copy with the bytes it was copied with, which the copy's manifests are
/// restored to after each install.
struct Manifests {
    files: Vec<CopiedFile>,
}

/// One file of a copy and the bytes it was copied with.
struct CopiedFile {
    path: camino::Utf8PathBuf,
    bytes: Vec<u8>,
}

impl Manifests {
    /// Every `package.json` below `copy_root`, which holds only the inputs a copy stages.
    fn take(copy_root: &Utf8Path) -> Result<Self> {
        let mut files = Vec::new();
        let mut directories = vec![copy_root.to_path_buf()];
        while let Some(directory) = directories.pop() {
            let entries =
                std::fs::read_dir(&directory).map_err(|err| filesystem(&directory, &err))?;
            for entry in entries {
                let entry = entry.map_err(|err| filesystem(&directory, &err))?;
                let Ok(path) = camino::Utf8PathBuf::from_path_buf(entry.path()) else {
                    continue;
                };
                let kind = entry.file_type().map_err(|err| filesystem(&path, &err))?;
                if kind.is_dir() {
                    directories.push(path);
                } else if kind.is_file() && path.file_name() == Some("package.json") {
                    let bytes = std::fs::read(&path).map_err(|err| filesystem(&path, &err))?;
                    files.push(CopiedFile { path, bytes });
                }
            }
        }
        Ok(Manifests { files })
    }

    /// Writes every manifest's copied bytes back.
    fn restore(&self) -> Result<()> {
        for CopiedFile { path, bytes } in &self.files {
            write(path, bytes)?;
        }
        Ok(())
    }
}

/// Pins every install declaration of `change`'s package in its declaring members' manifests in
/// the copy at `copy_root` to the exact target.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`] or [`CoreError::Parse`] when a manifest cannot be read or
/// written, and [`CoreError::System`] when a declaration cannot be rewritten in place.
fn pin_in_copy(copy_root: &Utf8Path, change: &Change) -> Result<()> {
    let name = change.package.name.as_str();
    let to = change.to.as_str();
    for rel in manifest::manifest_rels(&change.members) {
        let path = copy_root.join(&rel);
        let bytes = std::fs::read(&path).map_err(|err| filesystem(&path, &err))?;
        let original = String::from_utf8(bytes.clone())
            .map_err(|err| CoreError::Parse(format!("{path}: {err}")))?;
        let document: serde_json::Value = serde_json::from_str(&original)
            .map_err(|err| CoreError::Parse(format!("{path}: {err}")))?;
        let mut pinned = original;
        for (field, specifier) in install_declarations(&document, name) {
            pinned = manifest::replace_declared_range(&pinned, field, name, specifier, to)
                .ok_or_else(|| {
                    CoreError::System(format!(
                        "cooldown cannot pin {name} in {field} of {path} for the landing"
                    ))
                })?;
        }
        if pinned.as_bytes() != bytes.as_slice() {
            write(&path, pinned.as_bytes())?;
        }
    }
    Ok(())
}

/// Every install field of the manifest `document` declaring `name`, with its specifier.
fn install_declarations<'a>(
    document: &'a serde_json::Value,
    name: &str,
) -> impl Iterator<Item = (&'static str, &'a str)> {
    INSTALL_FIELDS.into_iter().filter_map(move |field| {
        let specifier = document.get(field)?.get(name)?.as_str()?;
        Some((field, specifier))
    })
}

/// The manifest at `path` as JSON.
fn manifest_document(path: &Utf8Path) -> Result<serde_json::Value> {
    let content = std::fs::read_to_string(path).map_err(|err| filesystem(path, &err))?;
    serde_json::from_str(&content).map_err(|err| CoreError::Parse(format!("{path}: {err}")))
}

/// The text of the file at `path`.
fn read(path: &Utf8Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|err| filesystem(path, &err))
}

fn write(path: &Utf8Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).map_err(|err| filesystem(path, &err))
}

fn filesystem(path: &Utf8Path, err: &std::io::Error) -> CoreError {
    CoreError::Filesystem(format!("{path}: {err}"))
}

fn set(name: &'static str, value: &str) -> EnvOverride {
    EnvOverride {
        name,
        value: OsString::from(value),
    }
}
