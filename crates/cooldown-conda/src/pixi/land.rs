//! Landing a batch of planned changes in a pixi project: one `pixi lock` in a scratch copy whose
//! manifest pins each target at the top level under cooldown's cutoff, verified against the lock it
//! started from, publishing only `pixi.lock`.
//!
//! The promised moves come from that lock alone; pixi owns how features, constraints and solve
//! groups combine, and the comparison of the two locks decides what is published.

use super::lock::{self, Binding, Kind, Lock, PUBLIC_PYPI, Slot, normal};
use super::manifest::{Location, Manifest, lexical};
use super::{LOCKFILE, PIXI_ID, kind_of, registry_of};
use crate::registry::CONDA;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::tree::utf8;
use cooldown_adapter_util::{Driver, canonical_path};
use cooldown_core::{
    ApplyReport, Change, CoreError, PackageId, Plan, Project, ProjectMutationFile,
    ProjectMutationState, Result, SkipReason, Skipped, UpdateKind, Version, failure_detail,
};
use cooldown_uv::version;
use jiff::Timestamp;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::Hasher as _;
use toml_edit::InlineTable;

/// The `find-links` directories of `manifest`, at the paths pixi reads them by below `root`.
///
/// A copy reproduces a directory and its entries where their real paths put them, so one reached
/// through a link, or an entry that is one, would sit elsewhere than pixi looks.
///
/// # Errors
///
/// Returns why a copy cannot hold one at that path: it is absolute or a `file:` URL, it does not
/// exist or cannot be read, or it or one of its entries is a link.
pub(super) fn find_links(
    root: &Utf8Path,
    manifest: &Manifest,
) -> std::result::Result<Vec<Utf8PathBuf>, String> {
    let mut directories = Vec::new();
    for link in manifest.find_links()? {
        let path = lexical(&root.join(link));
        let unreadable =
            |err: std::io::Error| format!("the find-links directory {link} cannot be read: {err}");
        if canonical_path(&path).map_err(unreadable)? != path {
            return Err(format!(
                "the find-links directory {link} is reached through a link, which cooldown \
                 cannot copy where pixi reads it"
            ));
        }
        for entry in path.read_dir_utf8().map_err(unreadable)? {
            let entry = entry.map_err(unreadable)?;
            if entry.file_type().map_err(unreadable)?.is_symlink() {
                return Err(format!(
                    "the find-links directory {link} holds the link {}, which cooldown cannot \
                     copy where pixi reads it",
                    entry.file_name()
                ));
            }
        }
        directories.push(path);
    }
    Ok(directories)
}

/// The files pixi reads to lock the project at `root`: its manifest, its `.pixi/config.toml`, and
/// the files directly in the `find-links` directories `links`, since a flat index lists its own
/// entries alone.
///
/// A preview copies these, and the directories themselves, as an apply's scratch copy does, so
/// pixi locks the same project in both.
///
/// # Errors
///
/// Returns the error of a directory that cannot be read.
pub(super) fn inputs(
    root: &Utf8Path,
    manifest: &Manifest,
    links: &[Utf8PathBuf],
) -> Result<Vec<Utf8PathBuf>> {
    let mut files = vec![root.join(manifest.file)];
    let config = root.join(".pixi").join("config.toml");
    if config.is_file() {
        files.push(config);
    }
    for link in links {
        for entry in link.read_dir_utf8()? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                files.push(entry.into_path());
            }
        }
    }
    let lock = root.join(LOCKFILE);
    files.retain(|file| *file != lock);
    files.sort();
    files.dedup();
    Ok(files)
}

/// The input pixi would read in place of a `pyproject.toml` manifest, whose absence the copy
/// records too, so one that appears while it locks is noticed; a `.pixi/config.toml` that appears
/// is a new input of [`inputs`] already.
fn optional_input(root: &Utf8Path) -> Utf8PathBuf {
    root.join("pixi.toml")
}

/// A change cooldown can force, and where it lands.
struct Target<'a> {
    change: &'a Change,
    kind: Kind,
    /// Every binding of the package, each at the version the change moves from.
    slots: Vec<&'a Slot>,
    /// For a PyPI package, the top-level requirements to narrow.
    requirements: Vec<Location>,
    /// The newest upload the target may land, as the planner admitted it.
    cutoff: Option<Timestamp>,
}

/// What a batch lands, decided before pixi runs.
struct Landing<'a> {
    targets: Vec<Target<'a>>,
    held: Vec<Skipped>,
    /// The scratch manifest, forcing every target and the cutoff.
    forced: Manifest,
}

/// What the lock a landing produced holds for publication.
#[derive(Debug)]
enum Verdict {
    /// The lock lands the targets, with the version moves no change planned as collateral rows.
    Accepted(Vec<Change>),
    /// The lock binds a record cooldown cannot model, which holds the whole workspace.
    Unsupported(String),
}

