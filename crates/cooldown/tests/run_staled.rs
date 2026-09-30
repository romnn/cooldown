//! Locks a mutating run stales itself, driven through the public [`Workspace`] API with a fake
//! tool whose projects resolve against each other's manifests: an apply in one project (or, for
//! the ping-pong case, any read of one) marks another project's lock stale, as a rewrite of the
//! root's `[workspace.dependencies]` stales a cargo-fuzz crate that path-depends on a member.

use async_trait::async_trait;
use camino::Utf8PathBuf;
use color_eyre::eyre;
use cooldown::app::{AdapterSet, Baseline, Exit, ProjectCtx, RunOpts, Workspace};
use cooldown_core::config::builtin_default_layer;
use cooldown_core::{
    Capabilities, CoreError, DepScope, Dependency, Diagnostic, LockStatus, LockVerifyReport,
    MajorKey, NativePolicyLayer, PackageId, PolicyStack, Project, ProjectMarker, Release,
    ReleaseOrder, ReleaseQuality, ToolId, ToolRead, ToolWrite, UpdateKind, Version,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

const FAKE: ToolId = ToolId("fake");

/// How one project's work stales another project's lock.
enum Staling {
    /// Nothing the run does stales a lock.
    Never,
    /// An apply in the first project stales the second's lock.
    OnApply {
        applied_in: Utf8PathBuf,
        stales: Utf8PathBuf,
    },
    /// Every graph read of a project stales every other project's lock, so each project's pass
    /// stales the other and the run can never settle.
    EveryRead,
}

/// A tool with one dependency per project, which has one matured update, and lock state per
/// project that the run's own work can stale.
struct StalingFake {
    staling: Staling,
    /// Each project's dependency version, keyed by project root.
    versions: Mutex<HashMap<Utf8PathBuf, String>>,
    /// The roots whose lock is stale.
    stale: Mutex<HashSet<Utf8PathBuf>>,
    /// The roots whose lock was refreshed, in order.
    refreshes: Mutex<Vec<Utf8PathBuf>>,
}

impl StalingFake {
    fn stale_all_but(&self, root: &Utf8PathBuf) -> eyre::Result<()> {
        let versions = self.versions.lock().map_err(|_| eyre::eyre!("poisoned"))?;
        let mut stale = self.stale.lock().map_err(|_| eyre::eyre!("poisoned"))?;
        stale.extend(versions.keys().filter(|other| *other != root).cloned());
        Ok(())
    }
}

fn poisoned() -> CoreError {
    CoreError::System("fake state poisoned".to_owned())
}

#[async_trait]
impl ToolRead for StalingFake {
    fn id(&self) -> ToolId {
        FAKE
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    fn project_marker(&self) -> ProjectMarker {
        ProjectMarker {
            marker: "lock",
            manifest: "fake.toml",
            alternate_manifests: &[],
            workspace_root: true,
        }
    }

    async fn dependencies(
        &self,
        project: &Project,
        _scope: DepScope,
    ) -> cooldown_core::Result<Vec<Dependency>> {
        if matches!(self.staling, Staling::EveryRead) {
            self.stale_all_but(&project.root).map_err(|_| poisoned())?;
        }
        let current = self
            .versions
            .lock()
            .map_err(|_| poisoned())?
            .get(&project.root)
            .cloned()
            .unwrap_or_else(|| "1.0.0".to_owned());
        Ok(vec![Dependency {
            package: PackageId::new(FAKE, "dep", Some("registry.example".into())),
            advisory_identity: None,
            current: Version::new(&current),
            current_quality: ReleaseQuality::Stable,
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: Vec::new(),
            pinned: false,
            hold_edges: Vec::new(),
        }])
    }

    async fn native_policy(
        &self,
        _project: &Project,
    ) -> cooldown_core::Result<Option<NativePolicyLayer>> {
        Ok(None)
    }

    async fn verify_lock_current(
        &self,
        project: &Project,
    ) -> cooldown_core::Result<LockVerifyReport> {
        let stale = self
            .stale
            .lock()
            .map_err(|_| poisoned())?
            .contains(&project.root);
        Ok(if stale {
            LockVerifyReport {
                status: LockStatus::Stale,
                detail: format!("lock is stale in {}", project.root),
            }
        } else {
            LockVerifyReport {
                status: LockStatus::Current,
                detail: "lock is current".to_owned(),
            }
        })
    }
}

/// The dependency's releases: `1.1.0` matured months before the runs' `now`.
fn releases() -> Vec<Release> {
    [
        ("1.0.0", 0, "2026-01-01T00:00:00Z", None),
        ("1.1.0", 1, "2026-01-15T00:00:00Z", Some(UpdateKind::Minor)),
    ]
    .into_iter()
    .map(|(version, order, published, kind)| Release {
        version: Version::new(version),
        order: ReleaseOrder(vec![order]),
        major: MajorKey(String::new()),
        major_number: Some(1),
        kind_from_current: kind,
        beyond_declared_bound: false,
        beyond_latest_tag: false,
        published_at: published.parse().ok(),
        yanked: false,
        quality: ReleaseQuality::Stable,
    })
    .collect()
}

#[async_trait]
impl cooldown_core::ReleaseFetcher for StalingFake {
    async fn releases(
        &self,
        _dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> cooldown_core::Result<Vec<Release>> {
        Ok(releases())
    }

    async fn locked_release(
        &self,
        dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
    ) -> cooldown_core::Result<Release> {
        releases()
            .into_iter()
            .find(|release| release.version == dep.current)
            .ok_or_else(|| CoreError::NotFound(dep.package.name.clone()))
    }
}

#[async_trait]
impl ToolWrite for StalingFake {
    fn mutation_tool(&self) -> ToolId {
        FAKE
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &cooldown_core::Plan,
    ) -> cooldown_core::Result<cooldown_core::ProjectMutationJournal> {
        cooldown_core::ProjectMutationJournal::capture(
            &project.root,
            std::iter::empty::<&camino::Utf8Path>(),
        )
    }

    async fn apply(
        &self,
        mutation: &cooldown_core::PreparedMutation,
    ) -> cooldown_core::Result<cooldown_core::ApplyReport> {
        let (project, plan, _) = mutation.parts_for(self)?;
        let mut report = cooldown_core::ApplyReport::default();
        for change in &plan.changes {
            self.versions
                .lock()
                .map_err(|_| poisoned())?
                .insert(project.root.clone(), change.to.as_str().to_owned());
            report.applied.push(change.clone());
        }
        if let Staling::OnApply { applied_in, stales } = &self.staling
            && *applied_in == project.root
            && !plan.changes.is_empty()
        {
            self.stale
                .lock()
                .map_err(|_| poisoned())?
                .insert(stales.clone());
        }
        Ok(report)
    }

    async fn build(
        &self,
        _project: &Project,
    ) -> cooldown_core::Result<cooldown_core::VerifyReport> {
        Ok(cooldown_core::VerifyReport {
            ok: true,
            detail: String::new(),
        })
    }

    async fn refresh_lock(
        &self,
        project: &Project,
    ) -> cooldown_core::Result<Option<LockVerifyReport>> {
        self.stale
            .lock()
            .map_err(|_| poisoned())?
            .remove(&project.root);
        self.refreshes
            .lock()
            .map_err(|_| poisoned())?
            .push(project.root.clone());
        Ok(Some(LockVerifyReport {
            status: LockStatus::Current,
            detail: "lock refreshed".to_owned(),
        }))
    }

    fn supports_lock_refresh(&self) -> bool {
        true
    }
}

/// A temp repository with a fake project at each of `roots`, in scoped order, and the fake
/// staling locks the way `staling` names, spelled with roots relative to the repository.
struct Repo {
    _dir: tempfile::TempDir,
    root: Utf8PathBuf,
    ws: Workspace,
    fake: Arc<StalingFake>,
}

fn repo(roots: [&str; 2], staling: impl FnOnce(&Utf8PathBuf) -> Staling) -> eyre::Result<Repo> {
    // A non-Git temp directory coordinates project access under its own `.cooldown/locks`, so
    // the leases the run takes are real.
    let dir = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(dir.path().canonicalize()?)
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    let mut projects = Vec::new();
    let mut versions = HashMap::new();
    for rel in roots {
        let project_root = root.join(rel);
        std::fs::create_dir_all(&project_root)?;
        std::fs::write(project_root.join("fake.toml"), "")?;
        std::fs::write(project_root.join("lock"), "")?;
        versions.insert(project_root.clone(), "1.0.0".to_owned());
        projects.push(ProjectCtx {
            tool: FAKE,
            project: Project {
                manifest: project_root.join("fake.toml"),
                root: project_root,
                kind: FAKE,
                exclude_newer: None,
                generated_members: cooldown_core::GeneratedMembers::undeclared(),
            },
            rel_path: Utf8PathBuf::from(rel),
            policy: PolicyStack {
                layers: vec![builtin_default_layer()],
                strict_native: false,
            },
            edge_policy: cooldown_core::EdgePolicy::default(),
            single_copy: Vec::new(),
            generated_members: None,
        });
    }
    let fake = Arc::new(StalingFake {
        staling: staling(&root),
        versions: Mutex::new(versions),
        stale: Mutex::new(HashSet::new()),
        refreshes: Mutex::new(Vec::new()),
    });
    let mut adapters = AdapterSet::new();
    adapters.register_target_verified_mutator(Arc::clone(&fake))?;
    let ws = Workspace::new(
        adapters,
        projects,
        "2026-06-17T00:00:00Z".parse()?,
        Baseline::default(),
        root.clone(),
        vec![builtin_default_layer()],
    );
    Ok(Repo {
        _dir: dir,
        root,
        ws,
        fake,
    })
}

fn refreshes(repo: &Repo) -> eyre::Result<Vec<Utf8PathBuf>> {
    Ok(repo
        .fake
        .refreshes
        .lock()
        .map_err(|_| eyre::eyre!("poisoned"))?
        .clone())
}

fn stale_lock_projects(diagnostics: &[Diagnostic]) -> Vec<&str> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.kind == cooldown_core::DiagnosticKind::StaleLock)
        .filter_map(|diagnostic| diagnostic.project.as_deref())
        .collect()
}

/// The workspace's apply stales the later project's lock before its turn: the turn refreshes it
/// and the run succeeds, reporting the refresh against the project.
#[tokio::test]
async fn a_lock_an_earlier_project_staled_is_refreshed_at_its_turn() -> eyre::Result<()> {
    let repo = repo(["ws", "zz-fuzz"], |root| Staling::OnApply {
        applied_in: root.join("ws"),
        stales: root.join("zz-fuzz"),
    })?;

    let out = repo.ws.upgrade(&RunOpts::default()).await;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(stale_lock_projects(&out.warnings), ["zz-fuzz"]);
    assert_eq!(refreshes(&repo)?, [repo.root.join("zz-fuzz")]);
    // Both projects' upgrades landed: the refresh only cleared the way for the second.
    assert_eq!(out.summary.applied, 2, "{:?}", out.items);
    Ok(())
}

/// The later project stales the earlier one's lock after its turn: the run probes again once the
/// lanes finish and runs the staled project again, leaving no lock stale behind it.
#[tokio::test]
async fn a_lock_a_later_project_staled_is_refreshed_after_the_lanes() -> eyre::Result<()> {
    let repo = repo(["aa-fuzz", "ws"], |root| Staling::OnApply {
        applied_in: root.join("ws"),
        stales: root.join("aa-fuzz"),
    })?;

    let out = repo.ws.upgrade(&RunOpts::default()).await;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(stale_lock_projects(&out.warnings), ["aa-fuzz"]);
    assert_eq!(refreshes(&repo)?, [repo.root.join("aa-fuzz")]);
    Ok(())
}

/// A lock that was stale before the run began is not the run's doing: it keeps the ordinary
/// stale-lock failure and is never refreshed.
#[tokio::test]
async fn a_lock_stale_at_the_start_is_not_refreshed() -> eyre::Result<()> {
    let repo = repo(["aa", "bb"], |_| Staling::Never)?;
    repo.fake
        .stale
        .lock()
        .map_err(|_| eyre::eyre!("poisoned"))?
        .insert(repo.root.join("bb"));

    let out = repo.ws.upgrade(&RunOpts::default()).await;

    assert_eq!(stale_lock_projects(&out.errors), ["bb"]);
    assert_eq!(out.exit, Exit::Environment);
    assert!(refreshes(&repo)?.is_empty());
    Ok(())
}

/// Projects that keep staling each other are re-run a bounded number of times, and the lock
/// still stale after that is reported with its cause instead of being chased forever.
#[tokio::test]
async fn projects_that_keep_staling_each_other_stop_at_the_bound() -> eyre::Result<()> {
    let repo = repo(["aa", "bb"], |_| Staling::EveryRead)?;

    let out = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        repo.ws.upgrade(&RunOpts::default()),
    )
    .await
    .map_err(|_| eyre::eyre!("the run did not finish"))?;

    assert_eq!(out.exit, Exit::Environment);
    assert_eq!(stale_lock_projects(&out.errors), ["aa"]);
    assert!(
        out.errors.iter().any(|error| error
            .message
            .contains("still stale after 2 rounds of re-runs")),
        "{:?}",
        out.errors
    );
    Ok(())
}

/// A dry run mutates nothing, so it never probes for or refreshes a lock it could not stale.
#[tokio::test]
async fn a_dry_run_refreshes_nothing() -> eyre::Result<()> {
    let repo = repo(["ws", "zz-fuzz"], |root| Staling::OnApply {
        applied_in: root.join("ws"),
        stales: root.join("zz-fuzz"),
    })?;
    let opts = RunOpts {
        dry_run: true,
        ..RunOpts::default()
    };

    let out = repo.ws.upgrade(&opts).await;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(refreshes(&repo)?.is_empty());
    Ok(())
}
