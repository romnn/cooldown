//! Network-free conformance tests: drive the `Workspace` use cases against a fake `Tool` with
//! canned data, asserting the universal invariants and the cross-cutting behaviours (the check
//! gate, baseline acknowledgement, and the upgrade trial-rollback that never commits a violating
//! lock).
#![allow(
    clippy::unwrap_used,
    reason = "integration-test helpers and the in-file fake adapter; unwrap on known-good fixtures is the intended immediate test failure (clippy.toml sets allow-unwrap-in-tests)"
)]

use async_trait::async_trait;
use camino::Utf8PathBuf;
use cooldown::app::{
    AdapterSet, Baseline, CheckStatus, Exit, OutdatedStatus, ProjectCtx, RunOpts, Workspace,
};
use cooldown_core::config::builtin_default_layer;
use cooldown_core::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

const GO: ToolId = ToolId("go");

fn ts(s: &str) -> jiff::Timestamp {
    s.parse().unwrap()
}
fn now() -> jiff::Timestamp {
    ts("2026-06-17T00:00:00Z")
}

fn rel(v: &str, ord: u32, pub_at: Option<&str>, kind: Option<UpdateKind>) -> Release {
    Release {
        version: Version::new(v),
        order: ReleaseOrder(ord.to_be_bytes().to_vec()),
        major: MajorKey(String::new()),
        kind_from_current: kind,
        published_at: pub_at.map(ts),
        yanked: false,
        quality: ReleaseQuality::Stable,
    }
}

fn dep(name: &str, current: &str, direct: bool) -> Dependency {
    Dependency {
        package: PackageId::new(GO, name, Some("proxy.example".into())),
        current: Version::new(current),
        current_quality: ReleaseQuality::Stable,
        direct,
        artifacts: Vec::new(),
        graph_floor: None,
        graph_ceiling: None,
        members: Vec::new(),
        pinned: false,
    }
}

#[derive(Default)]
struct State {
    /// Simulates a re-lock having dragged in a fresh transitive.
    fresh_transitive_present: bool,
    /// Whether `apply` has already mutated the project once.
    apply_attempted: bool,
    /// Package versions pinned by a successful fake apply, surfaced by the next graph probe.
    applied_versions: HashMap<String, Version>,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "test fixture toggles independent failure modes to exercise the workspace invariants"
)]
struct FakeEco {
    direct: Vec<Dependency>,
    transitive: Vec<Dependency>,
    fresh_transitive: Option<Dependency>,
    releases: HashMap<String, Vec<Release>>,
    locked: HashMap<String, Release>,
    inject_fresh_on_apply: bool,
    stale_lock: bool,
    fail_graph_after_apply: bool,
    fail_locked_release_after_apply_for: Option<String>,
    stale_lock_after_apply: bool,
    build_fails_after_apply: bool,
    state: Mutex<State>,
    root: Utf8PathBuf,
}

impl FakeEco {
    fn project(&self) -> Project {
        Project {
            root: self.root.clone(),
            kind: GO,
            manifest: self.root.join("go.mod"),
            exclude_newer: None,
        }
    }
}

fn apply_versions(
    mut deps: Vec<Dependency>,
    versions: &HashMap<String, Version>,
) -> Vec<Dependency> {
    for dep in &mut deps {
        if let Some(version) = versions.get(&dep.package.name) {
            dep.current = version.clone();
        }
    }
    deps
}

#[async_trait]
impl ToolRead for FakeEco {
    fn id(&self) -> ToolId {
        GO
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: true,
            has_incompatible: true,
            ..Default::default()
        }
    }
    fn project_marker(&self) -> cooldown_core::ProjectMarker {
        cooldown_core::ProjectMarker {
            lockfile: "fake.lock",
            manifest: "fake.toml",
            workspace_root: true,
        }
    }
    async fn dependencies(&self, _p: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let state = self.state.lock().unwrap();
        if scope == DepScope::Graph && self.fail_graph_after_apply && state.apply_attempted {
            return Err(CoreError::Transient("post-apply graph probe failed".into()));
        }
        let mut out = apply_versions(self.direct.clone(), &state.applied_versions);
        if scope == DepScope::Graph {
            out.extend(apply_versions(
                self.transitive.clone(),
                &state.applied_versions,
            ));
            if state.fresh_transitive_present
                && let Some(ft) = &self.fresh_transitive
            {
                // Reflect any applied downgrade so an `upgrade` reconcile pass that rolls the
                // floated-up transitive back is visible on the next graph probe.
                out.extend(apply_versions(vec![ft.clone()], &state.applied_versions));
            }
        }
        Ok(out)
    }
    async fn native_policy(&self, _p: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }
    async fn verify_lock_current(&self, _p: &Project) -> Result<VerifyReport> {
        let stale = self.stale_lock
            || (self.stale_lock_after_apply && self.state.lock().unwrap().apply_attempted);
        Ok(VerifyReport {
            ok: !stale,
            detail: if stale { "stale".into() } else { "tidy".into() },
        })
    }
}

#[async_trait]
impl ReleaseFetcher for FakeEco {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> Result<Vec<Release>> {
        Ok(self
            .releases
            .get(&dep.package.name)
            .cloned()
            .unwrap_or_default())
    }
    async fn locked_release(
        &self,
        dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
    ) -> Result<Release> {
        let applied = {
            let state = self.state.lock().unwrap();
            if state.apply_attempted
                && self
                    .fail_locked_release_after_apply_for
                    .as_deref()
                    .is_some_and(|name| name == dep.package.name)
            {
                return Err(CoreError::Transient(
                    format!(
                        "post-apply locked release probe failed for {}",
                        dep.package.name
                    )
                    .into(),
                ));
            }
            state.applied_versions.get(&dep.package.name).cloned()
        };
        if let Some(version) = applied {
            return self
                .releases
                .get(&dep.package.name)
                .and_then(|releases| releases.iter().find(|release| release.version == version))
                .cloned()
                .ok_or_else(|| CoreError::NotFound(dep.package.name.clone()));
        }
        self.locked
            .get(&dep.package.name)
            .cloned()
            .ok_or_else(|| CoreError::NotFound(dep.package.name.clone()))
    }
}

#[async_trait]
impl ToolWrite for FakeEco {
    async fn mutation_journal(&self, _p: &Project, _plan: &Plan) -> Result<ProjectMutationJournal> {
        Ok(ProjectMutationJournal::default())
    }

    async fn apply(
        &self,
        _p: &Project,
        plan: &Plan,
        _journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        let mut state = self.state.lock().unwrap();
        state.apply_attempted = true;
        if self.inject_fresh_on_apply {
            state.fresh_transitive_present = true;
        }
        for change in &plan.changes {
            state
                .applied_versions
                .insert(change.package.name.clone(), change.to.clone());
        }
        Ok(ApplyReport {
            applied: plan.changes.clone(),
            skipped: Vec::new(),
        })
    }
    async fn build(&self, _p: &Project) -> Result<VerifyReport> {
        Ok(VerifyReport {
            ok: !(self.build_fails_after_apply && self.state.lock().unwrap().apply_attempted),
            detail: if self.build_fails_after_apply && self.state.lock().unwrap().apply_attempted {
                "build failed".into()
            } else {
                "ok".into()
            },
        })
    }
}

