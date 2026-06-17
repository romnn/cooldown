//! The Go [`Ecosystem`]: detection, the resolved module graph, classified releases (GOPROXY +
//! `x/mod` semantics), the locked-pin metadata `check` evaluates, and `go`-driven apply/build.

use crate::gocmd::{Go, GoModule};
use crate::proxy::GoProxy;
use crate::semver;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    ApplyReport, Capabilities, Change, DepScope, Dependency, Ecosystem, EcosystemId, LockSnapshot,
    MajorKey, NativePolicyLayer, PackageRegistry, Plan, Project, Release, ReleaseOrder,
    ReleaseQuality, Result, SkipReason, Skipped, TargetContext, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;

pub const GO_ID: EcosystemId = EcosystemId("go");

/// The Go adapter, constructed from a [`GoProxy`] (itself built over the shared HTTP layer).
pub struct GoEcosystem {
    proxy: GoProxy,
    go: Go,
}

impl GoEcosystem {
    pub fn new(proxy: GoProxy) -> Self {
        GoEcosystem {
            proxy,
            go: Go::new(),
        }
    }

    /// Convenience: build the proxy from `GOPROXY` over the shared HTTP client.
    pub fn from_http(http: SharedHttp) -> Self {
        GoEcosystem::new(GoProxy::from_env(http))
    }

    fn registry(&self) -> Option<String> {
        self.proxy.registry_name()
    }
}

