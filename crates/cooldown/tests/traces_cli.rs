//! Default diagnostic recording, isolated storage, and result-preserving failure behavior.

use color_eyre::eyre;
use indoc::indoc;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(root: &Path, config: &Path, args: &[&str]) -> eyre::Result<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_cooldown"))
        .current_dir(root)
        .env_remove("COOLDOWN_NO_TRACE")
        .env_remove("COOLDOWN_TRACE_DIR")
        .env_remove("COOLDOWN_FULL_TRACE")
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("COOLDOWN_NPM", root.join("missing-npm"))
        .args(args)
        .output()?)
}

fn traces(config: &Path) -> eyre::Result<Vec<PathBuf>> {
    let directory = config.join("cooldown").join("traces");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    Ok(std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()?)
}

/// Every record of a finished trace, which must not be cut short.
fn records(path: &Path) -> eyre::Result<Vec<Value>> {
    let mut records = cooldown::trace::records(path)?;
    let read = records.by_ref().collect::<Result<Vec<_>, _>>()?;
    eyre::ensure!(
        !records.truncated(),
        "trace {} is truncated",
        path.display()
    );
    Ok(read)
}

#[test]
fn default_trace_keeps_json_stdout_and_captures_setup_failure() -> eyre::Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join(".git"))?;
    let config = root.path().join("config");
    let output = run(
        root.path(),
        &config,
        &["outdated", "--json", "--no-progress"],
    )?;
    assert_eq!(
        output.status.code(),
        Some(cooldown::app::Exit::NoTool.code()),
        "{output:?}"
    );
    let _: Value = serde_json::from_slice(&output.stdout)?;
    assert!(String::from_utf8_lossy(&output.stderr).contains("trace recorded at"));
    let files = traces(&config)?;
    assert_eq!(files.len(), 1);
    let file = files.first().ok_or_else(|| eyre::eyre!("missing trace"))?;
    // The trace is a zstd stream, compact unless a full trace is requested.
    assert!(
        file.to_string_lossy().ends_with(".jsonl.zst"),
        "{}",
        file.display()
    );
    assert!(std::fs::read(file)?.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]));
    let events = records(file)?;
    assert_eq!(
        events.first().map(|event| &event["event"]["kind"]),
        Some(&Value::from("run_started"))
    );
    assert_eq!(
        events.first().map(|event| &event["event"]["detail"]),
        Some(&Value::from("compact"))
    );
    assert_eq!(
        events.last().map(|event| &event["event"]["kind"]),
        Some(&Value::from("run_finished"))
    );
    let output = run(root.path(), &config, &["outdated", "--min-age", "invalid"])?;
    assert!(!output.status.success());
    let files = traces(&config)?;
    assert_eq!(files.len(), 2);
    assert!(
        files
            .iter()
            .map(|path| records(path))
            .collect::<eyre::Result<Vec<_>>>()?
            .iter()
            .any(|events| events.last().is_some_and(
                |event| event["event"]["exitCode"] != 0 && event["event"]["error"].is_string()
            ))
    );
    Ok(())
}

#[test]
fn opt_out_and_storage_failure_preserve_command_result() -> eyre::Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join(".git"))?;
    let config = root.path().join("config");
    let output = run(root.path(), &config, &["outdated", "--json", "--no-trace"])?;
    assert_eq!(
        output.status.code(),
        Some(cooldown::app::Exit::NoTool.code())
    );
    assert_eq!(traces(&config)?, Vec::<PathBuf>::new());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("trace recorded"));
    let blocked = root.path().join("blocked-trace-directory");
    std::fs::write(&blocked, "a file cannot contain a trace directory")?;
    let output = run(
        root.path(),
        &config,
        &[
            "outdated",
            "--json",
            "--trace-dir",
            blocked
                .to_str()
                .ok_or_else(|| eyre::eyre!("UTF-8 path expected"))?,
        ],
    )?;
    assert_eq!(
        output.status.code(),
        Some(cooldown::app::Exit::NoTool.code())
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot record run trace"));
    let _: Value = serde_json::from_slice(&output.stdout)?;
    Ok(())
}