fn workspace(fake: FakeEco, baseline: Baseline) -> Workspace {
    let project = fake.project();
    let ctx = ProjectCtx {
        tool: GO,
        project,
        rel_path: Utf8PathBuf::from("."),
        policy: PolicyStack {
            layers: vec![builtin_default_layer()],
            strict_native: false,
        },
    };
    let mut adapters = AdapterSet::new();
    adapters.register(Arc::new(fake));
    Workspace::new(
        adapters,
        vec![ctx],
        now(),
        baseline,
        Utf8PathBuf::from("."),
        Vec::new(),
    )
}

fn opts() -> RunOpts {
    RunOpts {
        concurrency: 4,
        ..Default::default()
    }
}

fn tmp_root() -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    (dir, root)
}

fn fake(
    root: Utf8PathBuf,
    direct: Vec<Dependency>,
    transitive: Vec<Dependency>,
    releases: HashMap<String, Vec<Release>>,
    locked: HashMap<String, Release>,
) -> FakeEco {
    FakeEco {
        direct,
        transitive,
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root,
    }
}

fn too_fresh_fix_releases() -> Vec<Release> {
    vec![
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
        rel(
            "v1.0.1",
            1,
            Some("2026-06-01T00:00:00Z"),
            Some(UpdateKind::Patch),
        ),
        rel(
            "v1.0.2",
            2,
            Some("2026-06-16T00:00:00Z"),
            Some(UpdateKind::Patch),
        ),
    ]
}

fn release_named(releases: &[Release], version: &str) -> Release {
    releases
        .iter()
        .find(|release| release.version == Version::new(version))
        .unwrap()
        .clone()
}

#[tokio::test]
async fn outdated_splits_adoptable_and_in_cooldown() {
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    // `a`: the newest (v1.2.0) is still cooling, but v1.1.0 has matured → adoptable (you can update).
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ), // mature
            rel(
                "v1.2.0",
                2,
                Some("2026-06-16T00:00:00Z"),
                Some(UpdateKind::Minor),
            ), // fresh
        ],
    );
    // `b`: the only newer version is fresh and nothing has matured → in cooldown (cannot update yet).
    releases.insert(
        "b".to_string(),
        vec![
            rel("v2.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v2.1.0",
                1,
                Some("2026-06-16T00:00:00Z"),
                Some(UpdateKind::Minor),
            ), // fresh
        ],
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true), dep("b", "v2.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.outdated(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.items.len(), 2);
    let a = out.items.iter().find(|i| i.name == "a").expect("a");
    let b = out.items.iter().find(|i| i.name == "b").expect("b");
    // `a` has a matured version, so it is adoptable even though its newest is still cooling.
    assert_eq!(a.status, OutdatedStatus::Adoptable);
    assert_eq!(a.adoptable_target.as_deref(), Some("v1.1.0"));
    assert_eq!(a.latest.as_ref().unwrap().version, "v1.2.0");
    assert_eq!(a.candidate_age_days, Some(1.0));
    // `b` has nothing matured, so it genuinely cannot update yet.
    assert_eq!(b.status, OutdatedStatus::InCooldown);
    assert_eq!(b.adoptable_target, None);
    assert_eq!(b.candidate_age_days, Some(1.0));
    assert_eq!(out.summary.adoptable, 1);
    assert_eq!(out.summary.in_cooldown, 1);
}

#[tokio::test]
async fn outdated_countdown_tracks_latest_or_soonest_maturing() {
    // The ruff scenario: locked at 0.15.15 with three newer patches under the default 7-day window
    // (now = 2026-06-17, cutoff 2026-06-10). 0.15.16 has matured (adoptable); 0.15.17 and 0.15.18 are
    // still cooling. 0.15.18 is the freshest (newest), but 0.15.17 unlocks three days sooner.
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "ruff".to_string(),
        vec![
            rel("0.15.15", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "0.15.16",
                1,
                Some("2026-06-05T00:00:00Z"),
                Some(UpdateKind::Patch),
            ), // matured
            rel(
                "0.15.17",
                2,
                Some("2026-06-13T00:00:00Z"),
                Some(UpdateKind::Patch),
            ), // cooling, matures soonest
            rel(
                "0.15.18",
                3,
                Some("2026-06-16T00:00:00Z"),
                Some(UpdateKind::Patch),
            ), // cooling, the newest
        ],
    );
    let make = || {
        workspace(
            fake(
                root.clone(),
                vec![dep("ruff", "0.15.15", true)],
                vec![],
                releases.clone(),
                HashMap::new(),
            ),
            Baseline::default(),
        )
    };

    // Latest horizon (explicit, now that `soonest` is the default): the Cooldown column tracks the
    // freshest version, 0.15.18 (age 1d). It needs no version label because that is exactly what the
    // Latest column already shows.
    let latest_opts = RunOpts {
        cooldown_horizon: CooldownHorizon::Latest,
        ..opts()
    };
    let latest = make().outdated(&latest_opts).await;
    let item = latest
        .items
        .iter()
        .find(|i| i.name == "ruff")
        .expect("ruff");
    assert_eq!(item.status, OutdatedStatus::Adoptable);
    assert_eq!(item.adoptable_target.as_deref(), Some("0.15.16"));
    assert_eq!(item.latest.as_ref().unwrap().version, "0.15.18");
    assert_eq!(item.candidate_age_days, Some(1.0));
    assert_eq!(item.cooldown_version, None);

    // Soonest horizon: the Cooldown column tracks 0.15.17 (age 4d) — the next version to mature —
    // while adoptable/latest are unchanged, because the choice is display-only. Because 0.15.17 is
    // not the latest version, it is labelled so the cell reads `4d/7d (0.15.17)`.
    let soonest_opts = RunOpts {
        cooldown_horizon: CooldownHorizon::Soonest,
        ..opts()
    };
    let soonest = make().outdated(&soonest_opts).await;
    let item = soonest
        .items
        .iter()
        .find(|i| i.name == "ruff")
        .expect("ruff");
    assert_eq!(item.status, OutdatedStatus::Adoptable);
    assert_eq!(item.adoptable_target.as_deref(), Some("0.15.16"));
    assert_eq!(item.latest.as_ref().unwrap().version, "0.15.18");
    assert_eq!(item.candidate_age_days, Some(4.0));
    assert_eq!(item.cooldown_version.as_deref(), Some("0.15.17"));
}

