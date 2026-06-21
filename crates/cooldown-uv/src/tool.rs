//! The Python/uv [`Tool`]: detection, the resolved graph + per-file upload times from
//! `uv.lock`, `PyPI` as the publish-time fallback, native `[tool.uv]` cooldown config, and
//! `uv`-driven resolution/apply. The core owns the verdict; uv only resolves/applies a window.

use crate::artifact::published_at_for_artifacts;
use crate::lock::UvLock;
use crate::manifest;
use crate::native::parse_native;
use crate::pypi::{PYPI, PyPi};
use crate::uvcmd::Uv;
use crate::version;
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_adapter_util::{build_registry_releases, verify_current_report};
use cooldown_core::{
    ApplyReport, ArtifactScope, Capabilities, Change, DepScope, Dependency, FetchContext,
    MemberRef, NativePolicyLayer, PackageId, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, RawRelease, Release, ReleaseOrder, ReleaseQuality, ResolvedPolicy,
    Result, RewriteMode, SkipReason, Skipped, SyncReport, ToolId, ToolRead, ToolWrite,
    VerifyReport, Version,
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
        // Each `uv.lock` marks an independent project. A uv *workspace* keeps a single lock at its
        // root and its members carry only a `pyproject.toml` (no nested lock), so a `uv.lock` found
        // below another is never a workspace member — it is a separate project that resolves on its
        // own and must be synced/checked in its own right. Hence `workspace_root: false`: nested
        // locks are not collapsed into the topmost one.
        ProjectMarker {
            lockfile: "uv.lock",
            manifest: "pyproject.toml",
            workspace_root: false,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let lock = read_lock(project)?;
        let direct: std::collections::HashSet<String> = lock.direct_names().into_iter().collect();
        let floors = lock.graph_floors();
        let exact_pins = crate::native::exact_pinned_names(&project.manifest);
        // A uv project is a single package, so it is the source for every dependency it declares.
        // The lock's root package carries the project's package name.
        let project_member: Vec<MemberRef> = lock
            .packages
            .iter()
            .find(|pkg| {
                pkg.source
                    .as_ref()
                    .is_some_and(crate::lock::Source::is_project_root)
            })
            .map(|pkg| {
                vec![MemberRef {
                    name: pkg.name.clone(),
                    path: ".".to_string(),
                }]
            })
            .unwrap_or_default();

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
                members: if is_direct {
                    project_member.clone()
                } else {
                    Vec::new()
                },
                pinned: exact_pins.contains(&pkg.name),
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

/// The result of applying one change: adopted, or skipped with a reason. Tool-spawn failures
/// propagate as `Err` instead, so a broken `uv` aborts rather than masquerading as a per-change skip.
enum ChangeOutcome {
    Applied,
    Skipped(SkipReason),
}

impl UvTool {
    /// Apply one change, honoring the [`RewriteMode`]. `Auto` first tries to move the lock within the
    /// declared requirement (`uv lock --upgrade-package name==version`) and only rewrites the
    /// `pyproject.toml` constraint when that is rejected — typically because the target is past an
    /// upper bound. `Always` rewrites the requirement up front (a no-op for an unconstrained dep).
    async fn apply_change(
        &self,
        project: &Project,
        change: &Change,
        mode: RewriteMode,
    ) -> Result<ChangeOutcome> {
        match mode {
            RewriteMode::Always => {
                manifest::widen_constraint(
                    &project.manifest,
                    &change.package.name,
                    change.to.as_str(),
                )?;
                self.relock(project, change).await
            }
            RewriteMode::Auto => match self.lock_to_target(project, change).await {
                Ok(()) => Ok(ChangeOutcome::Applied),
                Err(err) if err.is_tool_spawn_failure() => Err(err),
                Err(_) => {
                    // The lock-only move was rejected. If the requirement can be widened, do so and
                    // retry; otherwise there is no constraint to relax (the dep is transitive-only or
                    // pinned), so the resolver is holding it where it is — a real conflict.
                    let widened = manifest::widen_constraint(
                        &project.manifest,
                        &change.package.name,
                        change.to.as_str(),
                    )?;
                    if !widened {
                        return Ok(ChangeOutcome::Skipped(SkipReason::ResolverConflict));
                    }
                    self.relock(project, change).await
                }
            },
        }
    }

    /// Re-lock to the target after a (possible) constraint rewrite, mapping a resolver rejection to a
    /// skip and a spawn failure to a fatal error.
    async fn relock(&self, project: &Project, change: &Change) -> Result<ChangeOutcome> {
        match self.lock_to_target(project, change).await {
            Ok(()) => Ok(ChangeOutcome::Applied),
            Err(err) if err.is_tool_spawn_failure() => Err(err),
            Err(_) => Ok(ChangeOutcome::Skipped(SkipReason::ResolverConflict)),
        }
    }

    async fn lock_to_target(&self, project: &Project, change: &Change) -> Result<()> {
        self.uv
            .upgrade_to(&project.root, &change.package.name, change.to.as_str())
            .await
    }
}

#[async_trait]
impl ToolWrite for UvTool {
    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        // Capture the lock and the manifest: `apply` re-locks (uv.lock) and, when the target falls
        // outside the declared requirement, rewrites the constraint (pyproject.toml). Capturing the
        // manifest unconditionally is harmless — restore runs only on rollback.
        Ok(ProjectMutationJournal {
            files: vec![
                ProjectMutationJournal::capture_file(&project.root, Utf8Path::new("uv.lock"))?,
                ProjectMutationJournal::capture_file(
                    &project.root,
                    Utf8Path::new("pyproject.toml"),
                )?,
            ],
        })
    }

    async fn apply(
        &self,
        project: &Project,
        plan: &Plan,
        _journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            match self.apply_change(project, change, plan.rewrite).await? {
                ChangeOutcome::Applied => report.applied.push(change.clone()),
                ChangeOutcome::Skipped(reason) => report.skipped.push(Skipped {
                    change: change.clone(),
                    reason,
                    offending: Some(change.package.clone()),
                }),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.uv.sync(&project.root).await
    }

    async fn write_native(
        &self,
        project: &Project,
        policy: &ResolvedPolicy,
        dry_run: bool,
    ) -> Result<SyncReport> {
        crate::native::write_native(&project.manifest, policy, dry_run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use cooldown_adapter_util::skipped_on_apply_error;
    use cooldown_core::{ArtifactId, CoreError, FetchContext, RawArtifact, RawRelease};
    use jiff::Timestamp;

    #[test]
    fn apply_spawn_failure_is_not_downgraded_to_skip() {
        let change = Change {
            package: PackageId::new(UV_ID, "requests", Some(PYPI.to_string())),
            from: Version::new("2.34.1"),
            to: Version::new("2.34.2"),
            kind: cooldown_core::UpdateKind::Patch,
            direct: true,
            members: Vec::new(),
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
            members: Vec::new(),
            pinned: false,
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
    async fn dependencies_attribute_only_direct_declarations_to_project_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let manifest = root.join("pyproject.toml");
        std::fs::write(
            &manifest,
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        std::fs::write(
            root.join("uv.lock"),
            r#"
version = 1
revision = 3

[[package]]
name = "demo"
version = "0.1.0"
source = { virtual = "." }
dependencies = [{ name = "requests" }]

[[package]]
name = "requests"
version = "2.34.2"
source = { registry = "https://pypi.org/simple" }
dependencies = [{ name = "idna" }]

[[package]]
name = "idna"
version = "3.10"
source = { registry = "https://pypi.org/simple" }
"#,
        )
        .expect("write lock");
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let tool = UvTool::from_http(
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

        let graph = tool
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("deps");
        let direct = graph
            .iter()
            .find(|dep| dep.package.name == "requests")
            .expect("direct dep");
        assert_eq!(
            direct
                .members
                .iter()
                .map(|member| (member.name.as_str(), member.path.as_str()))
                .collect::<Vec<_>>(),
            vec![("demo", ".")]
        );

        let transitive = graph
            .iter()
            .find(|dep| dep.package.name == "idna")
            .expect("transitive dep");
        assert!(!transitive.direct);
        assert!(
            transitive.members.is_empty(),
            "transitive dependencies are not declared by the project member"
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