/// Lands `plan` in `project` and publishes the new lock through `owned`, the write set as it was
/// prepared, which it leaves holding exactly what the landing wrote.
///
/// The batch resolves at once, so moves that are only valid together land together, and a lock
/// cooldown cannot accept fails the batch with [`CoreError::UnacceptableResolve`] for the caller
/// to isolate.
pub(super) async fn relock(
    pixi: &Driver,
    project: &Project,
    plan: &Plan,
    owned: &mut ProjectMutationState,
) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    if plan.changes.is_empty() {
        return Ok(report);
    }
    let lock_path = Utf8Path::new(LOCKFILE);
    // The publish below refuses to write once any file differs from `owned`, so this is the lock
    // the landing starts from.
    let before_text = owned
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
    let before = lock::read(&before_text)?;
    let manifest = Manifest::read(&project.root)?
        .ok_or_else(|| CoreError::System(format!("{} holds no pixi manifest", project.root)))?;
    let root = canonical_path(&project.root)?;
    let links = match find_links(&root, &manifest) {
        Ok(links) => links,
        Err(detail) => {
            report.skipped = hold_all(plan, &detail);
            return Ok(report);
        }
    };
    let Some(policy) = &plan.pin_policy else {
        let detail = "cooldown has no policy to judge what the landing may move beside its targets";
        report.skipped = hold_all(plan, detail);
        return Ok(report);
    };
    let now = plan.evaluated_at.unwrap_or_else(Timestamp::now);
    let cutoff_of = |name: &str| policy.cutoff(name, Some(CONDA), now);
    let Landing {
        targets,
        held,
        forced,
    } = prepare(
        &manifest,
        &before,
        plan,
        &cutoff_of,
        run_cutoff(project, now),
    )?;
    report.skipped = held;
    if targets.is_empty() {
        return Ok(report);
    }

    // One pixi lock, then pixi's own verdict on the result under the project's manifest
    let files = inputs(&root, &manifest, &links)?;
    let scratch = Scratch::stage(&root, &manifest, &files, &links, &before_text)?;
    scratch.write(manifest.file, &forced.render())?;
    scratch
        .run(pixi, &["lock"], "pixi cannot lock the targets")
        .await?;
    let after_text = std::fs::read_to_string(scratch.root.join(LOCKFILE))?;
    let collateral = match verify(&before, &lock::read(&after_text)?, &targets, &cutoff_of)? {
        Verdict::Accepted(collateral) => collateral,
        Verdict::Unsupported(detail) => {
            report.skipped = hold_all(plan, &detail);
            return Ok(report);
        }
    };
    scratch.write(manifest.file, &manifest.text)?;
    scratch
        .run(
            pixi,
            &["lock", "--check"],
            "pixi finds the relocked lock out of date for the project's manifest",
        )
        .await?;
    // The copy locked the project as it was staged; an input that changed since then is someone
    // else's change, which the landing must neither overwrite nor ignore.
    if !scratch.inputs_unchanged(&root, &manifest)? {
        return Err(CoreError::LockConflict(format!(
            "the inputs of {LOCKFILE} changed while cooldown locked them in a copy, so the lock \
             was left as it is; re-run once they have settled"
        )));
    }

    owned.write_owned(lock_path, after_text.as_bytes())?;
    report
        .applied
        .extend(targets.iter().map(|target| target.change.clone()));
    report.applied.extend(collateral);
    Ok(report)
}

/// The cutoff the project's own window puts on every package, which the scratch manifest sets,
/// or `None` when it has no window.
fn run_cutoff(project: &Project, now: Timestamp) -> Option<Timestamp> {
    let cutoff = project.exclude_newer.as_deref()?;
    cutoff.parse::<Timestamp>().ok().or_else(|| {
        let span = cooldown_core::duration::parse_duration(cutoff).ok()?;
        now.checked_sub(span).ok()
    })
}

/// Decides which changes of `plan` land and writes the scratch manifest that forces them, before
/// pixi runs, with each target's cutoff from the plan or else `cutoff_of` its name.
///
/// The scratch `cutoff` exempts each young build the lock holds of a package no target moves by
/// its own archive, and holds the batch when one package needs two.
///
/// # Errors
///
/// Returns [`CoreError::UnacceptableResolve`] when an edit of an eligible target cannot be
/// written, which fails the batch for the caller to isolate.
fn prepare<'a>(
    manifest: &Manifest,
    before: &'a Lock,
    plan: &'a Plan,
    cutoff_of: &dyn Fn(&str) -> Option<Timestamp>,
    cutoff: Option<Timestamp>,
) -> Result<Landing<'a>> {
    let mut forced = manifest.clone();
    if let Some(detail) = &before.unsupported {
        return Ok(Landing {
            targets: Vec::new(),
            held: hold_all(plan, detail),
            forced,
        });
    }
    let mut targets = Vec::new();
    let mut held = Vec::new();
    for change in &plan.changes {
        match target(manifest, before, plan, cutoff_of, change) {
            Ok(target) => {
                force(&mut forced, &target).map_err(CoreError::UnacceptableResolve)?;
                targets.push(target);
            }
            Err(detail) => held.push(hold(change, detail)),
        }
    }
    let Some(cutoff) = cutoff else {
        return Ok(Landing {
            targets,
            held,
            forced,
        });
    };
    let moved: BTreeSet<&str> = targets
        .iter()
        .filter(|target| target.kind == Kind::Conda)
        .flat_map(|target| target.slots.iter().map(|slot| slot.name.as_str()))
        .collect();
    let mut archives: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (slot, binding) in &before.bindings {
        let young = binding.uploaded.is_some_and(|uploaded| uploaded > cutoff);
        if slot.kind == Kind::Conda && young && !moved.contains(slot.name.as_str()) {
            archives
                .entry(slot.name.as_str())
                .or_default()
                .insert(binding.url.as_str());
        }
    }
    if let Some(name) = archives
        .iter()
        .find_map(|(name, urls)| (urls.len() > 1).then_some(name))
    {
        let detail = format!(
            "{LOCKFILE} holds more than one build of {name} younger than the cutoff, and pixi can \
             let only one of them past it"
        );
        held.extend(
            targets
                .drain(..)
                .map(|target| hold(target.change, detail.clone())),
        );
    }
    let exemptions = archives.into_iter().filter_map(|(name, urls)| {
        let url = urls.first()?;
        Some((name, InlineTable::from_iter([("url", *url)])))
    });
    forced.exclude_newer(cutoff, InlineTable::from_iter(exemptions));
    Ok(Landing {
        targets,
        held,
        forced,
    })
}

/// Every change of `plan`, held as not eligible for `detail`.
fn hold_all(plan: &Plan, detail: &str) -> Vec<Skipped> {
    plan.changes
        .iter()
        .map(|change| hold(change, detail.to_owned()))
        .collect()
}

/// `change`, held as not eligible for `detail`.
fn hold(change: &Change, detail: String) -> Skipped {
    Skipped {
        change: change.clone(),
        reason: SkipReason::NotEligible,
        offending: Some(change.package.clone()),
        detail: Some(detail),
    }
}

