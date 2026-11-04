//! Planning a copy: walking the project, following every range a manifest declares to the files
//! it makes Yarn read, and choosing where the copy may go.

use super::{Entry, InPlace, Merge, Plan, Refusal, filesystem, merge};
use crate::lock::{BerryLock, percent_decode};
use crate::manifest::display_rel;
use crate::manifest::workspaces::{DEFAULT_RC_FILENAME, YarnEnvironment, parse_settings};
use crate::yarn::verify::settings::{self, RcFile};
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use cooldown_core::CoreError;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The manifest Yarn reads in each workspace and local package.
const MANIFEST: &str = "package.json";

const LOCKFILE: &str = "yarn.lock";

/// The environment file Yarn 4 injects by default.
const ENVIRONMENT_FILE: &str = ".env.yarn";

/// The directories Yarn's workspace glob ignores at any depth.
const IGNORED: [&str; 3] = ["node_modules", ".git", ".yarn"];

/// The folders below `.yarn` the copy holds whole: Yarn releases a `yarnPath` runs, and the
/// patches `yarn patch-commit` writes.
const YARN_FOLDERS: [&str; 2] = ["releases", "patches"];

/// The most symbolic links one resolution follows before it counts as a loop, as Linux's
/// `MAXSYMLINKS` does.
const MAX_LINK_HOPS: usize = 40;

/// The manifest fields whose ranges a resolver reads.
const RANGE_FIELDS: [&str; 5] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
    "resolutions",
];

/// The state of one planning pass.
pub(super) struct Planner<'a> {
    root: Utf8PathBuf,
    boundary: Utf8PathBuf,
    environment: &'a YarnEnvironment,
    folders: Vec<Utf8PathBuf>,
    entries: BTreeMap<Utf8PathBuf, Entry>,
    in_place: BTreeMap<Utf8PathBuf, InPlace>,
    /// Where each recreated link the walk meets must resolve in the copy: the mirrored position
    /// of what the source's link reaches.
    expected_links: BTreeMap<Utf8PathBuf, Utf8PathBuf>,
    /// Manifests whose ranges are still to be followed, by the path Yarn reads them at.
    pending: Vec<Utf8PathBuf>,
    followed: BTreeSet<Utf8PathBuf>,
}

impl<'a> Planner<'a> {
    pub(super) fn plan(
        root: &Utf8Path,
        lock_text: &str,
        environment: &'a YarnEnvironment,
        bases: &[Utf8PathBuf],
    ) -> std::result::Result<Plan, Refusal> {
        let lock = BerryLock::read(lock_text)?;
        let root = canonical(root.as_std_path())?;
        if let Some(reference) = lock.references().find(|range| settings::is_exec(range)) {
            return decline(format!(
                "{LOCKFILE} records the `exec:` package `{reference}`, whose generator may read \
                 files the verification copy leaves out"
            ));
        }
        let Some(home) = &environment.home else {
            return decline(
                "no home directory is established, so the rc file Yarn reads there is unknown",
            );
        };
        let home = canonical_lenient(home)?;
        if root == home {
            return decline(
                "the project is the home directory itself, whose rc file Yarn reads as the home \
                 one",
            );
        }
        let under_home = root.starts_with(&home);
        let boundary = if under_home {
            home.clone()
        } else {
            root.ancestors().last().unwrap_or(&root).to_owned()
        };
        let mut planner = Planner {
            root,
            boundary,
            environment,
            folders: Vec::new(),
            entries: BTreeMap::new(),
            in_place: BTreeMap::new(),
            expected_links: BTreeMap::new(),
            pending: Vec::new(),
            followed: BTreeSet::new(),
        };

        // Project inputs, then everything their ranges reach
        planner.stage_project(lock_text)?;
        planner.follow_manifests()?;

        // Where the copy goes decides which rc files it holds and which it shares
        let base = planner.choose_base(&home, under_home, bases)?;
        let rc_files = planner.stage_rc_files(&home, &base)?;
        let root = planner.root.clone();
        settings::screen(&rc_files, environment, |path| shown(&root, path))
            .map_err(Refusal::Decline)?;
        planner.stage_yarn_paths(&rc_files)?;
        planner.check_links()?;

        Ok(Plan {
            root: planner.root,
            boundary: planner.boundary,
            base,
            folders: planner.folders,
            entries: planner.entries,
            in_place: planner.in_place,
        })
    }

