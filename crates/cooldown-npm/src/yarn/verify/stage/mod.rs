//! The copy of a Yarn Berry project a verification runs in: every input Yarn reads, where Yarn
//! looks for it, and nothing Yarn could write back to the source through.
//!
//! # Placement
//!
//! The copy mirrors the source below a boundary: the home directory when the project lies below
//! it, the filesystem root otherwise.
//! Its scratch directory is created in the first base directory that has the same relation to the
//! home directory, so Yarn meets the home directory, and every directory the two paths share, in
//! the same order in both runs and reads their rc files in place.
//! The home directory is never replaced, so its rc file and Corepack's cache are the real ones.
//! The project's rc file, and each one between the project and the boundary that the scratch
//! directory does not share, is copied to its mirrored position.
//! An rc file above the scratch directory that the source does not share would be read only in
//! the copy, so a base directory with one is not used.
//!
//! # Contents
//!
//! The copy holds:
//!
//! - `yarn.lock`, `.env.yarn`, and the `.yarn/releases` and `.yarn/patches` folders;
//! - every `package.json` below the project outside `node_modules`, `.git`, and `.yarn`, a superset
//!   of the manifests Yarn's workspace glob reads, and the symbolic links that glob follows;
//! - each `file:` folder whole, since Yarn archives it and hashes the archive into the lock, and
//!   each `file:` archive;
//! - the manifest of each `portal:` and `link:` target;
//! - each patch file a `patch:` range names;
//! - each `yarnPath` a copied rc file names by a relative path.
//!
//! Local references are followed from every copied manifest, those of `file:`, `portal:`, and
//! `link:` targets included.
//! Yarn's own output folders are redirected into the scratch directory by the run's settings
//! ([`Outputs`]).
//!
//! # Declines
//!
//! An input the copy cannot reproduce faithfully declines rather than risk a false verdict: an
//! `exec:` range anywhere a resolver reads one, a workspace pattern that may leave the project, a
//! local reference by an absolute path or through a symbolic link, a relative path that climbs
//! above the boundary at any step (even one that comes back down, which the copy would resolve
//! outside its tree), a symbolic link whose copy would resolve outside the copy or elsewhere than
//! the source's, and the settings [`settings`](super::settings) screens.
//!
//! # Screened bytes
//!
//! Every file a rule screens (the lock, each followed manifest, and each rc file) is read once,
//! while planning, and the copy holds exactly those bytes.
//! Writing the copy reads each such source again and declines if it no longer holds the screened
//! bytes, so Yarn never runs on, or beside, a file no rule saw.
//! The same check covers the rc files Yarn reads in place, absent ones included.
//! Those stay in the home directory and the directories the scratch directory shares, outside the
//! project, so an edit between that check and Yarn's own read remains possible there.
//!
//! # Digest
//!
//! The [`InputDigest`] covers every entry of the copy, with the bytes and permissions written,
//! and every input read in place, absent ones included, so planning again after verification
//! tells whether any input changed or appeared.

mod planner;
#[cfg(test)]
mod tests;

use crate::manifest::display_rel;
use crate::manifest::workspaces::YarnEnvironment;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, Result};
use planner::{Planner, canonical};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;

/// A SHA-256 digest of every input a verification copied or read in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::yarn) struct InputDigest([u8; 32]);

/// The folders Yarn writes outside the project, each redirected into the scratch directory.
#[derive(Debug, Clone)]
pub(super) struct Outputs {
    pub(super) cache: Utf8PathBuf,
    pub(super) global: Utf8PathBuf,
    pub(super) install_state: Utf8PathBuf,
    /// Ends in `__virtual__`, the only name Yarn accepts for it.
    pub(super) virtual_folder: Utf8PathBuf,
    pub(super) unplugged: Utf8PathBuf,
}

