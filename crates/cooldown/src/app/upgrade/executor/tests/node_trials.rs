//! File-backed Node-family trials exercise the current-line fallback: a held cross-major target
//! retries the highest adoptable target on the current major line through the same rollback and
//! replay the lower-line fallback uses.

use crate::app::upgrade::PreviewEvidence;
use crate::app::{
    AdapterSet, Baseline, Exit, ProjectCtx, ProjectProgress, RunOpts, TransitiveGate, Workspace,
};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre;
use cooldown_core::config::builtin_default_layer;
use cooldown_core::{
    AdvisoryFetch, AdvisoryPolicy, AdvisorySource, AdvisorySourceId, ApplyReport, CandidateScope,
    Capabilities, Change, CoreError, DepScope, Dependency, FetchContext, GeneratedMembers,
    GraphHoldEdge, GraphHoldKind, JudgedPin, LockStatus, LockVerifyReport, LockedVersion, MajorKey,
    MemberRef, NativeAdmissionQuery, NativeAssessment, NativeEdit, NativeGateQuery,
    NativeGateRefusal, NativePolicyLayer, PackageAdvisories, PackageId, Plan, PolicyLayer,
    PolicyStack, PreparedMutation, Project, ProjectMarker, ProjectMutationJournal, RawAdvisory,
    Release, ReleaseFetcher, ReleaseOrder, ReleaseQuality, ResolveInputs, SkipReason, Skipped,
    TargetFallback, ToolId, ToolRead, ToolWrite, UpdateKind, VerifyReport, Version,
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

/// The package name of a lock key: a `<name>#<copy>` key holds a further copy of `<name>`.
fn copy_name(key: &str) -> &str {
    key.split_once('#').map_or(key, |(name, _)| name)
}

/// The lock key of `change`'s own copy: the one whose name is `change`'s and whose version is its
/// source, or the bare name when no copy holds that version.
fn copy_slot(lock: &BTreeMap<String, String>, change: &Change) -> String {
    lock.iter()
        .find(|(key, version)| {
            copy_name(key) == change.package.name && version.as_str() == change.from.as_str()
        })
        .map_or_else(|| change.package.name.clone(), |(key, _)| key.clone())
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

/// One copy's move the resolver leaves short of its target, matched by its source version, while
/// another copy of the same name may still land on that target.
struct HeldCopy {
    name: &'static str,
    from: &'static str,
    to: &'static str,
}

impl HeldCopy {
    fn matches(&self, change: &Change) -> bool {
        self.name == change.package.name
            && self.from == change.from.as_str()
            && self.to == change.to.as_str()
    }
}

/// A batch landing a planned move of `trigger` also moves `copy` onto its target for only some of
/// its dependents, the way a pnpm resolve can.
/// Like pnpm, the fake reports that move as the copy's own row only while the batch neither
/// plans the copy's move nor withholds it, since otherwise the move's own verdict stands.
struct PartialFloat {
    trigger: &'static str,
    copy: HeldCopy,
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
    held_copies: Vec<HeldCopy>,
    partial_float: Option<PartialFloat>,
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

/// A native exemption list the fake keeps beside its lock, the way pnpm keeps
/// `minimumReleaseAgeExclude` in pnpm-workspace.yaml.
#[derive(Default)]
struct NativeList {
    /// The admitted packages whose locked version an assessment grants.
    grant: &'static [&'static str],
    /// The packages whose origin an assessment cannot prove, so it grants their admitted versions
    /// nothing, while a version the native gate refused in the starting lock needs no such proof.
    unproven: &'static [&'static str],
    /// Whether the assessed edit names a file the journal does not hold, so writing it fails.
    write_fails: bool,
    /// The `name@version` the native backstop refuses while the lock holds it and the list does
    /// not exempt it, if any.
    backstop_refuses: Option<&'static str>,
    /// Whether the native backstop and the final lock check refuse even a version the list
    /// exempts, as pnpm does when a setting above the file overrides the list.
    backstop_ignores_list: bool,
    /// Whether the verification of the starting lock fails, as one whose refusals pnpm cannot
    /// all name does.
    starting_probe_fails: bool,
    /// Whether the verification of the starting lock rewrites it with the same entries spelled
    /// otherwise, as pnpm's frozen lockfile-only run can.
    starting_probe_reformats_lock: bool,
    /// The `name@version` the final lock check requires the list to exempt, as a bare pnpm
    /// refuses a lock holding that young version otherwise, and every time under
    /// [`backstop_ignores_list`](Self::backstop_ignores_list).
    finalize_requires: Option<&'static str>,
    /// Whether the native backstop rewrites the lock with the same entries spelled otherwise
    /// before its verdict, as a pnpmfile hook can during a verification.
    backstop_reformats_lock: bool,
    /// Whether the manifest is edited while the native backstop runs, as someone else's edit
    /// would be, which no verification writes.
    backstop_edits_manifest: bool,
}

/// The manifest someone else writes while the native backstop runs.
const EDITED_MANIFEST: &str = r#"{"name":"app","edited":true}"#;

/// The fake's native exemption list file.
const NATIVE: &str = "native.yaml";

struct FakeNode {
    fallback: TargetFallback,
    /// Whether the fake can pin an undeclared package, so `upgrade` plans transitive copies too.
    transitive_advance: bool,
    script: Script,
    releases: BTreeMap<String, Vec<Release>>,
    advisory_ecosystem: Option<&'static str>,
    /// Each apply's plan as `name@target` entries, kept outside the project because a policy
    /// preview runs in a throwaway copy.
    trials: Mutex<Vec<Vec<String>>>,
    shared: Option<SharedFile>,
    native: Option<NativeList>,
    /// The order of the applies and native calls, as `apply`, `assess`, and `backstop`.
    events: Mutex<Vec<&'static str>>,
    /// What the last native backstop saw.
    backstop_saw: Mutex<Option<BackstopView>>,
    /// The lock bytes each apply found.
    apply_saw_lock: Mutex<Vec<Vec<u8>>>,
}

/// The project a native backstop judged, and the native list it held.
#[derive(Clone)]
struct BackstopView {
    root: Utf8PathBuf,
    list: Option<String>,
}

/// A file outside the project that every apply rewrites, the way `go get` rewrites the `go.work`
/// a Go workspace's modules share.
struct SharedFile {
    path: Utf8PathBuf,
    /// Whether the fake names the file for the run to lease.
    leased: bool,
}

impl FakeNode {
    fn new(fallback: TargetFallback, script: Script) -> Self {
        FakeNode {
            fallback,
            transitive_advance: false,
            script,
            releases: BTreeMap::new(),
            advisory_ecosystem: None,
            trials: Mutex::new(Vec::new()),
            shared: None,
            native: None,
            events: Mutex::new(Vec::new()),
            backstop_saw: Mutex::new(None),
            apply_saw_lock: Mutex::new(Vec::new()),
        }
    }

    fn apply_saw_lock(&self) -> Vec<Vec<u8>> {
        self.apply_saw_lock.lock().expect("apply log").clone()
    }

    fn backstop_saw(&self) -> Option<BackstopView> {
        self.backstop_saw.lock().expect("backstop view").clone()
    }

    fn with_native(mut self, native: NativeList) -> Self {
        self.native = Some(native);
        self
    }

    fn events(&self) -> Vec<&'static str> {
        self.events.lock().expect("event log").clone()
    }

    fn record(&self, event: &'static str) {
        self.events.lock().expect("event log").push(event);
    }

    fn with_transitive_advance(mut self) -> Self {
        self.transitive_advance = true;
        self
    }

    fn with_shared_file(mut self, path: Utf8PathBuf, leased: bool) -> Self {
        self.shared = Some(SharedFile { path, leased });
        self
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

    /// The skip the resolver reports for `change` instead of landing it, if any; `earlier` counts
    /// the trials before this one that carried the target refused after the first.
    fn refusal(&self, change: &Change, earlier: usize) -> Option<Skipped> {
        let target = format!("{}@{}", change.package.name, change.to);
        if self
            .script
            .held_copies
            .iter()
            .any(|held| held.matches(change))
        {
            return Some(Skipped {
                change: change.clone(),
                reason: SkipReason::GraphCeilingHeld,
                offending: Some(package("keeper")),
                detail: None,
            });
        }
        if let Some(hold) = self.script.holds.iter().find(|hold| hold.target == target) {
            return Some(Skipped {
                change: change.clone(),
                reason: hold.reason,
                offending: hold.offending.map(package),
                detail: hold.detail.map(str::to_string),
            });
        }
        (self.script.refused_after_first == Some(target.as_str()) && earlier > 0).then(|| Skipped {
            change: change.clone(),
            reason: SkipReason::ResolverConflict,
            offending: None,
            detail: Some("the registry moved between trials".to_string()),
        })
    }

    /// The row of the scripted partial float, when the batch landed a planned move of its trigger
    /// and neither plans nor withholds the floated copy's move.
    fn partial_float_row(&self, mutation: &PreparedMutation, applied: &[Change]) -> Option<Change> {
        let PartialFloat { trigger, copy } = self.script.partial_float.as_ref()?;
        let (_, plan, _) = mutation.parts_for(self).ok()?;
        let triggered = applied
            .iter()
            .any(|row| row.package.name == *trigger && plan.changes.contains(row));
        let judged = plan
            .changes
            .iter()
            .chain(mutation.withheld())
            .any(|change| copy.matches(change));
        (triggered && !judged).then(|| Change {
            package: package(copy.name),
            from: Version::new(copy.from),
            to: Version::new(copy.to),
            kind: UpdateKind::Major,
            downgrade: false,
            direct: false,
            members: Vec::new(),
        })
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
            let name = copy_name(name);
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
        project: &Project,
    ) -> cooldown_core::Result<LockVerifyReport> {
        let required = self
            .native
            .as_ref()
            .and_then(|native| native.finalize_requires);
        if let Some(required) = required {
            let listed = std::fs::read_to_string(project.root.join(NATIVE)).unwrap_or_default();
            let ignores_list = self
                .native
                .as_ref()
                .is_some_and(|native| native.backstop_ignores_list);
            if ignores_list || !listed.lines().any(|entry| entry == required) {
                return Ok(LockVerifyReport {
                    status: LockStatus::Stale,
                    detail: format!("{required} was published within the minimumReleaseAge cutoff"),
                });
            }
        }
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

    fn supports_transitive_advance(&self) -> bool {
        self.transitive_advance
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> cooldown_core::Result<ProjectMutationJournal> {
        let native = self.native.as_ref().map(|_| Utf8Path::new(NATIVE));
        ProjectMutationJournal::capture_with_external(
            &project.root,
            [Utf8Path::new(LOCKFILE), Utf8Path::new(MANIFEST)]
                .into_iter()
                .chain(native),
            self.shared.iter().map(|shared| &shared.path),
        )
    }

    fn accepts_native_admissions(&self, _project: &Project) -> bool {
        self.native.is_some()
    }

    async fn assess_native_admissions(
        &self,
        query: NativeAdmissionQuery<'_>,
    ) -> cooldown_core::Result<NativeAssessment> {
        let Some(native) = &self.native else {
            return Ok(NativeAssessment::default());
        };
        self.record("assess");
        let locked = |pin: &&JudgedPin| LockedVersion {
            name: pin.name.clone(),
            version: pin.version.clone(),
        };
        let grants: Vec<LockedVersion> = query
            .pins
            .iter()
            .filter(|pin| {
                let name = pin.name.as_str();
                (pin.admitted && native.grant.contains(&name) && !native.unproven.contains(&name))
                    || query.refused_at_start.contains(&locked(pin))
            })
            .map(|pin| locked(&pin))
            .collect();
        let list: Vec<String> = grants
            .iter()
            .map(|grant| format!("{}@{}", grant.name, grant.version))
            .collect();
        // A failing write is one into a file the journal does not hold.
        let path = if native.write_fails {
            "outside.yaml"
        } else {
            NATIVE
        };
        Ok(NativeAssessment {
            edit: (!grants.is_empty()).then(|| NativeEdit {
                path: Utf8PathBuf::from(path),
                contents: list.join("\n").into_bytes(),
            }),
            grants,
        })
    }

    fn resolve_inputs(&self) -> ResolveInputs {
        ResolveInputs {
            filenames: &[LOCKFILE, MANIFEST, NATIVE],
            path_prefixes: &[],
            source_extensions: &[],
        }
    }

    fn native_gate_writes(&self, _project: &Project) -> Vec<Utf8PathBuf> {
        if self.native.is_some() {
            vec![Utf8PathBuf::from(LOCKFILE)]
        } else {
            Vec::new()
        }
    }

    async fn native_gate_refusal(
        &self,
        query: NativeGateQuery<'_>,
    ) -> cooldown_core::Result<Option<NativeGateRefusal>> {
        let Some(native) = &self.native else {
            return Ok(None);
        };
        let project = query.project;
        // The run first asks about the lock it starts from; only a later verification follows an
        // apply, and only that one rewrites or edits anything.
        let after_apply = self.events().contains(&"apply");
        self.record("backstop");
        *self.backstop_saw.lock().expect("backstop view") = Some(BackstopView {
            root: project.root.clone(),
            list: std::fs::read_to_string(project.root.join(NATIVE)).ok(),
        });
        if !after_apply && native.starting_probe_fails {
            return Err(CoreError::System(
                "pnpm refused only versions its command-line minimumReleaseAgeExclude already \
                 exempts"
                    .to_string(),
            ));
        }
        if !after_apply && native.starting_probe_reformats_lock {
            std::fs::write(
                project.root.join(LOCKFILE),
                serde_json::to_vec_pretty(&lock(project)?)
                    .map_err(|error| CoreError::Serialization(error.to_string()))?,
            )?;
        }
        if after_apply && native.backstop_edits_manifest {
            std::fs::write(project.root.join(MANIFEST), EDITED_MANIFEST)?;
        }
        if after_apply && native.backstop_reformats_lock {
            std::fs::write(
                project.root.join(LOCKFILE),
                serde_json::to_vec_pretty(&lock(project)?)
                    .map_err(|error| CoreError::Serialization(error.to_string()))?,
            )?;
        }
        let held = lock(project)?;
        let listed = std::fs::read_to_string(project.root.join(NATIVE)).unwrap_or_default();
        Ok(native.backstop_refuses.and_then(|refused| {
            let (name, version) = refused.split_once('@')?;
            let locked = held
                .iter()
                .any(|(key, held)| copy_name(key) == name && held == version);
            let exempt =
                !native.backstop_ignores_list && listed.lines().any(|entry| entry == refused);
            (locked && !exempt).then(|| NativeGateRefusal {
                versions: vec![LockedVersion {
                    name: name.to_string(),
                    version: Version::new(version),
                }],
                detail: format!("{refused} was published within the minimumReleaseAge cutoff"),
            })
        }))
    }

    async fn shared_write_files(
        &self,
        _project: &Project,
    ) -> cooldown_core::Result<Vec<Utf8PathBuf>> {
        Ok(self
            .shared
            .iter()
            .filter(|shared| shared.leased)
            .map(|shared| shared.path.clone())
            .collect())
    }

    async fn apply(&self, mutation: &PreparedMutation) -> cooldown_core::Result<ApplyReport> {
        self.record("apply");
        let (project, plan, _) = mutation.parts_for(self)?;
        self.apply_saw_lock
            .lock()
            .expect("apply log")
            .push(std::fs::read(project.root.join(LOCKFILE))?);
        let mut versions = lock(project)?;
        let targets: Vec<String> = plan
            .changes
            .iter()
            .map(|change| format!("{}@{}", change.package.name, change.to))
            .collect();
        if let Some(shared) = &self.shared {
            std::fs::write(&shared.path, targets.join(","))?;
        }
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
        // Each change moves its own copy, the one the starting lock holds at its source version.
        let starting = versions.clone();
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            if let Some(refusal) = self.refusal(change, earlier) {
                report.skipped.push(refusal);
                continue;
            }
            versions.insert(copy_slot(&starting, change), change.to.to_string());
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
        if let Some(row) = self.partial_float_row(mutation, &report.applied) {
            report.applied.push(row);
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

/// An exact manifest pin that excludes the major can still admit the in-line target, so unlike a
/// not-eligible hold the major's current-line target is tried and lands beside the pin-held row,
/// and the forward pin hold does not fail `--strict`.
#[tokio::test]
async fn a_pin_held_major_lands_its_current_line_target() -> eyre::Result<()> {
    let detail = "the exact pin 1.1.0 in devDependencies of apps/pinned/package.json excludes \
                  2.0.0; pass --rewrite to rewrite it, or edit the pin";
    let fixture = fixture(
        dom_and_safe(
            TargetFallback::CurrentLine,
            Script {
                holds: vec![Hold {
                    target: "dom@2.0.0",
                    reason: SkipReason::PinHeld,
                    offending: None,
                    detail: Some(detail),
                }],
                ..Script::default()
            },
            true,
        ),
        &[("dom", "1.0.0")],
    )?;

    let outcome = fixture
        .workspace
        .upgrade(&RunOpts {
            strict: true,
            ..major()
        })
        .await;

    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "dom")?.as_deref(), Some("1.1.0"));
    assert_eq!(
        rows(&outcome.items),
        sorted(vec![
            applied("dom", "1.0.0", "1.1.0"),
            skipped("dom", "1.0.0", "2.0.0", SkipReason::PinHeld, detail),
        ])
    );
    // The refused major, then its in-line target, which the replay commits.
    let trials = fixture.fake.trials();
    assert!(
        trials
            .iter()
            .skip(1)
            .any(|trial| trial.iter().any(|target| target == "dom@1.1.0")),
        "{trials:?}"
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

/// `safe` pinned at `1.0.0` with a matured `1.1.0` the resolver refuses, and a shared file
/// outside the project holding `original`.
fn refused_safe_with_shared_file(
    leased: bool,
) -> eyre::Result<(tempfile::TempDir, Utf8PathBuf, NodeFixture)> {
    let outside = tempfile::tempdir()?;
    let shared = Utf8PathBuf::from_path_buf(std::fs::canonicalize(outside.path())?)
        .map_err(|path| eyre::eyre!("non-UTF-8 root: {}", path.display()))?
        .join("shared.work");
    std::fs::write(&shared, "original")?;
    let fake = FakeNode::new(
        TargetFallback::None,
        Script {
            holds: vec![Hold {
                target: "safe@1.1.0",
                reason: SkipReason::ResolverConflict,
                offending: None,
                detail: Some("the resolver refuses safe@1.1.0"),
            }],
            ..Script::default()
        },
    )
    .with_releases("safe", &[("1.0.0", false), ("1.1.0", false)])
    .with_shared_file(shared.clone(), leased);
    let fixture = fixture(fake, &[("safe", "1.0.0")])?;
    Ok((outside, shared, fixture))
}

/// A trial the executor rejects after the apply returned restores the shared file the apply
/// rewrote, not only the project's own files.
#[tokio::test]
async fn a_rejected_trial_restores_a_leased_shared_file() -> eyre::Result<()> {
    let (_outside, shared, fixture) = refused_safe_with_shared_file(true)?;

    let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    // The refused target was tried, which rewrote the shared file, and then rolled back.
    assert_ne!(fixture.fake.trials(), Vec::<Vec<String>>::new());
    assert_eq!(locked(&fixture, "safe")?.as_deref(), Some("1.0.0"));
    assert_eq!(std::fs::read_to_string(&shared)?, "original");
    Ok(())
}

/// A journal that reaches a shared file the run does not lease is refused before any trial, so
/// two cooldown processes never trial one shared file at once.
#[tokio::test]
async fn a_shared_file_the_run_does_not_lease_is_refused() -> eyre::Result<()> {
    let (_outside, shared, fixture) = refused_safe_with_shared_file(false)?;

    let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

    assert!(
        outcome
            .errors
            .iter()
            .any(|error| error.message.contains("which this run does not hold")),
        "{:?}",
        outcome.errors
    );
    assert_eq!(fixture.fake.trials(), Vec::<Vec<String>>::new());
    assert_eq!(std::fs::read_to_string(&shared)?, "original");
    Ok(())
}

/// A dry run trials a throwaway copy, so a shared file outside that copy is the real one, and
/// the trial is refused rather than allowed to write it.
#[tokio::test]
async fn a_dry_run_refuses_a_shared_file_outside_its_copy() -> eyre::Result<()> {
    let (_outside, shared, fixture) = refused_safe_with_shared_file(true)?;

    let outcome = fixture
        .workspace
        .upgrade(&RunOpts {
            dry_run: true,
            ..RunOpts::default()
        })
        .await;

    assert!(
        outcome
            .errors
            .iter()
            .any(|error| error.message.contains("which this run does not hold")),
        "{:?}",
        outcome.errors
    );
    assert_eq!(fixture.fake.trials(), Vec::<Vec<String>>::new());
    assert_eq!(std::fs::read_to_string(&shared)?, "original");
    Ok(())
}

/// Two copies of the undeclared `child`, at `1.2.0` and `1.3.0`, both planned onto the matured
/// `1.4.0`, with the resolver leaving the older copy's move short of it.
fn converging_copies(fallback: TargetFallback) -> eyre::Result<NodeFixture> {
    let fake = FakeNode::new(
        fallback,
        Script {
            held_copies: vec![HeldCopy {
                name: "child",
                from: "1.2.0",
                to: "1.4.0",
            }],
            ..Script::default()
        },
    )
    .with_transitive_advance()
    .with_releases(
        "child",
        &[("1.2.0", false), ("1.3.0", false), ("1.4.0", false)],
    );
    fixture(fake, &[("child", "1.2.0"), ("child#newer", "1.3.0")])
}

/// A held copy and a landed sibling on the same target are two changes: the sibling's move
/// commits and is reported applied, and the held copy is reported held without being credited
/// with the sibling's landing.
#[tokio::test]
async fn a_held_copy_and_its_landed_sibling_are_credited_separately() -> eyre::Result<()> {
    let fixture = converging_copies(TargetFallback::CurrentLine)?;

    let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    // The batch committed with the sibling's move, and the held copy stayed where it was.
    assert_eq!(locked(&fixture, "child")?.as_deref(), Some("1.2.0"));
    assert_eq!(locked(&fixture, "child#newer")?.as_deref(), Some("1.4.0"));
    // One row per copy: the sibling's landing, and the held copy's hold with no applied row
    // borrowed from the sibling.
    assert_eq!(
        rows(&outcome.items),
        sorted(vec![
            applied("child", "1.3.0", "1.4.0"),
            skipped(
                "child",
                "1.2.0",
                "1.4.0",
                SkipReason::GraphCeilingHeld,
                "held: conflicts with keeper"
            ),
        ])
    );
    Ok(())
}

/// Once the held copy is told apart from its landed sibling, a writer with a lower-line fallback
/// retries the held copy at an older matured target on top of the sibling's landing, and both
/// moves commit.
#[tokio::test]
async fn a_held_copy_falls_back_beside_its_landed_sibling() -> eyre::Result<()> {
    let fixture = converging_copies(TargetFallback::LowerOnSelectedLine)?;

    let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "child")?.as_deref(), Some("1.3.0"));
    assert_eq!(locked(&fixture, "child#newer")?.as_deref(), Some("1.4.0"));
    // The held copy lands its fallback target, so it has no held row left.
    assert_eq!(
        rows(&outcome.items),
        sorted(vec![
            applied("child", "1.2.0", "1.3.0"),
            applied("child", "1.3.0", "1.4.0"),
        ])
    );
    Ok(())
}

/// A held copy whose current-line retry is held too reaches the replay of its landed sibling,
/// which moves the held copy onto the shared target for only some of its dependents.
/// The replay withholds the held copy from the writer, so that partial move never reads as the
/// held copy landing, and the copy keeps its hold.
#[tokio::test]
async fn a_held_copy_keeps_its_hold_when_the_sibling_replay_moves_it_partly() -> eyre::Result<()> {
    let held = |to| HeldCopy {
        name: "dom",
        from: "1.0.0",
        to,
    };
    let fake = FakeNode::new(
        TargetFallback::CurrentLine,
        Script {
            held_copies: vec![held("2.0.0"), held("1.9.0")],
            partial_float: Some(PartialFloat {
                trigger: "dom",
                copy: held("2.0.0"),
            }),
            ..Script::default()
        },
    )
    .with_releases(
        "dom",
        &[
            ("1.0.0", false),
            ("1.5.0", false),
            ("1.9.0", false),
            ("2.0.0", false),
        ],
    );
    let fixture = fixture(fake, &[("dom", "1.0.0"), ("dom#newer", "1.5.0")])?;

    let outcome = fixture.workspace.upgrade(&major()).await;

    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "dom")?.as_deref(), Some("1.0.0"));
    assert_eq!(locked(&fixture, "dom#newer")?.as_deref(), Some("2.0.0"));
    // The sibling's landing commits, and the older copy keeps its own hold with no applied row
    // standing in for it.
    assert_eq!(
        rows(&outcome.items),
        sorted(vec![
            applied("dom", "1.5.0", "2.0.0"),
            skipped(
                "dom",
                "1.0.0",
                "2.0.0",
                SkipReason::GraphCeilingHeld,
                "held: conflicts with keeper"
            ),
        ])
    );
    // The initial trial, the held copy's current-line retry beside the sibling, and the replay.
    assert_eq!(
        fixture.fake.trials().len(),
        3,
        "{:?}",
        fixture.fake.trials()
    );
    Ok(())
}

/// A reconciling batch that moves a held copy onto its target for only some of its dependents
/// lands nothing of that copy: every trial withholds its own changes from its reconciling
/// batches, so the copy keeps its hold beside its sibling's landing.
#[tokio::test]
async fn a_held_copy_keeps_its_hold_when_reconciliation_moves_it_partly() -> eyre::Result<()> {
    let held = |to| HeldCopy {
        name: "dom",
        from: "1.0.0",
        to,
    };
    // The sibling's landing floats `child` onto its fresh `1.1.0`, whose reconciliation back to
    // `1.0.0` is the batch that moves the held copy partly.
    let fake = FakeNode::new(
        TargetFallback::CurrentLine,
        Script {
            held_copies: vec![held("2.0.0"), held("1.9.0")],
            partial_float: Some(PartialFloat {
                trigger: "child",
                copy: held("2.0.0"),
            }),
            float: Some(Float {
                trigger: "dom",
                floated: "child",
                from: "1.0.0",
                to: "1.1.0",
            }),
            ..Script::default()
        },
    )
    .with_releases(
        "dom",
        &[
            ("1.0.0", false),
            ("1.5.0", false),
            ("1.9.0", false),
            ("2.0.0", false),
        ],
    )
    .with_releases("child", &[("1.0.0", false), ("1.1.0", true)]);
    let fixture = fixture(
        fake,
        &[("child", "1.0.0"), ("dom", "1.0.0"), ("dom#newer", "1.5.0")],
    )?;

    let outcome = fixture.workspace.upgrade(&major()).await;

    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "child")?.as_deref(), Some("1.0.0"));
    assert_eq!(locked(&fixture, "dom")?.as_deref(), Some("1.0.0"));
    assert_eq!(locked(&fixture, "dom#newer")?.as_deref(), Some("2.0.0"));
    // The held copy is never credited, so it keeps its own hold beside the sibling's landing.
    let rows = rows(&outcome.items);
    assert!(rows.contains(&applied("dom", "1.5.0", "2.0.0")), "{rows:?}");
    assert!(
        rows.contains(&skipped(
            "dom",
            "1.0.0",
            "2.0.0",
            SkipReason::GraphCeilingHeld,
            "held: conflicts with keeper"
        )),
        "{rows:?}"
    );
    assert!(
        !rows.contains(&applied("dom", "1.0.0", "2.0.0")),
        "{rows:?}"
    );
    Ok(())
}

