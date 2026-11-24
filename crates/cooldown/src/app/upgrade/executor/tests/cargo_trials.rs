//! File-backed Cargo trials exercise real rollback, remediation, and bounded target fallback.

use crate::app::upgrade::UpgradeOutcome;
use crate::app::{
    AdapterSet, Baseline, Exit, OutdatedStatus, Progress, ProjectCtx, RunOpts, UpgradeItem,
    Workspace,
};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre;
use cooldown_core::config::builtin_default_layer;
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, Change, CoreError, DepScope, Dependency,
    EdgeBindingAction, EdgeRebind, FetchContext, GeneratedMembers, GraphHoldEdge, GraphHoldKind,
    LockStatus, LockVerifyReport, MajorKey, NativePolicyLayer, Origin, PackageId, PatternGlob,
    Plan, PolicyLayer, PolicyStack, PreparedMutation, Project, ProjectMarker,
    ProjectMutationJournal, Release, ReleaseFetcher, ReleaseOrder, ReleaseQuality, Rule, Selector,
    SkipReason, Skipped, TargetFallback, ToolId, ToolRead, ToolTermination, ToolWrite, UpdateKind,
    VerifyReport, Version,
};
use indoc::indoc;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::Arc;

const CARGO: ToolId = ToolId("cargo");

#[derive(Clone, Copy)]
enum Resolver {
    Caret,
    AnchoredPair,
    SnapshotFamily {
        pinned_child: bool,
        fresh_sibling: bool,
        /// The parent is undeclared, so it moves only as collateral of the sibling that pins it.
        undeclared_parent: bool,
    },
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
    /// Holds `sibling` at every target, and `parent` too unless `coupled` and the plan leaves
    /// `sibling` out, attesting each hold that does not depend on the plan when `attests`.
    PersistentPair {
        coupled: bool,
        attests: bool,
    },
    /// Offers `parent` only on its next major, while the transitive `child` it requires has a
    /// matured release on its own line.
    ///
    /// Unless `lands`, the major stays held; when it `lands`, the resolver refuses to move `child`.
    /// When `pinned`, `parent` 1.0.0 pins `child` exactly, while its major only requires `child`'s
    /// line.
    MajorParent {
        lands: bool,
        pinned: bool,
    },
    /// Resolves the transitive `child` on its next major exactly while `parent` sits on a minor in
    /// `lowest..=highest`: on the fresh `2.1.0` when `fresh_major`, else on the matured `2.0.0`;
    /// `child` is capped at major 1.
    ///
    /// The lock starts with `parent` on its `fresh_newest` release when set, and `child` on
    /// `child_start`.
    MajorChild {
        lowest: u32,
        highest: u32,
        fresh_major: bool,
        fresh_newest: bool,
        child_start: &'static str,
    },
    /// Holds each `name@target` in `held`, as a resolver conflict, and lands every other target.
    Scripted {
        held: &'static [&'static str],
    },
    /// Declares `child` beside `parent`, while the undeclared `holder`, which has no successor,
    /// pins `child` exactly, as `wasm-bindgen-backend` pins `wasm-bindgen-shared` in #165.
    ///
    /// The `parent` 1.1.0 release needs `child` 1.1.0 and drops `holder` from the graph.
    /// The manifest pins `child` exactly too when `pinned_child`.
    DroppedPinner {
        pinned_child: bool,
    },
}

/// How a trial carrying a scripted target fails instead of resolving.
#[derive(Clone, Copy)]
enum Failure {
    /// Cargo prints a resolver conflict and then aborts, as in #130.
    Crash,
    /// The apply reports recovery evidence it still owns, leaving the lock as it found it.
    PendingRecovery,
    /// The apply rewrites the lock and then reports that the project changed under it.
    Drift,
}