    fn stage_project(&mut self, lock_text: &str) -> std::result::Result<(), Refusal> {
        let root = self.root.clone();
        self.stage_screened(&root.join(LOCKFILE), lock_text.as_bytes().to_vec())?;
        let environment_file = root.join(ENVIRONMENT_FILE);
        if is_file(&environment_file)? {
            self.stage_file(&environment_file)?;
        }
        for folder in YARN_FOLDERS {
            let folder = root.join(".yarn").join(folder);
            if is_directory(&folder)? {
                self.stage_folder(&folder)?;
            }
        }
        self.walk(&root)
    }

    /// Stages every manifest below `directory` outside the directories Yarn's glob ignores, and
    /// every symbolic link that glob would follow.
    fn walk(&mut self, directory: &Utf8Path) -> std::result::Result<(), Refusal> {
        for Child { name, file_type } in children(&self.root, directory)? {
            let path = directory.join(&name);
            if file_type.is_dir() {
                if !IGNORED.contains(&name.as_str()) {
                    self.walk(&path)?;
                }
            } else if file_type.is_symlink() {
                self.walk_link(&path, &name)?;
            } else if file_type.is_file() && name == MANIFEST {
                self.pending.push(path);
            }
        }
        Ok(())
    }

    /// Stages a symbolic link the walk meets.
    ///
    /// A linked manifest is copied as the file it reaches.
    /// A link to a directory is recreated with its relative target when what it reaches stays in
    /// the walked project and the recreated link resolves to the copy of exactly that
    /// ([`check_links`](Self::check_links)), so the copy's glob follows it to the copied
    /// manifests; any other directory link declines, since Yarn could find a workspace through it
    /// that the copy cannot hold.
    /// A dangling link, or one to a file other than a manifest, is nothing Yarn's glob reads.
    fn walk_link(&mut self, path: &Utf8Path, name: &str) -> std::result::Result<(), Refusal> {
        let reached = match dunce::canonicalize(path) {
            Ok(reached) => utf8(reached)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(filesystem(path, &err).into()),
        };
        let metadata = std::fs::metadata(path).map_err(|err| filesystem(path, &err))?;
        if metadata.is_file() {
            if name == MANIFEST {
                self.pending.push(path.to_owned());
            }
            return Ok(());
        }
        if !metadata.is_dir() {
            return Ok(());
        }
        let target = utf8(std::fs::read_link(path).map_err(|err| filesystem(path, &err))?)?;
        if target.is_absolute() || !self.walked(&reached) {
            return decline(format!(
                "{} links to {}, which the verification copy cannot recreate where Yarn's \
                 workspace glob would follow it",
                shown(&self.root, path),
                shown(&self.root, &reached)
            ));
        }
        let relative = self.relative(path)?;
        let expected = self.relative(&reached)?;
        self.expected_links.insert(relative.clone(), expected);
        self.insert(
            relative,
            Entry::Link {
                target,
                directory: true,
            },
        )
    }

    /// Whether `path` lies in the project outside every directory the walk skips, so the walk
    /// copied whatever Yarn could read there.
    fn walked(&self, path: &Utf8Path) -> bool {
        path.strip_prefix(&self.root).is_ok_and(|relative| {
            relative
                .components()
                .all(|component| !IGNORED.contains(&component.as_str()))
        })
    }

    /// Stages every pending manifest as the bytes its ranges are followed from, and follows them,
    /// and those of every manifest they reach.
    fn follow_manifests(&mut self) -> std::result::Result<(), Refusal> {
        while let Some(manifest) = self.pending.pop() {
            if !self.followed.insert(manifest.clone()) {
                continue;
            }
            let content = std::fs::read(&manifest).map_err(|err| filesystem(&manifest, &err))?;
            self.stage_screened(&manifest, content.clone())?;
            // Yarn reports a manifest it cannot parse itself, before any verdict.
            let Ok(document) = serde_json::from_slice::<Value>(&content) else {
                continue;
            };
            self.screen_workspaces(&manifest, &document)?;
            let directory = parent(&manifest).to_owned();
            for range in ranges(&document) {
                self.follow(&manifest, &directory, range)?;
            }
        }
        Ok(())
    }

