//! File-backed Cargo trials exercise real rollback, remediation, and bounded target fallback.

use crate::app::upgrade::PreviewEvidence;
use crate::app::{
    AdapterSet, Baseline, Exit, Progress, ProjectCtx, ProjectProgress, RunOpts, Workspace,
};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre;
use cooldown_core::config::builtin_default_layer;
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, Change, CoreError, DepScope, Dependency,
    EdgeBindingAction, EdgeRebind, FetchContext, GeneratedMembers, GraphHoldEdge, GraphHoldKind,
    LockStatus, LockVerifyReport, MajorKey, NativePolicyLayer, PackageId, Plan, PolicyStack,
    PreparedMutation, Project, ProjectMarker, ProjectMutationJournal, Release, ReleaseFetcher,
    ReleaseOrder, ReleaseQuality, SkipReason, Skipped, ToolId, ToolRead, ToolWrite, UpdateKind,
    VerifyReport, Version,
};
use indoc::indoc;
use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::sync::Arc;

const CARGO: ToolId = ToolId("cargo");

#[derive(Clone, Copy)]
enum Resolver {
    Caret,
    AnchoredPair,
    PersistentParent {
        companions: u32,
        lower_conflicts: bool,
    },
    ExactFrom(u32),
    RefuseLower {
        blocked_from: u32,
        refused: u32,
    },
    CoupledPatchFrom(u32),
    HeldParent {
        sibling_newest: u32,
    },
}

struct FakeCargo {
    resolver: Resolver,
    releases: BTreeMap<String, Vec<Release>>,
    edge_rebinds_on_apply: Vec<EdgeRebind>,
}

fn timestamp(value: &str) -> jiff::Timestamp {
    value.parse().expect("fixed fixture timestamp")
}

fn release(version: &str, fresh: bool) -> Release {
    let parts: Vec<u32> = version
        .split('.')
        .map(|part| part.parse().expect("semver component"))
        .collect();
    Release {
        version: Version::new(version),
        order: ReleaseOrder(parts.iter().flat_map(|part| part.to_be_bytes()).collect()),
        major: MajorKey(parts.first().expect("major component").to_string()),
        major_number: parts.first().copied().map(u64::from),
        kind_from_current: Some(if parts.get(1) == Some(&0) {
            UpdateKind::Patch
        } else {
            UpdateKind::Minor
        }),
        beyond_declared_bound: false,
        beyond_latest_tag: false,
        published_at: Some(timestamp(if fresh {
            "2026-06-16T00:00:00Z"
        } else {
            "2026-05-01T00:00:00Z"
        })),
        yanked: false,
        quality: ReleaseQuality::Stable,
    }
}

fn lock(project: &Project) -> cooldown_core::Result<BTreeMap<String, String>> {
    serde_json::from_slice(&std::fs::read(project.root.join("Cargo.lock"))?)
        .map_err(|err| CoreError::LockUnreadable(err.to_string()))
}

fn dependency(name: &str, version: &str) -> Dependency {
    Dependency {
        package: PackageId::new(CARGO, name, Some("crates.io".to_string())),
        advisory_identity: Some(name.to_string()),
        current: Version::new(version),
        current_quality: ReleaseQuality::Stable,
        direct: matches!(name, "parent" | "sibling") || name.starts_with("safe"),
        artifacts: Vec::new(),
        graph_floor: None,
        graph_ceiling: None,
        declared_bound: None,
        members: Vec::new(),
        pinned: false,
        hold_edges: Vec::new(),
    }
}

fn hold(name: &str, version: &str, bound: &str, exact: bool) -> GraphHoldEdge {
    GraphHoldEdge {
        requirer: name.to_string(),
        requirer_version: Version::new(version),
        requirement: format!("{}{bound}", if exact { "=" } else { "^" }),
        bound: Version::new(bound),
        kind: if exact {
            GraphHoldKind::Ceiling
        } else {
            GraphHoldKind::Floor
        },
    }
}

