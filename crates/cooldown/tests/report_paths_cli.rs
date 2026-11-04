//! Pins how the CLI spells the paths it reports as data: relative to the base the report names
//! and `/`-separated, never the checkout's absolute location.

use color_eyre::eyre;
use indoc::indoc;
use std::process::Command;

/// A repository anchored by its own `.git`, so no ambient config above the temporary directory is
/// read, with one project's files under `apps/web`.
///
/// The project sits below the scan root so a diagnostic's `project` (`apps/web`) and its
/// project-root-relative `path` cannot coincide.
fn repository(files: &[(&str, &str)]) -> eyre::Result<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    std::fs::create_dir_all(root.join(".git"))?;
    std::fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n")?;
    let project = root.join("apps").join("web");
    std::fs::create_dir_all(&project)?;
    for (name, contents) in files {
        std::fs::write(project.join(name), contents)?;
    }
    Ok(directory)
}

/// Runs cooldown at the repository root with `args` against an empty cache, returning the JSON
/// envelope.
fn cooldown_json(directory: &tempfile::TempDir, args: &[&str]) -> eyre::Result<serde_json::Value> {
    // An empty cache keeps `--offline` from answering any release, so the run stays hermetic.
    let cache = tempfile::tempdir()?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_cooldown"));
    // The user's own `COOLDOWN_*` settings and global config could narrow detection, such as a
    // `tool` filter that skips the fixture project, so neither reaches the run.
    // Windows matches variable names regardless of case, so the prefix is compared the same way.
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("COOLDOWN_")
        {
            command.env_remove(name);
        }
    }
    let output = command
        .current_dir(directory.path())
        .env("XDG_CACHE_HOME", cache.path())
        .args(args)
        .args(["--json", "--no-progress", "--no-global"])
        .output()?;
    serde_json::from_slice(&output.stdout).map_err(|err| {
        eyre::eyre!(
            "cooldown {args:?} printed no JSON envelope ({err}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// The `kind`, `project` and `path` of every error and warning in the envelope.
fn diagnostics(document: &serde_json::Value) -> Vec<[Option<String>; 3]> {
    ["errors", "warnings"]
        .into_iter()
        .filter_map(|section| document.get(section).and_then(serde_json::Value::as_array))
        .flatten()
        .map(|diagnostic| {
            ["kind", "project", "path"].map(|field| {
                diagnostic
                    .get(field)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
        })
        .collect()
}

fn diagnostic(kind: &str, project: &str, path: &str) -> [Option<String>; 3] {
    [kind, project, path].map(|field| Some(field.to_string()))
}

/// A Hex project whose lock cooldown reads but cannot prove current.
const MIX_EXS: &str = indoc! {r#"
    defmodule Web.MixProject do
      use Mix.Project
      def project, do: [app: :web, version: "0.1.0", deps: [{:jason, "~> 1.4"}]]
    end
"#};

const MIX_LOCK: &str = indoc! {r#"
    %{
      "jason": {:hex, :jason, "1.4.0", "e855647bc964a44e2f67df589ccf49105ae039d4179db7f6271dfd3843dc27e6", [:mix], [], "hexpm", "79a3791085b2a0f743ca04cec0f7be26443738779d09302e01318f97bdb82121"},
    }
"#};

/// A `check` warning about the project's lock names the manifest relative to the project root.
#[test]
fn check_reports_the_manifest_relative_to_the_project() -> eyre::Result<()> {
    let repository = repository(&[("mix.exs", MIX_EXS), ("mix.lock", MIX_LOCK)])?;

    let check = cooldown_json(&repository, &["check", "--offline"])?;

    // The Hex adapter cannot verify lock currency, so the run warns about the lock and points at
    // the manifest it read, spelled the same on every machine and platform.
    assert_eq!(
        diagnostics(&check),
        [diagnostic("lock_unknown", "apps/web", "mix.exs")],
        "check envelope: {check:#}"
    );
    Ok(())
}

/// An `outdated` error about an unreadable lock names the manifest relative to the project root.
#[test]
fn outdated_reports_the_manifest_relative_to_the_project() -> eyre::Result<()> {
    let pyproject = indoc! {r#"
        [project]
        name = "web"
        version = "0.1.0"
        dependencies = ["rich"]
    "#};
    let repository = repository(&[("pyproject.toml", pyproject), ("uv.lock", "garbage = [")])?;

    let outdated = cooldown_json(&repository, &["outdated", "--offline"])?;

    // The lock fails to parse before any release is looked up, and the error points at the
    // project's manifest rather than at the checkout's absolute location.
    assert_eq!(
        diagnostics(&outdated),
        [diagnostic(
            "lockfile_unreadable",
            "apps/web",
            "pyproject.toml"
        )],
        "outdated envelope: {outdated:#}"
    );
    Ok(())
}

/// A `sync` row names the native config it touched relative to its project, and `baseline` names
/// its file relative to the repository root.
#[test]
fn sync_and_baseline_report_relative_file_paths() -> eyre::Result<()> {
    let repository = repository(&[
        (
            "package.json",
            r#"{ "name": "web", "private": true, "packageManager": "pnpm@10.0.0" }"#,
        ),
        (
            "pnpm-lock.yaml",
            indoc! {"
                lockfileVersion: '9.0'
                importers:
                  .: {}
            "},
        ),
    ])?;
    std::fs::write(
        repository.path().join("cooldown.toml"),
        "min-age = \"7d\"\n",
    )?;

    let sync = cooldown_json(&repository, &["sync", "--dry-run"])?;
    let rows: Vec<_> = sync
        .get("items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| {
            ["project", "path"].map(|field| item.get(field).and_then(serde_json::Value::as_str))
        })
        .collect();
    // pnpm keeps its native window in the project's own workspace file.
    assert_eq!(
        rows,
        [[Some("apps/web"), Some("pnpm-workspace.yaml")]],
        "sync envelope: {sync:#}"
    );

    let baseline = cooldown_json(&repository, &["baseline", "--dry-run", "--offline"])?;
    assert_eq!(
        baseline.get("path").and_then(serde_json::Value::as_str),
        Some(".cooldown-baseline.toml"),
        "baseline envelope: {baseline:#}"
    );
    Ok(())
}