#[tokio::test]
async fn outdated_default_view_never_labels_even_with_an_unclassifiable_newest() {
    // Regression: the default (`latest`) view must never append a `(version)` label. Here the newest
    // eligible release 0.15.18 is unclassifiable (`kind_from_current = None`), so it is `verdict.latest`
    // yet never becomes a candidate; the shown candidate is the next one down, 0.15.17. The label is
    // suppressed by comparing the shown version against the newest *candidate* (not `verdict.latest`),
    // so the cell stays bare — byte-identical to the pre-feature output.
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "ruff".to_string(),
        vec![
            rel("0.15.15", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "0.15.16",
                1,
                Some("2026-06-05T00:00:00Z"),
                Some(UpdateKind::Patch),
            ), // adoptable
            rel(
                "0.15.17",
                2,
                Some("2026-06-13T00:00:00Z"),
                Some(UpdateKind::Patch),
            ), // cooling, the newest *candidate*
            rel("0.15.18", 3, Some("2026-06-16T00:00:00Z"), None), // newest eligible, but unclassifiable → skipped as a candidate
        ],
    );
    let ws = workspace(
        fake(
            root,
            vec![dep("ruff", "0.15.15", true)],
            vec![],
            releases,
            HashMap::new(),
        ),
        Baseline::default(),
    );
    let out = ws.outdated(&opts()).await;
    let item = out.items.iter().find(|i| i.name == "ruff").expect("ruff");
    // `latest` reports the unclassifiable newest; the cooldown tracks the newest candidate, unlabelled.
    assert_eq!(item.latest.as_ref().unwrap().version, "0.15.18");
    assert_eq!(item.candidate_age_days, Some(4.0));
    assert_eq!(
        item.cooldown_version, None,
        "the default view must not label the cooldown version"
    );
}

#[tokio::test]
async fn upgrade_readopts_a_matured_indirect_while_fix_leaves_it() {
    // A fix-downgrade is not a permanent pin: once the newer version of an indirect dep clears the
    // window, `upgrade` moves it forward again, while `fix` (downgrade-only) never does.
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "t".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            // Newer, and now itself matured past the 7-day window (cutoff 2026-06-10).
            rel(
                "v1.0.1",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Patch),
            ),
        ],
    );
    // A direct dep with nothing newer, so only the indirect `t` could move.
    releases.insert(
        "a".to_string(),
        vec![rel("v2.0.0", 0, Some("2026-01-01T00:00:00Z"), None)],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v2.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert(
        "t".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );

    let make = || {
        workspace(
            fake(
                root.clone(),
                vec![dep("a", "v2.0.0", true)],
                vec![dep("t", "v1.0.0", false)],
                releases.clone(),
                locked.clone(),
            ),
            Baseline::default(),
        )
    };

    // `fix` never moves a dep forward — `t@v1.0.0` is already matured, so it is left untouched.
    let fixed = make().fix(&opts()).await;
    assert_eq!(fixed.summary.applied, 0);
    assert!(fixed.items.is_empty());

    // `upgrade` re-adopts the newest matured version of the indirect dep.
    let upgraded = make().upgrade(&opts()).await;
    assert_eq!(upgraded.summary.applied, 1);
    let item = upgraded
        .items
        .iter()
        .find(|item| item.name == "t")
        .expect("t advanced");
    assert_eq!(item.to, "v1.0.1");
}

#[tokio::test]
async fn outdated_transitive_scopes_in_indirect_deps() {
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    // Both a direct and a transitive dep have a matured newer version → both are adoptable.
    for name in ["a", "t"] {
        releases.insert(
            name.to_string(),
            vec![
                rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
                rel(
                    "v1.1.0",
                    1,
                    Some("2026-06-01T00:00:00Z"),
                    Some(UpdateKind::Minor),
                ),
            ],
        );
    }
    let make = || {
        let fake = FakeEco {
            direct: vec![dep("a", "v1.0.0", true)],
            transitive: vec![dep("t", "v1.0.0", false)],
            fresh_transitive: None,
            releases: releases.clone(),
            locked: HashMap::new(),
            inject_fresh_on_apply: false,
            stale_lock: false,
            fail_graph_after_apply: false,
            fail_locked_release_after_apply_for: None,
            stale_lock_after_apply: false,
            build_fails_after_apply: false,
            state: Mutex::new(State::default()),
            root: root.clone(),
        };
        workspace(fake, Baseline::default())
    };

    // Default: direct-only — the transitive dep is not in the report.
    let out = make().outdated(&opts()).await;
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0].name, "a");

    // `--transitive`: the indirect dep is scoped in too.
    let mut transitive = opts();
    transitive.transitive = true;
    let out = make().outdated(&transitive).await;
    assert_eq!(out.items.len(), 2);
    assert!(out.items.iter().any(|item| item.name == "t"));
}