impl FakeCargo {
    fn validate_resolve(
        &self,
        versions: &BTreeMap<String, String>,
        plan: &Plan,
    ) -> cooldown_core::Result<()> {
        if matches!(self.resolver, Resolver::AnchoredPair)
            && versions
                .get("sibling")
                .is_some_and(|version| version == "1.4.0")
            && !versions
                .get("parent")
                .is_some_and(|version| version == "1.4.0" || version == "1.3.0")
        {
            return Err(CoreError::UnacceptableResolve(
                "sibling needs a compatible parent".to_string(),
            ));
        }
        if let Resolver::HeldParent { sibling_newest } = self.resolver
            && plan.changes.iter().any(|change| {
                change.package.name == "parent"
                    || (change.package.name == "sibling"
                        && change.to.as_str() == format!("1.{sibling_newest}.0"))
            })
        {
            return Err(CoreError::UnacceptableResolve(
                "parent is held and sibling's newest release cannot resolve".to_string(),
            ));
        }
        if let Resolver::RefuseLower { refused, .. } = self.resolver
            && plan.changes.iter().any(|change| {
                change.package.name == "parent" && change.to.as_str() == format!("1.{refused}.0")
            })
        {
            return Err(CoreError::UnacceptableResolve(
                "lower parent release cannot resolve".to_string(),
            ));
        }
        if matches!(self.resolver, Resolver::CoupledPatchFrom(_))
            && versions.get("parent") != versions.get("sibling")
        {
            return Err(CoreError::UnacceptableResolve(
                "parent and sibling must use the same line".to_string(),
            ));
        }
        if self.exact_child(versions)
            && versions
                .get("child")
                .is_some_and(|version| version != "1.1.0")
        {
            return Err(CoreError::UnacceptableResolve(
                "parent requires child =1.1.0".to_string(),
            ));
        }
        if matches!(self.resolver, Resolver::Caret)
            && versions
                .get("holder")
                .is_some_and(|version| version == "1.1.0")
            && versions
                .get("child")
                .is_some_and(|version| version == "1.0.0")
        {
            return Err(CoreError::UnacceptableResolve(
                "holder requires child ^1.1.0".to_string(),
            ));
        }
        Ok(())
    }

    fn exact_child(&self, versions: &BTreeMap<String, String>) -> bool {
        match self.resolver {
            Resolver::Caret | Resolver::HeldParent { .. } | Resolver::PersistentParent { .. } => {
                false
            }
            Resolver::AnchoredPair => versions
                .get("parent")
                .is_some_and(|version| version == "1.4.0"),
            Resolver::ExactFrom(minor)
            | Resolver::RefuseLower {
                blocked_from: minor,
                ..
            } => versions.get("parent").is_some_and(|version| {
                version
                    .split('.')
                    .nth(1)
                    .and_then(|minor| minor.parse::<u32>().ok())
                    .is_some_and(|current| current >= minor)
            }),
            Resolver::CoupledPatchFrom(patch) => ["parent", "sibling"].iter().any(|name| {
                versions.get(*name).is_some_and(|version| {
                    version
                        .split('.')
                        .nth(2)
                        .and_then(|patch| patch.parse::<u32>().ok())
                        .is_some_and(|current| current >= patch)
                })
            }),
        }
    }
}

#[async_trait]
impl ToolRead for FakeCargo {
    fn id(&self) -> ToolId {
        CARGO
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    fn project_marker(&self) -> ProjectMarker {
        ProjectMarker {
            marker: "Cargo.lock",
            manifest: "Cargo.toml",
            alternate_manifests: &[],
            workspace_root: true,
        }
    }

    async fn dependencies(
        &self,
        project: &Project,
        scope: DepScope,
    ) -> cooldown_core::Result<Vec<Dependency>> {
        let versions = lock(project)?;
        let mut dependencies = Vec::new();
        for (name, version) in &versions {
            let mut dep = dependency(name, version);
            if name == "child" {
                if self.exact_child(&versions) {
                    let parent = versions
                        .get("parent")
                        .ok_or_else(|| CoreError::NotFound("parent".to_string()))?;
                    dep.graph_floor = Some(Version::new("1.1.0"));
                    dep.graph_ceiling = Some(Version::new("1.1.0"));
                    dep.hold_edges = vec![hold("parent", parent, "1.1.0", true)];
                } else if let Some(holder) = versions.get("holder") {
                    let floor = if holder == "1.1.0" { "1.1.0" } else { "1.0.0" };
                    dep.graph_floor = Some(Version::new(floor));
                    dep.hold_edges = vec![hold("holder", holder, floor, false)];
                }
            }
            if scope == DepScope::Graph || dep.direct {
                dependencies.push(dep);
            }
        }
        Ok(dependencies)
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
            detail: "consistent fake Cargo resolve".to_string(),
        })
    }
}

