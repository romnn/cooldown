//! Defines the trusted whole-project recovery wire format and state classification.

use super::artifact::untrusted_record;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    AcceptedProjectState, CoreError, Project, ProjectMutationFile, ProjectMutationJournal, Result,
};
use sha2::{Digest as _, Sha256};

const PROJECT_RECOVERY_FORMAT: &str = "cooldown-cargo-project-recovery-v1";
pub(super) const RECOVERY_ANCHOR_FORMAT: &str = "cooldown-cargo-recovery-anchor-v1";
pub(crate) const RECOVERY_MARKER: &str = "Cargo.lock.cooldown-recovery";

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(super) struct ProjectRecoveryRecord {
    pub(super) format: String,
    pub(super) project_root: String,
    pub(super) files: Vec<ProjectRecoveryFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(super) struct RecoveryAnchor {
    pub(super) format: String,
    pub(super) project_root: String,
    pub(super) record_digest: String,
}

#[derive(Clone, Copy)]
pub(super) enum ExpectedProjectState {
    Original,
    Candidate,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(super) struct ProjectRecoveryFile {
    pub(super) path: String,
    pub(super) original: Option<String>,
    pub(super) candidate: Option<String>,
    pub(super) original_permissions: RecoveryPermissions,
    pub(super) candidate_permissions: RecoveryPermissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(super) struct RecoveryPermissions {
    pub(super) present: bool,
    pub(super) readonly: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) unix_mode: Option<u32>,
}

impl ProjectRecoveryRecord {
    pub(super) fn new(project: &Project, accepted: &AcceptedProjectState) -> Result<Self> {
        let files = accepted
            .files()
            .map(|(original, candidate)| ProjectRecoveryFile::new(original, candidate))
            .collect::<Result<Vec<_>>>()?;
        if files.is_empty() {
            return Err(CoreError::System(
                "cannot publish an unchanged Cargo project state".to_string(),
            ));
        }
        Ok(ProjectRecoveryRecord {
            format: PROJECT_RECOVERY_FORMAT.to_string(),
            project_root: canonical_project_root(project)?,
            files,
        })
    }

    pub(super) fn validate(&self, project: &Project, path: &Utf8Path) -> Result<()> {
        if self.format != PROJECT_RECOVERY_FORMAT
            || self.project_root != canonical_project_root(project)?
            || self.files.is_empty()
            || !self.files.iter().any(ProjectRecoveryFile::is_changed)
        {
            return Err(untrusted_record(path));
        }
        let mut paths = std::collections::BTreeSet::new();
        for file in &self.files {
            file.validate(path)?;
            if !paths.insert(file.path.as_str()) {
                return Err(untrusted_record(path));
            }
        }
        Ok(())
    }

    pub(super) fn original_journal(&self, root: &Utf8Path) -> Result<ProjectMutationJournal> {
        let files = self
            .files
            .iter()
            .map(|file| file.original_file(root))
            .collect::<Result<Vec<_>>>()?;
        ProjectMutationJournal::from_snapshot(root, files)
    }

    pub(super) fn classify_state(
        &self,
        project: &Project,
        live: &cooldown_core::ProjectMutationState,
        recovery_path: &Utf8Path,
    ) -> Result<(bool, bool)> {
        let mut saw_original = false;
        let mut saw_candidate = false;
        for (recorded, live) in self.files.iter().zip(live.files()) {
            if !recorded.is_changed() {
                if !recorded.matches_original(live) {
                    return Err(CoreError::LockUnreadable(format!(
                        "{} changed after the accepted project trial at {recovery_path}; left all files untouched",
                        project.root.join(live.path())
                    )));
                }
                continue;
            }
            if recorded.matches_original(live) {
                saw_original = true;
            } else if recorded.matches_candidate(live) {
                saw_candidate = true;
            } else {
                return Err(CoreError::LockUnreadable(format!(
                    "{} no longer matches either side of the interrupted project publication at {recovery_path}; left all files untouched",
                    project.root.join(live.path())
                )));
            }
        }
        Ok((saw_original, saw_candidate))
    }

    pub(super) fn validate_expected_state(
        &self,
        project: &Project,
        live: &cooldown_core::ProjectMutationState,
        expected: ExpectedProjectState,
        recovery_path: &Utf8Path,
    ) -> Result<()> {
        for (recorded, live) in self.files.iter().zip(live.files()) {
            let matches = match expected {
                ExpectedProjectState::Original => recorded.matches_original(live),
                ExpectedProjectState::Candidate => {
                    if recorded.is_changed() {
                        recorded.matches_candidate(live)
                    } else {
                        recorded.matches_original(live)
                    }
                }
            };
            if !matches {
                return Err(CoreError::LockUnreadable(format!(
                    "{} changed before recovery evidence could be consumed at {recovery_path}; retained the recovery record",
                    project.root.join(live.path())
                )));
            }
        }
        Ok(())
    }
}

impl RecoveryAnchor {
    pub(super) fn new(project: &Project, record: &[u8]) -> Result<Self> {
        Ok(RecoveryAnchor {
            format: RECOVERY_ANCHOR_FORMAT.to_string(),
            project_root: canonical_project_root(project)?,
            record_digest: bytes_digest(record),
        })
    }

    pub(super) fn validate(&self, project: &Project, record: &[u8], path: &Utf8Path) -> Result<()> {
        let valid = self.format == RECOVERY_ANCHOR_FORMAT
            && self.project_root == canonical_project_root(project)?
            && self.record_digest == bytes_digest(record)
            && is_sha256_digest(&self.record_digest);
        if valid {
            Ok(())
        } else {
            Err(untrusted_record(path))
        }
    }

    pub(super) fn validate_without_record(&self, project: &Project, path: &Utf8Path) -> Result<()> {
        let valid = self.format == RECOVERY_ANCHOR_FORMAT
            && self.project_root == canonical_project_root(project)?
            && is_sha256_digest(&self.record_digest);
        if valid {
            Ok(())
        } else {
            Err(untrusted_record(path))
        }
    }
}
impl ProjectRecoveryFile {
    pub(super) fn new(
        original: &ProjectMutationFile,
        candidate: &ProjectMutationFile,
    ) -> Result<Self> {
        let original_contents = utf8_contents(original)?;
        let candidate_contents = utf8_contents(candidate)?;
        Ok(ProjectRecoveryFile {
            path: original.path().to_string(),
            original: original_contents,
            candidate: candidate_contents,
            original_permissions: RecoveryPermissions::new(original.permissions()),
            candidate_permissions: RecoveryPermissions::new(candidate.permissions()),
        })
    }

    pub(super) fn validate(&self, record_path: &Utf8Path) -> Result<()> {
        let relative = Utf8Path::new(&self.path);
        let safe_path = !relative.as_str().is_empty()
            && !relative.is_absolute()
            && relative
                .components()
                .all(|component| matches!(component, camino::Utf8Component::Normal(_)));
        let cargo_output = matches!(relative.file_name(), Some("Cargo.toml" | "Cargo.lock"));
        let original_valid = self.original.is_some() == self.original_permissions.is_present();
        let candidate_valid = self.candidate.is_some() == self.candidate_permissions.is_present();
        if !safe_path
            || !cargo_output
            || !original_valid
            || !candidate_valid
            || !self.original_permissions.is_valid()
            || !self.candidate_permissions.is_valid()
        {
            return Err(untrusted_record(record_path));
        }
        Ok(())
    }

    pub(super) fn original_file(&self, root: &Utf8Path) -> Result<ProjectMutationFile> {
        ProjectMutationFile::from_snapshot(
            Utf8PathBuf::from(&self.path),
            self.original
                .as_ref()
                .map(|value| value.as_bytes().to_vec()),
            self.original_permissions.to_permissions(root)?,
        )
    }

    pub(super) fn matches_original(&self, live: &ProjectMutationFile) -> bool {
        self.matches(live, self.original.as_deref(), self.original_permissions)
    }

    pub(super) fn is_changed(&self) -> bool {
        self.original != self.candidate || self.original_permissions != self.candidate_permissions
    }

    pub(super) fn matches_candidate(&self, live: &ProjectMutationFile) -> bool {
        self.matches(live, self.candidate.as_deref(), self.candidate_permissions)
    }

    pub(super) fn matches(
        &self,
        live: &ProjectMutationFile,
        contents: Option<&str>,
        permissions: RecoveryPermissions,
    ) -> bool {
        live.path() == Utf8Path::new(&self.path)
            && live.contents() == contents.map(str::as_bytes)
            && permissions.matches(live.permissions())
    }
}

impl RecoveryPermissions {
    pub(super) fn new(permissions: Option<&std::fs::Permissions>) -> Self {
        #[cfg(unix)]
        let unix_mode = permissions.map(|permissions| {
            use std::os::unix::fs::PermissionsExt as _;
            permissions.mode()
        });
        #[cfg(not(unix))]
        let unix_mode = None;
        RecoveryPermissions {
            present: permissions.is_some(),
            readonly: permissions.is_some_and(std::fs::Permissions::readonly),
            unix_mode,
        }
    }

    pub(super) const fn is_present(self) -> bool {
        self.present
    }

    pub(super) const fn is_valid(self) -> bool {
        if !self.present {
            return !self.readonly && self.unix_mode.is_none();
        }
        #[cfg(unix)]
        {
            self.unix_mode.is_some()
        }
        #[cfg(not(unix))]
        {
            self.unix_mode.is_none()
        }
    }

    pub(super) fn matches(self, permissions: Option<&std::fs::Permissions>) -> bool {
        Self::new(permissions) == self
    }

    #[cfg_attr(
        unix,
        expect(
            clippy::unnecessary_wraps,
            reason = "the shared cross-platform signature remains fallible on non-Unix targets"
        )
    )]
    pub(super) fn to_permissions(self, root: &Utf8Path) -> Result<Option<std::fs::Permissions>> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = root;
            Ok(self.unix_mode.map(std::fs::Permissions::from_mode))
        }
        #[cfg(not(unix))]
        {
            if !self.present {
                return Ok(None);
            }
            let mut permissions = std::fs::metadata(root)?.permissions();
            permissions.set_readonly(self.readonly);
            Ok(Some(permissions))
        }
    }
}

pub(super) fn utf8_contents(file: &ProjectMutationFile) -> Result<Option<String>> {
    file.contents()
        .map(|contents| {
            String::from_utf8(contents.to_vec()).map_err(|error| {
                CoreError::Serialization(format!(
                    "Cargo mutation output {} is not UTF-8: {error}",
                    file.path()
                ))
            })
        })
        .transpose()
}

pub(super) fn canonical_project_root(project: &Project) -> Result<String> {
    let canonical = std::fs::canonicalize(&project.root)?;
    Utf8PathBuf::from_path_buf(canonical)
        .map(|path| path.to_string())
        .map_err(|path| CoreError::PathEncoding(format!("non-utf8 path: {}", path.display())))
}

pub(super) fn bytes_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn is_sha256_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
