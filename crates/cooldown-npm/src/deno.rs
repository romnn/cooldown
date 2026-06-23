//! The Deno [`Tool`]: detection by `deno.lock`, the resolved graph read from that lock, and
//! publish times routed per dependency to the registry that owns it. Unlike npm/pnpm/yarn/bun, a
//! Deno project mixes two registries — `jsr:` specifiers resolve from [`JsrRegistry`] and `npm:`
//! specifiers from [`NpmRegistry`] — so this adapter carries both clients and dispatches on each
//! dependency's recorded registry. Both registries speak `SemVer`, so the version model is shared.

use crate::jsr::{JSR, JsrRegistry};
use crate::lock::split_name_version;
use crate::nodecmd::NodeCmd;
use crate::registry::{NPM, NpmRegistry};
use crate::tool::{build_releases, classify_quality};
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::verify_current_report;
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, Change, DepScope, Dependency, FetchContext,
    NativePolicyLayer, PackageId, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, Release, ReleaseFetcher, ReleaseOrder, Result, SkipReason, Skipped,
    ToolId, ToolRead, ToolWrite, UpdateKind, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

/// The [`ToolId`] for the Deno adapter (`"deno"`).
pub const DENO_ID: ToolId = ToolId("deno");

/// The Deno implementation of the [`Tool`] port.
///
/// It reads `deno.lock` (the `workspace.dependencies` list for the direct set, the `jsr`/`npm`
/// maps for the resolved graph) and resolves each dependency's releases from the registry named on
/// its [`PackageId`]. Deno has no in-manifest cooldown field, so [`native_policy`] is always empty.
///
/// [`native_policy`]: ToolRead::native_policy
pub struct DenoTool {
    npm: NpmRegistry,
    jsr: JsrRegistry,
    cmd: NodeCmd,
}

impl DenoTool {
    /// Creates the adapter from the npm and JSR registry clients.
    #[must_use]
    pub fn new(npm: NpmRegistry, jsr: JsrRegistry) -> Self {
        DenoTool {
            npm,
            jsr,
            cmd: NodeCmd::new("deno"),
        }
    }

    /// Creates the adapter from a shared HTTP client, building both registry clients.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        DenoTool::new(NpmRegistry::new(http.clone()), JsrRegistry::new(http))
    }

    /// Dispatches a release listing to the registry that owns `dep`: `jsr:` deps to JSR, everything
    /// else (the default) to npm.
    async fn raw_releases(&self, dep: &Dependency) -> Result<Vec<cooldown_core::RawRelease>> {
        if dep.package.registry.as_deref() == Some(JSR) {
            self.jsr.releases(&dep.package).await
        } else {
            self.npm.releases(&dep.package).await
        }
    }

    async fn locked_published_at(&self, dep: &Dependency) -> Result<Option<jiff::Timestamp>> {
        if dep.package.registry.as_deref() == Some(JSR) {
            self.jsr.published_at(&dep.package, &dep.current, &[]).await
        } else {
            self.npm.published_at(&dep.package, &dep.current, &[]).await
        }
    }
}

/// Splits a `deno.lock` specifier (`npm:lodash@4.17.15`, `jsr:@std/path@1.0.0`) into its registry,
/// package name, and requested version. An unknown scheme (e.g. an `https:` import) yields `None`,
/// so only registry-backed dependencies are surfaced.
fn split_specifier(spec: &str) -> Option<(&'static str, String, String)> {
    let (scheme, rest) = spec.split_once(':')?;
    let registry = match scheme {
        "npm" => NPM,
        "jsr" => JSR,
        _ => return None,
    };
    let (name, version) = split_name_version(rest)?;
    Some((registry, name, version))
}

