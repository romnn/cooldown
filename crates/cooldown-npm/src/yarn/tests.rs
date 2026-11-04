//! The `yarn` adapter's routing: each project reaches the backend its own lock's format names, a
//! Berry lock is never handed to a Classic `yarn` nor mutated unverified, and a Classic lock is
//! never handed to a Berry `yarn`.

use super::YarnTool;
use crate::lock::yarn_berry_fixtures::{
    BerryProject, EVERY_SOURCE, ProjectFile, RESOLUTIONS, REWRITES, SCHEMA_FILES, SCHEMAS,
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

/// A Berry project reaches the Berry reader: its rows are the lock's npm releases, each credited
/// to the members whose own declarations bind it, and each carrying its provisional advisory
/// identity.
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
    // Each member owns its own `chalk` copy, and the transitive `is-number@6.0.0` none.
    let members = |name: &str, version: &str| -> Vec<String> {
        graph
            .iter()
            .filter(|dep| dep.package.name == name && dep.current.as_str() == version)
            .flat_map(|dep| dep.members.iter().map(|member| member.path.clone()))
            .collect()
    };
    assert_eq!(members("chalk", "4.1.2"), [".", "packages/a"]);
    assert_eq!(members("chalk", "4.1.0"), ["packages/b"]);
    assert_eq!(members("is-number", "6.0.0"), [] as [String; 0]);
    // Every row is a binding-free `npm:` locator fetched from the configured registry, so each
    // carries its identity provisionally until Yarn's routing confirms it at feed time.
    assert!(
        graph
            .iter()
            .all(|dep| dep.advisory_identity.as_deref() == Some(dep.package.name.as_str())),
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

/// A Berry workspace whose every declaration the lock proves warns of nothing, while one with
/// declarations a rewrite may have moved warns once, through the Berry backend.
#[tokio::test]
async fn a_berry_workspace_warns_only_of_unproven_declarations() -> eyre::Result<()> {
    let (_cache, tool) = tool()?;

    for fixture in [&EVERY_SOURCE, &RESOLUTIONS] {
        let TempProject { _guard, project } = berry_project(fixture)?;
        assert_eq!(tool.manifest_notices(&project).await?, []);
    }

    let TempProject { _guard, project } = berry_project(&REWRITES)?;
    let notices = tool.manifest_notices(&project).await?;
    let [notice] = notices.as_slice() else {
        eyre::bail!("one notice: {notices:?}");
    };
    assert_eq!(notice.kind, DiagnosticKind::Config);
    assert!(
        notice.message.starts_with(
            "cooldown cannot prove which locked copy these workspace declarations bind: chalk"
        ),
        "{notice:?}"
    );
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

    /// A Berry mutation starts only from a lock Yarn proves current, so a project whose lock no
    /// verification copy can judge, here through an `exec:` package, is refused while the mutation
    /// is prepared, before any Yarn runs, as an environment failure candidate isolation never
    /// bisects, and its lock stays byte for byte.
    #[tokio::test]
    async fn a_berry_lock_verification_cannot_judge_is_never_mutated() -> eyre::Result<()> {
        let TempProject { _guard, project } = berry_project(&EVERY_SOURCE)?;
        let script = fake_yarn(&project.root, "4.18.1")?;
        let (_cache, tool) = tool()?;
        let tool = tool.with_driver(script.as_str());

        let prepared = PreparedMutation::prepare(&tool, &project, &chalk_plan()).await;

        let message = system_message(prepared)?;
        assert!(
            message.contains("`exec:` package")
                && message.contains("only once Yarn proves it current"),
            "{message}"
        );
        assert!(CoreError::System(message).is_local_environment_failure());
        assert_eq!(yarn_invocations(&project.root), Vec::<String>::new());
        assert_eq!(
            std::fs::read_to_string(project.root.join("yarn.lock"))?,
            EVERY_SOURCE.lock
        );
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
    /// inline metadata precedes malformed YAML is a Berry lock, so `--build` asks Yarn only its
    /// version and refuses the Classic `yarn` instead of running a Classic install over it.
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
                message.contains("Yarn Berry (v2+) lockfile")
                    && message.contains("a Yarn Classic (v1) release"),
                "{lock:?}: {message}"
            );
            assert_eq!(yarn_invocations(&project.root), ["--version"], "{lock:?}");
        }
        Ok(())
    }
}

