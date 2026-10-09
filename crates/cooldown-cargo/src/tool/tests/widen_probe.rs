//! Joint widening commits only resolver-verified candidate subsets.

use super::*;
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt as _;

struct WidenFixture {
    directory: tempfile::TempDir,
    project: Project,
    tool: CargoTool,
    changes: Vec<Change>,
}

impl WidenFixture {
    fn new(count: usize, behavior: &str) -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let changes: Vec<_> = (0..count)
            .map(|index| Change {
                direct: false,
                ..change(&format!("pkg{index:03}"), "1.0.0", "2.0.0", false)
            })
            .collect();
        let mut manifest = String::from(indoc! {r#"
            [package]
            name = "app"
            version = "0.1.0"
            [dependencies]
        "#});
        for planned in &changes {
            writeln!(&mut manifest, "{} = \"1\"", planned.package.name)?;
        }
        std::fs::write(root.join("Cargo.toml"), manifest)?;
        std::fs::write(root.join("Cargo.lock"), Self::lock(&changes, 0))?;
        std::fs::write(root.join("landed.lock"), Self::lock(&changes, count))?;
        std::fs::write(
            root.join("partial.lock"),
            Self::lock(&changes, count.saturating_sub(1)),
        )?;
        std::fs::write(
            root.join("metadata.json"),
            r#"{"packages": [], "workspace_members": [], "workspace_root": "", "resolve": null}"#,
        )?;
        let script = root.join("fake-cargo");
        std::fs::write(
            &script,
            formatdoc! {r#"
            #!/bin/sh
            set -eu
            printf '%s\n' "$*" >> invocations
            count=$(wc -l < invocations)
            {behavior}
            cat metadata.json
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
            changes,
        })
    }

    fn held_manifest(name: &str) -> String {
        formatdoc! {r#"
            [package]
            name = "holder"
            version = "0.1.0"
            [dependencies]
            {name} = "1"
        "#}
    }

    fn assert_held_manifest(&self, planned: &Change) -> eyre::Result<()> {
        assert_eq!(
            std::fs::read_to_string(
                self.project
                    .root
                    .join(&planned.package.name)
                    .join("Cargo.toml")
            )?,
            Self::held_manifest(&planned.package.name)
        );
        Ok(())
    }

    fn add_held_members(&mut self, first: usize) -> eyre::Result<()> {
        for planned in self.changes.iter_mut().skip(first) {
            let path = planned.package.name.clone();
            std::fs::create_dir(self.project.root.join(&path))?;
            std::fs::write(
                self.project.root.join(&path).join("Cargo.toml"),
                Self::held_manifest(&planned.package.name),
            )?;
            planned.members = vec![MemberRef {
                name: "holder".to_owned(),
                path,
            }];
        }
        Ok(())
    }

    fn inherit_members(&mut self) -> eyre::Result<()> {
        for planned in &mut self.changes {
            let name = &planned.package.name;
            std::fs::create_dir(self.project.root.join(name))?;
            std::fs::write(
                self.project.root.join(name).join("Cargo.toml"),
                formatdoc! {r#"
                    [package]
                    name = "{name}-owner"
                    version = "0.1.0"
                    [dependencies]
                    {name}.workspace = true
                "#},
            )?;
            planned.members = vec![MemberRef {
                name: format!("{name}-owner"),
                path: name.clone(),
            }];
        }
        Ok(())
    }

    fn checkpoint(&self, followers: &[MemberRef]) -> Result<ProjectMutationJournal> {
        let candidates: Vec<_> = self.changes.iter().collect();
        widen_checkpoint(&self.project, &candidates, followers)
    }

    fn lock(changes: &[Change], landed: usize) -> String {
        let mut lock = String::from("version = 4\n\n");
        for (index, planned) in changes.iter().enumerate() {
            let name = &planned.package.name;
            let version = if index < landed { "2.0.0" } else { "1.0.0" };
            lock.push_str(&formatdoc! {r#"
                [[package]]
                name = "{name}"
                version = "{version}"
                source = "registry+https://github.com/rust-lang/crates.io-index"

            "#});
        }
        lock
    }

    async fn journal(&self) -> Result<ProjectMutationJournal> {
        self.tool
            .mutation_journal(
                &self.project,
                &Plan {
                    changes: self.changes.clone(),
                    ..Plan::default()
                },
            )
            .await
    }

    async fn probe(
        &self,
        journal: &ProjectMutationJournal,
        stats: &mut WholeGraphStats,
    ) -> Result<Option<WidenEvidence>> {
        let candidates: Vec<_> = self.changes.iter().collect();
        self.tool
            .joint_widen_probe(
                WidenProbe {
                    project: &self.project,
                    candidates: &candidates,
                    protected: &[],
                    followers: &[],
                    journal,
                    observer: None,
                },
                stats,
            )
            .await
    }

    fn report(&self, rejections: &PinRejections) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        classify_planned_changes(
            &Plan {
                changes: self.changes.clone(),
                ..Plan::default()
            },
            &read_lock(&self.project)?.crates_io_locked_versions(),
            None,
            rejections,
            &mut report,
        );
        Ok(report)
    }

    fn invocations(&self) -> eyre::Result<Vec<String>> {
        let path = self.directory.path().join("invocations");
        if !path.exists() {
            return Ok(Vec::new());
        }
        Ok(std::fs::read_to_string(path)?
            .lines()
            .map(str::to_owned)
            .collect())
    }
}

#[tokio::test]
async fn mutually_blocked_widens_land_in_one_resolve() -> eyre::Result<()> {
    let fixture = WidenFixture::new(
        2,
        indoc! {r#"
        grep -q 'pkg000 = "2' Cargo.toml
        grep -q 'pkg001 = "2' Cargo.toml
        cp landed.lock Cargo.lock
    "#},
    )?;
    let journal = fixture.journal().await?;
    let mut stats = WholeGraphStats::default();
    assert!(fixture.probe(&journal, &mut stats).await?.is_some());
    assert_eq!(fixture.invocations()?.len(), 1);
    assert_eq!(stats.joint_probes, 1);
    assert_eq!(stats.joint_full_successes, 1);
    for planned in &fixture.changes {
        assert!(read_lock(&fixture.project)?.has_crates_io_package(&planned.package.name, "2.0.0"));
    }
    Ok(())
}

#[tokio::test]
async fn partial_replay_keeps_only_the_independently_reached_widen() -> eyre::Result<()> {
    let fixture = WidenFixture::new(2, "cp partial.lock Cargo.lock")?;
    let journal = fixture.journal().await?;
    let mut stats = WholeGraphStats::default();
    assert!(fixture.probe(&journal, &mut stats).await?.is_some());
    assert_eq!(fixture.invocations()?.len(), 2);
    assert_eq!(stats.partial_replays_kept, 1);
    let manifest = std::fs::read_to_string(&fixture.project.manifest)?;
    assert!(manifest.contains("pkg000 = \"2"), "{manifest}");
    assert!(manifest.contains("pkg001 = \"1\""), "{manifest}");
    Ok(())
}

#[tokio::test]
async fn partial_replay_that_needs_the_discarded_sibling_restores_exact_bytes() -> eyre::Result<()>
{
    let fixture = WidenFixture::new(
        2,
        indoc! {r#"
        if [ "$count" = 1 ]; then
          cp partial.lock Cargo.lock
        else
          grep -q 'pkg001 = "1"' Cargo.toml
          printf '\n# metadata touched the lock\n' >> Cargo.lock
          echo 'error: pkg000 requires the widened pkg001' >&2
          exit 101
        fi
    "#},
    )?;
    let original_manifest = std::fs::read(&fixture.project.manifest)?;
    let original_lock = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
    let journal = fixture.journal().await?;
    let mut stats = WholeGraphStats::default();
    assert!(fixture.probe(&journal, &mut stats).await?.is_none());
    assert_eq!(fixture.invocations()?.len(), 2);
    assert_eq!(stats.partial_replays_discarded, 1);
    assert_eq!(std::fs::read(&fixture.project.manifest)?, original_manifest);
    assert_eq!(
        std::fs::read(fixture.project.root.join("Cargo.lock"))?,
        original_lock
    );
    Ok(())
}

#[tokio::test]
async fn fifty_candidate_partial_probe_uses_two_resolves() -> eyre::Result<()> {
    let fixture = WidenFixture::new(50, "cp partial.lock Cargo.lock")?;
    std::fs::write(
        fixture.project.root.join("partial.lock"),
        WidenFixture::lock(&fixture.changes, 47),
    )?;
    let journal = fixture.journal().await?;
    let mut stats = WholeGraphStats::default();
    assert!(fixture.probe(&journal, &mut stats).await?.is_some());
    assert_eq!(fixture.invocations()?.len(), 2);
    let lock = read_lock(&fixture.project)?;
    for (index, planned) in fixture.changes.iter().enumerate() {
        assert!(lock.has_crates_io_package(
            &planned.package.name,
            if index < 47 { "2.0.0" } else { "1.0.0" }
        ));
    }
    Ok(())
}

#[tokio::test]
async fn interrupted_joint_resolve_propagates_without_replay() -> eyre::Result<()> {
    let fixture = WidenFixture::new(2, "kill -TERM \"$$\"")?;
    let original_manifest = std::fs::read(&fixture.project.manifest)?;
    let original_lock = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
    let journal = fixture.journal().await?;
    let mut stats = WholeGraphStats::default();
    let err = fixture
        .probe(&journal, &mut stats)
        .await
        .err()
        .ok_or_else(|| eyre::eyre!("signal must terminate the probe"))?;
    assert!(matches!(
        err,
        CoreError::Tool {
            termination: cooldown_core::ToolTermination::Signal(15),
            ..
        }
    ));
    assert_eq!(fixture.invocations()?.len(), 1);
    assert_eq!(stats.joint_full_successes, 0);
    assert_eq!(stats.partial_replays_kept, 0);
    journal.restore()?;
    assert_eq!(std::fs::read(&fixture.project.manifest)?, original_manifest);
    assert_eq!(
        std::fs::read(fixture.project.root.join("Cargo.lock"))?,
        original_lock
    );
    Ok(())
}

#[tokio::test]
async fn local_filesystem_failure_propagates_without_replay() -> eyre::Result<()> {
    let fixture = WidenFixture::new(
        2,
        indoc! {"
        echo 'error: Permission denied reading the registry cache' >&2
        exit 101
    "},
    )?;
    let original_manifest = std::fs::read(&fixture.project.manifest)?;
    let original_lock = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
    let journal = fixture.journal().await?;
    let mut stats = WholeGraphStats::default();
    let err = fixture
        .probe(&journal, &mut stats)
        .await
        .err()
        .ok_or_else(|| eyre::eyre!("local failure must terminate the probe"))?;
    assert!(err.is_local_environment_failure(), "{err:?}");
    assert_eq!(fixture.invocations()?.len(), 1);
    assert_eq!(stats.joint_full_successes, 0);
    journal.restore()?;
    assert_eq!(std::fs::read(&fixture.project.manifest)?, original_manifest);
    assert_eq!(
        std::fs::read(fixture.project.root.join("Cargo.lock"))?,
        original_lock
    );
    Ok(())
}

#[tokio::test]
async fn lock_topology_change_terminates_without_acceptance() -> eyre::Result<()> {
    let fixture = WidenFixture::new(
        2,
        indoc! {"
        mv Cargo.lock foreign.lock
        ln -s foreign.lock Cargo.lock
        echo 'error: joint resolve failed' >&2
        exit 101
    "},
    )?;
    let journal = fixture.journal().await?;
    let mut stats = WholeGraphStats::default();
    let err = fixture
        .probe(&journal, &mut stats)
        .await
        .err()
        .ok_or_else(|| eyre::eyre!("topology change must terminate the probe"))?;
    assert!(matches!(err, CoreError::LockConflict(_)), "{err:?}");
    assert_eq!(fixture.invocations()?.len(), 1);
    assert_eq!(stats.joint_full_successes, 0);
    assert!(
        std::fs::symlink_metadata(fixture.project.root.join("Cargo.lock"))?
            .file_type()
            .is_symlink()
    );
    Ok(())
}

#[tokio::test]
async fn a_no_op_widen_companion_still_participates_in_the_joint_seed() -> eyre::Result<()> {
    let fixture = WidenFixture::new(2, "cp landed.lock Cargo.lock")?;
    let original = std::fs::read_to_string(&fixture.project.manifest)?;
    std::fs::write(
        &fixture.project.manifest,
        original.replace("pkg001 = \"1\"", "pkg001 = \"2.0.0\""),
    )?;
    let journal = fixture.journal().await?;
    let mut stats = WholeGraphStats::default();
    assert!(fixture.probe(&journal, &mut stats).await?.is_some());
    assert_eq!(fixture.invocations()?.len(), 1);
    assert!(read_lock(&fixture.project)?.has_crates_io_package("pkg001", "2.0.0"));
    Ok(())
}

#[tokio::test]
async fn protected_member_reach_cannot_be_replaced_by_a_disconnected_target_copy()
-> eyre::Result<()> {
    let fixture = WidenFixture::new(2, "cp landed.lock Cargo.lock")?;
    std::fs::write(
        fixture.project.root.join("Cargo.lock"),
        WidenFixture::lock(&fixture.changes, 1),
    )?;
    let protected = Change {
        direct: true,
        members: vec![MemberRef {
            name: "app".to_owned(),
            path: ".".to_owned(),
        }],
        ..fixture.changes.first().expect("protected change").clone()
    };
    // The target remains present globally, while the authored member resolves the old copy.
    let metadata = serde_json::json!({
        "packages": [
            {"id": "app", "name": "app", "version": "0.1.0", "manifest_path": fixture.project.manifest, "dependencies": [{"name": "pkg000", "req": "^1"}]},
            {"id": "old", "name": "pkg000", "version": "1.0.0", "source": crate::lockfile::CRATES_IO_SOURCE},
            {"id": "target", "name": "pkg000", "version": "2.0.0", "source": crate::lockfile::CRATES_IO_SOURCE}
        ],
        "workspace_members": ["app"], "workspace_root": fixture.project.root,
        "resolve": {"nodes": [{"id": "app", "deps": [{"name": "pkg000", "pkg": "old"}]}, {"id": "old", "deps": []}, {"id": "target", "deps": []}]}
    });
    std::fs::write(
        fixture.project.root.join("metadata.json"),
        serde_json::to_vec(&metadata)?,
    )?;
    let original_manifest = std::fs::read(&fixture.project.manifest)?;
    let original_lock = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
    let journal = fixture.journal().await?;
    let candidates = [fixture.changes.last().expect("candidate change")];
    let protected = [&protected];
    let mut stats = WholeGraphStats::default();
    assert!(
        fixture
            .tool
            .joint_widen_probe(
                WidenProbe {
                    project: &fixture.project,
                    candidates: &candidates,
                    protected: &protected,
                    followers: &[],
                    journal: &journal,
                    observer: None
                },
                &mut stats
            )
            .await?
            .is_none()
    );
    assert_eq!(std::fs::read(&fixture.project.manifest)?, original_manifest);
    assert_eq!(
        std::fs::read(fixture.project.root.join("Cargo.lock"))?,
        original_lock
    );
    assert_eq!(stats.joint_full_successes, 0);
    Ok(())
}

#[tokio::test]
async fn inherited_workspace_constraints_widen_at_the_shared_owner() -> eyre::Result<()> {
    let mut fixture = WidenFixture::new(2, "cp landed.lock Cargo.lock")?;
    std::fs::write(
        &fixture.project.manifest,
        indoc! {r#"
        [workspace]
        members = ["app"]
        [workspace.dependencies]
        pkg000 = "1"
        pkg001 = "1"
    "#},
    )?;
    std::fs::create_dir(fixture.project.root.join("app"))?;
    let member_manifest = fixture.project.root.join("app/Cargo.toml");
    let member_original = indoc! {r#"
        [package]
        name = "app"
        version = "0.1.0"
        [dependencies]
        pkg000.workspace = true
        pkg001.workspace = true
    "#};
    std::fs::write(&member_manifest, member_original)?;
    for planned in &mut fixture.changes {
        planned.members = vec![MemberRef {
            name: "app".to_owned(),
            path: "app".to_owned(),
        }];
    }
    let journal = fixture.journal().await?;
    assert!(
        fixture
            .probe(&journal, &mut WholeGraphStats::default())
            .await?
            .is_some()
    );
    let root = std::fs::read_to_string(&fixture.project.manifest)?;
    assert!(root.contains("pkg000 = \"2"), "{root}");
    assert!(root.contains("pkg001 = \"2"), "{root}");
    assert_eq!(std::fs::read_to_string(member_manifest)?, member_original);
    Ok(())
}

#[tokio::test]
async fn rejected_metadata_restores_removed_and_renamed_follower_entries_and_lock_bytes()
-> eyre::Result<()> {
    let mut fixture = WidenFixture::new(
        2,
        indoc! {"
        ! grep -q 'pkg000-old =' hack/Cargo.toml
        ! grep -q 'pkg001-old =' hack/Cargo.toml
        grep -q 'pkg001-new =' hack/Cargo.toml
        printf '\n# metadata touched the lock\n' >> Cargo.lock
        echo 'error: joint resolve failed' >&2
        exit 101
    "},
    )?;
    std::fs::write(
        &fixture.project.manifest,
        indoc! {r#"
            [workspace]
            members = ["pkg000", "pkg001", "hack"]
            [workspace.dependencies]
            pkg000 = "1" # Preserve the shared owner's spelling.
            pkg001 = { version = "1", features = ["kept"] }
        "#},
    )?;
    fixture.inherit_members()?;
    std::fs::create_dir(fixture.project.root.join("hack"))?;
    let follower_manifest = fixture.project.root.join("hack/Cargo.toml");
    let follower_original = indoc! {r#"
        [package]
        name = "hack"
        version = "0.1.0"
        ### BEGIN HAKARI SECTION
        [dependencies]
        pkg000-old = { package = "pkg000", version = "=1.0.0" }
        pkg000-new = { package = "pkg000", version = "=2.0.0" }
        pkg001-old = { package = "pkg001", version = "=1.0.0" }
        [build-dependencies]
        pkg001-new = { package = "pkg001", version = "=2.0.0" }
        ### END HAKARI SECTION
    "#};
    std::fs::write(&follower_manifest, follower_original)?;
    let original_manifest = std::fs::read(&fixture.project.manifest)?;
    let original_lock = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
    let candidates: Vec<_> = fixture.changes.iter().collect();
    let followers = [MemberRef {
        name: "hack".to_owned(),
        path: "hack".to_owned(),
    }];
    let journal = fixture.checkpoint(&followers)?;
    let mut stats = WholeGraphStats::default();
    assert!(
        fixture
            .tool
            .joint_widen_probe(
                WidenProbe {
                    project: &fixture.project,
                    candidates: &candidates,
                    protected: &[],
                    followers: &followers,
                    journal: &journal,
                    observer: None
                },
                &mut stats
            )
            .await?
            .is_none()
    );
    assert_eq!(std::fs::read(&fixture.project.manifest)?, original_manifest);
    assert_eq!(
        std::fs::read(fixture.project.root.join("Cargo.lock"))?,
        original_lock
    );
    assert_eq!(
        std::fs::read_to_string(follower_manifest)?,
        follower_original
    );
    for file in journal.files() {
        assert_eq!(
            std::fs::read(fixture.project.root.join(file.path()))?.as_slice(),
            file.contents().expect("captured file")
        );
    }
    assert!(
        fixture
            .tool
            .refused_alone
            .lock()
            .expect("refusal memory lock")
            .is_empty()
    );
    assert_eq!(fixture.invocations()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn whole_graph_keeps_forty_seven_landings_and_three_original_manifests_with_details()
-> eyre::Result<()> {
    let mut fixture = WidenFixture::new(
        50,
        indoc! {r#"
        if [ "$1" = update ]; then
          shift
          name=''
          while [ "$#" -gt 0 ]; do
            if [ "$1" = -p ]; then shift; name="${1##*#}"; name="${name%@*}"; fi
            shift
          done
          printf 'error: failed to select a version for the requirement `%s = "1"`\n' "$name" >&2
          echo 'required by package `holder v1.0.0`' >&2
          exit 101
        fi
        if grep -q 'pkg000 = "2' Cargo.toml; then
          if [ ! -f widened-at ]; then cp invocations widened-at; fi
          cp partial.lock Cargo.lock
        else
          cp original.lock Cargo.lock
        fi
    "#},
    )?;
    std::fs::write(
        fixture.project.root.join("original.lock"),
        WidenFixture::lock(&fixture.changes, 0),
    )?;
    std::fs::write(
        fixture.project.root.join("partial.lock"),
        WidenFixture::lock(&fixture.changes, 47),
    )?;
    fixture.add_held_members(47)?;
    let journal = fixture.journal().await?;
    let plan = Plan {
        changes: fixture.changes.clone(),
        rewrite: RewriteMode::Auto,
        ..Plan::default()
    };
    let rejections = fixture
        .tool
        .whole_graph_resolve(&fixture.project, &plan, &journal, None)
        .await?;
    let lock = read_lock(&fixture.project)?;
    let report = fixture.report(&rejections)?;
    assert_eq!(report.applied.len(), 47);
    assert_eq!(report.skipped.len(), 3);
    assert!(report.skipped.iter().all(|skipped| {
        skipped.reason == SkipReason::ResolverConflict
            && skipped
                .detail
                .as_ref()
                .is_some_and(|detail| detail.contains("holder"))
    }));
    for (index, planned) in fixture.changes.iter().enumerate() {
        assert!(lock.has_crates_io_package(
            &planned.package.name,
            if index < 47 { "2.0.0" } else { "1.0.0" }
        ));
        if index >= 47 {
            fixture.assert_held_manifest(planned)?;
            let detail = rejections
                .get(&rejection_key(planned))
                .expect("hopeless candidate detail");
            assert!(detail.contains(&planned.package.name), "{detail:?}");
        }
    }
    let initial = std::fs::read_to_string(fixture.project.root.join("widened-at"))?
        .lines()
        .count()
        .saturating_sub(1);
    let invocations = fixture.invocations()?;
    let widening_metadata = invocations
        .iter()
        .skip(initial)
        .filter(|arguments| arguments.starts_with("metadata "))
        .count();
    // Two resolves land the 47; three individual retries and two probes finish held candidates.
    // The next widen round reuses those three exact-input rejections.
    assert_eq!(widening_metadata, 4);
    assert_eq!(invocations.len() - initial, 7);
    let pins: Vec<_> = invocations
        .iter()
        .skip(initial)
        .filter(|arguments| arguments.starts_with("update "))
        .collect();
    assert_eq!(pins.len(), 3);
    for name in ["pkg047@", "pkg048@", "pkg049@"] {
        assert_eq!(
            pins.iter()
                .filter(|arguments| arguments.contains(name))
                .count(),
            1
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_dependent_partial_replay_returns_to_the_per_change_loop() -> eyre::Result<()> {
    let fixture = WidenFixture::new(
        2,
        indoc! {r#"
        if [ "$1" = update ]; then
          echo 'error: failed to select a version for the requirement `pkg000 = "1"`' >&2
          echo 'required by package `holder v1.0.0`' >&2
          exit 101
        fi
        if grep -q 'pkg000 = "2' Cargo.toml && grep -q 'pkg001 = "2' Cargo.toml; then
          cp invocations widened-at
          cp partial.lock Cargo.lock
        elif grep -q 'pkg000 = "2' Cargo.toml; then
          printf '\n# failed replay changed the lock\n' >> Cargo.lock
          echo 'error: pkg000 requires the widened pkg001' >&2
          exit 101
        else
          cp original.lock Cargo.lock
        fi
    "#},
    )?;
    let original_manifest = std::fs::read(&fixture.project.manifest)?;
    let original_lock = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
    std::fs::write(fixture.project.root.join("original.lock"), &original_lock)?;
    let journal = fixture.journal().await?;
    let plan = Plan {
        changes: fixture.changes.clone(),
        rewrite: RewriteMode::Auto,
        ..Plan::default()
    };
    let rejections = fixture
        .tool
        .whole_graph_resolve(&fixture.project, &plan, &journal, None)
        .await?;
    assert_eq!(rejections.len(), 2);
    assert_eq!(std::fs::read(&fixture.project.manifest)?, original_manifest);
    assert_eq!(
        std::fs::read(fixture.project.root.join("Cargo.lock"))?,
        original_lock
    );
    let initial = std::fs::read_to_string(fixture.project.root.join("widened-at"))?
        .lines()
        .count();
    assert!(
        fixture
            .invocations()?
            .iter()
            .skip(initial)
            .filter(|arguments| arguments.starts_with("update "))
            .count()
            >= 2
    );
    Ok(())
}

#[tokio::test]
async fn a_seeded_third_version_is_discarded_and_restored() -> eyre::Result<()> {
    let fixture = WidenFixture::new(2, "cp third.lock Cargo.lock")?;
    std::fs::write(
        fixture.project.root.join("third.lock"),
        WidenFixture::lock(&fixture.changes, 2).replace("2.0.0", "3.0.0"),
    )?;
    let original_manifest = std::fs::read(&fixture.project.manifest)?;
    let original_lock = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
    let journal = fixture.journal().await?;
    let mut stats = WholeGraphStats::default();
    assert!(fixture.probe(&journal, &mut stats).await?.is_none());
    assert_eq!(fixture.invocations()?.len(), 1);
    assert_eq!(std::fs::read(&fixture.project.manifest)?, original_manifest);
    assert_eq!(
        std::fs::read(fixture.project.root.join("Cargo.lock"))?,
        original_lock
    );
    assert_eq!(stats.joint_full_successes, 0);
    Ok(())
}

#[tokio::test]
async fn rollback_removes_a_manifest_that_was_absent_at_the_checkpoint() -> eyre::Result<()> {
    let mut fixture = WidenFixture::new(
        1,
        indoc! {r#"
        printf '[package]\nname = "created"\n' > missing/Cargo.toml
        echo 'error: joint resolve failed' >&2
        exit 101
    "#},
    )?;
    std::fs::create_dir(fixture.project.root.join("missing"))?;
    fixture.changes.first_mut().expect("candidate").members = vec![MemberRef {
        name: "created".to_owned(),
        path: "missing".to_owned(),
    }];
    let original_manifest = std::fs::read(&fixture.project.manifest)?;
    let journal = fixture.journal().await?;
    let mut stats = WholeGraphStats::default();
    assert!(fixture.probe(&journal, &mut stats).await?.is_none());
    assert!(!fixture.project.root.join("missing/Cargo.toml").exists());
    assert_eq!(std::fs::read(&fixture.project.manifest)?, original_manifest);
    assert_eq!(fixture.invocations()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn checked_write_failure_after_an_author_edit_is_restored_by_the_outer_journal()
-> eyre::Result<()> {
    let fixture = WidenFixture::new(1, "cp landed.lock Cargo.lock")?;
    std::fs::create_dir(fixture.project.root.join("hack"))?;
    let follower_path = fixture.project.root.join("hack/Cargo.toml");
    let follower_original = indoc! {r#"
        [package]
        name = "hack"
        [dependencies]
        pkg000 = "=1.0.0"
    "#};
    std::fs::write(&follower_path, follower_original)?;
    let original_manifest = std::fs::read(&fixture.project.manifest)?;
    let journal = fixture.journal().await?;
    let followers = [MemberRef {
        name: "hack".to_owned(),
        path: "hack".to_owned(),
    }];
    let checks = std::cell::Cell::new(0);
    let err = widen_and_follow_checked(
        &fixture.project.root,
        fixture.changes.first().expect("candidate"),
        &followers,
        || {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                Err(CoreError::from(std::io::Error::other(
                    "injected filesystem failure",
                )))
            } else {
                Ok(())
            }
        },
    )
    .err()
    .ok_or_else(|| eyre::eyre!("second checked write must fail"))?;
    assert!(matches!(err, CoreError::Filesystem(_)), "{err:?}");
    assert_eq!(checks.get(), 2);
    assert_ne!(std::fs::read(&fixture.project.manifest)?, original_manifest);
    assert_eq!(std::fs::read_to_string(&follower_path)?, follower_original);
    assert!(fixture.invocations()?.is_empty());
    journal.restore()?;
    assert_eq!(std::fs::read(&fixture.project.manifest)?, original_manifest);
    Ok(())
}

#[tokio::test]
async fn duplicate_lock_moves_keep_each_members_independent_reach_obligation() -> eyre::Result<()> {
    let mut fixture = WidenFixture::new(1, "cp landed.lock Cargo.lock")?;
    let original = fixture.changes.first().expect("candidate").clone();
    fixture.changes.clear();
    let mut packages = Vec::new();
    let mut nodes = Vec::new();
    let mut workspace_members = Vec::new();
    for member in ["app-a", "app-b"] {
        std::fs::create_dir(fixture.project.root.join(member))?;
        let manifest = fixture.project.root.join(member).join("Cargo.toml");
        std::fs::write(
            &manifest,
            formatdoc! {r#"
            [package]
            name = "{member}"
            version = "0.1.0"
            [dependencies]
            pkg000 = "1"
        "#},
        )?;
        fixture.changes.push(Change {
            direct: true,
            members: vec![MemberRef {
                name: member.to_owned(),
                path: member.to_owned(),
            }],
            ..original.clone()
        });
        packages.push(serde_json::json!({"id": member, "name": member, "version": "0.1.0", "manifest_path": manifest, "dependencies": [{"name": "pkg000", "req": "^1"}]}));
        nodes.push(serde_json::json!({"id": member, "deps": [{"name": "pkg000", "pkg": if member == "app-a" { "target" } else { "old" }}]}));
        workspace_members.push(member);
    }
    packages.push(serde_json::json!({"id": "old", "name": "pkg000", "version": "1.0.0", "source": crate::lockfile::CRATES_IO_SOURCE}));
    packages.push(serde_json::json!({"id": "target", "name": "pkg000", "version": "2.0.0", "source": crate::lockfile::CRATES_IO_SOURCE}));
    nodes.push(serde_json::json!({"id": "old", "deps": []}));
    nodes.push(serde_json::json!({"id": "target", "deps": []}));
    std::fs::write(
        fixture.project.root.join("metadata.json"),
        serde_json::to_vec(
            &serde_json::json!({"packages": packages, "workspace_members": workspace_members, "workspace_root": fixture.project.root, "resolve": {"nodes": nodes}}),
        )?,
    )?;
    let member_original = std::fs::read(fixture.project.root.join("app-b/Cargo.toml"))?;
    let journal = fixture.journal().await?;
    let mut stats = WholeGraphStats::default();
    assert!(fixture.probe(&journal, &mut stats).await?.is_some());
    assert_eq!(stats.joint_full_successes, 0);
    assert_eq!(stats.partial_replays_kept, 1);
    assert_eq!(fixture.invocations()?.len(), 2);
    assert_eq!(
        std::fs::read(fixture.project.root.join("app-b/Cargo.toml"))?,
        member_original
    );
    assert!(
        std::fs::read_to_string(fixture.project.root.join("app-a/Cargo.toml"))?
            .contains("pkg000 = \"2")
    );
    Ok(())
}

#[tokio::test]
async fn a_joint_filesystem_failure_after_one_widen_stops_before_resolving() -> eyre::Result<()> {
    let mut fixture = WidenFixture::new(2, "cp landed.lock Cargo.lock")?;
    fixture.add_held_members(0)?;
    let first = fixture.project.root.join("pkg000/Cargo.toml");
    let second = fixture.project.root.join("pkg001/Cargo.toml");
    let original = std::fs::read(&first)?;
    let journal = fixture.journal().await?;
    // The first write succeeds; the second candidate cannot open its manifest for writing.
    std::fs::set_permissions(&second, std::fs::Permissions::from_mode(0o444))?;
    let mut stats = WholeGraphStats::default();
    let err = fixture
        .probe(&journal, &mut stats)
        .await
        .err()
        .ok_or_else(|| eyre::eyre!("read-only candidate manifest must terminate the probe"))?;
    assert!(matches!(err, CoreError::Filesystem(_)), "{err:?}");
    assert_ne!(std::fs::read(&first)?, original);
    assert!(fixture.invocations()?.is_empty());
    assert_eq!(stats.joint_full_successes, 0);
    assert_eq!(stats.partial_replays_kept, 0);
    // Fatal errors unwind to the outer journal, without a rejected probe replay.
    journal.restore()?;
    for file in journal.files() {
        assert_eq!(
            std::fs::read(fixture.project.root.join(file.path()))?.as_slice(),
            file.contents().expect("captured file")
        );
    }
    Ok(())
}

fn lock_with_selected_targets(changes: &[Change], targets: &[&str]) -> String {
    let mut lock = String::from("version = 4\n\n");
    for planned in changes {
        let name = &planned.package.name;
        let version = if targets.contains(&name.as_str()) {
            "2.0.0"
        } else {
            "1.0.0"
        };
        lock.push_str(&formatdoc! {r#"
            [[package]]
            name = "{name}"
            version = "{version}"
            source = "registry+https://github.com/rust-lang/crates.io-index"

        "#});
    }
    lock
}

#[tokio::test]
async fn singleton_after_partial_replay_cannot_displace_retained_or_earlier_targets()
-> eyre::Result<()> {
    let mut fixture = WidenFixture::new(
        3,
        indoc! {r#"
        if [ "$1" = update ]; then
          if grep -q 'pkg001 = "2' Cargo.toml && grep -q 'pkg000 = "2' Cargo.toml; then
            cp displaced.lock Cargo.lock
            touch singleton-displaced
            exit 0
          fi
          echo 'error: failed to select a version for the requirement `pkg001 = "1"`' >&2
          echo 'required by package `holder v1.0.0`' >&2
          exit 101
        fi
        if grep -q 'pkg000 = "2' Cargo.toml; then
          cp retained.lock Cargo.lock
        else
          echo 'error: original requirements reject the seed' >&2
          exit 101
        fi
    "#},
    )?;
    fixture.tool = fixture.tool.with_rejection_memo(false);
    // pkg002 was reached before widening; pkg000 lands through the partial replay.
    std::fs::write(
        fixture.project.root.join("Cargo.lock"),
        lock_with_selected_targets(&fixture.changes, &["pkg002"]),
    )?;
    std::fs::write(
        fixture.project.root.join("retained.lock"),
        lock_with_selected_targets(&fixture.changes, &["pkg000", "pkg002"]),
    )?;
    std::fs::write(
        fixture.project.root.join("displaced.lock"),
        lock_with_selected_targets(&fixture.changes, &["pkg001"]),
    )?;
    let journal = fixture.journal().await?;
    let plan = Plan {
        changes: fixture.changes.clone(),
        rewrite: RewriteMode::Auto,
        ..Default::default()
    };
    let rejections = fixture
        .tool
        .whole_graph_resolve(&fixture.project, &plan, &journal, None)
        .await?;
    assert!(fixture.project.root.join("singleton-displaced").exists());
    let lock = read_lock(&fixture.project)?;
    assert!(lock.has_crates_io_package("pkg000", "2.0.0"));
    assert!(lock.has_crates_io_package("pkg002", "2.0.0"));
    assert!(lock.has_crates_io_package("pkg001", "1.0.0"));
    let manifest = std::fs::read_to_string(&fixture.project.manifest)?;
    assert!(manifest.contains("pkg000 = \"2"), "{manifest}");
    assert!(manifest.contains("pkg001 = \"1\""), "{manifest}");
    assert_eq!(fixture.report(&rejections)?.applied.len(), 2);
    Ok(())
}

#[tokio::test]
async fn no_op_end_round_pin_cannot_displace_retained_or_earlier_targets() -> eyre::Result<()> {
    let mut fixture = WidenFixture::new(
        4,
        indoc! {r#"
        if [ "$1" = update ]; then
          case "$*" in
            *pkg002@*)
              if grep -q 'pkg000 = "2' Cargo.toml; then
                cp displaced.lock Cargo.lock
                touch no-op-displaced
                exit 0
              fi
              ;;
          esac
          echo 'error: failed to select a version for the requirement `pkg001 = "1"`' >&2
          echo 'required by package `holder v1.0.0`' >&2
          exit 101
        fi
        if grep -q 'pkg000 = "2' Cargo.toml; then
          cp retained.lock Cargo.lock
        else
          echo 'error: original requirements reject the seed' >&2
          exit 101
        fi
    "#},
    )?;
    fixture.tool = fixture.tool.with_rejection_memo(false);
    // pkg002 already admits its target, so its singleton widening is a no-op.
    let manifest = std::fs::read_to_string(&fixture.project.manifest)?
        .replace("pkg002 = \"1\"", "pkg002 = \"2\"");
    std::fs::write(&fixture.project.manifest, manifest)?;
    std::fs::write(
        fixture.project.root.join("Cargo.lock"),
        lock_with_selected_targets(&fixture.changes, &["pkg003"]),
    )?;
    std::fs::write(
        fixture.project.root.join("retained.lock"),
        lock_with_selected_targets(&fixture.changes, &["pkg000", "pkg003"]),
    )?;
    std::fs::write(
        fixture.project.root.join("displaced.lock"),
        lock_with_selected_targets(&fixture.changes, &["pkg002"]),
    )?;
    let journal = fixture.journal().await?;
    let plan = Plan {
        changes: fixture.changes.clone(),
        rewrite: RewriteMode::Auto,
        ..Default::default()
    };
    let rejections = fixture
        .tool
        .whole_graph_resolve(&fixture.project, &plan, &journal, None)
        .await?;
    assert!(fixture.project.root.join("no-op-displaced").exists());
    let lock = read_lock(&fixture.project)?;
    assert!(lock.has_crates_io_package("pkg000", "2.0.0"));
    assert!(lock.has_crates_io_package("pkg003", "2.0.0"));
    assert!(lock.has_crates_io_package("pkg001", "1.0.0"));
    assert!(lock.has_crates_io_package("pkg002", "1.0.0"));
    let manifest = std::fs::read_to_string(&fixture.project.manifest)?;
    assert!(manifest.contains("pkg000 = \"2"), "{manifest}");
    assert!(manifest.contains("pkg001 = \"1\""), "{manifest}");
    assert!(manifest.contains("pkg002 = \"2\""), "{manifest}");
    assert_eq!(fixture.report(&rejections)?.applied.len(), 2);
    Ok(())
}

#[tokio::test]
async fn rejected_singleton_restores_bytes_permissions_and_absent_member_manifest()
-> eyre::Result<()> {
    let mut fixture = WidenFixture::new(
        1,
        indoc! {r#"
        printf '[package]\nname = "created"\n' > missing/Cargo.toml
        printf '\n# cargo touched the lock\n' >> Cargo.lock
        chmod 600 Cargo.toml
        echo 'error: failed to select a version for the requirement `pkg000 = "1"`' >&2
        echo 'required by package `holder v1.0.0`' >&2
        exit 101
    "#},
    )?;
    std::fs::create_dir(fixture.project.root.join("missing"))?;
    fixture
        .changes
        .first_mut()
        .ok_or_else(|| eyre::eyre!("candidate"))?
        .members = vec![MemberRef {
        name: "created".to_owned(),
        path: "missing".to_owned(),
    }];
    std::fs::set_permissions(
        &fixture.project.manifest,
        std::fs::Permissions::from_mode(0o640),
    )?;
    let original_manifest = std::fs::read(&fixture.project.manifest)?;
    let original_lock = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
    let journal = fixture.journal().await?;
    let planned = fixture
        .changes
        .first()
        .ok_or_else(|| eyre::eyre!("candidate"))?;
    let result = fixture
        .tool
        .tentative_widen_logged(
            WidenProbe {
                project: &fixture.project,
                candidates: &[planned],
                protected: &[],
                followers: &[],
                journal: &journal,
                observer: None,
            },
            planned,
            &mut PinRejections::new(),
            &mut MemoStats::default(),
        )
        .await?;
    assert!(matches!(result, TentativeWiden::Rejected));
    // A rejected singleton restores the same byte checkpoint as a joint probe.
    assert_eq!(std::fs::read(&fixture.project.manifest)?, original_manifest);
    assert_eq!(
        std::fs::read(fixture.project.root.join("Cargo.lock"))?,
        original_lock
    );
    assert_eq!(
        std::fs::metadata(&fixture.project.manifest)?
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    assert!(!fixture.project.root.join("missing/Cargo.toml").exists());
    assert_eq!(fixture.invocations()?.len(), 1);
    Ok(())
}

fn singleton_displacement_fixture() -> eyre::Result<WidenFixture> {
    let mut fixture = WidenFixture::new(
        2,
        indoc! {r#"
        if [ "$1" = update ]; then
          echo 'error: failed to select a version for the requirement `pkg001 = "1"`' >&2
          echo 'required by package `holder v1.0.0`' >&2
          exit 101
        fi
        cp displaced.lock Cargo.lock
    "#},
    )?;
    for planned in &mut fixture.changes {
        planned.direct = true;
        planned.members = vec![MemberRef {
            name: "app".to_owned(),
            path: ".".to_owned(),
        }];
    }
    let original_manifest = std::fs::read_to_string(&fixture.project.manifest)?
        .replace("pkg000 = \"1\"", "pkg000 = \"2\"");
    std::fs::write(&fixture.project.manifest, &original_manifest)?;
    let original_lock = lock_with_selected_targets(&fixture.changes, &["pkg000"]);
    std::fs::write(fixture.project.root.join("Cargo.lock"), &original_lock)?;
    std::fs::write(
        fixture.project.root.join("displaced.lock"),
        lock_with_selected_targets(&fixture.changes, &["pkg001"]),
    )?;
    // Metadata can land B despite its rejected precise pin, while moving A back to its old copy.
    let metadata = serde_json::json!({
        "packages": [
            {"id": "app", "name": "app", "version": "0.1.0", "manifest_path": fixture.project.manifest,
             "dependencies": [{"name": "pkg000", "req": "^2"}, {"name": "pkg001", "req": "^2"}]},
            {"id": "old-a", "name": "pkg000", "version": "1.0.0", "source": crate::lockfile::CRATES_IO_SOURCE},
            {"id": "target-b", "name": "pkg001", "version": "2.0.0", "source": crate::lockfile::CRATES_IO_SOURCE}
        ],
        "workspace_members": ["app"], "workspace_root": fixture.project.root,
        "resolve": {"nodes": [
            {"id": "app", "deps": [{"name": "pkg000", "pkg": "old-a"}, {"name": "pkg001", "pkg": "target-b"}]},
            {"id": "old-a", "deps": []}, {"id": "target-b", "deps": []}
        ]}
    });
    std::fs::write(
        fixture.project.root.join("metadata.json"),
        serde_json::to_vec(&metadata)?,
    )?;
    Ok(fixture)
}

#[tokio::test]
async fn protected_displacement_does_not_memoize_a_candidate_only_widen_rejection()
-> eyre::Result<()> {
    let fixture = singleton_displacement_fixture()?;
    let original_manifest = std::fs::read_to_string(&fixture.project.manifest)?;
    let original_lock = std::fs::read_to_string(fixture.project.root.join("Cargo.lock"))?;
    let journal = fixture.journal().await?;
    let protected = fixture
        .changes
        .first()
        .ok_or_else(|| eyre::eyre!("protected candidate"))?;
    let candidate = fixture
        .changes
        .last()
        .ok_or_else(|| eyre::eyre!("widen candidate"))?;
    let mut stats = MemoStats::default();
    let mut rejections = PinRejections::new();
    let first = fixture
        .tool
        .tentative_widen_logged(
            WidenProbe {
                project: &fixture.project,
                candidates: &[candidate],
                protected: &[protected],
                followers: &[],
                journal: &journal,
                observer: None,
            },
            candidate,
            &mut rejections,
            &mut stats,
        )
        .await?;
    assert!(matches!(first, TentativeWiden::Rejected));
    assert_eq!(fixture.invocations()?.len(), 2);
    assert_eq!(
        std::fs::read_to_string(&fixture.project.manifest)?,
        original_manifest
    );
    assert_eq!(
        std::fs::read_to_string(fixture.project.root.join("Cargo.lock"))?,
        original_lock
    );

    // The exact same inputs are viable when retaining A is no longer an obligation.
    let second = fixture
        .tool
        .tentative_widen_logged(
            WidenProbe {
                project: &fixture.project,
                candidates: &[candidate],
                protected: &[],
                followers: &[],
                journal: &journal,
                observer: None,
            },
            candidate,
            &mut rejections,
            &mut stats,
        )
        .await?;
    match second {
        TentativeWiden::Landed(evidence) => {
            assert!(evidence.reached(candidate));
            assert!(!evidence.reached(protected));
        }
        TentativeWiden::Unchanged | TentativeWiden::Rejected => {
            eyre::bail!("unprotected retry must resolve and land the candidate");
        }
    }
    let invocations = fixture.invocations()?;
    assert_eq!(invocations.len(), 3);
    assert!(
        invocations
            .last()
            .is_some_and(|arguments| arguments.starts_with("metadata "))
    );
    // Only the legitimate inner precise rejection is reused; both outer widen lookups miss.
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.misses, 3);
    Ok(())
}

#[tokio::test]
async fn transitive_widen_memo_does_not_skip_a_later_member_metadata_mode() -> eyre::Result<()> {
    let fixture = WidenFixture::new(
        2,
        indoc! {r#"
        echo 'error: failed to select a version for the requirement `pkg001 = "1"`' >&2
        echo 'required by package `holder v1.0.0`' >&2
        exit 101
    "#},
    )?;
    std::fs::write(
        fixture.project.root.join("Cargo.lock"),
        lock_with_selected_targets(&fixture.changes, &["pkg000"]),
    )?;
    let journal = fixture.journal().await?;
    let original_manifest = std::fs::read(&fixture.project.manifest)?;
    let original_lock = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
    let candidate = fixture
        .changes
        .last()
        .ok_or_else(|| eyre::eyre!("candidate"))?;
    let protected = Change {
        direct: true,
        members: vec![MemberRef {
            name: "app".to_owned(),
            path: ".".to_owned(),
        }],
        ..fixture
            .changes
            .first()
            .ok_or_else(|| eyre::eyre!("protected candidate"))?
            .clone()
    };
    let mut stats = MemoStats::default();
    for protected in [&[][..], &[&protected][..]] {
        let result = fixture
            .tool
            .tentative_widen_logged(
                WidenProbe {
                    project: &fixture.project,
                    candidates: &[candidate],
                    protected,
                    followers: &[],
                    journal: &journal,
                    observer: None,
                },
                candidate,
                &mut PinRejections::new(),
                &mut stats,
            )
            .await?;
        assert!(matches!(result, TentativeWiden::Rejected));
        assert_eq!(std::fs::read(&fixture.project.manifest)?, original_manifest);
        assert_eq!(
            std::fs::read(fixture.project.root.join("Cargo.lock"))?,
            original_lock
        );
    }
    // The original transitive rejection skips metadata; the changed mode must run it.
    let invocations = fixture.invocations()?;
    assert_eq!(invocations.len(), 2);
    assert!(
        invocations
            .first()
            .is_some_and(|arguments| arguments.starts_with("update "))
    );
    assert!(
        invocations
            .last()
            .is_some_and(|arguments| arguments.starts_with("metadata "))
    );
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.misses, 3);
    Ok(())
}

#[tokio::test]
async fn composite_widen_transport_failures_disqualify_the_outer_rejection() -> eyre::Result<()> {
    for update_transport in [true, false] {
        let mut fixture = WidenFixture::new(
            1,
            indoc! {r#"
            if [ "$1" = update ]; then
              if [ -f update-transport ]; then
                echo 'error: failed to download config.json' >&2
                echo 'Caused by: Could not resolve host: index.crates.io' >&2
              else
                echo 'error: failed to select a version for the requirement `pkg000 = "1"`' >&2
                echo 'required by package `holder v1.0.0`' >&2
              fi
              exit 101
            fi
            if [ -f metadata-transport ]; then
              rm metadata-transport
              echo 'error: failed to download config.json' >&2
              echo 'Caused by: Could not resolve host: index.crates.io' >&2
              exit 101
            fi
            if [ -f update-transport ]; then
              echo 'error: failed to select a version for the requirement `pkg000 = "1"`' >&2
              echo 'required by package `holder v1.0.0`' >&2
              exit 101
            fi
        "#},
        )?;
        let marker = if update_transport {
            "update-transport"
        } else {
            "metadata-transport"
        };
        std::fs::write(fixture.project.root.join(marker), "")?;
        let candidate = fixture
            .changes
            .first_mut()
            .ok_or_else(|| eyre::eyre!("candidate"))?;
        candidate.direct = true;
        candidate.members = vec![MemberRef {
            name: "app".to_owned(),
            path: ".".to_owned(),
        }];
        let journal = fixture.journal().await?;
        let candidate = fixture
            .changes
            .first()
            .ok_or_else(|| eyre::eyre!("candidate"))?;
        let original_manifest = std::fs::read(&fixture.project.manifest)?;
        let original_lock = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
        let mut stats = MemoStats::default();
        for _ in 0..2 {
            let result = fixture
                .tool
                .tentative_widen_logged(
                    WidenProbe {
                        project: &fixture.project,
                        candidates: &[candidate],
                        protected: &[],
                        followers: &[],
                        journal: &journal,
                        observer: None,
                    },
                    candidate,
                    &mut PinRejections::new(),
                    &mut stats,
                )
                .await?;
            assert!(matches!(result, TentativeWiden::Rejected));
            assert_eq!(std::fs::read(&fixture.project.manifest)?, original_manifest);
            assert_eq!(
                std::fs::read(fixture.project.root.join("Cargo.lock"))?,
                original_lock
            );
        }
        let invocations = fixture.invocations()?;
        let updates = invocations
            .iter()
            .filter(|arguments| arguments.starts_with("update "))
            .count();
        let metadata = invocations
            .iter()
            .filter(|arguments| arguments.starts_with("metadata "))
            .count();
        assert_eq!(metadata, 2);
        assert_eq!(updates, if update_transport { 2 } else { 1 });
        // A resolver rejection cannot erase the earlier transport failure's disqualification.
        assert_eq!(stats.hits, usize::from(!update_transport));
    }
    Ok(())
}
