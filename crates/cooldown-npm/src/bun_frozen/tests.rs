use super::config::{Environment, cache_disabled};
use super::workspace::unproven_by_bun;
use super::{
    FROZEN, Verdict, enclosing_workspace, supports_frozen_dry_run, unsupported_feature, verdict,
};
use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre;
use cooldown_core::ToolTermination;
use indoc::indoc;
use serde_json::{Value, json};

/// A project root holding files at their project-relative paths.
struct TempProject {
    _directory: tempfile::TempDir,
    root: Utf8PathBuf,
}

impl TempProject {
    /// A project holding `files`, with the root canonical as the probe reads it.
    fn new(files: &[(&str, &str)]) -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = cooldown_adapter_util::canonical_path(
            Utf8Path::from_path(directory.path())
                .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?,
        )?;
        let project = TempProject {
            _directory: directory,
            root,
        };
        for (path, content) in files {
            project.write(path, content)?;
        }
        Ok(project)
    }

    fn write(&self, path: &str, content: &str) -> eyre::Result<()> {
        let path = self.root.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
        Ok(())
    }

    fn write_json(&self, path: &str, content: &Value) -> eyre::Result<()> {
        self.write(path, &serde_json::to_string(content)?)
    }
}

/// Bun's report of a stale lock, as Bun 1.4.5 prints it to standard error.
const STALE: &str = indoc! {"
    error: lockfile had changes, but lockfile is frozen
    note: dependencies in package.json changed since bun.lock was saved: ms changed from 2.1.3 to 2.1.2
    note: try re-running without --frozen-lockfile and commit the updated lockfile
"};

/// Bun's report of a lock it could not load, which it ignores before refusing the frozen install.
const IGNORED: &str = indoc! {"
    ParserError: failed to parse lockfile: 'bun.lock'
    warn: Ignoring lockfile
    error: lockfile had changes, but lockfile is frozen
"};

/// Bun's frozen refusal is stale only as its sole error line, and only on a normal exit 1.
#[test]
fn only_a_lone_frozen_refusal_is_stale() {
    let exit = ToolTermination::ExitCode;

    // Stale, whatever colors or line endings Bun printed it with
    assert_eq!(verdict(exit(1), STALE), Verdict::Stale);
    let colored = STALE
        .replace("error:", "\u{1b}[31;1merror\u{1b}[0m:")
        .replace('\n', "\r\n");
    assert_eq!(verdict(exit(1), &colored), Verdict::Stale);

    // Another error beside the refusal, such as an offline cache miss
    let offline = format!(
        "error: --offline: no cached manifest for \"ms\"\nerror: ms@2.1.2 failed to resolve\n{STALE}"
    );
    assert_eq!(verdict(exit(1), &offline), Verdict::Failed);

    // A lock Bun could not load and ignored
    assert_eq!(verdict(exit(1), IGNORED), Verdict::Failed);

    // The refusal quoted inside another line, or extended
    let quoted = format!("error: Workspace not found \"{FROZEN}\"");
    assert_eq!(verdict(exit(1), &quoted), Verdict::Failed);
    assert_eq!(verdict(exit(1), &format!("{FROZEN}!")), Verdict::Failed);

    // Another exit, a signal, or no refusal at all
    assert_eq!(verdict(exit(2), STALE), Verdict::Failed);
    assert_eq!(verdict(ToolTermination::Signal(9), STALE), Verdict::Failed);
    assert_eq!(verdict(exit(1), "error: 401 Unauthorized"), Verdict::Failed);
    assert_eq!(verdict(exit(1), ""), Verdict::Failed);
}

/// A successful run passes only without an error or lock diagnostic.
#[test]
fn a_success_with_a_diagnostic_proves_nothing() {
    let exit = ToolTermination::ExitCode;
    assert_eq!(verdict(exit(0), ""), Verdict::Passed);
    assert_eq!(verdict(exit(0), "warn: some advice\n"), Verdict::Passed);
    assert_eq!(
        verdict(exit(0), "warn: Ignoring lockfile\n"),
        Verdict::Diagnosed
    );
    assert_eq!(verdict(exit(0), "error: oops\n"), Verdict::Diagnosed);
}

/// Bun 1.4.5 is the first stable release the probe runs, and a prerelease or an unreadable
/// revision proves nothing.
#[test]
fn the_frozen_dry_run_needs_a_stable_bun_1_4_5() {
    for supported in [
        "1.4.5+a7733435c\n",
        "1.4.5",
        "1.4.6+abc",
        "1.5.0",
        "2.0.0+x",
    ] {
        assert!(supports_frozen_dry_run(supported), "{supported}");
    }
    for unsupported in [
        "1.4.4+abc",
        "1.3.9",
        "1.5.0-canary.1+abc",
        "1.4.5-rc.1",
        "bun 1.4.5",
        "1.4",
        "",
    ] {
        assert!(!supports_frozen_dry_run(unsupported), "{unsupported}");
    }
}

/// An environment whose home and configuration directories are `home` and `xdg` below the
/// project, so no user file outside the test is read.
fn environment(project: &TempProject, xdg: bool) -> Environment {
    Environment {
        xdg_config_home: xdg.then(|| project.root.join("xdg")),
        home: Some(project.root.join("home")),
        userconfig: Vec::new(),
    }
}

/// Every bunfig Bun reads can disable its cache, while a cache directory or an enabled cache
/// leaves it on.
#[test]
fn a_bunfig_can_disable_the_cache() -> eyre::Result<()> {
    let disabled = |files: &[(&str, &str)], xdg: bool| -> eyre::Result<Option<String>> {
        let project = TempProject::new(files)?;
        Ok(cache_disabled(&project.root, &environment(&project, xdg)))
    };

    // No configuration, or a cache directory
    assert_eq!(disabled(&[], false)?, None);
    let directory = "[install.cache]\ndir = \"./.bun-cache\"\n";
    assert_eq!(disabled(&[("bunfig.toml", directory)], false)?, None);
    let directory = "[install]\ncache = \"./.bun-cache\"\n";
    assert_eq!(disabled(&[("bunfig.toml", directory)], false)?, None);
    let enabled = "[install.cache]\ndisable = false\n";
    assert_eq!(disabled(&[("bunfig.toml", enabled)], false)?, None);

    // Either switch, in the project's bunfig
    let table = "[install.cache]\ndisable = true\n";
    let reason = disabled(&[("bunfig.toml", table)], false)?;
    assert!(
        reason
            .as_deref()
            .is_some_and(|reason| reason.contains("install.cache.disable = true")),
        "{reason:?}"
    );
    let switch = "[install]\ncache = false\n";
    assert!(disabled(&[("bunfig.toml", switch)], false)?.is_some());

    // A cache setting or a file that cannot be interpreted
    let number = "[install]\ncache = 3\n";
    assert!(disabled(&[("bunfig.toml", number)], false)?.is_some());
    let string = "[install.cache]\ndisable = \"yes\"\n";
    assert!(disabled(&[("bunfig.toml", string)], false)?.is_some());
    assert!(disabled(&[("bunfig.toml", "[install")], false)?.is_some());

    // The global bunfig in the home directory, unless `XDG_CONFIG_HOME` is set, even to a
    // directory without one
    assert!(disabled(&[("home/.bunfig.toml", table)], false)?.is_some());
    assert_eq!(disabled(&[("home/.bunfig.toml", table)], true)?, None);
    assert!(disabled(&[("xdg/.bunfig.toml", table)], true)?.is_some());
    Ok(())
}

/// The project and user npmrc can disable Bun's cache with `cache=false`.
#[test]
fn an_npmrc_can_disable_the_cache() -> eyre::Result<()> {
    let disabled = |files: &[(&str, &str)], environment: &dyn Fn(&TempProject) -> Environment| {
        let project = TempProject::new(files)?;
        eyre::Ok(cache_disabled(&project.root, &environment(&project)))
    };
    let plain = |project: &TempProject| environment(project, false);
    let xdg = |project: &TempProject| environment(project, true);

    // The project's `.npmrc`, with every `cache` line that is not a proven-safe form, since Bun
    // may read it as no cache: after a comment is cut, through quotes, behind an expansion or
    // escape, or a directory the probe does not take apart
    for spelling in [
        "cache=false",
        " cache = false ",
        "CACHE=false",
        "cache=false # keep it off",
        "cache=false;off",
        "cache=\"false\"",
        "cache='false'",
        "cache = ' false '",
        "\"cache\"=false",
        "'\"cache\"' = false",
        "cache[]=false",
        "cache=${INSTALL_CACHE}",
        "cache=\\u0066alse",
        "${CACHE_KEY}=false",
        "\\u0063ache=false",
        "cache=",
        "cache=null",
        "cache=./cache # moved",
    ] {
        let reason = disabled(&[(".npmrc", spelling)], &plain)?;
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("`cache` value")),
            "{spelling}: {reason:?}"
        );
    }
    // Comments, a plain cache directory or the enabled switch, and other keys
    for other in [
        ";cache=false",
        "# cache=false",
        "cache=./cache",
        "cache=/var/cache/bun",
        "cache=true",
        "cache",
        "cache-max=false",
        "//registry.npmjs.org/:_authToken=${NPM_TOKEN}",
    ] {
        assert_eq!(disabled(&[(".npmrc", other)], &plain)?, None, "{other}");
    }

    // A configured user npmrc, relative to the project, which replaces the default one
    let configured = |project: &TempProject| Environment {
        userconfig: vec![Utf8PathBuf::from("config/user.npmrc")],
        ..environment(project, false)
    };
    assert!(disabled(&[("config/user.npmrc", "cache=false")], &configured)?.is_some());
    assert_eq!(
        disabled(&[("home/.npmrc", "cache=false")], &configured)?,
        None
    );

    // The default user npmrc: `XDG_CONFIG_HOME`'s when it exists, else the home directory's
    assert!(disabled(&[("home/.npmrc", "cache=false")], &plain)?.is_some());
    assert!(disabled(&[("home/.npmrc", "cache=false")], &xdg)?.is_some());
    let both = [("xdg/.npmrc", "registry=x"), ("home/.npmrc", "cache=false")];
    assert_eq!(disabled(&both, &xdg)?, None);
    Ok(())
}

