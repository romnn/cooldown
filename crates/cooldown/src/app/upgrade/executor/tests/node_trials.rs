//! File-backed Node-family trials exercise the current-line fallback: a held cross-major target
//! retries the highest adoptable target on the current major line through the same rollback and
//! replay the lower-line fallback uses.

use crate::app::upgrade::PreviewEvidence;
use crate::app::{AdapterSet, Baseline, Exit, ProjectCtx, ProjectProgress, RunOpts, Workspace};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre;
use cooldown_core::config::builtin_default_layer;
use cooldown_core::{
    AdvisoryFetch, AdvisoryPolicy, AdvisorySource, AdvisorySourceId, ApplyReport, CandidateScope,
    Capabilities, Change, CoreError, DepScope, Dependency, FetchContext, GeneratedMembers,
    GraphHoldEdge, GraphHoldKind, LockStatus, LockVerifyReport, MajorKey, MemberRef,
    NativePolicyLayer, PackageAdvisories, PackageId, Plan, PolicyLayer, PolicyStack,
    PreparedMutation, Project, ProjectMarker, ProjectMutationJournal, RawAdvisory, Release,
    ReleaseFetcher, ReleaseOrder, ReleaseQuality, SkipReason, Skipped, TargetFallback, ToolId,
    ToolRead, ToolWrite, UpdateKind, VerifyReport, Version,
};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

const NODE: ToolId = ToolId("fakenode");
const LOCKFILE: &str = "package-lock.json";
const MANIFEST: &str = "package.json";

fn timestamp(value: &str) -> jiff::Timestamp {
    value.parse().expect("fixed fixture timestamp")
}

