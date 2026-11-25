//! End-to-end tests that drive the real Bun against a generated `bun.lock` fixture, proving the
//! lock probe `check` gates a Bun project with.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use indoc::indoc;
use support::{Envelope, Fixture, project_files};

const FREEZE: &str = "2026-06-30T00:00:00Z";

/// A workspace whose root pins the registry's `ms@2.1.3` and whose `packages/*` holds one member.
const PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-bun-lock-probe",
      "private": true,
      "workspaces": ["packages/*"],
      "dependencies": {
        "ms": "2.1.3"
      }
    }
"#};

const MEMBER_JSON: &str = indoc! {r#"
    {
      "name": "member",
      "version": "1.0.0",
      "dependencies": {
        "ms": "^2.1.0"
      }
    }
"#};

/// The workspace locked with a `bun install --lockfile-only` of its own.
fn lock_probe_fixture() -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", PACKAGE_JSON);
    fixture.write("packages/member/package.json", MEMBER_JSON);
    fixture
        .run_tool(
            "bun",
            &[
                "install",
                "--lockfile-only",
                "--ignore-scripts",
                "--no-progress",
            ],
            &[],
        )
        .expect_success();
    fixture
}

/// Rewrites the root manifest's pin of `ms`, which Bun's frozen check refuses.
fn edit_root_pin(fixture: &Fixture) {
    fixture.write("package.json", &PACKAGE_JSON.replace("2.1.3", "2.1.2"));
}

/// Asserts that `check` failed with only `lock_unknown`.
fn assert_lock_unknown(check: &Envelope) {
    assert!(!check.ok(), "{check:#?}");
    assert_eq!(
        check.error_kinds(),
        ["lock_unknown".to_string()].into_iter().collect(),
        "{check:#?}"
    );
}

/// Bun's own frozen dry run proves a current lock, and the project, its `node_modules` included,
/// stays byte for byte as it was.
#[test]
fn check_gates_a_bun_lock_bun_proves_current() {
    skip_if_missing!("bun");
    let fixture = lock_probe_fixture();
    // A project bunfig pointing Bun's cache into the checkout, which the probe must override
    fixture.write("bunfig.toml", "[install.cache]\ndir = \"./.bun-cache\"\n");
    // An install tree a real install would replace, with a file no package owns
    fixture.write("node_modules/sentinel.txt", "left alone\n");
    fixture.write(
        "node_modules/ms/package.json",
        r#"{"name":"ms","version":"2.1.0"}"#,
    );
    let before = project_files(fixture.root());

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);

    assert!(check.ok(), "{check:#?}");
    assert_eq!(check.summary_errors(), 0, "{check:#?}");
    assert!(check.summary_checked() > 0, "{check:#?}");
    let after = project_files(fixture.root());
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>()
    );
    assert_eq!(before, after);
}

/// An edited manifest is Bun's own stale verdict, which `--allow-stale-lock` waives.
#[test]
fn check_reports_a_bun_lock_bun_refuses_as_stale() {
    skip_if_missing!("bun");
    let fixture = lock_probe_fixture();
    edit_root_pin(&fixture);

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert!(!check.ok(), "{check:#?}");
    assert_eq!(
        check.error_kinds(),
        ["stale_lock".to_string()].into_iter().collect(),
        "{check:#?}"
    );
    assert!(
        check
            .error_messages()
            .iter()
            .any(|message| message.contains("bun.lock is stale in ")
                && message.contains("lockfile had changes, but lockfile is frozen")),
        "{:?}",
        check.error_messages()
    );

    let waived = fixture.cooldown_json(&["check", "--freeze", FREEZE, "--allow-stale-lock"]);
    assert!(waived.ok(), "{waived:#?}");
    assert!(waived.warning_kinds().contains("stale_lock"), "{waived:#?}");
}

/// A configuration that disables Bun's cache, under which Bun writes into the project for a stale
/// lock, leaves the lock unknown before Bun runs, so the project stays as it was.
#[test]
fn check_refuses_a_disabled_cache_without_touching_the_project() {
    skip_if_missing!("bun");
    for (file, content) in [
        ("bunfig.toml", "[install.cache]\ndisable = true\n"),
        (".npmrc", "cache=false\n"),
        (".npmrc", "cache=false # keep Bun's cache off\n"),
    ] {
        let fixture = lock_probe_fixture();
        edit_root_pin(&fixture);
        fixture.write(file, content);
        let before = project_files(fixture.root());

        let check = fixture.cooldown_json(&["check", "--freeze", FREEZE, "--allow-stale-lock"]);

        assert_lock_unknown(&check);
        assert_eq!(project_files(fixture.root()), before, "{file}");
    }
}

/// A new member named like the root's registry dependency passes Bun's frozen check, so the lock
/// is unknown rather than current.
#[test]
fn check_refuses_a_new_member_named_like_a_locked_dependency() {
    skip_if_missing!("bun");
    let fixture = lock_probe_fixture();
    fixture.write(
        "packages/ms/package.json",
        r#"{ "name": "ms", "version": "9.0.0" }"#,
    );

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE, "--allow-stale-lock"]);

    assert_lock_unknown(&check);
}

/// A member's version edit passes Bun's frozen check, so the lock is unknown rather than current.
#[test]
fn check_refuses_a_member_version_edit() {
    skip_if_missing!("bun");
    let fixture = lock_probe_fixture();
    fixture.write(
        "packages/member/package.json",
        &MEMBER_JSON.replace("1.0.0", "1.0.1"),
    );

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE, "--allow-stale-lock"]);

    assert_lock_unknown(&check);
}

/// A project with only a binary `bun.lockb` is no Bun project cooldown reads, so `check` never
/// passes it.
#[test]
fn check_never_passes_a_binary_lock_alone() {
    skip_if_missing!("bun");
    let fixture = lock_probe_fixture();
    std::fs::remove_file(fixture.root().join("bun.lock")).expect("remove the text lock");
    fixture.write("bun.lockb", "binary lock\n");

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE, "--allow-stale-lock"]);

    assert!(!check.ok(), "{check:#?}");
    assert_eq!(
        check.error_kinds(),
        ["not_found".to_string()].into_iter().collect(),
        "{check:#?}"
    );
}