/// Classify a version string into a [`ReleaseQuality`].
pub fn classify_quality(v: &str) -> ReleaseQuality {
    if semver::is_pseudo(v) {
        ReleaseQuality::Pseudo
    } else if semver::is_incompatible(v) {
        ReleaseQuality::Incompatible
    } else if !semver::prerelease(v).is_empty() {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

/// The MajorKey for a module *path* — the `/vN` suffix (`""` for v0/v1/+incompatible base paths).
fn major_key_for_path(path: &str) -> MajorKey {
    let (_, path_major, _) = semver::split_path_version(path);
    MajorKey(path_major)
}

/// The update kind of `cand` relative to `current`, by semver.
fn classify_kind(current: &str, cand: &str) -> Option<cooldown_core::UpdateKind> {
    use cooldown_core::UpdateKind::*;
    if !semver::is_valid(current) || !semver::is_valid(cand) {
        return None;
    }
    if semver::major(current) != semver::major(cand) {
        Some(Major)
    } else if semver::major_minor(current) != semver::major_minor(cand) {
        Some(Minor)
    } else {
        Some(Patch)
    }
}

/// Classify a set of source-tagged raw releases into ordered, deduped [`Release`]s: assign quality
/// and `kind_from_current`, derive each release's `MajorKey` from the path it came from, sort by
/// semver, dedupe by canonical version, and assign a within-package order token. Pure (no I/O), so
/// the adapter's classification logic is unit-testable without network.
pub fn build_releases(
    current: &str,
    raw: Vec<(String, cooldown_core::RawRelease)>,
) -> Vec<Release> {
    let mut releases: Vec<Release> = raw
        .into_iter()
        .filter(|(_, rr)| semver::is_valid(rr.version.as_str()))
        .map(|(path, rr)| {
            let v = rr.version.as_str();
            Release {
                version: rr.version.clone(),
                order: ReleaseOrder(Vec::new()),
                major: major_key_for_path(&path),
                kind_from_current: classify_kind(current, v),
                published_at: rr.published_at,
                yanked: rr.yanked,
                quality: classify_quality(v),
            }
        })
        .collect();

    // Deduplicate by canonical version (the same tag can appear from base + a /vN probe). Within an
    // equal-canonical group, sort a release that HAS a publish time ahead of one that does not, so
    // `dedup_by` (which keeps the first) preserves the dated record.
    releases.sort_by(|a, b| {
        semver::compare(a.version.as_str(), b.version.as_str())
            .then_with(|| a.published_at.is_none().cmp(&b.published_at.is_none()))
    });
    releases.dedup_by(|a, b| {
        semver::canonical_version(a.version.as_str())
            == semver::canonical_version(b.version.as_str())
    });
    for (i, r) in releases.iter_mut().enumerate() {
        r.order = ReleaseOrder((i as u32).to_be_bytes().to_vec());
    }
    releases
}

fn find_go_mods(root: &Utf8Path, out: &mut Vec<Utf8PathBuf>) {
    let manifest = root.join("go.mod");
    if manifest.is_file() {
        out.push(manifest);
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            "vendor" | ".git" | "node_modules" | "testdata" | "target"
        ) {
            continue;
        }
        if let Ok(child) = Utf8PathBuf::from_path_buf(path) {
            find_go_mods(&child, out);
        }
    }
}

impl GoEcosystem {
    /// Build a `Dependency` from a resolved module + the MVS-floor map.
    fn dependency_of(
        &self,
        m: &GoModule,
        floors: &std::collections::HashMap<String, String>,
    ) -> Option<Dependency> {
        if m.main || m.is_local_replace() {
            return None;
        }
        let path = m.effective_path().to_string();
        let version = m.effective_version()?.to_string();
        let graph_floor = floors.get(&path).map(|v| Version::new(v.clone()));
        Some(Dependency {
            package: cooldown_core::PackageId::new(GO_ID, path, self.registry()),
            current: Version::new(version.clone()),
            current_quality: classify_quality(&version),
            direct: !m.indirect,
            artifacts: Vec::new(),
            graph_floor,
        })
    }

    /// Discover higher major-version module paths (`prefix/v2`, `/v3`, …) for cross-major candidates.
    async fn discover_major_paths(&self, module: &str) -> Vec<String> {
        let (prefix, path_major, ok) = semver::split_path_version(module);
        if !ok {
            return Vec::new();
        }
        let current_major: u32 = if path_major.is_empty() {
            1
        } else {
            path_major
                .trim_start_matches(['/', '.'])
                .trim_start_matches('v')
                .parse()
                .unwrap_or(1)
        };
        let mut found = Vec::new();
        let mut misses = 0;
        let mut n = current_major + 1;
        while misses < 2 && n <= current_major + 8 {
            let p = semver::major_path(&prefix, n);
            match self.proxy.list(&p).await {
                Ok(list) if !list.is_empty() => {
                    found.push(p);
                    misses = 0;
                }
                _ => misses += 1,
            }
            n += 1;
        }
        found
    }

    /// Rewrite import paths `old` → `new` across `.go` files (best-effort `/vN` migration).
    fn rewrite_imports(root: &Utf8Path, old: &str, new: &str) -> usize {
        fn walk(dir: &Utf8Path, old: &str, new: &str, count: &mut usize) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    if matches!(name.as_ref(), "vendor" | ".git" | "testdata") {
                        continue;
                    }
                    if let Some(up) = Utf8Path::from_path(&p) {
                        walk(up, old, new, count);
                    }
                } else if p.extension().and_then(|s| s.to_str()) == Some("go") {
                    if let Ok(src) = std::fs::read_to_string(&p) {
                        let replaced = src
                            .replace(&format!("\"{old}\""), &format!("\"{new}\""))
                            .replace(&format!("\"{old}/"), &format!("\"{new}/"));
                        if replaced != src && std::fs::write(&p, replaced).is_ok() {
                            *count += 1;
                        }
                    }
                }
            }
        }
        let mut count = 0;
        walk(root, old, new, &mut count);
        count
    }
}

