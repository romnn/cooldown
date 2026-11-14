//! Filesystem state and lifecycle values for recoverable project mutations.

use crate::error::{CoreError, Result};
use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest as _, Sha256};

/// The project-bound pre-change state a planned mutation may rewrite.
///
/// Entries are project-relative, except for shared files captured through
/// [`capture_with_external`](Self::capture_with_external) outside the project root, which are
/// recorded under their canonical absolute path.
#[derive(Debug, Clone)]
pub struct ProjectMutationJournal {
    project: ProjectMutationRoot,
    files: Vec<ProjectMutationFile>,
    shared: Vec<SharedMutationFile>,
}

/// A file outside the project's ownership that a mutation may rewrite, such as a Go workspace's
/// `go.work`, bound to the directory it was resolved in.
#[derive(Debug, Clone)]
struct SharedMutationFile {
    /// The file as the caller named it, whose directory is resolved again on every validation so
    /// a retargeted symbolic link in it is refused like a replaced project root.
    named: Utf8PathBuf,
    /// The canonical parent directory and its filesystem identity.
    parent: ProjectMutationRoot,
    /// The canonical path of the file, which [`ProjectMutationJournal::shared_files`] reports.
    path: Utf8PathBuf,
}

/// The post-apply state observed at a rollback boundary.
///
/// A mutation journal captures the pre-image.
/// This state captures the corresponding post-image so rollback can refuse to overwrite a later
/// independent change while cooldown is verifying the candidate.
/// This observation closes cooldown's ownership boundary at adapter return.
/// A writer that does not honor cooldown's project lease can still race the adapter's own final
/// write.
/// Rollback detects later drift but cannot prove authorship for bytes already present at this
/// boundary.
#[derive(Debug, Clone)]
pub struct ProjectMutationState {
    project: ProjectMutationRoot,
    files: Vec<ProjectMutationFile>,
}

#[derive(Debug, Clone)]
struct ProjectMutationRoot {
    path: Utf8PathBuf,
    identity: std::sync::Arc<same_file::Handle>,
}

impl ProjectMutationState {
    /// The ordered file entries carried by this state.
    #[must_use]
    pub fn files(&self) -> &[ProjectMutationFile] {
        &self.files
    }

    /// Adds paths absent from this state without changing existing expected contents.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the states belong to different projects.
    pub fn extend_missing(&mut self, other: ProjectMutationState) -> Result<()> {
        self.ensure_same_project(&other.project)?;
        for file in other.files {
            if self.files.iter().all(|existing| existing.path != file.path) {
                self.files.push(file);
            }
        }
        Ok(())
    }

    /// Replaces expected contents for every path carried by `other`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the states belong to different projects.
    pub fn replace(&mut self, other: ProjectMutationState) -> Result<()> {
        self.ensure_same_project(&other.project)?;
        for file in other.files {
            if let Some(existing) = self
                .files
                .iter_mut()
                .find(|existing| existing.path == file.path)
            {
                *existing = file;
            } else {
                self.files.push(file);
            }
        }
        Ok(())
    }

    /// Projects this observed state onto a journal's ordered write set.
    ///
    /// This lets a nested transaction validate only the files it owns while retaining the exact
    /// postimage captured at the outer command boundary.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the requested write set is invalid or is not a
    /// subset of this state.
    pub fn state_for(&self, write_set: &ProjectMutationJournal) -> Result<Self> {
        self.ensure_same_project(&write_set.project)?;
        Ok(ProjectMutationState {
            project: self.project.clone(),
            files: projected_files(&self.files, &write_set.files)?,
        })
    }

    /// Captures a journal's portable write set under another project root.
    ///
    /// This is the explicit boundary used to inspect an isolated project copy without transferring
    /// the source journal's restore authority to that copy.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] when the project identity or any file cannot be captured safely.
    /// Writable paths with a symbolic-link ancestor or final entry are rejected because another
    /// project lease may govern their resolved target.
    /// Existing multiply-linked files are rejected for the same reason.
    pub fn capture(root: &Utf8Path, write_set: &ProjectMutationJournal) -> Result<Self> {
        // A shared file outside the project has no position under another root.
        if let Some(file) = write_set
            .files
            .iter()
            .find(|file| !safe_mutation_path(&file.path))
        {
            return Err(CoreError::LockConflict(format!(
                "cannot capture shared file {} under another project root",
                file.path
            )));
        }
        let project = ProjectMutationRoot::capture(root)?;
        let files = write_set
            .files
            .iter()
            .map(|file| ProjectMutationFile::capture(project.path(), &file.path))
            .collect::<Result<Vec<_>>>()?;
        Ok(ProjectMutationState { project, files })
    }

    fn ensure_same_project(&self, other: &ProjectMutationRoot) -> Result<()> {
        self.project.ensure_same(other)
    }
}

/// Read-only project files an adapter captured when a mutation was prepared, the facts every trial
/// of that mutation is judged against.
///
/// Unlike a [`ProjectMutationJournal`], these files are never written or restored, so a plain read
/// that follows symbolic and hard links captures them, and capturing one grants no write
/// authority.
/// A prepared mutation shares one baseline with every recovery subset derived from it.
///
/// Beside the files, an adapter may [record](Self::record) a fact it derived at capture beyond any
/// one file's bytes, such as the members it found by walking the project, so every trial reads
/// the capture-time fact rather than re-deriving it from a project the trials may have changed.
#[derive(Debug, Clone, Default)]
pub struct MutationBaseline {
    files: Vec<BaselineFile>,
    /// Adapter-derived facts, by the name the adapter records them under.
    records: std::collections::BTreeMap<String, String>,
}

/// One project-relative file of a [`MutationBaseline`], with the bytes read at capture.
#[derive(Debug, Clone)]
pub struct BaselineFile {
    path: Utf8PathBuf,
    contents: Option<Vec<u8>>,
}

impl MutationBaseline {
    /// Reads each project-relative path under `root`, recording an absent file as `None`.
    ///
    /// Duplicate paths are read once, in first-seen order.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Filesystem`] when a path is absolute or leaves the project through a
    /// `..` component, or when an existing file cannot be read.
    pub fn read<I, P>(root: &Utf8Path, paths: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Utf8Path>,
    {
        let mut files: Vec<BaselineFile> = Vec::new();
        for path in paths {
            let path = path.as_ref();
            let contained = path
                .components()
                .all(|component| matches!(component, camino::Utf8Component::Normal(_)));
            if !contained {
                return Err(CoreError::Filesystem(format!(
                    "mutation baseline path is not project-relative: {path}"
                )));
            }
            if files.iter().any(|file| file.path == path) {
                continue;
            }
            let absolute = root.join(path);
            let contents = match std::fs::read(&absolute) {
                Ok(bytes) => Some(bytes),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => return Err(CoreError::Filesystem(format!("{absolute}: {err}"))),
            };
            if let Some(bytes) = &contents {
                crate::trace::artifact(
                    crate::trace::ArtifactSource::File {
                        path: absolute.as_str(),
                        role: crate::trace::InputRole::ResolverInput,
                    },
                    bytes,
                );
            }
            files.push(BaselineFile {
                path: path.to_owned(),
                contents,
            });
        }
        Ok(MutationBaseline {
            files,
            records: std::collections::BTreeMap::new(),
        })
    }

    /// The captured files, in capture order.
    #[must_use]
    pub fn files(&self) -> &[BaselineFile] {
        &self.files
    }

    /// Records `fact` under `name`, replacing an earlier fact of that name.
    ///
    /// The name is the adapter's own, so it should carry the adapter's prefix.
    pub fn record(&mut self, name: impl Into<String>, fact: impl Into<String>) {
        self.records.insert(name.into(), fact.into());
    }

    /// The fact recorded under `name`, or `None` when the adapter recorded none.
    #[must_use]
    pub fn recorded(&self, name: &str) -> Option<&str> {
        self.records.get(name).map(String::as_str)
    }
}

impl BaselineFile {
    /// The project-relative path.
    #[must_use]
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }

    /// The bytes read at capture, or `None` when the file did not exist.
    #[must_use]
    pub fn contents(&self) -> Option<&[u8]> {
        self.contents.as_deref()
    }
}

/// Resolution inputs whose identity and contents must still match before publication.
#[derive(Debug, Clone, Default)]
pub struct ProjectInputSnapshot {
    files: Vec<ProjectInputFile>,
}

#[derive(Debug, Clone)]
struct ProjectInputFile {
    path: Utf8PathBuf,
    file_identity: ProjectInputIdentity,
    symlink_target: Option<std::path::PathBuf>,
    digest: [u8; 32],
    permissions: std::fs::Permissions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectInputIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows(std::sync::Arc<same_file::Handle>),
}

impl ProjectInputSnapshot {
    /// Captures file inputs by absolute path, including links to regular files.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] when a path is relative, does not resolve to a regular file, changes
    /// while being read, or cannot be read.
    pub fn capture(paths: &[Utf8PathBuf]) -> Result<Self> {
        Self::capture_with_contents(paths, |_, _, _| Ok(()))
    }

    /// Captures file inputs and visits each validated content buffer before moving to the next.
    ///
    /// The callback can copy a staged input from the same bytes used for its digest, avoiding a
    /// second source read while preserving identity and permission validation.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] under the same conditions as [`Self::capture`], or when the callback
    /// cannot consume a validated input.
    pub fn capture_with_contents<F>(paths: &[Utf8PathBuf], mut visit: F) -> Result<Self>
    where
        F: FnMut(&Utf8Path, &[u8], &std::fs::Permissions) -> Result<()>,
    {
        let mut unique = std::collections::BTreeSet::new();
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            if !path.is_absolute() || !unique.insert(path.as_str()) {
                return Err(CoreError::Filesystem(format!(
                    "isolated project input path is relative or duplicated: {path}"
                )));
            }
            files.push(ProjectInputFile::capture_with(path, &mut visit)?);
        }
        Ok(ProjectInputSnapshot { files })
    }

    /// Whether any input lost the captured identity, bytes, or permissions since the capture.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] when an input can no longer be inspected.
    pub fn changed(&self) -> Result<bool> {
        for expected in &self.files {
            if !expected.matches_live()? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Checks that every source input still has the captured identity, bytes, and permissions.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when any input changed independently.
    pub fn validate(&self) -> Result<()> {
        for expected in &self.files {
            if !expected.matches_live()? {
                return Err(CoreError::LockConflict(format!(
                    "{} changed independently after cooldown staged the isolated resolver trial; left project files untouched",
                    expected.path
                )));
            }
        }
        Ok(())
    }
}

impl ProjectInputFile {
    fn capture(path: &Utf8Path) -> Result<Self> {
        Self::capture_with(path, &mut |_, _, _| Ok(()))
    }

    fn capture_with<F>(path: &Utf8Path, visit: &mut F) -> Result<Self>
    where
        F: FnMut(&Utf8Path, &[u8], &std::fs::Permissions) -> Result<()>,
    {
        use std::io::Read as _;

        let path_metadata = std::fs::symlink_metadata(path)?;
        let symlink_target = if path_metadata.file_type().is_symlink() {
            Some(std::fs::read_link(path)?)
        } else if path_metadata.file_type().is_file() {
            None
        } else {
            return Err(CoreError::Filesystem(format!(
                "isolated project input does not resolve to a regular file: {path}"
            )));
        };
        let mut file = std::fs::File::open(path)?;
        let file_metadata = file.metadata()?;
        if !file_metadata.file_type().is_file() {
            return Err(CoreError::Filesystem(format!(
                "isolated project input does not resolve to a regular file: {path}"
            )));
        }
        let file_identity = project_input_identity(&file)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let final_path_metadata = std::fs::symlink_metadata(path)?;
        let final_symlink_target = if final_path_metadata.file_type().is_symlink() {
            Some(std::fs::read_link(path)?)
        } else if final_path_metadata.file_type().is_file() {
            None
        } else {
            return Err(CoreError::LockConflict(format!(
                "{path} changed kind while cooldown captured an isolated resolver input"
            )));
        };
        let final_file_metadata = std::fs::metadata(path)?;
        let final_file = std::fs::File::open(path)?;
        let final_file_identity = project_input_identity(&final_file)?;
        if file_identity != final_file_identity || symlink_target != final_symlink_target {
            return Err(CoreError::LockConflict(format!(
                "{path} changed identity while cooldown captured an isolated resolver input"
            )));
        }
        let permissions = final_file_metadata.permissions();
        crate::trace::artifact(
            crate::trace::ArtifactSource::File {
                path: path.as_str(),
                role: crate::trace::InputRole::ResolverInput,
            },
            &bytes,
        );
        visit(path, &bytes, &permissions)?;
        Ok(ProjectInputFile {
            path: path.to_owned(),
            file_identity,
            symlink_target,
            digest: Sha256::digest(bytes).into(),
            permissions,
        })
    }

    fn matches_live(&self) -> Result<bool> {
        let current = Self::capture(&self.path).map_err(|error| {
            CoreError::LockConflict(format!(
                "could not revalidate isolated resolver input {}: {error}",
                self.path
            ))
        })?;
        Ok(self.file_identity == current.file_identity
            && self.symlink_target == current.symlink_target
            && self.digest == current.digest
            && permissions_equal(Some(&self.permissions), Some(&current.permissions)))
    }
}

#[cfg(unix)]
fn project_input_identity(file: &std::fs::File) -> Result<ProjectInputIdentity> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata()?;
    Ok(ProjectInputIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn project_input_identity(file: &std::fs::File) -> Result<ProjectInputIdentity> {
    let handle = same_file::Handle::from_file(file.try_clone()?)?;
    Ok(ProjectInputIdentity::Windows(std::sync::Arc::new(handle)))
}

#[cfg(not(any(unix, windows)))]
fn project_input_identity(_file: &std::fs::File) -> Result<ProjectInputIdentity> {
    Err(CoreError::Filesystem(
        "cannot obtain stable resolver-input identity on this platform".to_string(),
    ))
}

/// A verified project-copy result paired with every source state that authorized it.
#[derive(Debug, Clone)]
pub struct AcceptedProjectState {
    original: ProjectMutationJournal,
    candidate: ProjectMutationState,
    inputs: ProjectInputSnapshot,
}

impl AcceptedProjectState {
    /// Pairs a source preimage with the corresponding accepted project-copy state.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the two states do not describe the same ordered
    /// write set.
    pub fn new(
        original: ProjectMutationJournal,
        candidate: ProjectMutationState,
        inputs: ProjectInputSnapshot,
    ) -> Result<Self> {
        if !matching_write_sets(&original.files, &candidate.files)
            || !valid_mutation_files(&original.files)
        {
            return Err(CoreError::LockConflict(
                "accepted project state has an invalid or mismatched source write set".to_string(),
            ));
        }
        Ok(AcceptedProjectState {
            original,
            candidate,
            inputs,
        })
    }

    /// Returns the files whose accepted bytes or permissions differ from the source preimage.
    pub fn changed_files(
        &self,
    ) -> impl Iterator<Item = (&ProjectMutationFile, &ProjectMutationFile)> {
        self.files()
            .filter(|(original, candidate)| !original.matches(candidate))
    }

    /// Returns every tracked preimage/candidate pair in publication order.
    pub fn files(&self) -> impl Iterator<Item = (&ProjectMutationFile, &ProjectMutationFile)> {
        self.original.files.iter().zip(&self.candidate.files)
    }

    /// Checks that the source still equals the preimage used to build the isolated trial.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when any source file changed independently.
    pub fn validate_source(&self, root: &Utf8Path) -> Result<()> {
        self.original.validate_project(root)?;
        self.inputs.validate()?;
        validate_state(
            self.original.project.path(),
            &self.original.files,
            "before publishing the accepted project state",
        )
    }

    /// Installs the accepted files after checking each source path immediately before replacement.
    ///
    /// Callers must publish durable recovery evidence before invoking this method.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the source no longer matches the preimage.
    /// Returns a filesystem error when a candidate cannot be installed.
    pub fn install(&self, root: &Utf8Path) -> Result<()> {
        self.validate_source(root)?;
        let root = self.original.project.path();
        for (original, candidate) in self.changed_files() {
            candidate.restore_if_unchanged(root, original)?;
        }
        self.validate_candidate(root)
    }

    /// Checks that every accepted file is now present in the source project.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when publication is incomplete or drifted.
    pub fn validate_candidate(&self, root: &Utf8Path) -> Result<()> {
        self.original.validate_project(root)?;
        validate_state(
            self.original.project.path(),
            &self.candidate.files,
            "after publishing the accepted project state",
        )
    }
}

/// One file entry recorded in a [`ProjectMutationJournal`].
#[derive(Debug, Clone)]
pub struct ProjectMutationFile {
    path: Utf8PathBuf,
    contents: Option<Vec<u8>>,
    permissions: Option<std::fs::Permissions>,
}

impl ProjectMutationJournal {
    /// Captures a project-bound journal for the requested relative paths.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] when the project identity or a file cannot be
    /// captured safely.
    /// Writable paths with a symbolic-link ancestor or final entry are rejected.
    /// Existing multiply-linked files are rejected because an in-place write could mutate an
    /// alias outside the project lease.
    pub fn capture<I, P>(root: &Utf8Path, paths: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Utf8Path>,
    {
        Self::capture_with_external(root, paths, std::iter::empty::<&Utf8Path>())
    }

    /// Captures a project-bound journal whose write set also covers shared files that may live
    /// outside the project root, such as a Go workspace's `go.work` in an ancestor directory.
    ///
    /// Each `external` path is absolute and is resolved by [`canonical_shared_file`].
    /// A file that lands inside the project root joins the write set under its project-relative
    /// path, unless `paths` already names it; any other file joins it under its canonical
    /// absolute path.
    /// Either way the journal binds the file's parent directory, so [`Self::validate_project`]
    /// and every restore refuse a parent that was replaced or whose spelling now resolves
    /// elsewhere.
    /// [`Self::shared_files`] reports every external file, wherever it landed.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::capture`], plus [`CoreError::Filesystem`] when an external
    /// path is relative or names no file, and a filesystem error when its parent directory cannot
    /// be resolved.
    /// External files follow the same rules as `paths`: a symbolic link, special file, or
    /// multiply-linked file is rejected, and so is a symbolic-link ancestor of the resolved path.
    pub fn capture_with_external<I, P, E, Q>(root: &Utf8Path, paths: I, external: E) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Utf8Path>,
        E: IntoIterator<Item = Q>,
        Q: AsRef<Utf8Path>,
    {
        let project = ProjectMutationRoot::capture(root)?;
        let mut files = paths
            .into_iter()
            .map(|path| {
                let path = path.as_ref();
                if !safe_mutation_path(path) {
                    return Err(CoreError::Filesystem(format!(
                        "refusing unsafe project-relative mutation path `{path}`"
                    )));
                }
                ProjectMutationFile::capture(project.path(), path)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut shared: Vec<SharedMutationFile> = Vec::new();
        for path in external {
            let file = SharedMutationFile::capture(path.as_ref())?;
            let entry = match file.path.strip_prefix(project.path()) {
                Ok(relative) => relative.to_owned(),
                Err(_) => file.path.clone(),
            };
            if files.iter().all(|existing| existing.path != entry) {
                files.push(ProjectMutationFile::capture(project.path(), &entry)?);
            }
            if shared.iter().all(|existing| existing.path != file.path) {
                shared.push(file);
            }
        }
        Self::from_bound_snapshot(project, files, shared)
    }

    /// Binds validated portable snapshots to one project restore authority.
    ///
    /// Recovery formats use this boundary after validating their project identity and file set.
    /// Normal adapters should prefer [`Self::capture`].
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] when the project identity, a path, or a snapshot is
    /// invalid.
    /// Writable paths with a symbolic-link ancestor or final entry are rejected, as are existing
    /// multiply-linked files.
    /// Portable snapshots are project-relative, so an absolute entry is rejected too.
    pub fn from_snapshot(root: &Utf8Path, files: Vec<ProjectMutationFile>) -> Result<Self> {
        Self::from_bound_snapshot(ProjectMutationRoot::capture(root)?, files, Vec::new())
    }

    fn from_bound_snapshot(
        project: ProjectMutationRoot,
        files: Vec<ProjectMutationFile>,
        shared: Vec<SharedMutationFile>,
    ) -> Result<Self> {
        // An absolute entry is legitimate only as a shared file this journal binds.
        let unbound = files.iter().any(|file| {
            file.path.is_absolute() && shared.iter().all(|shared| shared.path != file.path)
        });
        if unbound || !valid_mutation_files(&files) {
            return Err(CoreError::Filesystem(
                "mutation journal contains an unsafe, duplicate, or inconsistent project file"
                    .to_string(),
            ));
        }
        for file in &files {
            validate_mutation_target(project.path(), &file.path)?;
        }
        Ok(ProjectMutationJournal {
            project,
            files,
            shared,
        })
    }

    /// The ordered captured entries in this journal's write set.
    #[must_use]
    pub fn files(&self) -> &[ProjectMutationFile] {
        &self.files
    }

    /// The canonical paths of the shared files this journal covers, in capture order.
    ///
    /// These are the files captured through [`Self::capture_with_external`], including one that
    /// lies inside the project root and was journaled under its relative path.
    /// Each path is spelled as [`canonical_shared_file`] spells it, so a caller that leases these
    /// files can compare its own resolution with them directly.
    pub fn shared_files(&self) -> impl Iterator<Item = &Utf8Path> {
        self.shared.iter().map(|shared| shared.path.as_path())
    }

    /// The shared files as [`Self::capture_with_external`] was handed them, in the order of
    /// [`Self::shared_files`].
    ///
    /// A tool that resolves paths relative to the file, as `go` resolves a `go.work`'s `use`
    /// entries, must be handed this spelling rather than the canonical one: through a symbolic
    /// link in its directory, `../lib` names another directory than it does from the target.
    /// [`Self::validate_project`] checks that this spelling still resolves to the captured
    /// directory.
    pub fn shared_files_as_named(&self) -> impl Iterator<Item = &Utf8Path> {
        self.shared.iter().map(|shared| shared.named.as_path())
    }

    /// Checks that every shared file's parent directory is still the one captured.
    fn validate_shared(&self) -> Result<()> {
        self.shared
            .iter()
            .try_for_each(SharedMutationFile::validate)
    }

    /// Checks that this journal still belongs to `root` and has a safe writable topology.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when `root` names another project, its directory was
    /// replaced after journal capture, a shared file's parent directory was replaced or now
    /// resolves elsewhere, a writable ancestor is no longer a regular directory, or a final target
    /// became a symbolic link, special file, or multiply-linked regular file.
    pub fn validate_project(&self, root: &Utf8Path) -> Result<()> {
        let live = ProjectMutationRoot::capture(root).map_err(|error| {
            CoreError::LockConflict(format!(
                "could not revalidate mutation project {root}: {error}"
            ))
        })?;
        self.project.ensure_same(&live)?;
        self.validate_shared()?;
        for file in &self.files {
            validate_mutation_target(self.project.path(), &file.path).map_err(|error| {
                CoreError::LockConflict(format!(
                    "could not revalidate mutation write set for {}: {error}",
                    self.project.path()
                ))
            })?;
        }
        Ok(())
    }

    /// Adds paths absent from this journal while preserving its earlier snapshots.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the journals belong to different projects, or
    /// when they bind one shared file path to two different parent directories.
    pub fn extend_missing(&mut self, other: &ProjectMutationJournal) -> Result<()> {
        self.project.ensure_same(&other.project)?;
        for shared in &other.shared {
            match self
                .shared
                .iter()
                .find(|existing| existing.path == shared.path)
            {
                Some(existing) => existing.parent.ensure_same(&shared.parent)?,
                None => self.shared.push(shared.clone()),
            }
        }
        for file in &other.files {
            if self.files.iter().all(|existing| existing.path != file.path) {
                self.files.push(file.clone());
            }
        }
        Ok(())
    }

    /// Capture the current contents of one write-set entry.
    ///
    /// Missing files are recorded as `None`, which tells [`restore`](Self::restore) to remove them
    /// if a trial created them.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the path is not a singly linked regular file,
    /// changes identity during capture, or cannot be read.
    fn capture_file(root: &Utf8Path, rel: &Utf8Path) -> Result<ProjectMutationFile> {
        ProjectMutationFile::capture(root, rel)
    }

    /// Captures the current state of every file in this journal's write set.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] if the project identity changed or a file cannot
    /// be read safely.
    pub fn capture_state(&self) -> Result<ProjectMutationState> {
        self.project.validate_live()?;
        self.validate_shared()?;
        let files = self
            .files
            .iter()
            .map(|file| Self::capture_file(self.project.path(), &file.path))
            .collect::<Result<Vec<_>>>()?;
        Ok(ProjectMutationState {
            project: self.project.clone(),
            files,
        })
    }

    /// Projects this captured preimage onto another journal's ordered write set.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the journals belong to different projects or the
    /// requested write set is invalid or absent.
    pub fn state_for(&self, write_set: &ProjectMutationJournal) -> Result<ProjectMutationState> {
        self.project.ensure_same(&write_set.project)?;
        Ok(ProjectMutationState {
            project: self.project.clone(),
            files: projected_files(&self.files, &write_set.files)?,
        })
    }

    /// Restores this journal only when every live file still matches `expected`.
    ///
    /// Validation covers the complete write set before any file is changed.
    /// Each path is checked again immediately before its own atomic replacement, so drift during a
    /// multi-file restore stops before that path is overwritten.
    /// The multi-file operation is not globally atomic.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the expected state
    /// belongs to another project, does not describe this journal, or a live file changed
    /// independently.
    /// Returns a filesystem error if validation or restoration cannot read, replace, or remove a
    /// file.
    pub fn restore_if_unchanged(&self, expected: &ProjectMutationState) -> Result<()> {
        let project = self.project.path().as_str();
        crate::trace::Event::Mutation {
            project,
            transition: crate::trace::Mutation::RollbackStarted,
        }
        .emit();
        let result = self.restore_if_unchanged_inner(expected);
        match &result {
            Ok(()) => crate::trace::Event::Mutation {
                project,
                transition: crate::trace::Mutation::RolledBack,
            }
            .emit(),
            Err(err) => crate::trace::Event::Mutation {
                project,
                transition: crate::trace::Mutation::RollbackFailed {
                    reason: &err.to_string(),
                },
            }
            .emit(),
        }
        result
    }

    fn restore_if_unchanged_inner(&self, expected: &ProjectMutationState) -> Result<()> {
        self.project.ensure_same(&expected.project)?;
        self.project.validate_live()?;
        self.validate_shared()?;
        let root = self.project.path();
        if !matching_write_sets(&self.files, &expected.files) || !valid_mutation_files(&self.files)
        {
            return Err(CoreError::LockConflict(
                "rollback state does not match the mutation journal write set; left project files untouched"
                    .to_string(),
            ));
        }

        for expected_file in &expected.files {
            let live = Self::capture_file(root, &expected_file.path)?;
            if !live.matches(expected_file) {
                return Err(CoreError::LockConflict(format!(
                    "{} changed independently while cooldown was verifying a mutation; left project files untouched",
                    root.join(&expected_file.path)
                )));
            }
        }

        for (file, expected_file) in self.files.iter().zip(&expected.files) {
            let live = Self::capture_file(root, &expected_file.path)?;
            if !live.matches(expected_file) {
                return Err(CoreError::LockConflict(format!(
                    "{} changed independently during rollback; stopped before replacing it",
                    root.join(&expected_file.path)
                )));
            }
            file.restore_if_unchanged(root, expected_file)?;
        }
        Ok(())
    }

    /// Restores every captured file entry in this journal's project.
    ///
    /// Entries whose captured contents are `None` are removed if they now exist.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] if the project identity changed or a file cannot
    /// be restored safely.
    pub fn restore(&self) -> Result<()> {
        let project = self.project.path().as_str();
        crate::trace::Event::Mutation {
            project,
            transition: crate::trace::Mutation::RollbackStarted,
        }
        .emit();
        let result = self.restore_inner();
        match &result {
            Ok(()) => crate::trace::Event::Mutation {
                project,
                transition: crate::trace::Mutation::RolledBack,
            }
            .emit(),
            Err(err) => crate::trace::Event::Mutation {
                project,
                transition: crate::trace::Mutation::RollbackFailed {
                    reason: &err.to_string(),
                },
            }
            .emit(),
        }
        result
    }

    fn restore_inner(&self) -> Result<()> {
        self.project.validate_live()?;
        self.validate_shared()?;
        if !valid_mutation_files(&self.files) {
            return Err(CoreError::Filesystem(
                "mutation journal contains an unsafe or duplicate project path".to_string(),
            ));
        }
        for file in &self.files {
            file.restore(self.project.path())?;
        }
        Ok(())
    }
}

impl ProjectMutationFile {
    fn capture(root: &Utf8Path, rel: &Utf8Path) -> Result<Self> {
        use std::io::Read as _;

        validate_mutation_ancestors(root, rel)?;
        let path = root.join(rel);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProjectMutationFile {
                    path: rel.to_owned(),
                    contents: None,
                    permissions: None,
                });
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() {
            return Err(CoreError::Filesystem(format!(
                "refusing to journal non-regular project path {path}"
            )));
        }
        let mut file = std::fs::File::open(&path)?;
        let opened = file.metadata()?;
        require_single_link(&file, &opened, &path)?;
        let identity = same_file::Handle::from_file(file.try_clone()?)?;
        let current = std::fs::symlink_metadata(&path)?;
        let current_identity = same_file::Handle::from_path(&path)?;
        if !current.file_type().is_file() || identity != current_identity {
            return Err(CoreError::LockConflict(format!(
                "{path} changed identity while cooldown captured its mutation journal"
            )));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let final_opened = file.metadata()?;
        let final_metadata = std::fs::symlink_metadata(&path)?;
        let final_identity = same_file::Handle::from_path(&path)?;
        if !final_opened.file_type().is_file()
            || !final_metadata.file_type().is_file()
            || identity != final_identity
        {
            return Err(CoreError::LockConflict(format!(
                "{path} changed identity while cooldown read its mutation journal"
            )));
        }
        require_single_link(&file, &final_opened, &path)?;
        crate::trace::artifact(
            crate::trace::ArtifactSource::File {
                path: path.as_str(),
                role: crate::trace::InputRole::ResolverInput,
            },
            &bytes,
        );
        Ok(ProjectMutationFile {
            path: rel.to_owned(),
            contents: Some(bytes),
            permissions: Some(final_opened.permissions()),
        })
    }
}

impl ProjectMutationRoot {
    fn capture(root: &Utf8Path) -> Result<Self> {
        let canonical = std::fs::canonicalize(root)?;
        let path = Utf8PathBuf::from_path_buf(canonical).map_err(|path| {
            CoreError::PathEncoding(format!(
                "project mutation root is not valid UTF-8: {}",
                path.display()
            ))
        })?;
        let identity = same_file::Handle::from_path(&path)?;
        Ok(ProjectMutationRoot {
            path,
            identity: std::sync::Arc::new(identity),
        })
    }

    fn path(&self) -> &Utf8Path {
        &self.path
    }

    fn ensure_same(&self, other: &ProjectMutationRoot) -> Result<()> {
        if self.identity == other.identity {
            return Ok(());
        }
        Err(CoreError::LockConflict(format!(
            "mutation authority for {} cannot be used with project {}",
            self.path, other.path
        )))
    }

    fn validate_live(&self) -> Result<()> {
        let live = same_file::Handle::from_path(&self.path).map_err(|error| {
            CoreError::LockConflict(format!(
                "could not revalidate mutation project {}: {error}",
                self.path
            ))
        })?;
        if *self.identity == live {
            Ok(())
        } else {
            Err(CoreError::LockConflict(format!(
                "mutation project {} changed identity; left project files untouched",
                self.path
            )))
        }
    }
}

impl SharedMutationFile {
    fn capture(path: &Utf8Path) -> Result<Self> {
        let (named_parent, _) = split_shared_file(path)?;
        let parent = ProjectMutationRoot::capture(named_parent)?;
        let canonical = canonical_shared_file(path)?;
        // Resolving the parent twice leaves a window in which it could be retargeted.
        if canonical.parent() != Some(parent.path()) {
            return Err(CoreError::LockConflict(format!(
                "{named_parent} changed while cooldown resolved a shared mutation file"
            )));
        }
        Ok(SharedMutationFile {
            named: path.to_owned(),
            parent,
            path: canonical,
        })
    }

    fn validate(&self) -> Result<()> {
        self.parent.validate_live()?;
        let (named_parent, _) = split_shared_file(&self.named)?;
        let live = ProjectMutationRoot::capture(named_parent).map_err(|error| {
            CoreError::LockConflict(format!(
                "could not revalidate the directory of shared mutation file {}: {error}",
                self.path
            ))
        })?;
        self.parent.ensure_same(&live)
    }
}

/// Resolves the absolute path of a shared mutation file to the one spelling every cooldown
/// process derives for it.
///
/// The parent directory is canonicalized.
/// When the file exists as a regular file, its name is taken from the filesystem too, so on a
/// case-insensitive volume `GO.WORK` and `go.work` resolve to the same path.
/// Any other final entry keeps the given name: a symbolic link is not followed, so a journal
/// still refuses it rather than writing through it.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`] when `path` is relative or names no file, a filesystem error
/// when its parent directory cannot be resolved, and [`CoreError::PathEncoding`] when the
/// resolved path is not valid UTF-8.
pub fn canonical_shared_file(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let (parent, name) = split_shared_file(path)?;
    let joined = canonical_utf8(parent)?.join(name);
    match std::fs::symlink_metadata(&joined) {
        Ok(metadata) if metadata.file_type().is_file() => canonical_utf8(&joined),
        Ok(_) => Ok(joined),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(joined),
        Err(error) => Err(error.into()),
    }
}

fn split_shared_file(path: &Utf8Path) -> Result<(&Utf8Path, &str)> {
    match (path.is_absolute(), path.parent(), path.file_name()) {
        (true, Some(parent), Some(name)) => Ok((parent, name)),
        _ => Err(CoreError::Filesystem(format!(
            "a shared mutation file needs an absolute file path, got `{path}`"
        ))),
    }
}

fn canonical_utf8(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    Utf8PathBuf::from_path_buf(canonical).map_err(|path| {
        CoreError::PathEncoding(format!(
            "shared mutation path is not valid UTF-8: {}",
            path.display()
        ))
    })
}

fn projected_files(
    source: &[ProjectMutationFile],
    write_set: &[ProjectMutationFile],
) -> Result<Vec<ProjectMutationFile>> {
    if !valid_mutation_files(source) || !valid_mutation_files(write_set) {
        return Err(CoreError::LockConflict(
            "cannot project an invalid mutation write set".to_string(),
        ));
    }
    write_set
        .iter()
        .map(|requested| {
            source
                .iter()
                .find(|candidate| candidate.path == requested.path)
                .cloned()
                .ok_or_else(|| {
                    CoreError::LockConflict(format!(
                        "mutation state does not contain requested path {}",
                        requested.path
                    ))
                })
        })
        .collect()
}

impl ProjectMutationFile {
    /// Reconstructs one captured file entry from a validated portable snapshot.
    ///
    /// `contents` and `permissions` must either both be present or both be absent.
    /// ACLs and extended attributes remain outside the portable rollback contract.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] when the path is unsafe or the snapshot fields are
    /// inconsistent.
    pub fn from_snapshot(
        path: Utf8PathBuf,
        contents: Option<Vec<u8>>,
        permissions: Option<std::fs::Permissions>,
    ) -> Result<Self> {
        if !safe_mutation_path(&path) || contents.is_some() != permissions.is_some() {
            return Err(CoreError::Filesystem(format!(
                "refusing invalid project mutation snapshot for `{path}`"
            )));
        }
        Ok(ProjectMutationFile {
            path,
            contents,
            permissions,
        })
    }

    /// The project-relative path captured by this entry.
    #[must_use]
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }

    /// The captured contents, or `None` when the file was absent.
    #[must_use]
    pub fn contents(&self) -> Option<&[u8]> {
        self.contents.as_deref()
    }

    /// The captured standard permissions, or `None` when the file was absent.
    #[must_use]
    pub fn permissions(&self) -> Option<&std::fs::Permissions> {
        self.permissions.as_ref()
    }

    fn matches(&self, other: &ProjectMutationFile) -> bool {
        self.path == other.path
            && self.contents == other.contents
            && permissions_equal(self.permissions.as_ref(), other.permissions.as_ref())
    }

    fn restore(&self, root: &Utf8Path) -> Result<()> {
        validate_mutation_ancestors(root, &self.path)?;
        let path = root.join(&self.path);
        match &self.contents {
            Some(bytes) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::fs::atomic_write_with_permissions(
                    path.as_std_path(),
                    bytes,
                    self.permissions.as_ref(),
                )?;
            }
            None => match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            },
        }
        Ok(())
    }

    fn restore_if_unchanged(&self, root: &Utf8Path, expected: &ProjectMutationFile) -> Result<()> {
        validate_mutation_ancestors(root, &self.path)?;
        let path = root.join(&self.path);
        if let Some(bytes) = &self.contents {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::fs::atomic_write_durable_with_permissions_checked(
                path.as_std_path(),
                bytes,
                self.permissions.as_ref(),
                || {
                    let live = ProjectMutationJournal::capture_file(root, &expected.path)?;
                    if live.matches(expected) {
                        Ok(())
                    } else {
                        Err(rollback_drift(root, &expected.path))
                    }
                },
            )
            .map_err(|error| error.into_core_error(path.as_std_path()))?;
        } else {
            let live = ProjectMutationJournal::capture_file(root, &expected.path)?;
            if !live.matches(expected) {
                return Err(rollback_drift(root, &expected.path));
            }
            crate::fs::remove_file_durable(path.as_std_path())
                .map_err(|error| error.into_core_error(path.as_std_path()))?;
        }
        Ok(())
    }
}

fn rollback_drift(root: &Utf8Path, path: &Utf8Path) -> CoreError {
    CoreError::LockConflict(format!(
        "{} changed independently during rollback; stopped before replacing it",
        root.join(path)
    ))
}

fn matching_write_sets(left: &[ProjectMutationFile], right: &[ProjectMutationFile]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.path == right.path)
}