/// Decides whether `change` can land: every binding of its package in the lock sits at the
/// version it moves from, a conda package's written constraint can be narrowed, and a PyPI
/// package comes from the public index with a top-level requirement to narrow.
///
/// What the manifest requires elsewhere is pixi's to judge, in the landing and its check.
///
/// # Errors
///
/// Returns why the change is held.
fn target<'a>(
    manifest: &Manifest,
    before: &'a Lock,
    plan: &Plan,
    cutoff_of: &dyn Fn(&str) -> Option<Timestamp>,
    change: &'a Change,
) -> std::result::Result<Target<'a>, String> {
    let kind = kind_of(change);
    let name = normal(kind, &change.package.name);
    let from = change.from.as_str();
    let to = change.to.as_str();
    let bindings: Vec<(&Slot, &Binding)> = before
        .bindings
        .iter()
        .filter(|(slot, _)| slot.kind == kind && slot.name == name)
        .collect();
    if bindings.is_empty() {
        return Err(format!("{LOCKFILE} binds no {name}"));
    }
    if let Some((slot, binding)) = bindings.iter().find(|(_, binding)| binding.version != from) {
        return Err(format!(
            "{LOCKFILE} also binds {name} {} in environment {} on {}, so one pin cannot move it \
             from {from} alone",
            binding.version, slot.environment, slot.platform
        ));
    }
    let mut requirements = Vec::new();
    if kind == Kind::Conda
        && let Some(detail) = manifest.constraint_refusal(&change.package.name, to)
    {
        return Err(detail);
    }
    if kind == Kind::Pypi {
        if let Some((_, binding)) = bindings
            .iter()
            .find(|(_, binding)| binding.source != PUBLIC_PYPI)
        {
            return Err(format!(
                "{LOCKFILE} takes {name} from {}, whose release ages cooldown cannot read",
                binding.source
            ));
        }
        requirements.extend(
            manifest
                .declarations(kind, &name)
                .into_iter()
                .filter_map(|declaration| declaration.location.clone()),
        );
        if requirements.is_empty() {
            return Err(format!(
                "{} declares {name} only where cooldown cannot narrow it: a top-level PyPI \
                 requirement on a package index is the only one it can",
                manifest.file
            ));
        }
    }
    let cutoff = plan
        .target_cutoffs
        .iter()
        .find(|admitted| {
            admitted.package == change.package
                && admitted.from == change.from
                && admitted.to == change.to
        })
        .map_or_else(|| cutoff_of(&name), |admitted| admitted.cutoff);
    Ok(Target {
        change,
        kind,
        slots: bindings.into_iter().map(|(slot, _)| slot).collect(),
        requirements,
        cutoff,
    })
}

/// Forces `target` in the scratch manifest `forced`: a conda package by one top-level constraint,
/// a PyPI package by narrowing its top-level requirements.
///
/// # Errors
///
/// Returns why an edit cannot be written.
fn force(forced: &mut Manifest, target: &Target<'_>) -> std::result::Result<(), String> {
    let to = target.change.to.as_str();
    match target.kind {
        Kind::Conda => forced.constrain(&target.change.package.name, to),
        Kind::Pypi => target
            .requirements
            .iter()
            .try_for_each(|location| forced.narrow(location, to)),
    }
}

/// Whether `a` and `b` are one build from one source with one age.
fn same_build(a: &Binding, b: &Binding) -> bool {
    a.version == b.version
        && a.url == b.url
        && a.identity == b.identity
        && a.source == b.source
        && a.uploaded == b.uploaded
}