/// A dotenv file Bun loads can move the configuration the probe reads, so setting one of those
/// variables there leaves the cache unproven.
#[test]
fn a_dotenv_file_can_move_the_configuration() -> eyre::Result<()> {
    let disabled = |files: &[(&str, &str)]| {
        let project = TempProject::new(files)?;
        eyre::Ok(cache_disabled(&project.root, &environment(&project, false)))
    };
    for (file, line) in [
        (".env", "npm_config_userconfig=./user.npmrc"),
        (".env.local", "export NPM_CONFIG_USERCONFIG=./user.npmrc"),
        (".env.production", "NPM_CONFIG_USERCONFIG: ./user.npmrc"),
        (
            ".env.test.local",
            "NPM_CONFIG_USERCONFIG = \"./user.npmrc\"",
        ),
    ] {
        let reason = disabled(&[(file, line)])?;
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with(file)),
            "{file}: {reason:?}"
        );
    }
    // Variables the process environment governs, and files Bun does not load
    let unrelated = "BUN_OPTIONS=--config=other.toml\nHOME=/elsewhere\nXDG_CONFIG_HOME=/x";
    assert_eq!(disabled(&[(".env", unrelated)])?, None);
    let example = "NPM_CONFIG_USERCONFIG=./user.npmrc";
    assert_eq!(
        disabled(&[(".env.example", example), (".envrc", example)])?,
        None
    );
    Ok(())
}

