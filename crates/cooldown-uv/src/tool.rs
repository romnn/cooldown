//! The Python/uv [`Tool`]: detection, the resolved graph + per-file upload times from
//! `uv.lock`, `PyPI` as the publish-time fallback, native `[tool.uv]` cooldown config, and
//! `uv`-driven resolution/apply. The core owns the verdict; uv only resolves/applies a window.

use crate::artifact::published_at_for_artifacts;
use crate::lock::UvLock;
use crate::native::parse_native;
use crate::pypi::{PYPI, PyPi};
use crate::uvcmd::Uv;
use crate::version;
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_adapter_util::{
    build_registry_releases, single_lock_journal, skipped_on_apply_error, verify_current_report,
};
use cooldown_core::{
    ApplyReport, ArtifactScope, Capabilities, DepScope, Dependency, FetchContext,
    NativePolicyLayer, PackageId, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, RawRelease, Release, ReleaseOrder, ReleaseQuality, ResolvedPolicy,
    Result, SyncReport, ToolId, ToolRead, ToolWrite, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;

/// The [`ToolId`] for the Python/uv adapter (`"uv"`).
pub const UV_ID: ToolId = ToolId("uv");

/// The Python/uv implementation of the [`Tool`] port.
///
/// It detects `uv.lock` projects, reads the resolved graph and per-file upload
/// times from the lock (falling back to [`PyPi`] for the publish instant), parses
/// `[tool.uv]` cooldown config as a native policy layer, and drives the `uv` CLI
/// to re-resolve and apply a chosen window. The verdict itself is the core's;
/// uv only resolves and applies.
pub struct UvTool {
    pypi: PyPi,
    uv: Uv,
}

impl UvTool {
    /// Creates the adapter from a configured [`PyPi`] client.
    #[must_use]
    pub fn new(pypi: PyPi) -> Self {
        UvTool {
            pypi,
            uv: Uv::new(),
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`PyPi`] client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        UvTool::new(PyPi::new(http))
    }
}

fn classify_quality(v: &str) -> ReleaseQuality {
    if version::is_prerelease(v) {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

/// Builds the sorted, deduplicated [`Release`] list the core consumes.
///
/// Unparsable versions are dropped, the rest are sorted by [`version::compare`]
/// and deduplicated, then each is stamped with its update kind relative to
/// `current`, its quality, and an opaque [`ReleaseOrder`] token: the big-endian
/// index, so byte-lexicographic order matches PEP 440 order. The index is widened
/// with [`u32::try_from`] and saturated at [`u32::MAX`], which a real release count
/// can never reach.
#[must_use]
pub fn build_releases(
    current: &str,
    raw: Vec<RawRelease>,
    dep: &Dependency,
    fetch: &FetchContext<'_>,
) -> Vec<Release> {
    let raw: Vec<RawRelease> = raw
        .into_iter()
        .map(|mut release| {
            if matches!(fetch.artifacts, ArtifactScope::Environment) {
                release.published_at =
                    published_at_for_artifacts(&release.artifacts, &dep.artifacts);
            }
            release
        })
        .collect();
    build_registry_releases(
        current,
        raw,
        |value| version::parse(value).is_some(),
        version::compare,
        version::major_key,
        version::classify_kind,
        classify_quality,
    )
}

fn read_lock(project: &Project) -> Result<UvLock> {
    let content = std::fs::read_to_string(project.root.join("uv.lock"))?;
    UvLock::parse(&content)
}

#[async_trait]
impl ToolRead for UvTool {
    fn id(&self) -> ToolId {
        UV_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: true,
            artifact_granular: true,
        }
    }

    fn project_marker(&self) -> ProjectMarker {
        // A `uv.lock` marks a workspace root; nested locks below it belong to the same uv workspace.
        ProjectMarker {
            lockfile: "uv.lock",
            manifest: "pyproject.toml",
            workspace_root: true,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let lock = read_lock(project)?;
        let direct: std::collections::HashSet<String> = lock.direct_names().into_iter().collect();
        let floors = lock.graph_floors();

        let mut deps = Vec::new();
        for pkg in &lock.packages {
            let Some(source) = &pkg.source else { continue };
            if source.is_root() || !source.is_registry() {
                continue; // skip the root project and non-registry (path/git) packages
            }
            let Some(version) = &pkg.version else {
                continue;
            };
            let is_direct = direct.contains(&pkg.name);
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            deps.push(Dependency {
                package: PackageId::new(UV_ID, pkg.name.clone(), Some(PYPI.to_string())),
                current: Version::new(version.clone()),
                current_quality: classify_quality(version),
                direct: is_direct,
                artifacts: pkg.artifact_ids(),
                graph_floor: floors.get(&pkg.name).map(|v| Version::new(v.clone())),
            });
        }
        Ok(deps)
    }

    async fn releases(
        &self,
        dep: &Dependency,
        fetch: &FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.pypi.releases(&dep.package).await?;
        Ok(build_releases(dep.current.as_str(), raw, dep, fetch))
    }

    async fn locked_release(&self, dep: &Dependency, fetch: &FetchContext<'_>) -> Result<Release> {
        // Prefer the lock's recorded per-file upload time; fall back to PyPI.
        let from_lock = read_lock(fetch.project).ok().and_then(|lock| {
            lock.find(&dep.package.name, dep.current.as_str())
                .and_then(|package| {
                    let selected = match fetch.artifacts {
                        ArtifactScope::Environment => dep.artifacts.as_slice(),
                        ArtifactScope::All => &[],
                    };
                    package.published_at_for_artifacts(selected)
                })
        });
        let time = match from_lock {
            Some(t) => Some(t),
            None => {
                self.pypi
                    .published_at(
                        &dep.package,
                        &dep.current,
                        match fetch.artifacts {
                            ArtifactScope::Environment => &dep.artifacts,
                            ArtifactScope::All => &[],
                        },
                    )
                    .await?
            }
        };
        Ok(Release {
            version: dep.current.clone(),
            order: ReleaseOrder(Vec::new()),
            major: version::major_key(dep.current.as_str()),
            kind_from_current: None,
            published_at: time,
            yanked: false,
            quality: dep.current_quality,
        })
    }

    async fn native_policy(&self, project: &Project) -> Result<Option<NativePolicyLayer>> {
        parse_native(&project.manifest)
    }

    async fn verify_lock_current(&self, project: &Project) -> Result<VerifyReport> {
        match self.uv.verify_check(&project.root).await {
            Ok(ok) => Ok(verify_current_report(
                ok,
                "uv.lock is current",
                "uv.lock is stale; run `uv lock`",
            )),
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl ToolWrite for UvTool {
    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        single_lock_journal(&project.root, Utf8Path::new("uv.lock"))
    }

    async fn apply(
        &self,
        project: &Project,
        plan: &Plan,
        _journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            match self
                .uv
                .upgrade_to(&project.root, &change.package.name, change.to.as_str())
                .await
            {
                Ok(()) => report.applied.push(change.clone()),
                Err(e) => report.skipped.push(skipped_on_apply_error(change, e)?),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.uv.sync(&project.root).await
    }

    async fn write_native(&self, project: &Project, policy: &ResolvedPolicy) -> Result<SyncReport> {
        crate::native::write_native(&project.manifest, policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use cooldown_core::{ArtifactId, Change, CoreError, FetchContext, RawArtifact, RawRelease};
    use jiff::Timestamp;

    #[test]
    fn apply_spawn_failure_is_not_downgraded_to_skip() {
        let change = Change {
            package: PackageId::new(UV_ID, "requests", Some(PYPI.to_string())),
            from: Version::new("2.34.1"),
            to: Version::new("2.34.2"),
            kind: cooldown_core::UpdateKind::Patch,
        };
        let err = CoreError::ToolSpawn {
            tool: "uv".into(),
            detail: "spawn failed".into(),
        };

        let result = skipped_on_apply_error(&change, err);
        assert!(matches!(result, Err(CoreError::ToolSpawn { .. })));
    }

    #[test]
    fn build_releases_respects_environment_artifact_scope() {
        let project = Project {
            root: Utf8PathBuf::from("."),
            kind: UV_ID,
            manifest: Utf8PathBuf::from("pyproject.toml"),
        };
        let dep = Dependency {
            package: PackageId::new(UV_ID, "requests", Some(PYPI.to_string())),
            current: Version::new("2.32.0"),
            current_quality: ReleaseQuality::Stable,
            direct: true,
            artifacts: vec![ArtifactId("wheel:py3-none-any".into())],
            graph_floor: None,
        };
        let raw = vec![RawRelease {
            version: Version::new("2.32.1"),
            published_at: Some("2026-06-05T00:00:00Z".parse::<Timestamp>().unwrap()),
            yanked: false,
            artifacts: vec![
                RawArtifact {
                    id: ArtifactId("wheel:py3-none-any".into()),
                    published_at: Some("2026-06-01T00:00:00Z".parse::<Timestamp>().unwrap()),
                    markers: Vec::new(),
                },
                RawArtifact {
                    id: ArtifactId("sdist".into()),
                    published_at: Some("2026-06-05T00:00:00Z".parse::<Timestamp>().unwrap()),
                    markers: Vec::new(),
                },
            ],
        }];

        let env_fetch = FetchContext {
            project: &project,
            artifacts: ArtifactScope::Environment,
        };
        let all_fetch = FetchContext {
            project: &project,
            artifacts: ArtifactScope::All,
        };

        let env_releases = build_releases(dep.current.as_str(), raw.clone(), &dep, &env_fetch);
        let all_releases = build_releases(dep.current.as_str(), raw, &dep, &all_fetch);

        assert_eq!(
            env_releases[0].published_at.unwrap().to_string(),
            "2026-06-01T00:00:00Z"
        );
        assert_eq!(
            all_releases[0].published_at.unwrap().to_string(),
            "2026-06-05T00:00:00Z"
        );
    }

    #[tokio::test]
    async fn mutation_journal_restore_removes_lock_created_after_capture() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let manifest = root.join("pyproject.toml");
        std::fs::write(
            &manifest,
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let eco = UvTool::from_http(
            cooldown_registry::SharedHttp::new(
                cache_dir.path(),
                cooldown_registry::HttpOptions::default(),
            )
            .expect("http"),
        );
        let project = Project {
            root: root.clone(),
            kind: UV_ID,
            manifest,
        };

        let journal = eco
            .mutation_journal(&project, &Plan::default())
            .await
            .expect("journal");
        let lock = root.join("uv.lock");
        std::fs::write(&lock, "generated").expect("write lock");

        journal.restore(&project.root).expect("restore");
        assert!(!lock.exists());
    }
}