#[async_trait]
impl ReleaseFetcher for FakeCargo {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: CandidateScope,
    ) -> cooldown_core::Result<Vec<Release>> {
        Ok(self
            .releases
            .get(&dep.package.name)
            .cloned()
            .unwrap_or_default())
    }

    async fn locked_release(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
    ) -> cooldown_core::Result<Release> {
        self.releases
            .get(&dep.package.name)
            .and_then(|releases| {
                releases
                    .iter()
                    .find(|release| release.version == dep.current)
            })
            .cloned()
            .ok_or_else(|| CoreError::NotFound(format!("{}@{}", dep.package.name, dep.current)))
    }
}

#[async_trait]
impl ToolWrite for FakeCargo {
    fn mutation_tool(&self) -> ToolId {
        CARGO
    }
    fn supports_transitive_advance(&self) -> bool {
        true
    }

    fn supports_target_fallback(&self) -> bool {
        true
    }

    async fn lock_edge_snapshot(
        &self,
        project: &Project,
    ) -> cooldown_core::Result<Option<Vec<u8>>> {
        if self.edge_rebinds_on_apply.is_empty() {
            Ok(None)
        } else {
            Ok(Some(std::fs::read(project.root.join("Cargo.lock"))?))
        }
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> cooldown_core::Result<ProjectMutationJournal> {
        ProjectMutationJournal::capture(&project.root, [Utf8Path::new("Cargo.lock")])
    }

    async fn apply(&self, mutation: &PreparedMutation) -> cooldown_core::Result<ApplyReport> {
        let (project, plan, _) = mutation.parts_for(self)?;
        if !self.edge_rebinds_on_apply.is_empty() {
            assert!(plan.initial_lock_snapshot.is_some());
        }
        let mut versions = lock(project)?;
        let baseline = versions.get("parent").cloned().unwrap_or_default();
        let mut record = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(project.root.join("trials"))?;
        writeln!(
            record,
            "{baseline}:{}",
            plan.changes
                .iter()
                .map(|change| format!("{}@{}", change.package.name, change.to))
                .collect::<Vec<_>>()
                .join(",")
        )?;
        let mut skipped = Vec::new();
        let mut applied = Vec::new();
        if let Resolver::PersistentParent {
            lower_conflicts, ..
        } = self.resolver
        {
            let parent = plan
                .changes
                .iter()
                .find(|change| change.package.name == "parent");
            if lower_conflicts
                && parent.is_some_and(|change| change.to.as_str() != "1.4.0")
                && plan
                    .changes
                    .iter()
                    .any(|change| change.package.name.starts_with("safe"))
            {
                return Err(CoreError::UnacceptableResolve(
                    "lower parent conflicts with safe companions".to_string(),
                ));
            }
            if let Some(parent) =
                parent.filter(|change| !lower_conflicts || change.to.as_str() == "1.4.0")
            {
                skipped.push(Skipped {
                    change: parent.clone(),
                    reason: SkipReason::ResolverConflict,
                    offending: None,
                    detail: Some("parent is held by requiring holder =1.0.0".to_string()),
                });
            }
        }
        for change in &plan.changes {
            if skipped
                .iter()
                .any(|item| item.change.package == change.package)
            {
                continue;
            }
            applied.push(change.clone());
            versions.insert(change.package.name.clone(), change.to.to_string());
            if change.package.name == "parent" {
                match self.resolver {
                    Resolver::Caret => {
                        versions.insert("holder".to_string(), "1.1.0".to_string());
                        versions.insert("child".to_string(), "1.1.0".to_string());
                    }
                    Resolver::AnchoredPair
                    | Resolver::ExactFrom(_)
                    | Resolver::RefuseLower { .. }
                    | Resolver::CoupledPatchFrom(_)
                    | Resolver::HeldParent { .. }
                    | Resolver::PersistentParent { .. } => {
                        let child = if self.exact_child(&versions) {
                            "1.1.0"
                        } else {
                            "1.0.0"
                        };
                        versions.insert("child".to_string(), child.to_string());
                    }
                }
            }
        }
        self.validate_resolve(&versions, plan)?;
        std::fs::write(
            project.root.join("Cargo.lock"),
            serde_json::to_vec(&versions).map_err(|err| CoreError::System(err.to_string()))?,
        )?;
        Ok(ApplyReport {
            applied,
            skipped,
            edge_rebinds: self.edge_rebinds_on_apply.clone(),
            ..Default::default()
        })
    }

    async fn build(&self, _project: &Project) -> cooldown_core::Result<VerifyReport> {
        Ok(VerifyReport {
            ok: true,
            detail: String::new(),
        })
    }
}

struct CargoFixture {
    directory: tempfile::TempDir,
    workspace: Workspace,
    project: Project,
}

fn fixture(resolver: Resolver, newest: u32, sibling: bool) -> eyre::Result<CargoFixture> {
    fixture_with_edges(resolver, newest, sibling, Vec::new())
}

fn fixture_adapter(
    resolver: Resolver,
    newest: u32,
    edge_rebinds_on_apply: Vec<EdgeRebind>,
) -> FakeCargo {
    let mut parents = vec![release("0.9.0", false)];
    parents.extend((0..=newest).map(|component| {
        let version = if matches!(resolver, Resolver::CoupledPatchFrom(_)) {
            format!("49.0.{component}")
        } else {
            format!("1.{component}.0")
        };
        release(&version, false)
    }));
    let siblings = if matches!(resolver, Resolver::AnchoredPair) {
        vec![release("1.0.0", false), release("1.4.0", false)]
    } else if matches!(
        resolver,
        Resolver::CoupledPatchFrom(_) | Resolver::HeldParent { .. }
    ) {
        parents.clone()
    } else {
        vec![release("1.0.0", false), release("1.1.0", false)]
    };
    let mut fake = FakeCargo {
        resolver,
        edge_rebinds_on_apply,
        releases: BTreeMap::from([
            ("parent".to_string(), parents),
            (
                "child".to_string(),
                vec![release("1.0.0", false), release("1.1.0", true)],
            ),
            (
                "holder".to_string(),
                vec![release("1.0.0", false), release("1.1.0", true)],
            ),
            ("sibling".to_string(), siblings),
        ]),
    };
    if let Resolver::PersistentParent { companions, .. } = resolver {
        for index in 0..companions {
            fake.releases.insert(
                format!("safe{index:03}"),
                vec![release("1.0.0", false), release("1.4.0", false)],
            );
        }
    }
    fake
}

fn fixture_with_edges(
    resolver: Resolver,
    newest: u32,
    sibling: bool,
    edge_rebinds_on_apply: Vec<EdgeRebind>,
) -> eyre::Result<CargoFixture> {
    let directory = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
        .map_err(|path| eyre::eyre!("non-UTF-8 root: {}", path.display()))?;
    std::fs::write(
        root.join("Cargo.toml"),
        indoc! {r#"
            [package]
            name = "app"
            version = "0.1.0"
            [dependencies]
            parent = "1"
        "#},
    )?;
    let project = Project {
        root: root.clone(),
        kind: CARGO,
        manifest: root.join("Cargo.toml"),
        exclude_newer: None,
        generated_members: GeneratedMembers::undeclared(),
    };
    let initial_direct = if matches!(resolver, Resolver::CoupledPatchFrom(_)) {
        "49.0.0"
    } else {
        "1.0.0"
    };
    let mut initial = BTreeMap::from([("parent", initial_direct), ("child", "1.0.0")]);
    if matches!(resolver, Resolver::Caret) {
        initial.insert("holder", "1.0.0");
    }
    if sibling {
        initial.insert("sibling", initial_direct);
    }
    let mut initial: BTreeMap<String, String> = initial
        .into_iter()
        .map(|(name, version)| (name.to_string(), version.to_string()))
        .collect();
    if let Resolver::PersistentParent { companions, .. } = resolver {
        for index in 0..companions {
            initial.insert(format!("safe{index:03}"), initial_direct.to_string());
        }
    }
    std::fs::write(root.join("Cargo.lock"), serde_json::to_vec(&initial)?)?;
    let fake = fixture_adapter(resolver, newest, edge_rebinds_on_apply);
    let mut adapters = AdapterSet::new();
    adapters.register_target_verified_mutator(Arc::new(fake))?;
    let context = ProjectCtx {
        tool: CARGO,
        project: project.clone(),
        rel_path: Utf8PathBuf::from("."),
        policy: PolicyStack {
            layers: vec![builtin_default_layer()],
            strict_native: false,
        },
        edge_policy: cooldown_core::EdgePolicy::default(),
        single_copy: Vec::new(),
        generated_members: None,
    };
    let workspace = Workspace::new(
        adapters,
        vec![context],
        timestamp("2026-06-17T00:00:00Z"),
        Baseline::default(),
        root,
        Vec::new(),
    );
    Ok(CargoFixture {
        directory,
        workspace,
        project,
    })
}

#[tokio::test]
async fn preview_keeps_per_batch_edge_rows_with_a_duplicate_guard_baseline() -> eyre::Result<()> {
    let rebind = EdgeRebind {
        dependent: "parent".to_string(),
        dependent_version: Version::new("1.1.0"),
        dependent_source: Some("crates.io".to_string()),
        dependency: PackageId::new(CARGO, "child", Some("crates.io".to_string())),
        from: Version::new("1.1.0"),
        to: Version::new("1.0.0"),
        action: EdgeBindingAction::Restored,
        detail: None,
    };
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture_with_edges(Resolver::ExactFrom(2), 1, false, vec![rebind.clone()])?;
    let before = std::fs::read(project.root.join("Cargo.lock"))?;
    let context = workspace.projects().first().expect("fixture project");
    let preview = workspace
        .preview_project_upgrade(
            context,
            &RunOpts::default(),
            &ProjectProgress::default(),
            vec![Change {
                package: PackageId::new(CARGO, "parent", Some("crates.io".to_string())),
                from: Version::new("1.0.0"),
                to: Version::new("1.1.0"),
                kind: UpdateKind::Minor,
                downgrade: false,
                direct: true,
                members: Vec::new(),
            }],
            PreviewEvidence {
                manifest_only: HashSet::default(),
                advisories: None,
                excluded_members: Vec::new(),
            },
        )
        .await;
    assert!(preview.errors.is_empty(), "{:?}", preview.errors);
    assert_eq!(preview.items.len(), 1);
    assert!(preview.items.first().expect("preview upgrade").applied);
    assert_eq!(preview.edge_items.len(), 1);
    let edge = preview.edge_items.first().expect("preview edge row");
    assert_eq!(edge.name, rebind.dependency.name);
    assert_eq!(edge.from, rebind.from.as_str());
    assert_eq!(edge.to, rebind.to.as_str());
    assert_eq!(
        edge.edge.as_ref().expect("edge provenance").action,
        rebind.action
    );
    assert_eq!(std::fs::read(project.root.join("Cargo.lock"))?, before);
    Ok(())
}

#[tokio::test]
async fn newest_parent_remediates_caret_children_before_accepting() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(Resolver::Caret, 4, false)?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.4.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.0.0"));
    assert_eq!(versions.get("holder").map(String::as_str), Some("1.0.0"));
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(
        trials
            .lines()
            .any(|line| line.contains("child@1.0.0") && line.contains("holder@1.0.0")),
        "{trials}"
    );
    assert!(outcome.items.iter().all(|item| item.security.is_none()));
    assert_clean(&workspace).await;
    Ok(())
}

#[tokio::test]
async fn next_older_parent_rolls_back_fresh_children_and_keeps_companion_batch() -> eyre::Result<()>
{
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(Resolver::ExactFrom(4), 4, true)?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.3.0"));
    assert_eq!(versions.get("sibling").map(String::as_str), Some("1.1.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.0.0"));
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(
        trials.lines().any(|line| line.starts_with("1.0.0:")
            && line.contains("parent@1.3.0")
            && line.contains("sibling@1.1.0")),
        "{trials}"
    );
    let parent = outcome
        .items
        .iter()
        .find(|item| item.name == "parent" && item.applied)
        .expect("landed parent row");
    assert_eq!(parent.to, "1.3.0");
    assert!(outcome.items.iter().all(|item| item.security.is_none()));
    assert_clean(&workspace).await;
    Ok(())
}

#[tokio::test]
async fn fallback_tries_only_three_lower_targets_in_the_selected_major() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(Resolver::ExactFrom(3), 6, false)?;
    let before = std::fs::read(project.root.join("Cargo.lock"))?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(std::fs::read(project.root.join("Cargo.lock"))?, before);
    assert_eq!(
        lock(&project)?.get("parent").map(String::as_str),
        Some("1.0.0")
    );
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    for version in ["1.6.0", "1.5.0", "1.4.0", "1.3.0"] {
        assert!(trials.contains(&format!("parent@{version}")), "{trials}");
    }
    assert_eq!(
        trials
            .lines()
            .filter(|line| line.contains("parent@"))
            .count(),
        4,
        "{trials}"
    );
    assert!(!trials.contains("parent@1.2.0"), "{trials}");
    assert!(!trials.contains("parent@0.9.0"), "{trials}");
    assert!(
        trials
            .lines()
            .filter(|line| line.contains("parent@"))
            .all(|line| line.starts_with("1.0.0:")),
        "{trials}"
    );
    assert!(
        outcome
            .items
            .iter()
            .all(|item| !item.applied && item.security.is_none())
    );
    Ok(())
}