/// The resolved lock's `name -> (registry, newest version)` map, the snapshot `apply` diffs
/// before/after the whole-graph re-resolve so *every* net version change is reported (the planned
/// moves, the collateral churn the joint resolve forced on other packages, and the candidates left
/// held below their target). A Deno graph spans two registries, so both the `jsr` and `npm` sections
/// are read; a name that resolves to several copies keeps its newest, so a moved direct declaration
/// is never masked by a stale transitive copy of the same name. An unparsable lock yields an empty
/// map, so a package that moved is still reported (never silent).
fn locked_versions(content: &str) -> HashMap<String, (&'static str, String)> {
    let Ok(lock) = serde_json::from_str::<serde_json::Value>(content) else {
        return HashMap::new();
    };
    let mut out: HashMap<String, (&'static str, String)> = HashMap::new();
    for registry in [JSR, NPM] {
        let Some(section) = lock.get(registry).and_then(|v| v.as_object()) else {
            continue;
        };
        for key in section.keys() {
            let Some((name, version)) = split_name_version(key) else {
                continue;
            };
            match out.entry(name) {
                Entry::Occupied(mut slot) => {
                    if version::compare(&version, &slot.get().1).is_gt() {
                        *slot.get_mut() = (registry, version);
                    }
                }
                Entry::Vacant(slot) => {
                    slot.insert((registry, version));
                }
            }
        }
    }
    out
}

/// Whether a planned candidate landed at or beyond its target in `after`, respecting the move's
/// direction: a forward move must reach at/above its target, a downgrade at/below it. A package the
/// resolve dropped from the lock (no entry) counts as not reached.
fn reached(after: &HashMap<String, (&'static str, String)>, change: &Change) -> bool {
    after.get(&change.package.name).is_some_and(|(_, landed)| {
        let ordering = version::compare(landed, change.to.as_str());
        if change.downgrade {
            ordering.is_le()
        } else {
            ordering.is_ge()
        }
    })
}

/// A net version change `apply` derived from the before/after lock diff for a package the plan did
/// not itself name — collateral movement the whole-graph re-resolve forced. Reported so no package's
/// version change is ever silent: a transitive the window pulled forward or back to keep the lock
/// consistent surfaces as its own report row, tagged with the registry it resolved from.
fn collateral_change(registry: &'static str, name: &str, from: &str, to: &str) -> Change {
    Change {
        package: PackageId::new(DENO_ID, name.to_string(), Some(registry.to_string())),
        from: Version::new(from.to_string()),
        to: Version::new(to.to_string()),
        // A collateral move is transitive consistency churn, not a directly-declared bump; its update
        // kind is informational only and `Minor` is the neutral label the renderer shows.
        kind: UpdateKind::Minor,
        downgrade: version::compare(to, from).is_lt(),
        direct: false,
        members: Vec::new(),
    }
}

/// The held skip for a candidate the joint resolve could not place at its target. Deno's resolver
/// imposes no transitive `==` ceiling (the npm family floats a transitive up unless a manifest pins
/// it), so there is no structural blocker to attribute — the candidate names itself, yielding the
/// generic "the resolver rejected this change" message.
fn resolver_conflict(change: &Change) -> Skipped {
    Skipped {
        change: change.clone(),
        reason: SkipReason::ResolverConflict,
        offending: Some(change.package.clone()),
    }
}

/// Deno's `--minimum-dependency-age` value for cooldown's resolution window. An absolute RFC3339
/// freeze instant is handed to deno verbatim — deno treats it as the publish-date cutoff (exclude
/// everything published after it), exactly as uv consumes `--exclude-newer`, so the whole graph is
/// windowed natively. A relative age span (`"14 days"`, `"36 hours"`) is converted to whole minutes,
/// which deno also accepts. `None` (a `Latest`/opt-out window) omits the flag so nothing is excluded.
fn deno_cutoff_arg(cutoff: Option<&str>) -> Option<String> {
    let cutoff = cutoff?.trim();
    if cutoff.parse::<jiff::Timestamp>().is_ok() {
        return Some(cutoff.to_string());
    }
    let (count, unit) = cutoff.split_once(' ')?;
    let count: i64 = count.parse().ok()?;
    let minutes = match unit.trim_end_matches('s') {
        "day" => count.checked_mul(24 * 60)?,
        "hour" => count.checked_mul(60)?,
        "minute" => count,
        // A second-granularity window rounds up to a whole minute so a sub-minute age still excludes
        // the just-published release rather than silently disabling the cooldown.
        "second" => count.checked_add(59)? / 60,
        _ => return None,
    };
    (minutes > 0).then(|| minutes.to_string())
}

/// Pin the `deno.json`/`deno.jsonc` import that resolves `(scheme, name)` to the exact `target`,
/// rewriting its specifier to `<scheme>:<name>@<target>`.
///
/// Deno's `deno install` is conservative — it keeps an existing lock pin even when the window would
/// exclude it, and a range like `^1.4.0` re-resolves to the window-newest, not cooldown's per-package
/// target. So the only way to land exactly `change.to` (the version cooldown computed under this
/// package's own window) is to narrow its declared specifier to that exact version; the resolve then
/// pins it there in both directions (a forward move or a `fix` downgrade). This mirrors cargo's
/// `--precise`/pnpm's `update <pkg>@<target>` — the per-package target encodes the per-package window,
/// so pinning it enforces that window exactly while the `--minimum-dependency-age` cutoff floors the
/// transitives. A candidate not declared in the manifest (a transitive) has no import to pin and is
/// left to the cutoff. The edit rewrites only this import's value, leaving the rest of the file
/// byte-identical so a converged graph re-applies stably.
fn pin_import(root: &Utf8Path, scheme: &str, name: &str, target: &str) -> Result<()> {
    for rel in workspace_manifest_rels(root) {
        let path = root.join(&rel);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue; // a deno.jsonc with comments is not plain JSON; leave it to the cutoff
        };
        let Some(imports) = doc.get("imports").and_then(|v| v.as_object()) else {
            continue;
        };
        // The import alias whose specifier resolves this package, paired with that specifier. Keying
        // the edit on the alias (not just the specifier text) targets exactly this import's value even
        // when the alias equals the specifier or the same string appears under `scopes`.
        let entry = imports.iter().find_map(|(alias, value)| {
            let spec = value.as_str()?;
            split_specifier(spec)
                .is_some_and(|(reg, n, _)| reg == scheme && n == name)
                .then(|| (alias.clone(), spec.to_string()))
        });
        let Some((alias, old_spec)) = entry else {
            continue;
        };
        let new_spec = format!("{scheme}:{name}@{target}");
        if old_spec == new_spec {
            return Ok(()); // already at the target — no edit keeps the file byte-stable
        }
        // Reuse the package.json range editor: a byte-targeted rewrite of the `imports` → `alias`
        // value that leaves the rest of the file untouched.
        if let Some(updated) = crate::manifest::replace_declared_range(
            &content, "imports", &alias, &old_spec, &new_spec,
        ) {
            std::fs::write(&path, updated)?;
        }
        return Ok(());
    }
    Ok(())
}

/// Every `deno.json`/`deno.jsonc` manifest path (relative to the project root) that could declare an
/// import: the root's, plus each workspace member's. deno lists member directories in the root
/// manifest's `workspace` array, and a member declares its own imports in its own manifest — so a
/// member-declared dependency is pinned (and journaled for rollback) there, not in the root. The root
/// pair is always included; members are added when the root manifest declares a `workspace` array.
fn workspace_manifest_rels(root: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut rels = vec![
        Utf8PathBuf::from("deno.json"),
        Utf8PathBuf::from("deno.jsonc"),
    ];
    for manifest in ["deno.json", "deno.jsonc"] {
        let Ok(content) = std::fs::read_to_string(root.join(manifest)) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let Some(members) = doc.get("workspace").and_then(|value| value.as_array()) else {
            continue;
        };
        for member in members.iter().filter_map(serde_json::Value::as_str) {
            let dir = member.trim_start_matches("./").trim_end_matches('/');
            // Skip a member that resolves to the root itself (`"."`/`"./"`); its manifests are already
            // the root pair added above.
            if dir.is_empty() || dir == "." {
                continue;
            }
            rels.push(Utf8PathBuf::from(format!("{dir}/deno.json")));
            rels.push(Utf8PathBuf::from(format!("{dir}/deno.jsonc")));
        }
    }
    // Both root manifests can declare the same `workspace` array, so a member may be added twice; keep
    // only the first occurrence (capture/restore is idempotent, but the duplicate is needless work).
    let mut seen = std::collections::HashSet::new();
    rels.retain(|rel| seen.insert(rel.clone()));
    rels
}

/// Every direct specifier the workspace declares, keyed by `(registry, name)`: the root
/// `workspace.dependencies` unioned with each `workspace.members.<name>.dependencies`. deno records a
/// member's own direct imports under its member entry (the root list carries only the root package's),
/// so a dependency a member alone declares must still count as direct — otherwise it is misclassified
/// as transitive and dropped from the default (direct-only) scope, so the upgrade never moves it.
fn workspace_direct_specifiers(lock: &serde_json::Value) -> HashSet<(&'static str, String)> {
    let mut direct = HashSet::new();
    let mut collect = |array: Option<&serde_json::Value>| {
        for spec in array
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(spec) = spec.as_str()
                && let Some((registry, name, _)) = split_specifier(spec)
            {
                direct.insert((registry, name));
            }
        }
    };
    collect(lock.pointer("/workspace/dependencies"));
    if let Some(members) = lock
        .pointer("/workspace/members")
        .and_then(|value| value.as_object())
    {
        for member in members.values() {
            collect(member.get("dependencies"));
        }
    }
    direct
}