#[test]
fn baseline_binds_the_exact_native_graph_and_preserves_input_bytes() -> eyre::Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join(".git"))?;
    std::fs::write(
        root.path().join("package.json"),
        r#"{"name":"sample","version":"1.0.0"}"#,
    )?;
    let lock = r#"{"name":"sample","lockfileVersion":3,"packages":{"":{"name":"sample","version":"1.0.0"}}}"#;
    std::fs::write(root.path().join("package-lock.json"), lock)?;
    let config = root.path().join("config");
    let output = run(
        root.path(),
        &config,
        &[
            "outdated",
            "--offline",
            "--json",
            "--no-progress",
            "--no-advisories",
        ],
    )?;
    assert!(output.status.success(), "{output:?}");
    let _: Value = serde_json::from_slice(&output.stdout)?;
    let files = traces(&config)?;
    let events = records(files.first().ok_or_else(|| eyre::eyre!("missing trace"))?)?;
    assert!(
        events
            .iter()
            .any(|record| record["event"]["kind"] == "snapshot_started"
                && record["event"]["phase"] == "initial")
    );
    let graph = events
        .iter()
        .find(|record| {
            record["event"]["kind"] == "artifact_started"
                && record["event"]["source"]["path"]
                    .as_str()
                    .is_some_and(|path| {
                        Path::new(path).canonicalize().ok()
                            == root.path().join("package-lock.json").canonicalize().ok()
                    })
        })
        .ok_or_else(|| eyre::eyre!("graph artifact missing"))?;
    let artifact = &graph["event"]["artifact"];
    assert!(
        events
            .iter()
            .any(|record| &record["event"]["artifact"] == artifact
                && record["event"]["kind"] == "artifact_chunk"
                && record["event"]["content"]["data"] == lock)
    );
    assert!(
        events
            .iter()
            .any(|record| record["event"]["kind"] == "tool_version")
    );
    Ok(())
}

/// One offline `outdated` evaluation of a single npm dependency with three releases, recorded at
/// the given trace detail.
fn offline_evaluation(detail_args: &[&str]) -> eyre::Result<OfflineEvaluation> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join(".git"))?;
    std::fs::write(
        root.path().join("package.json"),
        r#"{"name":"sample","dependencies":{"dep":"^1.0.0"}}"#,
    )?;
    let lock = serde_json::json!({
        "name": "sample", "lockfileVersion": 3,
        "packages": {
            "": {"name": "sample", "dependencies": {"dep": "^1.0.0"}},
            "node_modules/dep": {"version": "1.0.0", "resolved": "https://registry.npmjs.org/dep/-/dep-1.0.0.tgz"},
        },
    });
    std::fs::write(
        root.path().join("package-lock.json"),
        serde_json::to_vec(&lock)?,
    )?;
    let metadata = serde_json::json!({
        "name": "dep", "dist-tags": {"latest": "2.0.0"},
        "time": {"1.0.0": "2020-01-01T00:00:00Z", "1.1.0": "2020-02-01T00:00:00Z", "2.0.0": "2020-03-01T00:00:00Z"},
        "versions": {"1.0.0": {"version": "1.0.0"}, "1.1.0": {"version": "1.1.0"}, "2.0.0": {"version": "2.0.0"}},
    });
    cooldown_registry::cache::write_entry(
        &root.path().join("cache").join("cooldown"),
        &cooldown_registry::cache::CacheEntry {
            url: "https://registry.npmjs.org/dep".into(),
            fetched_at: jiff::Timestamp::now().to_string(),
            etag: None,
            status: 200,
            body: metadata.to_string(),
            request_body: None,
        },
    )?;
    let config = root.path().join("config");
    let mut args = vec![
        "outdated",
        "--offline",
        "--no-advisories",
        "--json",
        "--no-progress",
    ];
    args.extend_from_slice(detail_args);
    let output = run(root.path(), &config, &args)?;
    eyre::ensure!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout)?;
    let files = traces(&config)?;
    let events = records(files.first().ok_or_else(|| eyre::eyre!("missing trace"))?)?;
    Ok(OfflineEvaluation { report, events })
}