/// Both sides of a coupled pair must fall back before the resolver can accept the batch.
#[tokio::test]
async fn coupled_pair_can_land_both_older_patch_targets() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(Resolver::CoupledPatchFrom(2), 2, true)?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("49.0.1"));
    assert_eq!(versions.get("sibling").map(String::as_str), Some("49.0.1"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.0.0"));
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(
        trials.lines().any(|line| line.starts_with("49.0.0:")
            && line.contains("parent@49.0.1")
            && line.contains("sibling@49.0.1")),
        "{trials}"
    );
    for name in ["parent", "sibling"] {
        let landed = outcome
            .items
            .iter()
            .find(|item| item.name == name && item.applied)
            .expect("landed pair row");
        assert_eq!(landed.to, "49.0.1");
    }
    assert!(outcome.items.iter().all(|item| item.security.is_none()));
    assert_clean(&workspace).await;
    Ok(())
}

/// A resolver refusal at the first lower target does not exhaust the policy fallback search.
#[tokio::test]
async fn resolver_refused_lower_target_does_not_prevent_the_next_lower_target() -> eyre::Result<()>
{
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::RefuseLower {
            blocked_from: 4,
            refused: 3,
        },
        4,
        false,
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.2.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.0.0"));
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(trials.contains("1.0.0:parent@1.3.0"), "{trials}");
    assert!(trials.contains("1.0.0:parent@1.2.0"), "{trials}");
    assert!(outcome.items.iter().all(|item| item.security.is_none()));
    assert_clean(&workspace).await;
    Ok(())
}

async fn assert_clean(workspace: &Workspace) {
    let check = workspace.check(&RunOpts::default()).await;
    assert_eq!(check.exit, Exit::Ok, "{:?}", check.errors);
    assert!(check.summary.checked >= 2);
    assert_eq!(check.summary.violations, 0);
    assert_eq!(check.summary.errors, 0);
    assert!(check.items.is_empty(), "{:?}", check.items);
}

/// A held newest resolve enters fallback even when it introduces no policy violation.
#[tokio::test]
async fn initially_refused_newest_parent_tries_an_older_target() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::RefuseLower {
            blocked_from: 5,
            refused: 4,
        },
        4,
        false,
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.3.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.0.0"));
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(trials.contains("1.0.0:parent@1.4.0"), "{trials}");
    assert!(trials.contains("1.0.0:parent@1.3.0"), "{trials}");
    let landed = outcome
        .items
        .iter()
        .find(|item| item.name == "parent" && item.applied)
        .expect("landed parent row");
    assert_eq!(landed.to, "1.3.0");
    assert!(outcome.items.iter().all(|item| item.security.is_none()));
    assert_clean(&workspace).await;
    Ok(())
}

