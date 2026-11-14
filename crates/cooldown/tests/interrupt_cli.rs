//! Exercises the process-level interruption contract: a signal stops the package manager cooldown
//! is running, with its whole process tree, and the run exits `130` promptly.
#![cfg(unix)]

use color_eyre::eyre;
use indoc::indoc;
use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A `SIGINT` sent to cooldown alone, as a supervisor or `kill` does, reaches a package manager
/// that is blocked with a background descendant holding its output pipes open, and the run ends
/// with the interrupted status instead of waiting on the pipes forever.
#[test]
fn a_signal_stops_the_package_manager_tree_and_exits_130() -> eyre::Result<()> {
    interrupted_command("check", Recording::None)?;
    interrupted_command("outdated", Recording::Compact)?;
    interrupted_command("outdated", Recording::Full)
}

/// Whether and how the interrupted command records a trace.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Recording {
    None,
    /// The default trace, which records native output once its stream ends.
    Compact,
    /// A full trace, which records native output as it arrives.
    Full,
}

fn interrupted_command(command: &str, recording: Recording) -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    // A `.git` marks the repository root, so discovery stays inside the temporary directory.
    std::fs::create_dir(root.join(".git"))?;
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
    // Every pnpm invocation announces itself and blocks on a background `sleep` that inherits the
    // captured output pipes, the shape that kept a wait on the direct child alone from returning.
    let fake = root.join("fake-pnpm.sh");
    std::fs::write(
        &fake,
        indoc! {r#"
            #!/bin/sh
            echo $$ > "$(dirname "$0")/pnpm-started"
            echo native-process-started
            sleep 300 &
            echo $! > "$(dirname "$0")/descendant"
            wait
        "#},
    )?;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;

    let mut cooldown = Command::new(env!("CARGO_BIN_EXE_cooldown"))
        .env("COOLDOWN_NO_TRACE", "false")
        .env_remove("COOLDOWN_TRACE_DIR")
        .env_remove("COOLDOWN_FULL_TRACE")
        .env("XDG_CONFIG_HOME", root.join("trace-config"))
        .current_dir(root)
        .env("COOLDOWN_PNPM", &fake)
        .args([command, "--tool", "pnpm", "--offline", "--no-progress"])
        .args((recording == Recording::Full).then_some("--full-trace"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    // Interrupt only once the package manager and its descendant are running.
    let started = Instant::now();
    // A full trace shows the package manager's output while it still runs.
    while !root.join("descendant").exists()
        || (recording == Recording::Full && !output_recorded(root)?)
    {
        if started.elapsed() > Duration::from_mins(1) {
            let _ = cooldown.kill();
            eyre::bail!("the fake package manager never started");
        }
        if let Some(status) = cooldown.try_wait()? {
            eyre::bail!("cooldown exited with {status} before running the package manager");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = i32::try_from(cooldown.id())?;
    // SAFETY: `kill` only sends a signal to the cooldown child this test spawned and still owns.
    #[expect(unsafe_code, reason = "sending a signal needs the libc call")]
    let sent = unsafe { libc::kill(pid, libc::SIGINT) };
    assert_eq!(sent, 0, "the signal reaches cooldown");

    // The run ends well inside the descendant's lifetime and reports the interruption.
    let interrupted = Instant::now();
    let status = loop {
        if let Some(status) = cooldown.try_wait()? {
            break status;
        }
        if interrupted.elapsed() > Duration::from_secs(30) {
            let _ = cooldown.kill();
            eyre::bail!("cooldown did not finish after the interruption");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(130), "an interrupted run exits 130");

    // The descendant went down with the package manager's process group.
    let descendant: i32 = std::fs::read_to_string(root.join("descendant"))?
        .trim()
        .parse()?;
    // SAFETY: signal 0 only probes whether the process still exists.
    #[expect(unsafe_code, reason = "probing a process needs the libc call")]
    let alive = unsafe { libc::kill(descendant, 0) } == 0;
    assert!(!alive, "no descendant of the package manager survives");
    if recording != Recording::None {
        assert_interrupted_trace(root)?;
    }
    Ok(())
}

fn assert_interrupted_trace(root: &std::path::Path) -> eyre::Result<()> {
    let traces = root.join("trace-config/cooldown/traces");
    let trace = std::fs::read_dir(traces)?
        .next()
        .ok_or_else(|| eyre::eyre!("missing interrupted trace"))??;
    let mut records = cooldown::trace::records(&trace.path())?;
    let records = {
        let read = records.by_ref().collect::<Result<Vec<_>, _>>()?;
        eyre::ensure!(
            !records.truncated(),
            "the interrupted run finished its trace"
        );
        read
    };
    let terminal = records.last().ok_or_else(|| eyre::eyre!("empty trace"))?;
    assert_eq!(
        terminal
            .pointer("/event/kind")
            .and_then(serde_json::Value::as_str),
        Some("run_finished")
    );
    assert_eq!(
        terminal
            .pointer("/event/exitCode")
            .and_then(serde_json::Value::as_u64),
        Some(130)
    );
    assert_eq!(
        terminal
            .pointer("/event/interrupted")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert!(records.iter().any(|record| {
        record
            .pointer("/event/kind")
            .is_some_and(|kind| kind == "command_finished")
    }));
    // Either detail keeps the output the package manager printed before the interruption.
    assert!(records.iter().any(|record| {
        record
            .pointer("/event/content/data")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|data| data.contains("native-process-started"))
    }));
    Ok(())
}

fn output_recorded(root: &std::path::Path) -> eyre::Result<bool> {
    let directory = root.join("trace-config").join("cooldown").join("traces");
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        // The run is still recording, so its trace ends at the last flushed block.
        for record in cooldown::trace::records(&entry?.path())? {
            if record?
                .pointer("/event/content/data")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|data| data.contains("native-process-started"))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
