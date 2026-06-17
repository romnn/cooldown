//! The Rust/Cargo [`Ecosystem`]: detection, the resolved graph via `cargo metadata`, classified
//! releases from the crates.io sparse index, and `cargo`-driven apply/build. `=`-pinned versions
//! that `cargo update --precise` cannot move are reported as `GraphHeld`/`ResolverConflict` skips.

use crate::cargocmd::Cargo;
use crate::index::{CRATES_IO, CratesIoIndex};
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    ApplyReport, Capabilities, DepScope, Dependency, Ecosystem, EcosystemId, LockSnapshot,
    NativePolicyLayer, NativeRule, PackageId, PackageRegistry, Plan, Project, RawWindow, Release,
    ReleaseOrder, ReleaseQuality, Result, Selector, SkipReason, Skipped, TargetContext,
    VerifyReport, Version,
};
use cooldown_registry::SharedHttp;

pub const CARGO_ID: EcosystemId = EcosystemId("rust");

pub struct CargoEcosystem {
    index: CratesIoIndex,
    cargo: Cargo,
}

impl CargoEcosystem {
    pub fn new(index: CratesIoIndex) -> Self {
        CargoEcosystem {
            index,
            cargo: Cargo::new(),
        }
    }
    pub fn from_http(http: SharedHttp) -> Self {
        CargoEcosystem::new(CratesIoIndex::new(http))
    }
}

fn classify_quality(v: &str) -> ReleaseQuality {
    if version::is_prerelease(v) {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

/// Classify raw crates.io releases into ordered, deduped releases relative to the current pin.
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
        r.order = ReleaseOrder((i as u32).to_be_bytes().to_vec());
    }
    releases
}

fn find_cargo_locks(root: &Utf8Path, out: &mut Vec<Utf8PathBuf>) {
    if root.join("Cargo.lock").is_file() {
        out.push(root.to_owned());
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = e.file_name();
        let name = name.to_string_lossy();
        if matches!(name.as_ref(), "target" | ".git" | "node_modules" | "vendor") {
            continue;
        }
        if let Ok(child) = Utf8PathBuf::from_path_buf(p) {
            find_cargo_locks(&child, out);
        }
    }
}

/// Parse `[package.metadata.cooldown]` / `[workspace.metadata.cooldown]` `min-age` into a native rule.
fn parse_native(manifest: &Utf8Path) -> Option<NativePolicyLayer> {
    let content = std::fs::read_to_string(manifest).ok()?;
    let value: toml::Value = content.parse().ok()?;
    let cooldown = value
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("cooldown"))
        .or_else(|| {
            value
                .get("workspace")
                .and_then(|w| w.get("metadata"))
                .and_then(|m| m.get("cooldown"))
        })?;
    let min_age = cooldown.get("min-age")?.as_str()?;
    let window = cooldown_core::duration::parse_duration(min_age)
        .map(RawWindow::RelativeDuration)
        .ok()?;
    Some(NativePolicyLayer {
        rules: vec![NativeRule {
            selector: Selector::Default,
            window,
        }],
    })
}

#[async_trait]
impl Ecosystem for CargoEcosystem {
    fn id(&self) -> EcosystemId {
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

    async fn detect(&self, root: &Utf8Path) -> Result<Vec<Project>> {
        let mut roots = Vec::new();
        find_cargo_locks(root, &mut roots);
        Ok(roots
            .into_iter()
            .map(|dir| Project {
                manifest: dir.join("Cargo.toml"),
                root: dir,
                kind: CARGO_ID,
            })
            .collect())
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

    async fn releases(&self, dep: &Dependency, _ctx: &TargetContext<'_>) -> Result<Vec<Release>> {
        let raw = self.index.releases(&dep.package).await?;
        Ok(build_releases(dep.current.as_str(), raw))
    }

    async fn locked_release(&self, dep: &Dependency, _ctx: &TargetContext<'_>) -> Result<Release> {
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
        Ok(parse_native(&project.manifest))
    }

    async fn apply(&self, project: &Project, plan: &Plan) -> Result<ApplyReport> {
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
                Err(_) => {
                    // A `=`-pin or resolver conflict blocks `--precise`.
                    report.skipped.push(Skipped {
                        change: change.clone(),
                        reason: SkipReason::ResolverConflict,
                        offending: Some(change.package.clone()),
                    });
                }
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.cargo.build(&project.root).await
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

    async fn snapshot_lock(&self, project: &Project) -> Result<LockSnapshot> {
        let mut files = Vec::new();
        if let Ok(bytes) = std::fs::read(project.root.join("Cargo.lock")) {
            files.push((Utf8PathBuf::from("Cargo.lock"), bytes));
        }
        Ok(LockSnapshot { files })
    }

    async fn restore_lock(&self, project: &Project, snapshot: &LockSnapshot) -> Result<()> {
        for (rel, bytes) in &snapshot.files {
            std::fs::write(project.root.join(rel), bytes)?;
        }
        Ok(())
    }
}
