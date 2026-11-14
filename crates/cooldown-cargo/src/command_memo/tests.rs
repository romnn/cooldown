use super::*;
use color_eyre::eyre;
use indoc::{formatdoc, indoc};
use std::os::unix::fs::PermissionsExt as _;

/// The fake `cargo`, run in `<fixture>/project`, logging each command to `<fixture>/invocations`.
///
/// `update` writes `<fixture>/updated.lock` and succeeds, refuses as the resolver does while
/// `<fixture>/refuse` exists, and fails as a transport error while `<fixture>/transport` exists.
/// `metadata` writes `<fixture>/resolved.lock` and prints the package `app`.
/// With `<fixture>/mutate`, a command also edits the manifest while it runs.
const SCRIPT: &str = indoc! {r#"
    #!/bin/sh
    set -eu
    printf '%s\n' "$*" >> ../invocations
    if [ -f ../mutate ]; then printf '# edited\n' >> Cargo.toml; fi
    case "$1" in
      update)
        if [ -f ../refuse ]; then
          echo 'error: failed to select a version for the requirement `demo = "=2.0.0"`' >&2
          echo 'required by package `holder v1.0.0`' >&2
          exit 101
        fi
        if [ -f ../transport ]; then
          echo 'error: failed to download config.json' >&2
          exit 101
        fi
        cp ../updated.lock Cargo.lock
        echo 'updated'
        ;;
      metadata)
        cp ../resolved.lock Cargo.lock
        printf '{"packages": [{"id": "app", "name": "app", "version": "0.1.0", "source": null, "manifest_path": "%s/Cargo.toml", "targets": []}], "workspace_members": ["app"], "workspace_root": "%s", "resolve": null}\n' "$PWD" "$PWD"
        ;;
    esac
"#};

const MANIFEST: &str = indoc! {r#"
    [package]
    name = "app"
    version = "0.1.0"
    edition = "2024"
"#};

const LOCK: &str = "version = 4\n";

struct CommandFixture {
    fixture: Utf8PathBuf,
    project: Project,
    cargo: Cargo,
    memo: RejectionMemo,
    outcomes: CommandOutcomes,
    reads: LockedReads,
    #[expect(dead_code, reason = "dropping the guard deletes the fixture directory")]
    directory: tempfile::TempDir,
}

impl CommandFixture {
    /// A fixture whose project is registered as the staged copy of another directory, with the
    /// whole fixture directory as its private tree.
    fn staged() -> eyre::Result<Self> {
        let fixture = Self::unstaged()?;
        let original = fixture.fixture.join("original");
        std::fs::create_dir_all(&original)?;
        fixture
            .memo
            .register_origin(&fixture.project.root, &original)?;
        fixture.register_staged();
        Ok(fixture)
    }

    fn register_staged(&self) {
        self.outcomes
            .register_staged(&self.project.root, &self.fixture);
    }

    fn unstaged() -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        let fixture = Utf8PathBuf::from_path_buf(std::fs::canonicalize(directory.path())?)
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let root = fixture.join("project");
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::write(root.join("src/lib.rs"), "")?;
        std::fs::write(root.join("Cargo.toml"), MANIFEST)?;
        std::fs::write(root.join("Cargo.lock"), LOCK)?;
        std::fs::write(fixture.join("updated.lock"), "version = 4\n# updated\n")?;
        std::fs::write(fixture.join("resolved.lock"), "version = 4\n# resolved\n")?;
        let fake = fixture.join("fake-cargo");
        std::fs::write(&fake, SCRIPT)?;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
        Ok(Self {
            project: Project {
                manifest: root.join("Cargo.toml"),
                root,
                kind: crate::CARGO_ID,
                exclude_newer: None,
                exclude_newer_package: std::collections::BTreeMap::new(),
                generated_members: cooldown_core::GeneratedMembers::undeclared(),
            },
            cargo: Cargo::with_bin(fake.as_str()),
            fixture,
            memo: RejectionMemo::default(),
            outcomes: CommandOutcomes::default(),
            reads: LockedReads::default(),
            directory,
        })
    }

    fn commands(&self) -> MemoizedCargo<'_> {
        MemoizedCargo {
            cargo: &self.cargo,
            memo: &self.memo,
            outcomes: &self.outcomes,
            reads: &self.reads,
            enabled: true,
        }
    }

    fn reader(&self) -> LockedReader<'_> {
        LockedReader {
            cargo: &self.cargo,
            memo: &self.memo,
            reads: &self.reads,
        }
    }

    async fn pin(&self, to: &str) -> Result<(), CoreError> {
        self.commands()
            .update_precise_crates_io(&self.project, "demo", "1.0.0", to)
            .await
    }

    fn invocations(&self) -> eyre::Result<usize> {
        let log = self.fixture.join("invocations");
        Ok(if log.exists() {
            std::fs::read_to_string(log)?.lines().count()
        } else {
            0
        })
    }

    fn lock(&self) -> eyre::Result<String> {
        Ok(std::fs::read_to_string(
            self.project.root.join("Cargo.lock"),
        )?)
    }

    /// Puts the lock back the way the adapter restores a refused step.
    fn restore_lock(&self) -> eyre::Result<()> {
        Ok(std::fs::write(self.project.root.join("Cargo.lock"), LOCK)?)
    }

    fn mark(&self, name: &str) -> eyre::Result<()> {
        Ok(std::fs::write(self.fixture.join(name), "")?)
    }
}