/// `safe` moves to its matured `1.1.0` under a fake that keeps a native exemption list.
fn native_safe(native: NativeList, float: Option<Float>) -> eyre::Result<NodeFixture> {
    let fake = FakeNode::new(
        TargetFallback::None,
        Script {
            float,
            ..Script::default()
        },
    )
    .with_releases("safe", &[("1.0.0", false), ("1.1.0", false)])
    .with_releases("child", &[("1.0.0", false), ("1.1.0", true)])
    .with_native(native);
    let fixture = fixture(fake, &[("child", "1.0.0"), ("safe", "1.0.0")])?;
    std::fs::write(fixture.project.root.join(NATIVE), "before")?;
    Ok(fixture)
}

/// `safe`'s landing floats the transitive `child` onto its cooling `1.1.0`.
fn fresh_child_float() -> Float {
    Float {
        trigger: "safe",
        floated: "child",
        from: "1.0.0",
        to: "1.1.0",
    }
}

/// A committed batch is assessed after its apply, and its assessed list is written before the
/// native gate judges the settled project itself, so the gate's refusal rolls the written list
/// back with the lock, as cooldown's own write rather than a conflict, while the candidate is
/// held by the version the gate refused, as a cooldown violation would hold it.
#[tokio::test]
async fn a_native_backstop_refusal_rolls_the_written_list_back_and_holds_the_candidate()
-> eyre::Result<()> {
    let fixture = native_safe(
        NativeList {
            grant: &["safe"],
            backstop_refuses: Some("safe@1.1.0"),
            backstop_ignores_list: true,
            ..NativeList::default()
        },
        None,
    )?;
    let lock_before = std::fs::read(fixture.project.root.join(LOCKFILE))?;

    let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(
        fixture.fake.events(),
        ["backstop", "apply", "assess", "backstop"]
    );
    // The backstop judged the project itself, which already held the written list.
    let seen = fixture.fake.backstop_saw().expect("the backstop ran");
    assert_eq!(seen.root, fixture.project.root);
    assert_eq!(seen.list.as_deref(), Some("safe@1.1.0"));
    assert_eq!(
        std::fs::read(fixture.project.root.join(LOCKFILE))?,
        lock_before
    );
    assert_eq!(
        std::fs::read_to_string(fixture.project.root.join(NATIVE))?,
        "before"
    );
    let [held] = outcome.items.as_slice() else {
        panic!("one held row: {:?}", outcome.items);
    };
    let skipped = held.skipped.as_ref().expect("the candidate is held");
    assert_eq!(skipped.reason, SkipReason::TransitiveInCooldown);
    assert_eq!(skipped.offending.as_deref(), Some("safe"));
    Ok(())
}

