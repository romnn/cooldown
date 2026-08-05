//! End-to-end tests for the cargo `--cargo-edge-policy` enforcement, driving the REAL `cargo` resolver
//! against a fixture shaped like the production regression that motivated it: a registry crate
//! (`diesel`) whose wide `uuid = ">=0.7.0, <2.0.0"` range admits **two coexisting locked majors**
//! (`0.8.2` pinned by one root dep, a `1.x` line declared by another).
//! Cargo's incremental
//! re-resolves may bind diesel's edge to either one — a build-affecting choice that is invisible at
//! the per-version level and that `cargo metadata --locked` accepts both ways (neither binding
//! orphans a package).
//! The tests seed the *bad* binding deliberately (a lock-text edit, exactly how the regression was
//! produced and later hand-fixed) and assert what each policy does about it.
//!
//! Cargo's rebinding churn itself is chaotic — it depends on resolver traversal order in large
//! graphs and cannot be forced deterministically from a small fixture — so the *churn-restoration*
//! path of `preserve` is covered by the deterministic unit tests in `cooldown-cargo`'s `edges`
//! modules; here the real-resolver suite covers what is deterministic end to end: healing under
//! `canonicalize` (with and without a planned version change), the preserve/none defaults leaving
//! a pre-existing binding alone, `[tool.cargo]` config plumbing, lock verifiability, and
//! convergence.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use indoc::indoc;
use support::Fixture;

/// The absolute resolution cutoff (same instant as `convergence_cargo.rs`): crates.io history
/// before it is immutable, so the matured `uuid 1.x` target reproduces forever.
const FREEZE: &str = "2024-06-01T00:00:00Z";

/// The seeded `uuid 1.x` version, far below anything the freeze admits, so the fixture always
/// plans a real `uuid` upgrade (the edge policy runs inside `apply`, which needs a planned change).
const SEED_UUID: &str = "1.2.2";

/// `diesel` is `=`-pinned (it is scenery, not an upgrade candidate) and enables its `uuid` feature,
/// giving the lock a registry crate with the wide `uuid >=0.7.0, <2.0.0` edge.
/// `uuid_old` pins the
/// `0.8` major into the lock; `uuid_new` keeps a `1.x` line that both majors' requirements keep
/// referenced whichever way diesel's edge binds (so neither binding orphans a package).
const EDGE_MANIFEST: &str = indoc! {r#"
    [package]
    name = "cargo-edge-policy"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    diesel = { version = "=2.3.11", default-features = false, features = ["uuid"] }
    uuid_old = { package = "uuid", version = "=0.8.2" }
    uuid_new = { package = "uuid", version = "1" }
"#};

/// Every `version` recorded for `crate_name` in a `Cargo.lock`, one per `[[package]]` block.
fn cargo_lock_versions_of(crate_name: &str, lock: &[u8]) -> Vec<String> {
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

/// The version `dependent`'s dependency entry binds `dep` to in a `Cargo.lock` (`"uuid 0.8.2"` →
/// `0.8.2`), or `None` when the entry is unqualified or absent.
fn edge_binding(lock: &[u8], dependent: &str, dep: &str) -> Option<String> {
    let text = String::from_utf8_lossy(lock);
    let mut in_dependent = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name = ") {
            in_dependent = rest.trim_matches('"') == dependent;
        } else if in_dependent
            && let Some(entry) = trimmed
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix("\","))
            && let Some(version) = entry.strip_prefix(&format!("{dep} "))
        {
            return Some(version.to_string());
        }
    }
    None
}

/// Rebinds `dependent`'s `dep` entry to `to` by editing the lock text within the dependent's
/// `[[package]]` block — the same lock-level state a stray incremental re-resolve produces (and
/// the way the production regression was seeded and later hand-fixed).
fn seed_edge_binding(fixture: &Fixture, dependent: &str, dep: &str, to: &str) {
    let lock = String::from_utf8(fixture.read_bytes("Cargo.lock")).expect("utf8 lock");
    let from = edge_binding(fixture.read_bytes("Cargo.lock").as_slice(), dependent, dep)
        .expect("the dependent must hold a qualified edge to rebind");
    let mut rewritten = Vec::new();
    let mut in_dependent = false;
    for line in lock.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name = ") {
            in_dependent = rest.trim_matches('"') == dependent;
        }
        if in_dependent && trimmed == format!("\"{dep} {from}\",") {
            rewritten.push(line.replace(&format!("{dep} {from}"), &format!("{dep} {to}")));
            continue;
        }
        rewritten.push(line.to_string());
    }
    let mut text = rewritten.join("\n");
    text.push('\n');
    fixture.write("Cargo.lock", &text);
}

