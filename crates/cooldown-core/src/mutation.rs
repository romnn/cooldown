//! Filesystem state and lifecycle values for recoverable project mutations.

use crate::error::{CoreError, Result};
use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest as _, Sha256};

/// The project-bound pre-change state a planned mutation may rewrite.
#[derive(Debug, Clone)]
pub struct ProjectMutationJournal {
    project: ProjectMutationRoot,
    files: Vec<ProjectMutationFile>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Writable paths with a symbolic-link ancestor are rejected because another project lease may
    /// govern their resolved target.
    pub fn capture(root: &Utf8Path, write_set: &ProjectMutationJournal) -> Result<Self> {
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
    /// Returns a [`CoreError`](crate::CoreError) when the project identity or a file cannot be
    /// captured safely.
    /// Writable paths with a symbolic-link ancestor are rejected.
    pub fn capture<I, P>(root: &Utf8Path, paths: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Utf8Path>,
    {
        let project = ProjectMutationRoot::capture(root)?;
        let files = paths
            .into_iter()
            .map(|path| ProjectMutationFile::capture(project.path(), path.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        Self::from_bound_snapshot(project, files)
    }

    /// Binds validated portable snapshots to one project restore authority.
    ///
    /// Recovery formats use this boundary after validating their project identity and file set.
    /// Normal adapters should prefer [`Self::capture`].
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) when the project identity, a path, or a snapshot is
    /// invalid.
    /// Writable paths with a symbolic-link ancestor are rejected.
    pub fn from_snapshot(root: &Utf8Path, files: Vec<ProjectMutationFile>) -> Result<Self> {
        Self::from_bound_snapshot(ProjectMutationRoot::capture(root)?, files)
    }

    fn from_bound_snapshot(
        project: ProjectMutationRoot,
        files: Vec<ProjectMutationFile>,
    ) -> Result<Self> {
        if !valid_mutation_files(&files) {
            return Err(CoreError::Filesystem(
                "mutation journal contains an unsafe, duplicate, or inconsistent project file"
                    .to_string(),
            ));
        }
        for file in &files {
            validate_mutation_ancestors(project.path(), &file.path)?;
        }
        Ok(ProjectMutationJournal { project, files })
    }

    /// The ordered captured entries in this journal's write set.
    #[must_use]
    pub fn files(&self) -> &[ProjectMutationFile] {
        &self.files
    }

    /// Checks that this journal belongs to `root`'s current project identity.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when `root` names another project or its directory was
    /// replaced after journal capture.
    pub fn validate_project(&self, root: &Utf8Path) -> Result<()> {
        self.project
            .ensure_same(&ProjectMutationRoot::capture(root)?)
    }

    /// Adds paths absent from this journal while preserving its earlier snapshots.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the journals belong to different projects.
    pub fn extend_missing(&mut self, other: &ProjectMutationJournal) -> Result<()> {
        self.project.ensure_same(&other.project)?;
        for file in &other.files {
            if self.files.iter().all(|existing| existing.path != file.path) {
                self.files.push(file.clone());
            }
        }
        Ok(())
    }

    /// Capture the current contents of one project-relative file.
    ///
    /// Missing files are recorded as `None`, which tells [`restore`](Self::restore) to remove them
    /// if a trial created them.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the path is not a regular file, changes identity
    /// during capture, or cannot be read.
    fn capture_file(root: &Utf8Path, rel: &Utf8Path) -> Result<ProjectMutationFile> {
        ProjectMutationFile::capture(root, rel)
    }

    /// Captures the current state of every file in this journal's write set.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) if the project identity changed or a file cannot
    /// be read safely.
    pub fn capture_state(&self) -> Result<ProjectMutationState> {
        self.project.validate_live()?;
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
    /// Returns [`CoreError::LockConflict`](crate::CoreError::LockConflict) when the expected state
    /// belongs to another project, does not describe this journal, or a live file changed
    /// independently.
    /// Returns a filesystem error if validation or restoration cannot read, replace, or remove a
    /// file.
    pub fn restore_if_unchanged(&self, expected: &ProjectMutationState) -> Result<()> {
        self.project.ensure_same(&expected.project)?;
        self.project.validate_live()?;
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
    /// Returns a [`CoreError`](crate::CoreError) if the project identity changed or a file cannot
    /// be restored safely.
    pub fn restore(&self) -> Result<()> {
        self.project.validate_live()?;
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
                return ProjectMutationFile::from_snapshot(rel.to_owned(), None, None);
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
        let final_metadata = std::fs::symlink_metadata(&path)?;
        let final_identity = same_file::Handle::from_path(&path)?;
        if !final_metadata.file_type().is_file() || identity != final_identity {
            return Err(CoreError::LockConflict(format!(
                "{path} changed identity while cooldown read its mutation journal"
            )));
        }
        ProjectMutationFile::from_snapshot(rel.to_owned(), Some(bytes), Some(opened.permissions()))
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
        if self == other {
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
    /// Returns a [`CoreError`](crate::CoreError) when the path is unsafe or the snapshot fields are
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
        safe_mutation_path(&file.path)
            && file.contents.is_some() == file.permissions.is_some()
            && paths.insert(file.path.as_str())
    })
}

fn safe_mutation_path(path: &Utf8Path) -> bool {
    !path.as_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, camino::Utf8Component::Normal(_)))
}

fn validate_mutation_ancestors(root: &Utf8Path, path: &Utf8Path) -> Result<()> {
    if !safe_mutation_path(path) {
        return Err(CoreError::Filesystem(format!(
            "refusing unsafe project-relative mutation path `{path}`"
        )));
    }
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let mut current = root.to_owned();
    for component in parent.components() {
        let camino::Utf8Component::Normal(component) = component else {
            return Err(CoreError::Filesystem(format!(
                "refusing unsafe project-relative mutation path `{path}`"
            )));
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

        std::assert_matches!(result, Err(CoreError::Filesystem(_)));
        assert_eq!(
            std::fs::read_to_string(external_root.join("Cargo.toml"))?,
            "external"
        );
        Ok(())
    }
}
