//! How the tools' lanes overlap, driven through the public [`Workspace`] API with a fake tool
//! family: two tools that only read overlap even at one root, two tools that mutate one root run
//! in turn instead of colliding on the project lease, and the merged report keeps the run's
//! project order however the lanes finish.

use async_trait::async_trait;
use camino::Utf8PathBuf;
use color_eyre::eyre;
use cooldown::app::{AdapterSet, Baseline, ProjectCtx, RunOpts, Workspace};
use cooldown_core::config::builtin_default_layer;
use cooldown_core::{
    Capabilities, CoreError, DepScope, Dependency, LockStatus, LockVerifyReport, MajorKey,
    NativePolicyLayer, PackageId, PolicyStack, Project, ProjectDetection, ProjectMarker, Release,
    ReleaseOrder, ReleaseQuality, ToolId, ToolRead, ToolWrite, UpdateKind, Version,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Barrier, Notify};

const ALPHA: ToolId = ToolId("alpha");
const BETA: ToolId = ToolId("beta");

/// What a fake tool's dependency read does before it answers.
enum Read {
    /// Waits, on its first read, until the other tool's read is live too, so a test proves the
    /// two overlap; later reads (a mutation reads the graph again after applying) pass through.
    Meet(Arc<Barrier>),
    /// Yields a few times, so a sibling lane could enter meanwhile if the scheduler let it.
    Yield,
    /// Fails once the other tool's read has signalled, so that read finished first.
    FailAfter(Arc<Notify>),
    /// Fails at once, signalling the other tool's read on the way out.
    FailAndSignal(Arc<Notify>),
}

/// How many of one step are live and the most that ever were, shared by both tools so a test
/// can tell overlapping steps from serialized ones.
#[derive(Default)]
struct Peak {
    live: AtomicUsize,
    most: AtomicUsize,
    /// How many times the step ran at all, so a peak of one is known to be two steps in turn
    /// rather than one step alone.
    total: AtomicUsize,
}

impl Peak {
    fn enter(&self) {
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        self.total.fetch_add(1, Ordering::SeqCst);
    }

    fn leave(&self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }

    fn most(&self) -> usize {
        self.most.load(Ordering::SeqCst)
    }

    fn total(&self) -> usize {
        self.total.load(Ordering::SeqCst)
    }

    /// Holds the step open across a real sleep, so a sibling's could overlap it if allowed:
    /// a sleep parks the runtime, which is when a sibling woken from the blocking pool, where
    /// the leases are taken, is polled again.
    async fn hold(&self) {
        self.enter();
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.leave();
    }
}

/// What both tools share: the read, install, and build peaks, and the order their reads
/// finished in.
#[derive(Default)]
struct Shared {
    reads: Peak,
    installs: Peak,
    builds: Peak,
    finished: Mutex<Vec<&'static str>>,
}

/// A minimal tool: one dependency with one matured update, a current lock, an apply that
/// records the new version so the re-read after it sees the change land, and a build that
/// yields a while so a sibling's build could overlap it if allowed.
struct LaneFake {
    id: ToolId,
    /// The manifest the tool rewrites, which names the family its project lease guards.
    manifest: &'static str,
    read: Read,
    /// The dependency's version, advanced by an apply.
    version: Mutex<String>,
    /// Whether the apply installs into an environment, like `pip install` does, and so yields a
    /// while under the environment turn.
    installs: bool,
    shared: Arc<Shared>,
    /// Whether the meeting read already happened.
    met: AtomicBool,
}

