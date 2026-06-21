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
                out.push(ft.clone());
            }
        }
        Ok(out)
    }
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
    Workspace::new(adapters, vec![ctx], now(), baseline)
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
        out.warnings
            .iter()
            .any(|warning| warning.message.contains("left in place by --transitive allow")),
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

    // The change is skipped (not committed) because it would introduce a too-fresh transitive.
    assert_eq!(out.summary.applied, 0);
    assert_eq!(out.summary.skipped, 1);
    let sk = out.items[0].skipped.as_ref().expect("a skip");
    assert_eq!(sk.reason, SkipReason::TransitiveInCooldown);
    assert_eq!(sk.offending.as_deref(), Some("t"));
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
    let ws = Workspace::new(adapters, vec![ctx], now(), Baseline::default());

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
