//! End-to-end tests that drive the real Poetry against a `poetry.lock` it generated, proving the
//! lock probe `check` gates a Poetry project with.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use indoc::indoc;
use support::{Envelope, Fixture, project_files};

/// An instant after `six` 1.16.0 was published and before 1.17.0 was.
const FREEZE: &str = "2024-06-30T00:00:00Z";

/// A project pinning the registry's `six==1.16.0`.
const PYPROJECT: &str = indoc! {r#"
    [project]
    name = "cooldown-poetry-lock-probe"
    version = "0.1.0"
    requires-python = ">=3.9"
    dependencies = ["six==1.16.0"]

    [tool.poetry]
    package-mode = false
"#};

/// The wheel of the same `six` 1.16.0 the registry pin resolves to, as a direct URL.
const SIX_WHEEL: &str = "https://files.pythonhosted.org/packages/d9/5a/\
                         e7c31adbe875f2abbb91bd84cf2dc52d792b5a01506781dbcf25c91daf11/\
                         six-1.16.0-py2.py3-none-any.whl";

/// A project holding `pyproject`, locked with a `poetry lock` of its own.
fn locked_fixture(pyproject: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture.write("pyproject.toml", pyproject);
    fixture
        .run_tool("poetry", &["lock", "--no-ansi", "--no-interaction"], &[])
        .expect_success();
    fixture
}

/// The registry pin of `six` replaced by its wheel URL, at the same version.
fn url_pyproject() -> String {
    PYPROJECT.replace("six==1.16.0", &format!("six @ {SIX_WHEEL}"))
}

/// Asserts that `check` failed with only the diagnostic `kind`.
fn assert_only_error(check: &Envelope, kind: &str) {
    assert!(!check.ok(), "{check:#?}");
    assert_eq!(
        check.error_kinds(),
        [kind.to_string()].into_iter().collect(),
        "{check:#?}"
    );
}

/// Poetry's own check proves a current lock, the registry pin is evaluated, and the project stays
/// byte for byte as it was, with no virtualenv or cache created in it even where `poetry.toml`
/// asks for an in-project virtualenv.
#[test]
fn check_gates_a_poetry_lock_poetry_proves_current() {
    skip_if_missing!("poetry");
    let fixture = locked_fixture(PYPROJECT);
    fixture.write("poetry.toml", "[virtualenvs]\nin-project = true\n");
    let before = project_files(fixture.root());

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);

    assert!(check.ok(), "{check:#?}");
    assert_eq!(check.summary_errors(), 0, "{check:#?}");
    assert!(check.summary_checked() > 0, "{check:#?}");
    // The file walk skips empty directories, so the ones Poetry would create are checked by name.
    assert_eq!(project_files(fixture.root()), before);
    for directory in [".venv", ".poetry", "__pycache__"] {
        assert!(!fixture.root().join(directory).exists(), "{directory}");
    }
}

/// A registry pin replaced by a URL at the same version changes the declarations Poetry hashes,
/// so it is Poetry's own stale verdict, which `--allow-stale-lock` waives.
#[test]
fn check_reports_a_same_version_source_edit_as_stale() {
    skip_if_missing!("poetry");
    let fixture = locked_fixture(PYPROJECT);
    fixture.write("pyproject.toml", &url_pyproject());

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);

    assert_only_error(&check, "stale_lock");
    assert!(
        check
            .error_messages()
            .iter()
            .any(|message| message.contains("poetry.lock is stale in ")
                && message.contains("pyproject.toml changed significantly")),
        "{:?}",
        check.error_messages()
    );
    let waived = fixture.cooldown_json(&["check", "--freeze", FREEZE, "--allow-stale-lock"]);
    assert!(waived.ok(), "{waived:#?}");
    assert!(waived.warning_kinds().contains("stale_lock"), "{waived:#?}");
}

/// `[tool.poetry] name` beside `[project] name`, which Poetry warns about without failing.
fn warned(pyproject: &str) -> String {
    pyproject.replace(
        "package-mode = false",
        "package-mode = false\nname = \"cooldown-poetry-lock-probe\"",
    )
}

/// A warning leaves Poetry's verdict alone: a current lock beside it passes, and a stale one is
/// still Poetry's own stale verdict.
#[test]
fn check_reads_poetry_warnings_as_no_failure() {
    skip_if_missing!("poetry");
    let fixture = locked_fixture(PYPROJECT);
    fixture.write("pyproject.toml", &warned(PYPROJECT));

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert!(check.ok(), "{check:#?}");
    assert!(check.summary_checked() > 0, "{check:#?}");

    fixture.write(
        "pyproject.toml",
        &warned(&PYPROJECT.replace("six==1.16.0", "six==1.15.0")),
    );
    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert_only_error(&check, "stale_lock");
}

/// An invalid configuration, and a stale refusal beside another error such as a missing README,
/// are Poetry failures that `--allow-stale-lock` does not waive.
#[test]
fn check_reports_other_poetry_failures_as_tool_errors() {
    skip_if_missing!("poetry");
    let invalid = PYPROJECT.replace("package-mode = false", "package-mode = false\nfoo = 1");
    let unreadable_readme = PYPROJECT.replace("six==1.16.0", "six==1.15.0").replace(
        "requires-python = \">=3.9\"",
        "requires-python = \">=3.9\"\nreadme = \"README.md\"",
    );
    for pyproject in [invalid, unreadable_readme] {
        let fixture = locked_fixture(PYPROJECT);
        fixture.write("pyproject.toml", &pyproject);

        let check = fixture.cooldown_json(&["check", "--freeze", FREEZE, "--allow-stale-lock"]);

        assert_only_error(&check, "tool_failed");
    }
}

/// Poetry's check compares the declarations alone, so a registry pin whose locked version was
/// edited by hand passes, and cooldown evaluates the version the lock now records.
#[test]
fn check_accepts_a_hand_edited_registry_version() {
    skip_if_missing!("poetry");
    let fixture = locked_fixture(PYPROJECT);
    let lock = String::from_utf8(fixture.read_bytes("poetry.lock")).expect("UTF-8 lock");
    fixture.write(
        "poetry.lock",
        &lock.replace("version = \"1.16.0\"", "version = \"1.15.0\""),
    );

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);

    assert!(check.ok(), "{check:#?}");
    assert!(check.summary_checked() > 0, "{check:#?}");
}

/// A lock emptied by hand keeps its content hash, so Poetry passes it, yet it resolves nothing the
/// manifest declares, so it is unknown rather than current.
#[test]
fn check_refuses_an_emptied_lock() {
    skip_if_missing!("poetry");
    let fixture = locked_fixture(PYPROJECT);
    let lock = String::from_utf8(fixture.read_bytes("poetry.lock")).expect("UTF-8 lock");
    let (generated, _) = lock.split_once('\n').expect("generator comment");
    let metadata = lock.find("[metadata]").expect("metadata table");
    fixture.write(
        "poetry.lock",
        &format!("{generated}\npackage = []\n\n{}", &lock[metadata..]),
    );

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE, "--allow-stale-lock"]);

    assert_only_error(&check, "lock_unknown");
}

/// A direct URL dependency can change behind an unchanged declaration, so its lock is unknown
/// rather than current.
#[test]
fn check_refuses_a_direct_url_dependency() {
    skip_if_missing!("poetry");
    let fixture = locked_fixture(&url_pyproject());

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE, "--allow-stale-lock"]);

    assert_only_error(&check, "lock_unknown");
}