#[async_trait]
impl Ecosystem for GoEcosystem {
    fn id(&self) -> EcosystemId {
        GO_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: true,
            has_incompatible: true,
            has_dist_tags: false,
            can_sync: false,
            artifact_granular: false,
        }
    }

    async fn detect(&self, root: &Utf8Path) -> Result<Vec<Project>> {
        let mut manifests = Vec::new();
        find_go_mods(root, &mut manifests);
        Ok(manifests
            .into_iter()
            .map(|manifest| Project {
                root: manifest
                    .parent()
                    .map(|p| p.to_owned())
                    .unwrap_or_else(|| root.to_owned()),
                kind: GO_ID,
                manifest,
            })
            .collect())
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let modules = self.go.list_modules(&project.root).await?;
        let main_path = modules
            .iter()
            .find(|m| m.main)
            .map(|m| m.path.clone())
            .unwrap_or_default();
        let floors = self
            .go
            .mod_graph_floors(&project.root, &main_path)
            .await
            .unwrap_or_default();

        let mut deps = Vec::new();
        for m in &modules {
            let Some(dep) = self.dependency_of(m, &floors) else {
                continue;
            };
            if scope == DepScope::Direct && !dep.direct {
                continue;
            }
            deps.push(dep);
        }
        Ok(deps)
    }

    async fn releases(&self, dep: &Dependency, _ctx: &TargetContext<'_>) -> Result<Vec<Release>> {
        let module = &dep.package.name;

        // (source_path, raw_release) across the module's own path and discovered higher majors.
        let mut raw: Vec<(String, cooldown_core::RawRelease)> = Vec::new();
        for rr in self.proxy.releases(&dep.package).await? {
            raw.push((module.clone(), rr));
        }
        for path in self.discover_major_paths(module).await {
            let pkg = cooldown_core::PackageId::new(GO_ID, path.clone(), self.registry());
            if let Ok(list) = self.proxy.releases(&pkg).await {
                for rr in list {
                    raw.push((path.clone(), rr));
                }
            }
        }

        // Ensure the current pin is present so the core can locate its order.
        let current = dep.current.as_str();
        if !raw.iter().any(|(_, rr)| rr.version.as_str() == current) {
            let time = self
                .proxy
                .published_at(&dep.package, &dep.current, &[])
                .await
                .unwrap_or(None);
            raw.push((
                module.clone(),
                cooldown_core::RawRelease {
                    version: dep.current.clone(),
                    published_at: time,
                    yanked: false,
                    artifacts: Vec::new(),
                },
            ));
        }

        Ok(build_releases(current, raw))
    }

    async fn locked_release(&self, dep: &Dependency, _ctx: &TargetContext<'_>) -> Result<Release> {
        let time = self
            .proxy
            .published_at(&dep.package, &dep.current, &[])
            .await?;
        Ok(Release {
            version: dep.current.clone(),
            order: ReleaseOrder(Vec::new()),
            major: major_key_for_path(&dep.package.name),
            kind_from_current: None,
            published_at: time,
            yanked: false,
            quality: dep.current_quality,
        })
    }

    async fn native_policy(&self, _project: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None) // Go has no native cooldown config.
    }

    async fn apply(&self, project: &Project, plan: &Plan) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            let target_path = &change.package.name;
            match self
                .go
                .get(&project.root, target_path, change.to.as_str())
                .await
            {
                Ok(()) => {
                    // Cross-major path change → rewrite imports old→new (best-effort).
                    if let Some(old_path) = old_import_path(change) {
                        if old_path != *target_path {
                            GoEcosystem::rewrite_imports(&project.root, &old_path, target_path);
                        }
                    }
                    report.applied.push(change.clone());
                }
                Err(e) => {
                    // MVS/resolver rejection → a skip (Ok data), not a hard error.
                    report.skipped.push(Skipped {
                        change: change.clone(),
                        reason: SkipReason::ResolverConflict,
                        offending: Some(change.package.clone()),
                    });
                    let _ = e;
                }
            }
        }
        // Re-tidy once after applying the (single-change) plan.
        if !report.applied.is_empty() {
            self.go.mod_tidy(&project.root).await?;
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.go.build(&project.root).await
    }

    async fn verify_lock_current(&self, project: &Project) -> Result<VerifyReport> {
        match self.go.mod_tidy_is_clean(&project.root).await {
            Ok(true) => Ok(VerifyReport {
                ok: true,
                detail: "go.mod/go.sum are tidy".to_string(),
            }),
            Ok(false) => Ok(VerifyReport {
                ok: false,
                detail: "go.mod/go.sum are stale; run `go mod tidy`".to_string(),
            }),
            Err(e) => Err(e),
        }
    }

    async fn snapshot_lock(&self, project: &Project) -> Result<LockSnapshot> {
        let mut files = Vec::new();
        for name in ["go.mod", "go.sum"] {
            let path = project.root.join(name);
            if let Ok(bytes) = std::fs::read(&path) {
                files.push((Utf8PathBuf::from(name), bytes));
            }
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

/// The pre-upgrade import path for a change, derived from the `from` version's major. Used to
/// rewrite imports on a cross-major `/vN` upgrade.
fn old_import_path(change: &Change) -> Option<String> {
    let new_path = &change.package.name;
    let (prefix, _, ok) = semver::split_path_version(new_path);
    if !ok {
        return None;
    }
    let from_major = semver::major(change.from.as_str());
    let n: u32 = from_major.trim_start_matches('v').parse().ok()?;
    if n <= 1 {
        Some(prefix)
    } else {
        Some(semver::major_path(&prefix, n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cooldown_core::{PackageId, RawRelease, UpdateKind};

    fn rr(v: &str, t: Option<&str>) -> RawRelease {
        RawRelease {
            version: Version::new(v),
            published_at: t.map(|s| s.parse().unwrap()),
            yanked: false,
            artifacts: Vec::new(),
        }
    }

    #[test]
    fn quality_classification() {
        assert_eq!(classify_quality("v1.2.3"), ReleaseQuality::Stable);
        assert_eq!(classify_quality("v1.2.3-rc1"), ReleaseQuality::Prerelease);
        assert_eq!(
            classify_quality("v3.0.0+incompatible"),
            ReleaseQuality::Incompatible
        );
        assert_eq!(
            classify_quality("v0.0.0-20191109021931-daa7c04131f5"), // spellcheck:ignore-line
            ReleaseQuality::Pseudo
        );
    }

    #[test]
    fn kind_classification() {
        assert_eq!(classify_kind("v1.2.3", "v1.2.4"), Some(UpdateKind::Patch));
        assert_eq!(classify_kind("v1.2.3", "v1.3.0"), Some(UpdateKind::Minor));
        assert_eq!(classify_kind("v1.2.3", "v2.0.0"), Some(UpdateKind::Major));
        assert_eq!(
            classify_kind("v1.2.3", "v3.0.0+incompatible"),
            Some(UpdateKind::Major)
        );
    }

    #[test]
    fn major_key_is_per_path() {
        assert_eq!(major_key_for_path("example.com/foo"), MajorKey("".into()));
        assert_eq!(
            major_key_for_path("example.com/foo/v2"),
            MajorKey("/v2".into())
        );
    }

    #[test]
    fn build_releases_orders_dedupes_and_tags() {
        let raw = vec![
            (
                "example.com/foo".to_string(),
                rr("v1.1.0", Some("2026-02-01T00:00:00Z")),
            ),
            (
                "example.com/foo".to_string(),
                rr("v1.0.0", Some("2026-01-01T00:00:00Z")),
            ),
            (
                "example.com/foo".to_string(),
                rr("v1.1.0", Some("2026-02-01T00:00:00Z")),
            ), // dup
            (
                "example.com/foo/v2".to_string(),
                rr("v2.0.0", Some("2026-03-01T00:00:00Z")),
            ),
            ("example.com/foo".to_string(), rr("not-semver", None)), // dropped
        ];
        let rels = build_releases("v1.0.0", raw);
        let versions: Vec<&str> = rels.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(
            versions,
            vec!["v1.0.0", "v1.1.0", "v2.0.0"],
            "sorted + deduped + invalid dropped"
        );
        // Orders strictly ascending.
        assert!(rels[0].order < rels[1].order && rels[1].order < rels[2].order);
        // MajorKey reflects the source path.
        assert_eq!(rels[2].major, MajorKey("/v2".into()));
        // kind_from_current relative to v1.0.0.
        assert_eq!(rels[1].kind_from_current, Some(UpdateKind::Minor));
        assert_eq!(rels[2].kind_from_current, Some(UpdateKind::Major));
    }

    #[test]
    fn old_import_path_for_cross_major() {
        let change = Change {
            package: PackageId::new(GO_ID, "example.com/foo/v2", None),
            from: Version::new("v1.5.0"),
            to: Version::new("v2.0.0"),
            kind: UpdateKind::Major,
        };
        assert_eq!(
            old_import_path(&change),
            Some("example.com/foo".to_string())
        );

        let change3 = Change {
            package: PackageId::new(GO_ID, "example.com/foo/v3", None),
            from: Version::new("v2.3.0"),
            to: Version::new("v3.0.0"),
            kind: UpdateKind::Major,
        };
        assert_eq!(
            old_import_path(&change3),
            Some("example.com/foo/v2".to_string())
        );
    }
}