/// An enclosing directory that declares workspaces, or whose manifest is unreadable, may own the
/// project, while one without workspaces does not.
#[test]
fn an_enclosing_workspace_leaves_the_scope_unproven() -> eyre::Result<()> {
    let outer = TempProject::new(&[("app/package.json", "{}")])?;
    let root = outer.root.join("app");
    assert_eq!(enclosing_workspace(&root), None);

    outer.write("package.json", r#"{ "name": "outer" }"#)?;
    assert_eq!(enclosing_workspace(&root), None);

    outer.write("package.json", r#"{ "workspaces": ["app"] }"#)?;
    assert!(enclosing_workspace(&root).is_some());

    outer.write("package.json", "{ not json")?;
    assert!(enclosing_workspace(&root).is_some());
    Ok(())
}

/// Trusted and patched dependencies, and local folder or link dependencies, leave the lock
/// unproven; an empty patch list and workspace members do not.
#[test]
fn features_bun_does_not_compare_leave_the_lock_unproven() {
    let manifest = json!({ "name": "app" });
    let lock = json!({
        "workspaces": { "": { "name": "app" } },
        "packages": {
            "ms": ["ms@2.1.3", "", {}, "sha512-x"],
            "a": ["a@workspace:packages/a"],
        },
    });
    assert_eq!(unsupported_feature(&manifest, &lock), None);
    let no_patches = json!({ "name": "app", "patchedDependencies": {} });
    assert_eq!(unsupported_feature(&no_patches, &lock), None);

    for manifest in [
        json!({ "trustedDependencies": ["esbuild"] }),
        json!({ "patchedDependencies": { "ms@2.1.3": "patches/ms.patch" } }),
    ] {
        assert!(
            unsupported_feature(&manifest, &lock).is_some(),
            "{manifest}"
        );
    }
    let mut trusted = lock.clone();
    trusted["trustedDependencies"] = json!(["esbuild"]);
    assert!(unsupported_feature(&manifest, &trusted).is_some());

    for identity in ["x@file:libs/x", "y@link:y"] {
        let mut local = lock.clone();
        local["packages"]["x"] = json!([identity, {}]);
        let reason = unsupported_feature(&manifest, &local);
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains(identity)),
            "{reason:?}"
        );
    }
}

