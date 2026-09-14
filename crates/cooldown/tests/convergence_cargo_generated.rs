//! End-to-end tests for `[tool.cargo] generated-members`, driving the REAL `cargo` resolver
//! against a workspace shaped like a cargo-hakari workspace-hack: authored members declare a
//! handful of crates.io dependencies, and a generated member every authored member depends on
//! mirrors them between `### BEGIN HAKARI SECTION` markers — hash-aliased renames for the two
//! coexisting majors of one crate, major-only requirements, and one line (`hex`) nothing else in
//! the workspace uses any more.
//!
//! The undeclared runs pin today's behaviour as the regression baseline: the hack's mirrored lines
//! are ordinary direct rows attributed to it, and the authored `bitflags 1` line cannot cross its
//! major because the hack's hash-aliased entry, which the authored widen never sees, keeps the old
//! major demanded. The declared runs show the follower semantics: the hack's rows disappear from
//! the direct report while `check` still gates every crate they name, the cross-major move lands
//! with the hack's aliased entry following it (so the workspace still resolves `--locked` with no
//! second `bitflags` copy), and the run ends by telling the user to regenerate the hack.
//!
//! # Determinism
//!
//! The freeze pins the resolution clock as in `convergence_cargo.rs`; the lock is seeded at
//! "newest now" and the `bitflags 2` line rolled down to the newest release under the freeze, so
//! the cross-major target of the `1.x` line is the same node the `2` line already holds and the
//! move is a clean unification rather than a resolver-order question.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use indoc::indoc;
use support::Fixture;

/// The absolute resolution cutoff (the instant `convergence_cargo.rs` uses): crates.io history
/// before it is immutable. `bitflags 2.5.0` (2024-03) is the newest `2.x` release under it, and
/// `2.6.0` (2024-06-13) is the first one past it.
const FREEZE: &str = "2024-06-01T00:00:00Z";

/// The newest `bitflags 2.x` release under [`FREEZE`]: the target of the `1.x` line's cross-major
/// move, and the version the `2` line is rolled down to so that move unifies onto it.
const BITFLAGS_UNDER_FREEZE: &str = "2.5.0";

