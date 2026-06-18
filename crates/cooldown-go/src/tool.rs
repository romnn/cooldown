//! The Go [`Tool`]: detection, the resolved module graph, classified releases (GOPROXY +
//! `x/mod` semantics), the locked-pin metadata `check` evaluates, and `go`-driven apply/build.

use crate::gocmd::{Go, GoModule};
use crate::mutation;
use crate::proxy::GoProxy;
use crate::semver;
use async_trait::async_trait;
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, Change, DepScope, Dependency, FetchContext,
    MajorKey, NativePolicyLayer, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, Release, ReleaseOrder, ReleaseQuality, Result, SkipReason, Skipped,
    ToolId, ToolRead, ToolWrite, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;

/// The [`ToolId`] for the Go adapter.
pub const GO_ID: ToolId = ToolId("go");

/// The Go adapter, constructed from a [`GoProxy`] (itself built over the shared HTTP layer).
pub struct GoTool {
    proxy: GoProxy,
    go: Go,
}

impl GoTool {
    /// Creates the adapter over `proxy`, using a default [`Go`] driver for resolution/apply.
    #[must_use]
    pub fn new(proxy: GoProxy) -> Self {
        GoTool {
            proxy,
            go: Go::new(),
        }
    }

    /// Convenience: build the proxy from `GOPROXY` over the shared HTTP client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        GoTool::new(GoProxy::from_env(http))
    }

    fn registry(&self) -> Option<String> {
        self.proxy.registry_name()
    }
}

/// Classify a version string into a [`ReleaseQuality`].
#[must_use]
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

/// The `MajorKey` for a module *path* — the `/vN` suffix (`""` for v0/v1/+incompatible base paths).
fn major_key_for_path(path: &str) -> MajorKey {
    let (_, path_major, _) = semver::split_path_version(path);
    MajorKey(path_major)
}

/// The update kind of `cand` relative to `current`, by semver.
fn classify_kind(current: &str, cand: &str) -> Option<cooldown_core::UpdateKind> {
    use cooldown_core::UpdateKind::{Major, Minor, Patch};
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
#[must_use]
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
        // `i` is a release index, which cannot realistically approach `u32::MAX`; saturate
        // rather than truncate so the big-endian order token stays monotonic.
        let order = u32::try_from(i).unwrap_or(u32::MAX);
        r.order = ReleaseOrder(order.to_be_bytes().to_vec());
    }
    releases
}

fn skipped_on_apply_error(change: &Change, error: cooldown_core::CoreError) -> Result<Skipped> {
    if error.is_tool_spawn_failure() {
        return Err(error);
    }
    Ok(Skipped {
        change: change.clone(),
        reason: SkipReason::ResolverConflict,
        offending: Some(change.package.clone()),
    })
}

impl GoTool {
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
    async fn discover_major_paths(&self, module: &str) -> Result<Vec<String>> {
        let (prefix, path_major, ok) = semver::split_path_version(module);
        if !ok {
            return Ok(Vec::new());
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
            let list = self.proxy.list(&p).await?;
            if list.is_empty() {
                misses += 1;
            } else {
                found.push(p);
                misses = 0;
            }
            n += 1;
        }
        Ok(found)
    }
}