/// A workspace with members `packages/a` and `packages/b` and the lock Bun 1.4.5 wrote for it.
fn workspace() -> eyre::Result<(TempProject, Value)> {
    let project = TempProject::new(&[])?;
    project.write_json(
        "package.json",
        &json!({
            "name": "root",
            "private": true,
            "version": "1.0.0",
            "workspaces": ["packages/*"],
            "dependencies": { "ms": "2.1.3", "debug": "4.3.4" },
            "optionalDependencies": { "debug": "4.3.4" },
            "peerDependencies": { "semver": "^7" },
            "peerDependenciesMeta": {
                "semver": { "optional": true },
                "chalk": { "optional": true },
                "ansi": {},
            },
        }),
    )?;
    project.write_json(
        "packages/a/package.json",
        &json!({ "name": "a", "version": "1.0.0", "dependencies": { "ms": "^2.1.0" } }),
    )?;
    project.write_json(
        "packages/b/package.json",
        &json!({ "name": "b", "version": "2.0.0", "devDependencies": { "a": "workspace:*" } }),
    )?;
    let lock = json!({
        "lockfileVersion": 1,
        "workspaces": {
            "": {
                "name": "root",
                "optionalDependencies": { "debug": "4.3.4" },
                "dependencies": { "ms": "2.1.3" },
                "peerDependencies": { "chalk": "*", "semver": "^7" },
                "optionalPeers": ["chalk", "semver"],
            },
            "packages/a": { "name": "a", "version": "1.0.0", "dependencies": { "ms": "^2.1.0" } },
            "packages/b": { "name": "b", "version": "2.0.0", "devDependencies": { "a": "workspace:*" } },
        },
    });
    Ok((project, lock))
}

fn unproven(project: &TempProject, lock: &Value) -> eyre::Result<Option<String>> {
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(project.root.join("package.json"))?)?;
    Ok(unproven_by_bun(&project.root, &manifest, lock))
}