struct FakeCargo {
    resolver: Resolver,
    releases: BTreeMap<String, Vec<Release>>,
    edge_rebinds_on_apply: Vec<EdgeRebind>,
    /// The `name@target`s whose trials fail, and how.
    failures: Vec<(&'static str, Failure)>,
    /// The packages whose releases the registry cannot serve.
    unavailable_releases: &'static [&'static str],
    /// The `name@version` whose locking makes every later scoped graph read fail.
    unreadable_with: Option<&'static str>,
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

/// A repository config layer capping `child` at major 1.
fn child_max_major_layer(root: &Utf8Path) -> eyre::Result<PolicyLayer> {
    let mut layer = PolicyLayer::new(Origin::Repo(root.join("cooldown.toml")));
    let mut rule = Rule::new(Selector::Package {
        glob: PatternGlob::new("child")?,
        tool: Some(CARGO),
    });
    rule.max_major = Some(1);
    layer.rules.push(rule);
    Ok(layer)
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
        if matches!(self.resolver, Resolver::SnapshotFamily { .. })
            && (versions.get("parent") != versions.get("child")
                || versions.get("parent") != versions.get("sibling"))
        {
            return Err(CoreError::UnacceptableResolve(
                "the snapshot family must resolve together".to_string(),
            ));
        }
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
        if matches!(self.resolver, Resolver::DroppedPinner { .. }) {
            if versions.contains_key("holder")
                && versions
                    .get("child")
                    .is_some_and(|version| version != "1.0.0")
            {
                return Err(CoreError::UnacceptableResolve(
                    "holder requires child =1.0.0".to_string(),
                ));
            }
            if versions
                .get("parent")
                .is_some_and(|version| version == "1.1.0")
                && versions
                    .get("child")
                    .is_some_and(|version| version != "1.1.0")
            {
                return Err(CoreError::UnacceptableResolve(
                    "parent 1.1.0 requires child ^1.1.0".to_string(),
                ));
            }
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

    /// The holds an attesting resolver reports as independent of the rest of its plan.
    fn independent_holds(&self, skipped: &[Skipped]) -> Vec<Change> {
        // A coupled parent's hold depends on the planned sibling, so only the sibling's is
        // attested.
        match self.resolver {
            Resolver::PersistentPair {
                attests: true,
                coupled,
            } => skipped
                .iter()
                .filter(|item| !coupled || item.change.package.name == "sibling")
                .map(|item| item.change.clone())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The changes a [`Resolver::PersistentPair`] or [`Resolver::MajorParent`] holds.
    fn held_pair(&self, plan: &Plan) -> Vec<Skipped> {
        let sibling_planned = plan
            .changes
            .iter()
            .any(|change| change.package.name == "sibling");
        let held: &[&str] = match self.resolver {
            Resolver::PersistentPair { coupled, .. } if coupled && !sibling_planned => &["sibling"],
            Resolver::PersistentPair { .. } => &["parent", "sibling"],
            Resolver::MajorParent { lands: false, .. } => &["parent"],
            Resolver::MajorParent { lands: true, .. } => &["child"],
            Resolver::Scripted { held } => {
                return plan
                    .changes
                    .iter()
                    .filter(|change| {
                        held.contains(&format!("{}@{}", change.package.name, change.to).as_str())
                    })
                    .map(|change| Skipped {
                        change: change.clone(),
                        reason: SkipReason::ResolverConflict,
                        offending: None,
                        detail: Some(format!("{} is held by holder", change.package.name)),
                    })
                    .collect();
            }
            _ => return Vec::new(),
        };
        plan.changes
            .iter()
            .filter(|change| held.contains(&change.package.name.as_str()))
            .map(|change| Skipped {
                change: change.clone(),
                reason: SkipReason::ResolverConflict,
                offending: None,
                detail: Some(format!("{} is held by holder", change.package.name)),
            })
            .collect()
    }

    /// The `child` the resolve settles on beside the locked `parent`.
    fn resolved_child(&self, versions: &BTreeMap<String, String>) -> &'static str {
        if let Resolver::MajorChild {
            lowest,
            highest,
            fresh_major,
            ..
        } = self.resolver
        {
            let minor = versions
                .get("parent")
                .and_then(|version| version.split('.').nth(1))
                .and_then(|minor| minor.parse::<u32>().ok())
                .unwrap_or_default();
            if !(lowest..=highest).contains(&minor) {
                "1.0.0"
            } else if fresh_major {
                "2.1.0"
            } else {
                "2.0.0"
            }
        } else if self.exact_child(versions) {
            "1.1.0"
        } else {
            "1.0.0"
        }
    }

    /// Fails a trial planning a target scripted to fail, the way [`Failure`] describes, over
    /// the lock's `versions`.
    fn fail_as_scripted(
        &self,
        project: &Project,
        plan: &Plan,
        versions: &BTreeMap<String, String>,
    ) -> cooldown_core::Result<()> {
        let Some(failure) = plan.changes.iter().find_map(|change| {
            let target = format!("{}@{}", change.package.name, change.to);
            self.failures
                .iter()
                .find(|(failing, _)| *failing == target)
                .map(|(_, failure)| *failure)
        }) else {
            return Ok(());
        };
        Err(match failure {
            Failure::Crash => CoreError::Tool {
                tool: "cargo".to_string(),
                termination: ToolTermination::Signal(6),
                stderr: "error: failed to select a version for `child`.\n\
                         corrupted double-linked list"
                    .to_string(),
            },
            Failure::PendingRecovery => {
                CoreError::PendingRecovery("cargo left recovery evidence".to_string())
            }
            Failure::Drift => {
                let mut drifted = versions.clone();
                drifted.insert("drift".to_string(), "1.0.0".to_string());
                std::fs::write(
                    project.root.join("Cargo.lock"),
                    serde_json::to_vec(&drifted)
                        .map_err(|err| CoreError::System(err.to_string()))?,
                )?;
                CoreError::LockConflict("Cargo.lock changed during the apply".to_string())
            }
        })
    }

    fn exact_child(&self, versions: &BTreeMap<String, String>) -> bool {
        match self.resolver {
            Resolver::Caret
            | Resolver::HeldParent { .. }
            | Resolver::PersistentParent { .. }
            | Resolver::PersistentPair { .. }
            | Resolver::MajorParent { .. }
            | Resolver::MajorChild { .. }
            | Resolver::Scripted { .. }
            | Resolver::SnapshotFamily { .. }
            | Resolver::DroppedPinner { .. } => false,
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
            if let Resolver::SnapshotFamily {
                pinned_child,
                undeclared_parent,
                ..
            } = self.resolver
            {
                if name == "child" {
                    dep.direct = true;
                    dep.pinned = pinned_child;
                }
                if name == "parent" {
                    dep.direct = !undeclared_parent;
                }
                let requirer = match name.as_str() {
                    "child" => Some("parent"),
                    "parent" => Some("sibling"),
                    _ => None,
                };
                if let Some(requirer) = requirer {
                    let requiring_version = versions
                        .get(requirer)
                        .ok_or_else(|| CoreError::NotFound(requirer.to_string()))?;
                    dep.graph_ceiling = Some(Version::new(version));
                    dep.hold_edges = vec![hold(requirer, requiring_version, version, true)];
                }
            }
            if name == "child"
                && let Resolver::MajorParent { pinned, .. } = self.resolver
            {
                let parent = versions
                    .get("parent")
                    .ok_or_else(|| CoreError::NotFound("parent".to_string()))?;
                let exact = pinned && parent == "1.0.0";
                dep.graph_ceiling = exact.then(|| Version::new(version));
                dep.hold_edges = vec![hold("parent", parent, "1.0.0", exact)];
            } else if name == "child"
                && let Resolver::DroppedPinner { pinned_child } = self.resolver
            {
                dep.direct = true;
                dep.pinned = pinned_child;
                if let Some(holder) = versions.get("holder") {
                    // Like Cargo's adapter, an authored pin masks the ceiling but keeps the edge.
                    dep.graph_ceiling = (!pinned_child).then(|| Version::new(version));
                    dep.hold_edges = vec![hold("holder", holder, version, true)];
                }
            } else if name == "child" {
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

    // Only a scoped read asks for the manifest's notices, so failing them fails the follower
    // replan's read and the final one, never the graph reads that verify a batch.
    async fn manifest_notices(
        &self,
        project: &Project,
    ) -> cooldown_core::Result<Vec<cooldown_core::Diagnostic>> {
        let locked = lock(project)?;
        let unreadable = self.unreadable_with.is_some_and(|unreadable| {
            locked
                .iter()
                .any(|(name, version)| format!("{name}@{version}") == unreadable)
        });
        if unreadable {
            return Err(CoreError::LockUnreadable(
                "the manifest cannot be read".to_string(),
            ));
        }
        Ok(Vec::new())
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
        if self
            .unavailable_releases
            .contains(&dep.package.name.as_str())
        {
            return Err(CoreError::Transient(
                format!("the registry cannot serve {}", dep.package.name).into(),
            ));
        }
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

    fn target_fallback(&self) -> TargetFallback {
        TargetFallback::LowerOnSelectedLine
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
        log_batch(&project.root.join("trials"), &baseline, plan)?;
        // Which search each batch asked for, apart from `trials` so their expectations stay put.
        let search = if plan.fast { "fast" } else { "exhaustive" };
        log_batch(&project.root.join("searches"), search, plan)?;
        self.fail_as_scripted(project, plan, &versions)?;
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
        skipped.extend(self.held_pair(plan));
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
                    | Resolver::PersistentPair { .. }
                    | Resolver::MajorParent { .. }
                    | Resolver::MajorChild { .. }
                    | Resolver::Scripted { .. }
                    | Resolver::PersistentParent { .. } => {
                        let child = self.resolved_child(&versions);
                        versions.insert("child".to_string(), child.to_string());
                    }
                    Resolver::SnapshotFamily { .. } => {}
                    Resolver::DroppedPinner { .. } => {
                        versions.remove("holder");
                    }
                }
            }
        }
        // Cargo moves an undeclared parent along with the sibling that pins it exactly.
        if let Resolver::SnapshotFamily {
            undeclared_parent: true,
            ..
        } = self.resolver
            && let Some(sibling) = versions.get("sibling").cloned()
        {
            versions.insert("parent".to_string(), sibling);
        }
        self.validate_resolve(&versions, plan)?;
        std::fs::write(
            project.root.join("Cargo.lock"),
            serde_json::to_vec(&versions).map_err(|err| CoreError::System(err.to_string()))?,
        )?;
        Ok(ApplyReport {
            applied,
            independent_holds: self.independent_holds(&skipped),
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

/// Appends `label` and the batch's targets as one line to the log at `path`.
fn log_batch(path: &Utf8Path, label: &str, plan: &Plan) -> cooldown_core::Result<()> {
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        log,
        "{label}:{}",
        plan.changes
            .iter()
            .map(|change| format!("{}@{}", change.package.name, change.to))
            .collect::<Vec<_>>()
            .join(",")
    )?;
    Ok(())
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
        Resolver::CoupledPatchFrom(_)
            | Resolver::HeldParent { .. }
            | Resolver::PersistentPair { .. }
            | Resolver::Scripted { .. }
    ) {
        parents.clone()
    } else {
        vec![release("1.0.0", false), release("1.1.0", false)]
    };
    let mut fake = FakeCargo {
        resolver,
        edge_rebinds_on_apply,
        failures: Vec::new(),
        unavailable_releases: &[],
        unreadable_with: None,
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
    if let Resolver::SnapshotFamily { fresh_sibling, .. } = resolver {
        // The next family is visible but has not matured under the fixture's fixed clock.
        for name in ["parent", "child", "sibling"] {
            fake.releases.insert(
                name.to_string(),
                vec![
                    release("1.0.0", false),
                    release("1.1.0", fresh_sibling && name == "sibling"),
                    release("1.2.0", true),
                ],
            );
        }
    }
    if matches!(resolver, Resolver::DroppedPinner { .. }) {
        fake.releases.insert(
            "child".to_string(),
            vec![release("1.0.0", false), release("1.1.0", false)],
        );
        fake.releases
            .insert("holder".to_string(), vec![release("1.0.0", false)]);
    }
    if matches!(resolver, Resolver::MajorParent { .. }) {
        fake.releases.insert(
            "parent".to_string(),
            vec![release("1.0.0", false), release("2.0.0", false)],
        );
        fake.releases.insert(
            "child".to_string(),
            vec![release("1.0.0", false), release("1.1.0", false)],
        );
    }
    if let Resolver::MajorChild { fresh_newest, .. } = resolver {
        // Only the newest parent is fresh, so `fix` matures it down to the next older minor.
        fake.releases.insert(
            "parent".to_string(),
            (0..=newest)
                .map(|minor| release(&format!("1.{minor}.0"), fresh_newest && minor == newest))
                .collect(),
        );
        fake.releases.insert(
            "child".to_string(),
            vec![
                release("1.0.0", false),
                release("1.1.0", true),
                release("2.0.0", false),
                release("2.1.0", true),
            ],
        );
    }
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

/// The fixture's starting lock for `resolver`.
fn initial_lock(resolver: Resolver, newest: u32, sibling: bool) -> BTreeMap<String, String> {
    let initial_direct = if matches!(resolver, Resolver::CoupledPatchFrom(_)) {
        "49.0.0"
    } else {
        "1.0.0"
    };
    let newest_parent = format!("1.{newest}.0");
    let (initial_parent, initial_child) = match resolver {
        Resolver::MajorChild {
            fresh_newest,
            child_start,
            ..
        } => (
            if fresh_newest {
                newest_parent.as_str()
            } else {
                initial_direct
            },
            child_start,
        ),
        _ => (initial_direct, "1.0.0"),
    };
    let mut initial = BTreeMap::from([("parent", initial_parent), ("child", initial_child)]);
    if matches!(resolver, Resolver::Caret | Resolver::DroppedPinner { .. }) {
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
    initial
}

fn fixture_with_edges(
    resolver: Resolver,
    newest: u32,
    sibling: bool,
    edge_rebinds_on_apply: Vec<EdgeRebind>,
) -> eyre::Result<CargoFixture> {
    fixture_with(
        resolver,
        newest,
        sibling,
        edge_rebinds_on_apply,
        Overrides::default(),
    )
}

/// What a fixture changes in its default fake Cargo, registry, and policy.
#[derive(Default)]
struct Overrides {
    /// The `name@target`s whose trials fail, and how.
    trials: Vec<(&'static str, Failure)>,
    /// The packages whose releases the registry cannot serve.
    unavailable: &'static [&'static str],
    /// The `name@version` whose locking makes every later scoped graph read fail.
    unreadable_with: Option<&'static str>,
    /// Releases replacing the registry's for their packages.
    releases: BTreeMap<String, Vec<Release>>,
    /// Policy layers after the built-in default.
    layers: Vec<PolicyLayer>,
}

impl From<Vec<(&'static str, Failure)>> for Overrides {
    fn from(trials: Vec<(&'static str, Failure)>) -> Self {
        Overrides {
            trials,
            ..Overrides::default()
        }
    }
}

fn fixture_with(
    resolver: Resolver,
    newest: u32,
    sibling: bool,
    edge_rebinds_on_apply: Vec<EdgeRebind>,
    overrides: Overrides,
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
    if let Resolver::SnapshotFamily {
        pinned_child,
        undeclared_parent,
        ..
    } = resolver
    {
        std::fs::write(
            root.join("Cargo.toml"),
            indoc::formatdoc! {r#"
            [package]
            name = "app"
            version = "0.1.0"
            [dependencies]
            {}sibling = "1.0.0"
            child = "{}1.0.0"
        "#,
            if undeclared_parent { "" } else { "parent = \"1.0.0\"\n" },
            if pinned_child { "=" } else { "" }},
        )?;
    }
    let project = Project {
        root: root.clone(),
        kind: CARGO,
        manifest: root.join("Cargo.toml"),
        exclude_newer: None,
        exclude_newer_package: std::collections::BTreeMap::new(),
        generated_members: GeneratedMembers::undeclared(),
    };
    let initial = initial_lock(resolver, newest, sibling);
    std::fs::write(root.join("Cargo.lock"), serde_json::to_vec(&initial)?)?;
    let mut fake = FakeCargo {
        failures: overrides.trials,
        unavailable_releases: overrides.unavailable,
        unreadable_with: overrides.unreadable_with,
        ..fixture_adapter(resolver, newest, edge_rebinds_on_apply)
    };
    fake.releases.extend(overrides.releases);
    let mut adapters = AdapterSet::new();
    adapters.register_target_verified_mutator(Arc::new(fake))?;
    let mut layers = vec![builtin_default_layer()];
    if matches!(resolver, Resolver::MajorChild { .. }) {
        layers.push(child_max_major_layer(&root)?);
    }
    layers.extend(overrides.layers);
    let context = ProjectCtx {
        tool: CARGO,
        project: project.clone(),
        rel_path: Utf8PathBuf::from("."),
        policy: PolicyStack {
            layers,
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

/// A dry run hands every batch the run's starting lock as its duplicate-guard baseline (the fake
/// asserts it) and leaves the lock as it was.
/// With a run-level edge snapshot, the final normalization rather than a batch reports edge rows,
/// and this one finds none to report.
#[tokio::test]
async fn a_dry_run_gives_every_batch_the_starting_lock() -> eyre::Result<()> {
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
    } = fixture_with_edges(Resolver::ExactFrom(2), 1, false, vec![rebind])?;
    let before = std::fs::read(project.root.join("Cargo.lock"))?;
    let outcome = workspace
        .upgrade(&RunOpts {
            dry_run: true,
            ..RunOpts::default()
        })
        .await;
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(rows(&outcome), [row("parent", "1.0.0", "1.1.0", None)]);
    assert_eq!(std::fs::read(project.root.join("Cargo.lock"))?, before);
    Ok(())
}

#[tokio::test]
async fn coordinated_snapshot_family_advances_only_to_the_matured_release() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::SnapshotFamily {
            pinned_child: false,
            fresh_sibling: false,
            undeclared_parent: false,
        },
        2,
        true,
    )?;
    let manifest = std::fs::read(project.manifest.as_std_path())?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(outcome.items.iter().filter(|item| item.applied).count(), 3);
    assert!(lock(&project)?.values().all(|version| version == "1.1.0"));
    assert_eq!(std::fs::read(project.manifest.as_std_path())?, manifest);
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(
        trials.lines().any(|line| line.contains("parent@1.1.0")
            && line.contains("child@1.1.0")
            && line.contains("sibling@1.1.0")),
        "{trials}"
    );
    assert!(!trials.contains("@1.2.0"), "{trials}");
    assert_clean(&workspace).await;

    let settled = std::fs::read(project.root.join("Cargo.lock"))?;
    let second = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(second.exit, Exit::Ok, "{:?}", second.errors);
    assert!(second.items.iter().all(|item| !item.applied));
    assert_eq!(std::fs::read(project.root.join("Cargo.lock"))?, settled);
    Ok(())
}

#[test]
fn upgrade_trials_fit_a_small_thread_stack() -> eyre::Result<()> {
    // Windows debug frames need more stack than Unix ones; both budgets are below test defaults.
    // The stricter Unix budget reproduced the regression before trial futures were heap-pinned.
    let stack_bytes = if cfg!(windows) {
        1024 * 1024
    } else {
        256 * 1024
    };
    std::thread::Builder::new()
        .stack_size(stack_bytes)
        .spawn(|| -> eyre::Result<()> {
            let CargoFixture {
                directory: _directory,
                workspace,
                project,
            } = fixture(
                Resolver::SnapshotFamily {
                    pinned_child: false,
                    fresh_sibling: false,
                    undeclared_parent: false,
                },
                2,
                true,
            )?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let outcome = runtime.block_on(workspace.upgrade(&RunOpts::default()));
            assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
            assert_eq!(outcome.items.iter().filter(|item| item.applied).count(), 3);
            assert!(lock(&project)?.values().all(|version| version == "1.1.0"));
            Ok(())
        })?
        .join()
        .map_err(|_| eyre::eyre!("small-stack upgrade thread panicked"))?
}

/// A declared child whose exact requirer is undeclared, and so never planned itself, still moves
/// with the family when that requirer is pinned in turn by a moving sibling: the requirer follows
/// the sibling as collateral, so it does not keep the child's ceiling.
#[tokio::test]
async fn coordinated_snapshot_family_moves_through_an_undeclared_pinning_parent() -> eyre::Result<()>
{
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::SnapshotFamily {
            pinned_child: false,
            fresh_sibling: false,
            undeclared_parent: true,
        },
        2,
        true,
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    // The declared child and sibling are planned and land together; the parent follows.
    assert!(lock(&project)?.values().all(|version| version == "1.1.0"));
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(
        trials
            .lines()
            .any(|line| line.contains("child@1.1.0") && line.contains("sibling@1.1.0")),
        "{trials}"
    );
    assert!(!trials.contains("parent@"), "{trials}");
    Ok(())
}

#[tokio::test]
async fn coordinated_snapshot_family_preserves_an_authored_exact_pin() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::SnapshotFamily {
            pinned_child: true,
            fresh_sibling: false,
            undeclared_parent: false,
        },
        2,
        true,
    )?;
    let before = std::fs::read(project.root.join("Cargo.lock"))?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert!(outcome.items.iter().all(|item| !item.applied));
    assert_eq!(std::fs::read(project.root.join("Cargo.lock"))?, before);
    Ok(())
}

#[tokio::test]
async fn coordinated_snapshot_family_keeps_unselected_requirers_authoritative() -> eyre::Result<()>
{
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::SnapshotFamily {
            pinned_child: false,
            fresh_sibling: false,
            undeclared_parent: false,
        },
        2,
        true,
    )?;
    let before = std::fs::read(project.root.join("Cargo.lock"))?;
    let outcome = workspace
        .upgrade(&RunOpts {
            package: vec![
                cooldown_core::PatternGlob::new("parent")?,
                cooldown_core::PatternGlob::new("child")?,
            ],
            ..RunOpts::default()
        })
        .await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(std::fs::read(project.root.join("Cargo.lock"))?, before);
    // The unselected sibling still pins the parent, so the declared pair is tried and refused.
    assert_eq!(
        rows(&outcome),
        [
            row(
                "child",
                "1.0.0",
                "1.1.0",
                Some(SkipReason::ResolverConflict)
            ),
            row(
                "parent",
                "1.0.0",
                "1.1.0",
                Some(SkipReason::ResolverConflict)
            ),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn coordinated_snapshot_family_keeps_an_immature_requirer_authoritative() -> eyre::Result<()>
{
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::SnapshotFamily {
            pinned_child: false,
            fresh_sibling: true,
            undeclared_parent: false,
        },
        2,
        true,
    )?;
    let before = std::fs::read(project.root.join("Cargo.lock"))?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(std::fs::read(project.root.join("Cargo.lock"))?, before);
    // The immature sibling stays and still pins the parent, so the declared pair is refused.
    assert_eq!(
        rows(&outcome),
        [
            row(
                "child",
                "1.0.0",
                "1.1.0",
                Some(SkipReason::ResolverConflict)
            ),
            row(
                "parent",
                "1.0.0",
                "1.1.0",
                Some(SkipReason::ResolverConflict)
            ),
        ]
    );
    Ok(())
}

/// A declared crate a third-party requirer pins exactly moves beside a declared driver whose target
/// drops that requirer, as `wasm-bindgen` 0.2.105 carries `wasm-bindgen-shared` past the pin of
/// the `wasm-bindgen-backend` it drops (#165).
#[tokio::test]
async fn a_declared_crate_moves_with_a_driver_that_drops_its_pinning_requirer() -> eyre::Result<()>
{
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::DroppedPinner {
            pinned_child: false,
        },
        1,
        false,
    )?;
    let strict = RunOpts {
        strict: true,
        ..RunOpts::default()
    };

    // Before the upgrade, `outdated` already offers both targets the upgrade lands.
    let outdated = workspace.outdated(&RunOpts::default()).await;
    let statuses: Vec<_> = outdated
        .items
        .iter()
        .map(|item| (item.name.as_str(), item.status))
        .collect();
    assert_eq!(
        statuses,
        [
            ("child", OutdatedStatus::Adoptable),
            ("parent", OutdatedStatus::Adoptable),
        ]
    );

    // Both are planned into one trial, land together, and leave `--strict` satisfied.
    let outcome = workspace.upgrade(&strict).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(
        rows(&outcome),
        [
            row("child", "1.0.0", "1.1.0", None),
            row("parent", "1.0.0", "1.1.0", None),
        ]
    );
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(
        trials.contains("1.0.0:child@1.1.0,parent@1.1.0"),
        "{trials}"
    );
    let versions = lock(&project)?;
    assert_eq!(versions.get("child").map(String::as_str), Some("1.1.0"));
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.1.0"));
    assert!(!versions.contains_key("holder"));
    Ok(())
}

/// A declared crate a surviving requirer still pins exactly is tried, and its refusal is reported
/// with the resolver's reason, alike by `upgrade`, `outdated` and `explain`.
///
/// The row counts as an incomplete move, so `--strict` fails while a default run does not.
#[tokio::test]
async fn a_declared_crate_a_surviving_requirer_pins_is_refused_and_reported() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::DroppedPinner {
            pinned_child: false,
        },
        0,
        false,
    )?;
    let before = std::fs::read(project.root.join("Cargo.lock"))?;
    let reason = "holder requires child =1.0.0";

    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(
        rows(&outcome),
        [row(
            "child",
            "1.0.0",
            "1.1.0",
            Some(SkipReason::ResolverConflict)
        )]
    );
    let child = item(&outcome, "child")?;
    assert_eq!(
        child
            .skipped
            .as_ref()
            .map(|skipped| skipped.message.as_str()),
        Some(reason)
    );
    assert_eq!(std::fs::read(project.root.join("Cargo.lock"))?, before);

    let strict = workspace
        .upgrade(&RunOpts {
            strict: true,
            ..RunOpts::default()
        })
        .await;
    assert!(strict.errors.is_empty(), "{:?}", strict.errors);
    assert_eq!(strict.exit, Exit::Policy);
    assert_eq!(std::fs::read(project.root.join("Cargo.lock"))?, before);

    // The previews of `outdated` and `explain` hold the target for the same reason.
    let outdated = workspace.outdated(&RunOpts::default()).await;
    let explained = workspace.explain("child", &RunOpts::default()).await?;
    for verdicts in [&outdated.items, &explained.meta.verdicts] {
        let child = verdicts
            .iter()
            .find(|item| item.name == "child")
            .ok_or_else(|| eyre::eyre!("missing child verdict"))?;
        assert_eq!(child.status, OutdatedStatus::Blocked);
        assert_eq!(child.adoptable_target.as_deref(), Some("1.1.0"));
        assert_eq!(child.blocked_reason.as_deref(), Some(reason));
    }
    assert_eq!(std::fs::read(project.root.join("Cargo.lock"))?, before);
    Ok(())
}

/// The workspace author's own exact pin still holds a declared crate a third-party requirer pins
/// too: it is never tried, and its `pin_held` row leaves `--strict` satisfied.
#[tokio::test]
async fn an_authored_exact_pin_holds_a_declared_crate_without_a_trial() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(Resolver::DroppedPinner { pinned_child: true }, 0, false)?;
    let before = std::fs::read(project.root.join("Cargo.lock"))?;
    let outcome = workspace
        .upgrade(&RunOpts {
            strict: true,
            ..RunOpts::default()
        })
        .await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(
        rows(&outcome),
        [row("child", "1.0.0", "1.1.0", Some(SkipReason::PinHeld))]
    );
    assert!(!project.root.join("trials").exists());
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

/// Two candidates the resolver holds at every target each get their lower targets tried jointly
/// once; with nothing landing jointly and both holds attested, retrying each alone would only
/// repeat those trials.
#[tokio::test]
async fn persistent_pair_skips_individual_retries_after_futile_joint_fallbacks() -> eyre::Result<()>
{
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::PersistentPair {
            coupled: false,
            attests: true,
        },
        4,
        true,
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    // Neither candidate moves.
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    assert_eq!(versions.get("sibling").map(String::as_str), Some("1.0.0"));
    // The initial trial and three joint fallbacks, and no individual fallback.
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    let fallback: Vec<_> = trials
        .lines()
        .filter(|line| !line.contains("parent@1.4.0") || !line.contains("sibling@1.4.0"))
        .collect();
    assert_eq!(fallback.len(), 3, "{trials}");
    assert!(
        fallback
            .iter()
            .all(|line| line.contains("parent@") && line.contains("sibling@")),
        "{trials}"
    );
    Ok(())
}

/// Without the writer's attestation, nothing proves a retry alone would be refused too, so each
/// candidate still gets its individual retries.
#[tokio::test]
async fn unattested_holds_keep_every_individual_retry() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::PersistentPair {
            coupled: false,
            attests: false,
        },
        4,
        true,
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    // Three joint retries, then three alone for each of the two candidates.
    let fallback = trials
        .lines()
        .filter(|line| !line.contains("parent@1.4.0") || !line.contains("sibling@1.4.0"))
        .count();
    assert_eq!(fallback, 9, "{trials}");
    Ok(())
}

/// A hold that depends on another planned candidate is not attested, so the candidate's retry
/// alone still runs and lands, while the attested sibling is not retried alone.
#[tokio::test]
async fn a_hold_coupled_to_a_planned_sibling_is_retried_alone() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::PersistentPair {
            coupled: true,
            attests: true,
        },
        4,
        true,
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.3.0"));
    assert_eq!(versions.get("sibling").map(String::as_str), Some("1.0.0"));
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(
        trials
            .lines()
            .any(|line| line.contains("parent@1.3.0") && !line.contains("sibling@")),
        "{trials}"
    );
    assert!(
        !trials
            .lines()
            .any(|line| line.contains("sibling@1.3.0") && !line.contains("parent@")),
        "{trials}"
    );
    Ok(())
}

/// A transitive the parent's next major would carry off its line is not planned beside that
/// parent, and once the parent stays held it gets its own in-line upgrade.
#[tokio::test]
async fn a_child_deferred_for_a_held_major_parent_still_upgrades() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::MajorParent {
            lands: false,
            pinned: false,
        },
        1,
        false,
    )?;
    let outcome = workspace
        .upgrade(&RunOpts {
            allow_major: true,
            ..RunOpts::default()
        })
        .await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.1.0"));
    // The parent's trial plans the parent alone, and the child follows in a trial of its own.
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    let lines: Vec<_> = trials.lines().collect();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("parent@2.0.0") && !line.contains("child@")),
        "{trials}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("child@1.1.0") && !line.contains("parent@")),
        "{trials}"
    );
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

/// One report row reduced to what the fallback decides: the move, whether it landed, and the
/// reason it was held.
type Row = (String, String, String, bool, Option<SkipReason>);

fn rows(outcome: &UpgradeOutcome) -> Vec<Row> {
    let mut rows: Vec<Row> = outcome
        .items
        .iter()
        .map(|item| {
            (
                item.name.clone(),
                item.from.clone(),
                item.to.clone(),
                item.applied,
                item.skipped.as_ref().map(|skipped| skipped.reason),
            )
        })
        .collect();
    rows.sort_by(|left, right| (&left.0, &left.2).cmp(&(&right.0, &right.2)));
    rows
}

fn row(name: &str, from: &str, to: &str, held: Option<SkipReason>) -> Row {
    (
        name.to_string(),
        from.to_string(),
        to.to_string(),
        held.is_none(),
        held,
    )
}

/// The trials that carried `target`.
fn trials_with(trials: &str, target: &str) -> usize {
    trials
        .lines()
        .filter(|line| line.split([':', ',']).any(|entry| entry == target))
        .count()
}

/// The crash a scripted [`Failure::Crash`] reports.
const CRASH: &str = "corrupted double-linked list";

/// Asserts that the failure naming `needle` is reported once, as a project error naming no
/// package, and that no row carries an error.
fn assert_project_error(errors: &[cooldown_core::Diagnostic], items: &[UpgradeItem], needle: &str) {
    let reported: Vec<_> = errors
        .iter()
        .filter(|error| error.message.contains(needle))
        .collect();
    assert_eq!(reported.len(), 1, "{errors:?}");
    assert!(
        reported.iter().all(|error| error.package.is_none()),
        "{errors:?}"
    );
    assert!(items.iter().all(|item| item.error.is_none()), "{items:?}");
}

/// Crashes every trial carrying one of `targets`.
fn crashes(targets: &[&'static str]) -> Overrides {
    targets
        .iter()
        .map(|target| (*target, Failure::Crash))
        .collect::<Vec<_>>()
        .into()
}

/// Every lower-line trial of the held parent fails, as Cargo's crash did in #130: the failure
/// is reported once for the project, every bounded attempt still runs, and the verified sibling
/// is replayed and lands beside the parent's original hold.
#[tokio::test]
async fn failed_fallback_trials_are_reported_and_keep_the_verified_set() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture_with(
        Resolver::Scripted {
            held: &["parent@1.4.0"],
        },
        4,
        true,
        Vec::new(),
        crashes(&["parent@1.3.0", "parent@1.2.0", "parent@1.1.0"]),
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_project_error(&outcome.errors, &outcome.items, CRASH);
    assert_ne!(outcome.exit, Exit::Ok);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    assert_eq!(versions.get("sibling").map(String::as_str), Some("1.4.0"));
    assert_eq!(
        rows(&outcome),
        [
            row(
                "parent",
                "1.0.0",
                "1.4.0",
                Some(SkipReason::ResolverConflict)
            ),
            row("sibling", "1.0.0", "1.4.0", None),
        ]
    );
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    for target in ["parent@1.3.0", "parent@1.2.0", "parent@1.1.0"] {
        assert_eq!(trials_with(&trials, target), 1, "{trials}");
    }
    assert_eq!(
        trials.lines().last(),
        Some("1.0.0:sibling@1.4.0"),
        "{trials}"
    );
    Ok(())
}

/// A failed fallback trial affects only its own alternative: the parent's next lower target is
/// still tried and lands beside the verified sibling, while the failure is reported.
#[tokio::test]
async fn a_failed_fallback_trial_rejects_only_its_own_alternative() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture_with(
        Resolver::Scripted {
            held: &["parent@1.4.0"],
        },
        4,
        true,
        Vec::new(),
        crashes(&["parent@1.3.0"]),
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_project_error(&outcome.errors, &outcome.items, CRASH);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.2.0"));
    assert_eq!(versions.get("sibling").map(String::as_str), Some("1.4.0"));
    assert_eq!(
        rows(&outcome),
        [
            row("parent", "1.0.0", "1.2.0", None),
            row("sibling", "1.0.0", "1.4.0", None),
        ]
    );
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert_eq!(trials_with(&trials, "parent@1.1.0"), 0, "{trials}");
    assert_eq!(
        trials.lines().last(),
        Some("1.0.0:sibling@1.4.0,parent@1.2.0"),
        "{trials}"
    );
    Ok(())
}

/// A joint fallback trial that landed the sibling's next lower target stays the pass's best
/// after the pass's later trials fail.
#[tokio::test]
async fn failed_fallback_trials_keep_an_earlier_better_fallback() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture_with(
        Resolver::Scripted {
            held: &[
                "parent@1.4.0",
                "parent@1.3.0",
                "parent@1.2.0",
                "parent@1.1.0",
                "sibling@1.4.0",
            ],
        },
        4,
        true,
        Vec::new(),
        crashes(&["sibling@1.2.0", "sibling@1.1.0"]),
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_project_error(&outcome.errors, &outcome.items, CRASH);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    assert_eq!(versions.get("sibling").map(String::as_str), Some("1.3.0"));
    assert_eq!(
        rows(&outcome),
        [
            row(
                "parent",
                "1.0.0",
                "1.4.0",
                Some(SkipReason::ResolverConflict)
            ),
            row("sibling", "1.0.0", "1.3.0", None),
        ]
    );
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert_eq!(trials_with(&trials, "sibling@1.2.0"), 1, "{trials}");
    assert_eq!(trials_with(&trials, "sibling@1.1.0"), 1, "{trials}");
    // The parent's individual retries build on the kept best rather than on nothing.
    for parent in ["parent@1.3.0", "parent@1.2.0", "parent@1.1.0"] {
        let retry = format!("1.0.0:sibling@1.3.0,{parent}");
        assert!(trials.lines().any(|line| line == retry), "{trials}");
    }
    assert_eq!(
        trials.lines().last(),
        Some("1.0.0:sibling@1.3.0"),
        "{trials}"
    );
    Ok(())
}

/// Failed joint trials and the sibling's own failed trials do not stop the parent's independent
/// lower target from landing in its individual pass.
#[tokio::test]
async fn an_independent_fallback_lands_after_failed_trials() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture_with(
        Resolver::Scripted {
            held: &["parent@1.4.0", "sibling@1.4.0"],
        },
        4,
        true,
        Vec::new(),
        crashes(&["sibling@1.3.0", "sibling@1.2.0", "sibling@1.1.0"]),
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_project_error(&outcome.errors, &outcome.items, CRASH);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.3.0"));
    assert_eq!(versions.get("sibling").map(String::as_str), Some("1.0.0"));
    assert_eq!(
        rows(&outcome),
        [
            row("parent", "1.0.0", "1.3.0", None),
            row(
                "sibling",
                "1.0.0",
                "1.4.0",
                Some(SkipReason::ResolverConflict)
            ),
        ]
    );
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert_eq!(
        trials.lines().last(),
        Some("1.0.0:parent@1.3.0"),
        "{trials}"
    );
    Ok(())
}

/// With every planned change held and every fallback trial failing, nothing is verified: the
/// failure is reported once, nothing replays, and nothing lands.
#[tokio::test]
async fn failed_fallback_trials_with_nothing_verified_land_nothing() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture_with(
        Resolver::Scripted {
            held: &["parent@1.4.0", "sibling@1.4.0"],
        },
        4,
        true,
        Vec::new(),
        crashes(&[
            "parent@1.3.0",
            "parent@1.2.0",
            "parent@1.1.0",
            "sibling@1.3.0",
            "sibling@1.2.0",
            "sibling@1.1.0",
        ]),
    )?;
    let before = lock(&project)?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_project_error(&outcome.errors, &outcome.items, CRASH);
    assert_ne!(outcome.exit, Exit::Ok);
    assert_eq!(lock(&project)?, before);
    assert_eq!(
        rows(&outcome),
        [
            row(
                "parent",
                "1.0.0",
                "1.4.0",
                Some(SkipReason::ResolverConflict)
            ),
            row(
                "sibling",
                "1.0.0",
                "1.4.0",
                Some(SkipReason::ResolverConflict)
            ),
        ]
    );
    // The initial trial, then three joint and three individual attempts per candidate, all
    // failed, and no replay.
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert_eq!(trials.lines().count(), 10, "{trials}");
    Ok(())
}

/// A failed fallback trial that leaves a restore conflict still stops the project without a
/// replay: whether its apply reports recovery evidence it still owns while the rollback itself
/// succeeds, or the rollback meets the lock it rewrote, which is kept.
#[tokio::test]
async fn a_failed_fallback_trial_with_a_restore_conflict_stops_without_a_replay() -> eyre::Result<()>
{
    for failure in [Failure::PendingRecovery, Failure::Drift] {
        let CargoFixture {
            directory: _directory,
            workspace,
            project,
        } = fixture_with(
            Resolver::Scripted {
                held: &["parent@1.4.0"],
            },
            4,
            true,
            Vec::new(),
            vec![("parent@1.3.0", failure)].into(),
        )?;
        let before = lock(&project)?;
        let outcome = workspace.upgrade(&RunOpts::default()).await;
        assert!(
            !outcome.errors.is_empty() || outcome.items.iter().any(|item| item.error.is_some()),
            "{:?}",
            outcome.items
        );
        assert_ne!(outcome.exit, Exit::Ok);
        assert!(outcome.items.iter().all(|item| !item.applied));
        let mut expected = before;
        if matches!(failure, Failure::Drift) {
            expected.insert("drift".to_string(), "1.0.0".to_string());
        }
        assert_eq!(lock(&project)?, expected);
        // No trial runs after the conflicted fallback attempt.
        let trials = std::fs::read_to_string(project.root.join("trials"))?;
        assert_eq!(
            trials.lines().last(),
            Some("1.0.0:sibling@1.4.0,parent@1.3.0"),
            "{trials}"
        );
    }
    Ok(())
}

/// Progress counts each planned candidate decided once, whether its own target, a fallback, or
/// its hold settled it.
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
        let progress = Progress::plain().project(CARGO, ".");
        let dry_run = workspace
            .dry_run_upgrade(
                workspace.projects().first().expect("fixture project"),
                &RunOpts::default(),
                &progress,
            )
            .await;
        assert!(dry_run.errors.is_empty(), "{:?}", dry_run.errors);
        let parent = dry_run
            .items
            .iter()
            .find(|item| item.name == "parent")
            .expect("parent decision");
        assert_eq!(parent.applied, landed_parent.is_some());
        if let Some(target) = landed_parent {
            assert_eq!(parent.to, target);
        }
        assert!(
            dry_run
                .items
                .iter()
                .any(|item| item.name == companion && item.applied)
        );
        assert_eq!(progress.decided_candidates(), 2, "{:?}", dry_run.items);
    }
    Ok(())
}

