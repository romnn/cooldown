//! Exercises what `check --allow-stale-lock` waives when pnpm's frozen verification of the lock
//! fails: pnpm's own stale-lock refusal, and nothing else.
#![cfg(unix)]

use color_eyre::eyre;
use indoc::{formatdoc, indoc};
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

/// A pnpm project whose `pnpm` is a script answering the frozen verification with `verification`
/// (a shell snippet) and every configuration query with `null`, anchored as its own Git
/// repository so no ambient config above the temporary directory is read.
fn pnpm_project(verification: &str) -> eyre::Result<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    std::fs::create_dir_all(root.join(".git"))?;
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "app", "dependencies": { "dep": "^1.0.0" } }"#,
    )?;
    std::fs::write(root.join("pnpm-workspace.yaml"), "packages: []\n")?;
    std::fs::write(
        root.join("pnpm-lock.yaml"),
        indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  dep:
                    specifier: ^1.0.0
                    version: 1.0.0

            packages:

              dep@1.0.0:
                resolution: {integrity: sha512-a}
        "},
    )?;
    let fake = root.join("fake-pnpm.sh");
    std::fs::write(
        &fake,
        formatdoc! {r#"
            #!/bin/sh
            case "$*" in
              *"--frozen-lockfile"*)
                {verification} ;;
            esac
            echo 'null'
        "#},
    )?;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
    Ok(directory)
}

/// Runs `cooldown check --allow-stale-lock` on the project in `directory`, returning the exit code
/// and the JSON envelope.
fn check_allowing_stale_lock(
    directory: &tempfile::TempDir,
) -> eyre::Result<(Option<i32>, serde_json::Value)> {
    // An empty cache keeps `--offline` from answering any release, so the run stays hermetic.
    let cache = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_cooldown"))
        .current_dir(directory.path())
        .env("XDG_CACHE_HOME", cache.path())
        .env("COOLDOWN_PNPM", directory.path().join("fake-pnpm.sh"))
        .args([
            "check",
            "--tool",
            "pnpm",
            "--offline",
            "--allow-stale-lock",
            "--json",
            "--no-progress",
        ])
        .output()?;
    let document = serde_json::from_slice(&output.stdout).map_err(|err| {
        eyre::eyre!(
            "cooldown check printed no JSON envelope ({err}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    Ok((output.status.code(), document))
}

/// The diagnostic kinds in the envelope's `section` array, in order.
fn kinds(document: &serde_json::Value, section: &str) -> Vec<String> {
    document
        .get(section)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("kind").and_then(serde_json::Value::as_str))
        .map(ToString::to_string)
        .collect()
}

/// pnpm's refusal of a stale lock is what `--allow-stale-lock` waives: the run passes with a
/// stale-lock warning and evaluates nothing.
#[test]
fn allow_stale_lock_waives_pnpms_stale_lock_refusal() -> eyre::Result<()> {
    let project = pnpm_project(indoc! {r#"
        echo '[ERR_PNPM_OUTDATED_LOCKFILE] Cannot install with "frozen-lockfile" because pnpm-lock.yaml is not up to date with <ROOT>/package.json'
        exit 1
    "#})?;

    let (code, document) = check_allowing_stale_lock(&project)?;

    assert_eq!(code, Some(0), "{document:#}");
    assert_eq!(kinds(&document, "errors"), Vec::<String>::new());
    assert_eq!(kinds(&document, "warnings"), ["stale_lock"], "{document:#}");
    Ok(())
}

/// A verification that failed for any other reason is a tool error even under
/// `--allow-stale-lock`: the lock pnpm never confirmed is not evaluated and the run fails.
#[test]
fn allow_stale_lock_never_waives_other_pnpm_failures() -> eyre::Result<()> {
    let failures = [
        // Network
        "echo '[ERR_PNPM_META_FETCH_FAIL] GET https://registry.npmjs.org/dep: request failed, reason: getaddrinfo ENOTFOUND registry.npmjs.org'; exit 1",
        // Authentication
        "echo '[ERR_PNPM_FETCH_401] GET https://registry.npmjs.org/dep: Unauthorized - 401'; exit 1",
        // Unsupported flag
        "echo \"[ERROR] Unknown option: 'reporter'\" >&2; exit 1",
    ];
    for failure in failures {
        let project = pnpm_project(failure)?;

        let (code, document) = check_allowing_stale_lock(&project)?;

        assert_ne!(code, Some(0), "{failure}: {document:#}");
        assert_eq!(
            kinds(&document, "errors"),
            ["tool_failed"],
            "{failure}: {document:#}"
        );
        assert!(
            !kinds(&document, "warnings").contains(&"stale_lock".to_string()),
            "{failure}: {document:#}"
        );
    }
    Ok(())
}
