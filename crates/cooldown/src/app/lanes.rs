//! The concurrency plan of a run: which projects may run at the same time.
//!
//! Every tool gets a lane.
//! Lanes run concurrently, since different package managers wait on different registries and
//! resolvers, which is where the overlap pays.
//! The projects inside a lane run one after another: concurrent invocations of one package
//! manager block on that manager's own cache lock, and a tool's next project is the same
//! manager invoked again.
//!
//! The project lease is held per manifest family at a root (see
//! [`ManifestFamily`](cooldown_core::fs::ManifestFamily)): cargo and uv at one directory hold
//! different leases and never wait on each other, while uv and poetry, which both rewrite
//! `pyproject.toml`, share one — and a same-process conflict on a lease fails at once instead of
//! waiting.
//! So under a command that takes an exclusive lease, two tools of one family whose roots coincide
//! or nest are merged into one lane and run in order: a shared root is one lease file, and an
//! enclosing workspace may rewrite the same manifest below it that the nested project owns (a
//! pnpm workspace rewrites a member's `package.json`, which an npm project at that member owns
//! too).
//! Under a command that only reads, every tool reads side by side under shared leases, which
//! never conflict.
//! Roots are compared as the scan spelled them, which is one spelling per directory: the walk
//! never follows a directory symlink, so no directory is yielded twice.
//! A workspace assembled by an embedder from two spellings of one directory plans them as two
//! roots; their leases still meet on the canonical path, so the second fails at once rather than
//! writing alongside the first.
//!
//! `--build` installs into environments shared at a root, which the family leases leave open,
//! so builds take turns; see [`Workspace::build_project`].
//!
//! `--jobs` caps how many lanes run at once; without it every tool's lane starts immediately.

use super::{ProjectCtx, RunOpts, Workspace};
use camino::Utf8Path;
use cooldown_core::ToolId;
use cooldown_core::fs::ManifestFamily;
use futures::future::join_all;
use std::future::Future;
use std::num::NonZeroUsize;
use tokio::sync::Semaphore;

/// The strongest project lease a command takes on any of its projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaneAccess {
    /// Every project is read under a shared lease, so projects of two tools at one root may run
    /// side by side.
    Shared,
    /// Some project takes an exclusive lease, so projects of one manifest family whose roots
    /// coincide or nest run in one lane.
    Exclusive,
}

impl LaneAccess {
    /// The access a read command implies: `--lock` refreshes under an exclusive lease, except
    /// in a dry run, which never refreshes.
    pub(crate) fn for_lock_refresh(opts: &RunOpts) -> Self {
        if opts.refreshes_lock() {
            LaneAccess::Exclusive
        } else {
            LaneAccess::Shared
        }
    }

    /// The access a mutating command implies: exclusive, except in a dry run, which mutates a
    /// throwaway copy under a shared lease.
    pub(crate) fn for_mutation(opts: &RunOpts) -> Self {
        if opts.mutates_source() {
            LaneAccess::Exclusive
        } else {
            LaneAccess::Shared
        }
    }
}

/// The run's in-scope projects partitioned into lanes; see the module docs.
pub(crate) struct Lanes<'a> {
    lanes: Vec<Lane<'a>>,
    /// How many lanes may run at once; `None` runs them all.
    jobs: Option<NonZeroUsize>,
}

struct Lane<'a> {
    tools: Vec<ToolId>,
    /// The lane's projects, kept ascending by scoped position so the lane runs them in the
    /// run's order.
    projects: Vec<LaneProject<'a>>,
}

struct LaneProject<'a> {
    /// The project's position in the run's scoped order.
    index: usize,
    project: &'a ProjectCtx,
    /// The family of the project's lease, as its adapter declares it.
    family: ManifestFamily,
}

