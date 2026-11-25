use super::{
    NO_LOCK, OUT_OF_SYNC, Refusal, ci_refusal, error_code, non_registry_override, supports_dry_run,
    unproven_by_npm,
};
use camino::Utf8PathBuf;
use color_eyre::eyre;
use indoc::{formatdoc, indoc};
use serde_json::{Map, Value, json};

/// npm 11's report for a lock that no longer satisfies `package.json`, usage text included.
const OUT_OF_SYNC_NPM_11: &str = indoc! {"
    npm error code EUSAGE
    npm error
    npm error `npm ci` can only install packages when your package.json and package-lock.json or npm-shrinkwrap.json are in sync. Please update your lock file with `npm install` before continuing.
    npm error
    npm error Invalid: lock file's ms@2.1.3 does not satisfy ms@2.1.2
    npm error Missing: debug@4.3.4 from lock file
    npm error
    npm error Clean install a project
    npm error
    npm error Usage:
    npm error npm ci
    npm error A complete log of this run can be found in: /cache/npm-verify/logs/debug-0.log
"};

/// npm 10.5.1's report for a project without a lock.
const NO_LOCK_NPM_10: &str = indoc! {"
    npm ERR! code EUSAGE
    npm ERR!
    npm ERR! The `npm ci` command can only install with an existing package-lock.json or
    npm ERR! npm-shrinkwrap.json with lockfileVersion >= 1. Run an install with npm@5 or
    npm ERR! later to generate a package-lock.json file, then try again.
    npm ERR!
    npm ERR! Clean install a project
"};

/// Both refusals are read in npm 10's and npm 11's rendering, with npm's message and mismatches
/// kept for the stale detail and the usage text left out.
#[test]
fn ci_refusal_reads_both_renderings() {
    let mismatches = "Invalid: lock file's ms@2.1.3 does not satisfy ms@2.1.2 \
                      Missing: debug@4.3.4 from lock file";
    let out_of_sync = || Refusal::OutOfSync(format!("{OUT_OF_SYNC} {mismatches}"));
    assert_eq!(ci_refusal(OUT_OF_SYNC_NPM_11), Some(out_of_sync()));
    let npm_10 = OUT_OF_SYNC_NPM_11
        .replace("npm error", "npm ERR!")
        .replace("npm ERR!\n", "npm ERR! \n");
    assert_eq!(ci_refusal(&npm_10), Some(out_of_sync()));

    let no_lock = || {
        Refusal::NoLock(format!(
            "{NO_LOCK} npm-shrinkwrap.json with lockfileVersion >= 1. Run an install with npm@5 \
             or later to generate a package-lock.json file, then try again."
        ))
    };
    assert_eq!(ci_refusal(NO_LOCK_NPM_10), Some(no_lock()));
    let npm_11 = NO_LOCK_NPM_10
        .replace("npm ERR! ", "npm error ")
        .replace("npm error \n", "npm error\n");
    assert_eq!(ci_refusal(&npm_11), Some(no_lock()));
}

/// Only a report that opens with `EUSAGE` and leads with one of npm's two messages is a refusal.
#[test]
fn ci_refusal_is_anchored_on_the_code_and_the_message() {
    // Another code ahead of the message
    let other_code = OUT_OF_SYNC_NPM_11.replace("code EUSAGE", "code ENOTCACHED");
    assert_eq!(ci_refusal(&other_code), None);

    // The message quoted below another usage error's own
    let quoted = formatdoc! {"
        npm error code EUSAGE
        npm error
        npm error Unknown option: --frozen
        npm error
        npm error {OUT_OF_SYNC}
    "};
    assert_eq!(ci_refusal(&quoted), None);

    // The message on lines without npm's error heading, or with a heading glued to the text
    let unheaded = formatdoc! {"
        code EUSAGE

        {OUT_OF_SYNC}
        npm errorcode EUSAGE
    "};
    assert_eq!(ci_refusal(&unheaded), None);

    // A message that merely starts like npm's
    let extended = OUT_OF_SYNC_NPM_11.replace("before continuing.", "before continuing. Or not.");
    assert_eq!(ci_refusal(&extended), None);
    assert_eq!(ci_refusal(""), None);
}

/// The code a report opens with is read in both renderings, so a configured `workspace` filter
/// is recognized whichever npm refuses `npm prefix` for it.
#[test]
fn error_code_reads_the_opening_code() {
    let npm_10 = indoc! {"
        npm ERR! code ENOWORKSPACES
        npm ERR! This command does not support workspaces.
    "};
    assert_eq!(error_code(npm_10), Some("ENOWORKSPACES"));
    assert_eq!(
        error_code(&npm_10.replace("npm ERR!", "npm error")),
        Some("ENOWORKSPACES")
    );
    assert_eq!(
        error_code("This command does not support workspaces."),
        None
    );
}

/// npm 10.5.1 is the first whose `ci --dry-run` keeps `node_modules`, a prerelease of it still
/// does not, and an unreadable version proves nothing.
#[test]
fn the_dry_run_needs_npm_10_5_1() {
    for supported in ["10.5.1\n", "10.9.8", "11.0.0-pre.1", "11.16.0"] {
        assert!(supports_dry_run(supported), "{supported}");
    }
    for unsupported in [
        "10.5.0",
        "10.5.1-pre.0",
        "6.14.18",
        "9.9.9",
        "",
        "v11.16.0",
        "eleven",
    ] {
        assert!(!supports_dry_run(unsupported), "{unsupported}");
    }
}

/// A project root holding `files` at their project-relative paths.
struct TempProject {
    _directory: tempfile::TempDir,
    root: Utf8PathBuf,
}

fn temp_project(files: &[(&str, Value)]) -> eyre::Result<TempProject> {
    let directory = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    for (path, content) in files {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string(content)?)?;
    }
    Ok(TempProject {
        _directory: directory,
        root,
    })
}

fn packages(lock: Value) -> eyre::Result<Map<String, Value>> {
    match lock {
        Value::Object(packages) => Ok(packages),
        other => eyre::bail!("not a packages map: {other}"),
    }
}

/// A workspace whose root, member `packages/a`, and `file:` target `libs/x` each declare what
/// the lock recorded for them, the way npm 11 wrote it.
fn workspace() -> [(&'static str, Value); 3] {
    [
        (
            "package.json",
            json!({
                "name": "ws",
                "private": true,
                "workspaces": ["packages/*"],
                "dependencies": { "x": "file:libs/x", "debug": "4.3.4" },
                "optionalDependencies": { "debug": "4.3.4" },
                "overrides": { "ms": "2.1.3", "debug": { ".": "$debug", "ms": "^2.1.0" } }
            }),
        ),
        (
            "packages/a/package.json",
            json!({ "name": "a", "version": "1.0.0", "dependencies": { "ms": "2.1.3" } }),
        ),
        (
            "libs/x/package.json",
            json!({
                "name": "x",
                "version": "0.1.0",
                "dependencies": { "ms": "^2.1.0" },
                "devDependencies": {}
            }),
        ),
    ]
}

/// The lock npm records for [`workspace`]: optional declarations out of `dependencies`, empty
/// fields as written, and no `overrides`.
fn workspace_lock() -> Value {
    json!({
        "": {
            "name": "ws",
            "workspaces": ["packages/*"],
            "dependencies": { "x": "file:libs/x" },
            "optionalDependencies": { "debug": "4.3.4" }
        },
        "libs/x": { "version": "0.1.0", "dependencies": { "ms": "^2.1.0" }, "devDependencies": {} },
        "node_modules/a": { "resolved": "packages/a", "link": true },
        "node_modules/x": { "resolved": "libs/x", "link": true },
        "node_modules/ms": { "version": "2.1.3", "dependencies": { "unrecorded": "1.0.0" } },
        "packages/a": { "version": "1.0.0", "dependencies": { "ms": "2.1.3" } }
    })
}

/// A lock npm wrote for the manifests as they are leaves npm's exit 0 standing.
#[test]
fn recorded_declarations_leave_npm_the_verdict() -> eyre::Result<()> {
    let project = temp_project(&workspace())?;
    assert_eq!(
        unproven_by_npm(&project.root, &packages(workspace_lock())?),
        None
    );
    Ok(())
}

/// Every local package's declarations and the root's workspace patterns are compared, since
/// npm's own check sees versions alone, and every local package the lock records must still have
/// its manifest.
/// A member newly matched by the patterns is npm's to find.
#[test]
fn declarations_npm_does_not_compare_leave_the_lock_unproven() -> eyre::Result<()> {
    let unproven = |path: &str, edit: &dyn Fn(&mut Value)| -> eyre::Result<Option<String>> {
        let mut files = workspace();
        for (file, content) in &mut files {
            if *file == path {
                edit(content);
            }
        }
        let project = temp_project(&files)?;
        Ok(unproven_by_npm(&project.root, &packages(workspace_lock())?))
    };

    // A same-version source change of the member's declaration
    let tarball = unproven("packages/a/package.json", &|manifest| {
        manifest["dependencies"]["ms"] = json!("https://registry.npmjs.org/ms/-/ms-2.1.3.tgz");
    })?;
    assert_eq!(
        tarball.as_deref(),
        Some(
            "packages/a/package.json declares other dependencies than package-lock.json \
             recorded for it; run `npm install`"
        )
    );

    // A declaration added to a `file:` target, moved to another field at the root, or a changed
    // workspace pattern
    for (path, edit) in [
        (
            "libs/x/package.json",
            &(|manifest: &mut Value| manifest["devDependencies"] = json!({ "ms": "2.1.3" }))
                as &dyn Fn(&mut Value),
        ),
        ("package.json", &|manifest: &mut Value| {
            manifest["devDependencies"] = json!({ "x": "file:libs/x" });
            manifest["dependencies"] = json!({ "debug": "4.3.4" });
        }),
        ("package.json", &|manifest: &mut Value| {
            manifest["workspaces"] = json!(["packages/*", "tools/*"]);
        }),
    ] {
        assert!(unproven(path, edit)?.is_some(), "{path}");
    }

    // A member the lock records without a manifest on disk
    let mut files = workspace().to_vec();
    files.retain(|(path, _)| *path != "packages/a/package.json");
    let project = temp_project(&files)?;
    assert_eq!(
        unproven_by_npm(&project.root, &packages(workspace_lock())?).as_deref(),
        Some("packages/a/package.json cannot be read as JSON")
    );

    // A lock without the root package's entry
    let mut lock = workspace_lock();
    if let Some(packages) = lock.as_object_mut() {
        packages.remove("");
    }
    let project = temp_project(&workspace())?;
    assert!(unproven_by_npm(&project.root, &packages(lock)?).is_some());
    Ok(())
}

/// A lock early npm 7 wrote, recording an optional dependency in `dependencies` as well, matches
/// the manifest that declares it optional alone.
#[test]
fn an_optional_dependency_recorded_twice_matches_the_lock() -> eyre::Result<()> {
    let project = temp_project(&[(
        "package.json",
        json!({ "name": "app", "optionalDependencies": { "ms": "2.1.3" } }),
    )])?;
    let lock = json!({ "": {
        "name": "app",
        "dependencies": { "ms": "2.1.3" },
        "optionalDependencies": { "ms": "2.1.3" }
    } });
    assert_eq!(unproven_by_npm(&project.root, &packages(lock)?), None);
    Ok(())
}

/// An empty `workspaces` list, which npm does not record, matches the lock, while the object form
/// is compared as written.
#[test]
fn an_empty_workspaces_list_is_no_workspace_pattern() -> eyre::Result<()> {
    let unproven = |workspaces: Value| -> eyre::Result<Option<String>> {
        let project = temp_project(&[(
            "package.json",
            json!({ "name": "app", "workspaces": workspaces, "dependencies": { "ms": "2.1.3" } }),
        )])?;
        let lock = json!({ "": { "name": "app", "dependencies": { "ms": "2.1.3" } } });
        Ok(unproven_by_npm(&project.root, &packages(lock)?))
    };

    assert_eq!(unproven(json!([]))?, None);
    assert_eq!(unproven(Value::Null)?, None);
    assert!(unproven(json!({ "packages": [] }))?.is_some());
    Ok(())
}

/// A root override naming another source than a registry range, which the lock records nowhere,
/// leaves the lock unproven, at any depth of the override tree.
#[test]
fn a_non_registry_override_leaves_the_lock_unproven() -> eyre::Result<()> {
    let mut files = workspace();
    let [(_, root_manifest), ..] = &mut files;
    root_manifest["overrides"]["debug"]["ms"] = json!("npm:other-ms@2.1.3");
    let project = temp_project(&files)?;
    assert_eq!(
        unproven_by_npm(&project.root, &packages(workspace_lock())?).as_deref(),
        Some(
            "package.json overrides a dependency with `npm:other-ms@2.1.3`, a source \
             package-lock.json does not record"
        )
    );
    Ok(())
}

/// Versions, ranges, and references to a root declaration that is a range are registry
/// overrides; every other source is not.
#[test]
fn overrides_qualify_by_their_source() {
    let manifest = json!({});
    for registry in ["2.1.3", "^2.1.0", "~1.2", ">=1 <2", "1.x || ^3", "*"] {
        assert_eq!(
            non_registry_override(&json!(registry), &manifest),
            None,
            "{registry}"
        );
    }
    assert_eq!(
        non_registry_override(
            &json!({ "a@1": { ".": "1.0.0", "b": "^2" }, "c": null }),
            &manifest
        ),
        None
    );
    for other in [
        "npm:other@1.0.0",
        "https://example.com/ms-2.1.3.tgz",
        "git+https://github.com/vercel/ms.git",
        "github:vercel/ms",
        "vercel/ms",
        "file:../ms",
        "link:../ms",
        "./ms-2.1.3.tgz",
        "latest",
        "",
    ] {
        assert_eq!(
            non_registry_override(&json!({ "ms": other }), &manifest).as_deref(),
            Some(other),
            "{other}"
        );
    }
    assert_eq!(
        non_registry_override(&json!({ "ms": 2 }), &manifest).as_deref(),
        Some("2")
    );
}

/// A `$name` override stands for the root's own declaration of `name`, found in npm's field
/// order, so it is a registry override only when that declaration is a range.
#[test]
fn a_reference_override_qualifies_by_the_declaration_it_names() {
    let manifest = json!({
        "dependencies": { "ms": "https://registry.npmjs.org/ms/-/ms-2.1.3.tgz" },
        "devDependencies": { "ms": "^2.1.0", "debug": "" },
        "optionalDependencies": { "debug": "github:debug-js/debug" },
        "peerDependencies": { "chalk": "^5" }
    });
    let override_of =
        |reference: &str| non_registry_override(&json!({ "x": reference }), &manifest);

    // `devDependencies` wins over `dependencies`, and a peer declaration counts.
    assert_eq!(override_of("$ms"), None);
    assert_eq!(override_of("$chalk"), None);
    // An empty declaration is skipped for the next field, here a git source.
    assert_eq!(override_of("$debug").as_deref(), Some("$debug"));
    // A name the root does not declare
    assert_eq!(override_of("$absent").as_deref(), Some("$absent"));

    // npm drops a `dependencies` entry `optionalDependencies` also declares, so an empty optional
    // declaration leads on to the peer's tarball rather than back to the range.
    let shadowed = json!({
        "dependencies": { "ms": "^2.1.0" },
        "optionalDependencies": { "ms": "" },
        "peerDependencies": { "ms": "https://registry.npmjs.org/ms/-/ms-2.1.3.tgz" }
    });
    assert_eq!(
        non_registry_override(&json!({ "x": "$ms" }), &shadowed).as_deref(),
        Some("$ms")
    );

    // A reference to a tarball declaration
    let tarball =
        json!({ "dependencies": { "ms": "https://registry.npmjs.org/ms/-/ms-2.1.3.tgz" } });
    assert_eq!(
        non_registry_override(&json!({ "ms": "$ms" }), &tarball).as_deref(),
        Some("$ms")
    );
}

/// A scripted `npm` and the project it runs in, with every invocation logged.
#[cfg(unix)]
struct FakeNpm {
    project: TempProject,
    cmd: crate::nodecmd::NodeCmd,
    scratch: Utf8PathBuf,
}

#[cfg(unix)]
impl FakeNpm {
    /// An npm reporting `version`, the working directory as its prefix unless `prefix` answers
    /// instead, and `ci` as `ci` answers.
    fn new(files: &[(&str, Value)], version: &str, prefix: &str, ci: &str) -> eyre::Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;
        let project = temp_project(files)?;
        let script = project.root.join("fake-npm.sh");
        std::fs::write(
            &script,
            formatdoc! {r#"
                #!/bin/sh
                echo "$* offline=${{npm_config_offline-unset}} git=${{GIT_ALLOW_PROTOCOL-unset}}" >> calls.log
                case "$1" in
                  --version) echo "{version}" ;;
                  prefix) {prefix} ;;
                  ci) {ci} ;;
                  *) echo "unexpected: $*" >&2; exit 1 ;;
                esac
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let scratch = project.root.join("scratch");
        Ok(FakeNpm {
            cmd: crate::nodecmd::NodeCmd::with_bin(script.as_str()),
            project,
            scratch,
        })
    }

    async fn verify(&self) -> cooldown_core::Result<cooldown_core::LockVerifyReport> {
        self.verify_with(false).await
    }

    async fn verify_with(
        &self,
        offline: bool,
    ) -> cooldown_core::Result<cooldown_core::LockVerifyReport> {
        super::lock_current(&self.cmd, &self.project.root, &self.scratch, offline).await
    }

    /// The commands npm ran, one per line, without the settings every run carries.
    fn commands(&self) -> eyre::Result<Vec<String>> {
        let log = std::fs::read_to_string(self.project.root.join("calls.log"))?;
        Ok(log
            .lines()
            .map(|line| line.split(" --").next().unwrap_or_default().to_string())
            .collect())
    }
}

/// The in-sync single-package project the fakes run in.
#[cfg(unix)]
fn in_sync() -> [(&'static str, Value); 2] {
    [
        (
            "package.json",
            json!({ "name": "app", "dependencies": { "ms": "2.1.3" } }),
        ),
        (
            super::LOCKFILE,
            json!({
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "app", "dependencies": { "ms": "2.1.3" } },
                    "node_modules/ms": { "version": "2.1.3" }
                }
            }),
        ),
    ]
}

#[cfg(unix)]
const PWD: &str = "pwd -P";

#[cfg(unix)]
const PASS: &str = "echo 'added 1 package'";

/// npm's exit 0 proves an in-sync lock current, while a qualification the probe cannot
/// establish is unknown, each before `npm ci` would run.
#[cfg(unix)]
#[tokio::test]
async fn the_verdict_needs_every_qualification() -> eyre::Result<()> {
    use cooldown_core::LockStatus;

    // Current
    let npm = FakeNpm::new(&in_sync(), "11.16.0", PWD, PASS)?;
    let report = npm.verify().await?;
    assert_eq!(report.status, LockStatus::Current, "{}", report.detail);
    assert_eq!(npm.commands()?, ["--version", "prefix", "ci"]);
    // Every run keeps npm's cache and logs out of the project.
    let log = std::fs::read_to_string(npm.project.root.join("calls.log"))?;
    assert!(
        log.lines().all(
            |line| line.contains(&format!("--cache={}/cache", npm.scratch))
                && line.contains(&format!("--logs-dir={}/logs", npm.scratch))
        ),
        "{log}"
    );

    // An npm below the floor, which never runs `ci`
    let npm = FakeNpm::new(&in_sync(), "10.5.0", PWD, PASS)?;
    let report = npm.verify().await?;
    assert_eq!(report.status, LockStatus::Unknown);
    assert!(
        report.detail.contains("requires npm >= 10.5.1"),
        "{}",
        report.detail
    );
    assert_eq!(npm.commands()?, ["--version"]);

    // A shrinkwrap npm would verify instead
    let mut files = in_sync().to_vec();
    files.push(("npm-shrinkwrap.json", json!({})));
    let npm = FakeNpm::new(&files, "11.16.0", PWD, PASS)?;
    assert_eq!(npm.verify().await?.status, LockStatus::Unknown);
    assert!(!npm.project.root.join("calls.log").exists());

    // An enclosing project npm would run instead
    let npm = FakeNpm::new(&in_sync(), "11.16.0", "dirname \"$(pwd -P)\"", PASS)?;
    assert_eq!(npm.verify().await?.status, LockStatus::Unknown);
    assert_eq!(npm.commands()?, ["--version", "prefix"]);

    // A configured `workspace` filter
    let enoworkspaces = "printf 'npm error code ENOWORKSPACES\\nnpm error This command does not \
                         support workspaces.\\n' >&2; exit 1";
    let npm = FakeNpm::new(&in_sync(), "11.16.0", enoworkspaces, PASS)?;
    let report = npm.verify().await?;
    assert_eq!(report.status, LockStatus::Unknown);
    assert!(
        report.detail.contains("`workspace` filter"),
        "{}",
        report.detail
    );

    // Declarations npm's exit 0 does not compare
    let mut files = in_sync();
    let [(_, manifest), _] = &mut files;
    manifest["dependencies"]["ms"] = json!("https://registry.npmjs.org/ms/-/ms-2.1.3.tgz");
    let npm = FakeNpm::new(&files, "11.16.0", PWD, PASS)?;
    assert_eq!(npm.verify().await?.status, LockStatus::Unknown);
    assert_eq!(npm.commands()?, ["--version", "prefix", "ci"]);
    Ok(())
}

/// The fake's `ci` printing `report` to standard error and failing.
#[cfg(unix)]
fn refusing(report: &str) -> String {
    format!("cat >&2 <<'EOF'\n{report}EOF\nexit 1")
}

/// npm's out-of-sync refusal is stale, while its missing-lock refusal is stale only when no lock
/// exists, and any other failure is the tool's error.
#[cfg(unix)]
#[tokio::test]
async fn only_npm_refusals_are_stale() -> eyre::Result<()> {
    use cooldown_core::{CoreError, LockStatus};

    // Out of sync
    let npm = FakeNpm::new(&in_sync(), "11.16.0", PWD, &refusing(OUT_OF_SYNC_NPM_11))?;
    let report = npm.verify().await?;
    assert_eq!(report.status, LockStatus::Stale);
    assert!(
        report.detail.starts_with(&format!(
            "package-lock.json is stale in {}: `npm ci` can only install",
            npm.project.root
        )),
        "{}",
        report.detail
    );
    assert!(
        !report.detail.contains("Clean install"),
        "{}",
        report.detail
    );

    // The missing-lock refusal without a lock
    let files = in_sync();
    let manifest_only = &files[..1];
    let npm = FakeNpm::new(manifest_only, "11.16.0", PWD, &refusing(NO_LOCK_NPM_10))?;
    assert_eq!(npm.verify().await?.status, LockStatus::Stale);

    // The missing-lock refusal while a lock exists
    let npm = FakeNpm::new(&in_sync(), "11.16.0", PWD, &refusing(NO_LOCK_NPM_10))?;
    let error = npm
        .verify()
        .await
        .expect_err("a present lock is never missing");
    assert!(matches!(error, CoreError::Tool { .. }), "{error}");

    // A dangling link in the lock's place is no missing lock either.
    let npm = FakeNpm::new(manifest_only, "11.16.0", PWD, &refusing(NO_LOCK_NPM_10))?;
    std::os::unix::fs::symlink(
        npm.project.root.join("elsewhere.json"),
        npm.project.root.join(super::LOCKFILE),
    )?;
    let error = npm
        .verify()
        .await
        .expect_err("a dangling lock is unreadable");
    assert!(matches!(error, CoreError::Filesystem(_)), "{error}");

    // Another npm failure, such as an offline run with a cold cache
    let not_cached = "printf 'npm error code ENOTCACHED\\n' >&2; exit 1";
    let npm = FakeNpm::new(&in_sync(), "11.16.0", PWD, not_cached)?;
    let error = npm.verify().await.expect_err("a cold cache is no verdict");
    assert!(matches!(error, CoreError::Tool { .. }), "{error}");
    Ok(())
}

/// A lock that appears after the probe found none was never compared with the manifests, so
/// npm's exit 0 leaves it unknown.
#[cfg(unix)]
#[tokio::test]
async fn a_lock_written_during_the_probe_is_not_current() -> eyre::Result<()> {
    use cooldown_core::LockStatus;

    let [manifest, (_, lock)] = in_sync();
    // A concurrent install writes the lock while `npm ci` runs.
    let written_now = formatdoc! {"
        cat > {lockfile} <<'EOF'
        {content}
        EOF
    ", lockfile = super::LOCKFILE, content = serde_json::to_string(&lock)?};
    let npm = FakeNpm::new(&[manifest], "11.16.0", PWD, &written_now)?;

    let report = npm.verify().await?;

    assert_eq!(report.status, LockStatus::Unknown, "{}", report.detail);
    assert!(npm.project.root.join(super::LOCKFILE).exists());
    Ok(())
}

/// An offline run keeps every npm command offline through the flag and the environment, which an
/// `npm.cmd` helper reads before the flags reach npm, and keeps git off the network.
#[cfg(unix)]
#[tokio::test]
async fn an_offline_probe_keeps_every_npm_offline() -> eyre::Result<()> {
    let npm = FakeNpm::new(&in_sync(), "11.16.0", PWD, PASS)?;

    npm.verify_with(true).await?;

    let log = std::fs::read_to_string(npm.project.root.join("calls.log"))?;
    assert_eq!(log.lines().count(), 3, "{log}");
    assert!(
        log.lines()
            .all(|line| line.contains("--offline") && line.ends_with(" offline=true git=")),
        "{log}"
    );
    Ok(())
}

/// A lock the npm reader refuses fails the probe before npm judges it.
#[cfg(unix)]
#[tokio::test]
async fn a_v1_lock_is_refused() -> eyre::Result<()> {
    let mut files = in_sync();
    let [_, (_, lock)] = &mut files;
    *lock = json!({ "lockfileVersion": 1, "dependencies": { "ms": { "version": "2.1.3" } } });
    let npm = FakeNpm::new(&files, "11.16.0", PWD, PASS)?;
    let error = npm.verify().await.expect_err("a v1 lock is refused");
    assert!(
        error
            .to_string()
            .contains("lockfileVersion 1; re-lock with npm 7 or newer"),
        "{error}"
    );
    assert_eq!(npm.commands()?, ["--version", "prefix"]);
    Ok(())
}
