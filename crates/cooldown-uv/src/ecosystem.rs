//! The Python/uv [`Ecosystem`]: detection, the resolved graph + per-file upload times from
//! `uv.lock`, PyPI as the publish-time fallback, native `[tool.uv]` cooldown config, and
//! `uv`-driven resolution/apply. The core owns the verdict; uv only resolves/applies a window.

use crate::lock::UvLock;
use crate::pypi::{PYPI, PyPi};
use crate::uvcmd::Uv;
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

pub const UV_ID: EcosystemId = EcosystemId("python");

pub struct UvEcosystem {
    pypi: PyPi,
    uv: Uv,
}

impl UvEcosystem {
    pub fn new(pypi: PyPi) -> Self {
        UvEcosystem {
            pypi,
            uv: Uv::new(),
        }
    }
    pub fn from_http(http: SharedHttp) -> Self {
        UvEcosystem::new(PyPi::new(http))
    }
}

fn classify_quality(v: &str) -> ReleaseQuality {
    if version::is_prerelease(v) {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

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

fn find_uv_locks(root: &Utf8Path, out: &mut Vec<Utf8PathBuf>) {
    if root.join("uv.lock").is_file() {
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
        if matches!(
            name.as_ref(),
            ".git" | ".venv" | "node_modules" | "target" | "__pycache__"
        ) {
            continue;
        }
        if let Ok(child) = Utf8PathBuf::from_path_buf(p) {
            find_uv_locks(&child, out);
        }
    }
}

fn read_lock(project: &Project) -> Result<UvLock> {
    let content = std::fs::read_to_string(project.root.join("uv.lock"))?;
    UvLock::parse(&content)
}

/// Parse `[tool.uv] exclude-newer` / `exclude-newer-package` into native rules.
fn parse_native(manifest: &Utf8Path) -> Option<NativePolicyLayer> {
    let content = std::fs::read_to_string(manifest).ok()?;
    let value: toml::Value = content.parse().ok()?;
    let uv = value.get("tool").and_then(|t| t.get("uv"))?;
    let mut rules = Vec::new();

    if let Some(en) = uv.get("exclude-newer").and_then(|v| v.as_str()) {
        if let Some(w) = parse_raw_window(en) {
            rules.push(NativeRule {
                selector: Selector::Default,
                window: w,
            });
        }
    }
    if let Some(table) = uv.get("exclude-newer-package").and_then(|v| v.as_table()) {
        for (pkg, val) in table {
            // One unparsable package pattern must not discard the entire native policy.
            let Ok(glob) = cooldown_core::PatternGlob::new(pkg) else {
                continue;
            };
            let selector = Selector::Package(glob);
            if let Some(false) = val.as_bool() {
                rules.push(NativeRule {
                    selector,
                    window: RawWindow::OptOut,
                });
            } else if let Some(s) = val.as_str() {
                if let Some(w) = parse_raw_window(s) {
                    rules.push(NativeRule {
                        selector,
                        window: w,
                    });
                }
            }
        }
    }
    if rules.is_empty() {
        None
    } else {
        Some(NativePolicyLayer { rules })
    }
}

/// `exclude-newer` is an RFC3339/date cutoff or a friendly/ISO span.
fn parse_raw_window(s: &str) -> Option<RawWindow> {
    if let Ok(t) = cooldown_core::duration::parse_freeze(s) {
        return Some(RawWindow::AbsoluteDate(t));
    }
    cooldown_core::duration::parse_duration(s)
        .ok()
        .map(RawWindow::RelativeDuration)
}

#[async_trait]
impl Ecosystem for UvEcosystem {
    fn id(&self) -> EcosystemId {
        UV_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: true,
            artifact_granular: true,
        }
    }

    async fn detect(&self, root: &Utf8Path) -> Result<Vec<Project>> {
        let mut roots = Vec::new();
        find_uv_locks(root, &mut roots);
        Ok(roots
            .into_iter()
            .map(|dir| Project {
                manifest: dir.join("pyproject.toml"),
                root: dir,
                kind: UV_ID,
            })
            .collect())
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let lock = read_lock(project)?;
        let direct: std::collections::HashSet<String> = lock.direct_names().into_iter().collect();
        let floors = lock.graph_floors();

        let mut deps = Vec::new();
        for pkg in &lock.packages {
            let Some(source) = &pkg.source else { continue };
            if source.is_root() || !source.is_registry() {
                continue; // skip the root project and non-registry (path/git) packages
            }
            let Some(version) = &pkg.version else {
                continue;
            };
            let is_direct = direct.contains(&pkg.name);
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            deps.push(Dependency {
                package: PackageId::new(UV_ID, pkg.name.clone(), Some(PYPI.to_string())),
                current: Version::new(version.clone()),
                current_quality: classify_quality(version),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: floors.get(&pkg.name).map(|v| Version::new(v.clone())),
            });
        }
        Ok(deps)
    }

    async fn releases(&self, dep: &Dependency, _ctx: &TargetContext<'_>) -> Result<Vec<Release>> {
        let raw = self.pypi.releases(&dep.package).await?;
        Ok(build_releases(dep.current.as_str(), raw))
    }

    async fn locked_release(&self, dep: &Dependency, ctx: &TargetContext<'_>) -> Result<Release> {
        // Prefer the lock's recorded per-file upload time; fall back to PyPI.
        let from_lock = read_lock(ctx.project).ok().and_then(|lock| {
            lock.find(&dep.package.name, dep.current.as_str())
                .and_then(|p| p.newest_upload_time())
        });
        let time = match from_lock {
            Some(t) => Some(t),
            None => {
                self.pypi
                    .published_at(&dep.package, &dep.current, &[])
                    .await?
            }
        };
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
                .uv
                .upgrade_to(&project.root, &change.package.name, change.to.as_str())
                .await
            {
                Ok(()) => report.applied.push(change.clone()),
                Err(_) => report.skipped.push(Skipped {
                    change: change.clone(),
                    reason: SkipReason::ResolverConflict,
                    offending: Some(change.package.clone()),
                }),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.uv.sync(&project.root).await
    }

    async fn verify_lock_current(&self, project: &Project) -> Result<VerifyReport> {
        match self.uv.verify_check(&project.root).await {
            Ok(true) => Ok(VerifyReport {
                ok: true,
                detail: "uv.lock is current".into(),
            }),
            Ok(false) => Ok(VerifyReport {
                ok: false,
                detail: "uv.lock is stale; run `uv lock`".into(),
            }),
            Err(e) => Err(e),
        }
    }

    async fn snapshot_lock(&self, project: &Project) -> Result<LockSnapshot> {
        let mut files = Vec::new();
        if let Ok(bytes) = std::fs::read(project.root.join("uv.lock")) {
            files.push((Utf8PathBuf::from("uv.lock"), bytes));
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