impl<'a> Lane<'a> {
    /// Whether a project here could contend with a project of `family` at `root` under an
    /// exclusive lease: the two rewrite the same manifest, and their roots coincide, so they
    /// share one lease file, or one root encloses the other, so the enclosing workspace may
    /// rewrite the manifest the nested project owns.
    /// `starts_with` compares whole components, so sibling directories with a common prefix
    /// stay apart.
    fn contends_with(&self, root: &Utf8Path, family: &ManifestFamily) -> bool {
        self.projects.iter().any(|held| {
            held.family == *family
                && (held.project.project.root.starts_with(root)
                    || root.starts_with(&held.project.project.root))
        })
    }

    fn absorb(&mut self, other: Lane<'a>) {
        for tool in other.tools {
            if !self.tools.contains(&tool) {
                self.tools.push(tool);
            }
        }
        self.projects.extend(other.projects);
        self.projects.sort_by_key(|project| project.index);
    }
}

impl<'a> Lanes<'a> {
    /// Plans lanes over `projects` in scoped order, each with the family its adapter declares.
    fn plan(
        projects: impl Iterator<Item = (&'a ProjectCtx, ManifestFamily)>,
        access: LaneAccess,
        jobs: Option<NonZeroUsize>,
    ) -> Self {
        let mut lanes: Vec<Lane<'a>> = Vec::new();
        for (index, (project, family)) in projects.enumerate() {
            // The lane of the project's tool and, under exclusive access, every lane that
            // could contend with it: they all have to become one lane.
            let matching: Vec<usize> = lanes
                .iter()
                .enumerate()
                .filter(|(_, lane)| {
                    lane.tools.contains(&project.tool)
                        || (access == LaneAccess::Exclusive
                            && lane.contends_with(&project.project.root, &family))
                })
                .map(|(position, _)| position)
                .collect();
            let planned = LaneProject {
                index,
                project,
                family,
            };
            let Some(&target) = matching.first() else {
                lanes.push(Lane {
                    tools: vec![project.tool],
                    projects: vec![planned],
                });
                continue;
            };
            // Every other matching lane folds into the target lane, which is the lowest of
            // them: removing from the back keeps each remaining position valid, and taking the
            // target out by value leaves no lookup that could quietly drop a project.
            let absorbed: Vec<Lane<'a>> = matching
                .iter()
                .skip(1)
                .rev()
                .map(|&position| lanes.remove(position))
                .collect();
            let mut lane = lanes.remove(target);
            for other in absorbed {
                lane.absorb(other);
            }
            if !lane.tools.contains(&project.tool) {
                lane.tools.push(project.tool);
            }
            lane.projects.push(planned);
            lanes.insert(target, lane);
        }
        Lanes { lanes, jobs }
    }

    /// Runs `work` on every project — the lanes concurrently, each lane's projects in order —
    /// and returns the outputs in the run's scoped order, so merged diagnostics read exactly as
    /// a sequential run's would.
    ///
    /// The lanes are polled on the calling task rather than spawned: the work is bound on
    /// subprocesses and registries, which are already asynchronous, so overlapping their waits
    /// is the whole gain, and borrowing the workspace stays possible.
    pub(crate) async fn run<T, F, Fut>(self, work: F) -> Vec<T>
    where
        F: Fn(&'a ProjectCtx) -> Fut,
        Fut: Future<Output = T>,
    {
        let work = &work;
        // Every lane gets a permit at once unless `--jobs` caps them; the cap is clamped to the
        // lane count, which keeps it below the semaphore's permit limit, and the semaphore is
        // never closed, so a permit is always granted eventually.
        // The floor of one only matters for an empty plan, which never acquires.
        let lanes = self.lanes.len();
        let permits = Semaphore::new(self.jobs.map_or(lanes, |jobs| jobs.get().min(lanes)).max(1));
        let permits = &permits;
        let lanes = join_all(self.lanes.into_iter().map(|lane| async move {
            let _permit = permits.acquire().await.ok();
            let mut outputs = Vec::with_capacity(lane.projects.len());
            for LaneProject { index, project, .. } in lane.projects {
                outputs.push((index, work(project).await));
            }
            outputs
        }))
        .await;
        let mut outputs: Vec<(usize, T)> = lanes.into_iter().flatten().collect();
        outputs.sort_by_key(|(index, _)| *index);
        outputs.into_iter().map(|(_, output)| output).collect()
    }
}

impl Workspace {
    /// The run's lanes over its in-scope projects (see [`Lanes`]).
    pub(crate) fn lanes<'a>(&'a self, opts: &'a RunOpts, access: LaneAccess) -> Lanes<'a> {
        Lanes::plan(
            self.scoped_projects(opts)
                .map(|pctx| (pctx, self.lease_family(pctx))),
            access,
            opts.jobs,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{LaneAccess, Lanes};
    use crate::app::{ProjectCtx, RunOpts};
    use camino::Utf8PathBuf;
    use cooldown_core::fs::ManifestFamily;
    use cooldown_core::{PolicyStack, Project, ToolId};
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const CARGO: ToolId = ToolId("cargo");
    const GO: ToolId = ToolId("go");
    const UV: ToolId = ToolId("uv");
    const POETRY: ToolId = ToolId("poetry");

    /// The manifest each tool rewrites: uv and poetry share `pyproject.toml`.
    fn manifest(tool: ToolId) -> &'static str {
        match tool.as_str() {
            "cargo" => "Cargo.toml",
            "go" => "go.mod",
            _ => "pyproject.toml",
        }
    }

    fn project(tool: ToolId, root: &str) -> ProjectCtx {
        let root = Utf8PathBuf::from(root);
        ProjectCtx {
            tool,
            project: Project {
                manifest: root.join(manifest(tool)),
                root,
                kind: tool,
                exclude_newer: None,
            },
            rel_path: Utf8PathBuf::from("."),
            policy: PolicyStack {
                layers: Vec::new(),
                strict_native: false,
            },
            edge_policy: cooldown_core::EdgePolicy::default(),
            single_copy: Vec::new(),
        }
    }

    /// Each project paired with its family, as the workspace hands them to the planner: the
    /// file name of the manifest the tool rewrites.
    fn planned(projects: &[ProjectCtx]) -> impl Iterator<Item = (&ProjectCtx, ManifestFamily)> {
        projects
            .iter()
            .map(|project| (project, ManifestFamily::of(&project.project.manifest)))
    }

    /// The tools of each lane, in lane order.
    fn lane_tools(lanes: &Lanes<'_>) -> Vec<Vec<&'static str>> {
        lanes
            .lanes
            .iter()
            .map(|lane| lane.tools.iter().map(ToolId::as_str).collect())
            .collect()
    }

    /// A read command holds an exclusive lease only to refresh the lock, which a dry run never
    /// does; a mutation holds one unless it is a dry run, which mutates a copy.
    #[test]
    fn access_follows_the_lease_the_options_imply() {
        let opts = |lock: bool, dry_run: bool| RunOpts {
            lock,
            dry_run,
            ..RunOpts::default()
        };

        assert_eq!(
            LaneAccess::for_lock_refresh(&opts(false, false)),
            LaneAccess::Shared
        );
        assert_eq!(
            LaneAccess::for_lock_refresh(&opts(true, false)),
            LaneAccess::Exclusive
        );
        assert_eq!(
            LaneAccess::for_lock_refresh(&opts(true, true)),
            LaneAccess::Shared
        );
        assert_eq!(
            LaneAccess::for_mutation(&opts(false, false)),
            LaneAccess::Exclusive
        );
        assert_eq!(
            LaneAccess::for_mutation(&opts(false, true)),
            LaneAccess::Shared
        );
    }

    #[test]
    fn each_tool_gets_a_lane_in_first_seen_order() {
        let projects = [
            project(CARGO, "/repo"),
            project(GO, "/repo/svc"),
            project(CARGO, "/repo/tools"),
            project(UV, "/repo/py"),
        ];
        let lanes = Lanes::plan(planned(&projects), LaneAccess::Shared, None);

        assert_eq!(lane_tools(&lanes), [vec!["cargo"], vec!["go"], vec!["uv"]]);
        // The cargo lane runs its two projects in scoped order.
        let cargo: Vec<usize> = lanes.lanes[0]
            .projects
            .iter()
            .map(|project| project.index)
            .collect();
        assert_eq!(cargo, [0, 2]);
    }

    /// A polyglot root (cargo, go, and uv all at `.`) reads side by side under shared leases,
    /// which never conflict.
    #[test]
    fn shared_access_keeps_tools_at_one_root_apart() {
        let projects = [
            project(CARGO, "/repo"),
            project(GO, "/repo"),
            project(UV, "/repo"),
        ];
        let lanes = Lanes::plan(planned(&projects), LaneAccess::Shared, None);

        assert_eq!(lane_tools(&lanes), [vec!["cargo"], vec!["go"], vec!["uv"]]);
    }

    /// Tools of different manifest families at one root hold different leases and rewrite
    /// different files, so even under an exclusive lease they keep their own lanes.
    #[test]
    fn exclusive_access_keeps_tools_of_different_families_at_one_root_apart() {
        let projects = [
            project(CARGO, "/repo"),
            project(GO, "/repo"),
            project(UV, "/repo"),
        ];
        let lanes = Lanes::plan(planned(&projects), LaneAccess::Exclusive, None);

        assert_eq!(lane_tools(&lanes), [vec!["cargo"], vec!["go"], vec!["uv"]]);
    }

    /// Two tools of one family at one root (uv and poetry both rewrite `pyproject.toml`) share
    /// a lease, and a same-process conflict on it fails at once, so they share a lane; a tool
    /// in a sibling directory keeps its own.
    #[test]
    fn exclusive_access_merges_tools_of_one_family_that_share_a_root() {
        let projects = [
            project(UV, "/repo/py"),
            project(GO, "/repo/go"),
            project(POETRY, "/repo/py"),
        ];
        let lanes = Lanes::plan(planned(&projects), LaneAccess::Exclusive, None);

        assert_eq!(lane_tools(&lanes), [vec!["uv", "poetry"], vec!["go"]]);
        let merged: Vec<usize> = lanes.lanes[0]
            .projects
            .iter()
            .map(|project| project.index)
            .collect();
        assert_eq!(merged, [0, 2]);
    }

    /// A project inside another tool's root of the same family may have its manifest rewritten
    /// by that workspace, so under an exclusive lease the two share a lane; a sibling directory
    /// with a common name prefix is not inside it, and a nested tool of another family rewrites
    /// nothing the workspace touches.
    #[test]
    fn exclusive_access_merges_a_same_family_tool_nested_in_another_root() {
        let projects = [
            project(UV, "/repo/app"),
            project(POETRY, "/repo/app/py"),
            project(POETRY, "/repo/apps"),
            project(CARGO, "/repo/app/rs"),
        ];
        let lanes = Lanes::plan(planned(&projects), LaneAccess::Exclusive, None);

        assert_eq!(lane_tools(&lanes), [vec!["uv", "poetry"], vec!["cargo"]]);
    }

    /// Nesting merges whichever project comes first: an enclosing root scoped after its nested
    /// project still pulls it into one lane.
    #[test]
    fn exclusive_access_merges_an_enclosing_root_scoped_later() {
        let projects = [project(POETRY, "/repo/py"), project(UV, "/repo")];
        let lanes = Lanes::plan(planned(&projects), LaneAccess::Exclusive, None);

        assert_eq!(lane_tools(&lanes), [vec!["poetry", "uv"]]);
    }

    /// Reading needs no exclusive lease, so nesting does not merge lanes.
    #[test]
    fn shared_access_keeps_nested_tools_apart() {
        let projects = [project(UV, "/repo"), project(POETRY, "/repo/py")];
        let lanes = Lanes::plan(planned(&projects), LaneAccess::Shared, None);

        assert_eq!(lane_tools(&lanes), [vec!["uv"], vec!["poetry"]]);
    }

    /// `--jobs` caps how many lanes are live at once.
    #[tokio::test]
    async fn a_jobs_cap_bounds_the_live_lanes() {
        let projects = [
            project(CARGO, "/repo"),
            project(GO, "/repo"),
            project(UV, "/repo"),
        ];
        // A cap above the lane count is clamped to it, so the permit count stays valid.
        for (jobs, expected_peak) in [(1, 1), (2, 2), (usize::MAX, 3)] {
            let lanes = Lanes::plan(
                planned(&projects),
                LaneAccess::Shared,
                NonZeroUsize::new(jobs),
            );
            let live = AtomicUsize::new(0);
            let peak = AtomicUsize::new(0);

            lanes
                .run(|_| async {
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    for _ in 0..4 {
                        tokio::task::yield_now().await;
                    }
                    live.fetch_sub(1, Ordering::SeqCst);
                })
                .await;

            assert_eq!(peak.load(Ordering::SeqCst), expected_peak, "jobs = {jobs}");
        }
    }

    /// No projects plan no lanes and run nothing.
    #[tokio::test]
    async fn no_projects_run_nothing() {
        let outputs: Vec<()> = Lanes::plan(
            std::iter::empty::<(&ProjectCtx, ManifestFamily)>(),
            LaneAccess::Shared,
            NonZeroUsize::new(1),
        )
        .run(|_| async {})
        .await;

        assert!(outputs.is_empty());
    }

    /// A project that matches its tool's lane and contends with another lane folds the two
    /// into one: uv at `x` and poetry at `y` are apart until a poetry project at `x` ties
    /// them together, keeping every project and the scoped order.
    #[test]
    fn exclusive_access_merges_transitively_through_a_tool() {
        let projects = [
            project(UV, "/x"),
            project(POETRY, "/y"),
            project(POETRY, "/x"),
        ];
        let lanes = Lanes::plan(planned(&projects), LaneAccess::Exclusive, None);

        assert_eq!(lane_tools(&lanes), [vec!["uv", "poetry"]]);
        let order: Vec<usize> = lanes.lanes[0]
            .projects
            .iter()
            .map(|project| project.index)
            .collect();
        assert_eq!(order, [0, 1, 2]);
    }

    /// Outputs come back in scoped order however the lanes finish.
    #[tokio::test]
    async fn outputs_follow_the_scoped_order_not_the_finishing_order() {
        let projects = [
            project(CARGO, "/repo"),
            project(GO, "/repo/svc"),
            project(CARGO, "/repo/tools"),
        ];
        let lanes = Lanes::plan(planned(&projects), LaneAccess::Shared, None);

        let outputs = lanes
            .run(|pctx| async move {
                // The cargo lane is the slow one, so go finishes first.
                if pctx.tool == CARGO {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                pctx.project.root.clone()
            })
            .await;

        assert_eq!(outputs, ["/repo", "/repo/svc", "/repo/tools"]);
    }

    /// Two lanes really overlap: each waits at a barrier only the other can release.
    #[tokio::test]
    async fn lanes_run_concurrently() {
        let projects = [project(CARGO, "/repo"), project(GO, "/repo/svc")];
        let lanes = Lanes::plan(planned(&projects), LaneAccess::Shared, None);
        let barrier = tokio::sync::Barrier::new(2);

        let overlapped = tokio::time::timeout(
            Duration::from_secs(5),
            lanes.run(|_| async {
                barrier.wait().await;
            }),
        )
        .await;

        assert!(overlapped.is_ok(), "the lanes ran one after the other");
    }

    /// A merged lane runs its projects strictly one after another.
    #[tokio::test]
    async fn a_merged_lane_never_overlaps_its_projects() {
        let projects = [project(UV, "/repo"), project(POETRY, "/repo")];
        let lanes = Lanes::plan(planned(&projects), LaneAccess::Exclusive, None);
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);

        lanes
            .run(|_| async {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                // Yield so a sibling lane, if there were one, could enter meanwhile.
                for _ in 0..4 {
                    tokio::task::yield_now().await;
                }
                live.fetch_sub(1, Ordering::SeqCst);
            })
            .await;

        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }
}
