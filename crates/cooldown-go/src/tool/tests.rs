use super::apply::skipped_on_apply_error;
use super::releases::{build_releases, classify_kind, classify_quality, major_key_for_path};
use super::*;
use crate::proxy::ProxyBase;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    ArtifactScope, CandidateScope, Change, Dependency, FetchContext, MajorKey, PackageId, Plan,
    Project, RawRelease, ReleaseQuality, UpdateKind, Version,
};
use cooldown_registry::{HttpOptions, SharedHttp};
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
                        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
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
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
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

fn rr(version: &str, published_at: Option<&str>) -> RawRelease {
    RawRelease {
        version: Version::new(version),
        published_at: published_at.map(|value| value.parse().unwrap()),
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
        ),
        (
            "example.com/foo/v2".to_string(),
            rr("v2.0.0", Some("2026-03-01T00:00:00Z")),
        ),
        ("example.com/foo".to_string(), rr("not-semver", None)),
    ];
    let releases = build_releases("v1.0.0", raw);
    let versions: Vec<&str> = releases
        .iter()
        .map(|release| release.version.as_str())
        .collect();
    assert_eq!(
        versions,
        vec!["v1.0.0", "v1.1.0", "v2.0.0"],
        "sorted + deduped + invalid dropped"
    );
    assert!(releases[0].order < releases[1].order && releases[1].order < releases[2].order);
    assert_eq!(releases[2].major, MajorKey("/v2".into()));
    assert_eq!(releases[1].kind_from_current, Some(UpdateKind::Minor));
    assert_eq!(releases[2].kind_from_current, Some(UpdateKind::Major));
}

#[test]
fn build_releases_applies_go_incompatible_rule() {
    // A module-aware pin (a compatible, go.mod-bearing v0.x version like k8s.io/client-go) must
    // never see a bare `+incompatible` tag as a candidate: `go list -m -versions` hides the ancient
    // v11.0.0+incompatible because client-go adopted go.mod, so cooldown must too.
    let compatible = vec![
        (
            "k8s.io/client-go".to_string(),
            rr("v0.36.1", Some("2026-01-01T00:00:00Z")),
        ),
        (
            "k8s.io/client-go".to_string(),
            rr("v0.36.2", Some("2026-02-01T00:00:00Z")),
        ),
        (
            "k8s.io/client-go".to_string(),
            rr("v11.0.0+incompatible", Some("2018-01-01T00:00:00Z")),
        ),
    ];
    let compatible = build_releases("v0.36.1", compatible);
    let versions: Vec<&str> = compatible
        .iter()
        .map(|release| release.version.as_str())
        .collect();
    assert_eq!(
        versions,
        vec!["v0.36.1", "v0.36.2"],
        "a compatible pin drops the bare +incompatible tag"
    );

    // A pin already on the `+incompatible` line (github.com/docker/cli has no go.mod) keeps moving
    // within it — Go lists and selects the higher `+incompatible` patch.
    let incompatible = vec![
        (
            "github.com/docker/cli".to_string(),
            rr("v29.5.2+incompatible", Some("2026-01-01T00:00:00Z")),
        ),
        (
            "github.com/docker/cli".to_string(),
            rr("v29.5.3+incompatible", Some("2026-02-01T00:00:00Z")),
        ),
    ];
    let incompatible = build_releases("v29.5.2+incompatible", incompatible);
    let versions: Vec<&str> = incompatible
        .iter()
        .map(|release| release.version.as_str())
        .collect();
    assert_eq!(
        versions,
        vec!["v29.5.2+incompatible", "v29.5.3+incompatible"],
        "an +incompatible pin keeps the +incompatible line"
    );
}

#[test]
fn old_import_path_for_cross_major() {
    let change = Change {
        package: PackageId::new(GO_ID, "example.com/foo/v2", None),
        from: Version::new("v1.5.0"),
        to: Version::new("v2.0.0"),
        kind: UpdateKind::Major,
        members: Vec::new(),
    };
    assert_eq!(
        crate::mutation::old_import_path(&change),
        Some("example.com/foo".to_string())
    );

    let change3 = Change {
        package: PackageId::new(GO_ID, "example.com/foo/v3", None),
        from: Version::new("v2.3.0"),
        to: Version::new("v3.0.0"),
        kind: UpdateKind::Major,
        members: Vec::new(),
    };
    assert_eq!(
        crate::mutation::old_import_path(&change3),
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
        members: Vec::new(),
        pinned: false,
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
        members: Vec::new(),
    };
    let error = cooldown_core::CoreError::ToolSpawn {
        tool: "go".into(),
        detail: "spawn failed".into(),
    };

    let result = skipped_on_apply_error(&change, error);
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
    std::fs::write(&source, "package main\n\nimport \"example.com/foo\"\n").expect("write source");
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let http = SharedHttp::new(cache_dir.path(), HttpOptions::default()).expect("http");
    let tool = GoTool::new(crate::proxy::GoProxy::new(http, Vec::new()));
    let project = Project {
        root: root.clone(),
        kind: GO_ID,
        manifest,
    };

    let journal = tool
        .mutation_journal(
            &project,
            &Plan {
                changes: vec![Change {
                    package: PackageId::new(GO_ID, "example.com/foo/v2", None),
                    from: Version::new("v1.0.0"),
                    to: Version::new("v2.0.0"),
                    kind: UpdateKind::Major,
                    members: Vec::new(),
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
    let tool = GoTool::new(crate::proxy::GoProxy::new(
        http,
        vec![ProxyBase {
            url: server.base_url.clone(),
            fallback_on_errors: false,
        }],
    ));
    let repo = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(repo.path().to_path_buf()).expect("utf8 path");
    let project = project(&root);

    let releases = tool
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
    let tool = GoTool::new(crate::proxy::GoProxy::new(
        http,
        vec![ProxyBase {
            url: server.base_url.clone(),
            fallback_on_errors: false,
        }],
    ));
    let repo = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(repo.path().to_path_buf()).expect("utf8 path");
    let project = project(&root);

    let error = tool
        .releases(
            &dep("example.com/mod", "v1.0.0"),
            &fetch_ctx(&project),
            CandidateScope::AllowCrossMajor,
        )
        .await
        .expect_err("cross-major probe must fail closed");
    assert!(error.is_transient());
}