impl DenoTool {
    fn read_deps(project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let content = std::fs::read_to_string(project.root.join("deno.lock"))?;
        let lock: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| cooldown_core::CoreError::Parse(format!("deno.lock: {e}")))?;

        // The workspace's declared specifiers are the direct set, keyed by (registry, name).
        let direct = workspace_direct_specifiers(&lock);

        let mut seen = HashSet::new();
        let mut deps = Vec::new();
        // The `jsr` and `npm` sections key every resolved package by its `name@version` identity.
        for registry in [JSR, NPM] {
            let Some(section) = lock.get(registry).and_then(|v| v.as_object()) else {
                continue;
            };
            for key in section.keys() {
                let Some((name, version)) = split_name_version(key) else {
                    continue;
                };
                let is_direct = direct.contains(&(registry, name.clone()));
                if scope == DepScope::Direct && !is_direct {
                    continue;
                }
                if !seen.insert((name.clone(), version.clone())) {
                    continue;
                }
                deps.push(Dependency {
                    package: PackageId::new(DENO_ID, name, Some(registry.to_string())),
                    current: Version::new(version.clone()),
                    current_quality: classify_quality(&version),
                    direct: is_direct,
                    artifacts: Vec::new(),
                    graph_floor: None,
                    graph_ceiling: None,
                    members: Vec::new(),
                    pinned: false,
                });
            }
        }
        Ok(deps)
    }
}