    /// Declines a workspace pattern that may match outside the project: one with a parent step or
    /// an absolute path.
    ///
    /// Every copied manifest is screened, so a nested workspace's patterns are too.
    fn screen_workspaces(
        &self,
        manifest: &Utf8Path,
        document: &Value,
    ) -> std::result::Result<(), Refusal> {
        let patterns = match document.get("workspaces") {
            Some(Value::Array(patterns)) => patterns,
            Some(Value::Object(object)) => match object.get("packages") {
                Some(Value::Array(patterns)) => patterns,
                _ => return Ok(()),
            },
            _ => return Ok(()),
        };
        for pattern in patterns.iter().filter_map(Value::as_str) {
            let parent_step = pattern.split(['/', '\\']).any(|segment| segment == "..");
            let absolute = pattern.starts_with(['/', '\\']) || Utf8Path::new(pattern).is_absolute();
            if parent_step || absolute {
                return decline(format!(
                    "{} declares the workspace pattern `{pattern}`, which may match outside the \
                     project the verification copy holds",
                    shown(&self.root, manifest)
                ));
            }
        }
        Ok(())
    }

    /// Stages what one range of the manifest at `manifest`, in `directory`, makes Yarn read.
    fn follow(
        &mut self,
        manifest: &Utf8Path,
        directory: &Utf8Path,
        range: &str,
    ) -> std::result::Result<(), Refusal> {
        if settings::is_exec(range) {
            return decline(format!(
                "{} declares the `exec:` range `{range}`, whose generator may read files the \
                 verification copy leaves out",
                shown(&self.root, manifest)
            ));
        }
        if let Some(patch) = range.strip_prefix("patch:") {
            return self.follow_patch(manifest, directory, patch);
        }
        let (local, path) = if let Some(path) = range.strip_prefix("file:") {
            (Local::File, path)
        } else if let Some(path) = range.strip_prefix("portal:") {
            (Local::Target, path)
        } else if let Some(path) = range.strip_prefix("link:") {
            (Local::Target, path)
        } else if is_path_range(range) {
            (Local::File, range)
        } else {
            return Ok(());
        };
        let path = path.split_once('#').map_or(path, |(path, _)| path);
        self.follow_local(manifest, directory, local, range, path)
    }

    /// Stages the local package `range` names at `path`, relative to `directory`.
    fn follow_local(
        &mut self,
        manifest: &Utf8Path,
        directory: &Utf8Path,
        local: Local,
        range: &str,
        path: &str,
    ) -> std::result::Result<(), Refusal> {
        let shown_manifest = shown(&self.root, manifest);
        if path.starts_with(['/', '\\']) || Utf8Path::new(path).is_absolute() {
            return decline(format!(
                "{shown_manifest} names the local package `{range}` by an absolute path, which \
                 the verification copy cannot redirect into itself"
            ));
        }
        let Some(lexical) = self.mirrored(directory, Utf8Path::new(path)) else {
            return decline(format!(
                "{shown_manifest} names the local package `{range}` outside {}, below which the \
                 verification copy mirrors the project",
                self.boundary
            ));
        };
        let reached = match dunce::canonicalize(&lexical) {
            Ok(reached) => utf8(reached)?,
            // Yarn fails on a missing local package in either run, before any verdict.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(filesystem(&lexical, &err).into()),
        };
        if reached != lexical {
            return decline(format!(
                "{shown_manifest} reaches the local package `{range}` through a symbolic link, \
                 which the verification copy does not reproduce"
            ));
        }
        let directory_target = is_directory(&lexical)?;
        match local {
            Local::File if directory_target => {
                self.stage_folder(&lexical)?;
                self.folders.push(lexical.clone());
                self.queue_manifest(&lexical)?;
            }
            Local::File => self.stage_file(&lexical)?,
            Local::Target if directory_target => {
                let target = lexical.join(MANIFEST);
                if is_file(&target)? {
                    self.pending.push(target);
                }
            }
            Local::Target => {}
        }
        Ok(())
    }