/// A release relative to a `1.0.0` pin; `fresh` releases are younger than the default window at
/// the fixture's evaluation instant.
fn release(version: &str, fresh: bool) -> Release {
    let parts: Vec<u32> = version
        .split('.')
        .map(|part| part.parse().expect("semver component"))
        .collect();
    let major = *parts.first().expect("major component");
    Release {
        version: Version::new(version),
        order: ReleaseOrder(parts.iter().flat_map(|part| part.to_be_bytes()).collect()),
        major: MajorKey(major.to_string()),
        major_number: Some(u64::from(major)),
        kind_from_current: Some(if major != 1 {
            UpdateKind::Major
        } else if parts.get(1) != Some(&0) {
            UpdateKind::Minor
        } else {
            UpdateKind::Patch
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

fn releases(versions: &[(&str, bool)]) -> Vec<Release> {
    versions
        .iter()
        .map(|(version, fresh)| release(version, *fresh))
        .collect()
}

fn package(name: &str) -> PackageId {
    PackageId::new(NODE, name, None)
}

fn lock(project: &Project) -> cooldown_core::Result<BTreeMap<String, String>> {
    serde_json::from_slice(&std::fs::read(project.root.join(LOCKFILE))?)
        .map_err(|err| CoreError::LockUnreadable(err.to_string()))
}

/// A target the fake resolver refuses, with the skip it reports.
struct Hold {
    target: &'static str,
    reason: SkipReason,
    offending: Option<&'static str>,
    detail: Option<&'static str>,
}

/// A companion whose landing floats another locked package, the way a joint resolve moves a
/// shared dependency the plan does not land itself (absent from the plan, or held in it).
/// Each float is logged into its trial as `float:<name>@<version>`.
struct Float {
    trigger: &'static str,
    floated: &'static str,
    from: &'static str,
    to: &'static str,
}

/// A workspace member a float leaves on its old version of the floated package.
struct Straggler {
    package: &'static str,
    member: &'static str,
}

/// Two packages that only resolve at one shared version.
struct Coupled {
    left: &'static str,
    right: &'static str,
}

/// How the fake resolver reacts to a plan.
#[derive(Default)]
struct Script {
    holds: Vec<Hold>,
    coupled: Option<Coupled>,
    float: Option<Float>,
    /// `dom` on its 2.x line requires `child` at exactly its fresh `1.1.0`.
    fresh_child_on_major: bool,
    /// A target the resolver accepts once and refuses on every later trial.
    refused_after_first: Option<&'static str>,
    /// A target whose landing makes the next graph read drift the manifest behind cooldown's
    /// back.
    drift_after: Option<&'static str>,
    /// The workspace members declaring every direct dependency; none means the root alone.
    members: &'static [&'static str],
    /// A member a float leaves behind, recorded in the lock as a `<name>@<member>` entry.
    straggler: Option<Straggler>,
}

struct FakeNode {
    fallback: TargetFallback,
    script: Script,
    releases: BTreeMap<String, Vec<Release>>,
    advisory_ecosystem: Option<&'static str>,
    /// Each apply's plan as `name@target` entries, kept outside the project because a policy
    /// preview runs in a throwaway copy.
    trials: Mutex<Vec<Vec<String>>>,
}

impl FakeNode {
    fn new(fallback: TargetFallback, script: Script) -> Self {
        FakeNode {
            fallback,
            script,
            releases: BTreeMap::new(),
            advisory_ecosystem: None,
            trials: Mutex::new(Vec::new()),
        }
    }

    fn with_releases(mut self, name: &str, versions: &[(&str, bool)]) -> Self {
        self.releases.insert(name.to_string(), releases(versions));
        self
    }

    /// Whether the resolver refuses `change`'s target outright.
    fn holds(&self, change: &Change) -> bool {
        let target = format!("{}@{}", change.package.name, change.to);
        self.script.holds.iter().any(|hold| hold.target == target)
    }

    fn trials(&self) -> Vec<Vec<String>> {
        self.trials.lock().expect("trial log").clone()
    }
}

fn member_ref(name: &str) -> MemberRef {
    MemberRef {
        name: name.to_string(),
        path: format!("packages/{name}"),
    }
}

fn dependency(name: &str, version: &str) -> Dependency {
    Dependency {
        package: package(name),
        advisory_identity: Some(name.to_string()),
        current: Version::new(version),
        current_quality: ReleaseQuality::Stable,
        direct: name != "child",
        artifacts: Vec::new(),
        graph_floor: None,
        graph_ceiling: None,
        declared_bound: None,
        members: Vec::new(),
        pinned: false,
        hold_edges: Vec::new(),
    }
}

#[async_trait]
impl ToolRead for FakeNode {
    fn id(&self) -> ToolId {
        NODE
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            advisory_ecosystem: self.advisory_ecosystem,
            ..Capabilities::default()
        }
    }

    fn project_marker(&self) -> ProjectMarker {
        ProjectMarker {
            marker: LOCKFILE,
            manifest: MANIFEST,
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
        if let Some(target) = self.script.drift_after
            && versions
                .iter()
                .any(|(name, version)| format!("{name}@{version}") == target)
            && !std::fs::read_to_string(project.root.join(MANIFEST))?.contains("drift")
        {
            std::fs::write(project.root.join(MANIFEST), r#"{"drift":true}"#)?;
        }
        let mut dependencies = Vec::new();
        for (key, version) in &versions {
            let (name, straggler) = key
                .split_once('@')
                .map_or((key.as_str(), None), |(name, member)| (name, Some(member)));
            let mut dep = dependency(name, version);
            if dep.direct {
                dep.members = self
                    .script
                    .members
                    .iter()
                    .filter(|member| match straggler {
                        Some(straggler) => **member == straggler,
                        None => !versions.contains_key(&format!("{name}@{member}")),
                    })
                    .map(|member| member_ref(member))
                    .collect();
            }
            if name == "child"
                && let Some(dom) = versions.get("dom")
                && dom.starts_with("2.")
                && self.script.fresh_child_on_major
            {
                dep.graph_floor = Some(Version::new("1.1.0"));
                dep.graph_ceiling = Some(Version::new("1.1.0"));
                dep.hold_edges = vec![GraphHoldEdge {
                    requirer: "dom".to_string(),
                    requirer_version: Version::new(dom),
                    requirement: "1.1.0".to_string(),
                    bound: Version::new("1.1.0"),
                    kind: GraphHoldKind::Ceiling,
                }];
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
            detail: "consistent fake Node resolve".to_string(),
        })
    }
}

#[async_trait]
impl ReleaseFetcher for FakeNode {
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
impl ToolWrite for FakeNode {
    fn mutation_tool(&self) -> ToolId {
        NODE
    }

    fn target_fallback(&self) -> TargetFallback {
        self.fallback
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> cooldown_core::Result<ProjectMutationJournal> {
        ProjectMutationJournal::capture(
            &project.root,
            [Utf8Path::new(LOCKFILE), Utf8Path::new(MANIFEST)],
        )
    }

    async fn apply(&self, mutation: &PreparedMutation) -> cooldown_core::Result<ApplyReport> {
        let (project, plan, _) = mutation.parts_for(self)?;
        let mut versions = lock(project)?;
        let targets: Vec<String> = plan
            .changes
            .iter()
            .map(|change| format!("{}@{}", change.package.name, change.to))
            .collect();
        let earlier = {
            let mut trials = self.trials.lock().expect("trial log");
            let earlier = self.script.refused_after_first.map_or(0, |refused| {
                trials
                    .iter()
                    .filter(|trial| trial.iter().any(|target| target == refused))
                    .count()
            });
            trials.push(targets);
            earlier
        };
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            let target = format!("{}@{}", change.package.name, change.to);
            if let Some(hold) = self.script.holds.iter().find(|hold| hold.target == target) {
                report.skipped.push(Skipped {
                    change: change.clone(),
                    reason: hold.reason,
                    offending: hold.offending.map(package),
                    detail: hold.detail.map(str::to_string),
                });
                continue;
            }
            if self.script.refused_after_first == Some(target.as_str()) && earlier > 0 {
                report.skipped.push(Skipped {
                    change: change.clone(),
                    reason: SkipReason::ResolverConflict,
                    offending: None,
                    detail: Some("the registry moved between trials".to_string()),
                });
                continue;
            }
            versions.insert(change.package.name.clone(), change.to.to_string());
            report.applied.push(change.clone());
            if self.script.fresh_child_on_major && change.package.name == "dom" {
                let child = if change.to.as_str().starts_with("2.") {
                    "1.1.0"
                } else {
                    "1.0.0"
                };
                versions.insert("child".to_string(), child.to_string());
            }
            if let Some(float) = &self.script.float
                && float.trigger == change.package.name
                && versions.get(float.floated).map(String::as_str) == Some(float.from)
                && !plan
                    .changes
                    .iter()
                    .any(|planned| planned.package.name == float.floated && !self.holds(planned))
            {
                versions.insert(float.floated.to_string(), float.to.to_string());
                if let Some(trial) = self.trials.lock().expect("trial log").last_mut() {
                    trial.push(format!("float:{}@{}", float.floated, float.to));
                }
                if let Some(straggler) = &self.script.straggler
                    && straggler.package == float.floated
                {
                    versions.insert(
                        format!("{}@{}", float.floated, straggler.member),
                        float.from.to_string(),
                    );
                }
                // Shaped like the npm adapter's lock-diff rows: transitive and unattributed.
                report.applied.push(Change {
                    package: package(float.floated),
                    from: Version::new(float.from),
                    to: Version::new(float.to),
                    kind: UpdateKind::Patch,
                    downgrade: false,
                    direct: false,
                    members: Vec::new(),
                });
            }
        }
        if let Some(Coupled { left, right }) = &self.script.coupled
            && versions.get(*left) != versions.get(*right)
        {
            return Err(CoreError::UnacceptableResolve(format!(
                "{left} and {right} must resolve to one version"
            )));
        }
        std::fs::write(
            project.root.join(LOCKFILE),
            serde_json::to_vec(&versions).map_err(|err| CoreError::System(err.to_string()))?,
        )?;
        Ok(report)
    }

    async fn build(&self, _project: &Project) -> cooldown_core::Result<VerifyReport> {
        Ok(VerifyReport {
            ok: true,
            detail: String::new(),
        })
    }
}

/// Reports two advisories against `dom@1.0.0`: the in-line `1.1.0` fixes only the first, the
/// cross-major `2.0.0` fixes both.
struct DomAdvisories;

#[async_trait]
impl AdvisorySource for DomAdvisories {
    fn id(&self) -> AdvisorySourceId {
        AdvisorySourceId("osv")
    }

    async fn advisories(
        &self,
        _ecosystem: &str,
        packages: &[String],
    ) -> cooldown_core::Result<AdvisoryFetch> {
        let advisory = |id: &str, affected: &[&str], fixes: &[&str]| RawAdvisory {
            id: id.to_string(),
            aliases: Vec::new(),
            severity: cooldown_core::AdvisorySeverity::Moderate,
            withdrawn: false,
            summary: String::new(),
            ranges: Vec::new(),
            affected_versions: affected.iter().map(ToString::to_string).collect(),
            fixes: fixes.iter().map(ToString::to_string).collect(),
        };
        Ok(AdvisoryFetch {
            packages: packages
                .iter()
                .map(|name| PackageAdvisories {
                    package: name.clone(),
                    advisories: if name == "dom" {
                        vec![
                            advisory("GHSA-inline", &["1.0.0"], &["1.1.0"]),
                            advisory("GHSA-major", &["1.0.0", "1.1.0"], &["2.0.0"]),
                        ]
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
            stale: false,
        })
    }
}

struct NodeFixture {
    _directory: tempfile::TempDir,
    workspace: Workspace,
    project: Project,
    fake: Arc<FakeNode>,
}

fn fixture(fake: FakeNode, initial: &[(&str, &str)]) -> eyre::Result<NodeFixture> {
    let advised = fake.advisory_ecosystem.is_some();
    let directory = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
        .map_err(|path| eyre::eyre!("non-UTF-8 root: {}", path.display()))?;
    std::fs::write(root.join(MANIFEST), r#"{"name":"app"}"#)?;
    let initial: BTreeMap<&str, &str> = initial.iter().copied().collect();
    std::fs::write(root.join(LOCKFILE), serde_json::to_vec(&initial)?)?;
    let project = Project {
        root: root.clone(),
        kind: NODE,
        manifest: root.join(MANIFEST),
        exclude_newer: None,
        exclude_newer_package: BTreeMap::new(),
        generated_members: GeneratedMembers::undeclared(),
    };
    let fake = Arc::new(fake);
    let mut adapters = AdapterSet::new();
    adapters.register_target_verified_mutator(Arc::clone(&fake))?;
    let mut layers = vec![builtin_default_layer()];
    if advised {
        let mut layer = PolicyLayer::new(cooldown_core::Origin::Cli);
        layer.advisories = Some(AdvisoryPolicy {
            enabled: Some(true),
            ..AdvisoryPolicy::default()
        });
        layers.push(layer);
    }
    let context = ProjectCtx {
        tool: NODE,
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
    let mut workspace = Workspace::new(
        adapters,
        vec![context],
        timestamp("2026-06-17T00:00:00Z"),
        Baseline::default(),
        root,
        Vec::new(),
    );
    if advised {
        workspace = workspace.with_advisory_source(Arc::new(DomAdvisories));
    }
    Ok(NodeFixture {
        _directory: directory,
        workspace,
        project,
        fake,
    })
}

fn major() -> RunOpts {
    RunOpts {
        allow_major: true,
        ..RunOpts::default()
    }
}

/// `dom` is pinned at `1.0.0` with `1.0.1` and `1.1.0` on its line (matured when
/// `inline_matured`), a cooling `1.2.0`, and a matured `2.0.0`; `safe` is an independent
/// companion with a matured `1.1.0`.
fn dom_and_safe(fallback: TargetFallback, script: Script, inline_matured: bool) -> FakeNode {
    FakeNode::new(fallback, script)
        .with_releases(
            "dom",
            &[
                ("1.0.0", false),
                ("1.0.1", !inline_matured),
                ("1.1.0", !inline_matured),
                ("1.2.0", true),
                ("2.0.0", false),
            ],
        )
        .with_releases("safe", &[("1.0.0", false), ("1.1.0", false)])
}

fn peer_held_dom() -> Hold {
    Hold {
        target: "dom@2.0.0",
        reason: SkipReason::PeerHeld,
        offending: Some("plugin"),
        detail: Some("plugin@1.0.0 peer-requires dom@^1.0.0"),
    }
}

/// One report row, reduced to what the fallback decides.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    name: String,
    from: String,
    to: String,
    applied: bool,
    reason: Option<SkipReason>,
    message: Option<String>,
}

fn rows(items: &[crate::app::UpgradeItem]) -> Vec<Row> {
    let mut rows: Vec<Row> = items
        .iter()
        .map(|item| Row {
            name: item.name.clone(),
            from: item.from.clone(),
            to: item.to.clone(),
            applied: item.applied,
            reason: item.skipped.as_ref().map(|skipped| skipped.reason),
            message: item.skipped.as_ref().map(|skipped| skipped.message.clone()),
        })
        .collect();
    sort_rows(&mut rows);
    rows
}

fn sort_rows(rows: &mut [Row]) {
    rows.sort_by(|a, b| {
        (&a.name, &a.to, &a.from, a.applied).cmp(&(&b.name, &b.to, &b.from, b.applied))
    });
}

fn applied(name: &str, from: &str, to: &str) -> Row {
    Row {
        name: name.to_string(),
        from: from.to_string(),
        to: to.to_string(),
        applied: true,
        reason: None,
        message: None,
    }
}

fn skipped(name: &str, from: &str, to: &str, reason: SkipReason, message: &str) -> Row {
    Row {
        name: name.to_string(),
        from: from.to_string(),
        to: to.to_string(),
        applied: false,
        reason: Some(reason),
        message: Some(message.to_string()),
    }
}

fn sorted(mut rows: Vec<Row>) -> Vec<Row> {
    sort_rows(&mut rows);
    rows
}

fn locked(fixture: &NodeFixture, name: &str) -> eyre::Result<Option<String>> {
    Ok(lock(&fixture.project)?.get(name).cloned())
}

/// A peer-held major still adopts its matured in-line target: one applied row for the in-line
/// move beside one skipped row for the major, which keeps the adapter's original explanation.
#[tokio::test]
async fn held_major_lands_its_current_line_target_beside_the_held_row() -> eyre::Result<()> {
    let fixture = fixture(
        dom_and_safe(
            TargetFallback::CurrentLine,
            Script {
                holds: vec![peer_held_dom()],
                ..Script::default()
            },
            true,
        ),
        &[("dom", "1.0.0"), ("safe", "1.0.0")],
    )?;

    let outcome = fixture.workspace.upgrade(&major()).await;

    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "dom")?.as_deref(), Some("1.1.0"));
    assert_eq!(locked(&fixture, "safe")?.as_deref(), Some("1.1.0"));
    // The in-line move is applied, the major keeps its peer hold, and the companion lands.
    assert_eq!(
        rows(&outcome.items),
        sorted(vec![
            applied("dom", "1.0.0", "1.1.0"),
            skipped(
                "dom",
                "1.0.0",
                "2.0.0",
                SkipReason::PeerHeld,
                "plugin@1.0.0 peer-requires dom@^1.0.0"
            ),
            applied("safe", "1.0.0", "1.1.0"),
        ])
    );
    let held = outcome
        .items
        .iter()
        .find(|item| !item.applied)
        .and_then(|item| item.skipped.as_ref())
        .and_then(|skipped| skipped.offending.as_deref());
    assert_eq!(held, Some("plugin"));
    // The initial trial, one current-line attempt on top of the landed companion, and the replay;
    // the refused major is never retried.
    let trials = fixture.fake.trials();
    assert_eq!(trials.len(), 3, "{trials:?}");
    assert!(
        trials
            .iter()
            .skip(1)
            .all(|trial| trial.iter().all(|target| target != "dom@2.0.0")),
        "{trials:?}"
    );
    assert!(
        trials
            .iter()
            .skip(1)
            .all(|trial| trial.iter().any(|target| target == "safe@1.1.0")),
        "{trials:?}"
    );
    Ok(())
}

/// Coupled in-line moves land together, while a rejected major without an in-line target beside
/// them is reported but never pushed back into a retry.
#[tokio::test]
async fn coupled_current_line_pair_lands_beside_a_major_without_an_alternative() -> eyre::Result<()>
{
    let pair_hold = |target| Hold {
        target,
        reason: SkipReason::PeerHeld,
        offending: None,
        detail: None,
    };
    let fake = FakeNode::new(
        TargetFallback::CurrentLine,
        Script {
            holds: vec![
                pair_hold("left@2.0.0"),
                pair_hold("right@2.0.0"),
                Hold {
                    target: "lone@2.0.0",
                    reason: SkipReason::ResolverConflict,
                    offending: None,
                    detail: None,
                },
            ],
            coupled: Some(Coupled {
                left: "left",
                right: "right",
            }),
            ..Script::default()
        },
    )
    .with_releases(
        "left",
        &[("1.0.0", false), ("1.1.0", false), ("2.0.0", false)],
    )
    .with_releases(
        "right",
        &[("1.0.0", false), ("1.1.0", false), ("2.0.0", false)],
    )
    // `lone` has no matured release on its current line.
    .with_releases(
        "lone",
        &[("1.0.0", false), ("1.1.0", true), ("2.0.0", false)],
    );
    let fixture = fixture(
        fake,
        &[("left", "1.0.0"), ("lone", "1.0.0"), ("right", "1.0.0")],
    )?;

    let outcome = fixture.workspace.upgrade(&major()).await;

    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "left")?.as_deref(), Some("1.1.0"));
    assert_eq!(locked(&fixture, "right")?.as_deref(), Some("1.1.0"));
    assert_eq!(locked(&fixture, "lone")?.as_deref(), Some("1.0.0"));
    let trials = fixture.fake.trials();
    // One joint attempt carries both halves of the pair.
    assert!(
        trials.iter().any(|trial| {
            trial.iter().any(|target| target == "left@1.1.0")
                && trial.iter().any(|target| target == "right@1.1.0")
        }),
        "{trials:?}"
    );
    // The refused major without an alternative only appears in the initial trial.
    assert!(
        trials
            .iter()
            .skip(1)
            .all(|trial| trial.iter().all(|target| target != "lone@2.0.0")),
        "{trials:?}"
    );
    let lone = rows(&outcome.items)
        .into_iter()
        .filter(|row| row.name == "lone")
        .collect::<Vec<_>>();
    assert_eq!(
        lone,
        [skipped(
            "lone",
            "1.0.0",
            "2.0.0",
            SkipReason::ResolverConflict,
            SkipReason::ResolverConflict.message()
        )]
    );
    Ok(())
}

/// Without a matured in-line target the settled trial commits exactly as a writer without
/// fallback commits it: the same rows from one trial, with no restore and no replay.
#[tokio::test]
async fn no_current_line_target_keeps_the_settled_trial_unchanged() -> eyre::Result<()> {
    let mut observed = Vec::new();
    for fallback in [TargetFallback::None, TargetFallback::CurrentLine] {
        let fixture = fixture(
            dom_and_safe(
                fallback,
                Script {
                    holds: vec![peer_held_dom()],
                    ..Script::default()
                },
                false,
            ),
            &[("dom", "1.0.0"), ("safe", "1.0.0")],
        )?;
        let outcome = fixture.workspace.upgrade(&major()).await;
        assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
        assert_eq!(locked(&fixture, "dom")?.as_deref(), Some("1.0.0"));
        observed.push((rows(&outcome.items), fixture.fake.trials()));
    }
    let [(plain_rows, plain_trials), (fallback_rows, fallback_trials)] = observed.as_slice() else {
        return Err(eyre::eyre!("expected one run per fallback strategy"));
    };
    assert_eq!(fallback_rows, plain_rows);
    assert_eq!(fallback_trials, plain_trials);
    assert_eq!(fallback_trials.len(), 1, "{fallback_trials:?}");
    Ok(())
}

/// A major held as not eligible has no editable requirement to retarget, so its matured in-line
/// target is not tried either.
#[tokio::test]
async fn a_not_eligible_major_tries_no_current_line_target() -> eyre::Result<()> {
    let fixture = fixture(
        dom_and_safe(
            TargetFallback::CurrentLine,
            Script {
                holds: vec![Hold {
                    target: "dom@2.0.0",
                    reason: SkipReason::NotEligible,
                    offending: None,
                    detail: None,
                }],
                ..Script::default()
            },
            true,
        ),
        &[("dom", "1.0.0")],
    )?;

    let outcome = fixture.workspace.upgrade(&major()).await;

    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "dom")?.as_deref(), Some("1.0.0"));
    assert_eq!(
        fixture.fake.trials().len(),
        1,
        "{:?}",
        fixture.fake.trials()
    );
    assert_eq!(
        rows(&outcome.items),
        [skipped(
            "dom",
            "1.0.0",
            "2.0.0",
            SkipReason::NotEligible,
            SkipReason::NotEligible.message()
        )]
    );
    Ok(())
}

/// Under `--strict` a held major is judged by its own reason: a conservative split hold never
/// fails the run, while a resolver rejection whose in-line target also fails still does.
#[tokio::test]
async fn strict_mode_judges_a_superseded_major_by_its_hold_reason() -> eyre::Result<()> {
    let strict = RunOpts {
        strict: true,
        ..major()
    };

    // A split-held major whose in-line target lands
    let split_held = fixture(
        dom_and_safe(
            TargetFallback::CurrentLine,
            Script {
                holds: vec![Hold {
                    target: "dom@2.0.0",
                    reason: SkipReason::MultiVersionHeld,
                    offending: None,
                    detail: None,
                }],
                ..Script::default()
            },
            true,
        ),
        &[("dom", "1.0.0")],
    )?;
    let outcome = split_held.workspace.upgrade(&strict).await;
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.items);
    assert_eq!(locked(&split_held, "dom")?.as_deref(), Some("1.1.0"));

    // A resolver-rejected major whose in-line target is rejected too
    let rejected = fixture(
        dom_and_safe(
            TargetFallback::CurrentLine,
            Script {
                holds: ["dom@2.0.0", "dom@1.1.0"]
                    .into_iter()
                    .map(|target| Hold {
                        target,
                        reason: SkipReason::ResolverConflict,
                        offending: None,
                        detail: None,
                    })
                    .collect(),
                ..Script::default()
            },
            true,
        ),
        &[("dom", "1.0.0")],
    )?;
    let outcome = rejected.workspace.upgrade(&strict).await;
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.exit, Exit::Policy);
    assert_eq!(locked(&rejected, "dom")?.as_deref(), Some("1.0.0"));
    // Only the planned major is reported; the failed in-line attempt adds no row.
    assert_eq!(
        rows(&outcome.items),
        [skipped(
            "dom",
            "1.0.0",
            "2.0.0",
            SkipReason::ResolverConflict,
            SkipReason::ResolverConflict.message()
        )]
    );
    Ok(())
}

/// A replay that loses the in-line target fails closed like any replay: the lock is restored and
/// every accepted candidate reports held beside the major.
#[tokio::test]
async fn replay_that_loses_the_current_line_target_restores_the_baseline() -> eyre::Result<()> {
    let fixture = fixture(
        dom_and_safe(
            TargetFallback::CurrentLine,
            Script {
                holds: vec![peer_held_dom()],
                refused_after_first: Some("dom@1.1.0"),
                ..Script::default()
            },
            true,
        ),
        &[("dom", "1.0.0"), ("safe", "1.0.0")],
    )?;
    let before = std::fs::read(fixture.project.root.join(LOCKFILE))?;

    let outcome = fixture.workspace.upgrade(&major()).await;

    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(std::fs::read(fixture.project.root.join(LOCKFILE))?, before);
    let rows = rows(&outcome.items);
    assert!(rows.iter().all(|row| !row.applied), "{rows:?}");
    // The major keeps its peer hold, and the lost in-line target and the companion are held by
    // the replay's verdict.
    assert_eq!(
        rows.iter()
            .map(|row| (row.name.as_str(), row.to.as_str(), row.reason))
            .collect::<Vec<_>>(),
        [
            ("dom", "1.1.0", Some(SkipReason::ResolverConflict)),
            ("dom", "2.0.0", Some(SkipReason::PeerHeld)),
            ("safe", "1.1.0", Some(SkipReason::ResolverConflict)),
        ]
    );
    assert_eq!(fixture.fake.trials().len(), 3);
    Ok(())
}

/// A trial whose rollback finds the project changed behind cooldown's back stops recovering: the
/// drift is kept, no row claims a landing, and the project reports the conflict.
#[tokio::test]
async fn restoration_conflict_after_a_current_line_trial_stops_the_project() -> eyre::Result<()> {
    let fixture = fixture(
        dom_and_safe(
            TargetFallback::CurrentLine,
            Script {
                holds: vec![peer_held_dom()],
                drift_after: Some("dom@1.1.0"),
                ..Script::default()
            },
            true,
        ),
        &[("dom", "1.0.0")],
    )?;

    let outcome = fixture.workspace.upgrade(&major()).await;

    assert!(!outcome.errors.is_empty(), "{:?}", outcome.items);
    assert_ne!(outcome.exit, Exit::Ok);
    assert!(outcome.items.iter().all(|item| !item.applied));
    assert_eq!(
        std::fs::read_to_string(fixture.project.root.join(MANIFEST))?,
        r#"{"drift":true}"#
    );
    // No trial runs after the conflicted current-line attempt.
    assert_eq!(
        fixture.fake.trials().len(),
        2,
        "{:?}",
        fixture.fake.trials()
    );
    Ok(())
}

/// A preview that bypasses planning selects the same in-line target as a planned run, even
/// though the companion's initial landing floats the held package off the version it moves
/// from.
#[tokio::test]
async fn warm_and_cold_caches_select_the_same_current_line_target() -> eyre::Result<()> {
    let script = || Script {
        holds: vec![peer_held_dom()],
        float: Some(Float {
            trigger: "safe",
            floated: "dom",
            from: "1.0.0",
            to: "1.0.1",
        }),
        ..Script::default()
    };
    let initial = [("dom", "1.0.0"), ("safe", "1.0.0")];

    // Warm: the planner caches the alternative.
    let planned = fixture(
        dom_and_safe(TargetFallback::CurrentLine, script(), true),
        &initial,
    )?;
    let upgrade = planned.workspace.upgrade(&major()).await;
    assert!(upgrade.errors.is_empty(), "{:?}", upgrade.errors);
    assert_eq!(locked(&planned, "dom")?.as_deref(), Some("1.1.0"));

    // Cold: the preview receives preselected targets.
    let previewed = fixture(
        dom_and_safe(TargetFallback::CurrentLine, script(), true),
        &initial,
    )?;
    let context = previewed
        .workspace
        .projects()
        .first()
        .ok_or_else(|| eyre::eyre!("fixture project"))?;
    let change = |name: &str, to: &str, kind| Change {
        package: package(name),
        from: Version::new("1.0.0"),
        to: Version::new(to),
        kind,
        downgrade: false,
        direct: true,
        members: Vec::new(),
    };
    let preview = previewed
        .workspace
        .preview_project_upgrade(
            context,
            &major(),
            &ProjectProgress::default(),
            vec![
                change("dom", "2.0.0", UpdateKind::Major),
                change("safe", "1.1.0", UpdateKind::Minor),
            ],
            PreviewEvidence {
                manifest_only: HashSet::default(),
                advisories: None,
                excluded_members: Vec::new(),
            },
        )
        .await;
    assert!(preview.errors.is_empty(), "{:?}", preview.errors);

    // Both select the in-line target from the pre-lock graph and run the same trials.
    let expected = sorted(vec![
        applied("dom", "1.0.0", "1.1.0"),
        skipped(
            "dom",
            "1.0.0",
            "2.0.0",
            SkipReason::PeerHeld,
            "plugin@1.0.0 peer-requires dom@^1.0.0",
        ),
        applied("safe", "1.0.0", "1.1.0"),
    ]);
    assert_eq!(rows(&upgrade.items), expected);
    assert_eq!(rows(&preview.items), expected);
    let trials = planned.fake.trials();
    assert_eq!(previewed.fake.trials(), trials);
    // The initial trial really moved the held `dom` off `1.0.0`, so evidence read from the
    // trial graph would find no dependency to move from; later trials pin `dom` and never float.
    let floated = |trial: &Vec<String>| trial.iter().any(|entry| entry == "float:dom@1.0.1");
    assert!(trials.first().is_some_and(floated), "{trials:?}");
    assert!(!trials.iter().skip(1).any(floated), "{trials:?}");
    Ok(())
}

/// Two members declare `a` and `c`; both majors are rejected and only `a` has an in-line target,
/// whose landing floats `c` onto `2.0.0` as an unattributed transitive row, the shape the npm
/// adapter's lock diff reports.
/// `straggler` names a member that float leaves on `c@1.0.0`.
fn collateral_major_fixture(straggler: Option<&'static str>) -> eyre::Result<NodeFixture> {
    let fake = FakeNode::new(
        TargetFallback::CurrentLine,
        Script {
            holds: vec![
                // A conservative hold, so only a `c` row can fail `--strict`.
                Hold {
                    target: "a@2.0.0",
                    reason: SkipReason::MultiVersionHeld,
                    offending: None,
                    detail: None,
                },
                Hold {
                    target: "c@2.0.0",
                    reason: SkipReason::ResolverConflict,
                    offending: None,
                    detail: None,
                },
            ],
            float: Some(Float {
                trigger: "a",
                floated: "c",
                from: "1.0.0",
                to: "2.0.0",
            }),
            members: &["web", "api"],
            straggler: straggler.map(|member| Straggler {
                package: "c",
                member,
            }),
            ..Script::default()
        },
    )
    .with_releases("a", &[("1.0.0", false), ("1.1.0", false), ("2.0.0", false)])
    // `c` has no matured release on its current line, so only `a` has an alternative.
    .with_releases("c", &[("1.0.0", false), ("1.1.0", true), ("2.0.0", false)]);
    fixture(fake, &[("a", "1.0.0"), ("c", "1.0.0")])
}

fn strict_major() -> RunOpts {
    RunOpts {
        strict: true,
        ..major()
    }
}

/// A rejected major that a retried sibling floats onto its own target in every declaring member
/// is reported as that landing alone: no held row beside it, so neither the skip count nor
/// `outdated` nor `--strict` reads a target the lock now holds as held.
#[tokio::test]
async fn a_major_landed_as_collateral_is_not_also_reported_held() -> eyre::Result<()> {
    let fixture = collateral_major_fixture(None)?;

    let outcome = fixture.workspace.upgrade(&strict_major()).await;

    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "a")?.as_deref(), Some("1.1.0"));
    assert_eq!(locked(&fixture, "c")?.as_deref(), Some("2.0.0"));
    // `a` reports its in-line move beside its held major; `c` reports only the collateral
    // landing of the very target it was planned at, though that row is transitive and the
    // planned change named both members.
    assert_eq!(
        rows(&outcome.items),
        sorted(vec![
            applied("a", "1.0.0", "1.1.0"),
            skipped(
                "a",
                "1.0.0",
                "2.0.0",
                SkipReason::MultiVersionHeld,
                SkipReason::MultiVersionHeld.message()
            ),
            applied("c", "1.0.0", "2.0.0"),
        ])
    );
    let collateral = outcome
        .items
        .iter()
        .find(|item| item.name == "c" && item.applied)
        .ok_or_else(|| eyre::eyre!("missing collateral row"))?;
    assert!(!collateral.direct);
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.items);
    Ok(())
}