struct OfflineEvaluation {
    report: Value,
    events: Vec<Value>,
}

impl OfflineEvaluation {
    fn events(&self, kind: &str) -> Vec<&Value> {
        self.events
            .iter()
            .filter_map(|record| record.get("event"))
            .filter(|event| event.get("kind").is_some_and(|value| value == kind))
            .collect()
    }

    /// The upgrade evaluation of `dep`, whose release list includes the locked 1.0.0.
    fn evaluation(&self) -> eyre::Result<&Value> {
        self.events("evaluation")
            .into_iter()
            .find(|event| {
                event
                    .pointer("/decision/kind")
                    .is_some_and(|value| value == "upgrade")
            })
            .ok_or_else(|| eyre::eyre!("missing upgrade evaluation"))
    }
}

#[test]
fn offline_evaluation_keeps_metadata_filter_reasons_and_the_printed_report() -> eyre::Result<()> {
    let compact = offline_evaluation(&[])?;
    let full = offline_evaluation(&["--full-trace"])?;
    for trace in [&compact, &full] {
        assert!(
            trace
                .events
                .iter()
                .any(|record| record["event"]["kind"] == "artifact_started"
                    && record["event"]["source"]["kind"] == "http"
                    && record["event"]["source"]["fromCache"] == true)
        );
        assert!(
            trace
                .events("report")
                .iter()
                .any(|event| event["report"] == trace.report)
        );
        for (sequence, record) in trace.events.iter().enumerate() {
            assert_eq!(record["sequence"].as_u64(), Some(u64::try_from(sequence)?));
            assert!(
                record["timestamp"]
                    .as_str()
                    .is_some_and(|value| value.parse::<jiff::Timestamp>().is_ok())
            );
        }
    }
    let compact_evaluation = compact.evaluation()?;
    let full_evaluation = full.evaluation()?;

    // Both details state why each release was rejected, in the evaluation itself.
    assert!(
        compact_evaluation["filtered"]
            .as_array()
            .is_some_and(|filtered| filtered
                .iter()
                .any(|release| release["version"] == "1.0.0"
                    && release["reason"] == "current_or_older"))
    );
    assert_eq!(compact_evaluation["filtered"], full_evaluation["filtered"]);

    // A compact trace records the release list once, before the evaluation that refers to it by
    // hash, and no per-release rejection records.
    let sets = compact.events("release_set");
    let [set] = sets.as_slice() else {
        eyre::bail!("one release set expected, got {sets:?}");
    };
    assert_eq!(compact_evaluation["releaseSet"], set["sha256"]);
    assert!(compact_evaluation.get("releases").is_none());
    assert_eq!(compact.events("release_filtered"), Vec::<&Value>::new());

    // A full trace inlines the same release list and adds one record per rejected release.
    assert_eq!(full_evaluation["releases"], set["releases"]);
    assert!(full_evaluation.get("releaseSet").is_none());
    assert_eq!(full.events("release_set"), Vec::<&Value>::new());
    assert!(
        full.events("release_filtered")
            .iter()
            .any(|event| event["reason"] == "current_or_older")
    );
    assert_eq!(
        full.events("run_started")
            .first()
            .map(|event| &event["detail"]),
        Some(&Value::from("full"))
    );
    Ok(())
}