/// A lock recording every member and declaration in Bun's representation leaves Bun the verdict,
/// whatever the pattern skips.
#[test]
fn recorded_members_and_declarations_leave_bun_the_verdict() -> eyre::Result<()> {
    let (project, lock) = workspace()?;
    assert_eq!(unproven(&project, &lock)?, None);

    // Children `*` does not match: a dot directory, `node_modules`, a directory without a
    // manifest, and a file
    project.write("packages/.hidden/package.json", r#"{ "name": "hidden" }"#)?;
    project.write("packages/node_modules/x/package.json", r#"{ "name": "x" }"#)?;
    project.write("packages/empty/README.md", "")?;
    project.write("packages/file", "")?;
    assert_eq!(unproven(&project, &lock)?, None);

    // The same members through literal paths and spelling variants
    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(project.root.join("package.json"))?)?;
    manifest["workspaces"] = json!({ "packages": ["./packages/a/", "packages/b"] });
    assert_eq!(unproven_by_bun(&project.root, &manifest, &lock), None);
    Ok(())
}

/// Every member or declaration change Bun's frozen check can pass leaves the lock unproven.
#[test]
fn edits_bun_does_not_compare_leave_the_lock_unproven() -> eyre::Result<()> {
    let relock =
        |reason: Option<String>| reason.is_some_and(|reason| reason.contains("bun install"));

    // A new member named like the root's registry dependency
    let (project, lock) = workspace()?;
    project.write(
        "packages/ms/package.json",
        r#"{ "name": "ms", "version": "9.0.0" }"#,
    )?;
    assert!(relock(unproven(&project, &lock)?));

    // A removed member
    let (project, lock) = workspace()?;
    std::fs::remove_dir_all(project.root.join("packages/b"))?;
    assert!(relock(unproven(&project, &lock)?));

    // A member's version, and the root's name
    let (project, lock) = workspace()?;
    project.write_json(
        "packages/a/package.json",
        &json!({ "name": "a", "version": "1.0.1", "dependencies": { "ms": "^2.1.0" } }),
    )?;
    assert!(relock(unproven(&project, &lock)?));
    let (project, mut lock) = workspace()?;
    lock["workspaces"][""]["name"] = json!("renamed");
    assert!(relock(unproven(&project, &lock)?));

    // A declaration, and an optional peer
    let (project, mut lock) = workspace()?;
    lock["workspaces"]["packages/a"]["dependencies"]["ms"] = json!("^2.1.1");
    assert!(relock(unproven(&project, &lock)?));
    let (project, mut lock) = workspace()?;
    lock["workspaces"][""]["optionalPeers"] = json!(["semver"]);
    assert!(relock(unproven(&project, &lock)?));

    // Not the root's version, which Bun does not record
    let (project, mut lock) = workspace()?;
    lock["workspaces"][""]["version"] = json!("0.1.0");
    assert_eq!(unproven(&project, &lock)?, None);
    Ok(())
}

/// A member's row keeps a `dependencies` entry that its `optionalDependencies` repeats, so
/// removing that entry, which Bun's frozen check passes, leaves the lock unproven.
#[test]
fn a_members_shadowed_declaration_still_counts() -> eyre::Result<()> {
    let (project, mut lock) = workspace()?;
    let both = json!({
        "name": "a",
        "version": "1.0.0",
        "dependencies": { "ms": "2.1.3" },
        "optionalDependencies": { "ms": "2.1.3" },
    });
    project.write_json("packages/a/package.json", &both)?;
    lock["workspaces"]["packages/a"] = both;
    assert_eq!(unproven(&project, &lock)?, None);

    project.write_json(
        "packages/a/package.json",
        &json!({ "name": "a", "version": "1.0.0", "optionalDependencies": { "ms": "2.1.3" } }),
    )?;
    let reason = unproven(&project, &lock)?;
    assert!(
        reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("packages/a/package.json differs")),
        "{reason:?}"
    );
    Ok(())
}

/// Bun's frozen mode does not compare a member's `trustedDependencies`, so a member listing any
/// leaves the lock unproven.
#[test]
fn a_member_trusting_dependencies_leaves_the_lock_unproven() -> eyre::Result<()> {
    let (project, lock) = workspace()?;
    project.write_json(
        "packages/a/package.json",
        &json!({
            "name": "a",
            "version": "1.0.0",
            "dependencies": { "ms": "^2.1.0" },
            "trustedDependencies": ["ms"],
        }),
    )?;
    let reason = unproven(&project, &lock)?;
    assert!(
        reason
            .as_deref()
            .is_some_and(|reason| reason.contains("trustedDependencies")),
        "{reason:?}"
    );
    Ok(())
}

/// A workspace pattern whose membership the probe cannot establish, or a lock member outside the
/// project, leaves the lock unproven.
#[test]
fn unsupported_patterns_leave_the_lock_unproven() -> eyre::Result<()> {
    let (project, lock) = workspace()?;
    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(project.root.join("package.json"))?)?;
    for pattern in [
        "packages/**",
        "packages/a*",
        "pack*/a",
        "!packages/b",
        "{packages}/*",
        "../outside",
        ".",
        "",
        "/abs",
    ] {
        manifest["workspaces"] = json!(["packages/*", pattern]);
        let reason = unproven_by_bun(&project.root, &manifest, &lock);
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("unsupported workspace pattern")),
            "{pattern}: {reason:?}"
        );
    }
    manifest["workspaces"] = json!("packages/*");
    assert!(unproven_by_bun(&project.root, &manifest, &lock).is_some());

    let (project, mut lock) = workspace()?;
    lock["workspaces"]["../outside"] = json!({ "name": "outside" });
    assert!(unproven(&project, &lock)?.is_some());
    Ok(())
}