/// Failed parent fallbacks retain the highest sibling target that settled safely.
#[tokio::test]
async fn exhausted_parent_fallbacks_replay_the_best_safe_sibling_subset() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(Resolver::HeldParent { sibling_newest: 4 }, 4, true)?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    assert_eq!(versions.get("sibling").map(String::as_str), Some("1.3.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.0.0"));
    assert!(
        outcome
            .items
            .iter()
            .filter(|item| item.name == "parent")
            .all(|item| !item.applied)
    );
    let landed = outcome
        .items
        .iter()
        .find(|item| item.name == "sibling" && item.applied)
        .expect("landed sibling row");
    assert_eq!(landed.from, "1.0.0");
    assert_eq!(landed.to, "1.3.0");
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(
        trials
            .lines()
            .any(|line| line.contains("parent@1.4.0") && line.contains("sibling@1.4.0")),
        "{trials}"
    );
    // One attempt discovers the safe subset; another installs it after all trials restore.
    assert!(
        trials
            .lines()
            .filter(|line| line.contains("sibling@1.3.0"))
            .count()
            >= 2,
        "{trials}"
    );
    assert!(outcome.items.iter().all(|item| item.security.is_none()));
    assert_clean(&workspace).await;
    Ok(())
}

/// A lower refused candidate cannot replace either of the settled safe companions.
#[tokio::test]
async fn lower_target_preserves_the_entire_settled_subset() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::PersistentParent {
            companions: 2,
            lower_conflicts: true,
        },
        4,
        false,
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    for name in ["safe000", "safe001"] {
        assert_eq!(versions.get(name).map(String::as_str), Some("1.4.0"));
        assert!(
            outcome
                .items
                .iter()
                .any(|item| item.name == name && item.applied)
        );
    }
    assert!(
        outcome
            .items
            .iter()
            .filter(|item| item.name == "parent")
            .all(|item| !item.applied)
    );
    Ok(())
}

