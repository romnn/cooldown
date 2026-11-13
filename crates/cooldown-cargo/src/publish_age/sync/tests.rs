use super::write_repo_native;
use crate::publish_age::CargoEnvironment;
use camino::Utf8PathBuf;
use color_eyre::eyre;
use cooldown_core::{DiagnosticKind, ResolvedPolicy, SyncReport, WindowSpec};
use indoc::indoc;
use jiff::SignedDuration;
use std::collections::BTreeMap;

fn repo() -> eyre::Result<(tempfile::TempDir, Utf8PathBuf)> {
    let directory = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
        .map_err(|path| eyre::eyre!("non-UTF-8 temp dir {}", path.display()))?;
    Ok((directory, root))
}

fn policy(window: WindowSpec) -> ResolvedPolicy {
    ResolvedPolicy {
        default_window: Some(window),
        exempt_packages: Vec::new(),
        withheld_exemptions: Vec::new(),
        package_windows: BTreeMap::new(),
        admitted_versions: Some(Vec::new()),
    }
}

fn days(count: i64) -> WindowSpec {
    WindowSpec::MinAge(SignedDuration::from_hours(24 * count))
}

fn warning_kinds(report: &SyncReport) -> Vec<DiagnosticKind> {
    match report {
        SyncReport::Written { warnings, .. } | SyncReport::Unchanged { warnings, .. } => {
            warnings.iter().map(|warning| warning.kind).collect()
        }
        other => panic!("unexpected sync report {other:?}"),
    }
}

#[test]
fn writes_a_new_config_toml_once() -> eyre::Result<()> {
    let (_directory, root) = repo()?;
    let env = CargoEnvironment::default();
    let config = root.join(".cargo/config.toml");

    // The first sync creates the file and its directory.
    let first = write_repo_native(&root, &policy(days(7)), &env, false)?;
    std::assert_matches!(&first, SyncReport::Written { path, .. } if *path == config);
    assert_eq!(
        std::fs::read_to_string(&config)?,
        indoc! {r#"
            [registries.crates-io]
            min-publish-age = "1 week"
        "#}
    );

    // A second sync finds the age already in place.
    let second = write_repo_native(&root, &policy(days(7)), &env, false)?;
    std::assert_matches!(second, SyncReport::Unchanged { .. });
    Ok(())
}

#[test]
fn an_equal_age_spelled_differently_is_unchanged() -> eyre::Result<()> {
    let (_directory, root) = repo()?;
    let config = root.join(".cargo/config.toml");
    std::fs::create_dir_all(root.join(".cargo"))?;
    let original = "[registries.crates-io]\nmin-publish-age = \"7days\"\n";
    std::fs::write(&config, original)?;

    let report = write_repo_native(&root, &policy(days(7)), &CargoEnvironment::default(), false)?;

    // Cargo reads both spellings as the same age, so the user's spelling stays.
    std::assert_matches!(report, SyncReport::Unchanged { .. });
    assert_eq!(std::fs::read_to_string(&config)?, original);
    Ok(())
}

#[test]
fn writes_into_the_legacy_config_cargo_actually_reads() -> eyre::Result<()> {
    let (_directory, root) = repo()?;
    let legacy = root.join(".cargo/config");
    std::fs::create_dir_all(root.join(".cargo"))?;
    std::fs::write(&legacy, "[net]\nretry = 5\n")?;

    let report = write_repo_native(
        &root,
        &policy(days(14)),
        &CargoEnvironment::default(),
        false,
    )?;

    // The extensionless file shadows `config.toml`, so the age must land in it, beside the
    // settings it already holds.
    std::assert_matches!(&report, SyncReport::Written { path, .. } if *path == legacy);
    assert!(!root.join(".cargo/config.toml").exists());
    assert_eq!(
        std::fs::read_to_string(&legacy)?,
        indoc! {r#"
            [net]
            retry = 5

            [registries.crates-io]
            min-publish-age = "2 weeks"
        "#}
    );
    Ok(())
}

#[test]
fn dry_run_reports_without_writing() -> eyre::Result<()> {
    let (_directory, root) = repo()?;
    let report = write_repo_native(&root, &policy(days(3)), &CargoEnvironment::default(), true)?;
    std::assert_matches!(report, SyncReport::Written { .. });
    assert!(!root.join(".cargo").exists());
    Ok(())
}

#[test]
fn a_freeze_leaves_the_age_alone_with_a_warning() -> eyre::Result<()> {
    let (_directory, root) = repo()?;
    let config = root.join(".cargo/config.toml");
    std::fs::create_dir_all(root.join(".cargo"))?;
    let original = "[registries.crates-io]\nmin-publish-age = \"30 days\"\n";
    std::fs::write(&config, original)?;
    let freeze = WindowSpec::Freeze("2026-01-01T00:00:00Z".parse()?);

    let report = write_repo_native(&root, &policy(freeze), &CargoEnvironment::default(), false)?;

    // Cargo cannot express an absolute cutoff, so the existing age stays and the user is told.
    assert_eq!(warning_kinds(&report), [DiagnosticKind::StricterNative]);
    std::assert_matches!(report, SyncReport::Unchanged { .. });
    assert_eq!(std::fs::read_to_string(&config)?, original);
    Ok(())
}

#[test]
fn reports_policy_cargo_cannot_carry() -> eyre::Result<()> {
    let (_directory, root) = repo()?;
    std::fs::create_dir_all(root.join(".cargo"))?;
    std::fs::write(
        root.join(".cargo/config.toml"),
        "[resolver]\nincompatible-publish-age = \"allow\"\n",
    )?;
    let mut with_package = policy(days(7));
    with_package
        .package_windows
        .insert("serde".to_owned(), WindowSpec::Latest);

    let report = write_repo_native(&root, &with_package, &CargoEnvironment::default(), false)?;

    // The age is still written, with one warning for the per-package window cargo drops and one
    // for the resolver switch that keeps a bare cargo from enforcing it.
    assert_eq!(
        warning_kinds(&report),
        [DiagnosticKind::StricterNative, DiagnosticKind::Config]
    );
    std::assert_matches!(report, SyncReport::Written { .. });
    Ok(())
}
