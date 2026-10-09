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
            sleep 300 &
            echo $! > "$(dirname "$0")/descendant"
            wait
        "#},
    )?;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;

    let mut cooldown = Command::new(env!("CARGO_BIN_EXE_cooldown"))
        .current_dir(root)
        .env("COOLDOWN_PNPM", &fake)
        .args(["check", "--tool", "pnpm", "--offline", "--no-progress"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    // Interrupt only once the package manager and its descendant are running.
    let started = Instant::now();
    while !root.join("descendant").exists() {
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
    Ok(())
}
