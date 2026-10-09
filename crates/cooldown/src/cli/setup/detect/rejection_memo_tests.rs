use super::adapter_set;
use crate::cli::Cli;
use camino::Utf8PathBuf;
use clap::Parser as _;
use color_eyre::eyre;
use cooldown_cargo::CARGO_ID;
use cooldown_core::fs::{ManifestFamily, ProjectWriteLease};
use cooldown_core::{
    Change, GeneratedMembers, MutationExecution, PackageId, Plan, PreparedMutation, Project,
    RewriteMode, UpdateKind, Version,
};
use cooldown_registry::HttpOptions;
use indoc::indoc;
use std::os::unix::fs::PermissionsExt as _;

/// The real adapter factory honors memo opt-outs without changing rejection explanations.
#[tokio::test]
async fn cli_memo_opt_out_repeats_native_cargo_attempts() -> eyre::Result<()> {
    if let Ok(flag) = std::env::var("COOLDOWN_TEST_MEMO_FLAG") {
        return attempt_twice(&flag).await;
    }

    // Process isolation keeps Cargo and clap environment overrides away from other tests.
    for flag in ["default", "--no-memo", "--fresh", "--no-cache"] {
        let directory = tempfile::tempdir()?;
        let fixture_root = Utf8PathBuf::from_path_buf(std::fs::canonicalize(directory.path())?)
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let root = fixture_root.join("project");
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::write(root.join("src/main.rs"), "fn main() {}")?;
        std::fs::write(
            root.join("Cargo.toml"),
            indoc! {r#"
                [package]
                name = "app"
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
        let metadata = serde_json::json!({
            "workspace_root": root,
            "workspace_members": ["app"],
            "packages": [{
                "id": "app", "name": "app", "version": "0.1.0",
                "manifest_path": root.join("Cargo.toml"),
                "targets": [{"src_path": root.join("src/main.rs")}]
            }],
            "resolve": {"nodes": []}
        });
        std::fs::write(
            fixture_root.join("metadata.json"),
            serde_json::to_vec(&metadata)?,
        )?;
        let script = fixture_root.join("fake-cargo");
        std::fs::write(
            &script,
            indoc! {r#"
                #!/bin/sh
                set -eu
                if [ "$1" = metadata ]; then
                    cat "$COOLDOWN_TEST_MEMO_ROOT/metadata.json"
                    exit 0
                fi
                printf '%s\n' "$*" >> "$COOLDOWN_TEST_MEMO_ROOT/invocations"
                echo 'error: failed to select a version for the requirement `demo = "=2.0.0"`' >&2
                echo 'required by package `holder v1.0.0`' >&2
                exit 101
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "cli::setup::detect::rejection_memo_tests::cli_memo_opt_out_repeats_native_cargo_attempts",
                "--nocapture",
            ])
            .env("COOLDOWN_CARGO", &script)
            .env("COOLDOWN_TEST_MEMO_FLAG", flag)
            .env("COOLDOWN_TEST_MEMO_ROOT", &fixture_root)
            .env("XDG_CACHE_HOME", fixture_root.join("cache"))
            .env_remove("COOLDOWN_NO_MEMO")
            .output()?;
        assert!(
            output.status.success(),
            "{flag}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }
    Ok(())
}

async fn attempt_twice(flag: &str) -> eyre::Result<()> {
    let mut args = vec!["cooldown", "upgrade", "--rewrite"];
    if flag != "default" {
        args.push(flag);
    }
    let cli = Cli::try_parse_from(args)?;
    let (adapters, _) = adapter_set(
        HttpOptions {
            fresh: cli.global.fresh,
            ..Default::default()
        },
        cli.global.rejection_memo_enabled(cli.global.fresh),
        false,
    )?;
    let writer = adapters
        .writer(CARGO_ID)
        .ok_or_else(|| eyre::eyre!("Cargo writer was not registered"))?;
    let fixture_root = Utf8PathBuf::from(std::env::var("COOLDOWN_TEST_MEMO_ROOT")?);
    let root = fixture_root.join("project");
    let project = Project {
        manifest: root.join("Cargo.toml"),
        root: root.clone(),
        kind: CARGO_ID,
        exclude_newer: None,
        generated_members: GeneratedMembers::undeclared(),
    };
    let manifest_before = std::fs::read(&project.manifest)?;
    let lock_before = std::fs::read(root.join("Cargo.lock"))?;
    let lease = ProjectWriteLease::acquire(&root, &ManifestFamily::named("Cargo.toml"))?;
    let MutationExecution::Isolated(strategy) = writer.mutation_execution() else {
        eyre::bail!("Cargo writer does not isolate mutations");
    };
    let stage = strategy.prepare(&project, lease.coordination()).await?;
    let crate::cli::Command::Upgrade { rewrite, .. } = cli.command else {
        eyre::bail!("fixture did not parse an upgrade command");
    };
    // With no owning declaration, rewriting is a no-op and bypasses automatic joint widens.
    let plan = Plan {
        rewrite: if rewrite {
            RewriteMode::Always
        } else {
            RewriteMode::Auto
        },
        changes: vec![Change {
            package: PackageId::new(CARGO_ID, "demo", Some("crates.io".to_string())),
            from: Version::new("1.0.0"),
            to: Version::new("2.0.0"),
            kind: UpdateKind::Major,
            downgrade: false,
            direct: false,
            members: Vec::new(),
        }],
        ..Default::default()
    };
    let mutation =
        PreparedMutation::prepare_isolated(writer, &stage.mutation_project(), &plan).await?;
    let first = writer.apply(&mutation).await?;
    let second = writer.apply(&mutation).await?;
    assert!(first.applied.is_empty());
    assert!(second.applied.is_empty());
    assert_eq!(first.skipped.len(), 1);
    let first_details = first
        .skipped
        .iter()
        .map(|skip| &skip.detail)
        .collect::<Vec<_>>();
    let second_details = second
        .skipped
        .iter()
        .map(|skip| &skip.detail)
        .collect::<Vec<_>>();
    assert_eq!(first_details, second_details);
    assert!(
        first_details
            .iter()
            .any(|detail| detail.as_ref().is_some_and(|text| text.contains("holder")))
    );
    let calls = std::fs::read_to_string(fixture_root.join("invocations"))?
        .lines()
        .count();
    assert_eq!(calls, if flag == "default" { 1 } else { 2 });
    assert_eq!(std::fs::read(&project.manifest)?, manifest_before);
    assert_eq!(std::fs::read(root.join("Cargo.lock"))?, lock_before);
    Ok(())
}