/// A command repeated from the inputs it ran on replays its result and the lock it wrote, without
/// running Cargo.
#[tokio::test]
async fn a_repeated_update_replays_its_result_and_lock() -> eyre::Result<()> {
    let fixture = CommandFixture::staged()?;

    fixture.pin("2.0.0").await?;
    let written = fixture.lock()?;
    fixture.restore_lock()?;
    fixture.pin("2.0.0").await?;

    // The replay rewrote the lock Cargo wrote, and Cargo ran once.
    assert_eq!(written, "version = 4\n# updated\n");
    assert_eq!(fixture.lock()?, written);
    assert_eq!(fixture.invocations()?, 1);
    Ok(())
}

/// A resolver refusal replays as the same error, leaving the lock as it was.
#[tokio::test]
async fn a_repeated_resolver_refusal_replays_the_same_error() -> eyre::Result<()> {
    let fixture = CommandFixture::staged()?;
    fixture.mark("refuse")?;

    let first = fixture
        .pin("2.0.0")
        .await
        .err()
        .map(|err| format!("{err:?}"));
    let second = fixture
        .pin("2.0.0")
        .await
        .err()
        .map(|err| format!("{err:?}"));

    assert!(
        first.as_deref().is_some_and(|err| err.contains("holder")),
        "{first:?}"
    );
    assert_eq!(second, first);
    assert_eq!(fixture.lock()?, LOCK);
    assert_eq!(fixture.invocations()?, 1);
    Ok(())
}

/// Any change of what the outcome depends on runs Cargo again: the arguments or a native input.
#[tokio::test]
async fn a_changed_argument_or_input_runs_again() -> eyre::Result<()> {
    let fixture = CommandFixture::staged()?;
    fixture.mark("refuse")?;
    fixture.pin("2.0.0").await.err();

    // Another target version
    fixture.pin("2.1.0").await.err();
    assert_eq!(fixture.invocations()?, 2);

    // An edited manifest
    std::fs::write(&fixture.project.manifest, format!("{MANIFEST}# edited\n"))?;
    fixture.pin("2.0.0").await.err();
    assert_eq!(fixture.invocations()?, 3);
    Ok(())
}

/// A whole-graph resolve that left the lock as it was serves the locked read of the same inputs
/// that follows, without running Cargo, whether the resolve ran or replayed.
#[tokio::test]
async fn an_unchanged_resolve_serves_the_next_locked_read() -> eyre::Result<()> {
    let fixture = CommandFixture::staged()?;
    // Cargo settles on the lock the project already has.
    std::fs::write(fixture.fixture.join("resolved.lock"), LOCK)?;

    // A resolve that ran, then a locked read
    fixture.commands().metadata(&fixture.project).await?;
    fixture.reader().graph(&fixture.project).await?;
    assert_eq!(fixture.invocations()?, 1);

    // A resolve that replayed, then a locked read the replay alone has to serve
    fixture.reads.clear();
    fixture.commands().metadata(&fixture.project).await?;
    fixture.reader().graph(&fixture.project).await?;
    assert_eq!(fixture.invocations()?, 1);
    Ok(())
}

/// A resolve that rewrote the lock vouches for no locked read: once the earlier lock is back, a
/// locked read runs Cargo, which may refuse that lock.
#[tokio::test]
async fn a_resolve_that_rewrote_the_lock_serves_no_locked_read() -> eyre::Result<()> {
    let fixture = CommandFixture::staged()?;

    fixture.commands().metadata(&fixture.project).await?;
    fixture.restore_lock()?;
    fixture.reader().graph(&fixture.project).await?;

    assert_eq!(fixture.invocations()?, 2);
    Ok(())
}