/// A single rejected candidate has three fallback attempts regardless of batch size.
#[tokio::test]
async fn persistent_blocker_in_two_hundred_candidates_has_bounded_fallback() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::PersistentParent {
            companions: 199,
            lower_conflicts: false,
        },
        4,
        false,
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    assert_eq!(
        versions
            .iter()
            .filter(|(name, version)| name.starts_with("safe") && version.as_str() == "1.4.0")
            .count(),
        199
    );
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    // Initial resolver isolation and final replay are separate from the fallback budget.
    let fallback: Vec<_> = trials
        .lines()
        .filter(|line| {
            ["parent@1.3.0", "parent@1.2.0", "parent@1.1.0"]
                .iter()
                .any(|target| line.contains(target))
        })
        .collect();
    assert_eq!(fallback.len(), 3, "{trials}");
    assert!(trials.lines().count() <= 6, "{trials}");
    Ok(())
}

/// A rejected exact companion keeps its original target when it has no lower alternative.
#[tokio::test]
async fn joint_fallback_keeps_a_companion_without_lower_alternatives() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(Resolver::AnchoredPair, 4, true)?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.3.0"));
    assert_eq!(versions.get("sibling").map(String::as_str), Some("1.4.0"));
    for name in ["parent", "sibling"] {
        assert!(
            outcome
                .items
                .iter()
                .filter(|item| item.name == name)
                .all(|item| item.applied),
            "{:?}",
            outcome.items
        );
    }
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(
        trials
            .lines()
            .any(|line| line.contains("parent@1.3.0") && line.contains("sibling@1.4.0")),
        "{trials}"
    );
    Ok(())
}