/// The backstop is cooldown's own step, like the resolve: a verification that rewrites the lock
/// and accepts it lands the trial with the verification's bytes, while one that rewrites it and
/// refuses restores the lock from before the trial and holds the candidate.
#[tokio::test]
async fn a_backstop_that_rewrites_the_lock_owns_its_bytes() -> eyre::Result<()> {
    for refuses in [None, Some("safe@1.1.0")] {
        let fixture = native_safe(
            NativeList {
                grant: &["safe"],
                backstop_refuses: refuses,
                backstop_ignores_list: true,
                backstop_reformats_lock: true,
                ..NativeList::default()
            },
            None,
        )?;
        let lock_before = std::fs::read(fixture.project.root.join(LOCKFILE))?;

        let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

        assert!(
            outcome.errors.is_empty(),
            "{refuses:?}: {:?}",
            outcome.errors
        );
        assert_eq!(
            fixture.fake.events(),
            ["backstop", "apply", "assess", "backstop"]
        );
        let lock_after = std::fs::read(fixture.project.root.join(LOCKFILE))?;
        let [row] = outcome.items.as_slice() else {
            panic!("one row: {:?}", outcome.items);
        };
        if refuses.is_some() {
            assert_eq!(lock_after, lock_before);
            let held = row.skipped.as_ref().expect("the candidate is held");
            assert_eq!(held.reason, SkipReason::TransitiveInCooldown);
        } else {
            assert!(row.applied, "{row:?}");
            assert_eq!(locked(&fixture, "safe")?.as_deref(), Some("1.1.0"));
            // The verification's spelling of the lock is what landed.
            let reformatted = serde_json::to_vec_pretty(&lock(&fixture.project)?)?;
            assert_eq!(lock_after, reformatted);
        }
    }
    Ok(())
}

