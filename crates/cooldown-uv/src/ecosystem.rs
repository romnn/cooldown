//! The Python/uv [`Ecosystem`]: detection, the resolved graph + per-file upload times from
//! `uv.lock`, `PyPI` as the publish-time fallback, native `[tool.uv]` cooldown config, and
//! `uv`-driven resolution/apply. The core owns the verdict; uv only resolves/applies a window.

use crate::lock::UvLock;
use crate::pypi::{PYPI, PyPi};
use crate::uvcmd::Uv;
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    ApplyReport, Capabilities, Change, CoreError, DepScope, Dependency, EcosystemId, EcosystemRead,
    EcosystemWrite, FetchContext, NativePolicyLayer, NativeRule, PackageId, PackageRegistry, Plan,
    Project, ProjectMutationJournal, RawWindow, Release, ReleaseOrder, ReleaseQuality, Result,
    Selector, SkipReason, Skipped, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use cooldown_toml_util::read_toml_file;

/// The [`EcosystemId`] for the Python/uv adapter.
pub const UV_ID: EcosystemId = EcosystemId("python");

/// The Python/uv implementation of the [`Ecosystem`] port.
///
/// It detects `uv.lock` projects, reads the resolved graph and per-file upload
/// times from the lock (falling back to [`PyPi`] for the publish instant), parses
/// `[tool.uv]` cooldown config as a native policy layer, and drives the `uv` CLI
/// to re-resolve and apply a chosen window. The verdict itself is the core's;
/// uv only resolves and applies.
pub struct UvEcosystem {
    pypi: PyPi,
    uv: Uv,
}