#[tokio::test]
async fn per_tool_exclude_prunes_workspace_member_dependencies() {
    let (_g, root) = tmp_root();
    let mut kept = dep("kept", "v1.0.0", true);
    kept.members = vec![MemberRef {
        name: "kept-app".into(),
        path: "apps/kept".into(),
    }];
    let mut dropped = dep("dropped", "v1.0.0", true);
    dropped.members = vec![MemberRef {
        name: "dropped-app".into(),
        path: "apps/dropped".into(),
    }];
    let mut releases = HashMap::new();
    for name in ["kept", "dropped"] {
        releases.insert(
            name.to_string(),
            vec![
                rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
                rel(
                    "v1.1.0",
                    1,
                    Some("2026-06-01T00:00:00Z"),
                    Some(UpdateKind::Minor),
                ),
            ],
        );
    }
    let fake = FakeEco {
        direct: vec![kept, dropped],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::default(),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let mut opts = opts();
    opts.exclude_folders_by_tool
        .insert(GO.as_str().to_string(), vec!["apps/dropped".to_string()]);

    let out = ws.outdated(&opts).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(
        out.items
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["kept"]
    );
    assert_eq!(out.items[0].members[0].path, "apps/kept");
}

#[tokio::test]
async fn per_tool_exclude_packages_prunes_workspace_member_dependencies() {
    let (_g, root) = tmp_root();
    let mut kept = dep("kept", "v1.0.0", true);
    kept.members = vec![MemberRef {
        name: "@app/kept".into(),
        path: "apps/kept".into(),
    }];
    let mut dropped = dep("dropped", "v1.0.0", true);
    dropped.members = vec![MemberRef {
        name: "@internal/dropped".into(),
        path: "apps/dropped".into(),
    }];
    let mut releases = HashMap::new();
    for name in ["kept", "dropped"] {
        releases.insert(
            name.to_string(),
            vec![
                rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
                rel(
                    "v1.1.0",
                    1,
                    Some("2026-06-01T00:00:00Z"),
                    Some(UpdateKind::Minor),
                ),
            ],
        );
    }
    let fake = FakeEco {
        direct: vec![kept, dropped],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::default(),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let mut opts = opts();
    // `@internal/*` matches the member's package NAME (`@internal/dropped`); it does NOT match the
    // member's path (`apps/dropped`), so this proves exclusion is name-based, not path-based. Keyed
    // by the canonical tool id.
    opts.exclude_packages_by_tool
        .insert(GO.as_str().to_string(), vec!["@internal/*".to_string()]);

    let out = ws.outdated(&opts).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(
        out.items
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["kept"]
    );
    assert_eq!(out.items[0].members[0].name, "@app/kept");
}

#[tokio::test]
async fn global_exclude_packages_prunes_workspace_member_dependencies() {
    // Coverage for the global/command `opts.exclude_packages` branch (set from `[global]`/
    // `[<command>]` or `--exclude-packages`), distinct from the per-tool map: `dependencies_in_scope`
    // seeds its package matcher from `opts.exclude_packages` before extending with the per-tool list.
    let (_g, root) = tmp_root();
    let mut kept = dep("kept", "v1.0.0", true);
    kept.members = vec![MemberRef {
        name: "@app/kept".into(),
        path: "apps/kept".into(),
    }];
    let mut dropped = dep("dropped", "v1.0.0", true);
    dropped.members = vec![MemberRef {
        name: "@internal/dropped".into(),
        path: "apps/dropped".into(),
    }];
    let mut releases = HashMap::new();
    for name in ["kept", "dropped"] {
        releases.insert(
            name.to_string(),
            vec![
                rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
                rel(
                    "v1.1.0",
                    1,
                    Some("2026-06-01T00:00:00Z"),
                    Some(UpdateKind::Minor),
                ),
            ],
        );
    }
    let fake = FakeEco {
        direct: vec![kept, dropped],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::default(),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let mut opts = opts();
    opts.exclude_packages = vec!["@internal/*".to_string()];

    let out = ws.outdated(&opts).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(
        out.items
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["kept"]
    );
}

#[tokio::test]
async fn check_flags_fresh_transitive_and_baseline_acknowledges() {
    let (_g, root) = tmp_root();
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    ); // mature
    locked.insert(
        "t".to_string(),
        rel("v0.5.0", 0, Some("2026-06-16T00:00:00Z"), None),
    ); // fresh → violation

    let make = || FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![dep("t", "v0.5.0", false)],
        fresh_transitive: None,
        releases: HashMap::new(),
        locked: locked.clone(),
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root: root.clone(),
    };

    // Without a baseline → the fresh transitive is a violation, exit 1.
    let ws = workspace(make(), Baseline::default());
    let out = ws.check(&opts()).await;
    assert_eq!(out.exit, Exit::Policy);
    assert_eq!(out.summary.violations, 1);
    assert_eq!(out.summary.checked, 2);
    assert_eq!(out.summary.direct, 1);
    assert_eq!(out.items[0].name, "t");
    assert_eq!(out.items[0].status, CheckStatus::Violation);

    // With an exact-scope baseline entry → acknowledged, exit 0.
    let baseline = Baseline {
        entries: vec![cooldown::app::baseline::AckEntry {
            tool: "go".into(),
            project: ".".into(),
            package: "t".into(),
            version: "v0.5.0".into(),
            registry: Some("proxy.example".into()),
            published_at: None,
            window_days: Some(7.0),
            reason: None,
            until: None,
        }],
    };
    let ws = workspace(make(), baseline);
    let out = ws.check(&opts()).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.violations, 0);
    assert_eq!(out.summary.acknowledged, 1);
}

#[tokio::test]
async fn check_transitive_allow_and_hide_modes() {
    let (_g, root) = tmp_root();
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    ); // direct, mature
    locked.insert(
        "t".to_string(),
        rel("v0.5.0", 0, Some("2026-06-16T00:00:00Z"), None),
    ); // transitive, fresh → would be a violation

    let make = || FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![dep("t", "v0.5.0", false)],
        fresh_transitive: None,
        releases: HashMap::new(),
        locked: locked.clone(),
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root: root.clone(),
    };

    // `--transitive allow`: the fresh transitive is still evaluated and reported, but as a non-fatal
    // `allowed` finding (distinct from a baselined `acknowledged`), so the gate passes.
    let mut allow = opts();
    allow.transitive_mode = cooldown::app::TransitiveGate::Allow;
    let out = workspace(make(), Baseline::default()).check(&allow).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.violations, 0);
    assert_eq!(out.summary.allowed, 1);
    assert_eq!(out.summary.acknowledged, 0);
    assert_eq!(out.summary.checked, 2, "the transitive is still evaluated");
    let allowed_item = out
        .items
        .iter()
        .find(|item| item.name == "t")
        .expect("the fresh transitive is reported");
    assert_eq!(allowed_item.status, CheckStatus::Allowed);

    // `--transitive hide`: the transitive is not evaluated at all (direct-only), gate passes.
    let mut hide = opts();
    hide.transitive_mode = cooldown::app::TransitiveGate::Hide;
    let out = workspace(make(), Baseline::default()).check(&hide).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.violations, 0);
    assert_eq!(out.summary.allowed, 0);
    assert_eq!(out.summary.acknowledged, 0);
    assert_eq!(out.summary.checked, 1, "only the direct dep is evaluated");
}

#[tokio::test]
async fn check_fails_closed_on_stale_lock() {
    let (_g, root) = tmp_root();
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases: HashMap::new(),
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        stale_lock: true,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.check(&opts()).await;
    assert_eq!(out.exit, Exit::Environment);
    assert_eq!(out.errors.len(), 1);
    assert_eq!(out.errors[0].kind, DiagnosticKind::StaleLock);
}

#[tokio::test]
async fn upgrade_applies_clean_change() {
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.upgrade(&opts()).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 1);
    assert!(out.items[0].applied);
    assert_eq!(out.items[0].to, "v1.1.0");
}

/// Releases for package "a": the current v1.0.0 plus a long-matured cross-major v2.0.0. `kind =
/// Major` makes v2.0.0 ineligible under a default (major-off) run yet adoptable under `--major`.
fn a_v1_and_matured_v2() -> Vec<Release> {
    vec![
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
        rel(
            "v2.0.0",
            1,
            Some("2026-01-15T00:00:00Z"),
            Some(UpdateKind::Major),
        ),
    ]
}

/// A fixture for package "a" locked at v1.0.0 with `a_releases`, placed as a direct dep (`direct`)
/// or a transitive one. Mirrors the dogfooding `fs4`/`toml_edit` case where `outdated` shows a
/// cross-major update but a default `upgrade` skips it.
fn major_update_fake(root: camino::Utf8PathBuf, direct: bool, a_releases: Vec<Release>) -> FakeEco {
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), a_releases);
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let (direct, transitive) = if direct {
        (vec![dep("a", "v1.0.0", true)], vec![])
    } else {
        (vec![], vec![dep("a", "v1.0.0", false)])
    };
    FakeEco {
        direct,
        transitive,
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root,
    }
}

