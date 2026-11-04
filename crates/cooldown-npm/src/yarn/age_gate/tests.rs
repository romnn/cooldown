//! Yarn Berry age-gate sync: what `sync` writes into `.yarnrc.yml`, what it reports, and what it
//! refuses without writing.

use super::support::GateSupport;
use super::{SyncEnvironment, write_supported, written_minutes};
use crate::manifest::workspaces::YarnEnvironment;
use camino::Utf8PathBuf;
use color_eyre::eyre;
use cooldown_core::{
    AdmittedVersion, CoreError, Diagnostic, DiagnosticKind, ResolvedPolicy, SyncReport, Version,
    WindowSpec,
};
use indoc::indoc;
use jiff::SignedDuration;

/// A project root in a temporary directory, with an isolated home directory beside it.
struct Scratch {
    /// Owns the temporary directory; dropping it deletes the tree, so it stays bound.
    _guard: tempfile::TempDir,
    root: Utf8PathBuf,
    home: Utf8PathBuf,
}

impl Scratch {
    fn new() -> eyre::Result<Self> {
        let guard = tempfile::tempdir()?;
        let top = Utf8PathBuf::from_path_buf(guard.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let root = top.join("project");
        let home = top.join("home");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&home)?;
        Ok(Scratch {
            _guard: guard,
            root,
            home,
        })
    }

    fn rc(&self) -> Utf8PathBuf {
        self.root.join(".yarnrc.yml")
    }

    fn write_rc(&self, content: &str) -> eyre::Result<()> {
        Ok(std::fs::write(self.rc(), content)?)
    }

    fn read_rc(&self) -> eyre::Result<String> {
        Ok(std::fs::read_to_string(self.rc())?)
    }

    /// An environment that sets no Yarn variable and whose home is this scratch's own.
    fn environment(&self) -> SyncEnvironment {
        SyncEnvironment {
            yarn: YarnEnvironment {
                home: Some(self.home.clone()),
                ..YarnEnvironment::default()
            },
            ..SyncEnvironment::default()
        }
    }
}

/// A Yarn that reads every gate setting, the pinned 4.18.1's support.
const CURRENT: GateSupport = GateSupport {
    scope_gates_apply: true,
    scope_gates_writable: true,
};

/// A Yarn from 4.10 through 4.16, which reads the global settings only.
const UNSCOPED: GateSupport = GateSupport {
    scope_gates_apply: false,
    scope_gates_writable: false,
};

fn days(days: i64) -> WindowSpec {
    WindowSpec::MinAge(SignedDuration::from_hours(days * 24))
}

/// A policy with the default `window`, the `exempt` selectors, and the `admitted` versions.
fn policy(
    window: WindowSpec,
    exempt: &[&str],
    admitted: Option<&[(&str, &str)]>,
) -> ResolvedPolicy {
    ResolvedPolicy {
        default_window: Some(window),
        exempt_packages: exempt.iter().map(ToString::to_string).collect(),
        withheld_exemptions: Vec::new(),
        package_windows: std::collections::BTreeMap::new(),
        admitted_versions: admitted.map(|admitted| {
            admitted
                .iter()
                .map(|(name, version)| AdmittedVersion {
                    name: (*name).to_string(),
                    version: Version::new(*version),
                })
                .collect()
        }),
    }
}

fn sync(
    scratch: &Scratch,
    policy: &ResolvedPolicy,
    support: GateSupport,
    dry_run: bool,
) -> cooldown_core::Result<SyncReport> {
    write_supported(
        &scratch.root,
        policy,
        &scratch.environment(),
        support,
        dry_run,
    )
}

/// The warnings of a written or unchanged sync, and whether it wrote.
fn outcome(report: &SyncReport) -> eyre::Result<(bool, &[Diagnostic])> {
    match report {
        SyncReport::Written { warnings, .. } => Ok((true, warnings)),
        SyncReport::Unchanged { warnings, .. } => Ok((false, warnings)),
        other => eyre::bail!("expected a written or unchanged sync, got {other:?}"),
    }
}

fn messages(warnings: &[Diagnostic], kind: DiagnosticKind) -> Vec<&str> {
    warnings
        .iter()
        .filter(|warning| warning.kind == kind)
        .map(|warning| warning.message.as_str())
        .collect()
}