/// The `--fast` run options.
fn fast() -> RunOpts {
    RunOpts {
        fast: true,
        ..RunOpts::default()
    }
}

/// The Config notices `--fast` adds for the candidates it left held.
fn fast_notices(warnings: &[cooldown_core::Diagnostic]) -> Vec<&str> {
    warnings
        .iter()
        .filter(|warning| {
            warning.kind == cooldown_core::DiagnosticKind::Config
                && warning.message.starts_with("--fast")
        })
        .map(|warning| warning.message.as_str())
        .collect()
}

/// `--fast` tries only the planned target of a held candidate, never its older matured versions,
/// and names the versions it left untried.
#[tokio::test]
async fn fast_mode_skips_the_lower_targets_and_names_them() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(Resolver::ExactFrom(3), 6, false)?;
    let before = std::fs::read(project.root.join("Cargo.lock"))?;
    let outcome = workspace.upgrade(&fast()).await;
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    // Only the planned target was tried, and the lock is untouched.
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(trials.contains("parent@1.6.0"), "{trials}");
    for version in ["1.5.0", "1.4.0", "1.3.0"] {
        assert!(!trials.contains(&format!("parent@{version}")), "{trials}");
    }
    assert_eq!(std::fs::read(project.root.join("Cargo.lock"))?, before);

    // The held row keeps the reason its planned target got.
    let parent = outcome
        .items
        .iter()
        .find(|item| item.name == "parent")
        .ok_or_else(|| eyre::eyre!("missing held parent"))?;
    assert!(!parent.applied);
    assert!(parent.skipped.is_some());

    // One notice names the held candidate and the older versions a full run would try.
    let notices = fast_notices(&outcome.warnings);
    let [notice] = notices.as_slice() else {
        eyre::bail!("expected one --fast notice, got {notices:?}");
    };
    assert!(notice.contains("--no-fast"), "{notice}");
    assert!(
        notice.contains(
            "parent 1.0.0 → 1.6.0 (older matured versions not tried: 1.5.0, 1.4.0, 1.3.0)"
        ),
        "{notice}"
    );
    Ok(())
}