/// Seeds the fixture: resolve at "newest now", roll the `uuid 1.x` line down to [`SEED_UUID`]
/// (so an upgrade is always planned under the freeze), then rebind diesel's wide-range edge onto
/// the coexisting `0.8.2` — the bad binding a from-scratch resolve never produces.
fn edge_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", EDGE_MANIFEST)
        .write("src/lib.rs", "")
        // Anchor the fixture as its own repo root: config discovery walks up to the nearest `.git`,
        // and a stray one in a temp-dir ancestor (e.g. `/tmp/.git`) would otherwise hijack the root
        // and drop the fixture's `cooldown.toml` `[<command>]` sections from the scan config.
        .write(".git/cooldown-fixture-anchor", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    let seeded = cargo_lock_versions_of("uuid", &fixture.read_bytes("Cargo.lock"))
        .into_iter()
        .find(|version| version.starts_with("1."))
        .expect("the uuid_new entry seeds a 1.x line");
    // The documented foundation: a *from-scratch* resolve binds the wide-range edge to the highest
    // satisfying version (the 1.x line), never to the coexisting 0.8.2.
    assert_eq!(
        edge_binding(&fixture.read_bytes("Cargo.lock"), "diesel", "uuid").as_deref(),
        Some(seeded.as_str()),
        "a from-scratch resolve must bind diesel's uuid edge to the highest satisfying version"
    );
    fixture
        .run_tool(
            "cargo",
            &[
                "update",
                "-p",
                &format!("uuid@{seeded}"),
                "--precise",
                SEED_UUID,
            ],
            &[],
        )
        .expect_success();
    // The incremental pin above may already have rebound diesel's edge (that instability is the
    // point); seed the bad binding unconditionally so every test starts from the same state.
    seed_edge_binding(&fixture, "diesel", "uuid", "0.8.2");
    // The bad binding satisfies diesel's declared range and orphans nothing, so cargo itself
    // accepts the lock — the blind spot the edge policy exists for.
    fixture
        .run_tool(
            "cargo",
            &["metadata", "--locked", "--format-version", "1"],
            &[],
        )
        .expect_success();
    fixture
}

/// The final `uuid 1.x` version the freeze admitted, read back from the lock (never hard-coded).
fn final_uuid_line(fixture: &Fixture) -> String {
    cargo_lock_versions_of("uuid", &fixture.read_bytes("Cargo.lock"))
        .into_iter()
        .find(|version| version.starts_with("1."))
        .expect("the uuid 1.x line survives the upgrade")
}

