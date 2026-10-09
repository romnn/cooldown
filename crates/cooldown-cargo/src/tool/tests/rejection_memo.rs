//! Rejected resolver attempts are reused only for identical, safely restored inputs.

use super::*;
use std::os::unix::fs::PermissionsExt as _;

struct MemoFixture {
    directory: tempfile::TempDir,
    project: Project,
    tool: CargoTool,
    change: Change,
}

impl MemoFixture {
    fn new() -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::create_dir(root.join("other"))?;
        std::fs::write(
            root.join("Cargo.toml"),
            indoc! {r#"
            [package]
            name = "app"
            version = "0.1.0"
            [workspace]
            members = ["other"]
            [dependencies]
            demo = "1"
        "#},
        )?;
        std::fs::write(
            root.join("other/Cargo.toml"),
            indoc! {r#"
            [package]
            name = "other"
            version = "0.1.0"
        "#},
        )?;
        std::fs::write(
            root.join("Cargo.lock"),
            indoc! {r#"
            version = 4
            [[package]]
            name = "demo"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        "#},
        )?;
        let script = root.join("fake-cargo");
        std::fs::write(
            &script,
            indoc! {r#"
            #!/bin/sh
            set -eu
            printf '%s\n' "$*" >> invocations
            if [ -f signal ]; then kill -TERM "$$"; fi
            if [ -f infrastructure ]; then
              echo 'error: Permission denied reading the registry cache' >&2
              exit 101
            fi
            if [ -f transport ]; then
              echo 'error: failed to download config.json' >&2
              echo 'Caused by: Could not resolve host: index.crates.io' >&2
              exit 101
            fi
            if [ -f mutate ]; then printf '\n# changed by cargo\n' >> other/Cargo.toml; fi
            if [ "$1" = metadata ]; then
              echo 'error: failed to select a version for `joint seed`' >&2
            else
              echo 'error: failed to select a version for the requirement `demo = "=1.0.0"`' >&2
              echo 'required by package `holder v1.0.0`' >&2
            fi
            exit 101
        "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let mut tool = CargoTool::from_http(SharedHttp::new(
            root.join("cache"),
            cooldown_registry::HttpOptions::default(),
        )?);
        tool.cargo = Cargo::with_bin(script.as_str());
        let project = Project {
            root: root.clone(),
            kind: CARGO_ID,
            manifest: root.join("Cargo.toml"),
            exclude_newer: None,
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        Ok(Self {
            directory,
            project,
            tool,
            change: change("demo", "1.0.0", "2.0.0", false),
        })
    }

    async fn journal(&self) -> Result<ProjectMutationJournal> {
        self.tool
            .mutation_journal(
                &self.project,
                &Plan {
                    changes: vec![self.change.clone()],
                    ..Default::default()
                },
            )
            .await
    }

    fn calls(&self) -> eyre::Result<usize> {
        let path = self.directory.path().join("invocations");
        Ok(if path.exists() {
            std::fs::read_to_string(path)?.lines().count()
        } else {
            0
        })
    }

    async fn attempt(
        &self,
        operation: MemoOperation,
        journal: &ProjectMutationJournal,
        stats: &mut PinBatchStats,
    ) -> Result<()> {
        match operation {
            MemoOperation::Precise => {
                self.precise(journal, stats).await?;
            }
            MemoOperation::Seed => {
                let moves = [PlannedNodeMove {
                    name: self.change.package.name.clone(),
                    from: self.change.from.to_string(),
                    to: self.change.to.to_string(),
                }];
                let changes = [&self.change];
                assert!(
                    self.tool
                        .seed_resolve(
                            &self.project,
                            SeedGroup {
                                moves: &moves,
                                changes: &changes
                            },
                            journal,
                            None,
                            stats,
                        )
                        .await?
                        .is_none()
                );
            }
            MemoOperation::Widen => {
                let mut memo_stats = MemoStats::default();
                let result = self
                    .tool
                    .tentative_widen_logged(
                        WidenProbe {
                            project: &self.project,
                            candidates: &[&self.change],
                            protected: &[],
                            followers: &[],
                            journal,
                            observer: None,
                        },
                        &self.change,
                        &mut PinRejections::new(),
                        &mut memo_stats,
                    )
                    .await;
                stats.memo.include(&memo_stats);
                assert!(matches!(result?, TentativeWiden::Rejected));
            }
        }
        Ok(())
    }

    async fn precise(
        &self,
        journal: &ProjectMutationJournal,
        stats: &mut PinBatchStats,
    ) -> Result<PinRejections> {
        let mut rejections = PinRejections::new();
        self.tool
            .pin_individually(
                &self.project,
                &[&self.change],
                journal,
                None,
                &mut rejections,
                stats,
            )
            .await?;
        Ok(rejections)
    }
}

#[tokio::test]
async fn identical_precise_rejection_replays_the_exact_detail_without_cargo() -> eyre::Result<()> {
    let fixture = MemoFixture::new()?;
    let journal = fixture.journal().await?;
    let mut stats = PinBatchStats::default();
    let first = fixture.precise(&journal, &mut stats).await?;
    assert_eq!(fixture.calls()?, 1);
    assert!(
        first
            .values()
            .any(|detail| detail.contains("demo") && detail.contains("holder"))
    );
    let second = fixture.precise(&journal, &mut stats).await?;
    assert_eq!(second, first);
    assert_eq!(fixture.calls()?, 1);
    assert_eq!(stats.memo.hits, 1);
    Ok(())
}

#[tokio::test]
async fn a_hit_without_cached_generated_facts_does_not_spawn_discovery_cargo() -> eyre::Result<()> {
    let mut fixture = MemoFixture::new()?;
    let manifest = std::fs::read_to_string(&fixture.project.manifest)?;
    std::fs::write(
        &fixture.project.manifest,
        manifest.replace(
            "members = [\"other\"]",
            "members = [\"other\", \"follower\"]",
        ),
    )?;
    std::fs::create_dir(fixture.project.root.join("follower"))?;
    std::fs::write(
        fixture.project.root.join("follower/Cargo.toml"),
        indoc! {r#"
        [package]
        name = "follower"
        version = "0.1.0"
        [dependencies]
        demo = "1"
    "#},
    )?;
    fixture.project.generated_members =
        cooldown_core::GeneratedMembers::declared(vec!["follower".into()]);
    fixture.tool.generated.lock().await.insert(
        fixture.project.root.clone(),
        Arc::new(GeneratedFacts {
            members: vec![MemberRef {
                name: "follower".into(),
                path: "follower".into(),
            }],
            ..Default::default()
        }),
    );
    let journal = fixture.journal().await?;
    let mut stats = PinBatchStats::default();
    let first = fixture.precise(&journal, &mut stats).await?;
    fixture.tool.generated.lock().await.clear();
    assert_eq!(fixture.precise(&journal, &mut stats).await?, first);
    assert_eq!(fixture.calls()?, 1);
    assert_eq!(stats.memo.hits, 1);
    Ok(())
}

#[tokio::test]
async fn failed_seed_repeats_without_cargo_or_per_member_diagnostics() -> eyre::Result<()> {
    let fixture = MemoFixture::new()?;
    let journal = fixture.journal().await?;
    let moves = [PlannedNodeMove {
        name: "demo".to_owned(),
        from: "1.0.0".to_owned(),
        to: "2.0.0".to_owned(),
    }];
    let changes = [&fixture.change];
    let mut stats = PinBatchStats::default();
    for _ in 0..2 {
        assert!(
            fixture
                .tool
                .seed_resolve(
                    &fixture.project,
                    SeedGroup {
                        moves: &moves,
                        changes: &changes
                    },
                    &journal,
                    None,
                    &mut stats
                )
                .await?
                .is_none()
        );
    }
    assert_eq!(fixture.calls()?, 1);
    assert_eq!(stats.memo.hits, 1);
    assert!(stats.rejection_effects.is_empty());
    // A group refusal must still obtain the individual Cargo explanation.
    let rejections = fixture.precise(&journal, &mut stats).await?;
    assert_eq!(fixture.calls()?, 2);
    assert!(
        rejections
            .values()
            .all(|detail| detail.contains("holder") && !detail.contains("joint seed"))
    );
    Ok(())
}

#[tokio::test]
async fn tentative_widen_hit_preserves_files_and_the_rejection_detail() -> eyre::Result<()> {
    let fixture = MemoFixture::new()?;
    let journal = fixture.journal().await?;
    let mut stats = MemoStats::default();
    let mut rejections = PinRejections::new();
    let mut first_rejections = PinRejections::new();
    for attempt in 0..2 {
        rejections.clear();
        if attempt == 1 {
            let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
            for relative in ["Cargo.toml", "Cargo.lock"] {
                std::fs::File::options()
                    .write(true)
                    .open(fixture.project.root.join(relative))?
                    .set_times(std::fs::FileTimes::new().set_modified(time))?;
            }
        }
        let manifest = std::fs::metadata(&fixture.project.manifest)?.modified()?;
        let lock = std::fs::metadata(fixture.project.root.join("Cargo.lock"))?.modified()?;
        let result = fixture
            .tool
            .tentative_widen_logged(
                WidenProbe {
                    project: &fixture.project,
                    candidates: &[&fixture.change],
                    protected: &[],
                    followers: &[],
                    journal: &journal,
                    observer: None,
                },
                &fixture.change,
                &mut rejections,
                &mut stats,
            )
            .await?;
        assert!(matches!(result, TentativeWiden::Rejected));
        if attempt == 0 {
            first_rejections = rejections.clone();
        } else {
            assert_eq!(rejections, first_rejections);
            // Cached widening must not even rewrite and restore identical bytes.
            assert_eq!(
                std::fs::metadata(&fixture.project.manifest)?.modified()?,
                manifest
            );
            assert_eq!(
                std::fs::metadata(fixture.project.root.join("Cargo.lock"))?.modified()?,
                lock
            );
        }
    }
    assert_eq!(fixture.calls()?, 1);
    assert_eq!(stats.hits, 1);
    assert!(rejections.values().any(|detail| detail.contains("holder")));
    Ok(())
}

#[tokio::test]
async fn every_native_input_change_forces_a_precise_retry() -> eyre::Result<()> {
    for relative in [
        "Cargo.toml",
        "other/Cargo.toml",
        "Cargo.lock",
        "follower/Cargo.toml",
        ".cargo/config",
        ".cargo/config.toml",
        "other/.cargo/config",
        "other/.cargo/config.toml",
    ] {
        let fixture = MemoFixture::new()?;
        let follower = MemberRef {
            name: "follower".to_owned(),
            path: "follower".to_owned(),
        };
        std::fs::create_dir(fixture.project.root.join("follower"))?;
        std::fs::write(
            fixture.project.root.join("follower/Cargo.toml"),
            indoc! {r#"
            [package]
            name = "follower"
            version = "0.1.0"
            [dependencies]
            demo = "=1.0.0"
        "#},
        )?;
        // Seed generated facts directly so input discovery cannot add metadata calls.
        fixture.tool.generated.lock().await.insert(
            fixture.project.root.clone(),
            Arc::new(GeneratedFacts {
                members: vec![follower],
                ..Default::default()
            }),
        );
        let path = fixture.project.root.join(relative);
        if relative.ends_with("config") || relative.ends_with("config.toml") {
            std::fs::create_dir_all(path.parent().ok_or_else(|| eyre::eyre!("config parent"))?)?;
            std::fs::write(
                &path,
                indoc! {"
                [net]
                offline = true
            "},
            )?;
        }
        let journal = fixture.journal().await?;
        let mut stats = PinBatchStats::default();
        fixture.precise(&journal, &mut stats).await?;
        fixture.precise(&journal, &mut stats).await?;
        assert_eq!(fixture.calls()?, 1, "{relative}");
        assert_eq!(stats.memo.hits, 1, "{relative}");
        let mut changed = std::fs::read(&path)?;
        changed.push(b'\n');
        std::fs::write(&path, changed)?;
        fixture.precise(&journal, &mut stats).await?;
        assert_eq!(fixture.calls()?, 2, "{relative}");
        assert_eq!(stats.memo.hits, 1, "{relative}");
    }
    Ok(())
}

#[tokio::test]
async fn infrastructure_failures_and_signals_are_never_memoized() -> eyre::Result<()> {
    for operation in [
        MemoOperation::Precise,
        MemoOperation::Seed,
        MemoOperation::Widen,
    ] {
        for marker in ["infrastructure", "signal"] {
            let fixture = MemoFixture::new()?;
            std::fs::write(fixture.project.root.join(marker), "")?;
            let journal = fixture.journal().await?;
            let mut stats = PinBatchStats::default();
            for _ in 0..2 {
                let err = fixture
                    .attempt(operation, &journal, &mut stats)
                    .await
                    .err()
                    .ok_or_else(|| eyre::eyre!("local failure must propagate"))?;
                if marker == "signal" {
                    assert!(
                        matches!(
                            err,
                            CoreError::Tool {
                                termination: cooldown_core::ToolTermination::Signal(15),
                                ..
                            }
                        ),
                        "{operation:?}: {err:?}"
                    );
                } else {
                    assert!(err.is_local_environment_failure(), "{operation:?}: {err:?}");
                }
                // Fatal widening errors unwind to the caller's journal before a fresh attempt.
                journal.restore()?;
            }
            assert_eq!(fixture.calls()?, 2, "{operation:?}: {marker}");
            assert_eq!(stats.memo.hits, 0, "{operation:?}: {marker}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn failed_attempt_with_unrestored_inputs_is_not_remembered() -> eyre::Result<()> {
    for operation in [
        MemoOperation::Precise,
        MemoOperation::Seed,
        MemoOperation::Widen,
    ] {
        let fixture = MemoFixture::new()?;
        let member = fixture.project.root.join("other/Cargo.toml");
        let original_member = std::fs::read(&member)?;
        let original_manifest = std::fs::read(&fixture.project.manifest)?;
        let original_lock = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
        std::fs::write(fixture.project.root.join("mutate"), "")?;
        let journal = fixture.journal().await?;
        let mut stats = PinBatchStats::default();
        fixture.attempt(operation, &journal, &mut stats).await?;
        // Rollback restores the files it owns, but not Cargo's unrelated member edit.
        assert_eq!(
            std::fs::read(&fixture.project.manifest)?,
            original_manifest,
            "{operation:?}"
        );
        assert_eq!(
            std::fs::read(fixture.project.root.join("Cargo.lock"))?,
            original_lock,
            "{operation:?}"
        );
        assert_ne!(std::fs::read(&member)?, original_member, "{operation:?}");
        std::fs::write(&member, original_member)?;
        std::fs::remove_file(fixture.project.root.join("mutate"))?;
        fixture.attempt(operation, &journal, &mut stats).await?;
        assert_eq!(fixture.calls()?, 2, "{operation:?}");
        assert_eq!(stats.memo.hits, 0, "{operation:?}");
    }
    Ok(())
}

#[tokio::test]
async fn registry_transport_failures_remain_uncached_for_every_operation() -> eyre::Result<()> {
    for operation in [
        MemoOperation::Precise,
        MemoOperation::Seed,
        MemoOperation::Widen,
    ] {
        let fixture = MemoFixture::new()?;
        std::fs::write(fixture.project.root.join("transport"), "")?;
        let journal = fixture.journal().await?;
        let mut stats = PinBatchStats::default();
        for _ in 0..2 {
            fixture.attempt(operation, &journal, &mut stats).await?;
        }
        assert_eq!(fixture.calls()?, 2, "{operation:?}");
        assert_eq!(stats.memo.hits, 0, "{operation:?}");
    }
    Ok(())
}

#[tokio::test]
async fn changed_topology_on_a_cached_attempt_propagates_before_cargo() -> eyre::Result<()> {
    let fixture = MemoFixture::new()?;
    let journal = fixture.journal().await?;
    let mut stats = PinBatchStats::default();
    fixture.precise(&journal, &mut stats).await?;
    let lock = fixture.project.root.join("Cargo.lock");
    let foreign = fixture.project.root.join("foreign.lock");
    std::fs::rename(&lock, &foreign)?;
    std::os::unix::fs::symlink(&foreign, &lock)?;
    let err = fixture
        .precise(&journal, &mut stats)
        .await
        .err()
        .ok_or_else(|| eyre::eyre!("topology failure must propagate"))?;
    assert!(matches!(err, CoreError::LockConflict(_)));
    assert_eq!(fixture.calls()?, 1);
    Ok(())
}

#[test]
fn interruption_on_cached_hit_runs_in_an_isolated_process() -> eyre::Result<()> {
    let output = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "tool::tests::rejection_memo::interruption_child",
            "--nocapture",
        ])
        .env("COOLDOWN_MEMO_INTERRUPTION_CHILD", "1")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    Ok(())
}

#[tokio::test]
async fn interruption_child() -> eyre::Result<()> {
    if std::env::var_os("COOLDOWN_MEMO_INTERRUPTION_CHILD").is_none() {
        return Ok(());
    }
    let fixture = MemoFixture::new()?;
    let journal = fixture.journal().await?;
    let mut stats = PinBatchStats::default();
    fixture.precise(&journal, &mut stats).await?;
    let original = std::fs::read(&fixture.project.manifest)?;
    cooldown_core::interrupt::request();
    let err = fixture
        .precise(&journal, &mut stats)
        .await
        .err()
        .ok_or_else(|| eyre::eyre!("interruption must precede a cache hit"))?;
    assert!(matches!(err, CoreError::System(_)));
    assert_eq!(fixture.calls()?, 1);
    assert_eq!(std::fs::read(&fixture.project.manifest)?, original);
    Ok(())
}

#[tokio::test]
async fn disabled_memo_repeats_every_native_attempt_and_preserves_details() -> eyre::Result<()> {
    for operation in [
        MemoOperation::Precise,
        MemoOperation::Seed,
        MemoOperation::Widen,
    ] {
        let mut fixture = MemoFixture::new()?;
        fixture.tool = fixture.tool.with_rejection_memo(false);
        let journal = fixture.journal().await?;
        let mut stats = PinBatchStats::default();
        let mut first = PinRejections::new();
        for attempt in 0..2 {
            let mut rejections = PinRejections::new();
            match operation {
                MemoOperation::Precise => {
                    rejections = fixture.precise(&journal, &mut stats).await?;
                }
                MemoOperation::Seed => {
                    fixture.attempt(operation, &journal, &mut stats).await?;
                    assert!(stats.rejection_effects.is_empty());
                }
                MemoOperation::Widen => {
                    let mut memo_stats = MemoStats::default();
                    let result = fixture
                        .tool
                        .tentative_widen_logged(
                            WidenProbe {
                                project: &fixture.project,
                                candidates: &[&fixture.change],
                                protected: &[],
                                followers: &[],
                                journal: &journal,
                                observer: None,
                            },
                            &fixture.change,
                            &mut rejections,
                            &mut memo_stats,
                        )
                        .await?;
                    assert!(matches!(result, TentativeWiden::Rejected));
                    stats.memo.include(&memo_stats);
                }
            }
            if attempt == 0 {
                first = rejections;
            } else {
                assert_eq!(rejections, first, "{operation:?}");
            }
        }
        assert_eq!(fixture.calls()?, 2, "{operation:?}");
        assert_eq!(stats.memo.hits, 0, "{operation:?}");
        if operation != MemoOperation::Seed {
            assert!(first.values().any(|detail| detail.contains("holder")));
        }
    }
    Ok(())
}

#[tokio::test]
async fn disabled_debug_fingerprint_does_not_read_project_files() -> eyre::Result<()> {
    let fixture = MemoFixture::new()?;
    let journal = fixture.journal().await?;
    std::fs::remove_file(&fixture.project.manifest)?;
    std::fs::create_dir(&fixture.project.manifest)?;
    let fingerprint =
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            widen_fingerprint(&fixture.project, &journal)
        })?;
    assert!(fingerprint.is_none());
    Ok(())
}

#[tokio::test]
async fn unreadable_optional_inputs_bypass_memo_without_aborting_native_attempts()
-> eyre::Result<()> {
    let fixture = MemoFixture::new()?;
    let directory = fixture.project.root.join("unreadable");
    std::fs::create_dir(&directory)?;
    std::fs::write(directory.join("Cargo.toml"), "[package]")?;
    let journal = fixture.journal().await?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o000))?;
    // Root can still read mode-000 directories, so this case requires ordinary user permissions.
    if std::fs::read_dir(&directory).is_ok() {
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        return Ok(());
    }
    let mut stats = PinBatchStats::default();
    let first = fixture.precise(&journal, &mut stats).await;
    let second = fixture.precise(&journal, &mut stats).await;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    assert_eq!(first?, second?);
    assert_eq!(fixture.calls()?, 2);
    assert_eq!(stats.memo.hits, 0);
    Ok(())
}

#[test]
fn memo_disqualification_is_monotone_across_composite_rejections() {
    let mut stats = PinBatchStats::default();
    stats.record_rejection(false);
    stats.record_rejection(true);
    assert!(stats.resolver_rejected);
    assert!(stats.memo_disqualified);
    assert!(!stats.memoizable_rejection());
}

#[tokio::test]
async fn unreadable_follower_discovery_falls_back_to_a_locked_native_graph() -> eyre::Result<()> {
    let mut fixture = MemoFixture::new()?;
    fixture.project.generated_members =
        cooldown_core::GeneratedMembers::declared(vec!["other".to_owned()]);
    let metadata = serde_json::json!({
        "packages": [{"id": "other", "name": "other", "version": "0.1.0",
            "manifest_path": fixture.project.root.join("other/Cargo.toml"), "dependencies": []}],
        "workspace_members": ["other"], "workspace_root": fixture.project.root,
        "resolve": {"nodes": [{"id": "other", "deps": []}]}
    });
    std::fs::write(
        fixture.project.root.join("metadata.json"),
        serde_json::to_vec(&metadata)?,
    )?;
    std::fs::write(
        fixture.project.root.join("fake-cargo"),
        indoc! {r#"
        #!/bin/sh
        set -eu
        printf '%s\n' "$*" >> invocations
        cat metadata.json
    "#},
    )?;
    let directory = fixture.project.root.join("unreadable");
    std::fs::create_dir(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o000))?;
    if std::fs::read_dir(&directory).is_ok() {
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        return Ok(());
    }
    let first = fixture.tool.pin_followers(&fixture.project).await;
    let second = fixture.tool.pin_followers(&fixture.project).await;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    let expected = vec![MemberRef {
        name: "other".to_owned(),
        path: "other".to_owned(),
    }];
    assert_eq!(first?, expected);
    assert_eq!(second?, expected);
    assert_eq!(fixture.calls()?, 1);
    let invocation = std::fs::read_to_string(fixture.project.root.join("invocations"))?;
    assert!(invocation.starts_with("metadata "));
    assert!(
        invocation
            .split_whitespace()
            .any(|argument| argument == "--locked")
    );
    Ok(())
}
