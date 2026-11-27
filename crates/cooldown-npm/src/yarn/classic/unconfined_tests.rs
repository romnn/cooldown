//! Where a Classic landing declines every candidate, nothing runs Yarn, tested on every platform
//! Yarn can be faked on.

use super::confine;

/// Off Linux there is no Landlock, so Yarn's writes are never confined.
#[cfg(not(target_os = "linux"))]
#[test]
fn yarn_is_never_confined_off_linux() {
    assert!(confine::available().is_err());
}

/// On this kernel, which the tests require to offer Landlock ABI 3, Yarn's writes are confined.
#[cfg(target_os = "linux")]
#[test]
fn yarn_is_confined_on_linux() {
    assert_eq!(confine::available(), Ok(()));
}

#[cfg(unix)]
mod declined {
    use crate::lock::{NodeLock as _, Yarn};
    use crate::manifest::workspaces::{YarnEnvironment, YarnVariable};
    use crate::registry::NPM;
    use crate::yarn::YarnTool;
    use camino::{Utf8Path, Utf8PathBuf};
    use color_eyre::eyre;
    use cooldown_core::{
        Change, MemberRef, PackageId, Plan, PreparedMutation, Project, SkipReason, ToolRead as _,
        ToolWrite as _, UpdateKind, Version,
    };
    use cooldown_registry::{HttpOptions, SharedHttp};
    use indoc::indoc;

    const MANIFEST: &str = indoc! {r#"
        { "name": "classic-declined", "version": "1.0.0", "dependencies": { "chalk": "^4.0.0" } }
    "#};

    const LOCK: &str = indoc! {r#"
        # yarn lockfile v1


        chalk@^4.0.0:
          version "4.0.0"
          resolved "https://registry.yarnpkg.com/chalk/-/chalk-4.0.0.tgz"
    "#};

    /// A Classic project, its home directory, and a fake `yarn` that logs every invocation, a
    /// version probe included, into Yarn's default cache folder, where even a confined Yarn may
    /// write.
    struct World {
        /// Owns every directory of the world; dropping it deletes them.
        _guard: tempfile::TempDir,
        project: Project,
        log: Utf8PathBuf,
        /// Owns the adapter's HTTP cache.
        _cache: tempfile::TempDir,
        tool: YarnTool,
    }

    fn new_world(
        confinement: std::result::Result<(), String>,
        variables: Vec<YarnVariable>,
    ) -> eyre::Result<World> {
        use std::os::unix::fs::PermissionsExt as _;
        let guard = tempfile::tempdir()?;
        let top = Utf8PathBuf::from_path_buf(dunce::canonicalize(guard.path())?)
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let root = top.join("project");
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("package.json"), MANIFEST)?;
        std::fs::write(root.join("yarn.lock"), LOCK)?;
        let home = top.join("home");
        let log = home.join(".cache").join("yarn").join("yarn.log");
        std::fs::create_dir_all(log.parent().unwrap_or(&home))?;
        let script = top.join("fake-yarn.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"$*\" >> \"{log}\"\necho 1.22.22\n"),
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let cache = tempfile::tempdir()?;
        let environment = YarnEnvironment {
            home: Some(home),
            variables,
            ..YarnEnvironment::default()
        };
        let tool = YarnTool::from_http(SharedHttp::new(cache.path(), HttpOptions::default())?)
            .with_driver(script.as_str())
            .with_verification(environment, vec![top.join("tmp")])
            .with_confinement(confinement);
        Ok(World {
            _guard: guard,
            project: Project::detected(root.clone(), Yarn::ID, root.join("package.json")),
            log,
            _cache: cache,
            tool,
        })
    }

    /// Every invocation the fake logged.
    fn invocations(log: &Utf8Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn chalk_plan() -> Plan {
        Plan {
            changes: vec![Change {
                package: PackageId::new(Yarn::ID, "chalk", Some(NPM.to_string())),
                from: Version::new("4.0.0"),
                to: Version::new("4.1.0"),
                kind: UpdateKind::Minor,
                downgrade: false,
                direct: true,
                members: vec![MemberRef {
                    name: "classic-declined".to_string(),
                    path: ".".to_string(),
                }],
            }],
            ..Plan::default()
        }
    }

    /// The apply entry point the executor uses holds every candidate as not eligible, saying
    /// why, before any Yarn runs, not even a version probe in the project, where the landing
    /// declines: Yarn's writes cannot be confined, or `YARN_CONFIG` names an rc file.
    #[tokio::test]
    async fn a_declined_landing_runs_no_yarn() -> eyre::Result<()> {
        /// An observer that ignores every notification.
        struct Silent;
        impl cooldown_core::ApplyObserver for Silent {}

        let yarn_config = YarnVariable {
            name: "yarn_config".to_string(),
            value: "private.yarnrc".to_string(),
        };
        for (confinement, variables, reason) in [
            (
                Err("no Landlock here".to_string()),
                Vec::new(),
                "no Landlock here",
            ),
            (Ok(()), vec![yarn_config], "`YARN_CONFIG`"),
        ] {
            let world = new_world(confinement, variables)?;

            let mutation =
                PreparedMutation::prepare(&world.tool, &world.project, &chalk_plan()).await?;
            let attempt = world.tool.apply_with_observer(&mutation, &Silent).await?;
            let report = match attempt.into_outcome_for(&mutation)? {
                cooldown_core::ApplyAttemptOutcome::Finished { report, .. } => report?,
                other @ cooldown_core::ApplyAttemptOutcome::PendingRecovery { .. } => {
                    eyre::bail!("expected a finished attempt, got {other:?}")
                }
            };

            let [held] = report.skipped.as_slice() else {
                eyre::bail!("expected one held row: {report:?}");
            };
            assert_eq!(held.reason, SkipReason::NotEligible);
            let detail = held.detail.as_deref().unwrap_or_default();
            assert!(detail.contains(reason), "{detail}");
            assert_eq!(invocations(&world.log), Vec::<String>::new());
            assert_eq!(
                std::fs::read_to_string(world.project.root.join("yarn.lock"))?,
                LOCK
            );
        }
        Ok(())
    }

    /// A preview of a project whose landing declines names nothing to copy, and fails nothing,
    /// even where no copy could be made: here no home directory is established.
    #[test]
    fn a_declined_preview_names_no_roots() -> eyre::Result<()> {
        let world = new_world(Err("no Landlock here".to_string()), Vec::new())?;
        let tool = world
            .tool
            .with_verification(YarnEnvironment::default(), Vec::new());

        let roots = tool.external_resolve_roots(&world.project)?;

        assert_eq!(roots, Vec::<Utf8PathBuf>::new());
        Ok(())
    }

    /// The trace asks no Yarn for its version, which in the project would run whatever launcher
    /// its `yarnPath` names, unconfined.
    #[test]
    fn the_trace_asks_no_yarn_its_version() -> eyre::Result<()> {
        let world = new_world(Ok(()), Vec::new())?;

        assert!(world.tool.version_command().is_none());
        Ok(())
    }
}