    /// Stages what a `patch:` range reads: the patch files its selector names, and whatever its
    /// source range reads.
    ///
    /// The range is `patch:<source>#<paths>`, the source an encoded descriptor and the paths
    /// separated by `&`, each optionally led by flags before a `!`.
    /// A path is relative to the declaring package, or to the project with a leading `~/`.
    fn follow_patch(
        &mut self,
        manifest: &Utf8Path,
        directory: &Utf8Path,
        patch: &str,
    ) -> std::result::Result<(), Refusal> {
        let body = patch.split_once("::").map_or(patch, |(body, _)| body);
        let (source, selector) = body.split_once('#').unwrap_or((body, ""));
        if let Some(source_range) = percent_decode(source).as_deref().and_then(descriptor_range) {
            self.follow(manifest, directory, source_range.as_str())?;
        }
        for selected in selector.split('&') {
            let file = selected.rsplit_once('!').map_or(selected, |(_, file)| file);
            if file.is_empty() || file.starts_with("builtin<") {
                continue;
            }
            let target = if let Some(relative) = file.strip_prefix("~/") {
                self.mirrored(&self.root, Utf8Path::new(relative))
            } else if Utf8Path::new(file).is_absolute() {
                self.read_in_place(Utf8PathBuf::from(file));
                continue;
            } else {
                self.mirrored(directory, Utf8Path::new(file))
            };
            let Some(target) = target else {
                return decline(format!(
                    "{} names the patch {file} outside {}, below which the verification copy \
                     mirrors the project",
                    shown(&self.root, manifest),
                    self.boundary
                ));
            };
            if is_file(&target)? {
                self.stage_file(&target)?;
            }
        }
        Ok(())
    }

    fn queue_manifest(&mut self, directory: &Utf8Path) -> std::result::Result<(), Refusal> {
        let manifest = directory.join(MANIFEST);
        if is_file(&manifest)? {
            self.pending.push(manifest);
        }
        Ok(())
    }

    /// Stages the folder `folder` whole, as Yarn archives a `file:` folder: every file and
    /// directory with its permissions, and every symbolic link with its own target, which must
    /// resolve inside the copy ([`check_links`](Self::check_links)).
    fn stage_folder(&mut self, folder: &Utf8Path) -> std::result::Result<(), Refusal> {
        let relative = self.relative(folder)?;
        self.insert(
            relative,
            Entry::Directory {
                source: folder.to_owned(),
            },
        )?;
        for Child { name, file_type } in children(&self.root, folder)? {
            let path = folder.join(&name);
            if file_type.is_dir() {
                self.stage_folder(&path)?;
            } else if file_type.is_symlink() {
                let target =
                    utf8(std::fs::read_link(&path).map_err(|err| filesystem(&path, &err))?)?;
                let relative = self.relative(&path)?;
                let directory = path.is_dir();
                self.insert(relative, Entry::Link { target, directory })?;
            } else if file_type.is_file() {
                self.stage_file(&path)?;
            }
        }
        Ok(())
    }

    /// Picks the first of `bases` whose relation to the home directory matches the project's,
    /// that lies outside the project and every copied folder, and above which Yarn would read no
    /// rc file the project does not share.
    fn choose_base(
        &self,
        home: &Utf8Path,
        under_home: bool,
        bases: &[Utf8PathBuf],
    ) -> std::result::Result<Utf8PathBuf, Refusal> {
        let names = self.rc_names();
        let mut reasons = Vec::new();
        for base in bases {
            let base = canonical_lenient(base)?;
            if base.starts_with(home) != under_home {
                let side = if under_home { "outside" } else { "inside" };
                reasons.push(format!("{base} lies {side} the home directory"));
                continue;
            }
            if base.starts_with(&self.root)
                || self.folders.iter().any(|folder| base.starts_with(folder))
            {
                reasons.push(format!(
                    "{base} lies inside the project or a folder it copies"
                ));
                continue;
            }
            if let Some(rc_file) = self.unshared_rc_file(&base, &names)? {
                reasons.push(format!(
                    "Yarn would read {rc_file} above a copy in {base}, and not for the project"
                ));
                continue;
            }
            return Ok(base);
        }
        decline(format!(
            "no scratch directory keeps the rc files Yarn reads in the order it reads them for \
             the project: {}",
            reasons.join("; ")
        ))
    }