/// Only the lock is the backstop's to adopt: a manifest edited while it runs is someone else's
/// change, so the refused trial's restore stops at it, reports the conflict, and leaves the edit
/// in place rather than restoring over it.
#[tokio::test]
async fn a_backstop_restore_never_writes_over_an_edit_made_during_it() -> eyre::Result<()> {
    let fixture = native_safe(
        NativeList {
            grant: &["safe"],
            backstop_refuses: Some("safe@1.1.0"),
            backstop_ignores_list: true,
            backstop_edits_manifest: true,
            ..NativeList::default()
        },
        None,
    )?;

    let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

    assert_eq!(
        fixture.fake.events(),
        ["backstop", "apply", "assess", "backstop"]
    );
    assert_eq!(
        std::fs::read_to_string(fixture.project.root.join(MANIFEST))?,
        EDITED_MANIFEST
    );
    assert!(
        outcome
            .errors
            .iter()
            .chain(outcome.items.iter().filter_map(|item| item.error.as_ref()))
            .any(|error| error.message.contains("changed independently")),
        "{:?}",
        outcome.errors
    );
    Ok(())
}

/// `fix` has no trial isolation, so the native gate judges each round before it is accepted: a
/// `--transitive allow` round whose young transitive the gate refuses is restored and held by
/// that transitive, quoting the tool, and the warning that the graph gate left the transitive in
/// place goes with it.
#[tokio::test]
async fn a_restored_fix_round_drops_its_left_in_place_warning() -> eyre::Result<()> {
    let fake = FakeNode::new(
        TargetFallback::None,
        Script {
            float: Some(fresh_child_float()),
            ..Script::default()
        },
    )
    .with_releases("safe", &[("1.0.0", false), ("1.1.0", true)])
    .with_releases("child", &[("1.0.0", false), ("1.1.0", true)])
    .with_native(NativeList {
        backstop_refuses: Some("child@1.1.0"),
        ..NativeList::default()
    });
    let fixture = fixture(fake, &[("child", "1.0.0"), ("safe", "1.1.0")])?;
    let allow = RunOpts {
        transitive_mode: TransitiveGate::Allow,
        ..RunOpts::default()
    };

    let outcome = fixture.workspace.fix(&allow).await;

    assert_eq!(locked(&fixture, "safe")?.as_deref(), Some("1.1.0"));
    assert_eq!(locked(&fixture, "child")?.as_deref(), Some("1.0.0"));
    assert!(
        !outcome
            .warnings
            .iter()
            .any(|warning| warning.message.contains("left in place")),
        "{:?}",
        outcome.warnings
    );
    let held = outcome
        .items
        .iter()
        .find_map(|item| item.skipped.as_ref())
        .expect("the downgrade is held");
    assert_eq!(held.reason, SkipReason::TransitiveInCooldown);
    assert_eq!(held.offending.as_deref(), Some("child"));
    assert!(
        held.message
            .contains("child@1.1.0 was published within the minimumReleaseAge cutoff"),
        "{}",
        held.message
    );
    Ok(())
}