/// A lower target the default run lands stays unlanded under `--fast`, which leaves the lock
/// gate-clean and fails `--strict` with the reason the planned target got.
#[tokio::test]
async fn fast_mode_lands_less_but_stays_within_policy() -> eyre::Result<()> {
    let resolver = Resolver::RefuseLower {
        blocked_from: 5,
        refused: 4,
    };

    // The default run falls back to 1.3.0.
    let CargoFixture {
        directory: _default_directory,
        workspace,
        project,
    } = fixture(resolver, 4, false)?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(
        lock(&project)?.get("parent").map(String::as_str),
        Some("1.3.0")
    );
    assert_eq!(fast_notices(&outcome.warnings), Vec::<&str>::new());

    // `--fast` keeps parent where it was and the graph gate-clean.
    let CargoFixture {
        directory: _fast_directory,
        workspace,
        project,
    } = fixture(resolver, 4, false)?;
    let outcome = workspace
        .upgrade(&RunOpts {
            strict: true,
            ..fast()
        })
        .await;
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.exit, Exit::Policy);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.0.0"));
    let parent = outcome
        .items
        .iter()
        .find(|item| item.name == "parent")
        .ok_or_else(|| eyre::eyre!("missing held parent"))?;
    assert!(!parent.applied);
    assert_eq!(
        parent.skipped.as_ref().map(|skipped| skipped.reason),
        Some(SkipReason::ResolverConflict)
    );
    assert_eq!(fast_notices(&outcome.warnings).len(), 1);
    assert_clean(&workspace).await;
    Ok(())
}