#[tokio::test]
async fn upgrade_surfaces_adoptable_major_update_held_back_by_default() {
    let (_g, root) = tmp_root();
    let ws = workspace(
        major_update_fake(root, true, a_v1_and_matured_v2()),
        Baseline::default(),
    );
    let out = ws.upgrade(&opts()).await;
    // Nothing is applied; the held-back cross-major counts as a skip (a `skipped` row whose Result is
    // `needs --major`)…
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.skipped, 1);
    // …recorded as a held-back item the user can act on with `--major`.
    let held: Vec<_> = out
        .items
        .iter()
        .filter(|it| {
            it.skipped
                .as_ref()
                .is_some_and(|s| s.reason == SkipReason::NeedsMajor)
        })
        .collect();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].name, "a");
    assert_eq!(held[0].from, "v1.0.0");
    assert_eq!(held[0].to, "v2.0.0");
}

#[tokio::test]
async fn upgrade_major_adopts_the_update_instead_of_hinting() {
    let (_g, root) = tmp_root();
    let ws = workspace(
        major_update_fake(root, true, a_v1_and_matured_v2()),
        Baseline::default(),
    );
    let out = ws
        .upgrade(&RunOpts {
            allow_major: true,
            ..opts()
        })
        .await;
    // With `--major` the same update is adopted, not held back — so no `needs --major` item.
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.items[0].to, "v2.0.0");
    assert!(
        !out.items.iter().any(|it| {
            it.skipped
                .as_ref()
                .is_some_and(|s| s.reason == SkipReason::NeedsMajor)
        }),
        "no held-back item when --major adopts the update"
    );
}

#[tokio::test]
async fn upgrade_major_crosses_a_direct_but_not_a_transitive() {
    // `--major` rewrites a *direct* dep's manifest constraint across a major boundary, but a
    // transitive dep has no editable constraint and the resolver would reject an independent
    // cross-major bump — so it is capped to its current major. Tool-agnostic: proven on the fake
    // adapter, so it holds for every tool that reports `direct` correctly.
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), a_v1_and_matured_v2());
    releases.insert("t".to_string(), a_v1_and_matured_v2());
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert(
        "t".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );

    let ws = workspace(
        fake(
            root,
            vec![dep("a", "v1.0.0", true)],
            vec![dep("t", "v1.0.0", false)],
            releases,
            locked,
        ),
        Baseline::default(),
    );
    let out = ws
        .upgrade(&RunOpts {
            allow_major: true,
            ..opts()
        })
        .await;

    // The direct dep crosses the major boundary.
    let a = out
        .items
        .iter()
        .find(|it| it.name == "a")
        .expect("a planned");
    assert_eq!(a.to, "v2.0.0");
    assert!(a.applied);
    // The transitive dep is not carried across the major — it produces no item at all.
    assert!(
        !out.items.iter().any(|it| it.name == "t"),
        "a transitive must not be cross-major'd under --major: {:?}",
        out.items
    );
}

#[tokio::test]
async fn upgrade_does_not_hint_a_transitive_major_update() {
    // Only a directly-declared dep can be adopted by `--major` (it rewrites a manifest constraint),
    // so a transitive cross-major must never be hinted — `cooldown upgrade --major -p <transitive>`
    // would do nothing. The dep is in scope (graph) but `dep.direct` is false.
    let (_g, root) = tmp_root();
    let ws = workspace(
        major_update_fake(root, false, a_v1_and_matured_v2()),
        Baseline::default(),
    );
    let out = ws.upgrade(&opts()).await;
    assert!(
        !out.items.iter().any(|it| {
            it.skipped
                .as_ref()
                .is_some_and(|s| s.reason == SkipReason::NeedsMajor)
        }),
        "a transitive major update must not be flagged"
    );
}

#[tokio::test]
async fn upgrade_applies_the_in_range_update_and_still_hints_the_major() {
    // A dep with both a matured in-range minor (v1.1.0) and a matured cross-major (v2.0.0): the
    // default run adopts the minor and still surfaces the major as a separate hint. The hint's `to`
    // is the major (not the just-applied minor) — the `!=` guard keeps them distinct.
    let (_g, root) = tmp_root();
    let releases = vec![
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
        rel(
            "v1.1.0",
            1,
            Some("2026-01-10T00:00:00Z"),
            Some(UpdateKind::Minor),
        ),
        rel(
            "v2.0.0",
            2,
            Some("2026-01-15T00:00:00Z"),
            Some(UpdateKind::Major),
        ),
    ];
    let ws = workspace(major_update_fake(root, true, releases), Baseline::default());
    let out = ws.upgrade(&opts()).await;
    assert_eq!(out.summary.applied, 1);
    assert!(out.items.iter().any(|it| it.applied && it.to == "v1.1.0"));
    // The major is still flagged as `needs --major` (to = the major, not the just-applied minor),
    // and is informational — not counted in the skipped tally.
    let held: Vec<_> = out
        .items
        .iter()
        .filter(|it| {
            it.skipped
                .as_ref()
                .is_some_and(|s| s.reason == SkipReason::NeedsMajor)
        })
        .collect();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].from, "v1.0.0");
    assert_eq!(held[0].to, "v2.0.0");
    // The held-back major counts as a skip (the renderer breaks out the "need --major" subset).
    assert_eq!(out.summary.skipped, 1);
}

#[tokio::test]
async fn fix_downgrades_too_fresh_direct_to_newest_matured() {
    let (_g, root) = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    let ws = workspace(
        fake(
            root,
            vec![dep("a", "v1.0.2", true)],
            Vec::new(),
            releases,
            locked,
        ),
        Baseline::default(),
    );

    let out = ws.fix(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.summary.skipped, 0);
    assert!(out.warnings.is_empty());
    assert_eq!(out.items[0].name, "a");
    assert_eq!(out.items[0].from, "v1.0.2");
    assert_eq!(out.items[0].to, "v1.0.1");
    assert!(out.items[0].applied);
    // The rollback is flagged a downgrade (so the report says "downgraded", not "upgraded").
    assert!(out.items[0].downgrade);
}

#[tokio::test]
async fn fix_warns_and_leaves_exact_pin_unless_opted_in() {
    let (_g, root) = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    let mut pinned = dep("a", "v1.0.2", true);
    pinned.pinned = true;
    let ws = workspace(
        fake(root, vec![pinned], Vec::new(), releases, locked),
        Baseline::default(),
    );

    let out = ws.fix(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert!(out.items.is_empty());
    assert_eq!(out.warnings.len(), 1);
    assert_eq!(out.warnings[0].kind, DiagnosticKind::Held);
    assert!(out.warnings[0].message.contains("--downgrade-pinned"));
}

#[tokio::test]
async fn fix_strict_fails_when_a_violation_is_left_unresolved() {
    let (_g, root) = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    let mut pinned = dep("a", "v1.0.2", true);
    pinned.pinned = true;
    let ws = workspace(
        fake(root, vec![pinned], Vec::new(), releases, locked),
        Baseline::default(),
    );
    let mut opts = opts();
    opts.strict = true;

    let out = ws.fix(&opts).await;

    assert_eq!(out.exit, Exit::Policy);
    assert_eq!(out.summary.applied, 0);
    assert!(out.items.is_empty());
    assert_eq!(out.warnings.len(), 1);
    assert_eq!(out.warnings[0].kind, DiagnosticKind::Held);
}

#[tokio::test]
async fn fix_downgrades_exact_pin_when_opted_in() {
    let (_g, root) = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    let mut pinned = dep("a", "v1.0.2", true);
    pinned.pinned = true;
    let ws = workspace(
        fake(root, vec![pinned], Vec::new(), releases, locked),
        Baseline::default(),
    );
    let mut opts = opts();
    opts.downgrade_pinned = true;

    let out = ws.fix(&opts).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.items[0].to, "v1.0.1");
}

