//! Verification against a scripted fake `yarn`, which prints a captured report and logs how it
//! was run.

use super::report::captures::{
    CURRENT, MALFORMED_RC, NETWORK_NEEDED, STALE_MEMBER, YARN2_CURRENT, YARN3_CURRENT,
};
use super::{Context, Verification, lock_current};
use crate::lock::yarn_berry_fixtures::{ProjectFile, SCHEMA_FILES, SCHEMAS};
use crate::lock::{NodeLock as _, Yarn};
use crate::manifest::workspaces::YarnEnvironment;
use crate::nodecmd::NodeCmd;
use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre;
use cooldown_core::{CoreError, LockStatus, Project};
use std::collections::BTreeMap;

/// A project below a home directory of its own, a scratch base outside both, and a fake `yarn`
/// logging to a file outside the project.
struct Fixture {
    /// Owns the temporary directory; dropping it deletes everything, so it stays bound.
    _guard: tempfile::TempDir,
    top: Utf8PathBuf,
    project: Project,
    context: Context,
}

fn new_fixture() -> eyre::Result<Fixture> {
    let guard = tempfile::tempdir()?;
    let top = Utf8PathBuf::from_path_buf(dunce::canonicalize(guard.path())?)
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    let root = top.join("project");
    for ProjectFile { path, contents } in SCHEMA_FILES {
        write(&root.join(path), contents)?;
    }
    let lock = SCHEMAS
        .last()
        .ok_or_else(|| eyre::eyre!("the schema fixtures hold a lock"))?
        .lock;
    write(&root.join("yarn.lock"), lock)?;
    let home = top.join("home");
    std::fs::create_dir_all(&home)?;
    Ok(Fixture {
        _guard: guard,
        project: Project::detected(root.clone(), Yarn::ID, root.join("package.json")),
        context: Context {
            environment: YarnEnvironment {
                home: Some(home),
                ..YarnEnvironment::default()
            },
            bases: vec![top.join("tmp")],
        },
        top,
    })
}

fn write(path: &Utf8Path, contents: &str) -> eyre::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}

/// How the fake `yarn` behaves.
struct Fake<'a> {
    /// What `--version` prints.
    version: &'a str,
    /// The status `--version` exits with.
    version_status: u8,
    /// The report `install` prints.
    report: &'a str,
    /// Shell commands `install` runs before printing its report.
    install: &'a str,
}

/// A Yarn 4.18.1 that proves the lock current and does nothing else.
const YARN_4: Fake<'static> = Fake {
    version: "4.18.1",
    version_status: 0,
    report: CURRENT,
    install: ":",
};