#[test]
fn directory_override_and_command_prefix_do_not_affect_other_commands() -> eyre::Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join(".git"))?;
    let config = root.path().join("config");
    let custom = root.path().join("custom-traces");
    for command in ["upgrade", "fix", "outdated"] {
        let output = run(
            root.path(),
            &config,
            &[
                command,
                "--trace-dir",
                custom
                    .to_str()
                    .ok_or_else(|| eyre::eyre!("UTF-8 path expected"))?,
            ],
        )?;
        assert_eq!(
            output.status.code(),
            Some(cooldown::app::Exit::NoTool.code())
        );
        assert!(
            std::fs::read_dir(&custom)?.any(|entry| entry.is_ok_and(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{command}-"))))
        );
    }
    assert_eq!(traces(&config)?, Vec::<PathBuf>::new());
    let output = run(root.path(), &config, &["schema"])?;
    assert!(output.status.success());
    assert_eq!(traces(&config)?, Vec::<PathBuf>::new());
    let output = run(
        root.path(),
        &config,
        &["outdated", "--trace-dir", "disabled", "--no-trace"],
    )?;
    assert_eq!(
        output.status.code(),
        Some(cooldown::app::Exit::NoTool.code())
    );
    assert!(!root.path().join("disabled").exists());
    let output = run(
        root.path(),
        &config,
        &[
            "prune",
            "--trace-dir",
            custom
                .to_str()
                .ok_or_else(|| eyre::eyre!("UTF-8 path expected"))?,
            "--older-than",
            "0s",
            "--dry-run",
            "--json",
        ],
    )?;
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["summary"]["traces"], 3);
    assert_eq!(std::fs::read_dir(&custom)?.count(), 3);
    Ok(())
}

#[test]
fn trace_directory_environment_and_relative_flag_obey_precedence() -> eyre::Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join(".git"))?;
    let from_environment = root.path().join("environment");
    for args in [
        vec!["outdated"],
        vec!["outdated", "--trace-dir", "relative-flag"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cooldown"))
            .current_dir(root.path())
            .env("COOLDOWN_NO_TRACE", "false")
            .env("COOLDOWN_TRACE_DIR", &from_environment)
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .args(args)
            .output()?;
        assert_eq!(
            output.status.code(),
            Some(cooldown::app::Exit::NoTool.code())
        );
    }
    assert_eq!(std::fs::read_dir(from_environment)?.count(), 1);
    assert_eq!(
        std::fs::read_dir(root.path().join("relative-flag"))?.count(),
        1
    );
    Ok(())
}

#[test]
fn uv_trace_preserves_ambient_routing_evidence() -> eyre::Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join(".git"))?;
    std::fs::write(
        root.path().join("pyproject.toml"),
        indoc! {"
        [project]
        name = 'sample'
        version = '1.0.0'
    "},
    )?;
    std::fs::write(root.path().join("uv.lock"), "version = 1\n")?;
    let explicit = root.path().join("external-uv.toml");
    let content = "index-url = 'https://private.invalid/simple'\n";
    std::fs::write(&explicit, content)?;
    let config = root.path().join("config");
    let output = Command::new(env!("CARGO_BIN_EXE_cooldown"))
        .current_dir(root.path())
        .env_remove("COOLDOWN_TRACE_DIR")
        .env("COOLDOWN_NO_TRACE", "false")
        .env("XDG_CONFIG_HOME", &config)
        .env("UV_CONFIG_FILE", &explicit)
        .env("UV_NO_INDEX", "1")
        .env("COOLDOWN_UV", root.path().join("missing-uv"))
        .args(["outdated", "--offline", "--no-advisories", "--no-progress"])
        .output()?;
    let files = traces(&config)?;
    let events = records(
        files
            .first()
            .ok_or_else(|| eyre::eyre!("missing uv trace: {output:?}"))?,
    )?;
    assert!(events.iter().any(|record| {
        record["event"]["evidence"]["environment"]
            .as_array()
            .is_some_and(|values| {
                values
                    .iter()
                    .any(|value| value["name"] == "UV_NO_INDEX" && value["value"] == "1")
            })
    }));
    assert!(events.iter().any(|record| {
        record["event"]["source"]["path"]
            .as_str()
            .is_some_and(|path| Path::new(path).canonicalize().ok() == explicit.canonicalize().ok())
    }));
    assert!(
        events
            .iter()
            .any(|record| record["event"]["content"]["data"] == content)
    );
    Ok(())
}