/// Checks the lock `after` that was asked to land `targets` against the lock `before`.
///
/// A binding the landing leaves the same build is the project's own; every other conda build must
/// have been uploaded by its cutoff: a target's own, or else `cutoff_of` its name.
///
/// # Errors
///
/// Returns [`CoreError::UnacceptableResolve`] when a target's binding does not sit at exactly the
/// target; when a binding, a target's among them, changes source, or a PyPI one the lock newly
/// selects comes from elsewhere than the public index; or when a conda build the lock newly
/// selects records no upload time or one past its cutoff.
fn verify(
    before: &Lock,
    after: &Lock,
    targets: &[Target<'_>],
    cutoff_of: &dyn Fn(&str) -> Option<Timestamp>,
) -> Result<Verdict> {
    if let Some(detail) = &after.unsupported {
        return Ok(Verdict::Unsupported(detail.clone()));
    }
    let unacceptable = CoreError::UnacceptableResolve;
    for target in targets {
        for slot in &target.slots {
            let landed = after.bindings.get(*slot);
            if landed.is_none_or(|binding| binding.version != target.change.to.as_str()) {
                return Err(unacceptable(format!(
                    "pixi locks {} {} in environment {} on {}, not {}",
                    slot.name,
                    landed.map_or("nowhere", |binding| binding.version.as_str()),
                    slot.environment,
                    slot.platform,
                    target.change.to
                )));
            }
        }
    }

    let mut collateral: Vec<Change> = Vec::new();
    for (slot, binding) in &after.bindings {
        let was = before.bindings.get(slot);
        if was.is_some_and(|was| same_build(was, binding)) {
            continue;
        }
        let Slot { kind, name, .. } = slot;
        if let Some(was) = was
            && was.source != binding.source
        {
            return Err(unacceptable(format!(
                "pixi would move {name} from {} to {}, whose releases cooldown cannot judge",
                was.source, binding.source
            )));
        }
        if *kind == Kind::Pypi && binding.source != PUBLIC_PYPI {
            return Err(unacceptable(format!(
                "pixi would take {name} from {}, whose release ages cooldown cannot read",
                binding.source
            )));
        }
        if *kind == Kind::Conda {
            admit(slot, binding, targets, cutoff_of)?;
        }
        let Some(was) = was else {
            continue;
        };
        let moves = |change: &Change| {
            normal(*kind, &change.package.name) == *name
                && change.package.registry.as_deref() == Some(registry_of(*kind))
                && change.from.as_str() == was.version
                && change.to.as_str() == binding.version
        };
        let planned = targets.iter().any(|target| moves(target.change));
        if was.version == binding.version || planned || collateral.iter().any(moves) {
            continue;
        }
        collateral.push(Change {
            package: PackageId::new(PIXI_ID, name.clone(), Some(registry_of(*kind).to_owned())),
            kind: version::classify_kind(&was.version, &binding.version)
                .unwrap_or(UpdateKind::Minor),
            downgrade: version::compare(&binding.version, &was.version).is_lt(),
            from: Version::new(was.version.clone()),
            to: Version::new(binding.version.clone()),
            direct: false,
            members: Vec::new(),
        });
    }
    Ok(Verdict::Accepted(collateral))
}

/// Checks that the conda build `binding` the landing newly selects at `slot` was uploaded by its
/// cutoff: the one that admitted the target it lands, or else `cutoff_of` its package.
///
/// # Errors
///
/// Returns [`CoreError::UnacceptableResolve`] naming the build when it records no upload time, or
/// one past its cutoff.
fn admit(
    slot: &Slot,
    binding: &Binding,
    targets: &[Target<'_>],
    cutoff_of: &dyn Fn(&str) -> Option<Timestamp>,
) -> Result<()> {
    let build = binding.url.rsplit('/').next().unwrap_or(&binding.url);
    let Some(uploaded) = binding.uploaded else {
        return Err(CoreError::UnacceptableResolve(format!(
            "pixi would lock {build}, which records no upload time"
        )));
    };
    let target = targets.iter().find(|target| {
        target.slots.contains(&slot) && target.change.to.as_str() == binding.version
    });
    let cutoff = target.map_or_else(|| cutoff_of(&slot.name), |target| target.cutoff);
    match cutoff {
        Some(cutoff) if uploaded > cutoff => Err(CoreError::UnacceptableResolve(format!(
            "pixi picked {build}, uploaded on {uploaded}, past the cutoff {cutoff} {}",
            if target.is_some() {
                "that admitted the target"
            } else {
                "that governs the package"
            }
        ))),
        _ => Ok(()),
    }
}

/// A copy of the files pixi locks a project from, deleted when dropped.
struct Scratch {
    #[expect(dead_code, reason = "the field keeps the copy alive")]
    dir: tempfile::TempDir,
    /// Where the project root sits in the copy.
    root: Utf8PathBuf,
    /// A hash of each input the copy was made from, and `None` for the optional one absent then.
    inputs: BTreeMap<Utf8PathBuf, Option<u64>>,
}

impl Scratch {
    /// Copies `files`, the inputs of the project at `root`, and creates `directories`, at their
    /// layout below the deepest directory that holds them all, the manifest as `manifest` read it,
    /// with `lock` as the lock.
    fn stage(
        root: &Utf8Path,
        manifest: &Manifest,
        files: &[Utf8PathBuf],
        directories: &[Utf8PathBuf],
        lock: &str,
    ) -> Result<Self> {
        let mut base = root.to_owned();
        let all = || files.iter().chain(directories);
        while !all().all(|path| path.starts_with(&base)) && base.pop() {}
        let dir = tempfile::tempdir()?;
        // A workspace named after its directory keeps its name in the copy.
        let copy = utf8(dir.path())?.join(base.file_name().unwrap_or("workspace"));
        let manifest_path = root.join(manifest.file);
        let optional = optional_input(root);
        let mut inputs = BTreeMap::new();
        for path in files.iter().chain([&optional]) {
            // The manifest is staged as it was read, so a change since then fails the publish.
            let bytes = if *path == manifest_path {
                Some(manifest.text.as_bytes().to_vec())
            } else {
                read_if_present(path)?
            };
            if let Some(bytes) = &bytes {
                let rel = path
                    .strip_prefix(&base)
                    .map_err(|_| CoreError::System(format!("{path} lies outside {base}")))?;
                let staged = copy.join(rel);
                if let Some(parent) = staged.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&staged, bytes)?;
            }
            inputs.insert(path.clone(), bytes.as_deref().map(hash));
        }
        for directory in directories {
            let rel = directory
                .strip_prefix(&base)
                .map_err(|_| CoreError::System(format!("{directory} lies outside {base}")))?;
            std::fs::create_dir_all(copy.join(rel))?;
        }
        let root = match root.strip_prefix(&base) {
            Ok(rel) if !rel.as_str().is_empty() => copy.join(rel),
            _ => copy,
        };
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join(LOCKFILE), lock)?;
        Ok(Scratch { dir, root, inputs })
    }

    /// Writes `text` to `file` at the copy's project root.
    fn write(&self, file: &str, text: &str) -> Result<()> {
        Ok(std::fs::write(self.root.join(file), text)?)
    }

    /// Runs pixi with `args` at the copy's project root.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::UnacceptableResolve`] with `refusal` and pixi's own account when the
    /// run fails.
    async fn run(&self, pixi: &Driver, args: &[&str], refusal: &str) -> Result<()> {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        let output = pixi.output_with_env(&self.root, &args, &[]).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(CoreError::UnacceptableResolve(format!(
                "{refusal}: {}",
                failure_detail(&output)
            )))
        }
    }

    /// Whether every input `manifest` gives the project at `root` still holds what the copy was
    /// made from, with none appeared or gone, read through the same check of its `find-links`
    /// directories, so a link added to one meanwhile counts as a change.
    fn inputs_unchanged(&self, root: &Utf8Path, manifest: &Manifest) -> Result<bool> {
        let Ok(links) = find_links(root, manifest) else {
            return Ok(false);
        };
        let files = inputs(root, manifest, &links)?;
        let optional = optional_input(root);
        let mut now = BTreeMap::new();
        for path in files.iter().chain([&optional]) {
            now.insert(path.clone(), read_if_present(path)?.as_deref().map(hash));
        }
        Ok(now == self.inputs)
    }
}