    /// The first rc file, under any name Yarn reads, in a directory at or above `base` that is
    /// no ancestor of the project.
    fn unshared_rc_file(
        &self,
        base: &Utf8Path,
        names: &[&str],
    ) -> std::result::Result<Option<Utf8PathBuf>, Refusal> {
        for directory in base.ancestors() {
            if self.root.starts_with(directory) {
                continue;
            }
            for name in names {
                let path = directory.join(name);
                match std::fs::symlink_metadata(&path) {
                    Ok(_) => return Ok(Some(path)),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(filesystem(&path, &err).into()),
                }
            }
        }
        Ok(None)
    }

    /// Every rc file name Yarn reads: the project's, and `.yarnrc.yml`, which Yarn 4 reads in
    /// the home directory whatever the project's is.
    fn rc_names(&self) -> Vec<&'a str> {
        let mut names = vec![DEFAULT_RC_FILENAME];
        let renamed = self.environment.rc_filename();
        if renamed != DEFAULT_RC_FILENAME {
            names.push(renamed);
        }
        names
    }

    /// Stages or records every rc file Yarn reads for the project, and returns them with their
    /// settings.
    ///
    /// An rc file in a directory the scratch directory shares, or in the home directory, is read
    /// in place by both runs; any other is copied to its mirrored position.
    ///
    /// Every rc file is read once, and the bytes screened are the ones copied or, for one read in
    /// place, the ones writing the copy checks are still there.
    /// The directories above the scratch directory that the project does not share hold no rc
    /// file ([`choose_base`](Self::choose_base)), and are recorded as such, so one appearing
    /// before Yarn runs is caught too.
    fn stage_rc_files(
        &mut self,
        home: &Utf8Path,
        base: &Utf8Path,
    ) -> std::result::Result<Vec<RcFile>, Refusal> {
        let name = self.environment.rc_filename();
        let root = self.root.clone();
        let mut rc_files = Vec::new();
        for directory in root.ancestors() {
            let path = directory.join(name);
            let shared = base.starts_with(directory);
            let content = read_optional(&path)?;
            if shared {
                self.in_place
                    .insert(path.clone(), InPlace::Screened(content.clone()));
            }
            let Some(content) = content else {
                continue;
            };
            let settings = parse_settings(&root, &path, &content).map_err(Refusal::Decline)?;
            if !shared {
                self.stage_screened(&path, content)?;
            }
            rc_files.push(RcFile {
                path,
                staged: !shared,
                settings,
            });
        }
        for home_name in self.rc_names() {
            let path = home.join(home_name);
            if self.in_place.contains_key(&path) || rc_files.iter().any(|rc| rc.path == path) {
                continue;
            }
            let content = read_optional(&path)?;
            self.in_place
                .insert(path.clone(), InPlace::Screened(content.clone()));
            if let Some(content) = content {
                let settings = parse_settings(&root, &path, &content).map_err(Refusal::Decline)?;
                rc_files.push(RcFile {
                    path,
                    staged: false,
                    settings,
                });
            }
        }
        let names = self.rc_names();
        for directory in base.ancestors() {
            if root.starts_with(directory) {
                continue;
            }
            for name in &names {
                self.in_place
                    .entry(directory.join(name))
                    .or_insert(InPlace::Screened(None));
            }
        }
        Ok(rc_files)
    }

    /// Stages or records the Yarn release each `yarnPath` names, in an rc file or the
    /// environment.
    fn stage_yarn_paths(&mut self, rc_files: &[RcFile]) -> std::result::Result<(), Refusal> {
        for rc_file in rc_files {
            let source = shown(&self.root, &rc_file.path);
            match rc_file.settings.get("yarnPath") {
                None | Some(Value::Null) => {}
                Some(Value::String(path)) => {
                    let directory = parent(&rc_file.path).to_owned();
                    self.stage_yarn_path(&source, &directory, rc_file.staged, path)?;
                }
                Some(other) => {
                    return decline(format!("{source} sets `yarnPath` to `{other}`, no path"));
                }
            }
        }
        let root = self.root.clone();
        let environment = self.environment;
        for path in environment.variable("yarn_yarn_path") {
            // An environment path is relative to the directory Yarn starts in, the copy's root.
            self.stage_yarn_path("YARN_YARN_PATH", &root, true, path)?;
        }
        Ok(())
    }

    fn stage_yarn_path(
        &mut self,
        source: &str,
        directory: &Utf8Path,
        staged: bool,
        path: &str,
    ) -> std::result::Result<(), Refusal> {
        if path.contains("${") {
            return decline(format!(
                "{source} sets `yarnPath` to `{path}`, which Yarn interpolates"
            ));
        }
        let path = Utf8Path::new(path);
        if path.is_absolute() {
            self.read_in_place(path.to_owned());
            return Ok(());
        }
        if !staged {
            self.read_in_place(normalize(&directory.join(path)));
            return Ok(());
        }
        let Some(target) = self.mirrored(directory, path) else {
            return decline(format!(
                "{source} sets `yarnPath` to `{path}`, outside {}, below which the verification \
                 copy mirrors the project",
                self.boundary
            ));
        };
        if is_file(&target)? {
            self.stage_file(&target)?;
        }
        Ok(())
    }

    /// Where the relative `path` from `directory` leads, provided it stays below the boundary at
    /// every step, so that its copy at the mirrored position leads to the same place inside the
    /// copy.
    ///
    /// A path that climbs above the boundary and comes back, such as `../` repeated past the
    /// filesystem root, has no mirrored position: in the copy it would leave the tree.
    fn mirrored(&self, directory: &Utf8Path, path: &Utf8Path) -> Option<Utf8PathBuf> {
        let start = directory.strip_prefix(&self.boundary).ok()?;
        let within = resolve_in_copy(&BTreeMap::new(), start, path, 0)?;
        Some(self.boundary.join(within))
    }

    fn stage_file(&mut self, path: &Utf8Path) -> std::result::Result<(), Refusal> {
        let relative = self.relative(path)?;
        self.insert(
            relative,
            Entry::File {
                source: path.to_owned(),
            },
        )
    }

    /// Stages the file at `path` as the `bytes` a rule screened, in place of any copy of it a
    /// folder holds.
    fn stage_screened(
        &mut self,
        path: &Utf8Path,
        bytes: Vec<u8>,
    ) -> std::result::Result<(), Refusal> {
        let relative = self.relative(path)?;
        self.insert(
            relative,
            Entry::Screened {
                source: path.to_owned(),
                bytes,
            },
        )
    }

    /// Records `entry` at `relative`, where a second traversal may already have staged an entry
    /// of its own ([`merge`]): an identical one stays, a screened reading of a file replaces a
    /// plain copy of the same file, and any other collision declines, so the order of traversal
    /// never decides what the copy holds.
    fn insert(&mut self, relative: Utf8PathBuf, entry: Entry) -> std::result::Result<(), Refusal> {
        use std::collections::btree_map::Entry as Slot;
        let mut slot = match self.entries.entry(relative) {
            Slot::Vacant(slot) => {
                slot.insert(entry);
                return Ok(());
            }
            Slot::Occupied(slot) => slot,
        };
        match merge(slot.get(), &entry) {
            Merge::Keep => Ok(()),
            Merge::Replace => {
                slot.insert(entry);
                Ok(())
            }
            Merge::Conflict => decline(format!(
                "{} would be copied both as {} and as {}, which the verification copy cannot \
                 hold at once",
                shown(&self.root, &self.boundary.join(slot.key())),
                slot.get().kind(),
                entry.kind()
            )),
        }
    }

    /// Records a file Yarn reads in place that no rule screens, unless it is already recorded.
    fn read_in_place(&mut self, path: Utf8PathBuf) {
        self.in_place.entry(path).or_insert(InPlace::Read);
    }

    /// Declines any recreated link that could resolve outside the copy, or that the walk needs
    /// to resolve where the source's link does and would not.
    ///
    /// Each link is resolved as the copy's filesystem would resolve it: from its own position,
    /// through every other recreated link, with a `..` that would climb above the mirrored tree
    /// counting as an escape ([`resolve_in_copy`]).
    /// A link the source's root absorbed (`../` repeated past `/`, then back down into the
    /// project) therefore declines rather than reach the source from the copy.
    fn check_links(&self) -> std::result::Result<(), Refusal> {
        for (position, entry) in &self.entries {
            let Entry::Link { target, .. } = entry else {
                continue;
            };
            let resolved = resolve_in_copy(&self.entries, parent(position), target, 0);
            let expected = self.expected_links.get(position);
            let faithful = match (&resolved, expected) {
                (None, _) => false,
                (Some(resolved), Some(expected)) => resolved == expected,
                (Some(_), None) => true,
            };
            if !faithful {
                return decline(format!(
                    "{} links to {target}, which the verification copy cannot recreate to \
                     resolve where it does, inside the copy",
                    shown(&self.root, &self.boundary.join(position))
                ));
            }
        }
        Ok(())
    }

    /// The position of `path` below the boundary, where the copy mirrors it.
    fn relative(&self, path: &Utf8Path) -> std::result::Result<Utf8PathBuf, Refusal> {
        match path.strip_prefix(&self.boundary) {
            Ok(relative) => Ok(relative.to_owned()),
            Err(_) => decline(format!(
                "{} lies outside {}, below which the verification copy mirrors the project",
                shown(&self.root, path),
                self.boundary
            )),
        }
    }
}