/// An accepted backstop commits the list the batch wrote with its lock.
#[tokio::test]
async fn an_accepted_batch_keeps_its_native_list() -> eyre::Result<()> {
    let fixture = native_safe(
        NativeList {
            grant: &["safe"],
            ..NativeList::default()
        },
        None,
    )?;

    let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "safe")?.as_deref(), Some("1.1.0"));
    assert_eq!(
        std::fs::read_to_string(fixture.project.root.join(NATIVE))?,
        "safe@1.1.0"
    );
    Ok(())
}

/// A native write that fails restores the batch and reports the failure as the error it is,
/// never as a hold.
#[tokio::test]
async fn a_failed_native_write_restores_the_batch_as_an_error() -> eyre::Result<()> {
    let fixture = native_safe(
        NativeList {
            grant: &["safe"],
            write_fails: true,
            ..NativeList::default()
        },
        None,
    )?;
    let lock_before = std::fs::read(fixture.project.root.join(LOCKFILE))?;

    let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

    assert_eq!(fixture.fake.events(), ["backstop", "apply", "assess"]);
    assert_eq!(
        std::fs::read(fixture.project.root.join(LOCKFILE))?,
        lock_before
    );
    assert_eq!(
        std::fs::read_to_string(fixture.project.root.join(NATIVE))?,
        "before"
    );
    let failed = outcome
        .items
        .iter()
        .find_map(|item| item.error.as_ref())
        .expect("the candidate reports the write failure");
    assert!(failed.message.contains("outside.yaml"), "{failed:?}");
    assert!(outcome.items.iter().all(|item| !item.applied));
    assert!(
        !outcome
            .errors
            .iter()
            .chain(outcome.items.iter().filter_map(|item| item.error.as_ref()))
            .any(|error| error.message.contains("changed independently")),
        "the rollback takes back cooldown's own write: {:?}",
        outcome.errors
    );
    Ok(())
}