/// A collateral move that leaves one declaring member on the old version has not landed the
/// planned change, so its hold is kept beside the partial landing.
#[tokio::test]
async fn a_major_landed_in_only_some_members_keeps_its_hold() -> eyre::Result<()> {
    let fixture = collateral_major_fixture(Some("api"))?;

    let outcome = fixture.workspace.upgrade(&strict_major()).await;

    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "c")?.as_deref(), Some("2.0.0"));
    assert_eq!(locked(&fixture, "c@api")?.as_deref(), Some("1.0.0"));
    let rows = rows(&outcome.items);
    assert!(rows.contains(&applied("c", "1.0.0", "2.0.0")), "{rows:?}");
    assert!(
        rows.contains(&skipped(
            "c",
            "1.0.0",
            "2.0.0",
            SkipReason::ResolverConflict,
            SkipReason::ResolverConflict.message()
        )),
        "{rows:?}"
    );
    // The kept resolver rejection still fails `--strict`.
    assert_eq!(outcome.exit, Exit::Policy);
    Ok(())
}

/// Each row carries the security provenance of its own target: the in-line move fixes only the
/// advisory it escapes, the held major both.
#[tokio::test]
async fn security_provenance_follows_each_target() -> eyre::Result<()> {
    let mut fake = dom_and_safe(
        TargetFallback::CurrentLine,
        Script {
            holds: vec![peer_held_dom()],
            ..Script::default()
        },
        true,
    );
    fake.advisory_ecosystem = Some("npm");
    let fixture = fixture(fake, &[("dom", "1.0.0")])?;

    let outcome = fixture.workspace.upgrade(&major()).await;

    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "dom")?.as_deref(), Some("1.1.0"));
    let fixes = |to: &str| {
        outcome
            .items
            .iter()
            .find(|item| item.name == "dom" && item.to == to)
            .and_then(|item| item.security.as_ref())
            .map(|security| {
                let mut fixes = security.fixes.clone();
                fixes.sort();
                fixes
            })
    };
    assert_eq!(fixes("1.1.0"), Some(vec!["GHSA-inline".to_string()]));
    assert_eq!(
        fixes("2.0.0"),
        Some(vec!["GHSA-inline".to_string(), "GHSA-major".to_string()])
    );
    Ok(())
}