#[test]
fn canonicalize_heals_a_seeded_bad_binding_and_converges() {
    skip_if_missing!("cargo");
    let fixture = edge_fixture();

    let upgrade = fixture.cooldown_json(&[
        "upgrade",
        "--freeze",
        FREEZE,
        "--package",
        "uuid",
        "--cargo-edge-policy",
        "canonicalize",
    ]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&[
                "upgrade",
                "--freeze",
                FREEZE,
                "--package",
                "uuid",
                "--cargo-edge-policy",
                "canonicalize",
            ])
            .stderr_str()
    );
    assert!(
        upgrade.applied_names().contains("uuid"),
        "the seeded 1.x line must move forward, got {:?}",
        upgrade.applied_names()
    );

    // The healed edge: bound to the (freeze-admitted) 1.x line, and reported as its own row.
    let target = final_uuid_line(&fixture);
    assert_ne!(target, SEED_UUID, "the freeze must admit a newer 1.x");
    assert_eq!(
        edge_binding(&fixture.read_bytes("Cargo.lock"), "diesel", "uuid").as_deref(),
        Some(target.as_str()),
        "canonicalize must bind diesel's edge to the highest satisfying locked version"
    );
    let edge_rows = upgrade.edge_rows();
    assert!(
        edge_rows.iter().any(|row| {
            row.dependency == "uuid"
                && row.dependent == "diesel"
                && row.dependent_source.as_deref()
                    == Some("registry+https://github.com/rust-lang/crates.io-index")
                && row.action == "canonicalized"
                && row.from == "0.8.2"
                && row.to == target
        }),
        "the healed binding must surface as a canonicalized edge row, got {edge_rows:?}"
    );
    // The 0.8.2 line survives (uuid_old still pins it) and cargo accepts the corrected lock.
    assert!(
        cargo_lock_versions_of("uuid", &fixture.read_bytes("Cargo.lock"))
            .contains(&"0.8.2".to_string())
    );
    fixture
        .run_tool(
            "cargo",
            &["metadata", "--locked", "--format-version", "1"],
            &[],
        )
        .expect_success();

    // Fixed point: the canonical lock re-applies byte-stable with no further edge rows.
    let lock_after = fixture.read_bytes("Cargo.lock");
    let second = fixture.cooldown_json(&[
        "upgrade",
        "--freeze",
        FREEZE,
        "--package",
        "uuid",
        "--cargo-edge-policy",
        "canonicalize",
    ]);
    assert!(second.ok());
    assert_eq!(second.summary_applied(), 0, "nothing left to move");
    assert!(
        second.edge_rows().is_empty(),
        "a canonical lock has no edges to correct: {:?}",
        second.edge_rows()
    );
    assert_eq!(
        lock_after,
        fixture.read_bytes("Cargo.lock"),
        "lock must be byte-identical across converged runs"
    );
}

#[test]
fn preserve_default_keeps_a_pre_existing_binding_and_stays_verifiable() {
    skip_if_missing!("cargo");
    let fixture = edge_fixture();

    // No --edge-policy flag means `preserve` is the default.
    // A binding that already sat on 0.8.2 before the run is pre-existing state, not churn this apply
    // introduced — preserve restores
    // only what the run's own re-resolve moved, so whichever way cargo's incremental resolver
    // jumps during the upgrade, the committed binding deterministically stays 0.8.2.
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--package", "uuid"]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE, "--package", "uuid"])
            .stderr_str()
    );
    assert!(upgrade.applied_names().contains("uuid"));
    assert_eq!(
        edge_binding(&fixture.read_bytes("Cargo.lock"), "diesel", "uuid").as_deref(),
        Some("0.8.2"),
        "preserve must keep the pre-apply binding, healed or not"
    );
    // Any edge row this run reports can only be a restoration of the run's own churn; an
    // uncorrected `rebound` row would mean preserve failed its contract.
    assert!(
        upgrade
            .edge_rows()
            .iter()
            .all(|row| row.action == "restored"),
        "under preserve no rebind may be left uncorrected: {:?}",
        upgrade.edge_rows()
    );
    // The kept lock still verifies with cargo itself.
    fixture
        .run_tool(
            "cargo",
            &["metadata", "--locked", "--format-version", "1"],
            &[],
        )
        .expect_success();
}

#[test]
fn edge_policy_none_reports_without_correcting() {
    skip_if_missing!("cargo");
    let fixture = edge_fixture();
    let binding_before = edge_binding(&fixture.read_bytes("Cargo.lock"), "diesel", "uuid");

    let upgrade = fixture.cooldown_json(&[
        "upgrade",
        "--freeze",
        FREEZE,
        "--package",
        "uuid",
        "--cargo-edge-policy",
        "none",
    ]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE, "--package", "uuid"])
            .stderr_str()
    );
    assert!(upgrade.applied_names().contains("uuid"));

    // `none` never corrects: the committed binding is whatever the resolver produced.
    // When the resolver moved it off the seeded 0.8.2, exactly that move must surface as a `rebound`
    // row; when it did not, no edge row may appear.
    // Either way nothing is corrected and nothing is silent.
    let binding_after = edge_binding(&fixture.read_bytes("Cargo.lock"), "diesel", "uuid");
    let edge_rows = upgrade.edge_rows();
    assert!(
        edge_rows.iter().all(|row| row.action == "rebound"),
        "edge-policy none must never correct a binding: {edge_rows:?}"
    );
    if binding_after == binding_before {
        assert!(
            edge_rows.is_empty(),
            "an unmoved binding must not produce edge rows: {edge_rows:?}"
        );
    } else {
        assert!(
            edge_rows.iter().any(|row| {
                row.dependency == "uuid"
                    && row.dependent == "diesel"
                    && row.action == "rebound"
                    && Some(row.to.as_str()) == binding_after.as_deref()
            }),
            "a moved binding must surface as a rebound row: {edge_rows:?}"
        );
    }
}