/// A scripted `bun` and the project it runs in, with every invocation logged.
#[cfg(unix)]
struct FakeBun {
    project: TempProject,
    cmd: crate::nodecmd::NodeCmd,
    scratch: TempProject,
}

#[cfg(unix)]
impl FakeBun {
    /// A Bun reporting `revision`, and answering `install` as `install` does, in the project with
    /// `package.json` and `bun.lock` as given.
    fn new(manifest: &Value, lock: &Value, revision: &str, install: &str) -> eyre::Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;
        let project = TempProject::new(&[])?;
        project.write_json("package.json", manifest)?;
        project.write_json("bun.lock", lock)?;
        let scratch = TempProject::new(&[])?;
        let script = scratch.root.join("fake-bun.sh");
        std::fs::write(
            &script,
            indoc::formatdoc! {r#"
                #!/bin/sh
                echo "$* cwd=$(pwd) pwd=$PWD options=[$BUN_OPTIONS] cache=$BUN_INSTALL_CACHE_DIR tmp=$TMPDIR" >> "{log}"
                case "$1" in
                  --revision) echo "{revision}" ;;
                  install) {install} ;;
                  *) echo "unexpected: $*" >&2; exit 1 ;;
                esac
            "#, log = scratch.root.join("calls.log")},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        Ok(FakeBun {
            cmd: crate::nodecmd::NodeCmd::with_bin(script.as_str()),
            project,
            scratch,
        })
    }

    async fn verify(
        &self,
        offline: bool,
    ) -> cooldown_core::Result<cooldown_core::LockVerifyReport> {
        let environment = Environment {
            home: Some(self.scratch.root.join("home")),
            ..Environment::default()
        };
        super::lock_current_in(
            &self.cmd,
            &self.project.root,
            &self.scratch.root.join("bun-verify"),
            offline,
            &environment,
        )
        .await
    }

    /// Every logged invocation.
    fn calls(&self) -> eyre::Result<Vec<String>> {
        match std::fs::read_to_string(self.scratch.root.join("calls.log")) {
            Ok(log) => Ok(log.lines().map(str::to_string).collect()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(err.into()),
        }
    }
}

#[cfg(unix)]
fn in_sync() -> (Value, Value) {
    (
        json!({ "name": "app", "dependencies": { "ms": "2.1.3" } }),
        json!({
            "lockfileVersion": 1,
            "workspaces": { "": { "name": "app", "dependencies": { "ms": "2.1.3" } } },
            "packages": { "ms": ["ms@2.1.3", "", {}, "sha512-x"] },
        }),
    )
}

#[cfg(unix)]
const PASS: &str = "echo 'Checked 1 installs across 2 packages (no changes)'";