/// `--transitive allow` leaves a floated-up young transitive in place only while the native gate
/// accepts it: one the native gate refuses holds the candidate that drags it in.
#[tokio::test]
async fn transitive_allow_holds_a_candidate_whose_young_transitive_the_native_gate_refuses()
-> eyre::Result<()> {
    let allow = RunOpts {
        transitive_mode: TransitiveGate::Allow,
        ..RunOpts::default()
    };
    let tolerated = native_safe(NativeList::default(), Some(fresh_child_float()))?;
    let outcome = tolerated.workspace.upgrade(&allow).await;
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(locked(&tolerated, "safe")?.as_deref(), Some("1.1.0"));
    assert_eq!(locked(&tolerated, "child")?.as_deref(), Some("1.1.0"));

    let refused = native_safe(
        NativeList {
            backstop_refuses: Some("child@1.1.0"),
            ..NativeList::default()
        },
        Some(fresh_child_float()),
    )?;
    let outcome = refused.workspace.upgrade(&allow).await;
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(locked(&refused, "safe")?.as_deref(), Some("1.0.0"));
    assert_eq!(locked(&refused, "child")?.as_deref(), Some("1.0.0"));
    let held = outcome
        .items
        .iter()
        .find(|item| item.name == "safe")
        .and_then(|item| item.skipped.as_ref())
        .expect("the candidate is held");
    assert_eq!(held.reason, SkipReason::TransitiveInCooldown);
    assert_eq!(held.offending.as_deref(), Some("child"));
    Ok(())
}