impl Fixture {
    /// Writes the fake and returns its driver.
    fn fake(&self, fake: &Fake<'_>) -> eyre::Result<NodeCmd> {
        use std::os::unix::fs::PermissionsExt as _;
        let report = self.top.join("report.ndjson");
        std::fs::write(&report, fake.report)?;
        let script = self.top.join("fake-yarn.sh");
        std::fs::write(
            &script,
            indoc::formatdoc! {r#"
                #!/bin/sh
                {{
                  echo "run: $*"
                  echo "pwd: $(pwd)"
                  env | grep -i -E '^(yarn_|corepack_|git_allow_protocol)' | sort
                }} >> "{log}"
                if [ "$1" = "--version" ]; then
                  echo "{version}"
                  exit {version_status}
                fi
                {install}
                cat "{report}"
                exit 1
            "#,
                log = self.log_path(),
                version = fake.version,
                version_status = fake.version_status,
                install = fake.install,
            },
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        Ok(NodeCmd::with_bin(script.as_str()))
    }

    fn log_path(&self) -> Utf8PathBuf {
        self.top.join("yarn.log")
    }

    /// Every line the fake logged, empty when it never ran.
    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.log_path())
            .map(|log| log.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// The commands the fake ran, in order.
    fn runs(&self) -> Vec<String> {
        self.log()
            .iter()
            .filter_map(|line| line.strip_prefix("run: ").map(str::to_string))
            .collect()
    }

    async fn verify(&self, fake: &Fake<'_>) -> eyre::Result<Verification> {
        let driver = self.fake(fake)?;
        Ok(lock_current(&driver, &self.project, &self.context).await?)
    }
}

/// A current report proves the lock current: Yarn ran its immutable install in a copy outside the
/// project, offline and with every output redirected into the scratch directory, and the copy is
/// gone afterwards.
#[tokio::test]
async fn a_current_report_proves_the_lock_current_in_a_copy() -> eyre::Result<()> {
    let fixture = new_fixture()?;

    let verification = fixture.verify(&YARN_4).await?;

    assert_eq!(verification.report.status, LockStatus::Current);
    assert!(verification.digest.is_some());
    assert_eq!(
        fixture.runs(),
        ["--version", "install --immutable --mode=skip-build --json"]
    );

    // The run's directory is the project's position in a copy below the scratch base.
    let log = fixture.log();
    let pwd = log
        .iter()
        .rev()
        .find_map(|line| line.strip_prefix("pwd: "))
        .ok_or_else(|| eyre::eyre!("the fake logged its directory: {log:?}"))?;
    let base = fixture.top.join("tmp");
    assert!(pwd.starts_with(base.as_str()), "{pwd}");
    assert!(pwd.ends_with("/project"), "{pwd}");
    assert!(
        !Utf8Path::new(pwd).exists(),
        "the copy {pwd} outlives the run"
    );

    // Offline, with hardened mode's automatic default overridden and outputs in the scratch.
    let settings: BTreeMap<&str, &str> =
        log.iter().filter_map(|line| line.split_once('=')).collect();
    for (name, value) in [
        ("YARN_ENABLE_NETWORK", "0"),
        ("COREPACK_ENABLE_NETWORK", "0"),
        ("GIT_ALLOW_PROTOCOL", "file"),
        ("YARN_ENABLE_HARDENED_MODE", "0"),
        ("YARN_ENABLE_GLOBAL_CACHE", "0"),
        ("COREPACK_ENABLE_AUTO_PIN", "0"),
    ] {
        assert_eq!(settings.get(name), Some(&value), "{name}: {settings:?}");
    }
    for name in [
        "YARN_CACHE_FOLDER",
        "YARN_GLOBAL_FOLDER",
        "YARN_INSTALL_STATE_PATH",
        "YARN_VIRTUAL_FOLDER",
        "YARN_PNP_UNPLUGGED_FOLDER",
    ] {
        let folder = settings.get(name).copied().unwrap_or_default();
        assert!(folder.starts_with(base.as_str()), "{name}={folder}");
    }
    Ok(())
}

/// A lockfile change is stale, quoting Yarn's diff, and binds no inputs.
#[tokio::test]
async fn a_lockfile_change_is_stale_with_its_diff() -> eyre::Result<()> {
    let fixture = new_fixture()?;

    let verification = fixture
        .verify(&Fake {
            report: STALE_MEMBER,
            ..YARN_4
        })
        .await?;

    assert_eq!(verification.report.status, LockStatus::Stale);
    assert!(verification.digest.is_none());
    let detail = &verification.report.detail;
    assert!(detail.contains("fails with YN0028"), "{detail}");
    assert!(
        detail.contains(r#"-    has-flag: "npm:^4.0.0""#),
        "{detail}"
    );
    assert!(detail.contains(fixture.project.root.as_str()), "{detail}");
    Ok(())
}

/// A report without a verdict, or no report at all, is unknown, and names the project rather
/// than the throwaway copy.
#[tokio::test]
async fn no_verdict_is_unknown_and_names_the_project() -> eyre::Result<()> {
    let fixture = new_fixture()?;
    let offline = fixture
        .verify(&Fake {
            report: NETWORK_NEEDED,
            ..YARN_4
        })
        .await?;
    assert_eq!(offline.report.status, LockStatus::Unknown);
    assert!(offline.digest.is_none());
    let detail = &offline.report.detail;
    assert!(detail.contains("reached no verdict offline"), "{detail}");
    assert!(detail.contains("YN0080"), "{detail}");

    // The usage error names the copy's rc file, which the diagnostic relabels.
    let fixture = new_fixture()?;
    let refused = fixture
        .verify(&Fake {
            report: MALFORMED_RC,
            install: r#"echo "Usage Error: Parse error when loading $(pwd)/.yarnrc.yml""#,
            ..YARN_4
        })
        .await?;
    assert_eq!(refused.report.status, LockStatus::Unknown);
    let detail = &refused.report.detail;
    assert!(detail.contains("printed no install report"), "{detail}");
    let named = format!("{}/.yarnrc.yml", fixture.project.root);
    assert!(detail.contains(&named), "{detail}");
    assert!(detail.contains("Usage Error"), "{detail}");
    assert!(!detail.contains('\u{1b}'), "{detail}");
    assert!(
        !detail.contains(fixture.top.join("tmp").as_str()),
        "{detail}"
    );
    Ok(())
}

/// Yarn 2 and 3 verify with their own build-skipping flag and without the hardened-mode
/// setting, which they would refuse.
#[tokio::test]
async fn each_release_line_runs_with_its_own_flags() -> eyre::Result<()> {
    for (version, report, flag) in [
        ("2.4.3", YARN2_CURRENT, "--skip-builds"),
        ("3.8.7", YARN3_CURRENT, "--mode=skip-build"),
    ] {
        let fixture = new_fixture()?;
        let verification = fixture
            .verify(&Fake {
                version,
                report,
                ..YARN_4
            })
            .await?;
        assert_eq!(verification.report.status, LockStatus::Current, "{version}");
        assert_eq!(
            fixture.runs(),
            [
                "--version".to_string(),
                format!("install --immutable {flag} --json")
            ]
        );
        assert!(
            !fixture
                .log()
                .iter()
                .any(|line| line.starts_with("YARN_ENABLE_HARDENED_MODE")),
            "{version}"
        );
    }
    Ok(())
}

/// A release without a verifying profile is unknown once `--version` has answered, and no
/// install runs.
#[tokio::test]
async fn a_release_without_a_profile_runs_only_the_version_probe() -> eyre::Result<()> {
    for version in ["1.22.22", "2.3.0", "5.0.0"] {
        let fixture = new_fixture()?;
        let verification = fixture.verify(&Fake { version, ..YARN_4 }).await?;
        assert_eq!(verification.report.status, LockStatus::Unknown, "{version}");
        assert_eq!(fixture.runs(), ["--version"], "{version}");
    }
    Ok(())
}

/// A version probe that fails, as Corepack does offline without the release cached, is
/// unknown, and no install runs.
#[tokio::test]
async fn a_failing_version_probe_is_unknown() -> eyre::Result<()> {
    let fixture = new_fixture()?;
    let driver = fixture.fake(&Fake {
        version_status: 1,
        ..YARN_4
    })?;

    let verification = lock_current(&driver, &fixture.project, &fixture.context).await?;

    assert_eq!(verification.report.status, LockStatus::Unknown);
    assert!(
        verification
            .report
            .detail
            .contains("`yarn --version` failed"),
        "{}",
        verification.report.detail
    );
    assert_eq!(fixture.runs(), ["--version"]);
    Ok(())
}

/// A copy that declines runs nothing at all.
#[tokio::test]
async fn a_declined_copy_runs_no_yarn() -> eyre::Result<()> {
    let fixture = new_fixture()?;
    write(
        &fixture.project.root.join(".yarnrc.yml"),
        "plugins:\n  - .yarn/plugins/x.cjs\n",
    )?;

    let verification = fixture.verify(&YARN_4).await?;

    assert_eq!(verification.report.status, LockStatus::Unknown);
    assert!(
        verification
            .report
            .detail
            .contains("configures Yarn plugins"),
        "{}",
        verification.report.detail
    );
    assert_eq!(fixture.log(), Vec::<String>::new());
    Ok(())
}

/// A lock the Berry reader refuses is unreadable before any Yarn runs.
#[tokio::test]
async fn a_malformed_lock_is_unreadable() -> eyre::Result<()> {
    let fixture = new_fixture()?;
    write(
        &fixture.project.root.join("yarn.lock"),
        "__metadata:\n  version: [\n",
    )?;

    let verified = fixture.verify(&YARN_4).await;

    let err = verified
        .err()
        .ok_or_else(|| eyre::eyre!("the lock is refused"))?;
    assert!(
        matches!(
            err.downcast_ref::<CoreError>(),
            Some(CoreError::LockUnreadable(_))
        ),
        "{err:?}"
    );
    assert_eq!(fixture.log(), Vec::<String>::new());
    Ok(())
}

/// Every path, kind, mode, content, and link target below `root`.
fn snapshot(root: &Utf8Path) -> eyre::Result<BTreeMap<String, String>> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut entries = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|path| eyre::eyre!("{} is not UTF-8", path.display()))?;
            let metadata = std::fs::symlink_metadata(&path)?;
            let mode = metadata.permissions().mode();
            let shown = if metadata.is_symlink() {
                format!("link {} {mode:o}", std::fs::read_link(&path)?.display())
            } else if metadata.is_dir() {
                pending.push(path.clone());
                format!("dir {mode:o}")
            } else {
                format!("file {mode:o} {:?}", std::fs::read(&path)?)
            };
            entries.insert(path.to_string(), shown);
        }
    }
    Ok(entries)
}