/// Berry verification and routing through the adapter, with a scripted fake `yarn` outside the
/// project.
#[cfg(unix)]
mod verification {
    use super::{TempProject, tool, yarn_project};
    use crate::lock::yarn_berry_fixtures::{SCHEMA_FILES, SCHEMAS};
    use crate::manifest::workspaces::YarnEnvironment;
    use crate::yarn::YarnTool;
    use crate::yarn::routing::captures::{
        JSR_ENTRY, MIRROR_REGISTRY, PRIVATE_SCOPE, PUBLIC_REGISTRY,
    };
    use crate::yarn::verify::captures::{CURRENT, STALE_MEMBER};
    use camino::{Utf8Path, Utf8PathBuf};
    use color_eyre::eyre;
    use cooldown_core::{CoreError, DepScope, LockStatus, Project, ToolRead};

    /// A Berry project, a home directory, a scratch base, and a fake `yarn` whose answers live in
    /// files beside it, so a test can change them between runs.
    struct World {
        /// Owns the project's temporary directory; dropping it deletes the project.
        _project_guard: tempfile::TempDir,
        /// Owns the directory around the project: home, scratch base, and fake.
        _guard: tempfile::TempDir,
        top: Utf8PathBuf,
        project: Project,
        /// Owns the adapter's HTTP cache.
        _cache: tempfile::TempDir,
        tool: YarnTool,
    }