/// A version the native gate already refuses in the lock the run starts from was introduced by
/// no trial, so even one its exemption cannot help, the list being overridden by a setting above
/// it, holds no candidate, as a run-start cooldown violation holds none; the final lock check
/// still reports the refusal the run started with.
#[tokio::test]
async fn a_refusal_the_starting_lock_already_had_holds_no_candidate() -> eyre::Result<()> {
    let fake = FakeNode::new(TargetFallback::None, Script::default())
        .with_releases("safe", &[("1.0.0", false), ("1.1.0", false)])
        .with_releases("child", &[("1.0.0", false), ("1.1.0", true)])
        .with_native(NativeList {
            backstop_refuses: Some("child@1.1.0"),
            backstop_ignores_list: true,
            finalize_requires: Some("child@1.1.0"),
            ..NativeList::default()
        });
    let fixture = fixture(fake, &[("child", "1.1.0"), ("safe", "1.0.0")])?;

    let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

    assert_eq!(locked(&fixture, "safe")?.as_deref(), Some("1.1.0"));
    assert!(
        outcome
            .items
            .iter()
            .any(|item| item.name == "safe" && item.applied && item.skipped.is_none()),
        "{:?}",
        outcome.items
    );
    let messages: Vec<&str> = outcome
        .errors
        .iter()
        .map(|error| error.message.as_str())
        .collect();
    assert_eq!(
        messages,
        ["child@1.1.0 was published within the minimumReleaseAge cutoff"]
    );
    Ok(())
}