#[tokio::test]
async fn fix_warns_and_leaves_graph_held_violation() {
    let (_g, root) = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("a".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert("a".to_string(), release_named(&package_releases, "v1.0.2"));
    let mut held = dep("a", "v1.0.2", true);
    held.graph_floor = Some(Version::new("v1.0.2"));
    let ws = workspace(
        fake(root, vec![held], Vec::new(), releases, locked),
        Baseline::default(),
    );

    let out = ws.fix(&opts()).await;

    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert!(out.items.is_empty());
    assert_eq!(out.warnings.len(), 1);
    assert_eq!(out.warnings[0].kind, DiagnosticKind::Held);
    assert!(out.warnings[0].message.contains("resolved graph requires"));
}

#[tokio::test]
async fn fix_downgrades_transitive_deps_by_default_with_modes_to_relax() {
    let (_g, root) = tmp_root();
    let package_releases = too_fresh_fix_releases();
    let mut releases = HashMap::new();
    releases.insert("t".to_string(), package_releases.clone());
    let mut locked = HashMap::new();
    locked.insert(
        "b".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert("t".to_string(), release_named(&package_releases, "v1.0.2"));

    // A fresh workspace per case: the fake records applied versions across a `fix` run.
    let make = || {
        workspace(
            fake(
                root.clone(),
                vec![dep("b", "v1.0.0", true)],
                vec![dep("t", "v1.0.2", false)],
                releases.clone(),
                locked.clone(),
            ),
            Baseline::default(),
        )
    };

    // Default (Enforce): the whole resolved graph is fixed, so the too-fresh transitive `t` is
    // downgraded to its newest matured version — no opt-in needed.
    let out = make().fix(&opts()).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.items[0].name, "t");
    assert_eq!(out.items[0].to, "v1.0.1");

    // `--transitive hide`: direct-only, so the transitive is neither evaluated nor touched.
    let mut hide = opts();
    hide.transitive_mode = cooldown::app::TransitiveGate::Hide;
    let out = make().fix(&hide).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert!(out.items.is_empty());

    // `--transitive allow`: the transitive is evaluated and reported, but left in place; only direct
    // deps would be downgraded.
    let mut allow = opts();
    allow.transitive_mode = cooldown::app::TransitiveGate::Allow;
    let out = make().fix(&allow).await;
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 0);
    assert!(
        out.warnings.iter().any(|warning| warning
            .message
            .contains("left in place by --transitive allow")),
        "the allowed transitive is reported"
    );
}

#[tokio::test]
async fn upgrade_rolls_back_when_change_introduces_fresh_transitive() {
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    // The fresh transitive's locked release is younger than the window.
    locked.insert(
        "t".to_string(),
        rel("v0.5.0", 0, Some("2026-06-16T00:00:00Z"), None),
    );

    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: Some(dep("t", "v0.5.0", false)),
        releases,
        locked,
        inject_fresh_on_apply: true, // applying the change drags in `t`
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.upgrade(&opts()).await;

    // The change is skipped (not committed) because it would force in an irreducible too-fresh
    // transitive (`t` has no graph floor below its fresh version, so it can't be rolled back).
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.skipped, 1);
    let sk = out.items[0].skipped.as_ref().expect("a skip");
    assert_eq!(sk.reason, SkipReason::TransitiveInCooldown);
    assert_eq!(sk.offending.as_deref(), Some("t"));
}

#[tokio::test]
async fn upgrade_reconciles_a_floated_up_transitive_instead_of_rolling_back() {
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let t_releases = too_fresh_fix_releases(); // v1.0.1 matured, v1.0.2 too fresh
    releases.insert("t".to_string(), t_releases.clone());

    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert("t".to_string(), release_named(&t_releases, "v1.0.2"));

    // Upgrading `a` floats `t` up to a too-fresh v1.0.2, but the graph still permits a lower version
    // (floor v1.0.0), so the transitive is *reconcilable* rather than forced.
    let mut floated = dep("t", "v1.0.2", false);
    floated.graph_floor = Some(Version::new("v1.0.0"));

    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: Some(floated),
        releases,
        locked,
        inject_fresh_on_apply: true, // applying the `a` upgrade drags in `t`
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root,
    };
    let out = workspace(fake, Baseline::default()).upgrade(&opts()).await;

    // The forward move is kept (not rolled back) and the floated-up transitive is reconciled down to
    // its newest matured version — one `upgrade` leaves a gate-clean lock, no separate `fix` needed.
    assert_eq!(out.exit, Exit::Ok);
    assert_eq!(out.summary.applied, 2);
    let upgraded = out.items.iter().find(|item| item.name == "a").expect("a");
    assert_eq!(upgraded.to, "v1.1.0");
    let reconciled = out.items.iter().find(|item| item.name == "t").expect("t");
    assert_eq!(reconciled.to, "v1.0.1");
}

#[tokio::test]
async fn upgrade_checks_full_graph_even_when_package_filtered() {
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    locked.insert(
        "t".to_string(),
        rel("v0.5.0", 0, Some("2026-06-16T00:00:00Z"), None),
    );

    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: Some(dep("t", "v0.5.0", false)),
        releases,
        locked,
        inject_fresh_on_apply: true,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let mut opts = opts();
    opts.package = vec![PatternGlob::new("a").expect("valid glob")];

    let out = ws.upgrade(&opts).await;

    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.skipped, 1);
    let skipped = out.items[0].skipped.as_ref().expect("skip recorded");
    assert_eq!(skipped.reason, SkipReason::TransitiveInCooldown);
    assert_eq!(skipped.offending.as_deref(), Some("t"));
}

#[tokio::test]
async fn upgrade_fails_closed_when_post_apply_validation_errors() {
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: true,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.upgrade(&opts()).await;

    assert_eq!(out.exit, Exit::Environment);
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.errors, 1);
    assert!(out.items[0].error.is_some());
}

#[tokio::test]
async fn upgrade_fails_closed_when_post_apply_locked_release_errors() {
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: Some("a".into()),
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.upgrade(&opts()).await;

    assert_eq!(out.exit, Exit::Environment);
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.errors, 1);
    assert!(out.items[0].error.is_some());
}