/// A hostile `yarn` that rewrites, deletes, and creates everything it can reach through the copy,
/// links included, leaves the source project byte for byte as it was.
#[tokio::test]
async fn a_hostile_yarn_leaves_the_source_byte_identical() -> eyre::Result<()> {
    let fixture = new_fixture()?;
    let root = &fixture.project.root;
    write(
        &root.join("packages/a/package.json"),
        r#"{"name":"pkg-a","dependencies":{"local":"file:../../local","portal":"portal:../../portal"}}"#,
    )?;
    write(&root.join("local/package.json"), r#"{"name":"local"}"#)?;
    write(&root.join("local/index.js"), "module.exports = 1;\n")?;
    std::os::unix::fs::symlink("index.js", root.join("local/main.js"))?;
    write(&root.join("portal/package.json"), r#"{"name":"portal"}"#)?;
    write(
        &root.join("libs/shared/package.json"),
        r#"{"name":"shared"}"#,
    )?;
    std::os::unix::fs::symlink("../libs/shared", root.join("packages/shared"))?;
    write(&root.join(".yarnrc.yml"), "nodeLinker: node-modules\n")?;
    write(&root.join(".yarn/releases/yarn.cjs"), "// yarn\n")?;
    write(&root.join(".yarn/cache/chalk.zip"), "zip\n")?;
    write(&root.join(".pnp.cjs"), "// pnp\n")?;
    let before = snapshot(root)?;

    let verification = fixture
        .verify(&Fake {
            install: indoc::indoc! {r#"
                find -L . -type f -exec sh -c 'echo pwned >> "$1"' _ {} \;
                find -L . -type d -exec sh -c 'echo pwned > "$1/pwned"' _ {} \;
                for link in $(find . -type l); do echo pwned > "$link"; done
                echo pwned > ../.yarnrc.yml
                mkdir -p node_modules/.bin .yarn/unplugged && echo pwned > .pnp.cjs
                for folder in "$YARN_CACHE_FOLDER" "$YARN_GLOBAL_FOLDER" "$YARN_VIRTUAL_FOLDER" "$YARN_PNP_UNPLUGGED_FOLDER"; do
                  mkdir -p "$folder" && echo pwned > "$folder/pwned"
                done
                echo pwned > "$YARN_INSTALL_STATE_PATH"
                rm -rf ./packages ./local yarn.lock
            "#},
            ..YARN_4
        })
        .await?;

    assert_eq!(verification.report.status, LockStatus::Current);
    assert_eq!(snapshot(root)?, before);
    Ok(())
}