const ROOT_MANIFEST: &str = indoc! {r#"
    [workspace]
    members = ["crates/app", "crates/lib", "crates/workspace-hack"]
    resolver = "2"
"#};

const APP_MANIFEST: &str = indoc! {r#"
    [package]
    name = "app"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    bitflags = "1"
    itertools = "0.12"
    toml = "0.7"
    workspace-hack = { path = "../workspace-hack" }
"#};

const LIB_MANIFEST: &str = indoc! {r#"
    [package]
    name = "lib"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    bitflags = "2"
    workspace-hack = { path = "../workspace-hack" }
"#};

/// The generated member, as cargo-hakari writes it: one entry per crate and compatibility line,
/// hash-aliased where two lines of one crate coexist, with major-only requirements and the
/// unified feature sets. `hex` is the stale line: nothing authored uses it any more. `toml_edit`
/// and `winnow` are companions of `toml 0.7` that its next major drags to other lines — the
/// collateral the projection must follow without any plan naming them.
const HACK_MANIFEST: &str = indoc! {r#"
    [package]
    name = "workspace-hack"
    version = "0.1.0"
    edition = "2021"
    publish = false

    ### BEGIN HAKARI SECTION
    [dependencies]
    bitflags-a8bc802c284492f8 = { package = "bitflags", version = "1" }
    bitflags-9cd2438375a6c43c = { package = "bitflags", version = "2" }
    either = { version = "1", features = ["use_std"] }
    hex = "0.4"
    itertools = { version = "0.12", features = ["use_std"] }
    serde_spanned = { version = "0.6", features = ["serde"] }
    toml = { version = "0.7", features = ["parse"] }
    toml_datetime = { version = "0.6", features = ["serde"] }
    toml_edit = { version = "0.19", features = ["serde"] }
    winnow = "0.5"

    ### END HAKARI SECTION
"#};

const DECLARED_CONFIG: &str = indoc! {r#"
    [tool.cargo]
    generated-members = ["workspace-hack"]
"#};

/// Every `version` recorded for `crate_name` in a `Cargo.lock`, one per `[[package]]` block.
fn lock_versions_of(crate_name: &str, lock: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(lock);
    let mut versions = Vec::new();
    let mut in_target = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name = ") {
            in_target = rest.trim_matches('"') == crate_name;
        } else if in_target && let Some(rest) = line.strip_prefix("version = ") {
            versions.push(rest.trim_matches('"').to_string());
            in_target = false;
        }
    }
    versions
}

/// Seeds the workspace and its lock: resolve at "newest now" with the real cargo, then roll the
/// `bitflags 2` line down to the newest release under the freeze.
fn hakari_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", ROOT_MANIFEST)
        .write("crates/app/Cargo.toml", APP_MANIFEST)
        .write("crates/app/src/lib.rs", "")
        .write("crates/lib/Cargo.toml", LIB_MANIFEST)
        .write("crates/lib/src/lib.rs", "")
        .write("crates/workspace-hack/Cargo.toml", HACK_MANIFEST)
        .write("crates/workspace-hack/src/lib.rs", "")
        // Anchor the fixture as its own repo root so config discovery never walks past it.
        .write(".git/cooldown-fixture-anchor", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    let newest_two = lock_versions_of("bitflags", &fixture.read_bytes("Cargo.lock"))
        .into_iter()
        .find(|version| version.starts_with("2."))
        .expect("lib seeds a bitflags 2.x line");
    fixture
        .run_tool(
            "cargo",
            &[
                "update",
                "-p",
                &format!("bitflags@{newest_two}"),
                "--precise",
                BITFLAGS_UNDER_FREEZE,
            ],
            &[],
        )
        .expect_success();
    assert_eq!(
        lock_versions_of("bitflags", &fixture.read_bytes("Cargo.lock")),
        ["1.3.2", BITFLAGS_UNDER_FREEZE],
        "the seed holds both bitflags lines"
    );
    fixture
}

fn assert_locked(fixture: &Fixture) {
    fixture
        .run_tool(
            "cargo",
            &["metadata", "--locked", "--format-version", "1"],
            &[],
        )
        .expect_success();
}

/// The regression baseline: undeclared, the hack is an ordinary member. Its mirrored lines are
/// direct rows attributed to it, the authored `bitflags 1` line is `blocked` because the hack's
/// hash-aliased entry keeps the old major demanded, and the run hints at the declaration.
#[test]
fn undeclared_hack_is_reported_and_blocks_as_today() {
    skip_if_missing!("cargo");
    let fixture = hakari_fixture();

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--major", "--all"]);
    assert!(outdated.ok(), "{:?}", outdated.error_messages());
    assert!(
        outdated.item_names().contains("hex"),
        "the hack's own line is a direct row today: {:?}",
        outdated.item_names()
    );
    assert_eq!(
        outdated.item_member_names("hex", "0.4.3"),
        ["workspace-hack"],
        "attributed to the hack"
    );
    assert_eq!(
        outdated.item_member_names("bitflags", "1.3.2"),
        ["app", "workspace-hack"],
        "the hack co-declares the authored line"
    );
    assert!(
        outdated
            .outdated_with_status("blocked")
            .contains("bitflags"),
        "the cross-major move is blocked by the hack's aliased entry: {:?}",
        outdated.outdated_with_status("blocked")
    );
    assert!(
        outdated.warning_kinds().contains("config"),
        "the marker earns a hint: {:?}",
        outdated.warning_kinds()
    );
    assert!(
        outdated
            .warning_messages()
            .iter()
            .any(|message| message.contains("generated-members = [\"workspace-hack\"]")),
        "the hint spells the declaration: {:?}",
        outdated.warning_messages()
    );

    let upgrade = fixture.cooldown_json(&[
        "upgrade",
        "--freeze",
        FREEZE,
        "--major",
        "--package",
        "bitflags",
    ]);
    assert!(
        !upgrade.applied_names().contains("bitflags"),
        "the aliased entry the widen never sees holds the move: {:?}",
        upgrade.applied_names()
    );
    assert_eq!(
        lock_versions_of("bitflags", &fixture.read_bytes("Cargo.lock")),
        ["1.3.2", BITFLAGS_UNDER_FREEZE]
    );
    assert_locked(&fixture);
}

/// Declared, the hack follows: its rows leave the direct report (and stay gated), the
/// cross-major move lands with the aliased entry following it, the workspace still resolves
/// `--locked` with a single `bitflags`, and the run says to regenerate the hack.
#[test]
fn declared_hack_follows_the_lock_instead_of_driving_it() {
    skip_if_missing!("cargo");
    let fixture = hakari_fixture();
    let baseline_check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    fixture.write("cooldown.toml", DECLARED_CONFIG);
    assert_declared_rows_follow(&fixture, &baseline_check);
    assert_declared_upgrade_follows(&fixture);
    assert_companion_lines_follow(&fixture);
}

/// A planned move's collateral: `toml 0.7 → 0.8` takes `toml_edit` and `winnow` to new lines
/// no plan named. The projection's entries for their old lines follow too, so the old copies are
/// not kept alive for the projection alone and the workspace still resolves `--locked`.
fn assert_companion_lines_follow(fixture: &Fixture) {
    let before = fixture.read_bytes("Cargo.lock");
    let old_toml_edit = lock_versions_of("toml_edit", &before);
    assert!(
        old_toml_edit
            .iter()
            .all(|version| version.starts_with("0.19.")),
        "the seed resolves toml 0.7's companion line: {old_toml_edit:?}"
    );

    let upgrade = fixture.cooldown_json(&[
        "upgrade",
        "--freeze",
        FREEZE,
        "--major",
        "--package",
        "toml",
    ]);
    assert!(upgrade.ok(), "{:?}", upgrade.error_messages());
    assert!(
        upgrade.applied_names().contains("toml"),
        "{:?}",
        upgrade.applied_names()
    );

    let after = fixture.read_bytes("Cargo.lock");
    let toml = lock_versions_of("toml", &after);
    assert!(
        matches!(toml.as_slice(), [only] if only.starts_with("0.8.")),
        "toml crossed its major: {toml:?}"
    );
    for (companion, new_line) in [("toml_edit", "0.22."), ("winnow", "0.6.")] {
        let lines = lock_versions_of(companion, &after);
        assert!(
            matches!(lines.as_slice(), [only] if only.starts_with(new_line)),
            "{companion} must be on its new line alone, not also on the old one the projection held: {lines:?}"
        );
    }
    let hack = String::from_utf8(fixture.read_bytes("crates/workspace-hack/Cargo.toml")).unwrap();
    let toml_edit = lock_versions_of("toml_edit", &after).remove(0);
    assert!(
        hack.contains(&format!(
            "toml_edit = {{ version = \"{toml_edit}\", features = [\"serde\"] }}"
        )),
        "the companion's projection followed to the authored line, features kept: {hack}"
    );
    assert!(!hack.contains("version = \"0.19\""), "{hack}");
    assert!(
        upgrade
            .warning_paths()
            .contains("crates/workspace-hack/Cargo.toml"),
        "{:?}",
        upgrade.warning_messages()
    );
    assert_locked(fixture);
}

/// The reporting side of the declaration: the hack's lines leave the direct report, stay
/// attributed through the real graph under `--transitive`, and stay gated by `check`.
fn assert_declared_rows_follow(fixture: &Fixture, baseline_check: &support::Envelope) {
    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--major", "--all"]);
    assert!(outdated.ok(), "{:?}", outdated.error_messages());
    let names = outdated.item_names();
    assert!(
        !names.contains("hex") && !names.contains("either"),
        "lines only the hack declares are not direct rows: {names:?}"
    );
    assert_eq!(
        outdated.item_member_names("bitflags", "1.3.2"),
        ["app"],
        "the authored line is attributed to its author alone"
    );
    assert!(
        outdated
            .outdated_with_status("adoptable")
            .contains("bitflags"),
        "the cross-major move is adoptable once the projection follows: {:?}",
        outdated.outdated_with_status("blocked")
    );
    assert!(
        !outdated.warning_kinds().contains("config"),
        "no hint once declared: {:?}",
        outdated.warning_messages()
    );
    assert!(
        outdated
            .warning_messages()
            .iter()
            .any(|message| message.contains("`workspace-hack`") && message.contains("hex 0.4.3")),
        "the stale line is reported for regeneration: {:?}",
        outdated.warning_messages()
    );
    // The policy preview that verified the move rewrote only its throwaway copy, and says so
    // conditionally.
    assert!(
        outdated
            .warning_messages()
            .iter()
            .any(|message| message.contains("would rewrite generated manifest")),
        "{:?}",
        outdated.warning_messages()
    );
    assert!(
        !outdated
            .warning_messages()
            .iter()
            .any(|message| message.contains("was rewritten")),
        "nothing the user keeps was rewritten: {:?}",
        outdated.warning_messages()
    );

    // Under `--transitive` the mirrored lines are rows again, attributed through the real graph:
    // `either` via the member whose itertools pulls it in, `hex` via the hack alone.
    let transitive = fixture.cooldown_json(&[
        "outdated",
        "--freeze",
        FREEZE,
        "--major",
        "--all",
        "--transitive",
    ]);
    assert_eq!(
        transitive.item_member_names("hex", "0.4.3"),
        ["workspace-hack"]
    );
    let either_current = lock_versions_of("either", &fixture.read_bytes("Cargo.lock"))
        .pop()
        .expect("either is locked");
    assert_eq!(
        transitive.item_member_names("either", &either_current),
        ["app"]
    );

    // `check` gates exactly the same lock graph; only what counts as direct changed.
    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert_eq!(check.summary_checked(), baseline_check.summary_checked());
    assert!(
        check.summary_direct() < baseline_check.summary_direct(),
        "the hack's mirrored lines are no longer direct: {} vs {}",
        check.summary_direct(),
        baseline_check.summary_direct()
    );
}

/// The mutation side: the cross-major move lands with the aliased entry following it, the
/// workspace still resolves `--locked` with a single `bitflags`, the run says to regenerate the
/// hack, and a second run is a no-op.
fn assert_declared_upgrade_follows(fixture: &Fixture) {
    let upgrade = fixture.cooldown_json(&[
        "upgrade",
        "--freeze",
        FREEZE,
        "--major",
        "--package",
        "bitflags",
    ]);
    assert!(upgrade.ok(), "{:?}", upgrade.error_messages());
    assert!(
        upgrade.applied_names().contains("bitflags"),
        "{:?}",
        upgrade.applied_names()
    );
    assert_eq!(
        lock_versions_of("bitflags", &fixture.read_bytes("Cargo.lock")),
        [BITFLAGS_UNDER_FREEZE],
        "the old major is gone and no second copy was resolved"
    );
    let app = String::from_utf8(fixture.read_bytes("crates/app/Cargo.toml")).unwrap();
    assert!(
        app.contains(&format!("bitflags = \"{BITFLAGS_UNDER_FREEZE}\"")),
        "{app}"
    );
    let hack = String::from_utf8(fixture.read_bytes("crates/workspace-hack/Cargo.toml")).unwrap();
    // The `1` line moved onto the line its sibling entry already projects, so following means
    // merging: the moving entry is gone and the sibling carries the crate, as a regeneration
    // would write it (bumped instead, both entries would name one node twice, which cargo
    // refuses).
    assert!(
        !hack.contains("bitflags-a8bc802c284492f8"),
        "the entry projecting the moved line is merged away: {hack}"
    );
    assert!(
        hack.contains("bitflags-9cd2438375a6c43c = { package = \"bitflags\", version = \"2\" }"),
        "the entry projecting the line that stayed is untouched: {hack}"
    );
    assert!(
        hack.contains("### BEGIN HAKARI SECTION") && hack.contains("hex = \"0.4\""),
        "everything else in the generated section is as written: {hack}"
    );
    assert!(
        upgrade
            .warning_paths()
            .contains("crates/workspace-hack/Cargo.toml"),
        "the run names the followed projection for regeneration: {:?}",
        upgrade.warning_messages()
    );
    assert!(
        upgrade.warning_messages().iter().any(|message| {
            message.contains("was rewritten") && message.contains("cargo hakari generate")
        }),
        "a real run speaks in the past tense: {:?}",
        upgrade.warning_messages()
    );
    assert_locked(fixture);

    // Converged: a second run has nothing left to move and rewrites nothing.
    let again = fixture.cooldown_json(&[
        "upgrade",
        "--freeze",
        FREEZE,
        "--major",
        "--package",
        "bitflags",
    ]);
    assert!(again.ok(), "{:?}", again.error_messages());
    assert!(
        again.applied_names().is_empty(),
        "{:?}",
        again.applied_names()
    );
    assert_eq!(
        String::from_utf8(fixture.read_bytes("crates/workspace-hack/Cargo.toml")).unwrap(),
        hack
    );
}

/// A declaration that names no member fails every command that reads the workspace as a config
/// error naming the entry and the members that exist — the project is not evaluated at all —
/// rather than quietly covering nothing. `check` exits non-zero on it; `outdated` never gates
/// and carries it in its report.
#[test]
fn declaration_that_matches_nothing_fails_loudly() {
    skip_if_missing!("cargo");
    let fixture = hakari_fixture();
    fixture.write(
        "cooldown.toml",
        indoc! {r#"
            [tool.cargo]
            generated-members = ["workspace-hak"]
        "#},
    );

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert!(!check.ok(), "check must fail");
    for command in ["outdated", "check"] {
        let envelope = fixture.cooldown_json(&[command, "--freeze", FREEZE]);
        assert!(
            envelope.item_names().is_empty(),
            "{command} must not evaluate the project under a bad declaration: {:?}",
            envelope.item_names()
        );
        assert!(
            envelope.error_kinds().contains("config"),
            "{command}: {:?}",
            envelope.error_kinds()
        );
        assert!(
            envelope.error_messages().iter().any(|message| {
                message.contains("`workspace-hak`") && message.contains("workspace-hack")
            }),
            "{command} names the bad entry beside the real members: {:?}",
            envelope.error_messages()
        );
    }
}

/// An explicit empty declaration keeps every member authored and silences the hint; `config`
/// reports the declaration and the file that made it either way.
#[test]
fn empty_declaration_and_config_report() {
    skip_if_missing!("cargo");
    let fixture = hakari_fixture();

    let undeclared = fixture.cooldown_json(&["config"]);
    let item = undeclared.first_item().expect("one cargo project");
    assert_eq!(
        item["generatedMembers"],
        serde_json::json!({ "names": [], "origin": null }),
        "nothing declares the key: {item}"
    );

    fixture.write("cooldown.toml", "[tool.cargo]\ngenerated-members = []\n");
    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--major", "--all"]);
    assert!(outdated.ok(), "{:?}", outdated.error_messages());
    assert!(
        !outdated.warning_kinds().contains("config"),
        "an explicit empty list is the user's decision: {:?}",
        outdated.warning_messages()
    );
    assert!(
        outdated.item_names().contains("hex"),
        "and every member stays authored: {:?}",
        outdated.item_names()
    );

    fixture.write("cooldown.toml", DECLARED_CONFIG);
    let declared = fixture.cooldown_json(&["config"]);
    let item = declared.first_item().expect("one cargo project");
    assert_eq!(
        item["generatedMembers"]["names"],
        serde_json::json!(["workspace-hack"])
    );
    let origin = item["generatedMembers"]["origin"]
        .as_str()
        .expect("the declaring file is named");
    assert!(
        origin.starts_with("repo:") && origin.ends_with("cooldown.toml"),
        "{origin}"
    );
    let text = fixture.cooldown(&["config"]).stdout_str();
    assert!(
        text.contains("generated members: workspace-hack (declared by repo:"),
        "{text}"
    );
}