/// A lone major the residual gate rejects (it forces a fresh transitive no reconcile can move)
/// still lands its in-line target, and the major reports the transitive that blocks it.
#[tokio::test]
async fn singleton_policy_blocked_major_lands_its_current_line_target() -> eyre::Result<()> {
    let fake = dom_and_safe(
        TargetFallback::CurrentLine,
        Script {
            fresh_child_on_major: true,
            ..Script::default()
        },
        true,
    )
    .with_releases("child", &[("1.0.0", false), ("1.1.0", true)]);
    let fixture = fixture(fake, &[("child", "1.0.0"), ("dom", "1.0.0")])?;

    let outcome = fixture.workspace.upgrade(&major()).await;

    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "dom")?.as_deref(), Some("1.1.0"));
    assert_eq!(locked(&fixture, "child")?.as_deref(), Some("1.0.0"));
    let rows = rows(&outcome.items);
    assert!(rows.contains(&applied("dom", "1.0.0", "1.1.0")), "{rows:?}");
    let major = rows
        .iter()
        .find(|row| row.to == "2.0.0")
        .ok_or_else(|| eyre::eyre!("missing held major row: {rows:?}"))?;
    assert!(!major.applied);
    assert_eq!(major.reason, Some(SkipReason::TransitiveInCooldown));
    Ok(())
}
