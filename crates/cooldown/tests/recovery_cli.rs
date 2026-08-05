//! Exercises the process-level recovery CLI contract.

use color_eyre::eyre;
use indoc::indoc;
use std::process::Command;

#[test]
fn recovery_setup_error_honors_json_output() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_cooldown"))
        .current_dir(directory.path())
        .args(["recover", "--tool", "npm", "--json", "--no-progress"])
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stderr.is_empty(),
        "JSON recovery error wrote to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        document
            .pointer("/schemaVersion")
            .and_then(serde_json::Value::as_u64),
        Some(4)
    );
    assert_eq!(
        document
            .pointer("/command")
            .and_then(serde_json::Value::as_str),
        Some("recover")
    );
    assert_eq!(
        document.pointer("/ok").and_then(serde_json::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        document
            .pointer("/summary/errors")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );
    assert_eq!(
        document
            .pointer("/errors/0/kind")
            .and_then(serde_json::Value::as_str),
        Some("config")
    );
    let message = document
        .pointer("/errors/0/message")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| eyre::eyre!("recovery error envelope omitted its diagnostic message"))?;
    assert!(message.contains("recovery currently supports Cargo only"));
    Ok(())
}

#[test]
fn clean_cargo_project_recovers_as_unchanged() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("Cargo.toml"),
        indoc! {r#"
            [workspace]
            resolver = "3"
        "#},
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_cooldown"))
        .args(["recover", "--cargo", "--json", "--no-progress", "-C"])
        .arg(directory.path())
        .output()?;

    assert_eq!(
        output.status.code(),
        Some(0),
        "recover failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        document
            .pointer("/schemaVersion")
            .and_then(serde_json::Value::as_u64),
        Some(4)
    );
    assert_eq!(
        document.pointer("/ok").and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        document
            .pointer("/summary/unchanged")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );
    assert_eq!(
        document
            .pointer("/items/0/status")
            .and_then(serde_json::Value::as_str),
        Some("unchanged")
    );
    Ok(())
}