/// Exhausting lower targets keeps the requiring package from Cargo's original held explanation.
#[tokio::test]
async fn exhausted_fallback_preserves_native_held_detail() -> eyre::Result<()> {
    for newest in [1, 4] {
        let CargoFixture {
            directory: _directory,
            workspace,
            project,
        } = fixture(
            Resolver::PersistentParent {
                companions: 1,
                lower_conflicts: false,
            },
            newest,
            false,
        )?;
        let outcome = workspace.upgrade(&RunOpts::default()).await;
        assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
        let parent = outcome
            .items
            .iter()
            .find(|item| item.name == "parent")
            .ok_or_else(|| eyre::eyre!("missing held parent"))?;
        assert!(!parent.applied);
        assert_eq!(
            parent
                .skipped
                .as_ref()
                .map(|skipped| skipped.message.as_str()),
            Some("parent is held by requiring holder =1.0.0")
        );
        assert_eq!(
            lock(&project)?.get("safe000").map(String::as_str),
            Some("1.4.0")
        );
    }
    Ok(())
}

#[tokio::test]
async fn exhausted_resolver_fallback_is_incomplete_in_strict_mode() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::PersistentParent {
            companions: 1,
            lower_conflicts: false,
        },
        4,
        false,
    )?;
    let outcome = workspace
        .upgrade(&RunOpts {
            strict: true,
            ..Default::default()
        })
        .await;
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.exit, Exit::Policy);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    assert_eq!(versions.get("safe000").map(String::as_str), Some("1.4.0"));
    let parent = outcome
        .items
        .iter()
        .find(|item| item.name == "parent")
        .expect("held parent row");
    assert!(!parent.applied);
    assert_eq!(
        parent.skipped.as_ref().expect("resolver hold").reason,
        SkipReason::ResolverConflict
    );
    Ok(())
}

