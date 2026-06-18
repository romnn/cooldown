//! The Rust/Cargo [`Tool`]: detection, the resolved graph via `cargo metadata`, classified
//! releases from the crates.io sparse index, and `cargo`-driven apply/build. `=`-pinned versions
//! that `cargo update --precise` cannot move are reported as `GraphHeld`/`ResolverConflict` skips.

use crate::cargocmd::Cargo;
use crate::index::{CRATES_IO, CratesIoIndex};
use crate::version;
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_core::{
    ApplyReport, Capabilities, Change, CoreError, DepScope, Dependency, FetchContext,
    NativePolicyLayer, NativeRule, PackageId, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, RawWindow, Release, ReleaseOrder, ReleaseQuality, Result, Selector,
    SkipReason, Skipped, ToolId, ToolRead, ToolWrite, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use cooldown_toml_util::read_toml_file;

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
    let mut releases: Vec<Release> = raw
        .into_iter()
        .filter(|rr| version::parse(rr.version.as_str()).is_some())
        .map(|rr| {
            let v = rr.version.as_str();
            Release {
                version: rr.version.clone(),
                order: ReleaseOrder(Vec::new()),
                major: version::major_key(v),
                kind_from_current: version::classify_kind(current, v),
                published_at: rr.published_at,
                yanked: rr.yanked,
                quality: classify_quality(v),
            }
        })
        .collect();
    releases.sort_by(|a, b| version::compare(a.version.as_str(), b.version.as_str()));
    releases.dedup_by(|a, b| a.version == b.version);
    for (i, r) in releases.iter_mut().enumerate() {
        // The release index is the ascending rank; a big-endian u32 token preserves that order
        // lexicographically. Saturating at u32::MAX is purely defensive — no crate approaches it.
        let rank = u32::try_from(i).unwrap_or(u32::MAX);
        r.order = ReleaseOrder(rank.to_be_bytes().to_vec());
    }
    cooldown_core::debug_assert_sorted(&releases);
    releases
}

fn skipped_on_apply_error(change: &Change, error: CoreError) -> Result<Skipped> {
    if error.is_tool_spawn_failure() {
        return Err(error);
    }
    Ok(Skipped {
        change: change.clone(),
        reason: SkipReason::ResolverConflict,
        offending: Some(change.package.clone()),
    })
}

/// Parse `[package.metadata.cooldown]` / `[workspace.metadata.cooldown]` `min-age` into a native rule.
fn parse_native(manifest: &Utf8Path) -> Result<Option<NativePolicyLayer>> {
    let Some(value) = read_toml_file::<toml::Value>(manifest, "Cargo.toml")? else {
        return Ok(None);
    };
    let cooldown = value
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("cooldown"))
        .or_else(|| {
            value
                .get("workspace")
                .and_then(|w| w.get("metadata"))
                .and_then(|m| m.get("cooldown"))
        });
    let Some(cooldown) = cooldown else {
        return Ok(None);
    };
    let min_age = cooldown
        .get("min-age")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| {
            CoreError::Config(format!(
                "{manifest}: [package.metadata.cooldown] min-age must be a string"
            ))
        })?;
    let window = cooldown_core::duration::parse_duration(min_age)
        .map(RawWindow::RelativeDuration)
        .map_err(|e| CoreError::Config(format!("{manifest}: invalid native min-age: {e}")))?;
    Ok(Some(NativePolicyLayer {
        rules: vec![NativeRule {
            selector: Selector::Default,
            window,
        }],
    }))
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
            Ok(true) => Ok(VerifyReport {
                ok: true,
                detail: "Cargo.lock is current".into(),
            }),
            Ok(false) => Ok(VerifyReport {
                ok: false,
                detail: "Cargo.lock is stale; run `cargo update` or `cargo generate-lockfile`"
                    .into(),
            }),
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl ToolWrite for CargoTool {
    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        Ok(ProjectMutationJournal {
            files: vec![ProjectMutationJournal::capture_file(
                &project.root,
                Utf8Path::new("Cargo.lock"),
            )?],
        })
    }

    async fn apply(
        &self,
        project: &Project,
        plan: &Plan,
        _journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            match self
                .cargo
                .update_precise(
                    &project.root,
                    &change.package.name,
                    change.from.as_str(),
                    change.to.as_str(),
                )
                .await
            {
                Ok(()) => report.applied.push(change.clone()),
                Err(e) => {
                    // A `=`-pin or resolver conflict blocks `--precise`.
                    report.skipped.push(skipped_on_apply_error(change, e)?);
                }
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

    fn write_manifest(contents: &str) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("Cargo.toml")).expect("utf8 path");
        std::fs::write(&path, contents).expect("write manifest");
        (dir, path)
    }

    #[test]
    fn parse_native_errors_on_invalid_min_age() {
        let (_dir, manifest) = write_manifest(
            r#"
[package]
name = "demo"
version = "0.1.0"

[package.metadata.cooldown]
min-age = "not-a-duration"
"#,
        );

        let err = parse_native(&manifest).expect_err("invalid native config must fail");
        assert!(matches!(err, CoreError::Config(_)));
    }

    #[test]
    fn parse_native_reads_valid_package_metadata() {
        let (_dir, manifest) = write_manifest(
            r#"
[package]
name = "demo"
version = "0.1.0"

[package.metadata.cooldown]
min-age = "14d"
"#,
        );

        let layer = parse_native(&manifest)
            .expect("valid native config")
            .expect("native layer");
        assert_eq!(layer.rules.len(), 1);
        assert!(matches!(
            layer.rules[0].window,
            RawWindow::RelativeDuration(_)
        ));
    }

    #[test]
    fn apply_spawn_failure_is_not_downgraded_to_skip() {
        let change = Change {
            package: PackageId::new(CARGO_ID, "serde", Some(CRATES_IO.to_string())),
            from: Version::new("1.0.0"),
            to: Version::new("1.0.1"),
            kind: cooldown_core::UpdateKind::Patch,
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