impl UvEcosystem {
    /// Creates the adapter from a configured [`PyPi`] client.
    #[must_use]
    pub fn new(pypi: PyPi) -> Self {
        UvEcosystem {
            pypi,
            uv: Uv::new(),
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`PyPi`] client.
    #[must_use]
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

/// Builds the sorted, deduplicated [`Release`] list the core consumes.
///
/// Unparsable versions are dropped, the rest are sorted by [`version::compare`]
/// and deduplicated, then each is stamped with its update kind relative to
/// `current`, its quality, and an opaque [`ReleaseOrder`] token: the big-endian
/// index, so byte-lexicographic order matches PEP 440 order. The index is widened
/// with [`u32::try_from`] and saturated at [`u32::MAX`], which a real release count
/// can never reach.
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
        let token = u32::try_from(i).unwrap_or(u32::MAX);
        r.order = ReleaseOrder(token.to_be_bytes().to_vec());
    }
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
fn parse_native(manifest: &Utf8Path) -> Result<Option<NativePolicyLayer>> {
    let Some(value) = read_toml_file::<toml::Value>(manifest, "pyproject.toml")? else {
        return Ok(None);
    };
    let Some(uv) = value.get("tool").and_then(|t| t.get("uv")) else {
        return Ok(None);
    };
    let mut rules = Vec::new();

    if let Some(en) = uv.get("exclude-newer").and_then(|v| v.as_str())
        && let Some(w) = parse_raw_window(en)
    {
        rules.push(NativeRule {
            selector: Selector::Default,
            window: w,
        });
    } else if uv.get("exclude-newer").is_some() {
        return Err(CoreError::Config(format!(
            "{manifest}: [tool.uv].exclude-newer must be a valid date/datetime or duration string"
        )));
    }
    if let Some(table) = uv.get("exclude-newer-package").and_then(|v| v.as_table()) {
        for (pkg, val) in table {
            let glob = cooldown_core::PatternGlob::new(pkg).map_err(|e| {
                CoreError::Config(format!(
                    "{manifest}: invalid [tool.uv].exclude-newer-package pattern {pkg:?}: {e}"
                ))
            })?;
            let selector = Selector::Package(glob);
            if let Some(false) = val.as_bool() {
                rules.push(NativeRule {
                    selector,
                    window: RawWindow::OptOut,
                });
            } else if let Some(s) = val.as_str()
                && let Some(w) = parse_raw_window(s)
            {
                rules.push(NativeRule {
                    selector,
                    window: w,
                });
            } else {
                return Err(CoreError::Config(format!(
                    "{manifest}: [tool.uv].exclude-newer-package.{pkg:?} must be false or a valid date/datetime or duration string"
                )));
            }
        }
    } else if uv.get("exclude-newer-package").is_some() {
        return Err(CoreError::Config(format!(
            "{manifest}: [tool.uv].exclude-newer-package must be a table"
        )));
    }
    if rules.is_empty() {
        Ok(None)
    } else {
        Ok(Some(NativePolicyLayer { rules }))
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
impl EcosystemRead for UvEcosystem {
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

    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.pypi.releases(&dep.package).await?;
        Ok(build_releases(dep.current.as_str(), raw))
    }

    async fn locked_release(&self, dep: &Dependency, fetch: &FetchContext<'_>) -> Result<Release> {
        // Prefer the lock's recorded per-file upload time; fall back to PyPI.
        let from_lock = read_lock(fetch.project).ok().and_then(|lock| {
            lock.find(&dep.package.name, dep.current.as_str())
                .and_then(super::lock::Package::newest_upload_time)
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
        parse_native(&project.manifest)
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
}

#[async_trait]
impl EcosystemWrite for UvEcosystem {
    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        Ok(ProjectMutationJournal {
            files: vec![ProjectMutationJournal::capture_file(
                &project.root,
                Utf8Path::new("uv.lock"),
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
                .uv
                .upgrade_to(&project.root, &change.package.name, change.to.as_str())
                .await
            {
                Ok(()) => report.applied.push(change.clone()),
                Err(e) => report.skipped.push(skipped_on_apply_error(change, e)?),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.uv.sync(&project.root).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(contents: &str) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pyproject.toml")).expect("utf8 path");
        std::fs::write(&path, contents).expect("write manifest");
        (dir, path)
    }

    #[test]
    fn parse_native_errors_on_invalid_package_rule() {
        let (_dir, manifest) = write_manifest(
            r#"
[tool.uv]
exclude-newer = "2026-06-01"

[tool.uv.exclude-newer-package]
"[" = false
"#,
        );

        let err = parse_native(&manifest).expect_err("invalid native config must fail");
        assert!(matches!(err, CoreError::Config(_)));
    }

    #[test]
    fn parse_native_reads_valid_rules() {
        let (_dir, manifest) = write_manifest(
            r#"
[tool.uv]
exclude-newer = "2026-06-01"

[tool.uv.exclude-newer-package]
requests = false
"#,
        );

        let layer = parse_native(&manifest)
            .expect("valid native config")
            .expect("native layer");
        assert_eq!(layer.rules.len(), 2);
        assert!(matches!(layer.rules[0].window, RawWindow::AbsoluteDate(_)));
        assert!(matches!(layer.rules[1].window, RawWindow::OptOut));
    }

    #[test]
    fn apply_spawn_failure_is_not_downgraded_to_skip() {
        let change = Change {
            package: PackageId::new(UV_ID, "requests", Some(PYPI.to_string())),
            from: Version::new("2.34.1"),
            to: Version::new("2.34.2"),
            kind: cooldown_core::UpdateKind::Patch,
        };
        let err = CoreError::ToolSpawn {
            tool: "uv".into(),
            detail: "spawn failed".into(),
        };

        let result = skipped_on_apply_error(&change, err);
        assert!(matches!(result, Err(CoreError::ToolSpawn { .. })));
    }

    #[tokio::test]
    async fn mutation_journal_restore_removes_lock_created_after_capture() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let manifest = root.join("pyproject.toml");
        std::fs::write(
            &manifest,
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let eco = UvEcosystem::from_http(
            cooldown_registry::SharedHttp::new(
                cache_dir.path(),
                cooldown_registry::HttpOptions::default(),
            )
            .expect("http"),
        );
        let project = Project {
            root: root.clone(),
            kind: UV_ID,
            manifest,
        };

        let journal = eco
            .mutation_journal(&project, &Plan::default())
            .await
            .expect("journal");
        let lock = root.join("uv.lock");
        std::fs::write(&lock, "generated").expect("write lock");

        journal.restore(&project.root).expect("restore");
        assert!(!lock.exists());
    }
}