#[async_trait]
impl ToolRead for LaneFake {
    fn id(&self) -> ToolId {
        self.id
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    fn project_detection(&self) -> ProjectDetection {
        ProjectDetection::Primary(ProjectMarker {
            lockfile: "lock",
            manifest: self.manifest,
            alternate_manifests: &[],
            workspace_root: true,
        })
    }

    async fn dependencies(
        &self,
        _project: &Project,
        _scope: DepScope,
    ) -> cooldown_core::Result<Vec<Dependency>> {
        self.shared.reads.enter();
        let result = match &self.read {
            Read::Meet(barrier) => {
                if self.met.swap(true, Ordering::SeqCst) {
                    Ok(self.graph())
                } else {
                    // A partner that never arrives means the lanes ran in turn; failing this
                    // read names that cause instead of letting the whole run hang.
                    tokio::time::timeout(Duration::from_secs(5), barrier.wait())
                        .await
                        .map(|_| self.graph())
                        .map_err(|_| {
                            CoreError::System(format!(
                                "{}'s read partner never started, so the tools ran in turn",
                                self.id.as_str()
                            ))
                        })
                }
            }
            Read::Yield => {
                for _ in 0..4 {
                    tokio::task::yield_now().await;
                }
                Ok(self.graph())
            }
            Read::FailAfter(signal) => {
                signal.notified().await;
                Err(CoreError::System(format!(
                    "{} graph failed",
                    self.id.as_str()
                )))
            }
            Read::FailAndSignal(signal) => {
                signal.notify_one();
                Err(CoreError::System(format!(
                    "{} graph failed",
                    self.id.as_str()
                )))
            }
        };
        self.shared.reads.leave();
        if let Ok(mut finished) = self.shared.finished.lock() {
            finished.push(self.id.as_str());
        }
        result
    }

    async fn native_policy(
        &self,
        _project: &Project,
    ) -> cooldown_core::Result<Option<NativePolicyLayer>> {
        Ok(None)
    }

    async fn verify_lock_current(
        &self,
        _project: &Project,
    ) -> cooldown_core::Result<LockVerifyReport> {
        Ok(LockVerifyReport {
            status: LockStatus::Current,
            detail: "current".to_string(),
        })
    }
}

impl LaneFake {
    /// The one dependency at the version the last apply left it.
    fn graph(&self) -> Vec<Dependency> {
        let current = self
            .version
            .lock()
            .map_or_else(|_| "1.0.0".to_owned(), |version| version.clone());
        vec![Dependency {
            package: PackageId::new(self.id, "dep", Some("registry.example".into())),
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
        }]
    }
}

/// The dependency's releases: `1.1.0` matured months before the runs' `now`, so every command
/// has an update to report and `upgrade` has one to apply.
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
impl cooldown_core::ReleaseFetcher for LaneFake {
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
impl ToolWrite for LaneFake {
    fn mutation_tool(&self) -> ToolId {
        self.id
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

    fn mutation_installs(&self) -> bool {
        self.installs
    }

    async fn apply(
        &self,
        mutation: &cooldown_core::PreparedMutation,
    ) -> cooldown_core::Result<cooldown_core::ApplyReport> {
        let (_, plan, _) = mutation.parts_for(self)?;
        if self.installs {
            self.shared.installs.hold().await;
        }
        let mut report = cooldown_core::ApplyReport::default();
        for change in &plan.changes {
            if let Ok(mut version) = self.version.lock() {
                *version = change.to.as_str().to_owned();
            }
            report.applied.push(change.clone());
        }
        Ok(report)
    }

    async fn build(
        &self,
        _project: &Project,
    ) -> cooldown_core::Result<cooldown_core::VerifyReport> {
        self.shared.builds.hold().await;
        Ok(cooldown_core::VerifyReport {
            ok: true,
            detail: String::new(),
        })
    }
}

/// The two tools, each reading the way its `Read` says and rewriting its manifest, with what
/// they share.
struct Tools {
    adapters: AdapterSet,
    shared: Arc<Shared>,
}

fn tools(reads: [Read; 2], manifests: [&'static str; 2], installs: bool) -> eyre::Result<Tools> {
    let shared = Arc::new(Shared::default());
    let mut adapters = AdapterSet::new();
    for ((id, read), manifest) in [ALPHA, BETA].into_iter().zip(reads).zip(manifests) {
        adapters.register_target_verified_mutator(Arc::new(LaneFake {
            id,
            manifest,
            read,
            version: Mutex::new("1.0.0".to_owned()),
            installs,
            shared: Arc::clone(&shared),
            met: AtomicBool::new(false),
        }))?;
    }
    Ok(Tools { adapters, shared })
}

/// Each tool with a manifest of its own, as cargo and uv are.
const OWN_MANIFESTS: [&str; 2] = ["alpha.toml", "beta.toml"];
/// Both tools rewriting one manifest, as uv and poetry do with `pyproject.toml`.
const SHARED_MANIFEST: [&str; 2] = ["shared.toml", "shared.toml"];

/// Both tools waiting for each other's read.
fn meet() -> [Read; 2] {
    let barrier = Arc::new(Barrier::new(2));
    [Read::Meet(Arc::clone(&barrier)), Read::Meet(barrier)]
}

/// A project of `tool` at `root` with manifest `manifest`, spelled relative to `repo`.
fn project(
    tool: ToolId,
    repo: &Utf8PathBuf,
    root: &Utf8PathBuf,
    manifest: &'static str,
) -> ProjectCtx {
    let rel_path = root
        .strip_prefix(repo)
        .ok()
        .filter(|rel| !rel.as_str().is_empty())
        .map_or_else(|| Utf8PathBuf::from("."), camino::Utf8Path::to_owned);
    ProjectCtx {
        tool,
        project: Project {
            root: root.clone(),
            kind: tool,
            manifest: root.join(manifest),
            exclude_newer: None,
        },
        rel_path,
        policy: PolicyStack {
            layers: vec![builtin_default_layer()],
            strict_native: false,
        },
        edge_policy: cooldown_core::EdgePolicy::default(),
        single_copy: Vec::new(),
    }
}

/// A temp repository with a project of each tool at `roots` (relative to the repo, `.` for the
/// root), each with the manifest and lock the fake declares, whose applies only rewrite files.
fn workspace(
    reads: [Read; 2],
    roots: [&str; 2],
    manifests: [&'static str; 2],
) -> eyre::Result<(tempfile::TempDir, Workspace, Arc<Shared>)> {
    workspace_of(reads, roots, manifests, false)
}

/// [`workspace`] with tools whose applies install when `installs` is set.
fn workspace_of(
    reads: [Read; 2],
    roots: [&str; 2],
    manifests: [&'static str; 2],
    installs: bool,
) -> eyre::Result<(tempfile::TempDir, Workspace, Arc<Shared>)> {
    // A non-Git temp directory coordinates project access under its own `.cooldown/locks`, so
    // the leases the lanes take are real.
    let dir = tempfile::tempdir()?;
    let repo = Utf8PathBuf::from_path_buf(dir.path().canonicalize()?)
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    let mut projects = Vec::new();
    for ((tool, root), manifest) in [ALPHA, BETA].into_iter().zip(roots).zip(manifests) {
        let root = if root == "." {
            repo.clone()
        } else {
            repo.join(root)
        };
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join(manifest), "")?;
        std::fs::write(root.join("lock"), "")?;
        projects.push(project(tool, &repo, &root, manifest));
    }
    let Tools { adapters, shared } = tools(reads, manifests, installs)?;
    let ws = Workspace::new(
        adapters,
        projects,
        "2026-06-17T00:00:00Z".parse()?,
        Baseline::default(),
        repo,
        vec![builtin_default_layer()],
    );
    Ok((dir, ws, shared))
}

/// Two tools that only read overlap even at one root: each dependency read waits for the
/// other's, which only a concurrent run can satisfy.
/// `outdated` also previews the adoptable update by applying it to a copy, which reads the
/// graph again; only the first read of each tool meets the other's.
#[tokio::test]
async fn read_commands_overlap_tools_at_one_root() -> eyre::Result<()> {
    let (_dir, ws, shared) = workspace(meet(), [".", "."], SHARED_MANIFEST)?;

    let out = tokio::time::timeout(Duration::from_secs(30), ws.outdated(&RunOpts::default()))
        .await
        .map_err(|_| eyre::eyre!("the run did not finish"))?;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(shared.reads.most(), 2);
    Ok(())
}

/// Two tools that rewrite one manifest at one root share a lease, and a same-process conflict
/// on it fails at once, so under a mutation they take turns: neither run errors, and no two
/// reads are live.
#[tokio::test]
async fn a_mutation_serializes_tools_sharing_a_root_and_manifest() -> eyre::Result<()> {
    let (_dir, ws, shared) = workspace([Read::Yield, Read::Yield], [".", "."], SHARED_MANIFEST)?;

    let out = ws.upgrade(&RunOpts::default()).await;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(shared.reads.most(), 1);
    Ok(())
}

/// Two tools with manifests of their own at one root hold different leases, so a mutation
/// overlaps them: cargo and uv at one directory never wait on each other.
#[tokio::test]
async fn a_mutation_overlaps_tools_at_one_root_with_their_own_manifests() -> eyre::Result<()> {
    let (_dir, ws, shared) = workspace(meet(), [".", "."], OWN_MANIFESTS)?;

    let out = tokio::time::timeout(Duration::from_secs(30), ws.upgrade(&RunOpts::default()))
        .await
        .map_err(|_| eyre::eyre!("the run did not finish"))?;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(shared.reads.most(), 2);
    Ok(())
}

/// A workspace may rewrite the manifest of a nested project that owns the same manifest, so
/// under a mutation the nested project takes its turn.
#[tokio::test]
async fn a_mutation_serializes_a_nested_tool_sharing_its_manifest() -> eyre::Result<()> {
    let (_dir, ws, shared) = workspace([Read::Yield, Read::Yield], [".", "beta"], SHARED_MANIFEST)?;

    let out = ws.upgrade(&RunOpts::default()).await;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(shared.reads.most(), 1);
    Ok(())
}

/// A nested project with a manifest of its own rewrites nothing the enclosing workspace
/// touches, so a mutation overlaps the two.
#[tokio::test]
async fn a_mutation_overlaps_a_nested_tool_with_its_own_manifest() -> eyre::Result<()> {
    let (_dir, ws, shared) = workspace(meet(), [".", "beta"], OWN_MANIFESTS)?;

    let out = tokio::time::timeout(Duration::from_secs(30), ws.upgrade(&RunOpts::default()))
        .await
        .map_err(|_| eyre::eyre!("the run did not finish"))?;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(shared.reads.most(), 2);
    Ok(())
}

/// `--jobs 1` runs the tools one after another even where they could overlap.
#[tokio::test]
async fn a_jobs_cap_runs_tools_in_turn() -> eyre::Result<()> {
    let (_dir, ws, shared) = workspace([Read::Yield, Read::Yield], [".", "beta"], OWN_MANIFESTS)?;
    let opts = RunOpts {
        jobs: std::num::NonZeroUsize::new(1),
        ..RunOpts::default()
    };

    let out = ws.outdated(&opts).await;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(shared.reads.most(), 1);
    Ok(())
}

/// Reading takes no exclusive lease, so a nested project still reads beside its enclosing one
/// whatever manifest it owns.
#[tokio::test]
async fn read_commands_overlap_nested_tools() -> eyre::Result<()> {
    let (_dir, ws, shared) = workspace(meet(), [".", "beta"], SHARED_MANIFEST)?;

    let out = tokio::time::timeout(Duration::from_secs(30), ws.check(&RunOpts::default()))
        .await
        .map_err(|_| eyre::eyre!("the run did not finish"))?;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(shared.reads.most(), 2);
    Ok(())
}

/// The same mutation on tools in sibling directories overlaps even when they own the same
/// manifest: their leases are different files and neither can rewrite the other's tree.
#[tokio::test]
async fn a_mutation_overlaps_tools_in_sibling_directories() -> eyre::Result<()> {
    let (_dir, ws, shared) = workspace(meet(), ["alpha", "beta"], SHARED_MANIFEST)?;

    let out = tokio::time::timeout(Duration::from_secs(30), ws.upgrade(&RunOpts::default()))
        .await
        .map_err(|_| eyre::eyre!("the run did not finish"))?;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(shared.reads.most(), 2);
    Ok(())
}

/// The merged report keeps the run's project order: the first tool's error still leads even
/// though the second tool's lane finished first.
#[tokio::test]
async fn diagnostics_keep_the_project_order_whichever_lane_finishes_first() -> eyre::Result<()> {
    // The second tool's read fails at once and releases the first tool's, so the second lane
    // finishes first by construction rather than by timing.
    let signal = Arc::new(Notify::new());
    let reads = [
        Read::FailAfter(Arc::clone(&signal)),
        Read::FailAndSignal(signal),
    ];
    let (_dir, ws, shared) = workspace(reads, [".", "beta"], OWN_MANIFESTS)?;

    let out = tokio::time::timeout(Duration::from_secs(30), ws.check(&RunOpts::default()))
        .await
        .map_err(|_| eyre::eyre!("the run did not finish"))?;

    // The second tool's read finished first, so the report's order is the scoped order and
    // not the finishing order.
    let finished = shared
        .finished
        .lock()
        .map_err(|_| eyre::eyre!("the finishing order was poisoned"))?
        .clone();
    assert_eq!(finished, ["beta", "alpha"]);
    let tools: Vec<&str> = out
        .errors
        .iter()
        .map(|error| error.tool.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(tools, ["alpha", "beta"]);
    Ok(())
}

/// `--build` steps take turns even between tools whose lanes overlap, at one root and across
/// roots: every tool installs into the environment the process shares, so one build runs at a
/// time while the reads still overlap.
#[tokio::test]
async fn builds_take_turns_while_reads_overlap() -> eyre::Result<()> {
    for roots in [[".", "."], [".", "beta"]] {
        let (_dir, ws, shared) = workspace(meet(), roots, OWN_MANIFESTS)?;
        let opts = RunOpts {
            build: true,
            ..RunOpts::default()
        };

        let out = tokio::time::timeout(Duration::from_secs(30), ws.upgrade(&opts))
            .await
            .map_err(|_| eyre::eyre!("the run did not finish"))?;

        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(shared.reads.most(), 2, "roots {roots:?}");
        assert_eq!(shared.builds.total(), 2, "roots {roots:?}");
        assert_eq!(shared.builds.most(), 1, "roots {roots:?}");
    }
    Ok(())
}

/// Under `--dry-run` the apply runs on a throwaway copy, but the environment it installs into
/// is the source root's, so that is where its lease is taken: a holder there stops the apply,
/// which a lease on the copy would never meet.
#[tokio::test]
async fn a_dry_run_apply_takes_the_source_roots_environment() -> eyre::Result<()> {
    let (dir, ws, _) = workspace_of(
        [Read::Yield, Read::Yield],
        [".", "beta"],
        OWN_MANIFESTS,
        true,
    )?;
    // The holder is in this process, so the conflict is immediate instead of a wait.
    let held = cooldown_core::fs::ProjectWriteLease::acquire_environment(
        cooldown_core::fs::ProjectCoordination::resolve(
            camino::Utf8Path::from_path(dir.path())
                .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?,
        )?,
    )?;
    let opts = RunOpts {
        dry_run: true,
        ..RunOpts::default()
    };

    let out = tokio::time::timeout(Duration::from_secs(30), ws.upgrade(&opts))
        .await
        .map_err(|_| eyre::eyre!("the run did not finish"))?;

    // Only the tool at the held root fails its apply; the tool at `beta` installs unhindered.
    let failed: Vec<&str> = out
        .items
        .iter()
        .filter(|item| item.error.is_some())
        .map(|item| item.tool.as_str())
        .collect();
    assert_eq!(failed, ["alpha"], "{:?}", out.items);
    drop(held);
    Ok(())
}

/// An apply that installs, like `poetry add`, takes the same turn a build does, so two such
/// tools in overlapping lanes install one after the other even at different roots, where their
/// manifests and leases are unrelated.
#[tokio::test]
async fn installing_applies_take_turns_while_reads_overlap() -> eyre::Result<()> {
    let (_dir, ws, shared) = workspace_of(meet(), [".", "beta"], OWN_MANIFESTS, true)?;

    let out = tokio::time::timeout(Duration::from_secs(30), ws.upgrade(&RunOpts::default()))
        .await
        .map_err(|_| eyre::eyre!("the run did not finish"))?;

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(shared.reads.most(), 2);
    assert_eq!(shared.installs.total(), 2);
    assert_eq!(shared.installs.most(), 1);
    Ok(())
}