#[async_trait]
impl ToolRead for GoTool {
    fn id(&self) -> ToolId {
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

    fn project_marker(&self) -> ProjectMarker {
        // Go multi-module repos nest independent modules, so every `go.mod` is its own project
        // (not a workspace root).
        ProjectMarker {
            lockfile: "go.mod",
            manifest: "go.mod",
            workspace_root: false,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let modules = self.go.list_modules(&project.root).await?;
        let main_path = modules
            .iter()
            .find(|m| m.main)
            .map(|m| m.path.clone())
            .unwrap_or_default();
        let floors = self.go.mod_graph_floors(&project.root, &main_path).await?;

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

    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        let module = &dep.package.name;

        // (source_path, raw_release) across the module's own path and discovered higher majors.
        let mut raw: Vec<(String, cooldown_core::RawRelease)> = Vec::new();
        for rr in self.proxy.releases(&dep.package).await? {
            raw.push((module.clone(), rr));
        }
        if candidates == CandidateScope::AllowCrossMajor {
            for path in self.discover_major_paths(module).await? {
                let pkg = cooldown_core::PackageId::new(GO_ID, path.clone(), self.registry());
                let list = self.proxy.releases(&pkg).await?;
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

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
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
}

#[async_trait]
impl ToolWrite for GoTool {
    async fn mutation_journal(
        &self,
        project: &Project,
        plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        mutation::mutation_journal(&project.root, plan)
    }

    async fn apply(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            let target_path = &change.package.name;
            match self
                .go
                .get(&project.root, target_path, change.to.as_str())
                .await
            {
                Ok(()) => {
                    // Cross-major path change → rewrite imports old→new before accepting the trial.
                    if let Some(old_path) = mutation::old_import_path(change)
                        && old_path != *target_path
                    {
                        mutation::rewrite_imports(&project.root, &old_path, target_path, journal)?;
                    }
                    report.applied.push(change.clone());
                }
                Err(e) => {
                    // MVS/resolver rejection → a skip (Ok data), not a hard error.
                    report.skipped.push(skipped_on_apply_error(change, e)?);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::ProxyBase;
    use camino::{Utf8Path, Utf8PathBuf};
    use cooldown_core::{
        ArtifactScope, Dependency, FetchContext, PackageId, Project, RawRelease, UpdateKind,
    };
    use cooldown_registry::HttpOptions;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    struct TestServer {
        base_url: String,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl TestServer {
        fn new(routes: HashMap<String, (u16, &'static str)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
            listener
                .set_nonblocking(true)
                .expect("listener nonblocking");
            let addr = listener.local_addr().expect("local addr");
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let handle = thread::spawn(move || {
                while !stop_thread.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let mut request_line = String::new();
                            let mut reader =
                                BufReader::new(stream.try_clone().expect("clone stream"));
                            let _ = reader.read_line(&mut request_line);
                            let path = request_line
                                .split_whitespace()
                                .nth(1)
                                .unwrap_or("/")
                                .to_string();
                            let (status, body) =
                                routes.get(&path).copied().unwrap_or((404, "not found"));
                            let reason = match status {
                                200 => "OK",
                                500 => "Internal Server Error",
                                _ => "Not Found",
                            };
                            let _ = write!(
                                stream,
                                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.flush();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            });
            TestServer {
                base_url: format!("http://{addr}"),
                stop,
                handle: Some(handle),
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = std::net::TcpStream::connect(self.base_url.trim_start_matches("http://"));
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

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
        assert_eq!(
            major_key_for_path("example.com/foo"),
            MajorKey(String::new())
        );
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
            mutation::old_import_path(&change),
            Some("example.com/foo".to_string())
        );

        let change3 = Change {
            package: PackageId::new(GO_ID, "example.com/foo/v3", None),
            from: Version::new("v2.3.0"),
            to: Version::new("v3.0.0"),
            kind: UpdateKind::Major,
        };
        assert_eq!(
            mutation::old_import_path(&change3),
            Some("example.com/foo/v2".to_string())
        );
    }

    fn dep(name: &str, current: &str) -> Dependency {
        Dependency {
            package: PackageId::new(GO_ID, name, None),
            current: Version::new(current),
            current_quality: classify_quality(current),
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
        }
    }

    fn project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            kind: GO_ID,
            manifest: root.join("go.mod"),
        }
    }

    fn fetch_ctx(project: &Project) -> FetchContext<'_> {
        FetchContext {
            project,
            environments: &[],
            artifacts: ArtifactScope::Environment,
        }
    }

    #[test]
    fn apply_spawn_failure_is_not_downgraded_to_skip() {
        let change = Change {
            package: PackageId::new(GO_ID, "example.com/foo", None),
            from: Version::new("v1.0.0"),
            to: Version::new("v1.0.1"),
            kind: UpdateKind::Patch,
        };
        let err = cooldown_core::CoreError::ToolSpawn {
            tool: "go".into(),
            detail: "spawn failed".into(),
        };

        let result = skipped_on_apply_error(&change, err);
        assert!(matches!(
            result,
            Err(cooldown_core::CoreError::ToolSpawn { .. })
        ));
    }

    #[tokio::test]
    async fn mutation_journal_restore_reverts_import_rewrites_and_removes_created_go_sum() {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(repo.path().to_path_buf()).expect("utf8 path");
        let manifest = root.join("go.mod");
        let source = root.join("main.go");
        std::fs::write(&manifest, "module example.com/demo\n\ngo 1.24\n").expect("write go.mod");
        std::fs::write(&source, "package main\n\nimport \"example.com/foo\"\n")
            .expect("write source");
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let http = SharedHttp::new(cache_dir.path(), HttpOptions::default()).expect("http");
        let eco = GoTool::new(GoProxy::new(http, Vec::new()));
        let project = Project {
            root: root.clone(),
            kind: GO_ID,
            manifest,
        };

        let journal = eco
            .mutation_journal(
                &project,
                &Plan {
                    changes: vec![Change {
                        package: PackageId::new(GO_ID, "example.com/foo/v2", None),
                        from: Version::new("v1.0.0"),
                        to: Version::new("v2.0.0"),
                        kind: UpdateKind::Major,
                    }],
                },
            )
            .await
            .expect("journal");
        std::fs::write(&source, "package main\n\nimport \"example.com/foo/v2\"\n")
            .expect("rewrite source");
        std::fs::write(root.join("go.sum"), "generated").expect("write go.sum");

        journal.restore(&project.root).expect("restore");
        assert_eq!(
            std::fs::read_to_string(&source).expect("read restored source"),
            "package main\n\nimport \"example.com/foo\"\n"
        );
        assert!(!root.join("go.sum").exists());
    }

    #[tokio::test]
    async fn releases_skip_cross_major_probe_when_scope_is_current_major_only() {
        let routes = HashMap::from([
            ("/example.com/mod/@v/list".to_string(), (200, "v1.0.0\n")),
            (
                "/example.com/mod/@v/v1.0.0.info".to_string(),
                (200, r#"{"Version":"v1.0.0","Time":"2026-01-01T00:00:00Z"}"#),
            ),
            (
                "/example.com/mod/v2/@v/list".to_string(),
                (500, "cross-major probe should be skipped"),
            ),
        ]);
        let server = TestServer::new(routes);
        let cache = tempfile::tempdir().expect("tempdir");
        let http = SharedHttp::new(cache.path(), HttpOptions::default()).expect("http");
        let eco = GoTool::new(GoProxy::new(
            http,
            vec![ProxyBase {
                url: server.base_url.clone(),
                fallback_on_errors: false,
            }],
        ));
        let repo = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(repo.path().to_path_buf()).expect("utf8 path");
        let project = project(&root);

        let releases = eco
            .releases(
                &dep("example.com/mod", "v1.0.0"),
                &fetch_ctx(&project),
                CandidateScope::CurrentMajorOnly,
            )
            .await
            .expect("same-major release fetch");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].version.as_str(), "v1.0.0");
    }

    #[tokio::test]
    async fn releases_fail_closed_on_cross_major_probe_error_when_enabled() {
        let routes = HashMap::from([
            ("/example.com/mod/@v/list".to_string(), (200, "v1.0.0\n")),
            (
                "/example.com/mod/@v/v1.0.0.info".to_string(),
                (200, r#"{"Version":"v1.0.0","Time":"2026-01-01T00:00:00Z"}"#),
            ),
            (
                "/example.com/mod/v2/@v/list".to_string(),
                (500, "cross-major probe should fail"),
            ),
        ]);
        let server = TestServer::new(routes);
        let cache = tempfile::tempdir().expect("tempdir");
        let http = SharedHttp::new(cache.path(), HttpOptions::default()).expect("http");
        let eco = GoTool::new(GoProxy::new(
            http,
            vec![ProxyBase {
                url: server.base_url.clone(),
                fallback_on_errors: false,
            }],
        ));
        let repo = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(repo.path().to_path_buf()).expect("utf8 path");
        let project = project(&root);

        let err = eco
            .releases(
                &dep("example.com/mod", "v1.0.0"),
                &fetch_ctx(&project),
                CandidateScope::AllowCrossMajor,
            )
            .await
            .expect_err("cross-major probe must fail closed");
        assert!(err.is_transient());
    }
}