/// How a local range reaches its package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Local {
    /// `file:`: a folder Yarn archives whole, or an archive.
    File,
    /// `portal:` or `link:`: a folder whose manifest is read in place.
    Target,
}

/// Every string range in a manifest's range fields.
fn ranges(document: &Value) -> Vec<&str> {
    RANGE_FIELDS
        .iter()
        .filter_map(|field| document.get(field).and_then(Value::as_object))
        .flat_map(|ranges| ranges.values().filter_map(Value::as_str))
        .collect()
}

/// Whether a protocol-less range is a path Yarn reads as a `file:` range: a relative or absolute
/// path, or an archive name.
fn is_path_range(range: &str) -> bool {
    let drive = range.as_bytes().get(1..3).is_some_and(|tail| {
        tail.first() == Some(&b':')
            && tail
                .get(1)
                .is_some_and(|slash| matches!(slash, b'/' | b'\\'))
    }) && range
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphabetic);
    range.starts_with("./")
        || range.starts_with("../")
        || range.starts_with('/')
        || drive
        || (!range.contains(':') && is_archive_name(range))
}

/// Whether `name` ends in `.tgz` or `.tar.gz`, compared without case, a superset of the names
/// Yarn reads as archives.
fn is_archive_name(name: &str) -> bool {
    let path = Utf8Path::new(name);
    let extension = |path: &Utf8Path, expected: &str| {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
    };
    extension(path, "tgz")
        || (extension(path, "gz")
            && path
                .file_stem()
                .is_some_and(|stem| extension(Utf8Path::new(stem), "tar")))
}