/// Writes a path package `dep` into `directory`.
fn dependency(directory: &Utf8Path) -> eyre::Result<()> {
    std::fs::create_dir_all(directory.join("src"))?;
    std::fs::write(directory.join("src/lib.rs"), "")?;
    Ok(std::fs::write(
        directory.join("Cargo.toml"),
        indoc! {r#"
            [package]
            name = "dep"
            version = "0.1.0"
        "#},
    )?)
}

/// Only a project whose packages and target sources all lie in its staged copy replays: Cargo
/// discovers the targets of a package from files outside the copy, which anything may change.
#[tokio::test]
async fn only_packages_inside_the_staged_copy_replay() -> eyre::Result<()> {
    let fixture = CommandFixture::staged()?;
    let outside = tempfile::tempdir()?;
    let outside = Utf8PathBuf::from_path_buf(std::fs::canonicalize(outside.path())?)
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    let pin_twice = async || -> eyre::Result<()> {
        for _ in 0..2 {
            fixture.restore_lock()?;
            fixture.pin("2.0.0").await?;
        }
        Ok(())
    };
    // Paths are serialized, so they stay valid TOML strings whatever characters they hold.
    let manifest = |extra: &str| {
        std::fs::write(
            &fixture.project.manifest,
            formatdoc! {"
            {MANIFEST}
            {extra}
        "},
        )
    };

    // A path dependency beside the project in the staged copy
    let inside = fixture.fixture.join("dep");
    dependency(&inside)?;
    let path = toml::Value::String(inside.to_string());
    manifest(&format!("[dependencies]\ndep = {{ path = {path} }}"))?;
    pin_twice().await?;
    assert_eq!(fixture.invocations()?, 1);

    // A path dependency outside the staged copy
    dependency(&outside.join("dep"))?;
    let path = toml::Value::String(outside.join("dep").to_string());
    manifest(&format!("[dependencies]\ndep = {{ path = {path} }}"))?;
    pin_twice().await?;
    assert_eq!(fixture.invocations()?, 3);

    // A target source outside the staged copy
    std::fs::write(outside.join("lib.rs"), "")?;
    let path = toml::Value::String(outside.join("lib.rs").to_string());
    manifest(&format!("[lib]\npath = {path}"))?;
    pin_twice().await?;
    assert_eq!(fixture.invocations()?, 5);

    // A symbolic link in the staged copy to a target source outside it
    std::os::unix::fs::symlink(
        outside.join("lib.rs"),
        fixture.project.root.join("linked.rs"),
    )?;
    manifest("[lib]\npath = \"linked.rs\"")?;
    pin_twice().await?;
    assert_eq!(fixture.invocations()?, 7);
    Ok(())
}

/// A failure the same inputs need not reproduce, such as a transport error, runs again.
#[tokio::test]
async fn a_failure_other_than_a_resolver_refusal_runs_again() -> eyre::Result<()> {
    let fixture = CommandFixture::staged()?;
    fixture.mark("transport")?;

    for _ in 0..2 {
        assert!(fixture.pin("2.0.0").await.is_err());
    }

    assert_eq!(fixture.invocations()?, 2);
    Ok(())
}

/// A project that is not a registered staged copy, or a memo switched off, runs every command.
#[tokio::test]
async fn unstaged_projects_and_disabled_memos_run_every_command() -> eyre::Result<()> {
    let unstaged = CommandFixture::unstaged()?;
    for _ in 0..2 {
        unstaged.pin("2.0.0").await?;
        unstaged.restore_lock()?;
    }
    assert_eq!(unstaged.invocations()?, 2);

    let disabled = CommandFixture::staged()?;
    let commands = MemoizedCargo {
        enabled: false,
        ..disabled.commands()
    };
    for _ in 0..2 {
        commands
            .update_precise_crates_io(&disabled.project, "demo", "1.0.0", "2.0.0")
            .await?;
        disabled.restore_lock()?;
    }
    assert_eq!(disabled.invocations()?, 2);
    Ok(())
}

/// A command that changed an input other than the lock while it ran is not recorded, since Cargo
/// may have read that input in its new state.
#[tokio::test]
async fn a_command_racing_an_input_change_is_not_recorded() -> eyre::Result<()> {
    let fixture = CommandFixture::staged()?;
    fixture.mark("mutate")?;
    fixture.pin("2.0.0").await?;
    std::fs::remove_file(fixture.fixture.join("mutate"))?;

    std::fs::write(&fixture.project.manifest, MANIFEST)?;
    fixture.restore_lock()?;
    fixture.pin("2.0.0").await?;

    assert_eq!(fixture.invocations()?, 2);
    Ok(())
}

/// A resolve recorded for a caller that needed only the lock keeps no output, so a caller that
/// needs the graph runs Cargo, while an outcome kept with its output serves both.
#[tokio::test]
async fn a_resolve_kept_without_output_does_not_serve_a_graph() -> eyre::Result<()> {
    let fixture = CommandFixture::staged()?;
    let commands = fixture.commands();

    // A status-only resolve, then a graph read of the same inputs
    commands.resolve(&fixture.project).await?;
    fixture.restore_lock()?;
    let graph = commands.metadata(&fixture.project).await?;
    assert_eq!(fixture.invocations()?, 2);
    assert!(graph.roots.contains("app"));

    // Both replay from the outcome kept with its output
    for _ in 0..2 {
        fixture.restore_lock()?;
        commands.resolve(&fixture.project).await?;
        fixture.restore_lock()?;
        let replayed = commands.metadata(&fixture.project).await?;
        assert_eq!(replayed.roots, graph.roots);
    }
    assert_eq!(fixture.invocations()?, 2);
    assert_eq!(fixture.lock()?, "version = 4\n# resolved\n");
    Ok(())
}

/// The least recently used outcome is evicted once the store is over its budget.
#[tokio::test]
async fn the_least_recently_used_outcome_is_evicted() -> eyre::Result<()> {
    let mut fixture = CommandFixture::staged()?;
    fixture.outcomes = CommandOutcomes::with_budget(Budget {
        bytes: usize::MAX,
        entries: 2,
    });
    fixture.register_staged();
    fixture.mark("refuse")?;

    // Two outcomes fit; reusing the first makes the second the least recently used.
    for to in ["2.0.0", "2.1.0", "2.0.0"] {
        fixture.pin(to).await.err();
    }
    assert_eq!(fixture.invocations()?, 2);

    // A third outcome evicts the second, while the first stays until a fourth evicts the third.
    for to in ["2.2.0", "2.0.0", "2.1.0"] {
        fixture.pin(to).await.err();
    }
    assert_eq!(fixture.invocations()?, 4);

    // A byte budget below one entry keeps nothing.
    fixture.outcomes = CommandOutcomes::with_budget(Budget {
        bytes: Budget::ENTRY_OVERHEAD,
        entries: usize::MAX,
    });
    fixture.register_staged();
    for _ in 0..2 {
        fixture.pin("2.0.0").await.err();
    }
    assert_eq!(fixture.invocations()?, 6);
    Ok(())
}

/// Whether the resolver can choose differently over time or with the crates downloaded so far,
/// which makes a command run every time.
#[test]
fn time_and_download_dependent_resolution_is_not_stable() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    let project = root.join("project");
    std::fs::create_dir_all(project.join(".cargo"))?;
    let bare = CargoEnvironment {
        cargo_home: Some(root.join("home")),
        ..CargoEnvironment::default()
    };
    let config = |text: &str| std::fs::write(project.join(".cargo/config.toml"), text);
    let crates_io_env = |value: &str| CargoEnvironment {
        registry_ages: vec![value.to_owned()],
        ..bare.clone()
    };

    // No age and online
    assert!(stable_resolution(&project, &bare, None));

    // The command line's crates.io age, zero or positive
    assert!(stable_resolution(&project, &bare, Some("0")));
    assert!(!stable_resolution(&project, &bare, Some("3 days")));

    // A crates.io age set in the environment, whose variable also names a registry called
    // `crates_io` that the command line's crates.io age does not override
    assert!(!stable_resolution(&project, &crates_io_env("7 days"), None));
    assert!(!stable_resolution(
        &project,
        &crates_io_env("7 days"),
        Some("0")
    ));
    assert!(stable_resolution(&project, &crates_io_env("0"), Some("0")));

    // A configured crates.io age the command line overrides, beside other registries' ages
    config("[registries.crates-io]\nmin-publish-age = \"7 days\"\n")?;
    assert!(stable_resolution(&project, &bare, Some("0")));
    config("[registries.private]\nmin-publish-age = \"7 days\"\n")?;
    assert!(!stable_resolution(&project, &bare, Some("0")));
    config("[registry]\nglobal-min-publish-age = \"7 days\"\n")?;
    assert!(!stable_resolution(&project, &bare, Some("0")));
    config("[registry]\nglobal-min-publish-age = \"0\"\n")?;
    assert!(stable_resolution(&project, &bare, Some("0")));
    let global = CargoEnvironment {
        global_age: Some("1 day".to_owned()),
        ..bare.clone()
    };
    assert!(!stable_resolution(&project, &global, Some("0")));

    // Offline resolution, from the environment or the config
    let offline = CargoEnvironment {
        offline: Some("true".to_owned()),
        ..bare.clone()
    };
    assert!(!stable_resolution(&project, &offline, None));
    let online = CargoEnvironment {
        offline: Some("false".to_owned()),
        ..bare.clone()
    };
    assert!(stable_resolution(&project, &online, Some("0")));
    config("[net]\noffline = true\n")?;
    assert!(!stable_resolution(&project, &bare, None));

    // A setting that cannot be read
    config("[registry]\nglobal-min-publish-age = \"soon\"\n")?;
    assert!(!stable_resolution(&project, &bare, None));
    config("not toml")?;
    assert!(!stable_resolution(&project, &bare, None));
    Ok(())
}