/// The gate is a whole number of minutes rounded up, so it never undercuts the window, and only
/// a positive rolling age has one.
#[test]
fn the_gate_is_the_window_in_minutes_rounded_up() {
    assert_eq!(written_minutes(&days(7)), Some(10080));
    assert_eq!(
        written_minutes(&WindowSpec::MinAge(SignedDuration::from_secs(90))),
        Some(2)
    );
    assert_eq!(
        written_minutes(&WindowSpec::MinAge(SignedDuration::from_secs(59))),
        Some(1)
    );
    assert_eq!(
        written_minutes(&WindowSpec::MinAge(SignedDuration::from_secs(60))),
        Some(1)
    );
    assert_eq!(
        written_minutes(&WindowSpec::MinAge(SignedDuration::from_millis(60_500))),
        Some(2)
    );
    assert_eq!(
        written_minutes(&WindowSpec::MinAge(SignedDuration::ZERO)),
        None
    );
    assert_eq!(written_minutes(&WindowSpec::Latest), None);
    assert_eq!(
        written_minutes(&WindowSpec::Freeze(jiff::Timestamp::UNIX_EPOCH)),
        None
    );
}

/// The gate and the whole effective list are written beside the file's other settings and
/// comments: selectors Yarn reads as cooldown does, then the admitted versions no selector
/// covers, each selector Yarn's gate still governs in part reported, and the file left alone by
/// a dry run and by a second sync.
#[test]
fn the_gate_and_its_entries_are_written_beside_the_rest_of_the_file() -> eyre::Result<()> {
    let scratch = Scratch::new()?;
    let before = indoc! {r#"
        # Yarn settings for the project
        enableTelemetry: false
        npmPreapprovedPackages:
          - "left-over"
    "#};
    scratch.write_rc(before)?;
    let mut policy = policy(
        days(7),
        &["@corp/*", "*-plugin", "@types/{node,react}", "has-flag"],
        Some(&[
            ("is-number", "7.0.0"),
            ("@corp/ui", "1.0.0"),
            ("@x/y", "2.0.0-rc.1"),
        ]),
    );
    policy.withheld_exemptions = vec!["eslint-*".to_string()];

    // A dry run reports the write and leaves the file as it is.
    let dry = sync(&scratch, &policy, CURRENT, true)?;
    assert!(outcome(&dry)?.0);
    assert_eq!(scratch.read_rc()?, before);

    let written = sync(&scratch, &policy, CURRENT, false)?;
    let (wrote, warnings) = outcome(&written)?;
    assert!(wrote);
    assert_eq!(
        scratch.read_rc()?,
        indoc! {r#"
            # Yarn settings for the project
            enableTelemetry: false
            npmPreapprovedPackages:
              onConflict: reset
              value:
                - "*-plugin"
                - "@corp/*"
                - "has-flag"
                - "@x/y@2.0.0-rc.1"
                - "is-number@7.0.0"
            npmMinimalAgeGate: 10080
        "#}
    );
    // The withheld, the omitted, and the narrowed selector are each reported, nothing else.
    let stricter = messages(warnings, DiagnosticKind::StricterNative);
    assert_eq!(stricter.len(), 3, "{warnings:?}");
    assert!(
        stricter
            .iter()
            .any(|message| message.contains("`eslint-*`"))
    );
    assert!(
        stricter
            .iter()
            .any(|message| message.contains("`@types/{node,react}`"))
    );
    assert!(
        stricter
            .iter()
            .any(|message| message.contains("`*-plugin`"))
    );
    assert_eq!(warnings.len(), 3, "{warnings:?}");

    // A second sync finds the file in sync.
    let again = sync(&scratch, &policy, CURRENT, false)?;
    assert!(!outcome(&again)?.0);
    Ok(())
}

/// Without an evaluation, the existing exact entries are kept verbatim beside the written
/// selectors, while every other entry goes; a successful evaluation replaces them instead.
#[test]
fn exact_entries_survive_an_unevaluated_sync_only() -> eyre::Result<()> {
    let scratch = Scratch::new()?;
    scratch.write_rc(indoc! {r#"
        npmPreapprovedPackages: ["is-number@7.0.0", "lodash", "a@^1", "@x/y@1.0.0 || 2.0.0"]
    "#})?;

    sync(
        &scratch,
        &policy(days(1), &["has-flag"], None),
        CURRENT,
        false,
    )?;
    assert_eq!(
        scratch.read_rc()?,
        indoc! {r#"
            npmPreapprovedPackages:
              onConflict: reset
              value:
                - "has-flag"
                - "is-number@7.0.0"
                - "@x/y@1.0.0 || 2.0.0"
            npmMinimalAgeGate: 1440
        "#}
    );

    sync(&scratch, &policy(days(1), &[], Some(&[])), CURRENT, false)?;
    assert_eq!(
        scratch.read_rc()?,
        indoc! {"
            npmPreapprovedPackages:
              onConflict: reset
              value: []
            npmMinimalAgeGate: 1440
        "}
    );
    Ok(())
}

/// A sync that cannot write the gate leaves it alone, but still drops every entry without a
/// range it would not write, keeps every ranged entry, and rewrites the list in reset form so
/// no inherited entry exempts more; a freeze is reported as inexpressible.
#[test]
fn a_window_yarn_cannot_express_prunes_the_list() -> eyre::Result<()> {
    let scratch = Scratch::new()?;
    scratch.write_rc(indoc! {r#"
        npmMinimalAgeGate: 10080
        npmPreapprovedPackages:
          - "lodash"
          - "@corp/*"
          - "a@^1"
          - "b@1.0.0"
          - "${PKG}@1.0.0"
    "#})?;
    let freeze = WindowSpec::Freeze("2026-01-01T00:00:00Z".parse()?);
    let report = sync(
        &scratch,
        &policy(freeze, &["@corp/*"], None),
        CURRENT,
        false,
    )?;
    let (wrote, warnings) = outcome(&report)?;
    assert!(wrote);
    assert_eq!(
        scratch.read_rc()?,
        indoc! {r#"
            npmMinimalAgeGate: 10080
            npmPreapprovedPackages:
              onConflict: reset
              value:
                - "@corp/*"
                - "a@^1"
                - "b@1.0.0"
        "#}
    );
    let config = messages(warnings, DiagnosticKind::Config);
    assert_eq!(config.len(), 1, "{warnings:?}");
    assert!(config.iter().all(|message| message.contains("freeze")));

    // Pruning a list in sync changes nothing.
    let again = sync(
        &scratch,
        &policy(WindowSpec::Latest, &["@corp/*"], None),
        CURRENT,
        false,
    )?;
    assert!(!outcome(&again)?.0);
    Ok(())
}

/// Without a list in the project, an unwritable window adds an empty reset list only when a
/// selector was withheld or left out, so inherited entries cannot exempt it, and otherwise
/// writes nothing, not even a missing file.
#[test]
fn an_unwritable_window_masks_inherited_entries_only_for_a_gated_selector() -> eyre::Result<()> {
    let scratch = Scratch::new()?;
    let report = sync(
        &scratch,
        &policy(WindowSpec::Latest, &["has-flag"], None),
        CURRENT,
        false,
    )?;
    assert!(!outcome(&report)?.0);
    assert!(!scratch.rc().exists());

    let report = sync(
        &scratch,
        &policy(WindowSpec::Latest, &["[a-c]*"], None),
        CURRENT,
        false,
    )?;
    assert!(outcome(&report)?.0);
    assert_eq!(
        scratch.read_rc()?,
        "npmPreapprovedPackages:\n  onConflict: reset\n  value: []\n"
    );
    Ok(())
}

/// A file sync cannot edit without changing what Yarn reads is refused, and left as it is: a
/// wrapped document, a key set twice, no mapping, an anchor another setting reuses, and a flow
/// list whose closing line is not indented.
#[test]
fn a_file_sync_cannot_edit_faithfully_is_refused() -> eyre::Result<()> {
    let scratch = Scratch::new()?;
    for content in [
        "onConflict: reset\nvalue:\n  enableTelemetry: false\n",
        "onConflict: true\nenableTelemetry: false\n",
        "npmMinimalAgeGate: 60\nnpmMinimalAgeGate: 120\n",
        "- a list\n",
        "npmPreapprovedPackages: &shared\n  - \"a\"\nunsafeHttpWhitelist: *shared\n",
        "npmPreapprovedPackages: [\n  \"a\"\n]\n",
        "npmMinimalAgeGate: [\n",
    ] {
        scratch.write_rc(content)?;
        let result = sync(&scratch, &policy(days(7), &[], Some(&[])), CURRENT, false);
        assert!(
            matches!(result, Err(CoreError::Parse(_))),
            "{content}: {result:?}"
        );
        assert_eq!(scratch.read_rc()?, content);
    }
    Ok(())
}

/// The gate of each scope the project's own file configures in block style is set in place,
/// leaving its routing alone, while every other configured scope whose gate differs is
/// reported: Yarn's one-day default inherited from the home file or behind a flow entry is
/// looser, and an inherited longer gate is stricter.
#[test]
fn scope_gates_are_set_in_the_project_file_and_reported_elsewhere() -> eyre::Result<()> {
    let scratch = Scratch::new()?;
    scratch.write_rc(indoc! {r#"
        npmScopes:
          corp:
            npmRegistryServer: "https://npm.corp.example"
          flowy: {npmAlwaysAuth: true}
    "#})?;
    std::fs::write(
        scratch.home.join(".yarnrc.yml"),
        indoc! {r#"
            npmScopes:
              home:
                npmAuthToken: "${HOME_TOKEN}"
              strict:
                npmMinimalAgeGate: 30d
              same:
                npmMinimalAgeGate: 7d
        "#},
    )?;

    let report = sync(&scratch, &policy(days(7), &[], Some(&[])), CURRENT, false)?;
    let (wrote, warnings) = outcome(&report)?;
    assert!(wrote);
    assert!(
        scratch.read_rc()?.starts_with(indoc! {r#"
            npmScopes:
              corp:
                npmMinimalAgeGate: 10080
                npmRegistryServer: "https://npm.corp.example"
              flowy: {npmAlwaysAuth: true}
        "#}),
        "{}",
        scratch.read_rc()?
    );
    let looser = messages(warnings, DiagnosticKind::Config);
    assert_eq!(looser.len(), 2, "{warnings:?}");
    assert!(
        looser
            .iter()
            .any(|message| message.contains("`npmScopes.flowy` in .yarnrc.yml"))
    );
    assert!(
        looser
            .iter()
            .any(|message| message.contains("`npmScopes.home`")
                && message.contains("one-day default"))
    );
    let stricter = messages(warnings, DiagnosticKind::StricterNative);
    assert_eq!(stricter.len(), 1, "{warnings:?}");
    assert!(
        stricter
            .iter()
            .all(|message| message.contains("`npmScopes.strict`"))
    );
    Ok(())
}

/// An empty scope entry, which Yarn reads as the scope's defaults hiding its entry in the home
/// file, is never filled with a gate, which would bring back the inherited registry and token;
/// its one-day default is reported instead.
#[test]
fn an_empty_scope_entry_is_never_filled() -> eyre::Result<()> {
    let scratch = Scratch::new()?;
    scratch.write_rc("npmScopes:\n  corp:\n")?;
    std::fs::write(
        scratch.home.join(".yarnrc.yml"),
        "npmScopes:\n  corp:\n    npmRegistryServer: \"https://npm.corp.example\"\n",
    )?;
    let report = sync(&scratch, &policy(days(7), &[], Some(&[])), CURRENT, false)?;
    let (_, warnings) = outcome(&report)?;
    assert!(
        scratch
            .read_rc()?
            .starts_with("npmScopes:\n  corp:\nnpmMinimalAgeGate: 10080\n")
    );
    assert!(
        messages(warnings, DiagnosticKind::Config)
            .iter()
            .any(
                |message| message.contains("`npmScopes.corp` in .yarnrc.yml")
                    && message.contains("one-day default")
            ),
        "{warnings:?}"
    );
    Ok(())
}

/// A scope's gate is written only when every Yarn reading the file knows it, and is not even
/// looked at by a Yarn that gates every scope by the global gate.
#[test]
fn scope_gates_follow_what_the_yarn_reading_the_file_knows() -> eyre::Result<()> {
    let scratch = Scratch::new()?;
    let scopes = "npmScopes:\n  corp:\n    npmAlwaysAuth: true\n";
    scratch.write_rc(scopes)?;

    // A Yarn older than 4.17 would refuse the entry's gate, so the looser default is reported.
    let mixed = GateSupport {
        scope_gates_apply: true,
        scope_gates_writable: false,
    };
    let report = sync(&scratch, &policy(days(7), &[], Some(&[])), mixed, true)?;
    let (_, warnings) = outcome(&report)?;
    assert!(
        messages(warnings, DiagnosticKind::Config)
            .iter()
            .any(|message| message.contains("`npmScopes.corp`") && message.contains("4.17.0")),
        "{warnings:?}"
    );

    sync(&scratch, &policy(days(7), &[], Some(&[])), UNSCOPED, false)?;
    let written = scratch.read_rc()?;
    assert!(written.starts_with(scopes), "{written}");
    assert!(!written.contains("    npmMinimalAgeGate"), "{written}");
    Ok(())
}

/// An environment variable that overrides a written setting is reported, the file still written.
#[test]
fn environment_overrides_are_reported() -> eyre::Result<()> {
    let scratch = Scratch::new()?;
    let environment = SyncEnvironment {
        minimal_age_gate: Some("0".to_string()),
        preapproved_packages: Some("*".to_string()),
        ..scratch.environment()
    };
    let report = write_supported(
        &scratch.root,
        &policy(days(7), &[], Some(&[])),
        &environment,
        CURRENT,
        false,
    )?;
    let (wrote, warnings) = outcome(&report)?;
    assert!(wrote);
    let config = messages(warnings, DiagnosticKind::Config);
    assert!(
        config
            .iter()
            .any(|message| message.starts_with("YARN_NPM_MINIMAL_AGE_GATE"))
    );
    assert!(
        config
            .iter()
            .any(|message| message.starts_with("YARN_NPM_PREAPPROVED_PACKAGES"))
    );
    Ok(())
}

/// `YARN_RC_FILENAME` names the file sync writes.
#[test]
fn the_rc_file_yarn_reads_is_the_one_written() -> eyre::Result<()> {
    let scratch = Scratch::new()?;
    let mut environment = scratch.environment();
    environment.yarn.rc_filename = Some(".custom.yml".to_string());
    write_supported(
        &scratch.root,
        &policy(days(7), &[], Some(&[])),
        &environment,
        CURRENT,
        false,
    )?;
    assert!(!scratch.rc().exists());
    assert!(
        std::fs::read_to_string(scratch.root.join(".custom.yml"))?
            .contains("npmMinimalAgeGate: 10080")
    );
    Ok(())
}

/// The adapter routes sync by the project's lock: a Classic project has no native gate and needs
/// no admission evaluation, while a Berry one has both.
#[cfg(unix)]
mod routing {
    use super::{Scratch, days, policy};
    use crate::nodecmd::NodeCmd;
    use crate::yarn::YarnTool;
    use camino::{Utf8Path, Utf8PathBuf};
    use color_eyre::eyre;
    use cooldown_core::{CoreError, Project, SyncReport, ToolWrite as _};
    use cooldown_registry::{HttpOptions, SharedHttp};

    /// A minimal Berry lock, enough for its format to be read.
    const BERRY_LOCK: &str = "__metadata:\n  version: 8\n  cacheKey: 10c0\n";

    /// Writes an executable fake `yarn` at `root` answering `--version` with `version`, and
    /// returns its path.
    fn fake_yarn(root: &Utf8Path, version: &str) -> eyre::Result<Utf8PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;
        let script = root.join("fake-yarn.sh");
        std::fs::write(
            &script,
            indoc::formatdoc! {r#"
                #!/bin/sh
                echo "$*" >> "{log}"
                if [ "$1" = "--version" ]; then
                  echo "{version}"
                fi
                exit 0
            "#, log = root.join("yarn.log")},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        Ok(script)
    }

    fn project(
        scratch: &Scratch,
        package_manager: Option<&str>,
        lock: &str,
    ) -> eyre::Result<Project> {
        let manifest = package_manager.map_or_else(
            || "{}\n".to_string(),
            |declared| format!("{{ \"packageManager\": \"{declared}\" }}\n"),
        );
        std::fs::write(scratch.root.join("package.json"), manifest)?;
        std::fs::write(scratch.root.join("yarn.lock"), lock)?;
        Ok(Project::detected(
            scratch.root.clone(),
            <crate::lock::Yarn as crate::lock::NodeLock>::ID,
            scratch.root.join("package.json"),
        ))
    }

    fn tool(cache: &tempfile::TempDir, yarn: &Utf8Path) -> eyre::Result<YarnTool> {
        Ok(
            YarnTool::from_http(SharedHttp::new(cache.path(), HttpOptions::default())?)
                .with_driver(yarn.as_str()),
        )
    }

    /// A Classic project reports no native gate without running Yarn, and a Berry one needs
    /// the admission evaluation a Classic one, or a Berry one declaring a Yarn without the gate,
    /// is spared.
    #[tokio::test]
    async fn a_classic_project_has_no_native_gate() -> eyre::Result<()> {
        let scratch = Scratch::new()?;
        let cache = tempfile::tempdir()?;
        let yarn = fake_yarn(&scratch.home, "1.22.22")?;
        let classic = project(&scratch, None, "# yarn lockfile v1\n")?;
        let tool = tool(&cache, &yarn)?;
        assert!(!tool.requires_sync_admission_evaluation(&classic));
        let report = tool
            .write_native(&classic, &policy(days(7), &[], Some(&[])), false)
            .await?;
        assert!(matches!(report, SyncReport::Unsupported), "{report:?}");
        assert!(!scratch.root.join(".yarnrc.yml").exists());

        let berry = project(&scratch, None, BERRY_LOCK)?;
        assert!(tool.requires_sync_admission_evaluation(&berry));

        // A declared Yarn without the gate leaves nothing to evaluate admissions for.
        let ungated = project(&scratch, Some("yarn@3.8.7"), BERRY_LOCK)?;
        assert!(!tool.requires_sync_admission_evaluation(&ungated));
        Ok(())
    }

    /// The Yarn the project declares and the one that runs here decide: one predating the gate
    /// leaves nothing to sync, unless the declared Yarn has the gate and only this machine's
    /// predates it, which is refused; nothing is written either way.
    #[tokio::test]
    async fn a_yarn_predating_the_gate_is_never_given_one() -> eyre::Result<()> {
        let scratch = Scratch::new()?;
        let policy = policy(days(7), &[], Some(&[]));
        for (declared, executable, refused) in [
            (Some("yarn@4.9.4"), "4.18.1", false),
            (None, "4.9.4", false),
            (Some("yarn@3.8.7"), "3.8.7", false),
            (Some("yarn@4.18.1"), "4.9.4", true),
        ] {
            let berry = project(&scratch, declared, BERRY_LOCK)?;
            let yarn = fake_yarn(&scratch.home, executable)?;
            let driver = crate::yarn::yarn_driver(NodeCmd::with_bin(yarn.as_str()));
            let environment = scratch.environment();
            let result = super::super::write(&driver, &berry, &policy, &environment, false).await;
            match result {
                Err(CoreError::System(message)) if refused => {
                    assert!(message.contains("4.9.4"), "{message}");
                }
                Ok(SyncReport::Unsupported) if !refused => {}
                other => eyre::bail!("{declared:?} / {executable}: unexpected {other:?}"),
            }
            assert!(
                !scratch.root.join(".yarnrc.yml").exists(),
                "{declared:?} / {executable}"
            );
        }

        // A declared Yarn without the scope gates keeps a newer executable from writing one.
        std::fs::write(
            scratch.root.join(".yarnrc.yml"),
            "npmScopes:\n  corp:\n    npmAlwaysAuth: true\n",
        )?;
        let berry = project(&scratch, Some("yarn@4.16.0"), BERRY_LOCK)?;
        let yarn = fake_yarn(&scratch.home, "4.18.1")?;
        let driver = crate::yarn::yarn_driver(NodeCmd::with_bin(yarn.as_str()));
        let report =
            super::super::write(&driver, &berry, &policy, &scratch.environment(), false).await?;
        assert!(matches!(report, SyncReport::Written { .. }), "{report:?}");
        let written = std::fs::read_to_string(scratch.root.join(".yarnrc.yml"))?;
        assert!(written.contains("npmMinimalAgeGate: 10080"), "{written}");
        assert!(!written.contains("    npmMinimalAgeGate"), "{written}");
        Ok(())
    }
}