/// The bytes of the file at `path`, or `None` when there is none.
fn read_if_present(path: &Utf8Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    hasher.write(bytes);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cooldown_core::{
        ByKind, Origin, PatternGlob, PinPolicy, PolicyLayer, Rule, Selector, TargetCutoff,
        WindowSpec,
    };
    use cooldown_uv::pypi::PYPI;
    use indoc::indoc;

    const CONDA_FORGE: &str = "https://conda.anaconda.org/conda-forge";

    /// The freeze every test judges collateral against.
    const FREEZE: &str = "2024-01-15T00:00:00Z";

    fn at(instant: &str) -> Timestamp {
        instant
            .parse()
            .unwrap_or_else(|err| panic!("{instant}: {err}"))
    }

    fn slot(environment: &str, platform: &str, kind: Kind, name: &str) -> Slot {
        Slot {
            environment: environment.to_owned(),
            platform: platform.to_owned(),
            kind,
            name: name.to_owned(),
        }
    }

    /// The conda-forge linux-64 build `digest` of `name` at `version`, uploaded at `uploaded`.
    fn build(name: &str, version: &str, digest: &str, uploaded: Option<&str>) -> Binding {
        Binding {
            version: version.to_owned(),
            url: format!("{CONDA_FORGE}/linux-64/{name}-{version}-h{digest}_0.conda"),
            identity: digest.to_owned(),
            source: CONDA_FORGE.to_owned(),
            uploaded: uploaded.map(at),
            capped: false,
        }
    }

    /// A wheel of the PyPI package `name` at `version` from `source`.
    fn wheel(name: &str, version: &str, source: &str) -> Binding {
        Binding {
            version: version.to_owned(),
            url: format!(
                "https://files.pythonhosted.org/packages/{name}-{version}-py3-none-any.whl"
            ),
            identity: format!("{name}-{version}"),
            source: source.to_owned(),
            uploaded: None,
            capped: false,
        }
    }

    fn lock(bindings: impl IntoIterator<Item = (Slot, Binding)>) -> Lock {
        Lock {
            bindings: bindings.into_iter().collect(),
            unsupported: None,
        }
    }

    fn change(registry: &str, name: &str, from: &str, to: &str) -> Change {
        Change {
            package: PackageId::new(PIXI_ID, name.to_owned(), Some(registry.to_owned())),
            from: Version::new(from.to_owned()),
            to: Version::new(to.to_owned()),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        }
    }

    fn plan(changes: Vec<Change>) -> Plan {
        Plan {
            changes,
            ..Plan::default()
        }
    }

    fn manifest(text: &str) -> Manifest {
        match Manifest::parse("pixi.toml", text.to_owned()) {
            Ok(Some(manifest)) => manifest,
            other => panic!("no pixi manifest: {:?}", other.err()),
        }
    }

    /// The collateral cutoff of a project frozen at [`FREEZE`].
    fn frozen(_name: &str) -> Option<Timestamp> {
        FREEZE.parse().ok()
    }

    /// What `prepare` decides for `plan` over `before` under `manifest` and the freeze.
    fn prepared<'a>(manifest: &Manifest, before: &'a Lock, plan: &'a Plan) -> Landing<'a> {
        prepare(manifest, before, plan, &frozen, Some(at(FREEZE))).expect("eligible edits apply")
    }

    /// Why `landing` holds each of its changes.
    fn held_for(landing: &Landing<'_>) -> Vec<String> {
        landing
            .held
            .iter()
            .filter_map(|skipped| skipped.detail.clone())
            .collect()
    }

    /// Whether `verdict` refuses the landing for a reason that names every one of `needles`.
    fn refuses(verdict: &Result<Verdict>, needles: &[&str]) -> bool {
        matches!(verdict, Err(CoreError::UnacceptableResolve(detail))
            if needles.iter().all(|needle| detail.contains(needle)))
    }

    const PCRE2: &str = indoc! {r#"
        [dependencies]
        pcre2 = ">=10.40,<11"
    "#};

    fn pcre2(version: &str) -> Binding {
        build("pcre2", version, version, Some("2023-10-29T00:00:00Z"))
    }

    /// The pcre2 10.40 → 10.42 landing most tests start from.
    fn pcre2_landing() -> Plan {
        plan(vec![change(CONDA, "pcre2", "10.40", "10.42")])
    }

    /// A conda target is pinned by one top-level constraint and lands only when every binding of
    /// its package sits at the version it moves from.
    #[test]
    fn a_conda_target_moves_from_one_version_everywhere() {
        let uniform = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.40"),
            ),
            (
                slot("regex", "win-64", Kind::Conda, "pcre2"),
                pcre2("10.40"),
            ),
        ]);
        let batch = pcre2_landing();
        let landing = prepared(&manifest(PCRE2), &uniform, &batch);
        assert_eq!(landing.targets.len(), 1, "{:?}", held_for(&landing));
        assert!(landing.forced.render().contains(r#"pcre2 = "==10.42""#));

        let mixed = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.40"),
            ),
            (
                slot("other", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.43"),
            ),
        ]);
        let landing = prepared(&manifest(PCRE2), &mixed, &batch);
        assert!(landing.targets.is_empty());
        assert!(
            held_for(&landing)
                .iter()
                .any(|detail| detail.contains("also binds pcre2 10.43 in environment other")),
            "{:?}",
            held_for(&landing)
        );
        assert!(!landing.forced.render().contains("==10.42"));
    }

    /// A conda target whose top-level constraint comes from the workspace is held before the
    /// scratch manifest is edited, since narrowing it would add a version beside the inheritance.
    #[test]
    fn an_inherited_constraint_holds_its_target() {
        let manifest = manifest(indoc! {r#"
            [dependencies]
            pcre2 = "*"

            [constraints]
            pcre2 = { workspace = true }
        "#});
        let before = lock([(
            slot("default", "linux-64", Kind::Conda, "pcre2"),
            pcre2("10.40"),
        )]);
        let batch = pcre2_landing();

        let landing = prepared(&manifest, &before, &batch);

        assert!(landing.targets.is_empty());
        assert!(
            held_for(&landing)
                .iter()
                .any(|detail| detail.contains("constrains pcre2 in a form cooldown cannot narrow")),
            "{:?}",
            held_for(&landing)
        );
        assert!(!landing.forced.render().contains("10.42"));
    }

    /// A PyPI target lands only from the public index and through a top-level requirement: one a
    /// feature alone declares, and one a private index serves, are held.
    #[test]
    fn a_pypi_target_needs_a_top_level_requirement_on_the_public_index() {
        let manifest = manifest(indoc! {r#"
            [pypi-dependencies]
            idna = ">=3"
            private = "*"

            [feature.web.pypi-dependencies]
            urllib3 = ">=2"
        "#});
        let before = lock([
            (
                slot("default", "linux-64", Kind::Pypi, "idna"),
                wheel("idna", "3.6", PUBLIC_PYPI),
            ),
            (
                slot("web", "linux-64", Kind::Pypi, "urllib3"),
                wheel("urllib3", "2.1.0", PUBLIC_PYPI),
            ),
            (
                slot("default", "linux-64", Kind::Pypi, "private"),
                wheel("private", "1.0", "https://example.org/simple"),
            ),
        ]);
        let batch = plan(vec![
            change(PYPI, "idna", "3.6", "3.7"),
            change(PYPI, "urllib3", "2.1.0", "2.2.0"),
            change(PYPI, "private", "1.0", "1.1"),
        ]);

        let landing = prepared(&manifest, &before, &batch);

        let details = held_for(&landing);
        assert_eq!(landing.targets.len(), 1, "{details:?}");
        assert_eq!(landing.targets[0].change.package.name, "idna");
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("only where cooldown cannot narrow it"))
                && details
                    .iter()
                    .any(|detail| detail.contains("from https://example.org/simple")),
            "{details:?}"
        );
        let forced = landing.forced.render();
        assert!(
            forced.contains(r#"idna = ">=3,==3.7""#) && forced.contains(r#"urllib3 = ">=2""#),
            "{forced}"
        );
    }

    /// A lock that binds a record cooldown cannot model, before the landing or after it, holds the
    /// whole workspace.
    #[test]
    fn an_unsupported_record_holds_the_workspace() -> Result<()> {
        let source = Some("environment default binds demo[abcd1234] @ .".to_owned());
        let mut before = lock([(
            slot("default", "linux-64", Kind::Conda, "pcre2"),
            pcre2("10.40"),
        )]);
        let batch = pcre2_landing();
        before.unsupported.clone_from(&source);
        let landing = prepared(&manifest(PCRE2), &before, &batch);
        assert!(landing.targets.is_empty());
        assert_eq!(held_for(&landing), [source.clone().unwrap_or_default()]);

        before.unsupported = None;
        let landing = prepared(&manifest(PCRE2), &before, &batch);
        let mut after = lock([(
            slot("default", "linux-64", Kind::Conda, "pcre2"),
            pcre2("10.42"),
        )]);
        after.unsupported = source;
        let verdict = verify(&before, &after, &landing.targets, &frozen)?;
        assert!(matches!(verdict, Verdict::Unsupported(_)), "{verdict:?}");
        Ok(())
    }

    /// A target that pixi lands on one platform but not on another is refused.
    #[test]
    fn a_binding_left_off_its_target_is_refused() {
        let before = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.40"),
            ),
            (
                slot("default", "win-64", Kind::Conda, "pcre2"),
                pcre2("10.40"),
            ),
        ]);
        let after = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.42"),
            ),
            (
                slot("default", "win-64", Kind::Conda, "pcre2"),
                pcre2("10.40"),
            ),
        ]);
        let batch = pcre2_landing();
        let landing = prepared(&manifest(PCRE2), &before, &batch);

        let verdict = verify(&before, &after, &landing.targets, &frozen);

        assert!(refuses(&verdict, &["win-64"]), "{verdict:?}");
    }

    /// A build the lock newly selects needs an upload time by its cutoff, even at a version and
    /// URL the lock already held, since another digest or age is another build.
    #[test]
    fn a_new_build_without_mature_age_evidence_is_refused() {
        let libzlib = build("libzlib", "1.2.13", "a4", Some("2022-10-14T00:00:00Z"));
        let before = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.40"),
            ),
            (
                slot("default", "linux-64", Kind::Conda, "libzlib"),
                libzlib.clone(),
            ),
        ]);
        let batch = pcre2_landing();
        let landing = prepared(&manifest(PCRE2), &before, &batch);
        let rebuilt = Binding {
            identity: "a5".to_owned(),
            uploaded: Some(at("2024-05-28T00:00:00Z")),
            ..libzlib.clone()
        };
        let undated = Binding {
            uploaded: None,
            ..libzlib.clone()
        };
        for (rebuild, reason) in [(rebuilt, "past the cutoff"), (undated, "no upload time")] {
            let after = lock([
                (
                    slot("default", "linux-64", Kind::Conda, "pcre2"),
                    pcre2("10.42"),
                ),
                (slot("default", "linux-64", Kind::Conda, "libzlib"), rebuild),
            ]);

            let verdict = verify(&before, &after, &landing.targets, &frozen);

            assert!(
                refuses(&verdict, &["libzlib-1.2.13", reason]),
                "{verdict:?}"
            );
        }
    }

    /// A young build may stay only in the binding it held: the same archive bound in another
    /// environment is a new selection, judged by its cutoff.
    #[test]
    fn a_young_build_stays_only_where_it_was() -> Result<()> {
        let young = build("zlib", "1.3.1", "z1", Some("2024-02-29T00:00:00Z"));
        let before = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.40"),
            ),
            (slot("zlib", "linux-64", Kind::Conda, "zlib"), young.clone()),
        ]);
        let batch = pcre2_landing();
        let landing = prepared(&manifest(PCRE2), &before, &batch);
        let stays = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.42"),
            ),
            (slot("zlib", "linux-64", Kind::Conda, "zlib"), young.clone()),
        ]);
        assert!(matches!(
            verify(&before, &stays, &landing.targets, &frozen)?,
            Verdict::Accepted(_)
        ));

        let spreads = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.42"),
            ),
            (
                slot("default", "linux-64", Kind::Conda, "zlib"),
                young.clone(),
            ),
            (slot("zlib", "linux-64", Kind::Conda, "zlib"), young),
        ]);
        let verdict = verify(&before, &spreads, &landing.targets, &frozen);
        assert!(
            refuses(&verdict, &["zlib-1.3.1", "past the cutoff"]),
            "{verdict:?}"
        );
        Ok(())
    }

    /// A target lands under the cutoff that admitted it, so a build pixi picks past a stricter one
    /// is refused, naming the build and the cutoff it missed.
    #[test]
    fn a_stricter_target_window_rejects_a_young_build() {
        let before = lock([(
            slot("default", "linux-64", Kind::Conda, "pcre2"),
            pcre2("10.40"),
        )]);
        let target = change(CONDA, "pcre2", "10.40", "10.42");
        let batch = Plan {
            target_cutoffs: vec![TargetCutoff {
                package: target.package.clone(),
                from: target.from.clone(),
                to: target.to.clone(),
                cutoff: Some(at("2023-12-01T00:00:00Z")),
            }],
            ..plan(vec![target])
        };
        let landing = prepared(&manifest(PCRE2), &before, &batch);
        let after = lock([(
            slot("default", "linux-64", Kind::Conda, "pcre2"),
            build("pcre2", "10.42", "b1", Some("2024-01-10T00:00:00Z")),
        )]);

        let verdict = verify(&before, &after, &landing.targets, &frozen);

        assert!(
            refuses(
                &verdict,
                &[
                    "pcre2-10.42-hb1_0.conda",
                    "2023-12-01T00:00:00Z",
                    "admitted the target"
                ]
            ),
            "{verdict:?}"
        );
    }

    /// Collateral is judged by the project's policy as `check` resolves it, so a build within the
    /// default window but past a stricter glob's is refused.
    #[test]
    fn collateral_under_a_stricter_glob_is_rejected() -> Result<()> {
        let mut layer = PolicyLayer::new(Origin::Cli);
        let mut default = Rule::new(Selector::Default);
        default.window = ByKind::scalar(WindowSpec::Freeze(at(FREEZE)));
        let mut strict = Rule::new(Selector::Package {
            glob: PatternGlob::new("lib*")?,
            tool: None,
        });
        strict.window = ByKind::scalar(WindowSpec::Freeze(at("2024-01-01T00:00:00Z")));
        layer.rules.extend([default, strict]);
        let policy = PinPolicy {
            layers: vec![layer],
            tool: PIXI_ID,
            project: Utf8PathBuf::from("."),
        };
        let cutoff_of = |name: &str| policy.cutoff(name, Some(CONDA), at(FREEZE));
        let libzlib = |version: &str, digest: &str| {
            build("libzlib", version, digest, Some("2024-01-09T00:00:00Z"))
        };
        let before = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.40"),
            ),
            (
                slot("default", "linux-64", Kind::Conda, "libzlib"),
                build("libzlib", "1.2.13", "a4", Some("2022-10-14T00:00:00Z")),
            ),
        ]);
        let after = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.42"),
            ),
            (
                slot("default", "linux-64", Kind::Conda, "libzlib"),
                libzlib("1.3", "c3"),
            ),
        ]);
        let batch = pcre2_landing();
        let landing = prepare(
            &manifest(PCRE2),
            &before,
            &batch,
            &cutoff_of,
            Some(at(FREEZE)),
        )?;

        let verdict = verify(&before, &after, &landing.targets, &cutoff_of);

        assert!(
            refuses(
                &verdict,
                &["libzlib-1.3", "2024-01-01T00:00:00Z", "governs the package"]
            ),
            "{verdict:?}"
        );
        Ok(())
    }

    /// A package the lock moves to another channel or index is refused, since cooldown judges the
    /// releases of the one it came from alone, and so is a PyPI package it newly takes from
    /// another index than the public one.
    #[test]
    fn a_binding_that_changes_channel_or_index_is_refused() {
        let bzip2 = build("bzip2", "1.0.8", "b0", Some("2023-11-06T00:00:00Z"));
        let moved = Binding {
            source: "https://conda.anaconda.org/bioconda".to_owned(),
            ..bzip2.clone()
        };
        let idna = wheel("idna", "3.6", PUBLIC_PYPI);
        let mirrored = wheel("idna", "3.6", "https://mirror.example.org/simple");
        for (name, kind, was, now, needle) in [
            ("bzip2", Kind::Conda, bzip2, moved, "bioconda"),
            ("idna", Kind::Pypi, idna, mirrored, "mirror.example.org"),
        ] {
            let before = lock([
                (
                    slot("default", "linux-64", Kind::Conda, "pcre2"),
                    pcre2("10.40"),
                ),
                (slot("default", "linux-64", kind, name), was),
            ]);
            let after = lock([
                (
                    slot("default", "linux-64", Kind::Conda, "pcre2"),
                    pcre2("10.42"),
                ),
                (slot("default", "linux-64", kind, name), now),
            ]);
            let batch = pcre2_landing();
            let landing = prepared(&manifest(PCRE2), &before, &batch);

            let verdict = verify(&before, &after, &landing.targets, &frozen);

            assert!(refuses(&verdict, &[needle]), "{verdict:?}");
        }

        let before = lock([(
            slot("default", "linux-64", Kind::Conda, "pcre2"),
            pcre2("10.40"),
        )]);
        let added = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.42"),
            ),
            (
                slot("default", "linux-64", Kind::Pypi, "private"),
                wheel("private", "1.0", "https://example.org/simple"),
            ),
        ]);
        let batch = pcre2_landing();
        let landing = prepared(&manifest(PCRE2), &before, &batch);
        let verdict = verify(&before, &added, &landing.targets, &frozen);
        assert!(refuses(&verdict, &["example.org"]), "{verdict:?}");
    }

    /// A move no change planned is reported as a collateral row, and the target's own move is not.
    #[test]
    fn an_unplanned_move_is_reported_as_collateral() -> Result<()> {
        let libzlib =
            |version: &str| build("libzlib", version, version, Some("2024-01-09T00:00:00Z"));
        let before = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.40"),
            ),
            (
                slot("default", "linux-64", Kind::Conda, "libzlib"),
                libzlib("1.2.13"),
            ),
        ]);
        let after = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.42"),
            ),
            (
                slot("default", "linux-64", Kind::Conda, "libzlib"),
                libzlib("1.3"),
            ),
        ]);
        let batch = pcre2_landing();
        let landing = prepared(&manifest(PCRE2), &before, &batch);

        let Verdict::Accepted(collateral) = verify(&before, &after, &landing.targets, &frozen)?
        else {
            panic!("the lock is supported");
        };

        let rows: Vec<String> = collateral
            .iter()
            .map(|row| format!("{} {} {}", row.package.name, row.from, row.to))
            .collect();
        assert_eq!(rows, ["libzlib 1.2.13 1.3"]);
        Ok(())
    }

    /// The scratch cutoff exempts the young build of a package no target moves by its own
    /// archive, and not the target's, and two such builds of one package, which pixi cannot both
    /// exempt, hold the batch.
    #[test]
    fn a_young_build_is_exempted_by_its_archive_alone() {
        let young = |digest: &str| build("libzlib", "1.3.1", digest, Some("2024-02-29T00:00:00Z"));
        let young_target = build("pcre2", "10.40", "p0", Some("2024-02-01T00:00:00Z"));
        let one = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                young_target,
            ),
            (
                slot("default", "linux-64", Kind::Conda, "libzlib"),
                young("d0"),
            ),
            (
                slot("other", "linux-64", Kind::Conda, "libzlib"),
                young("d0"),
            ),
        ]);
        let batch = pcre2_landing();
        let landing = prepared(&manifest(PCRE2), &one, &batch);
        let forced = landing.forced.render();
        assert!(
            forced.contains(&format!(
                r#"libzlib = {{ url = "{CONDA_FORGE}/linux-64/libzlib-1.3.1-hd0_0.conda" }}"#
            )) && !forced.contains("pcre2 = { url"),
            "{forced}"
        );

        let two = lock([
            (
                slot("default", "linux-64", Kind::Conda, "pcre2"),
                pcre2("10.40"),
            ),
            (
                slot("default", "linux-64", Kind::Conda, "libzlib"),
                young("d0"),
            ),
            (
                slot("default", "osx-arm64", Kind::Conda, "libzlib"),
                Binding {
                    url: format!("{CONDA_FORGE}/osx-arm64/libzlib-1.3.1-hd1_0.conda"),
                    ..young("d1")
                },
            ),
        ]);
        let landing = prepared(&manifest(PCRE2), &two, &batch);
        assert!(landing.targets.is_empty());
        assert!(
            held_for(&landing)
                .iter()
                .any(|detail| detail.contains("more than one build of libzlib")),
            "{:?}",
            held_for(&landing)
        );
    }

    /// A UTF-8 temporary directory, canonical.
    fn tempdir() -> Result<(tempfile::TempDir, Utf8PathBuf)> {
        let dir = tempfile::tempdir()?;
        let path = canonical_path(&utf8(dir.path())?)?;
        Ok((dir, path))
    }

    /// The copy holds a `find-links` directory beside the project at the path pixi reads it by,
    /// with the files directly in it alone, an empty one included, so `.` brings neither `.git`
    /// nor `.pixi/envs` along.
    #[test]
    fn find_links_are_staged_flat_where_pixi_reads_them() -> Result<()> {
        let (_dir, base) = tempdir()?;
        let root = base.join("app");
        for dir in [
            root.join(".git"),
            root.join(".pixi").join("envs"),
            base.join("wheels").join("nested"),
            base.join("empty"),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(base.join("wheels").join("a-1-py3-none-any.whl"), "wheel")?;
        std::fs::write(base.join("wheels").join("nested").join("b-1.whl"), "")?;
        std::fs::write(root.join(".git").join("HEAD"), "")?;
        let text = indoc! {r#"
            [pypi-options]
            find-links = [{ path = "../wheels" }, { path = "../empty" }, { path = "." }]
        "#};
        std::fs::write(root.join("pixi.toml"), text)?;
        let links = find_links(&root, &manifest(text)).map_err(CoreError::System)?;
        let files = inputs(&root, &manifest(text), &links)?;

        let scratch = Scratch::stage(&root, &manifest(text), &files, &links, "version: 6\n")?;

        assert_eq!(scratch.root.file_name(), Some("app"));
        let wheels = scratch.root.join("..").join("wheels");
        assert_eq!(
            std::fs::read_to_string(wheels.join("a-1-py3-none-any.whl"))?,
            "wheel"
        );
        assert!(!wheels.join("nested").exists());
        assert!(scratch.root.join("..").join("empty").is_dir());
        assert!(!scratch.root.join(".git").exists() && !scratch.root.join(".pixi").exists());
        Ok(())
    }

    /// A `find-links` directory reached through a link, one that holds a link, and one that does
    /// not exist are held, since a copy would put the link's target elsewhere than pixi looks.
    #[cfg(unix)]
    #[test]
    fn a_find_links_link_is_held() -> Result<()> {
        let (_dir, base) = tempdir()?;
        let root = base.join("app");
        for dir in [&root, &base.join("real"), &base.join("linking")] {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(base.join("real").join("a-1-py3-none-any.whl"), "wheel")?;
        std::os::unix::fs::symlink(base.join("real"), base.join("link"))?;
        std::os::unix::fs::symlink(
            base.join("real").join("a-1-py3-none-any.whl"),
            base.join("linking").join("a-1-py3-none-any.whl"),
        )?;
        for (link, reason) in [
            ("../link", "reached through a link"),
            ("../linking", "holds the link a-1-py3-none-any.whl"),
            ("../missing", "cannot be read"),
        ] {
            let text = format!("[pypi-options]\nfind-links = [{{ path = \"{link}\" }}]\n");
            let held = find_links(&root, &manifest(&text));
            assert!(
                held.as_ref().is_err_and(|detail| detail.contains(reason)),
                "{held:?}"
            );
        }
        Ok(())
    }

    /// A link added to a `find-links` directory while pixi locks counts as a changed input, as the
    /// check before staging would have held it.
    #[cfg(unix)]
    #[test]
    fn a_link_added_to_find_links_meanwhile_is_drift() -> Result<()> {
        let (_dir, root) = tempdir()?;
        std::fs::create_dir_all(root.join("wheels"))?;
        std::fs::write(root.join("wheels").join("a-1-py3-none-any.whl"), "wheel")?;
        let text = "[pypi-options]\nfind-links = [{ path = \"./wheels\" }]\n";
        std::fs::write(root.join("pixi.toml"), text)?;
        let manifest = manifest(text);
        let links = find_links(&root, &manifest).map_err(CoreError::System)?;
        let files = inputs(&root, &manifest, &links)?;
        let scratch = Scratch::stage(&root, &manifest, &files, &links, "version: 6\n")?;
        assert!(scratch.inputs_unchanged(&root, &manifest)?);

        std::os::unix::fs::symlink(
            root.join("wheels").join("a-1-py3-none-any.whl"),
            root.join("wheels").join("b-1-py3-none-any.whl"),
        )?;

        assert!(!scratch.inputs_unchanged(&root, &manifest)?);
        Ok(())
    }

    /// The copy tells when an input changes after it was made, and when one it did not hold
    /// appears: a `.pixi/config.toml`, or a `pixi.toml` pixi would read before `pyproject.toml`.
    #[test]
    fn a_copy_notices_a_changed_or_new_input() -> Result<()> {
        let (_dir, root) = tempdir()?;
        let text = "[tool.pixi.workspace]\n";
        std::fs::write(root.join("pyproject.toml"), text)?;
        let manifest = match Manifest::parse("pyproject.toml", text.to_owned()) {
            Ok(Some(manifest)) => manifest,
            other => panic!("no pixi manifest: {:?}", other.err()),
        };
        let files = inputs(&root, &manifest, &[])?;
        let scratch = Scratch::stage(&root, &manifest, &files, &[], "version: 6\n")?;
        assert!(scratch.inputs_unchanged(&root, &manifest)?);

        std::fs::write(root.join("pixi.toml"), "[workspace]\n")?;
        assert!(!scratch.inputs_unchanged(&root, &manifest)?);
        std::fs::remove_file(root.join("pixi.toml"))?;

        std::fs::create_dir_all(root.join(".pixi"))?;
        std::fs::write(root.join(".pixi").join("config.toml"), "")?;
        assert!(!scratch.inputs_unchanged(&root, &manifest)?);
        std::fs::remove_file(root.join(".pixi").join("config.toml"))?;

        std::fs::write(
            root.join("pyproject.toml"),
            "[tool.pixi.workspace]\nname = \"x\"\n",
        )?;
        assert!(!scratch.inputs_unchanged(&root, &manifest)?);
        Ok(())
    }
}
