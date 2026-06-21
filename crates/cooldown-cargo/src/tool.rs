//! The Rust/Cargo [`Tool`]: detection, the resolved graph via `cargo metadata`, classified
//! releases from the crates.io sparse index, and `cargo`-driven apply/build. `=`-pinned versions
//! that `cargo update --precise` cannot move are reported as `ResolverConflict` skips.

use crate::cargocmd::Cargo;
use crate::index::{CRATES_IO, CratesIoIndex};
use crate::manifest;
use crate::native::parse_native;
use crate::version;
use async_trait::async_trait;
use camino::Utf8PathBuf;
use cooldown_adapter_util::{build_registry_releases, verify_current_report};
use cooldown_core::{
    ApplyReport, Capabilities, Change, DepScope, Dependency, FetchContext, NativePolicyLayer,
    PackageId, PackageRegistry, Plan, Project, ProjectMarker, ProjectMutationJournal, Release,
    ReleaseOrder, ReleaseQuality, Result, RewriteMode, SkipReason, Skipped, ToolId, ToolRead,
    ToolWrite, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use std::collections::BTreeSet;

/// The [`ToolId`] identifying the Rust/Cargo tool (`"cargo"`).
pub const CARGO_ID: ToolId = ToolId("cargo");

/// The Rust/Cargo implementation of the [`Tool`] port.
///
/// Pairs the crates.io sparse-index client ([`CratesIoIndex`]) with a [`Cargo`]
/// CLI wrapper: the index supplies publish times and the release set, while
/// `cargo` resolves the dependency graph and applies precise version changes.
pub struct CargoTool {
    index: CratesIoIndex,
    cargo: Cargo,
}

impl CargoTool {
    /// Creates an tool from an existing crates.io [`CratesIoIndex`] client.
    ///
    /// The [`Cargo`] CLI wrapper is constructed with its defaults (honoring the
    /// `COOLDOWN_CARGO` environment override).
    #[must_use]
    pub fn new(index: CratesIoIndex) -> Self {
        CargoTool {
            index,
            cargo: Cargo::new(),
        }
    }

    /// Creates an tool backed by the shared HTTP layer, building the index for you.
    ///
    /// Convenience constructor equivalent to `CargoTool::new(CratesIoIndex::new(http))`.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        CargoTool::new(CratesIoIndex::new(http))
    }
}