impl Outputs {
    /// The output folders of a copy in the scratch directory `scratch`, beside its tree.
    pub(super) fn under(scratch: &Utf8Path) -> Self {
        let out = scratch.join("out");
        Outputs {
            cache: out.join("cache"),
            global: out.join("global"),
            install_state: out.join("install-state.gz"),
            virtual_folder: out.join("virtual").join("__virtual__"),
            unplugged: out.join("unplugged"),
        }
    }
}

/// Whether a project can be copied for verification, and the copy if it can.
pub(super) enum Planned {
    Ready(Plan),
    /// Why the copy could not reproduce what Yarn reads for the project.
    Declined(String),
}

/// What a copy holds and where it may be created, read from the source but not yet written.
#[derive(Debug)]
pub(super) struct Plan {
    /// The project root, canonical.
    root: Utf8PathBuf,
    /// The directory the copy mirrors the source below.
    boundary: Utf8PathBuf,
    /// The directory the scratch directory is created in.
    base: Utf8PathBuf,
    /// The `file:` folders copied whole, which the scratch directory must stay out of.
    folders: Vec<Utf8PathBuf>,
    /// Every entry of the copy, by its position below the boundary.
    entries: BTreeMap<Utf8PathBuf, Entry>,
    /// The files Yarn reads in place in both runs, which the digest covers where they are.
    in_place: BTreeMap<Utf8PathBuf, InPlace>,
}

/// One entry of the copy.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A regular file whose bytes no rule screens, copied from `source` with its permissions.
    File { source: Utf8PathBuf },
    /// A file planning screened, copied as exactly `bytes`, with the permissions of `source`,
    /// which must still hold those bytes when the copy is written.
    Screened { source: Utf8PathBuf, bytes: Vec<u8> },
    /// A directory of a folder copied whole, created even when empty, with the permissions of
    /// `source`.
    Directory { source: Utf8PathBuf },
    /// A symbolic link recreated with its relative `target`, which resolves inside the copy
    /// ([`resolve_in_copy`](planner::resolve_in_copy)).
    Link {
        target: Utf8PathBuf,
        directory: bool,
    },
}

impl Entry {
    /// What the entry is, as a reason names it.
    fn kind(&self) -> &'static str {
        match self {
            Entry::File { .. } => "a file",
            Entry::Screened { .. } => "a screened file",
            Entry::Directory { .. } => "a directory",
            Entry::Link { .. } => "a symbolic link",
        }
    }
}

/// What staging `proposed` at a position that already holds `existing` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Merge {
    Keep,
    Replace,
    /// The two cannot both be the copy's entry there.
    Conflict,
}

/// The rule for two traversals reaching the same position: only an identical proposal, or the
/// screened and the plain reading of one source file, which merge into the screened one.
///
/// Anything else, such as a file one traversal reads through a symbolic link that another copies
/// as the link, a directory and a file, or two link targets, has no entry that is faithful to
/// both, so the caller declines.
fn merge(existing: &Entry, proposed: &Entry) -> Merge {
    match (existing, proposed) {
        _ if existing == proposed => Merge::Keep,
        (
            Entry::File { source },
            Entry::Screened {
                source: screened, ..
            },
        ) if source == screened => Merge::Replace,
        (
            Entry::Screened {
                source: screened, ..
            },
            Entry::File { source },
        ) if source == screened => Merge::Keep,
        _ => Merge::Conflict,
    }
}

/// What a file Yarn reads in place is to the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InPlace {
    /// An rc file planning screened: its bytes, or `None` when it did not exist.
    Screened(Option<Vec<u8>>),
    /// A file no rule screens, such as a Yarn release.
    Read,
}

/// Whether the plan's scratch directory could be used, and the copy written there if it could.
pub(super) enum Materialized {
    Ready(Box<Staged>),
    Declined(String),
}

/// A written copy, deleted with its scratch directory when dropped.
#[derive(Debug)]
pub(super) struct Staged {
    /// Owns the scratch directory: dropping it deletes the copy and Yarn's output with it.
    pub(super) scratch: Scratch,
    /// The project's position in the copy, where Yarn runs.
    pub(super) copy_root: Utf8PathBuf,
    pub(super) outputs: Outputs,
    /// The digest of exactly the bytes copied and the inputs read in place.
    pub(super) digest: InputDigest,
}