/// `--fast` still lands every candidate that settles: the safe companion of a held parent is
/// replayed and committed, and nothing is noted when nothing stays held.
#[tokio::test]
async fn fast_mode_commits_the_settled_companions() -> eyre::Result<()> {
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
    let outcome = workspace.upgrade(&fast()).await;
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    assert_eq!(versions.get("safe000").map(String::as_str), Some("1.4.0"));
    let parent = outcome
        .items
        .iter()
        .find(|item| item.name == "parent")
        .ok_or_else(|| eyre::eyre!("missing held parent"))?;
    assert_eq!(
        parent
            .skipped
            .as_ref()
            .map(|skipped| skipped.message.as_str()),
        Some("parent is held by requiring holder =1.0.0")
    );
    assert_eq!(fast_notices(&outcome.warnings).len(), 1);

    // A run where everything lands has nothing to note.
    let CargoFixture {
        directory: _landed_directory,
        workspace,
        ..
    } = fixture(
        Resolver::SnapshotFamily {
            pinned_child: false,
            fresh_sibling: false,
            undeclared_parent: false,
        },
        2,
        true,
    )?;
    let outcome = workspace.upgrade(&fast()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(outcome.items.iter().filter(|item| item.applied).count(), 3);
    assert_eq!(fast_notices(&outcome.warnings), Vec::<&str>::new());
    Ok(())
}

/// `--fast` shortens only the forward upgrade batches: the remediation batch that reconciles a
/// trial's fresh transitives back within policy asks the adapter for the full search.
#[tokio::test]
async fn fast_mode_keeps_remediation_batches_exhaustive() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(Resolver::Caret, 4, false)?;
    let outcome = workspace.upgrade(&fast()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(
        lock(&project)?.get("parent").map(String::as_str),
        Some("1.4.0")
    );
    let searches = std::fs::read_to_string(project.root.join("searches"))?;
    assert!(
        searches.lines().any(|line| line == "fast:parent@1.4.0"),
        "{searches}"
    );
    assert!(
        searches
            .lines()
            .filter(|line| line.contains("child@1.0.0"))
            .all(|line| line.starts_with("exhaustive:")),
        "{searches}"
    );
    assert!(
        searches.lines().any(|line| line.contains("child@1.0.0")),
        "{searches}"
    );
    assert_clean(&workspace).await;
    Ok(())
}

/// The `--fast` notice names exactly the candidates held at the end: the held major parent, not
/// the child its deferred second pass still upgrades, and no older version where none exists.
#[tokio::test]
async fn fast_mode_notes_only_the_finally_held_candidates() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::MajorParent {
            lands: false,
            pinned: false,
        },
        1,
        false,
    )?;
    let outcome = workspace
        .upgrade(&RunOpts {
            allow_major: true,
            ..fast()
        })
        .await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.1.0"));
    let notices = fast_notices(&outcome.warnings);
    let [notice] = notices.as_slice() else {
        eyre::bail!("expected one --fast notice, got {notices:?}");
    };
    assert!(notice.ends_with(": parent 1.0.0 → 2.0.0"), "{notice}");
    assert!(!notice.contains("child"), "{notice}");
    Ok(())
}