#[tokio::test]
async fn original_candidates_are_decided_after_landing_fallback_or_hold() -> eyre::Result<()> {
    for (resolver, sibling, landed_parent) in [
        (Resolver::ExactFrom(5), true, Some("1.4.0")),
        (Resolver::ExactFrom(4), true, Some("1.3.0")),
        (
            Resolver::RefuseLower {
                blocked_from: 5,
                refused: 4,
            },
            true,
            Some("1.3.0"),
        ),
        (
            Resolver::PersistentParent {
                companions: 1,
                lower_conflicts: false,
            },
            false,
            None,
        ),
    ] {
        let CargoFixture {
            directory: _directory,
            workspace,
            project: _,
        } = fixture(resolver, 4, sibling)?;
        let companion = if sibling { "sibling" } else { "safe000" };
        let changes = [
            ("parent", "1.4.0"),
            (companion, if sibling { "1.1.0" } else { "1.4.0" }),
        ]
        .into_iter()
        .map(|(name, target)| Change {
            package: PackageId::new(CARGO, name, Some("crates.io".to_string())),
            from: Version::new("1.0.0"),
            to: Version::new(target),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        })
        .collect();
        let progress = Progress::plain().project(CARGO, ".");
        let preview = workspace
            .preview_project_upgrade(
                workspace.projects().first().expect("fixture project"),
                &RunOpts::default(),
                &progress,
                changes,
                PreviewEvidence {
                    manifest_only: HashSet::default(),
                    advisories: None,
                    excluded_members: Vec::new(),
                },
            )
            .await;
        assert!(preview.errors.is_empty(), "{:?}", preview.errors);
        let parent = preview
            .items
            .iter()
            .find(|item| item.name == "parent")
            .expect("parent decision");
        assert_eq!(parent.applied, landed_parent.is_some());
        if let Some(target) = landed_parent {
            assert_eq!(parent.to, target);
        }
        assert!(
            preview
                .items
                .iter()
                .any(|item| item.name == companion && item.applied)
        );
        assert_eq!(progress.decided_candidates(), 2, "{:?}", preview.items);
    }
    Ok(())
}