/// Why planning stopped: a reason to decline, or an input that could not be read.
enum Refusal {
    Decline(String),
    Error(CoreError),
}

impl From<CoreError> for Refusal {
    fn from(err: CoreError) -> Self {
        Refusal::Error(err)
    }
}

/// Plans the copy of the project at `root`, whose `yarn.lock` holds `lock`, for a Yarn reading
/// `environment`, in the first of `bases` that reproduces where Yarn looks for rc files.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] for a lock the Berry reader refuses, and
/// [`CoreError::Filesystem`] when an input cannot be read.
pub(super) fn plan(
    root: &Utf8Path,
    lock: &str,
    environment: &YarnEnvironment,
    bases: &[Utf8PathBuf],
) -> Result<Planned> {
    match Planner::plan(root, lock, environment, bases) {
        Ok(plan) => Ok(Planned::Ready(plan)),
        Err(Refusal::Decline(reason)) => Ok(Planned::Declined(reason)),
        Err(Refusal::Error(err)) => Err(err),
    }
}

impl Plan {
    /// The digest the copy would have if it were written now, or `None` when a screened input
    /// no longer holds the bytes planning screened.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Filesystem`] when an input cannot be read.
    pub(super) fn digest(&self) -> Result<Option<InputDigest>> {
        Ok(match self.copy(None)? {
            Copied::Digest(digest) => Some(digest),
            Copied::Changed(_) => None,
        })
    }

    /// Writes the copy into a new scratch directory in the plan's base directory, or declines when
    /// a screened input changed since planning.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Filesystem`] when an input cannot be read or the copy written.
    pub(super) fn materialize(self) -> Result<Materialized> {
        std::fs::create_dir_all(&self.base).map_err(|err| filesystem(&self.base, &err))?;
        let scratch = Scratch {
            directory: tempfile::Builder::new()
                .prefix("cooldown-yarn-")
                .tempdir_in(&self.base)
                .map_err(|err| filesystem(&self.base, &err))?,
        };
        let anchor = match canonical(scratch.path()) {
            Ok(anchor) => anchor,
            Err(Refusal::Decline(reason)) => return Ok(Materialized::Declined(reason)),
            Err(Refusal::Error(err)) => return Err(err),
        };
        if anchor.starts_with(&self.root)
            || self
                .folders
                .iter()
                .any(|folder| anchor.starts_with(folder) || folder.starts_with(&anchor))
        {
            return Ok(Materialized::Declined(format!(
                "the scratch directory {anchor} overlaps the project or a folder it copies"
            )));
        }
        let tree = anchor.join("tree");
        let digest = match self.copy(Some(&tree))? {
            Copied::Digest(digest) => digest,
            Copied::Changed(reason) => return Ok(Materialized::Declined(reason)),
        };
        let copy_root = match self.root.strip_prefix(&self.boundary) {
            Ok(relative) => tree.join(relative),
            Err(_) => tree.clone(),
        };
        std::fs::create_dir_all(&copy_root).map_err(|err| filesystem(&copy_root, &err))?;
        Ok(Materialized::Ready(Box::new(Staged {
            outputs: Outputs::under(&anchor),
            scratch,
            copy_root,
            digest,
        })))
    }

