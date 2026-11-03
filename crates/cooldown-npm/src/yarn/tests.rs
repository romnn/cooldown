//! The `yarn` adapter's routing: each project reaches the backend its own lock's format names, a
//! Berry project is read but never mutated, and a Classic lock is never handed to a Berry `yarn`.

use super::YarnTool;
use crate::lock::yarn_berry_fixtures::{
    BerryProject, EVERY_SOURCE, ProjectFile, RESOLUTIONS, SCHEMA_FILES, SCHEMAS,
};
use crate::lock::yarn_fixtures::{self, YarnWorkspace};
use crate::lock::{NodeLock, Yarn};
use camino::Utf8PathBuf;
use color_eyre::eyre;
use cooldown_core::{DepScope, Dependency, DiagnosticKind, Project, ToolRead};
use cooldown_registry::{HttpOptions, SharedHttp};
use std::collections::BTreeSet;

/// A project written into a temporary directory.
struct TempProject {
    /// Owns the temporary root directory; dropping it deletes the project on disk, so it must
    /// stay bound for as long as the project is used.
    _guard: tempfile::TempDir,
    project: Project,
}

/// A Yarn project of `files` and the lock `lock`.
fn yarn_project<'a>(
    files: impl IntoIterator<Item = (&'a str, &'a str)>,
    lock: &'a str,
) -> eyre::Result<TempProject> {
    let guard = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(guard.path().to_path_buf())
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    for (rel, contents) in files.into_iter().chain([("yarn.lock", lock)]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    let project = Project::detected(root.clone(), Yarn::ID, root.join("package.json"));
    Ok(TempProject {
        _guard: guard,
        project,
    })
}

/// The project a Berry fixture records.
fn berry_project(fixture: &BerryProject) -> eyre::Result<TempProject> {
    yarn_project(berry_files(fixture.files), fixture.lock)
}

/// The project a Classic fixture records.
fn classic_project(fixture: &YarnWorkspace) -> eyre::Result<TempProject> {
    yarn_project(fixture.manifests.iter().copied(), fixture.lock)
}

fn berry_files(files: &[ProjectFile]) -> impl Iterator<Item = (&str, &str)> {
    files
        .iter()
        .map(|ProjectFile { path, contents }| (*path, *contents))
}

/// The adapter over a throwaway HTTP cache, which these tests never fill.
fn tool() -> eyre::Result<(tempfile::TempDir, YarnTool)> {
    let cache = tempfile::tempdir()?;
    let tool = YarnTool::from_http(SharedHttp::new(cache.path(), HttpOptions::default())?);
    Ok((cache, tool))
}

/// Every row's `name@version`, sorted.
fn rows(deps: &[Dependency]) -> BTreeSet<String> {
    deps.iter()
        .map(|dep| format!("{}@{}", dep.package.name, dep.current))
        .collect()
}

/// A Berry project reaches the Berry reader: its rows are the lock's npm releases, without any
/// member, since Berry attribution is not supported yet.
#[tokio::test]
async fn a_berry_project_reads_its_registry_releases() -> eyre::Result<()> {
    let TempProject { _guard, project } = berry_project(&EVERY_SOURCE)?;
    let (_cache, tool) = tool()?;

    let graph = tool.dependencies(&project, DepScope::Graph).await?;

    assert_eq!(
        rows(&graph),
        BTreeSet::from(
            [
                "@esbuild/darwin-arm64@0.25.0",
                "@sindresorhus/is@4.6.0",
                "ansi-styles@4.3.0",
                "chalk@4.1.0",
                "chalk@4.1.2",
                "color-convert@2.0.1",
                "color-name@1.1.4",
                "es-errors@1.3.0",
                "function-bind@1.1.2",
                "has-flag@4.0.0",
                "hasown@2.0.4",
                "is-core-module@2.17.0",
                "is-number@6.0.0",
                "is-number@7.0.0",
                "is-odd@3.0.1",
                "path-parse@1.0.7",
                "resolve@1.22.13",
                "supports-color@7.2.0",
                "supports-preserve-symlinks-flag@1.0.0",
            ]
            .map(str::to_string)
        )
    );
    // No row names a member or carries advisory identity.
    assert!(
        graph
            .iter()
            .all(|dep| dep.members.is_empty() && dep.advisory_identity.is_none()),
        "{graph:?}"
    );
    Ok(())
}

/// A release only an npm alias resolves to is never claimed by the root's declaration of the
/// alias target's name, so it reads as transitive, while the root's own `is-number` copy is
/// direct.
#[tokio::test]
async fn an_alias_only_release_is_no_direct_dependency() -> eyre::Result<()> {
    let Some(schema) = SCHEMAS.last() else {
        eyre::bail!("no schema fixture");
    };
    let TempProject { _guard, project } = yarn_project(berry_files(SCHEMA_FILES), schema.lock)?;
    let (_cache, tool) = tool()?;

    let direct = tool.dependencies(&project, DepScope::Direct).await?;

    let direct = rows(&direct);
    assert!(direct.contains("is-number@7.0.0"), "{direct:?}");
    assert!(!direct.contains("is-number@6.0.0"), "{direct:?}");
    Ok(())
}

/// A Berry workspace warns that its members are not attributed, while a single-package Berry
/// project, whose only workspace is the root, does not.
#[tokio::test]
async fn a_berry_workspace_warns_that_its_members_are_unattributed() -> eyre::Result<()> {
    let (_cache, tool) = tool()?;

    let TempProject { _guard, project } = berry_project(&EVERY_SOURCE)?;
    let notices = tool.manifest_notices(&project).await?;
    let [notice] = notices.as_slice() else {
        eyre::bail!("one notice: {notices:?}");
    };
    assert_eq!(notice.kind, DiagnosticKind::Config);
    assert!(
        notice.message.contains("does not attribute yet"),
        "{notice:?}"
    );

    let TempProject { _guard, project } = berry_project(&RESOLUTIONS)?;
    assert_eq!(tool.manifest_notices(&project).await?, []);
    Ok(())
}

/// A lock whose metadata is null is an unreadable Berry lock, never an empty Classic graph.
#[tokio::test]
async fn a_null_metadata_lock_is_unreadable_not_an_empty_graph() -> eyre::Result<()> {
    let TempProject { _guard, project } = yarn_project(
        yarn_fixtures::PINNED_MEMBERS.manifests.iter().copied(),
        "__metadata: null\n",
    )?;
    let (_cache, tool) = tool()?;

    let graph = tool.dependencies(&project, DepScope::Graph).await;

    assert!(
        matches!(graph, Err(cooldown_core::CoreError::LockUnreadable(_))),
        "{graph:?}"
    );
    Ok(())
}

/// A Classic project still reaches the Classic reader, which attributes each member its own copy
/// through the workspace manifests.
#[tokio::test]
async fn a_classic_project_keeps_the_classic_reader() -> eyre::Result<()> {
    let TempProject { _guard, project } = classic_project(&yarn_fixtures::PINNED_MEMBERS)?;
    let (_cache, tool) = tool()?;

    let graph = tool.dependencies(&project, DepScope::Graph).await?;

    let members = graph
        .iter()
        .find(|dep| dep.package.name == "chalk" && dep.current.as_str() == "4.1.0")
        .map(|dep| {
            dep.members
                .iter()
                .map(|member| member.path.clone())
                .collect::<Vec<_>>()
        });
    assert_eq!(members, Some(vec!["packages/a".to_string()]));
    assert_eq!(tool.manifest_notices(&project).await?, []);
    Ok(())
}

/// Mutation through scripted fake `yarn` binaries, which need a shell to run.
#[cfg(unix)]
mod mutation {
    use super::{TempProject, berry_project, classic_project, tool, yarn_project};
    use crate::lock::yarn_berry_fixtures::EVERY_SOURCE;
    use crate::lock::yarn_fixtures;
    use crate::lock::{NodeLock, Yarn};
    use crate::registry::NPM;
    use camino::{Utf8Path, Utf8PathBuf};
    use color_eyre::eyre;
    use cooldown_core::{
        Change, CoreError, MemberRef, PackageId, Plan, PreparedMutation, ToolWrite, Version,
    };

    /// A plan moving one `chalk` copy, whatever the backend does with it.
    fn chalk_plan() -> Plan {
        Plan {
            changes: vec![Change {
                package: PackageId::new(Yarn::ID, "chalk", Some(NPM.to_string())),
                from: Version::new("4.1.0"),
                to: Version::new("4.1.2"),
                kind: cooldown_core::UpdateKind::Patch,
                downgrade: false,
                direct: true,
                members: vec![MemberRef {
                    name: "pkg-b".to_string(),
                    path: "packages/b".to_string(),
                }],
            }],
            ..Plan::default()
        }
    }

    /// Writes an executable fake `yarn` at `root` that logs every invocation to `yarn.log` and
    /// answers `--version` with `version`, and returns its path.
    fn fake_yarn(root: &Utf8Path, version: &str) -> eyre::Result<Utf8PathBuf> {
        fake_yarn_running(root, version, ":")
    }

    /// [`fake_yarn`] that also runs the shell command `on_version` before answering `--version`.
    ///
    /// Like Corepack 0.31 and older, it pins itself into the root manifest whenever
    /// `COREPACK_ENABLE_AUTO_PIN` is not `0`, so a run that leaves the auto-pin on rewrites
    /// `package.json`.
    fn fake_yarn_running(
        root: &Utf8Path,
        version: &str,
        on_version: &str,
    ) -> eyre::Result<Utf8PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;
        let script = root.join("fake-yarn.sh");
        std::fs::write(
            &script,
            indoc::formatdoc! {r#"
                #!/bin/sh
                echo "$*" >> "{log}"
                if [ "$COREPACK_ENABLE_AUTO_PIN" != "0" ]; then
                  echo '{{ "packageManager": "yarn@{version}" }}' > "{manifest}"
                fi
                if [ "$1" = "--version" ]; then
                  {on_version}
                  echo "{version}"
                fi
                exit 0
            "#, log = root.join("yarn.log"), manifest = root.join("package.json")},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        Ok(script)
    }

    /// The invocations a [`fake_yarn`] logged, empty when it never ran.
    fn yarn_invocations(root: &Utf8Path) -> Vec<String> {
        std::fs::read_to_string(root.join("yarn.log"))
            .map(|log| log.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// The message of a [`CoreError::System`] refusal, or a failure naming what was returned instead.
    fn system_message<T: std::fmt::Debug>(
        result: cooldown_core::Result<T>,
    ) -> eyre::Result<String> {
        match result {
            Err(CoreError::System(message)) => Ok(message),
            other => eyre::bail!("expected a system refusal, got {other:?}"),
        }
    }

    /// A Berry project is never mutated: `apply` and `--build` refuse it before running Yarn at all,
    /// so its lock stays byte for byte, and the refusal is an environment failure that candidate
    /// isolation propagates instead of blaming each candidate.
    #[tokio::test]
    async fn berry_mutation_is_refused_before_any_resolver_runs() -> eyre::Result<()> {
        let TempProject { _guard, project } = berry_project(&EVERY_SOURCE)?;
        let script = fake_yarn(&project.root, "4.18.1")?;
        let (_cache, tool) = tool()?;
        let tool = tool.with_driver(script.as_str());

        let mutation = PreparedMutation::prepare(&tool, &project, &chalk_plan()).await?;
        let applied = system_message(tool.apply(&mutation).await)?;
        let built = system_message(tool.build(&project).await)?;

        for message in [&applied, &built] {
            assert!(
                message.contains("Yarn Berry (v2+) lockfile")
                    && message.contains("cannot change yet"),
                "{message}"
            );
        }
        assert!(CoreError::System(applied).is_local_environment_failure());
        assert_eq!(yarn_invocations(&project.root), Vec::<String>::new());
        assert_eq!(
            std::fs::read_to_string(project.root.join("yarn.lock"))?,
            EVERY_SOURCE.lock
        );
        Ok(())
    }

    /// The observed apply reports the same refusal as a finished attempt, with the unchanged project
    /// as its postimage.
    #[tokio::test]
    async fn an_observed_berry_apply_finishes_refused() -> eyre::Result<()> {
        /// An observer that ignores every notification.
        struct Silent;
        impl cooldown_core::ApplyObserver for Silent {}

        let TempProject { _guard, project } = berry_project(&EVERY_SOURCE)?;
        let script = fake_yarn(&project.root, "4.18.1")?;
        let (_cache, tool) = tool()?;
        let tool = tool.with_driver(script.as_str());

        let mutation = PreparedMutation::prepare(&tool, &project, &chalk_plan()).await?;
        let attempt = tool.apply_with_observer(&mutation, &Silent).await?;

        match attempt.into_outcome_for(&mutation)? {
            cooldown_core::ApplyAttemptOutcome::Finished { report, .. } => {
                system_message(report)?;
            }
            other @ cooldown_core::ApplyAttemptOutcome::PendingRecovery { .. } => {
                eyre::bail!("expected a finished attempt, got {other:?}");
            }
        }
        assert_eq!(yarn_invocations(&project.root), Vec::<String>::new());
        Ok(())
    }

    /// A Classic lock whose project declares a Berry `packageManager` is refused before Yarn is even
    /// asked its version, naming the declaration.
    #[tokio::test]
    async fn a_declared_berry_package_manager_is_refused_beside_a_classic_lock() -> eyre::Result<()>
    {
        let fixture = &yarn_fixtures::PINNED_MEMBERS;
        let TempProject { _guard, project } = classic_project(fixture)?;
        let manifest = std::fs::read_to_string(&project.manifest)?.replacen(
            "\"private\": true,",
            "\"private\": true,\n  \"packageManager\": \"yarn@4.18.1\",",
            1,
        );
        std::fs::write(&project.manifest, manifest)?;
        let script = fake_yarn(&project.root, "4.18.1")?;
        let (_cache, tool) = tool()?;
        let tool = tool.with_driver(script.as_str());

        let mutation = PreparedMutation::prepare(&tool, &project, &chalk_plan()).await?;
        let message = system_message(tool.apply(&mutation).await)?;

        assert!(
            message.contains("Yarn Classic (v1) lockfile")
                && message.contains("\"packageManager\": \"yarn@4.18.1\"")
                && message.contains("does not migrate a lock"),
            "{message}"
        );
        assert_eq!(yarn_invocations(&project.root), Vec::<String>::new());
        assert_eq!(
            std::fs::read_to_string(project.root.join("yarn.lock"))?,
            fixture.lock
        );
        Ok(())
    }

    /// A Classic lock whose project declares another package manager is refused too, since `yarn`
    /// would not run there.
    #[tokio::test]
    async fn another_declared_package_manager_is_refused_beside_a_classic_lock() -> eyre::Result<()>
    {
        let TempProject { _guard, project } = classic_project(&yarn_fixtures::PINNED_MEMBERS)?;
        let manifest = std::fs::read_to_string(&project.manifest)?.replacen(
            "\"private\": true,",
            "\"private\": true,\n  \"packageManager\": \"pnpm@9.0.0\",",
            1,
        );
        std::fs::write(&project.manifest, manifest)?;
        let script = fake_yarn(&project.root, "1.22.22")?;
        let (_cache, tool) = tool()?;
        let tool = tool.with_driver(script.as_str());

        let message = system_message(tool.build(&project).await)?;

        assert!(message.contains("pnpm@9.0.0"), "{message}");
        assert_eq!(yarn_invocations(&project.root), Vec::<String>::new());
        Ok(())
    }

    /// A Classic lock the project's `yarn` would run as Yarn Berry, here through a `yarnPath`, is
    /// refused after asking only its version, naming the version and the redirect.
    #[tokio::test]
    async fn a_berry_yarn_is_refused_beside_a_classic_lock() -> eyre::Result<()> {
        let fixture = &yarn_fixtures::PINNED_MEMBERS;
        let TempProject { _guard, project } = classic_project(fixture)?;
        std::fs::write(
            project.root.join(".yarnrc.yml"),
            "yarnPath: .yarn/releases/yarn-4.18.1.cjs\n",
        )?;
        let script = fake_yarn(&project.root, "4.18.1")?;
        let (_cache, tool) = tool()?;
        let tool = tool.with_driver(script.as_str());

        let mutation = PreparedMutation::prepare(&tool, &project, &chalk_plan()).await?;
        let applied = system_message(tool.apply(&mutation).await)?;
        let built = system_message(tool.build(&project).await)?;

        for message in [&applied, &built] {
            assert!(
                message.contains("runs Yarn 4.18.1")
                    && message
                        .contains("`yarnPath: .yarn/releases/yarn-4.18.1.cjs` in .yarnrc.yml"),
                "{message}"
            );
        }
        assert_eq!(yarn_invocations(&project.root), ["--version", "--version"]);
        assert_eq!(
            std::fs::read_to_string(project.root.join("yarn.lock"))?,
            fixture.lock
        );
        Ok(())
    }

    /// A Classic lock under a Classic `yarn` runs as before: the executable check asks for the
    /// version, then the Classic backend installs.
    #[tokio::test]
    async fn a_classic_yarn_runs_a_classic_lock() -> eyre::Result<()> {
        let TempProject { _guard, project } = classic_project(&yarn_fixtures::PINNED_MEMBERS)?;
        let script = fake_yarn(&project.root, "1.22.22")?;
        let (_cache, tool) = tool()?;
        let tool = tool.with_driver(script.as_str());

        let report = tool.build(&project).await?;

        assert!(report.ok, "{report:?}");
        assert_eq!(yarn_invocations(&project.root), ["--version", "install"]);
        Ok(())
    }

    /// A lock that turns Berry while Yarn's version is probed is never handed to the Classic
    /// backend: the apply and the build stop with a lock conflict before any write, leaving the
    /// replacement in place.
    #[tokio::test]
    async fn a_lock_that_turns_berry_under_the_probe_is_a_conflict() -> eyre::Result<()> {
        let fixture = &yarn_fixtures::PINNED_MEMBERS;
        let TempProject { _guard, project } = classic_project(fixture)?;
        let lock = project.root.join("yarn.lock");
        let replacement = project.root.join("berry.lock");
        std::fs::write(&replacement, EVERY_SOURCE.lock)?;
        let script = fake_yarn_running(
            &project.root,
            "1.22.22",
            &format!("cp \"{replacement}\" \"{lock}\""),
        )?;
        let manifest = std::fs::read(&project.manifest)?;
        let (_cache, tool) = tool()?;
        let tool = tool.with_driver(script.as_str());

        let mutation = PreparedMutation::prepare(&tool, &project, &chalk_plan()).await?;
        let applied = tool.apply(&mutation).await;
        let Err(CoreError::LockConflict(message)) = applied else {
            eyre::bail!("expected a lock conflict, got {applied:?}");
        };
        assert!(
            message.contains("changed from a Yarn Classic (v1) to a Yarn Berry (v2+) lockfile"),
            "{message}"
        );
        // Only the probe ran, and the Berry lock that replaced the judged one stays.
        assert_eq!(yarn_invocations(&project.root), ["--version"]);
        assert_eq!(std::fs::read_to_string(&lock)?, EVERY_SOURCE.lock);
        assert_eq!(std::fs::read(&project.manifest)?, manifest);

        // `--build` judges the lock it reads first the same way.
        std::fs::write(&lock, fixture.lock)?;
        let built = tool.build(&project).await;
        assert!(
            matches!(built, Err(CoreError::LockConflict(_))),
            "{built:?}"
        );
        assert_eq!(yarn_invocations(&project.root), ["--version", "--version"]);
        assert_eq!(std::fs::read_to_string(&lock)?, EVERY_SOURCE.lock);
        Ok(())
    }

    /// A lock that turns Berry while the probe runs is a conflict even when the probe then refuses
    /// the Yarn it finds or fails outright: a refusal would have the caller restore the judged
    /// Classic lock over the Berry one, which a conflict never does.
    #[tokio::test]
    async fn a_lock_that_turns_berry_under_a_failing_probe_is_a_conflict() -> eyre::Result<()> {
        /// An observer that ignores every notification.
        struct Silent;
        impl cooldown_core::ApplyObserver for Silent {}

        for (version, then) in [("4.18.1", ":"), ("1.22.22", "exit 1")] {
            let fixture = &yarn_fixtures::PINNED_MEMBERS;
            let TempProject { _guard, project } = classic_project(fixture)?;
            let lock = project.root.join("yarn.lock");
            let replacement = project.root.join("berry.lock");
            std::fs::write(&replacement, EVERY_SOURCE.lock)?;
            let script = fake_yarn_running(
                &project.root,
                version,
                &format!("cp \"{replacement}\" \"{lock}\"; {then}"),
            )?;
            let (_cache, tool) = tool()?;
            let tool = tool.with_driver(script.as_str());

            let mutation = PreparedMutation::prepare(&tool, &project, &chalk_plan()).await?;
            let attempt = tool.apply_with_observer(&mutation, &Silent).await?;
            match attempt.into_outcome_for(&mutation)? {
                cooldown_core::ApplyAttemptOutcome::Finished { report, .. } => assert!(
                    matches!(report, Err(CoreError::LockConflict(_))),
                    "probe {version} then {then:?}: {report:?}"
                ),
                other @ cooldown_core::ApplyAttemptOutcome::PendingRecovery { .. } => {
                    eyre::bail!("expected a finished attempt, got {other:?}");
                }
            }
            assert_eq!(yarn_invocations(&project.root), ["--version"]);
            assert_eq!(std::fs::read_to_string(&lock)?, EVERY_SOURCE.lock);

            std::fs::write(&lock, fixture.lock)?;
            let built = tool.build(&project).await;
            assert!(
                matches!(built, Err(CoreError::LockConflict(_))),
                "probe {version} then {then:?}: {built:?}"
            );
            assert_eq!(std::fs::read_to_string(&lock)?, EVERY_SOURCE.lock);
        }
        Ok(())
    }

    /// Every Yarn run disables Corepack's auto-pin, so neither the version probe nor the install
    /// writes a `packageManager` field into a root manifest that declares none, whether the run
    /// goes on or is refused.
    #[tokio::test]
    async fn corepack_never_pins_the_root_manifest_during_a_yarn_run() -> eyre::Result<()> {
        for (version, runs) in [("1.22.22", true), ("4.18.1", false)] {
            let TempProject { _guard, project } = classic_project(&yarn_fixtures::PINNED_MEMBERS)?;
            let manifest = std::fs::read(&project.manifest)?;
            let script = fake_yarn(&project.root, version)?;
            let (_cache, tool) = tool()?;
            let tool = tool.with_driver(script.as_str());

            let built = tool.build(&project).await;

            assert_eq!(built.is_ok(), runs, "Yarn {version}: {built:?}");
            assert_eq!(
                std::fs::read(&project.manifest)?,
                manifest,
                "Yarn {version} rewrote package.json"
            );
        }
        Ok(())
    }

    /// A lock whose metadata key only the decoded YAML names, whose metadata is null, or whose
    /// inline metadata precedes malformed YAML is a Berry lock, so `--build` refuses it instead of
    /// running a Classic install over it.
    #[tokio::test]
    async fn an_unusual_metadata_spelling_is_never_built_as_classic() -> eyre::Result<()> {
        for lock in [
            "__metadata: null\n",
            "\"\\x5f_metadata\":\n  version: 10\n",
            "__metadata: { version: 10 }\nbroken: [unclosed\n",
            "__metadata: null\nbroken: [unclosed\n",
        ] {
            let TempProject { _guard, project } = yarn_project(
                yarn_fixtures::PINNED_MEMBERS.manifests.iter().copied(),
                lock,
            )?;
            let script = fake_yarn(&project.root, "1.22.22")?;
            let (_cache, tool) = tool()?;
            let tool = tool.with_driver(script.as_str());

            let message = system_message(tool.build(&project).await)?;

            assert!(
                message.contains("Yarn Berry (v2+) lockfile"),
                "{lock:?}: {message}"
            );
            assert_eq!(
                yarn_invocations(&project.root),
                Vec::<String>::new(),
                "{lock:?}"
            );
        }
        Ok(())
    }
}