/// The row reporting `name`, which must exist.
fn item<'a>(outcome: &'a UpgradeOutcome, name: &str) -> eyre::Result<&'a UpgradeItem> {
    outcome
        .items
        .iter()
        .find(|item| item.name == name)
        .ok_or_else(|| eyre::eyre!("no row for {name}: {:?}", outcome.items))
}

/// An upgrade whose resolve lifts a capped transitive onto its next major is rolled back, and the
/// candidate is held for the capped package even after every lower target crossed too.
#[tokio::test]
async fn a_collateral_major_crossing_holds_its_candidate() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::MajorChild {
            lowest: 1,
            highest: 99,
            fresh_major: false,
            fresh_newest: false,
            child_start: "1.0.0",
        },
        4,
        false,
    )?;
    let outcome = workspace
        .upgrade(&RunOpts {
            strict: true,
            ..RunOpts::default()
        })
        .await;

    // A ceiling hold is a deliberate policy hold, so even a strict run passes.
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.exit, Exit::Ok);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.0.0"));

    // The parent reports the capped child as what holds it, and no row claims the child moved.
    let parent = item(&outcome, "parent")?;
    assert!(!parent.applied);
    let skipped = parent.skipped.as_ref().expect("held parent");
    assert_eq!(skipped.reason, SkipReason::MaxMajorHeld);
    assert_eq!(skipped.offending.as_deref(), Some("child"));
    assert!(skipped.message.contains("child"), "{}", skipped.message);
    assert!(outcome.items.iter().all(|item| !item.applied));

    // Every lower target was tried before the parent was held.
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    for target in ["1.4.0", "1.3.0", "1.2.0", "1.1.0"] {
        assert!(trials.contains(&format!("parent@{target}")), "{trials}");
    }
    Ok(())
}