/// Bun's exit 0 proves an in-sync lock current, with Bun run in the project under the isolated
/// environment, while a qualification the probe cannot establish is unknown before Bun installs.
#[cfg(unix)]
#[tokio::test]
async fn the_verdict_needs_every_qualification() -> eyre::Result<()> {
    use cooldown_core::LockStatus;
    let (manifest, lock) = in_sync();

    // Current, offline too
    let bun = FakeBun::new(&manifest, &lock, "1.4.5+a7733435c", PASS)?;
    let report = bun.verify(true).await?;
    assert_eq!(report.status, LockStatus::Current, "{}", report.detail);
    let calls = bun.calls()?;
    let cache = bun.scratch.root.join("bun-verify/cache");
    let root = &bun.project.root;
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls
            .iter()
            .all(|call| call.contains(&format!("cwd={root} pwd={root} options=[] cache={cache}")))
    );
    assert!(
        calls[1].starts_with(
            "install --frozen-lockfile --dry-run --ignore-scripts --no-progress --offline "
        ),
        "{calls:?}"
    );

    // A Bun below the floor, which never installs
    let bun = FakeBun::new(&manifest, &lock, "1.4.4+abc", PASS)?;
    let report = bun.verify(false).await?;
    assert_eq!(report.status, LockStatus::Unknown);
    assert!(report.detail.contains("Bun >= 1.4.5"), "{}", report.detail);
    assert_eq!(bun.calls()?.len(), 1);

    // A disabled cache, which never runs Bun at all
    let bun = FakeBun::new(&manifest, &lock, "1.4.5", PASS)?;
    bun.project
        .write("bunfig.toml", "[install.cache]\ndisable = true\n")?;
    assert_eq!(bun.verify(false).await?.status, LockStatus::Unknown);
    assert_eq!(bun.calls()?, Vec::<String>::new());

    // No text lock, only a binary one
    let bun = FakeBun::new(&manifest, &lock, "1.4.5", PASS)?;
    std::fs::rename(
        bun.project.root.join("bun.lock"),
        bun.project.root.join("bun.lockb"),
    )?;
    assert_eq!(bun.verify(false).await?.status, LockStatus::Unknown);
    assert_eq!(bun.calls()?, Vec::<String>::new());

    // A declaration Bun's exit 0 does not compare
    let mut edited = manifest.clone();
    edited["peerDependenciesMeta"] = json!({ "semver": { "optional": true } });
    let bun = FakeBun::new(&edited, &lock, "1.4.5", PASS)?;
    assert_eq!(bun.verify(false).await?.status, LockStatus::Unknown);
    assert_eq!(bun.calls()?.len(), 2);

    // A lock rewritten while Bun ran
    let rewrite = "echo '{}' > bun.lock";
    let bun = FakeBun::new(&manifest, &lock, "1.4.5", rewrite)?;
    let report = bun.verify(false).await?;
    assert_eq!(report.status, LockStatus::Unknown);
    assert!(report.detail.contains("changed while"), "{}", report.detail);

    // A cache or temporary directory that links into the project, which Bun would write through,
    // whether the link's target exists yet or not
    for (child, target) in [("cache", ""), ("tmp", "node_modules/.cache")] {
        let bun = FakeBun::new(&manifest, &lock, "1.4.5", PASS)?;
        let scratch = bun.scratch.root.join("bun-verify");
        std::fs::create_dir_all(&scratch)?;
        std::os::unix::fs::symlink(bun.project.root.join(target), scratch.join(child))?;
        let report = bun.verify(false).await?;
        assert_eq!(report.status, LockStatus::Unknown, "{child}");
        assert!(
            report.detail.contains("leads into the project"),
            "{}",
            report.detail
        );
        assert_eq!(bun.calls()?, Vec::<String>::new());
    }
    Ok(())
}

/// The fake's `install` printing `report` to standard error and exiting with `code`.
#[cfg(unix)]
fn failing(report: &str, code: u8) -> String {
    format!("cat >&2 <<'EOF'\n{report}EOF\nexit {code}")
}

/// Bun's lone frozen refusal is stale, a diagnosed success is unknown, and every other failure is
/// the tool's error.
#[cfg(unix)]
#[tokio::test]
async fn only_the_frozen_refusal_is_stale() -> eyre::Result<()> {
    use cooldown_core::{CoreError, LockStatus};
    let (manifest, lock) = in_sync();

    let bun = FakeBun::new(&manifest, &lock, "1.4.5", &failing(STALE, 1))?;
    let report = bun.verify(false).await?;
    assert_eq!(report.status, LockStatus::Stale);
    assert!(
        report
            .detail
            .starts_with(&format!("bun.lock is stale in {}", bun.project.root))
            && report.detail.contains("ms changed from 2.1.3 to 2.1.2"),
        "{}",
        report.detail
    );

    let bun = FakeBun::new(&manifest, &lock, "1.4.5", &failing(IGNORED, 1))?;
    assert!(matches!(
        bun.verify(false).await,
        Err(CoreError::Tool { .. })
    ));

    let ignored = failing("warn: Ignoring lockfile\n", 0);
    let bun = FakeBun::new(&manifest, &lock, "1.4.5", &ignored)?;
    assert_eq!(bun.verify(false).await?.status, LockStatus::Unknown);

    let bun = FakeBun::new(
        &manifest,
        &lock,
        "1.4.5",
        &failing("error: 404 Not Found\n", 1),
    )?;
    assert!(matches!(
        bun.verify(false).await,
        Err(CoreError::Tool { .. })
    ));

    // A revision Bun cannot print
    let bun = FakeBun::new(&manifest, &lock, "1.4.5", PASS)?;
    let broken = crate::nodecmd::NodeCmd::with_bin(bun.scratch.root.join("missing").as_str());
    let environment = Environment::default();
    let result = super::lock_current_in(
        &broken,
        &bun.project.root,
        &bun.scratch.root.join("bun-verify"),
        false,
        &environment,
    )
    .await;
    assert!(result.is_err());
    Ok(())
}