#[async_trait]
impl ToolRead for DenoTool {
    fn id(&self) -> ToolId {
        DENO_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: true,
            artifact_granular: false,
        }
    }

    fn project_marker(&self) -> ProjectMarker {
        ProjectMarker {
            lockfile: "deno.lock",
            manifest: "deno.json",
            workspace_root: true,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        DenoTool::read_deps(project, scope)
    }

    async fn native_policy(&self, _project: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }

    async fn verify_lock_current(&self, _project: &Project) -> Result<VerifyReport> {
        Ok(verify_current_report(
            true,
            "lockfile taken as current",
            "lockfile is stale",
        ))
    }
}

#[async_trait]
impl ReleaseFetcher for DenoTool {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.raw_releases(dep).await?;
        Ok(build_releases(dep.current.as_str(), raw))
    }

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
        let time = self.locked_published_at(dep).await?;
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
}

#[async_trait]
impl ToolWrite for DenoTool {
    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        // Deno's manifest may be `deno.json` or `deno.jsonc`; capture the root pair and every
        // workspace member's pair (an absent one is a no-op to restore) plus the lock, since the
        // resolve narrows whichever manifest declares a pinned import and re-locks. Capturing the
        // member manifests keeps rollback correct when a member-declared candidate is the one pinned.
        let mut files = Vec::new();
        for rel in workspace_manifest_rels(&project.root) {
            files.push(ProjectMutationJournal::capture_file(&project.root, &rel)?);
        }
        files.push(ProjectMutationJournal::capture_file(
            &project.root,
            Utf8Path::new("deno.lock"),
        )?);
        Ok(ProjectMutationJournal { files })
    }

    /// Re-resolve the **whole** dependency graph once under cooldown's window, pinning every planned
    /// candidate to its EXACT per-package target, then report the full before/after `deno.lock` diff
    /// — the cargo/pnpm whole-graph pattern, with deno's native publish-date cutoff doing the
    /// windowing (the uv model) instead of out-of-band transitive pins.
    ///
    /// Each planned candidate's declared `deno.json` specifier is narrowed to its exact `change.to`
    /// (the version cooldown-core computed under that package's own window), then a single
    /// `deno install --lockfile-only --minimum-dependency-age <cutoff>` settles the entire graph —
    /// direct *and* transitive — in one pass. The cutoff is handed to deno verbatim as its native
    /// floor, so a fresh transitive the pins drag in is capped to the project-default window, exactly
    /// as pnpm's `minimumReleaseAge`; transitives floated past it are reconciled down by the caller's
    /// transitive-cooldown gate. The report is the diff of the journal's pre-apply lock against the
    /// result, so every planned candidate is reported reached or held and every collateral move of an
    /// unplanned package surfaces as its own row — no version change is ever silent. A resolver
    /// failure marks all candidates held and lets the caller restore the journal.
    async fn apply(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        if plan.changes.is_empty() {
            return Ok(report);
        }

        let before = journal
            .files
            .iter()
            .find(|file| file.path == Utf8Path::new("deno.lock"))
            .and_then(|file| file.contents.as_deref())
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .map(locked_versions)
            .unwrap_or_default();

        // Pin each planned candidate to its exact per-package target in the manifest so the
        // conservative resolve lands that target, not the range/window-newest. A transitive (no import
        // to pin) is windowed by the cutoff alone.
        for change in &plan.changes {
            let scheme = change.package.registry.as_deref().unwrap_or(NPM);
            pin_import(
                &project.root,
                scheme,
                &change.package.name,
                change.to.as_str(),
            )?;
        }

        let mut args = vec!["install".to_string(), "--lockfile-only".to_string()];
        if let Some(cutoff) = deno_cutoff_arg(project.exclude_newer.as_deref()) {
            args.push("--minimum-dependency-age".to_string());
            args.push(cutoff);
        }
        match self.cmd.run(&project.root, &args).await {
            Ok(()) => {}
            Err(err) if err.is_tool_spawn_failure() => return Err(err),
            // The joint resolve is unsatisfiable as a whole. Propagate so the caller's `apply_resilient`
            // can isolate the offending candidate(s) and apply the rest, instead of holding every
            // candidate. The caller restores the journal, so no partial lock is kept.
            Err(err) => return Err(err),
        }

        let after = locked_versions(&std::fs::read_to_string(project.root.join("deno.lock"))?);
        let planned: HashSet<&str> = plan
            .changes
            .iter()
            .map(|change| change.package.name.as_str())
            .collect();

        for change in &plan.changes {
            let name = change.package.name.as_str();
            // Whether the lock's version for this name actually moved. Reporting only genuine moves
            // keeps the report set equal to the lock-diff set, so a converged re-run reports zero
            // applied (no oscillation).
            let moved = match (before.get(name), after.get(name)) {
                (Some((_, from)), Some((_, to))) => version::compare(from, to).is_ne(),
                (None, Some(_)) | (Some(_), None) => true,
                (None, None) => false,
            };
            if reached(&after, change) {
                if moved {
                    report.applied.push(change.clone());
                }
            } else {
                report.skipped.push(resolver_conflict(change));
            }
        }

        let mut collateral: Vec<Change> = before
            .iter()
            .filter(|(name, _)| !planned.contains(name.as_str()))
            .filter_map(|(name, (_, from))| {
                let (registry, to) = after.get(name)?;
                (version::compare(from, to).is_ne())
                    .then(|| collateral_change(registry, name, from, to))
            })
            .collect();
        collateral.sort_by(|a, b| a.package.name.cmp(&b.package.name));
        report.applied.extend(collateral);
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.cmd
            .verify(&project.root, &["install".into()], "deno install succeeded")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn splits_npm_and_jsr_specifiers() {
        assert_eq!(
            split_specifier("npm:lodash@4.17.15"),
            Some((NPM, "lodash".into(), "4.17.15".into()))
        );
        assert_eq!(
            split_specifier("jsr:@std/path@1.0.0"),
            Some((JSR, "@std/path".into(), "1.0.0".into()))
        );
        assert_eq!(split_specifier("https://example.com/mod.ts"), None);
    }

    #[test]
    fn reads_direct_and_graph_from_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let lock_json = indoc! {r#"
            {
                "version": "5",
                "specifiers": {
                    "npm:lodash@4.17.15": "4.17.15",
                    "jsr:@std/path@1.0.0": "1.0.0"
                },
                "jsr": { "@std/path@1.0.0": { "integrity": "x" } },
                "npm": {
                    "lodash@4.17.15": { "integrity": "y" },
                    "ms@2.1.3": { "integrity": "z" }
                },
                "workspace": {
                    "dependencies": ["npm:lodash@4.17.15", "jsr:@std/path@1.0.0"]
                }
            }"#};
        std::fs::write(root.join("deno.lock"), lock_json).expect("write lock");
        let project = Project {
            root: root.clone(),
            kind: DENO_ID,
            manifest: root.join("deno.json"),
            exclude_newer: None,
        };

        let mut direct = DenoTool::read_deps(&project, DepScope::Direct).expect("direct");
        direct.sort_by(|a, b| a.package.name.cmp(&b.package.name));
        assert_eq!(direct.len(), 2);
        assert_eq!(direct[0].package.name, "@std/path");
        assert_eq!(direct[0].package.registry.as_deref(), Some(JSR));
        assert_eq!(direct[1].package.name, "lodash");
        assert_eq!(direct[1].package.registry.as_deref(), Some(NPM));

        let graph = DenoTool::read_deps(&project, DepScope::Graph).expect("graph");
        assert_eq!(graph.len(), 3); // + the transitive `ms`
        assert!(graph.iter().any(|d| d.package.name == "ms" && !d.direct));
    }

    const LOCK: &str = indoc! {r#"
        {
            "version": "5",
            "specifiers": { "npm:debug@4": "4.3.6", "jsr:@std/path@1": "1.0.3" },
            "jsr": { "@std/path@1.0.3": { "integrity": "p" } },
            "npm": {
                "debug@4.3.6": { "integrity": "d" },
                "ms@2.1.2": { "integrity": "m1" },
                "ms@2.1.3": { "integrity": "m2" }
            }
        }"#};

    #[test]
    fn locked_versions_unions_registries_and_keeps_newest_copy() {
        let pins = locked_versions(LOCK);
        assert_eq!(pins.get("debug"), Some(&(NPM, "4.3.6".to_string())));
        assert_eq!(pins.get("@std/path"), Some(&(JSR, "1.0.3".to_string())));
        // `ms` resolves to two copies; the newest is kept so a moved direct is never masked.
        assert_eq!(pins.get("ms"), Some(&(NPM, "2.1.3".to_string())));
        // An unparsable lock is empty (a moved package is still reported, never silent).
        assert!(locked_versions("not json").is_empty());
    }

    fn change(name: &str, registry: &str, to: &str, downgrade: bool) -> Change {
        Change {
            package: PackageId::new(DENO_ID, name, Some(registry.to_string())),
            from: Version::new("0.0.0"),
            to: Version::new(to),
            kind: UpdateKind::Minor,
            downgrade,
            direct: true,
            members: Vec::new(),
        }
    }

    #[test]
    fn reached_respects_move_direction() {
        let mut after: HashMap<String, (&'static str, String)> = HashMap::new();
        after.insert("debug".to_string(), (NPM, "4.3.6".to_string()));

        assert!(reached(&after, &change("debug", NPM, "4.3.6", false))); // forward, exact
        assert!(reached(&after, &change("debug", NPM, "4.3.0", false))); // forward, landed higher
        assert!(!reached(&after, &change("debug", NPM, "4.4.0", false))); // forward, short
        assert!(reached(&after, &change("debug", NPM, "4.4.0", true))); // downgrade, landed lower
        assert!(!reached(&after, &change("debug", NPM, "4.3.0", true))); // downgrade, not low enough
        assert!(!reached(&after, &change("absent", NPM, "1.0.0", false))); // dropped from the lock
    }

    #[test]
    fn collateral_change_tags_registry_and_direction() {
        let up = collateral_change(NPM, "ms", "2.1.2", "2.1.3");
        assert_eq!(up.package.registry.as_deref(), Some(NPM));
        assert!(!up.downgrade);
        assert!(!up.direct);
        let down = collateral_change(JSR, "@std/path", "1.0.3", "1.0.2");
        assert_eq!(down.package.registry.as_deref(), Some(JSR));
        assert!(down.downgrade);
    }

    #[test]
    fn cutoff_arg_passes_absolute_verbatim_and_converts_spans() {
        assert_eq!(
            deno_cutoff_arg(Some("2024-09-01T00:00:00Z")).as_deref(),
            Some("2024-09-01T00:00:00Z")
        );
        assert_eq!(deno_cutoff_arg(Some("14 days")).as_deref(), Some("20160"));
        assert_eq!(deno_cutoff_arg(Some("2 hours")).as_deref(), Some("120"));
        assert_eq!(deno_cutoff_arg(None), None);
    }

    fn pin(root: &camino::Utf8Path, json: &str, scheme: &str, name: &str, target: &str) -> String {
        std::fs::write(root.join("deno.json"), json).expect("write");
        pin_import(root, scheme, name, target).expect("pin");
        std::fs::read_to_string(root.join("deno.json")).expect("read")
    }

    #[test]
    fn pin_import_narrows_only_the_matching_specifier() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let manifest = indoc! {r#"
            {
              "imports": {
                "debug": "npm:debug@^4.0.0",
                "path": "jsr:@std/path@^1.0.0"
              }
            }
        "#};
        let after = pin(&root, manifest, NPM, "debug", "4.3.6");
        assert!(after.contains(r#""debug": "npm:debug@4.3.6""#));
        // Only the matched specifier changed; the jsr import and the alias keys are untouched.
        assert!(after.contains(r#""path": "jsr:@std/path@^1.0.0""#));

        // A package not declared in any import is a no-op (a transitive left to the cutoff).
        pin_import(&root, NPM, "leftpad", "1.0.0").expect("noop");
        assert_eq!(
            std::fs::read_to_string(root.join("deno.json")).expect("read"),
            after
        );
    }

    #[test]
    fn pin_import_targets_the_value_not_a_matching_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        // A bare-specifier import whose alias *equals* its specifier: the edit must rewrite the value,
        // not the key, or the resolve overshoots/never converges.
        let manifest = indoc! {r#"
            {
              "imports": {
                "npm:debug@^4.0.0": "npm:debug@^4.0.0"
              }
            }
        "#};
        let after = pin(&root, manifest, NPM, "debug", "4.3.6");
        assert!(
            after.contains(r#""npm:debug@^4.0.0": "npm:debug@4.3.6""#),
            "the key must stay and only the value pin to the target, got:\n{after}"
        );
    }

    #[test]
    fn pin_import_ignores_an_identical_specifier_under_scopes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        // A `scopes` entry carrying the same specifier *before* `imports`: the edit must rewrite the
        // `imports` value, leaving the scoped copy untouched.
        let manifest = indoc! {r#"
            {
              "scopes": {
                "https://example.com/": { "debug": "npm:debug@^4.0.0" }
              },
              "imports": {
                "debug": "npm:debug@^4.0.0"
              }
            }
        "#};
        let after = pin(&root, manifest, NPM, "debug", "4.3.6");
        assert!(
            after.contains(r#""debug": "npm:debug@4.3.6""#),
            "the imports value must pin to the target"
        );
        assert!(
            after.contains(r#""debug": "npm:debug@^4.0.0""#),
            "the scoped copy must be left untouched, got:\n{after}"
        );
    }
}