/// A lower target whose resolve keeps the capped transitive on its line lands instead.
#[tokio::test]
async fn a_collateral_major_crossing_falls_back_to_a_lower_target() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::MajorChild {
            lowest: 4,
            highest: 99,
            fresh_major: false,
            fresh_newest: false,
            child_start: "1.0.0",
        },
        4,
        false,
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.3.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.0.0"));
    let parent = item(&outcome, "parent")?;
    assert!(parent.applied);
    assert_eq!(parent.to, "1.3.0");
    Ok(())
}

/// A version the lock already held above its ceiling before the run does not hold an upgrade
/// that leaves it in place.
#[tokio::test]
async fn a_major_crossing_already_in_the_lock_does_not_hold_upgrades() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::MajorChild {
            lowest: 0,
            highest: 99,
            fresh_major: false,
            fresh_newest: false,
            child_start: "2.0.0",
        },
        4,
        false,
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.4.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("2.0.0"));
    let parent = item(&outcome, "parent")?;
    assert!(parent.applied, "{parent:?}");
    assert_eq!(parent.to, "1.4.0");
    Ok(())
}

/// A `fix` downgrade whose resolve lifts a capped transitive onto its next major is rolled back
/// and held for the capped package, leaving the cooldown violation, so a strict run fails.
#[tokio::test]
async fn fix_rolls_back_a_downgrade_that_crosses_a_major_ceiling() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::MajorChild {
            lowest: 0,
            highest: 3,
            fresh_major: false,
            fresh_newest: true,
            child_start: "1.0.0",
        },
        4,
        false,
    )?;
    let outcome = workspace
        .fix(&RunOpts {
            strict: true,
            ..RunOpts::default()
        })
        .await;
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.exit, Exit::Policy);

    // The downgrade to the matured 1.3.0 was tried, then restored.
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(trials.contains("parent@1.3.0"), "{trials}");
    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.4.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.0.0"));

    let parent = item(&outcome, "parent")?;
    assert!(!parent.applied);
    let skipped = parent.skipped.as_ref().expect("held parent");
    assert_eq!(skipped.reason, SkipReason::MaxMajorHeld);
    assert_eq!(skipped.offending.as_deref(), Some("child"));
    Ok(())
}

/// A move the reconcile pass plans from a version only the trial put above the ceiling does not
/// authorize its target: maturing a fresh `child` 2.1.0 down to 2.0.0 still leaves it above its
/// cap, so the parent that dragged it there is held.
#[tokio::test]
async fn reconciling_a_collateral_crossing_down_does_not_authorize_it() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::MajorChild {
            lowest: 1,
            highest: 99,
            fresh_major: true,
            fresh_newest: false,
            child_start: "1.0.0",
        },
        4,
        false,
    )?;
    let outcome = workspace.upgrade(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);

    // The reconcile pass did mature the fresh child down before the trial was judged.
    let trials = std::fs::read_to_string(project.root.join("trials"))?;
    assert!(trials.contains("child@2.0.0"), "{trials}");

    let versions = lock(&project)?;
    assert_eq!(versions.get("parent").map(String::as_str), Some("1.0.0"));
    assert_eq!(versions.get("child").map(String::as_str), Some("1.0.0"));
    let parent = item(&outcome, "parent")?;
    let skipped = parent.skipped.as_ref().expect("held parent");
    assert_eq!(skipped.reason, SkipReason::MaxMajorHeld);
    assert_eq!(skipped.offending.as_deref(), Some("child"));
    Ok(())
}

/// A `fix` downgrade of a version the lock already held above its ceiling continues that version,
/// so it lands although its target is above the ceiling too.
#[tokio::test]
async fn fix_matures_a_version_already_above_its_ceiling() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project,
    } = fixture(
        Resolver::MajorChild {
            lowest: 0,
            highest: 99,
            fresh_major: false,
            fresh_newest: false,
            child_start: "2.1.0",
        },
        4,
        false,
    )?;
    let outcome = workspace.fix(&RunOpts::default()).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    let versions = lock(&project)?;
    assert_eq!(versions.get("child").map(String::as_str), Some("2.0.0"));
    let child = item(&outcome, "child")?;
    assert!(child.applied, "{child:?}");
    assert_eq!(child.to, "2.0.0");
    Ok(())
}

/// What `outdated` reports for `name`: its status and target.
fn verdict_of(
    outdated: &crate::app::outdated::OutdatedOutcome,
    name: &str,
) -> (OutdatedStatus, Option<String>) {
    let item = outdated
        .items
        .iter()
        .find(|item| item.name == name)
        .unwrap_or_else(|| panic!("no {name} row: {:?}", outdated.items));
    (item.status, item.adoptable_target.clone())
}

/// `opts` for `upgrade --dry-run`.
fn dry_run(opts: &RunOpts) -> RunOpts {
    RunOpts {
        dry_run: true,
        ..opts.clone()
    }
}

/// An `allow`-exempt companion `upgrade` plans beside its driver lands with it, so `outdated`
/// reports the driver adoptable rather than blocked by the companion it would move, and the
/// companion stays `exempt` (#173).
#[tokio::test]
async fn outdated_lands_an_exempt_companion_beside_its_driver() -> eyre::Result<()> {
    let mut layer = PolicyLayer::new(Origin::Repo("cooldown.toml".into()));
    let mut rule = Rule::new(Selector::Package {
        glob: PatternGlob::new("child")?,
        tool: Some(CARGO),
    });
    rule.allow = true;
    layer.rules.push(rule);
    let CargoFixture {
        directory: _directory,
        workspace,
        project: _,
    } = fixture_with(
        Resolver::DroppedPinner {
            pinned_child: false,
        },
        1,
        false,
        Vec::new(),
        Overrides {
            layers: vec![layer],
            ..Overrides::default()
        },
    )?;

    let outdated = workspace.outdated(&RunOpts::default()).await;
    let upgrade = workspace.upgrade(&dry_run(&RunOpts::default())).await;

    assert!(outdated.errors.is_empty(), "{:?}", outdated.errors);
    assert_eq!(
        rows(&upgrade),
        [
            row("child", "1.0.0", "1.1.0", None),
            row("parent", "1.0.0", "1.1.0", None),
        ]
    );
    assert_eq!(
        verdict_of(&outdated, "parent"),
        (OutdatedStatus::Adoptable, Some("1.1.0".to_string()))
    );
    assert_eq!(
        verdict_of(&outdated, "child"),
        (OutdatedStatus::Exempt, Some("1.1.0".to_string()))
    );
    Ok(())
}