#[tokio::test]
async fn upgrade_reports_final_lock_and_build_failures() {
    let (_g, root) = tmp_root();
    let mut releases = HashMap::new();
    releases.insert(
        "a".to_string(),
        vec![
            rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
            rel(
                "v1.1.0",
                1,
                Some("2026-06-01T00:00:00Z"),
                Some(UpdateKind::Minor),
            ),
        ],
    );
    let mut locked = HashMap::new();
    locked.insert(
        "a".to_string(),
        rel("v1.0.0", 0, Some("2026-01-01T00:00:00Z"), None),
    );
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases,
        locked,
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: true,
        build_fails_after_apply: true,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let mut opts = opts();
    opts.build = true;
    let out = ws.upgrade(&opts).await;

    assert_eq!(out.exit, Exit::Environment);
    assert_eq!(out.summary.applied, 1);
    assert_eq!(out.summary.errors, 2);
    assert!(
        out.errors
            .iter()
            .any(|d| d.kind == DiagnosticKind::StaleLock)
    );
    assert!(
        out.errors
            .iter()
            .any(|d| d.kind == DiagnosticKind::ToolFailed)
    );
}

#[tokio::test]
async fn explain_traces_the_default_window() {
    let (_g, root) = tmp_root();
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)],
        transitive: vec![],
        fresh_transitive: None,
        releases: HashMap::new(),
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root,
    };
    let ws = workspace(fake, Baseline::default());
    let out = ws.explain("a", &opts()).await;
    assert_eq!(out.exit, Exit::Ok);
    assert!((out.meta.effective.min_age_days - 7.0).abs() < 1e-9);
    assert_eq!(out.meta.effective.decided_by, "default");
    assert!(out.steps.iter().any(|s| s.applied && s.field == "default"));
}

/// `explain` resolves the package's registry from the dependency graph, so a `[registry."…"]`
/// rule is applied (it would be silently skipped if explain resolved with no registry).
#[tokio::test]
async fn explain_applies_registry_scoped_rule() {
    let (_g, root) = tmp_root();
    let fake = FakeEco {
        direct: vec![dep("a", "v1.0.0", true)], // dep `a` is published from registry "proxy.example"
        transitive: vec![],
        fresh_transitive: None,
        releases: HashMap::new(),
        locked: HashMap::new(),
        inject_fresh_on_apply: false,
        stale_lock: false,
        fail_graph_after_apply: false,
        fail_locked_release_after_apply_for: None,
        stale_lock_after_apply: false,
        build_fails_after_apply: false,
        state: Mutex::new(State::default()),
        root,
    };

    // A repo layer with a registry-scoped 30d window — above the 7d default.
    let mut repo = PolicyLayer::new(Origin::Repo(Utf8PathBuf::from("cooldown.toml")));
    let mut rule = Rule::new(Selector::Registry("proxy.example".into()));
    rule.window = ByKind::scalar(WindowSpec::MinAge(jiff::SignedDuration::from_hours(
        24 * 30,
    )));
    repo.rules.push(rule);

    let project = fake.project();
    let ctx = ProjectCtx {
        tool: GO,
        project,
        rel_path: Utf8PathBuf::from("."),
        policy: PolicyStack {
            layers: vec![builtin_default_layer(), repo],
            strict_native: false,
        },
    };
    let mut adapters = AdapterSet::new();
    adapters.register(Arc::new(fake));
    let ws = Workspace::new(
        adapters,
        vec![ctx],
        now(),
        Baseline::default(),
        Utf8PathBuf::from("."),
        Vec::new(),
    );

    let out = ws.explain("a", &opts()).await;
    assert_eq!(out.exit, Exit::Ok);
    // The resolved registry is surfaced and the registry rule (30d) beats the 7d default.
    assert_eq!(out.meta.registry.as_deref(), Some("proxy.example"));
    assert!((out.meta.effective.min_age_days - 30.0).abs() < 1e-9);
    assert_eq!(
        out.meta.effective.decided_by,
        "repo:cooldown.toml:registry=proxy.example"
    );
    assert!(
        out.steps
            .iter()
            .any(|s| s.applied && s.selector.as_deref() == Some("registry=proxy.example"))
    );
}

/// A minimal repo-scoped fake tool used to assert `sync`'s `SyncScope::Repo` dispatch: it counts
/// `write_repo_native` calls so a multi-project run can prove the shared file is written exactly
/// once, and tracks whether the value was already written so a second run reports `Unchanged`.
struct RepoScopedFake {
    root: Utf8PathBuf,
    repo_writes: Arc<Mutex<usize>>,
    already_written: Mutex<bool>,
}

const REPO_TOOL: ToolId = ToolId("repotool");

impl RepoScopedFake {
    fn project(&self, rel: &str) -> Project {
        let root = self.root.join(rel);
        Project {
            root: root.clone(),
            kind: REPO_TOOL,
            manifest: root.join("pyproject.toml"),
            exclude_newer: None,
        }
    }
}

#[async_trait]
impl ToolRead for RepoScopedFake {
    fn id(&self) -> ToolId {
        REPO_TOOL
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can_sync: true,
            ..Default::default()
        }
    }
    fn project_marker(&self) -> cooldown_core::ProjectMarker {
        cooldown_core::ProjectMarker {
            lockfile: "repo.lock",
            manifest: "pyproject.toml",
            workspace_root: false,
        }
    }
    async fn dependencies(&self, _p: &Project, _scope: DepScope) -> Result<Vec<Dependency>> {
        Ok(Vec::new())
    }
    async fn native_policy(&self, _p: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }
    async fn verify_lock_current(&self, _p: &Project) -> Result<VerifyReport> {
        Ok(VerifyReport {
            ok: true,
            detail: "ok".into(),
        })
    }
}