/// The range of a descriptor `name@range`, the name possibly scoped.
fn descriptor_range(descriptor: &str) -> Option<String> {
    let after_scope = descriptor.get(1..)?;
    let (_, range) = after_scope.split_once('@')?;
    Some(range.to_string())
}

/// Where the link target `target`, at a link in `directory`, resolves in the copy, relative to
/// the mirrored tree, or `None` when the resolution leaves the tree, is absolute, or loops.
///
/// Components are followed in order as the filesystem follows them: a name that is itself a
/// recreated link continues from that link's own resolution, so a `..` after it climbs from
/// where the link leads rather than from where it sits.
/// A position the copy does not hold is taken as a plain directory, which can only keep a
/// resolution inside the tree, since the filesystem would stop there.
pub(super) fn resolve_in_copy(
    entries: &BTreeMap<Utf8PathBuf, Entry>,
    directory: &Utf8Path,
    target: &Utf8Path,
    hops: usize,
) -> Option<Utf8PathBuf> {
    if hops > MAX_LINK_HOPS {
        return None;
    }
    let mut position: Vec<String> = directory
        .components()
        .map(|component| component.as_str().to_string())
        .collect();
    for component in target.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                position.pop()?;
            }
            Utf8Component::Normal(name) => {
                position.push(name.to_string());
                let here: Utf8PathBuf = position.iter().collect();
                if let Some(Entry::Link { target: next, .. }) = entries.get(&here) {
                    position.pop();
                    let from: Utf8PathBuf = position.iter().collect();
                    let resolved = resolve_in_copy(entries, &from, next, hops + 1)?;
                    position = resolved
                        .components()
                        .map(|component| component.as_str().to_string())
                        .collect();
                }
            }
            Utf8Component::RootDir | Utf8Component::Prefix(_) => return None,
        }
    }
    Some(position.iter().collect())
}