#[test]
fn uv_relative_configuration_preserves_both_routing_and_native_inputs() -> eyre::Result<()> {
    let root = tempfile::tempdir()?;
    let working = root.path().join("repository");
    let project = working.join("packages").join("app");
    std::fs::create_dir_all(&project)?;
    std::fs::create_dir(working.join(".git"))?;
    std::fs::write(
        project.join("pyproject.toml"),
        indoc! {"
        [project]
        name = 'app'
        version = '1.0.0'
    "},
    )?;
    std::fs::write(project.join("uv.lock"), "version = 1\n")?;
    let routing = root.path().join("native-settings.toml");
    let native = working.join("packages").join("native-settings.toml");
    std::fs::write(&routing, "index-url = 'https://routing.invalid/simple'\n")?;
    std::fs::write(&native, "index-url = 'https://native.invalid/simple'\n")?;
    let config = root.path().join("config");
    let output = Command::new(env!("CARGO_BIN_EXE_cooldown"))
        .current_dir(&working)
        .env_remove("COOLDOWN_TRACE_DIR")
        .env("COOLDOWN_NO_TRACE", "false")
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("UV_CONFIG_FILE", "../native-settings.toml")
        .env("COOLDOWN_UV", root.path().join("missing-uv"))
        .args([
            "outdated",
            "--dir",
            "packages/app",
            "--offline",
            "--no-advisories",
            "--no-progress",
        ])
        .output()?;
    let files = traces(&config)?;
    let events = records(
        files
            .first()
            .ok_or_else(|| eyre::eyre!("missing trace: {output:?}"))?,
    )?;
    for expected in [routing, native] {
        let canonical = expected.canonicalize()?;
        assert!(events.iter().any(|record| {
            record["event"]["source"]["path"]
                .as_str()
                .is_some_and(|path| {
                    Path::new(path).canonicalize().ok().as_ref() == Some(&canonical)
                })
        }));
        let content = std::fs::read_to_string(expected)?;
        assert!(
            events
                .iter()
                .any(|record| record["event"]["content"]["data"] == content)
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn automatic_sync_failure_keeps_its_structured_diagnostics() -> eyre::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join(".git"))?;
    std::fs::write(root.path().join("package.json"), r#"{"name":"sample"}"#)?;
    std::fs::write(
        root.path().join("pnpm-lock.yaml"),
        indoc! {"
        lockfileVersion: '9.0'
        importers:
          .: {}
        packages: {}
    "},
    )?;
    let fake = root.path().join("pnpm");
    // Setup reads a missing native configuration; the version query makes its later write fail.
    std::fs::write(
        &fake,
        indoc! {r#"
        #!/bin/sh
        mkdir "$(dirname "$0")/pnpm-workspace.yaml"
        echo 10.0.0
    "#},
    )?;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
    let config = root.path().join("config");
    let output = Command::new(env!("CARGO_BIN_EXE_cooldown"))
        .current_dir(root.path())
        .env_remove("COOLDOWN_TRACE_DIR")
        .env("COOLDOWN_NO_TRACE", "false")
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("COOLDOWN_PNPM", &fake)
        .args([
            "outdated",
            "--sync",
            "--offline",
            "--no-advisories",
            "--no-progress",
        ])
        .output()?;
    assert!(!output.status.success());
    let files = traces(&config)?;
    let events = records(files.first().ok_or_else(|| eyre::eyre!("missing trace"))?)?;
    let sync = events
        .iter()
        .find(|record| {
            record["event"]["kind"] == "report"
                && record["event"]["report"]["phase"] == "before_command"
        })
        .ok_or_else(|| eyre::eyre!("missing automatic sync report: {output:?}"))?;
    assert!(
        sync["event"]["report"]["items"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(|item| item["error"]["message"].is_string()))
    );
    Ok(())
}