fn valid_mutation_files(files: &[ProjectMutationFile]) -> bool {
    let mut paths = std::collections::BTreeSet::new();
    files.iter().all(|file| {
        safe_journal_path(&file.path)
            && file.contents.is_some() == file.permissions.is_some()
            && paths.insert(file.path.as_str())
    })
}

/// Whether `path` is a project-relative path made only of normal components.
fn safe_mutation_path(path: &Utf8Path) -> bool {
    !path.as_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, camino::Utf8Component::Normal(_)))
}

/// Whether `path` is a valid journal entry: project-relative, or the canonical absolute path of a
/// shared file, which has no `.` or `..` component either.
fn safe_journal_path(path: &Utf8Path) -> bool {
    if !path.is_absolute() {
        return safe_mutation_path(path);
    }
    let mut components = path.components().peekable();
    if let Some(camino::Utf8Component::Prefix(_)) = components.peek() {
        components.next();
    }
    components.next() == Some(camino::Utf8Component::RootDir)
        && components.peek().is_some()
        && components.all(|component| matches!(component, camino::Utf8Component::Normal(_)))
}

fn validate_mutation_ancestors(root: &Utf8Path, path: &Utf8Path) -> Result<()> {
    if !safe_journal_path(path) {
        return Err(CoreError::Filesystem(format!(
            "refusing unsafe project-relative mutation path `{path}`"
        )));
    }
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    // A shared file's absolute path is checked from the filesystem root, so every directory
    // above it must be a real directory, as every directory between a project root and its own
    // files must be.
    let mut current = if path.is_absolute() {
        Utf8PathBuf::new()
    } else {
        root.to_owned()
    };
    for component in parent.components() {
        let component = match component {
            camino::Utf8Component::Normal(component) => component,
            camino::Utf8Component::Prefix(_) | camino::Utf8Component::RootDir
                if path.is_absolute() =>
            {
                current.push(component);
                continue;
            }
            _ => {
                return Err(CoreError::Filesystem(format!(
                    "refusing unsafe project-relative mutation path `{path}`"
                )));
            }
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(CoreError::Filesystem(format!(
                    "cannot safely mutate `{path}` because its ancestor {current} is a symbolic link; symlinked writable project paths are unsupported"
                )));
            }
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(CoreError::Filesystem(format!(
                    "cannot safely mutate `{path}` because its ancestor {current} is not a directory"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_mutation_target(root: &Utf8Path, path: &Utf8Path) -> Result<()> {
    validate_mutation_ancestors(root, path)?;
    let target = root.join(path);
    let metadata = match std::fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() {
        return Err(CoreError::Filesystem(format!(
            "refusing non-regular project mutation target {target}"
        )));
    }
    let file = std::fs::File::open(&target)?;
    let opened = file.metadata()?;
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    let current = std::fs::symlink_metadata(&target)?;
    let current_identity = same_file::Handle::from_path(&target)?;
    if !opened.file_type().is_file()
        || !current.file_type().is_file()
        || identity != current_identity
    {
        return Err(CoreError::LockConflict(format!(
            "{target} changed identity while cooldown revalidated its mutation target"
        )));
    }
    require_single_link(&file, &opened, &target)
}

#[cfg(unix)]
fn require_single_link(
    _file: &std::fs::File,
    metadata: &std::fs::Metadata,
    path: &Utf8Path,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    if metadata.nlink() == 1 {
        return Ok(());
    }
    Err(CoreError::Filesystem(format!(
        "refusing multiply linked project mutation target {path}"
    )))
}

#[cfg(windows)]
fn require_single_link(
    file: &std::fs::File,
    _metadata: &std::fs::Metadata,
    path: &Utf8Path,
) -> Result<()> {
    if winapi_util::file::information(file)?.number_of_links() == 1 {
        return Ok(());
    }
    Err(CoreError::Filesystem(format!(
        "refusing multiply linked project mutation target {path}"
    )))
}

#[cfg(not(any(unix, windows)))]
fn require_single_link(
    _file: &std::fs::File,
    _metadata: &std::fs::Metadata,
    path: &Utf8Path,
) -> Result<()> {
    Err(CoreError::Filesystem(format!(
        "cannot verify project mutation target link count for {path} on this platform"
    )))
}

fn validate_state(root: &Utf8Path, expected: &[ProjectMutationFile], context: &str) -> Result<()> {
    for expected in expected {
        let live = ProjectMutationJournal::capture_file(root, &expected.path)?;
        if !live.matches(expected) {
            return Err(CoreError::LockConflict(format!(
                "{} changed independently {context}; left project files untouched",
                root.join(&expected.path)
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn permissions_equal(
    left: Option<&std::fs::Permissions>,
    right: Option<&std::fs::Permissions>,
) -> bool {
    use std::os::unix::fs::PermissionsExt;
    left.map(PermissionsExt::mode) == right.map(PermissionsExt::mode)
}

#[cfg(not(unix))]
fn permissions_equal(
    left: Option<&std::fs::Permissions>,
    right: Option<&std::fs::Permissions>,
) -> bool {
    left.map(std::fs::Permissions::readonly) == right.map(std::fs::Permissions::readonly)
}

#[cfg(test)]
mod mutation_journal_tests {
    use super::*;
    use color_eyre::eyre;

    fn root(directory: &tempfile::TempDir) -> eyre::Result<Utf8PathBuf> {
        Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|_| eyre::eyre!("temporary path is not UTF-8"))
    }

    /// A baseline reads present files once each, records an absent one as `None`, follows a
    /// symbolic link the journal would refuse, and rejects a path that leaves the project.
    #[test]
    fn a_baseline_reads_project_files_without_write_restrictions() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = root(&directory)?;
        std::fs::write(root.join("package.json"), "root")?;

        let baseline = MutationBaseline::read(
            &root,
            ["package.json", "missing/package.json", "package.json"],
        )?;

        let read: Vec<(String, Option<&[u8]>)> = baseline
            .files()
            .iter()
            .map(|file| (file.path().to_string(), file.contents()))
            .collect();
        assert_eq!(
            read,
            [
                ("package.json".to_string(), Some(b"root".as_slice())),
                ("missing/package.json".to_string(), None),
            ]
        );
        for escaping in ["../package.json", "/package.json"] {
            std::assert_matches!(
                MutationBaseline::read(&root, [escaping]),
                Err(CoreError::Filesystem(_))
            );
        }

        #[cfg(unix)]
        {
            std::fs::create_dir_all(root.join("member"))?;
            std::os::unix::fs::symlink(
                root.join("package.json"),
                root.join("member/package.json"),
            )?;
            let linked = MutationBaseline::read(&root, ["member/package.json"])?;
            assert_eq!(
                linked.files().first().and_then(BaselineFile::contents),
                Some(b"root".as_slice())
            );
            assert!(ProjectMutationJournal::capture(&root, ["member/package.json"]).is_err());
        }
        Ok(())
    }

    /// A recorded fact travels with the baseline, a later record of one name replacing the earlier
    /// one, and a name never recorded reads as absent.
    #[test]
    fn a_baseline_carries_the_facts_recorded_at_capture() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = root(&directory)?;
        let mut baseline = MutationBaseline::read(&root, ["package.json"])?;

        baseline.record("adapter/members", "[]");
        baseline.record("adapter/members", r#"["a"]"#);
        let shared = baseline.clone();

        assert_eq!(shared.recorded("adapter/members"), Some(r#"["a"]"#));
        assert_eq!(shared.recorded("adapter/other"), None);
        Ok(())
    }

    #[test]
    fn checked_restore_refuses_independent_drift() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = root(&directory)?;
        let relative = Utf8Path::new("Cargo.toml");
        std::fs::write(root.join(relative), "original")?;
        let journal = ProjectMutationJournal::capture(&root, [relative])?;
        std::fs::write(root.join(relative), "candidate")?;
        let expected = journal.capture_state()?;
        std::fs::write(root.join(relative), "external")?;

        let Err(error) = journal.restore_if_unchanged(&expected) else {
            color_eyre::eyre::bail!("drift unexpectedly allowed rollback");
        };

        std::assert_matches!(error, CoreError::LockConflict(_));
        assert_eq!(std::fs::read_to_string(root.join(relative))?, "external");
        Ok(())
    }

    #[test]
    fn mutation_authority_rejects_another_projects_state() -> eyre::Result<()> {
        let project_a = tempfile::tempdir()?;
        let project_b = tempfile::tempdir()?;
        let root_a = root(&project_a)?;
        let root_b = root(&project_b)?;
        let relative = Utf8Path::new("Cargo.lock");
        std::fs::write(root_a.join(relative), "project-a")?;
        std::fs::write(root_b.join(relative), "project-b")?;
        let journal = ProjectMutationJournal::capture(&root_a, [relative])?;
        let state_b = ProjectMutationState::capture(&root_b, &journal)?;

        std::assert_matches!(
            journal.validate_project(&root_b),
            Err(CoreError::LockConflict(_))
        );
        let result = journal.restore_if_unchanged(&state_b);

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        assert_eq!(std::fs::read_to_string(root_a.join(relative))?, "project-a");
        assert_eq!(std::fs::read_to_string(root_b.join(relative))?, "project-b");
        Ok(())
    }

    #[test]
    fn mutation_authority_uses_filesystem_identity_not_path_spelling() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = root(&directory)?;
        let captured = ProjectMutationRoot::capture(&root)?;
        let equivalent = ProjectMutationRoot {
            path: root.join("."),
            identity: captured.identity.clone(),
        };

        captured.ensure_same(&equivalent)?;
        Ok(())
    }

    #[test]
    fn mutation_authority_reports_a_removed_root_as_a_conflict() -> eyre::Result<()> {
        let parent = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(parent.path().join("project"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::create_dir(&root)?;
        let journal = ProjectMutationJournal::capture(&root, std::iter::empty::<&Utf8Path>())?;
        std::fs::remove_dir(&root)?;

        let result = journal.validate_project(&root);

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        Ok(())
    }

    #[test]
    fn mutation_authority_reports_a_recreated_root_as_a_conflict() -> eyre::Result<()> {
        let parent = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(parent.path().join("project"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let moved = Utf8PathBuf::from_path_buf(parent.path().join("moved"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::create_dir(&root)?;
        let journal = ProjectMutationJournal::capture(&root, std::iter::empty::<&Utf8Path>())?;
        std::fs::rename(&root, &moved)?;
        std::fs::create_dir(&root)?;

        let result = journal.validate_project(&root);

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        Ok(())
    }

    #[test]
    fn mutation_journal_rejects_paths_outside_the_project() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = root(&directory)?;
        let result = ProjectMutationFile::from_snapshot(
            Utf8PathBuf::from("../outside"),
            Some(b"replacement".to_vec()),
            Some(std::fs::metadata(&root)?.permissions()),
        );

        std::assert_matches!(result, Err(CoreError::Filesystem(_)));
        Ok(())
    }

    #[test]
    fn mutation_file_rejects_inconsistent_snapshot_fields() {
        let result = ProjectMutationFile::from_snapshot(
            Utf8PathBuf::from("Cargo.lock"),
            Some(b"lock".to_vec()),
            None,
        );

        std::assert_matches!(result, Err(CoreError::Filesystem(_)));
    }

    #[test]
    fn mutation_journal_rejects_duplicate_paths() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = root(&directory)?;
        let relative = Utf8Path::new("Cargo.lock");
        std::fs::write(root.join(relative), "lock")?;
        let file = ProjectMutationFile::capture(&root, relative)?;

        let result = ProjectMutationJournal::from_snapshot(&root, vec![file.clone(), file]);

        std::assert_matches!(result, Err(CoreError::Filesystem(_)));
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_identity_distinguishes_same_length_replacement() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("Cargo.lock");
        let replacement = directory.path().join("replacement.lock");
        std::fs::write(&path, "first")?;
        std::fs::write(&replacement, "other")?;
        let original_identity = same_file::Handle::from_path(&path)?;

        std::fs::remove_file(&path)?;
        std::fs::rename(&replacement, &path)?;

        assert_ne!(original_identity, same_file::Handle::from_path(path)?);
        Ok(())
    }

    #[test]
    fn checked_restore_replaces_and_removes_only_expected_files() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = root(&directory)?;
        let existing = Utf8Path::new("Cargo.toml");
        let created = Utf8Path::new("Cargo.lock");
        std::fs::write(root.join(existing), "original")?;
        let journal = ProjectMutationJournal::capture(&root, [existing, created])?;
        std::fs::write(root.join(existing), "candidate")?;
        std::fs::write(root.join(created), "candidate")?;
        let expected = journal.capture_state()?;

        journal.restore_if_unchanged(&expected)?;

        assert_eq!(std::fs::read_to_string(root.join(existing))?, "original");
        assert!(!root.join(created).exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn checked_restore_restores_unix_permissions() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir()?;
        let root = root(&directory)?;
        let relative = Utf8Path::new("Cargo.lock");
        let path = root.join(relative);
        std::fs::write(&path, "original")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;
        let journal = ProjectMutationJournal::capture(&root, [relative])?;

        std::fs::write(&path, "candidate")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let expected = journal.capture_state()?;
        journal.restore_if_unchanged(&expected)?;

        let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn mutation_journal_rejects_regular_and_dangling_symlinks() -> eyre::Result<()> {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()?;
        let root = root(&directory)?;
        std::fs::write(root.join("real.toml"), "contents")?;
        symlink("real.toml", root.join("Cargo.toml"))?;
        symlink("missing.lock", root.join("Cargo.lock"))?;

        for relative in [Utf8Path::new("Cargo.toml"), Utf8Path::new("Cargo.lock")] {
            let result = ProjectMutationFile::capture(&root, relative);
            std::assert_matches!(result, Err(CoreError::Filesystem(_)));
        }
        assert!(root.join("Cargo.toml").is_symlink());
        assert!(root.join("Cargo.lock").is_symlink());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn mutation_journal_rejects_a_symlinked_writable_ancestor() -> eyre::Result<()> {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir()?;
        let external = tempfile::tempdir()?;
        let project_root = root(&project)?;
        let external_root = root(&external)?;
        std::fs::write(external_root.join("Cargo.toml"), "external")?;
        symlink(&external_root, project_root.join("linked"))?;

        let result =
            ProjectMutationJournal::capture(&project_root, [Utf8Path::new("linked/Cargo.toml")]);

        let Err(error) = result else {
            eyre::bail!("symlinked writable ancestor unexpectedly entered a journal");
        };
        assert!(
            error
                .to_string()
                .contains("symlinked writable project paths are unsupported")
        );
        assert_eq!(
            std::fs::read_to_string(external_root.join("Cargo.toml"))?,
            "external"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn mutation_authority_rejects_a_retargeted_file_symlink() -> eyre::Result<()> {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir()?;
        let external = tempfile::tempdir()?;
        let project_root = root(&project)?;
        let external_root = root(&external)?;
        let relative = Utf8Path::new("requirements.txt");
        let target = external_root.join(relative);
        std::fs::write(project_root.join(relative), "original")?;
        std::fs::write(&target, "external")?;
        let journal = ProjectMutationJournal::capture(&project_root, [relative])?;
        std::fs::remove_file(project_root.join(relative))?;
        symlink(&target, project_root.join(relative))?;

        let result = journal.validate_project(&project_root);

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        assert_eq!(std::fs::read_to_string(target)?, "external");
        Ok(())
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn mutation_journal_rejects_a_multiply_linked_file() -> eyre::Result<()> {
        let project = tempfile::tempdir()?;
        let external = tempfile::tempdir()?;
        let project_root = root(&project)?;
        let external_root = root(&external)?;
        let relative = Utf8Path::new("requirements.txt");
        let external_alias = external_root.join("requirements.txt");
        std::fs::write(project_root.join(relative), "original")?;
        std::fs::hard_link(project_root.join(relative), &external_alias)?;

        let result = ProjectMutationJournal::capture(&project_root, [relative]);

        std::assert_matches!(result, Err(CoreError::Filesystem(_)));
        assert_eq!(std::fs::read_to_string(external_alias)?, "original");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn accepted_publication_rejects_a_retargeted_output_ancestor() -> eyre::Result<()> {
        use std::os::unix::fs::symlink;

        let source = tempfile::tempdir()?;
        let candidate = tempfile::tempdir()?;
        let external = tempfile::tempdir()?;
        let source_root = root(&source)?;
        let candidate_root = root(&candidate)?;
        let external_root = root(&external)?;
        let relative = Utf8Path::new("member/Cargo.toml");
        std::fs::create_dir_all(source_root.join("member"))?;
        std::fs::create_dir_all(candidate_root.join("member"))?;
        std::fs::write(source_root.join(relative), "original")?;
        std::fs::write(candidate_root.join(relative), "accepted")?;
        std::fs::write(external_root.join("Cargo.toml"), "external")?;
        let journal = ProjectMutationJournal::capture(&source_root, [relative])?;
        let candidate = ProjectMutationState::capture(&candidate_root, &journal)?;
        let accepted =
            AcceptedProjectState::new(journal, candidate, ProjectInputSnapshot::default())?;

        std::fs::rename(
            source_root.join("member"),
            source_root.join("original-member"),
        )?;
        symlink(&external_root, source_root.join("member"))?;

        let result = accepted.install(&source_root);

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        assert_eq!(
            std::fs::read_to_string(external_root.join("Cargo.toml"))?,
            "external"
        );
        Ok(())
    }

    /// A temporary workspace directory holding a `go.work` and the project directory below it,
    /// the layout of a Go module that is one member of a workspace.
    struct SharedLayout {
        _directory: tempfile::TempDir,
        workspace: Utf8PathBuf,
        project: Utf8PathBuf,
    }

    fn shared_layout() -> eyre::Result<SharedLayout> {
        let directory = tempfile::tempdir()?;
        let workspace = canonical_utf8(&root(&directory)?)?;
        let project = workspace.join("app");
        std::fs::create_dir_all(&project)?;
        std::fs::write(project.join("go.mod"), "module app\n")?;
        std::fs::write(workspace.join("go.work"), "go 1.24.0\n")?;
        Ok(SharedLayout {
            _directory: directory,
            workspace,
            project,
        })
    }

    /// A shared file outside the project joins the write set under its canonical absolute path,
    /// so a checked rollback restores the edited one and removes the one the trial created.
    #[test]
    fn checked_restore_reverts_shared_files_outside_the_project() -> eyre::Result<()> {
        let layout = shared_layout()?;
        let work = layout.workspace.join("go.work");
        let created = layout.workspace.join("other.work");
        let journal = ProjectMutationJournal::capture_with_external(
            &layout.project,
            ["go.mod"],
            [&work, &created],
        )?;

        // Both shared files are journaled and reported by their canonical paths.
        let entries: Vec<&str> = journal
            .files()
            .iter()
            .map(|file| file.path().as_str())
            .collect();
        assert_eq!(entries, ["go.mod", work.as_str(), created.as_str()]);
        let shared: Vec<&Utf8Path> = journal.shared_files().collect();
        assert_eq!(shared, [work.as_path(), created.as_path()]);

        // A trial raises the workspace and creates a second shared file.
        std::fs::write(&work, "go 1.26.0\n")?;
        std::fs::write(&created, "candidate")?;
        let expected = journal.capture_state()?;
        journal.restore_if_unchanged(&expected)?;

        assert_eq!(std::fs::read_to_string(&work)?, "go 1.24.0\n");
        assert!(!created.exists());
        Ok(())
    }

    /// A shared file inside the project joins the write set once, under its relative path, yet
    /// still counts as shared.
    #[test]
    fn a_shared_file_inside_the_project_is_journaled_relative() -> eyre::Result<()> {
        let layout = shared_layout()?;
        std::fs::write(layout.project.join("go.work"), "go 1.24.0\n")?;
        let work = layout.project.join("go.work");

        let journal = ProjectMutationJournal::capture_with_external(
            &layout.project,
            ["go.mod", "go.work"],
            [&work],
        )?;

        let entries: Vec<&str> = journal
            .files()
            .iter()
            .map(|file| file.path().as_str())
            .collect();
        assert_eq!(entries, ["go.mod", "go.work"]);
        let shared: Vec<&Utf8Path> = journal.shared_files().collect();
        assert_eq!(shared, [work.as_path()]);
        Ok(())
    }

    /// A shared file's directory replaced after capture voids the journal's authority over it:
    /// revalidation and rollback both refuse, and the replacement is left alone.
    #[test]
    fn a_replaced_shared_directory_is_a_conflict() -> eyre::Result<()> {
        let layout = shared_layout()?;
        let directory = layout.workspace.join("shared");
        std::fs::create_dir_all(&directory)?;
        let work = directory.join("go.work");
        std::fs::write(&work, "original")?;
        let journal =
            ProjectMutationJournal::capture_with_external(&layout.project, ["go.mod"], [&work])?;
        let expected = journal.capture_state()?;

        std::fs::rename(&directory, layout.workspace.join("shared.old"))?;
        std::fs::create_dir_all(&directory)?;
        std::fs::write(&work, "replacement")?;

        std::assert_matches!(
            journal.validate_project(&layout.project),
            Err(CoreError::LockConflict(_))
        );
        std::assert_matches!(
            journal.restore_if_unchanged(&expected),
            Err(CoreError::LockConflict(_))
        );
        assert_eq!(std::fs::read_to_string(&work)?, "replacement");
        Ok(())
    }

    /// The directory a shared file was named through is resolved again on every validation, so
    /// retargeting a symbolic link in that spelling is refused.
    #[cfg(unix)]
    #[test]
    fn a_retargeted_shared_directory_spelling_is_a_conflict() -> eyre::Result<()> {
        let layout = shared_layout()?;
        let other = layout.workspace.join("other");
        std::fs::create_dir_all(&other)?;
        std::fs::write(other.join("go.work"), "other")?;
        let alias = layout.workspace.join("alias");
        std::os::unix::fs::symlink(&layout.workspace, &alias)?;
        let journal = ProjectMutationJournal::capture_with_external(
            &layout.project,
            ["go.mod"],
            [alias.join("go.work")],
        )?;
        journal.validate_project(&layout.project)?;

        std::fs::remove_file(&alias)?;
        std::os::unix::fs::symlink(&other, &alias)?;

        std::assert_matches!(
            journal.validate_project(&layout.project),
            Err(CoreError::LockConflict(_))
        );
        Ok(())
    }

    /// A shared file that is a symbolic link is refused like any other journaled link, even when
    /// it points into the project.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_shared_file_is_refused() -> eyre::Result<()> {
        let layout = shared_layout()?;
        std::fs::write(layout.project.join("go.work"), "go 1.24.0\n")?;
        std::fs::remove_file(layout.workspace.join("go.work"))?;
        std::os::unix::fs::symlink(
            layout.project.join("go.work"),
            layout.workspace.join("go.work"),
        )?;

        let result = ProjectMutationJournal::capture_with_external(
            &layout.project,
            ["go.mod"],
            [layout.workspace.join("go.work")],
        );

        std::assert_matches!(result, Err(CoreError::Filesystem(_)));
        Ok(())
    }

    /// Shared files never leave the journal that bound them: a portable snapshot, a capture under
    /// another root, and a plain capture all refuse an absolute path.
    #[test]
    fn absolute_entries_stay_inside_the_binding_journal() -> eyre::Result<()> {
        let layout = shared_layout()?;
        let work = layout.workspace.join("go.work");
        let journal =
            ProjectMutationJournal::capture_with_external(&layout.project, ["go.mod"], [&work])?;

        std::assert_matches!(
            ProjectMutationJournal::from_snapshot(&layout.project, journal.files().to_vec()),
            Err(CoreError::Filesystem(_))
        );
        let copy = tempfile::tempdir()?;
        std::assert_matches!(
            ProjectMutationState::capture(&root(&copy)?, &journal),
            Err(CoreError::LockConflict(_))
        );
        std::assert_matches!(
            ProjectMutationJournal::capture(&layout.project, [&work]),
            Err(CoreError::Filesystem(_))
        );
        std::assert_matches!(
            ProjectMutationJournal::capture_with_external(&layout.project, ["go.mod"], ["go.work"]),
            Err(CoreError::Filesystem(_))
        );
        Ok(())
    }

    /// On a case-insensitive volume two spellings of one file resolve to one shared path, which
    /// is what keys its lease.
    #[cfg(windows)]
    #[test]
    fn shared_file_spellings_resolve_to_the_on_disk_name() -> eyre::Result<()> {
        let layout = shared_layout()?;
        let lower = canonical_shared_file(&layout.workspace.join("go.work"))?;
        let upper = canonical_shared_file(&layout.workspace.join("GO.WORK"))?;
        assert_eq!(lower, upper);
        assert_eq!(lower.file_name(), Some("go.work"));
        Ok(())
    }
}