    /// Reads every input, writing the copy below `tree` when one is given, and returns the
    /// digest of what was read, or why a screened input no longer holds its screened bytes.
    ///
    /// One pass both writes and digests, so a written copy's digest covers exactly its bytes and
    /// permissions.
    /// Directory permissions are applied once the tree is complete, deepest first, since a
    /// directory Yarn would archive read-only could not otherwise be filled.
    fn copy(&self, tree: Option<&Utf8Path>) -> Result<Copied> {
        let mut digest = Digester::default();
        let mut directories = Vec::new();
        for (relative, entry) in &self.entries {
            let destination = tree.map(|tree| tree.join(relative));
            match entry {
                Entry::Directory { source } => {
                    let permissions = permissions(source)?;
                    if let Some(destination) = destination {
                        std::fs::create_dir_all(&destination)
                            .map_err(|err| filesystem(&destination, &err))?;
                        directories.push(Permitted {
                            path: destination,
                            permissions: permissions.clone(),
                        });
                    }
                    digest.record(Record::Directory(mode(&permissions)), relative, &[]);
                }
                Entry::File { source } => {
                    let bytes = std::fs::read(source).map_err(|err| filesystem(source, &err))?;
                    let permissions = permissions(source)?;
                    if let Some(destination) = &destination {
                        write_file(destination, &bytes, permissions.clone())?;
                    }
                    digest.record(Record::File(mode(&permissions)), relative, &bytes);
                }
                Entry::Screened { source, bytes } => {
                    if read_optional(source)?.as_ref() != Some(bytes) {
                        return Ok(Copied::Changed(changed(source)));
                    }
                    let permissions = permissions(source)?;
                    if let Some(destination) = &destination {
                        write_file(destination, bytes, permissions.clone())?;
                    }
                    digest.record(Record::File(mode(&permissions)), relative, bytes);
                }
                Entry::Link { target, directory } => {
                    if let Some(destination) = &destination {
                        create_parent(destination)?;
                        symlink(target, destination, *directory)?;
                    }
                    digest.record(Record::Link, relative, target.as_str().as_bytes());
                }
            }
        }
        for Permitted { path, permissions } in directories.into_iter().rev() {
            std::fs::set_permissions(&path, permissions).map_err(|err| filesystem(&path, &err))?;
        }
        for (path, in_place) in &self.in_place {
            if path.is_dir() {
                digest.record(Record::InPlaceDirectory, path, &[]);
                continue;
            }
            let bytes = read_optional(path)?;
            if let InPlace::Screened(screened) = in_place
                && *screened != bytes
            {
                return Ok(Copied::Changed(changed(path)));
            }
            match bytes {
                Some(bytes) => digest.record(Record::InPlace, path, &bytes),
                None => digest.record(Record::Absent, path, &[]),
            }
        }
        Ok(Copied::Digest(digest.finish()))
    }
}

/// What one pass over the inputs found.
enum Copied {
    Digest(InputDigest),
    /// Why a screened input no longer holds the bytes planning screened.
    Changed(String),
}

/// A directory of the copy and the permissions it takes once it is filled.
struct Permitted {
    path: Utf8PathBuf,
    permissions: std::fs::Permissions,
}

/// The scratch directory of a copy, deleted when dropped.
///
/// The copy reproduces the source's permissions, read-only directories and files included, so
/// dropping it first makes every entry writable again; otherwise the deletion would fail and
/// leave the copy behind.
#[derive(Debug)]
pub(super) struct Scratch {
    directory: tempfile::TempDir,
}

impl Scratch {
    pub(super) fn path(&self) -> &std::path::Path {
        self.directory.path()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        make_writable(self.directory.path());
    }
}

/// Makes `directory` and everything below it writable by its owner, without following links,
/// skipping whatever it cannot change.
fn make_writable(directory: &std::path::Path) {
    let Ok(metadata) = std::fs::symlink_metadata(directory) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let mut permissions = metadata.permissions();
    writable(&mut permissions, true);
    if std::fs::set_permissions(directory, permissions).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => make_writable(&path),
            Ok(file_type) if file_type.is_file() => {
                if let Ok(metadata) = std::fs::metadata(&path) {
                    let mut permissions = metadata.permissions();
                    writable(&mut permissions, false);
                    // A file still read-only fails only its own deletion, which the scratch
                    // directory's removal then reports to nobody, as any leftover would.
                    std::fs::set_permissions(&path, permissions).ok();
                }
            }
            _ => {}
        }
    }
}