/// A preferred target the resolve refuses leaves `upgrade` landing a lower fallback, which is
/// the target `outdated` reports (#173).
#[tokio::test]
async fn outdated_reports_the_fallback_upgrade_lands() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project: _,
    } = fixture(Resolver::ExactFrom(4), 4, true)?;

    let outdated = workspace.outdated(&RunOpts::default()).await;
    let upgrade = workspace.upgrade(&dry_run(&RunOpts::default())).await;

    assert!(
        rows(&upgrade).contains(&row("parent", "1.0.0", "1.3.0", None)),
        "{:?}",
        rows(&upgrade)
    );
    assert_eq!(
        verdict_of(&outdated, "parent"),
        (OutdatedStatus::Adoptable, Some("1.3.0".to_string()))
    );
    Ok(())
}

/// `outdated --major --transitive` judges a transitive within its own major, as `upgrade`
/// plans it, rather than offering the next major `upgrade` never grants an undeclared node
/// (#173).
#[tokio::test]
async fn outdated_judges_a_transitive_within_its_major() -> eyre::Result<()> {
    let releases = BTreeMap::from([(
        "child".to_string(),
        vec![
            release("1.0.0", false),
            release("1.1.0", false),
            release("2.0.0", false),
        ],
    )]);
    let CargoFixture {
        directory: _directory,
        workspace,
        project: _,
    } = fixture_with(
        Resolver::MajorParent {
            lands: false,
            pinned: false,
        },
        1,
        false,
        Vec::new(),
        Overrides {
            releases,
            ..Overrides::default()
        },
    )?;
    let opts = RunOpts {
        allow_major: true,
        transitive: true,
        ..RunOpts::default()
    };

    let outdated = workspace.outdated(&opts).await;
    let upgrade = workspace.upgrade(&dry_run(&opts)).await;

    assert!(
        rows(&upgrade).contains(&row("child", "1.0.0", "1.1.0", None)),
        "{:?}",
        rows(&upgrade)
    );
    assert_eq!(
        verdict_of(&outdated, "child"),
        (OutdatedStatus::Adoptable, Some("1.1.0".to_string()))
    );
    // Latest still names the newest version there is.
    let child = outdated
        .items
        .iter()
        .find(|item| item.name == "child")
        .ok_or_else(|| eyre::eyre!("no child row: {:?}", outdated.items))?;
    assert_eq!(
        child.latest.as_ref().map(|latest| latest.version.as_str()),
        Some("2.0.0")
    );
    Ok(())
}

/// A transitive its requirer pins exactly is tried once that requirer's major drops the pin,
/// in the follower pass, and `outdated` reports that pass's refusal rather than the pin its row
/// started under (#173).
#[tokio::test]
async fn outdated_reports_a_followers_refusal() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project: _,
    } = fixture(
        Resolver::MajorParent {
            lands: true,
            pinned: true,
        },
        1,
        false,
    )?;
    let opts = RunOpts {
        allow_major: true,
        transitive: true,
        ..RunOpts::default()
    };

    let outdated = workspace.outdated(&opts).await;
    let upgrade = workspace.upgrade(&dry_run(&opts)).await;

    assert_eq!(
        rows(&upgrade),
        [
            row(
                "child",
                "1.0.0",
                "1.1.0",
                Some(SkipReason::ResolverConflict)
            ),
            row("parent", "1.0.0", "2.0.0", None),
        ]
    );
    assert_eq!(
        verdict_of(&outdated, "parent"),
        (OutdatedStatus::Adoptable, Some("2.0.0".to_string()))
    );
    let child = outdated
        .items
        .iter()
        .find(|item| item.name == "child")
        .ok_or_else(|| eyre::eyre!("no child row: {:?}", outdated.items))?;
    assert_eq!(
        (
            child.status,
            child.adoptable_target.as_deref(),
            child.blocked_reason.as_deref(),
            child.held_by.as_ref(),
        ),
        (
            OutdatedStatus::Blocked,
            Some("1.1.0"),
            Some("child is held by holder"),
            None
        )
    );
    Ok(())
}

/// A deferred transitive whose follower replan cannot read the graph is left undecided by a
/// failure, so `outdated` reports that failure on its row rather than reading it as following
/// its requirers, beside the requirer's landed major (#173).
#[tokio::test]
async fn a_follower_its_replan_cannot_read_carries_the_failure() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project: _,
    } = fixture_with(
        Resolver::MajorParent {
            lands: true,
            pinned: false,
        },
        1,
        false,
        Vec::new(),
        Overrides {
            unreadable_with: Some("parent@2.0.0"),
            ..Overrides::default()
        },
    )?;
    let opts = RunOpts {
        allow_major: true,
        transitive: true,
        ..RunOpts::default()
    };

    let outdated = workspace.outdated(&opts).await;

    assert_eq!(
        verdict_of(&outdated, "parent"),
        (OutdatedStatus::Adoptable, Some("2.0.0".to_string()))
    );
    let child = outdated
        .items
        .iter()
        .find(|item| item.name == "child")
        .ok_or_else(|| eyre::eyre!("no child row: {:?}", outdated.items))?;
    assert_eq!(child.status, OutdatedStatus::Error, "{child:?}");
    assert!(
        child
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("the manifest cannot be read")),
        "{child:?}"
    );
    Ok(())
}

/// A failure the dry run meets only for a package `explain` does not report reaches the
/// verdict as a warning, never as the reported row's error (#173).
#[tokio::test]
async fn explain_notes_another_packages_failure() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project: _,
    } = fixture_with(
        Resolver::MajorParent {
            lands: true,
            pinned: true,
        },
        1,
        false,
        Vec::new(),
        crashes(&["child@1.1.0"]),
    )?;
    let opts = RunOpts {
        allow_major: true,
        ..RunOpts::default()
    };

    let explained = workspace.explain("parent", &opts).await?;

    let [parent] = explained.meta.verdicts.as_slice() else {
        eyre::bail!("expected one parent verdict: {:?}", explained.meta.verdicts);
    };
    assert_eq!(
        (parent.status, parent.adoptable_target.as_deref()),
        (OutdatedStatus::Adoptable, Some("2.0.0"))
    );
    assert!(explained.errors.is_empty(), "{:?}", explained.errors);
    assert!(
        explained.warnings.iter().any(|warning| {
            warning.message.contains(CRASH) && warning.package.as_deref() == Some("child")
        }),
        "{:?}",
        explained.warnings
    );
    Ok(())
}

/// A registry failure for one dependency leaves the dry run's settled outcomes standing: the
/// sibling it landed stays adoptable, and only the row it could not judge is an error.
#[tokio::test]
async fn an_unavailable_registry_entry_keeps_the_other_rows_settled() -> eyre::Result<()> {
    let CargoFixture {
        directory: _directory,
        workspace,
        project: _,
    } = fixture_with(
        Resolver::Scripted { held: &[] },
        4,
        true,
        Vec::new(),
        Overrides {
            unavailable: &["parent"],
            ..Overrides::default()
        },
    )?;

    let outdated = workspace.outdated(&RunOpts::default()).await;

    assert_eq!(
        verdict_of(&outdated, "sibling"),
        (OutdatedStatus::Adoptable, Some("1.4.0".to_string()))
    );
    let parent = outdated
        .items
        .iter()
        .find(|item| item.name == "parent")
        .expect("parent row");
    assert_eq!(parent.status, OutdatedStatus::Error);
    assert!(
        parent
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("the registry cannot serve parent")),
        "{parent:?}"
    );
    // The row carries the failure, so the report does not repeat it.
    assert!(outdated.errors.is_empty(), "{:?}", outdated.errors);
    assert!(
        outdated
            .warnings
            .iter()
            .all(|warning| !warning.message.contains("the registry cannot serve parent")),
        "{:?}",
        outdated.warnings
    );
    Ok(())
}