    fn new_world() -> eyre::Result<World> {
        use std::os::unix::fs::PermissionsExt as _;
        let lock = SCHEMAS
            .last()
            .ok_or_else(|| eyre::eyre!("the schema fixtures hold a lock"))?
            .lock;
        let files = SCHEMA_FILES.iter().map(|file| (file.path, file.contents));
        let TempProject {
            _guard: project_guard,
            project,
        } = yarn_project(files, lock)?;
        let guard = tempfile::tempdir()?;
        let top = Utf8PathBuf::from_path_buf(dunce::canonicalize(guard.path())?)
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let home = top.join("home");
        std::fs::create_dir_all(&home)?;
        let script = top.join("fake-yarn.sh");
        std::fs::write(
            &script,
            indoc::formatdoc! {r#"
                #!/bin/sh
                echo "$*" >> "{top}/yarn.log"
                case "$1 $3" in
                  "--version ") echo 4.18.1 ;;
                  "install --mode=skip-build") cat "{top}/report.ndjson"; exit 1 ;;
                  "config npmRegistryServer") cat "{top}/registry.json" || exit 1 ;;
                  "config npmScopes") cat "{top}/scopes.json" || exit 1 ;;
                esac
            "#, top = top},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        std::fs::write(top.join("report.ndjson"), CURRENT)?;
        let (cache, tool) = tool()?;
        let tool = tool.with_driver(script.as_str()).with_verification(
            YarnEnvironment {
                home: Some(home),
                ..YarnEnvironment::default()
            },
            vec![top.join("tmp")],
        );
        Ok(World {
            _project_guard: project_guard,
            _guard: guard,
            top,
            project,
            _cache: cache,
            tool,
        })
    }

    impl World {
        fn answer(&self, file: &str, contents: &str) -> eyre::Result<()> {
            std::fs::write(self.top.join(file), contents)?;
            Ok(())
        }

        fn runs(&self, prefix: &str) -> usize {
            std::fs::read_to_string(self.top.join("yarn.log"))
                .unwrap_or_default()
                .lines()
                .filter(|line| line.starts_with(prefix))
                .count()
        }

        fn write(&self, relative: &str, contents: &str) -> eyre::Result<()> {
            write(&self.project.root.join(relative), contents)
        }

        async fn verify(&self) -> eyre::Result<LockStatus> {
            Ok(self.tool.verify_lock_current(&self.project).await?.status)
        }
    }

    fn write(path: &Utf8Path, contents: &str) -> eyre::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
        Ok(())
    }

    fn assert_conflict(confirmed: &cooldown_core::Result<()>) {
        assert!(
            matches!(
                confirmed,
                Err(CoreError::LockConflict(message))
                    if message.contains("changed while cooldown verified them")
            ),
            "{confirmed:?}"
        );
    }

    /// A Berry lock verified current binds the inputs the copy held: confirming them unchanged
    /// passes once, and a changed manifest, a new manifest, or a new home rc file between the
    /// verification and the confirmation is a conflict.
    #[tokio::test]
    async fn a_verified_lock_binds_the_inputs_it_was_verified_against() -> eyre::Result<()> {
        let world = new_world()?;
        let project = &world.project;

        // Unchanged inputs confirm, and the record is spent.
        assert_eq!(world.verify().await?, LockStatus::Current);
        world.tool.confirm_verified_inputs(project).await?;
        world.tool.confirm_verified_inputs(project).await?;

        // A member manifest edited meanwhile
        assert_eq!(world.verify().await?, LockStatus::Current);
        world.write(
            "packages/b/package.json",
            r#"{"name":"pkg-b","version":"1.0.0","dependencies":{"chalk":"4.1.2"}}"#,
        )?;
        assert_conflict(&world.tool.confirm_verified_inputs(project).await);

        // A manifest that appeared meanwhile
        assert_eq!(world.verify().await?, LockStatus::Current);
        world.write("packages/c/package.json", r#"{"name":"pkg-c"}"#)?;
        assert_conflict(&world.tool.confirm_verified_inputs(project).await);

        // An rc file read in place that appeared meanwhile
        assert_eq!(world.verify().await?, LockStatus::Current);
        write(&world.top.join("home/.yarnrc.yml"), "nodeLinker: pnp\n")?;
        assert_conflict(&world.tool.confirm_verified_inputs(project).await);

        // A stale verdict binds nothing, so there is nothing to confirm.
        world.answer("report.ndjson", STALE_MEMBER)?;
        assert_eq!(world.verify().await?, LockStatus::Stale);
        world.write("packages/d/package.json", r#"{"name":"pkg-d"}"#)?;
        world.tool.confirm_verified_inputs(project).await?;
        Ok(())
    }

    /// The identities a Berry lock grants survive where Yarn routes every package to the public
    /// registry and are withheld behind a mirror; Yarn is asked once per project.
    #[tokio::test]
    async fn berry_identities_follow_yarns_routing() -> eyre::Result<()> {
        // Public routing keeps every identity, asked once.
        let world = new_world()?;
        world.answer("registry.json", PUBLIC_REGISTRY)?;
        world.answer("scopes.json", PRIVATE_SCOPE)?;
        let mut deps = world
            .tool
            .dependencies(&world.project, DepScope::Graph)
            .await?;
        assert!(deps.iter().all(|dep| dep.advisory_identity.is_some()));
        world
            .tool
            .confirm_advisory_identities(&world.project, &mut deps)
            .await;
        world
            .tool
            .confirm_advisory_identities(&world.project, &mut deps)
            .await;
        assert!(
            deps.iter().all(|dep| dep.advisory_identity.is_some()),
            "{deps:?}"
        );
        assert_eq!(world.runs("config get npmRegistryServer"), 1);
        assert_eq!(world.runs("config get npmScopes"), 1);

        // A mirror withholds every identity that falls back to it.
        let world = new_world()?;
        world.answer("registry.json", MIRROR_REGISTRY)?;
        world.answer("scopes.json", JSR_ENTRY)?;
        let mut deps = world
            .tool
            .dependencies(&world.project, DepScope::Graph)
            .await?;
        world
            .tool
            .confirm_advisory_identities(&world.project, &mut deps)
            .await;
        assert!(
            deps.iter().all(|dep| dep.advisory_identity.is_none()),
            "{deps:?}"
        );
        Ok(())
    }

    /// A routing Yarn cannot report, or one a plugin may change, withholds every identity; a
    /// plugin does so without asking Yarn at all.
    #[tokio::test]
    async fn an_unestablished_routing_withholds_every_identity() -> eyre::Result<()> {
        // The query fails: the fake has no answers to print.
        let world = new_world()?;
        let mut deps = world
            .tool
            .dependencies(&world.project, DepScope::Graph)
            .await?;
        world
            .tool
            .confirm_advisory_identities(&world.project, &mut deps)
            .await;
        assert!(deps.iter().all(|dep| dep.advisory_identity.is_none()));
        assert_eq!(world.runs("config get npmRegistryServer"), 1);

        // A plugin is configured.
        let world = new_world()?;
        world.answer("registry.json", PUBLIC_REGISTRY)?;
        world.answer("scopes.json", PRIVATE_SCOPE)?;
        let mut deps = world
            .tool
            .dependencies(&world.project, DepScope::Graph)
            .await?;
        world.write(".yarnrc.yml", "plugins:\n  - .yarn/plugins/x.cjs\n")?;
        world
            .tool
            .confirm_advisory_identities(&world.project, &mut deps)
            .await;
        assert!(deps.iter().all(|dep| dep.advisory_identity.is_none()));
        assert_eq!(world.runs("config"), 0);
        Ok(())
    }
}