/// A starting verdict that cannot be read in full is no baseline: the pin the gate refuses earns
/// no exemption on it, and the refusal holds the candidate whose trial meets it.
#[tokio::test]
async fn an_incomplete_starting_verdict_grants_and_excuses_nothing() -> eyre::Result<()> {
    let fake = FakeNode::new(TargetFallback::None, Script::default())
        .with_releases("safe", &[("1.0.0", false), ("1.1.0", false)])
        .with_releases("child", &[("1.0.0", false), ("1.1.0", true)])
        .with_native(NativeList {
            backstop_refuses: Some("child@1.1.0"),
            starting_probe_fails: true,
            ..NativeList::default()
        });
    let fixture = fixture(fake, &[("child", "1.1.0"), ("safe", "1.0.0")])?;
    std::fs::write(fixture.project.root.join(NATIVE), "before")?;

    let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

    assert_eq!(locked(&fixture, "safe")?.as_deref(), Some("1.0.0"));
    assert_eq!(
        std::fs::read_to_string(fixture.project.root.join(NATIVE))?,
        "before"
    );
    let held = outcome
        .items
        .iter()
        .find(|item| item.name == "safe")
        .and_then(|item| item.skipped.as_ref())
        .expect("the candidate is held");
    assert_eq!(held.offending.as_deref(), Some("child"));
    Ok(())
}

/// A young pin the native gate refused in the starting lock, as once a native window is
/// switched on, earns an exact exemption while the lock still holds it, so an unrelated upgrade
/// lands and the final lock check passes.
#[tokio::test]
async fn a_pin_the_native_gate_refused_at_the_start_earns_its_exemption() -> eyre::Result<()> {
    let fake = FakeNode::new(TargetFallback::None, Script::default())
        .with_releases("safe", &[("1.0.0", false), ("1.1.0", false)])
        .with_releases("child", &[("1.0.0", false), ("1.1.0", true)])
        .with_native(NativeList {
            backstop_refuses: Some("child@1.1.0"),
            finalize_requires: Some("child@1.1.0"),
            ..NativeList::default()
        });
    let fixture = fixture(fake, &[("child", "1.1.0"), ("safe", "1.0.0")])?;

    let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "safe")?.as_deref(), Some("1.1.0"));
    assert_eq!(
        std::fs::read_to_string(fixture.project.root.join(NATIVE))?,
        "child@1.1.0"
    );
    assert_eq!(outcome.exit, Exit::Ok, "{:?}", outcome.errors);
    Ok(())
}

/// The verification of the starting lock runs once the first batch's journal is captured, and
/// whatever it rewrites is put back from that journal, so the apply finds the starting lock byte
/// for byte as it was.
#[tokio::test]
async fn the_starting_verification_leaves_the_lock_byte_identical() -> eyre::Result<()> {
    let fixture = native_safe(
        NativeList {
            grant: &["safe"],
            starting_probe_reformats_lock: true,
            ..NativeList::default()
        },
        None,
    )?;
    let lock_before = std::fs::read(fixture.project.root.join(LOCKFILE))?;

    let outcome = fixture.workspace.upgrade(&RunOpts::default()).await;

    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(
        fixture.fake.events(),
        ["backstop", "apply", "assess", "backstop"]
    );
    assert_eq!(fixture.fake.apply_saw_lock().first(), Some(&lock_before));
    assert_eq!(locked(&fixture, "safe")?.as_deref(), Some("1.1.0"));
    Ok(())
}

/// The native gate's refusal is a trial verdict like a cooldown violation, so candidate
/// isolation finds the candidate whose young child the gate refuses and still lands the other.
#[tokio::test]
async fn a_native_refusal_isolates_the_candidate_that_caused_it() -> eyre::Result<()> {
    let allow = RunOpts {
        transitive_mode: TransitiveGate::Allow,
        ..RunOpts::default()
    };
    let fake = FakeNode::new(
        TargetFallback::None,
        Script {
            float: Some(fresh_child_float()),
            ..Script::default()
        },
    )
    .with_releases("safe", &[("1.0.0", false), ("1.1.0", false)])
    .with_releases("child", &[("1.0.0", false), ("1.1.0", true)])
    .with_releases("other", &[("1.0.0", false), ("1.1.0", false)])
    .with_native(NativeList {
        backstop_refuses: Some("child@1.1.0"),
        ..NativeList::default()
    });
    let fixture = fixture(
        fake,
        &[("child", "1.0.0"), ("other", "1.0.0"), ("safe", "1.0.0")],
    )?;

    let outcome = fixture.workspace.upgrade(&allow).await;

    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(locked(&fixture, "other")?.as_deref(), Some("1.1.0"));
    assert_eq!(locked(&fixture, "safe")?.as_deref(), Some("1.0.0"));
    assert_eq!(locked(&fixture, "child")?.as_deref(), Some("1.0.0"));
    let held = outcome
        .items
        .iter()
        .find(|item| item.name == "safe")
        .and_then(|item| item.skipped.as_ref())
        .expect("the candidate that floats the refused child is held");
    assert_eq!(held.reason, SkipReason::TransitiveInCooldown);
    assert_eq!(held.offending.as_deref(), Some("child"));
    assert!(
        outcome
            .items
            .iter()
            .any(|item| item.name == "other" && item.applied),
        "{:?}",
        outcome.items
    );
    Ok(())
}