fn classify_quality(v: &str) -> ReleaseQuality {
    if version::is_prerelease(v) {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

/// Classifies raw crates.io releases into ordered, deduped [`Release`]s relative to `current`.
///
/// Unparsable versions are dropped, the rest are sorted by [`version::compare`] and deduplicated,
/// then each is stamped with a [`ReleaseOrder`] token reflecting its rank (ascending). `current` is
/// the currently pinned version, used to compute each release's [`UpdateKind`](cooldown_core::UpdateKind)
/// via [`version::classify_kind`].
#[must_use]
pub fn build_releases(current: &str, raw: Vec<cooldown_core::RawRelease>) -> Vec<Release> {
    build_registry_releases(
        current,
        raw,
        |value| version::parse(value).is_some(),
        version::compare,
        version::major_key,
        version::classify_kind,
        classify_quality,
    )
}

#[async_trait]
impl ToolRead for CargoTool {
    fn id(&self) -> ToolId {
        CARGO_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: false,
            artifact_granular: false,
        }
    }

    fn project_marker(&self) -> ProjectMarker {
        // A `Cargo.lock` marks a workspace root: `cargo metadata` there already covers every
        // member, so nested lockfiles below it are not separate projects.
        ProjectMarker {
            lockfile: "Cargo.lock",
            manifest: "Cargo.toml",
            workspace_root: true,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let graph = self.cargo.metadata(&project.root).await?;
        let mut deps = Vec::new();
        for (id, info) in &graph.packages {
            if graph.roots.contains(id) || !info.is_crates_io() {
                continue; // skip workspace members and non-crates.io sources
            }
            let direct = graph.is_direct(id);
            if scope == DepScope::Direct && !direct {
                continue;
            }
            let graph_floor = if graph.is_graph_held(id) {
                Some(Version::new(info.version.clone()))
            } else {
                None
            };
            deps.push(Dependency {
                package: PackageId::new(CARGO_ID, info.name.clone(), Some(CRATES_IO.to_string())),
                current: Version::new(info.version.clone()),
                current_quality: classify_quality(&info.version),
                direct,
                artifacts: Vec::new(),
                graph_floor,
                // Direct deps are attributed to their declarers; a transitive dep is attributed to
                // the members that reach it through the graph (rendered as "via …").
                members: if direct {
                    graph.direct_members(id)
                } else {
                    graph.reaching_members(id)
                },
                pinned: graph.is_exact_pinned(&info.name, &info.version),
            });
        }
        Ok(deps)
    }

    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.index.releases(&dep.package).await?;
        Ok(build_releases(dep.current.as_str(), raw))
    }

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
        let time = self
            .index
            .published_at(&dep.package, &dep.current, &[])
            .await?;
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

    async fn native_policy(&self, project: &Project) -> Result<Option<NativePolicyLayer>> {
        parse_native(&project.manifest)
    }

    async fn verify_lock_current(&self, project: &Project) -> Result<VerifyReport> {
        match self.cargo.verify_locked(&project.root).await {
            Ok(ok) => Ok(verify_current_report(
                ok,
                "Cargo.lock is current",
                "Cargo.lock is stale; run `cargo update` or `cargo generate-lockfile`",
            )),
            Err(e) => Err(e),
        }
    }
}

/// The result of applying one change: adopted, or skipped with a reason. Tool-spawn failures are
/// not represented here — they propagate as `Err` so a broken `cargo` aborts rather than masquerading
/// as a per-change skip.
enum ChangeOutcome {
    Applied,
    Skipped(SkipReason),
}

impl CargoTool {
    /// Apply one change, honoring the [`RewriteMode`]. `Auto` first tries to move the lock within the
    /// existing requirement (`cargo update --precise`) and only rewrites the manifest when that is
    /// rejected — typically because the target is past the declared range (a cross-major bump).
    /// `Always` rewrites the owning manifest entry up front.
    async fn apply_change(
        &self,
        project: &Project,
        change: &Change,
        mode: RewriteMode,
    ) -> Result<ChangeOutcome> {
        match mode {
            RewriteMode::Always => {
                // No prior lock-only attempt: a missing requirement just means there is nothing to
                // rewrite, so the dependency is not a `--rewrite` target.
                self.rewrite_then_lock(project, change, SkipReason::NotEligible)
                    .await
            }
            RewriteMode::Auto => match self.precise_lock(project, change).await {
                Ok(()) => Ok(ChangeOutcome::Applied),
                Err(err) if err.is_tool_spawn_failure() => Err(err),
                // The lock-only move was rejected (the target is outside the manifest constraint, or
                // a genuine resolver conflict). Widen the constraint and retry; if there is nothing to
                // widen because the crate is transitive-only / `=`-pinned by the graph, the resolver
                // is holding it where it is — a real conflict, not a filtered candidate.
                Err(_) => {
                    self.rewrite_then_lock(project, change, SkipReason::ResolverConflict)
                        .await
                }
            },
        }
    }

    /// Rewrite the owning manifest entry to admit the target, then re-pin the lock to it.
    ///
    /// `no_requirement` is the skip reason when the package has no editable requirement to widen:
    /// `ResolverConflict` on the `Auto` fallback (the lock-only move was already rejected), or
    /// `NotEligible` under `Always` (nothing to rewrite, no resolver attempt made).
    async fn rewrite_then_lock(
        &self,
        project: &Project,
        change: &Change,
        no_requirement: SkipReason,
    ) -> Result<ChangeOutcome> {
        let rewrite = manifest::widen_constraint(
            &project.root,
            &change.members,
            &change.package.name,
            change.to.as_str(),
        )?;
        if rewrite.modified.is_empty() {
            // No editable requirement: the crate is transitive-only or a path/git source. Nothing to
            // widen, so it cannot be adopted by `upgrade`.
            return Ok(ChangeOutcome::Skipped(no_requirement));
        }
        match self.precise_lock(project, change).await {
            Ok(()) => Ok(ChangeOutcome::Applied),
            Err(err) if err.is_tool_spawn_failure() => Err(err),
            Err(_) => Ok(ChangeOutcome::Skipped(SkipReason::ResolverConflict)),
        }
    }

    async fn precise_lock(&self, project: &Project, change: &Change) -> Result<()> {
        self.cargo
            .update_precise(
                &project.root,
                &change.package.name,
                change.from.as_str(),
                change.to.as_str(),
            )
            .await
    }
}

#[async_trait]
impl ToolWrite for CargoTool {
    async fn mutation_journal(
        &self,
        project: &Project,
        plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        // Capture the lock and every manifest a rewrite could touch (the root, for
        // `[workspace.dependencies]`, plus each declaring member) so a rejected trial rolls back
        // both the re-lock and any constraint edit. Capturing an unmodified manifest is harmless —
        // restore only runs on rollback and rewrites identical bytes.
        let mut relative: BTreeSet<Utf8PathBuf> = BTreeSet::new();
        relative.insert(Utf8PathBuf::from("Cargo.lock"));
        relative.insert(Utf8PathBuf::from("Cargo.toml"));
        for change in &plan.changes {
            for member in &change.members {
                relative.insert(manifest::member_manifest_rel(&member.path));
            }
        }
        let mut files = Vec::with_capacity(relative.len());
        for rel in relative {
            files.push(ProjectMutationJournal::capture_file(&project.root, &rel)?);
        }
        Ok(ProjectMutationJournal { files })
    }

    async fn apply(
        &self,
        project: &Project,
        plan: &Plan,
        _journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            match self.apply_change(project, change, plan.rewrite).await? {
                ChangeOutcome::Applied => report.applied.push(change.clone()),
                ChangeOutcome::Skipped(reason) => report.skipped.push(Skipped {
                    change: change.clone(),
                    reason,
                    offending: Some(change.package.clone()),
                }),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.cargo.build(&project.root).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use cooldown_adapter_util::skipped_on_apply_error;
    use cooldown_core::CoreError;

    #[test]
    fn apply_spawn_failure_is_not_downgraded_to_skip() {
        let change = Change {
            package: PackageId::new(CARGO_ID, "serde", Some(CRATES_IO.to_string())),
            from: Version::new("1.0.0"),
            to: Version::new("1.0.1"),
            kind: cooldown_core::UpdateKind::Patch,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        };
        let err = CoreError::ToolSpawn {
            tool: "cargo".into(),
            detail: "spawn failed".into(),
        };

        let result = skipped_on_apply_error(&change, err);
        assert!(matches!(result, Err(CoreError::ToolSpawn { .. })));
    }

    #[tokio::test]
    async fn mutation_journal_restore_removes_lock_created_after_capture() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let manifest = root.join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("write manifest");
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let eco = CargoTool::from_http(
            cooldown_registry::SharedHttp::new(
                cache_dir.path(),
                cooldown_registry::HttpOptions::default(),
            )
            .expect("http"),
        );
        let project = Project {
            root: root.clone(),
            kind: CARGO_ID,
            manifest,
        };

        let journal = eco
            .mutation_journal(&project, &Plan::default())
            .await
            .expect("journal");
        let lock = root.join("Cargo.lock");
        std::fs::write(&lock, "generated").expect("write lock");

        journal.restore(&project.root).expect("restore");
        assert!(!lock.exists());
    }
}