#[async_trait]
impl ReleaseFetcher for RepoScopedFake {
    async fn releases(
        &self,
        _dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> Result<Vec<Release>> {
        Ok(Vec::new())
    }
    async fn locked_release(
        &self,
        dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
    ) -> Result<Release> {
        Err(CoreError::NotFound(dep.package.name.clone()))
    }
}

#[async_trait]
impl ToolWrite for RepoScopedFake {
    async fn mutation_journal(&self, _p: &Project, _plan: &Plan) -> Result<ProjectMutationJournal> {
        Ok(ProjectMutationJournal::default())
    }
    async fn apply(
        &self,
        _p: &Project,
        _plan: &Plan,
        _journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        Ok(ApplyReport::default())
    }
    async fn build(&self, _p: &Project) -> Result<VerifyReport> {
        Ok(VerifyReport {
            ok: true,
            detail: "ok".into(),
        })
    }
    fn sync_scope(&self) -> SyncScope {
        SyncScope::Repo
    }
    async fn write_repo_native(
        &self,
        repo_root: &camino::Utf8Path,
        _policy: &ResolvedPolicy,
        _dry_run: bool,
    ) -> Result<SyncReport> {
        *self.repo_writes.lock().unwrap() += 1;
        let path = repo_root.join("uv.toml");
        let mut written = self.already_written.lock().unwrap();
        if *written {
            Ok(SyncReport::Unchanged { path })
        } else {
            *written = true;
            Ok(SyncReport::Written { path })
        }
    }
}

#[tokio::test]
async fn sync_repo_scope_writes_once_for_many_projects_and_is_idempotent() {
    let (_dir, root) = tmp_root();
    let repo_writes = Arc::new(Mutex::new(0usize));
    let fake = RepoScopedFake {
        root: root.clone(),
        repo_writes: Arc::clone(&repo_writes),
        already_written: Mutex::new(false),
    };
    // Two in-scope projects of the same repo-scoped tool must still trigger a single repo write.
    let contexts = ["a", "b"]
        .into_iter()
        .map(|rel| ProjectCtx {
            tool: REPO_TOOL,
            project: fake.project(rel),
            rel_path: Utf8PathBuf::from(rel),
            policy: PolicyStack {
                layers: vec![builtin_default_layer()],
                strict_native: false,
            },
        })
        .collect::<Vec<_>>();
    let mut adapters = AdapterSet::new();
    adapters.register(Arc::new(fake));
    let ws = Workspace::new(
        adapters,
        contexts,
        now(),
        Baseline::default(),
        root.clone(),
        vec![builtin_default_layer()],
    );

    let out = ws.sync(&opts()).await;
    // Exactly one repo write and one item (labelled "." for the repo root), not one per project.
    assert_eq!(*repo_writes.lock().unwrap(), 1);
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0].project, ".");
    assert_eq!(out.items[0].status, cooldown::app::SyncStatus::Written);
    assert_eq!(out.summary.written, 1);
    // The default 7d window renders as the relative span uv re-evaluates each run.
    assert_eq!(out.items[0].window.as_deref(), Some("7d"));

    // A second sync against the now-current repo file reports unchanged, and still writes once more
    // only to compare (the adapter's own idempotence covers the no-op file write).
    let again = ws.sync(&opts()).await;
    assert_eq!(again.items.len(), 1);
    assert_eq!(again.items[0].status, cooldown::app::SyncStatus::Unchanged);
    assert_eq!(again.summary.unchanged, 1);
}

/// A minimal project-scoped fake tool used to assert `sync`'s `SyncScope::Project` dispatch: it
/// counts `write_native` calls so a multi-project run can prove the per-project file is written once
/// per project. Guards the regression where a tool overrides `write_native` but forgets `sync_scope`,
/// which silently defaults to `SyncScope::None` and stops `sync` writing anything.
struct ProjectScopedFake {
    root: Utf8PathBuf,
    native_writes: Arc<Mutex<Vec<String>>>,
}

const PROJECT_TOOL: ToolId = ToolId("projecttool");

impl ProjectScopedFake {
    fn project(&self, rel: &str) -> Project {
        let root = self.root.join(rel);
        Project {
            root: root.clone(),
            kind: PROJECT_TOOL,
            manifest: root.join("pyproject.toml"),
            exclude_newer: None,
        }
    }
}

#[async_trait]
impl ToolRead for ProjectScopedFake {
    fn id(&self) -> ToolId {
        PROJECT_TOOL
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can_sync: true,
            ..Default::default()
        }
    }
    fn project_marker(&self) -> cooldown_core::ProjectMarker {
        cooldown_core::ProjectMarker {
            lockfile: "project.lock",
            manifest: "pyproject.toml",
            workspace_root: false,
        }
    }
    async fn dependencies(&self, _p: &Project, _scope: DepScope) -> Result<Vec<Dependency>> {
        Ok(Vec::new())
    }
    async fn native_policy(&self, _p: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }
    async fn verify_lock_current(&self, _p: &Project) -> Result<VerifyReport> {
        Ok(VerifyReport {
            ok: true,
            detail: "ok".into(),
        })
    }
}

#[async_trait]
impl ReleaseFetcher for ProjectScopedFake {
    async fn releases(
        &self,
        _dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> Result<Vec<Release>> {
        Ok(Vec::new())
    }
    async fn locked_release(
        &self,
        dep: &Dependency,
        _fetch: &cooldown_core::FetchContext<'_>,
    ) -> Result<Release> {
        Err(CoreError::NotFound(dep.package.name.clone()))
    }
}

#[async_trait]
impl ToolWrite for ProjectScopedFake {
    async fn mutation_journal(&self, _p: &Project, _plan: &Plan) -> Result<ProjectMutationJournal> {
        Ok(ProjectMutationJournal::default())
    }
    async fn apply(
        &self,
        _p: &Project,
        _plan: &Plan,
        _journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        Ok(ApplyReport::default())
    }
    async fn build(&self, _p: &Project) -> Result<VerifyReport> {
        Ok(VerifyReport {
            ok: true,
            detail: "ok".into(),
        })
    }
    fn sync_scope(&self) -> SyncScope {
        SyncScope::Project
    }
    async fn write_native(
        &self,
        project: &Project,
        _policy: &ResolvedPolicy,
        _dry_run: bool,
    ) -> Result<SyncReport> {
        let path = project.root.join("project.toml");
        self.native_writes.lock().unwrap().push(path.to_string());
        Ok(SyncReport::Written { path })
    }
}

#[tokio::test]
async fn sync_project_scope_writes_native_per_project() {
    let (_dir, root) = tmp_root();
    let native_writes = Arc::new(Mutex::new(Vec::new()));
    let fake = ProjectScopedFake {
        root: root.clone(),
        native_writes: Arc::clone(&native_writes),
    };
    // Two in-scope projects of the same project-scoped tool must each get a `write_native`, so a tool
    // that overrides `write_native` but forgets `sync_scope` (defaulting to `None`) is caught.
    let contexts = ["a", "b"]
        .into_iter()
        .map(|rel| ProjectCtx {
            tool: PROJECT_TOOL,
            project: fake.project(rel),
            rel_path: Utf8PathBuf::from(rel),
            policy: PolicyStack {
                layers: vec![builtin_default_layer()],
                strict_native: false,
            },
        })
        .collect::<Vec<_>>();
    let mut adapters = AdapterSet::new();
    adapters.register(Arc::new(fake));
    let ws = Workspace::new(
        adapters,
        contexts,
        now(),
        Baseline::default(),
        root.clone(),
        vec![builtin_default_layer()],
    );

    let out = ws.sync(&opts()).await;
    // One `write_native` per project (two), and one written item per project.
    assert_eq!(native_writes.lock().unwrap().len(), 2);
    assert_eq!(out.items.len(), 2);
    assert!(
        out.items
            .iter()
            .all(|item| item.status == cooldown::app::SyncStatus::Written)
    );
    assert_eq!(out.summary.written, 2);
    assert_eq!(
        out.items
            .iter()
            .map(|item| item.project.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
}