/// Grants the owner write access, and for a directory also read and search access.
#[cfg(unix)]
fn writable(permissions: &mut std::fs::Permissions, directory: bool) {
    use std::os::unix::fs::PermissionsExt as _;
    let owner = if directory { 0o700 } else { 0o200 };
    permissions.set_mode(permissions.mode() | owner);
}

/// Clears the read-only attribute, the only permission Windows keeps.
#[cfg(not(unix))]
fn writable(permissions: &mut std::fs::Permissions, _directory: bool) {
    permissions.set_readonly(false);
}

fn permissions(path: &Utf8Path) -> Result<std::fs::Permissions> {
    Ok(std::fs::metadata(path)
        .map_err(|err| filesystem(path, &err))?
        .permissions())
}

/// The bytes of the file at `path`, or `None` when it does not exist.
fn read_optional(path: &Utf8Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(filesystem(path, &err)),
    }
}

fn changed(path: &Utf8Path) -> String {
    format!("{path} changed while cooldown copied the project, so no screened copy of it exists")
}

fn filesystem(path: &Utf8Path, err: &std::io::Error) -> CoreError {
    CoreError::Filesystem(format!("{path}: {err}"))
}

fn create_parent(path: &Utf8Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| filesystem(parent, &err))?;
    }
    Ok(())
}

fn write_file(path: &Utf8Path, bytes: &[u8], permissions: std::fs::Permissions) -> Result<()> {
    create_parent(path)?;
    std::fs::write(path, bytes).map_err(|err| filesystem(path, &err))?;
    // Yarn archives a `file:` folder with each entry's permission bits, which the locator's hash
    // covers.
    std::fs::set_permissions(path, permissions).map_err(|err| filesystem(path, &err))
}

/// The permission bits of an entry, all Yarn archives of it: Yarn copies a folder with its
/// modes masked to `0o777` and every time set to one fixed instant (`copyPromise.ts`), so
/// ownership and timestamps reach no locator.
/// Windows keeps only the read-only attribute, from which Node derives the whole mode.
#[cfg(unix)]
fn mode(permissions: &std::fs::Permissions) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    permissions.mode() & 0o777
}

#[cfg(not(unix))]
fn mode(permissions: &std::fs::Permissions) -> u32 {
    u32::from(permissions.readonly())
}

#[cfg(unix)]
fn symlink(target: &Utf8Path, link: &Utf8Path, _directory: bool) -> Result<()> {
    std::os::unix::fs::symlink(target, link).map_err(|err| filesystem(link, &err))
}

#[cfg(windows)]
fn symlink(target: &Utf8Path, link: &Utf8Path, directory: bool) -> Result<()> {
    let created = if directory {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    };
    created.map_err(|err| filesystem(link, &err))
}

/// What one digest record covers.
#[derive(Clone, Copy)]
enum Record {
    /// A copied file, with its mode.
    File(u32),
    /// A copied directory, with its mode.
    Directory(u32),
    Link,
    /// A file read in place.
    InPlace,
    /// A directory where an input read in place was expected.
    InPlaceDirectory,
    /// A file read in place that does not exist.
    Absent,
}

/// Accumulates digest records, each framed so no two record sequences hash alike.
#[derive(Default)]
struct Digester {
    hasher: Sha256,
}

impl Digester {
    fn record(&mut self, record: Record, path: &Utf8Path, content: &[u8]) {
        let (tag, mode) = match record {
            Record::File(mode) => (b'f', mode),
            Record::Directory(mode) => (b'd', mode),
            Record::Link => (b'l', 0),
            Record::InPlace => (b'p', 0),
            Record::InPlaceDirectory => (b'D', 0),
            Record::Absent => (b'a', 0),
        };
        self.hasher.update([tag]);
        self.hasher.update(mode.to_le_bytes());
        self.frame(display_rel(path).as_bytes());
        self.frame(content);
    }

    fn frame(&mut self, bytes: &[u8]) {
        self.hasher
            .update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
        self.hasher.update(bytes);
    }

    fn finish(self) -> InputDigest {
        InputDigest(self.hasher.finalize().into())
    }
}