/// Healing must not be gated on an unrelated upgrade existing: a lock whose versions are all
/// current (nothing to plan) but whose diesel edge sits on the bad binding is healed by
/// `canonicalize` through the standalone normalization pass.
#[test]
fn canonicalize_heals_without_any_planned_change() {
    skip_if_missing!("cargo");
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", EDGE_MANIFEST)
        .write("src/lib.rs", "")
        .write(".git/cooldown-fixture-anchor", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    // Without the seeded UUID downgrade, the 1.x line stays at newest-now, so under the past freeze
    // nothing is adoptable and the plan is empty.
    // Only the bad binding is seeded.
    seed_edge_binding(&fixture, "diesel", "uuid", "0.8.2");

    let upgrade = fixture.cooldown_json(&[
        "upgrade",
        "--freeze",
        FREEZE,
        "--package",
        "uuid",
        "--cargo-edge-policy",
        "canonicalize",
    ]);
    assert!(
        upgrade.ok(),
        "empty-plan upgrade should succeed: {}",
        fixture
            .cooldown(&[
                "upgrade",
                "--freeze",
                FREEZE,
                "--package",
                "uuid",
                "--cargo-edge-policy",
                "canonicalize",
            ])
            .stderr_str()
    );
    assert_eq!(
        upgrade.summary_applied(),
        0,
        "nothing is adoptable under the freeze — healing must not depend on an apply"
    );
    assert_eq!(
        upgrade.summary_edges_corrected(),
        1,
        "the healed binding is counted apart from the version changes"
    );
    assert_eq!(upgrade.summary_edges_held(), 0);
    assert_eq!(upgrade.summary_edges_unaddressable(), 0);
    assert!(
        upgrade.meta_applied(),
        "a corrected edge binding wrote the lock, so meta.applied must say so"
    );

    let target = final_uuid_line(&fixture);
    assert_eq!(
        edge_binding(&fixture.read_bytes("Cargo.lock"), "diesel", "uuid").as_deref(),
        Some(target.as_str()),
        "the standalone normalization pass must heal the seeded binding"
    );
    assert!(
        upgrade.edge_rows().iter().any(|row| {
            row.dependency == "uuid"
                && row.dependent == "diesel"
                && row.action == "canonicalized"
                && row.applied
                && row.from == "0.8.2"
                && row.to == target
        }),
        "the healed binding must surface as a row: {:?}",
        upgrade.edge_rows()
    );
    fixture
        .run_tool(
            "cargo",
            &["metadata", "--locked", "--format-version", "1"],
            &[],
        )
        .expect_success();
}

#[test]
fn config_file_edge_policy_is_honored_without_a_flag() {
    skip_if_missing!("cargo");
    let fixture = edge_fixture();
    fixture.write(
        "cooldown.toml",
        indoc! {r#"
            [tool.cargo]
            edge-policy = "canonicalize"
        "#},
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--package", "uuid"]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE, "--package", "uuid"])
            .stderr_str()
    );

    // The config layer alone must select canonicalize: the seeded bad binding is healed.
    let target = final_uuid_line(&fixture);
    assert_eq!(
        edge_binding(&fixture.read_bytes("Cargo.lock"), "diesel", "uuid").as_deref(),
        Some(target.as_str()),
        "the [tool.cargo] edge-policy config value must drive the enforcement"
    );
    assert!(
        upgrade
            .edge_rows()
            .iter()
            .any(|row| row.dependency == "uuid" && row.action == "canonicalized"),
        "the config-selected policy must report its correction: {:?}",
        upgrade.edge_rows()
    );
}