/// `path` with `.` dropped and each `..` removing the component before it, without reading the
/// filesystem.
fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut normalized = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_str()),
        }
    }
    normalized
}

fn parent(path: &Utf8Path) -> &Utf8Path {
    path.parent().unwrap_or(path)
}

/// `path` as a reason names it: relative to the project and `/`-separated inside it, and
/// absolute outside it, such as an rc file in the home directory.
pub(super) fn shown(root: &Utf8Path, path: &Utf8Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) if relative.as_str().is_empty() => ".".to_string(),
        Ok(relative) => display_rel(relative),
        Err(_) => path.to_string(),
    }
}

/// One entry of a directory being read.
struct Child {
    name: String,
    file_type: std::fs::FileType,
}

/// The entries of `directory`, sorted by name, a reason naming it as [`shown`] from `root`.
fn children(root: &Utf8Path, directory: &Utf8Path) -> std::result::Result<Vec<Child>, Refusal> {
    let entries = std::fs::read_dir(directory).map_err(|err| filesystem(directory, &err))?;
    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| filesystem(directory, &err))?;
        let name = entry.file_name().into_string().map_err(|name| {
            Refusal::Decline(format!(
                "{} holds an entry whose name is not UTF-8: {}",
                shown(root, directory),
                name.to_string_lossy()
            ))
        })?;
        let file_type = entry
            .file_type()
            .map_err(|err| filesystem(directory, &err))?;
        children.push(Child { name, file_type });
    }
    children.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(children)
}

/// The bytes of the file at `path`, or `None` when it does not exist.
fn read_optional(path: &Utf8Path) -> std::result::Result<Option<Vec<u8>>, Refusal> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(filesystem(path, &err).into()),
    }
}

fn is_file(path: &Utf8Path) -> std::result::Result<bool, Refusal> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(filesystem(path, &err).into()),
    }
}

fn is_directory(path: &Utf8Path) -> std::result::Result<bool, Refusal> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(filesystem(path, &err).into()),
    }
}

/// The canonical form of an existing `path`, without Windows' verbatim prefix.
pub(super) fn canonical(path: &std::path::Path) -> std::result::Result<Utf8PathBuf, Refusal> {
    let canonical = dunce::canonicalize(path).map_err(|err| {
        Refusal::Error(CoreError::Filesystem(format!("{}: {err}", path.display())))
    })?;
    utf8(canonical)
}

/// The canonical form of `path`, whose trailing components need not exist yet.
fn canonical_lenient(path: &Utf8Path) -> std::result::Result<Utf8PathBuf, Refusal> {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        match dunce::canonicalize(existing) {
            Ok(found) => {
                let mut found = utf8(found)?;
                for name in missing.iter().rev() {
                    found.push(name);
                }
                return Ok(found);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
                    return Err(filesystem(path, &err).into());
                };
                missing.push(name);
                existing = parent;
            }
            Err(err) => return Err(filesystem(existing, &err).into()),
        }
    }
}

fn utf8(path: std::path::PathBuf) -> std::result::Result<Utf8PathBuf, Refusal> {
    Utf8PathBuf::from_path_buf(path).map_err(|path| {
        Refusal::Decline(format!(
            "the path {} is not UTF-8, which the verification copy cannot name",
            path.display()
        ))
    })
}

fn decline<T>(reason: impl Into<String>) -> std::result::Result<T, Refusal> {
    Err(Refusal::Decline(reason.into()))
}
