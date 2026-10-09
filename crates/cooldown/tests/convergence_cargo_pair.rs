//! Real Cargo convergence tests for a library and its directly declared companion.

#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "the shared integration harness deliberately panics on invalid fixture setup or malformed command output"
)]
mod support;

use color_eyre::eyre;
use indoc::formatdoc;
use support::Fixture;

const FREEZE: &str = "2023-09-21T00:00:00Z";

fn pair_fixture(core_requirement: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write(
            "Cargo.toml",
            &formatdoc! {r#"
                [package]
                name = "cargo-direct-pair"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                rayon = "1"
                rayon-core = "{core_requirement}"
            "#},
        )
        .write("src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();

    // Seed the entire graph before the freeze, so only the upgrade can introduce fresh nodes.
    // Pin parents first because their older requirements admit the historical children.
    for (name, version) in [
        ("rayon", "1.6.1"),
        ("rayon-core", "1.11.0"),
        ("crossbeam-channel", "0.5.8"),
        ("crossbeam-deque", "0.8.3"),
        ("crossbeam-epoch", "0.9.15"),
        ("crossbeam-utils", "0.8.16"),
        ("num_cpus", "1.16.0"),
        ("hermit-abi", "0.3.3"),
        ("libc", "0.2.148"),
        ("either", "1.9.0"),
        ("memoffset", "0.9.0"),
        ("scopeguard", "1.2.0"),
        ("cfg-if", "1.0.0"),
        ("autocfg", "1.1.0"),
    ] {
        fixture
            .run_tool("cargo", &["update", "-p", name, "--precise", version], &[])
            .expect_success();
    }
    fixture
}

fn lock_versions(fixture: &Fixture, name: &str) -> eyre::Result<Vec<String>> {
    let lock = String::from_utf8(fixture.read_bytes("Cargo.lock"))?;
    let document: toml::Value = toml::from_str(&lock)?;
    let packages = document
        .get("package")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| eyre::eyre!("lockfile has no package array"))?;
    Ok(packages
        .iter()
        .filter(|package| package.get("name").and_then(toml::Value::as_str) == Some(name))
        .filter_map(|package| package.get("version").and_then(toml::Value::as_str))
        .map(str::to_owned)
        .collect())
}

fn assert_converged_pair(
    fixture: &Fixture,
    rayon_version: &str,
    core_version: &str,
) -> eyre::Result<()> {
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(upgrade.ok(), "pair upgrade should succeed");
    assert!(
        upgrade.applied_names().contains("rayon"),
        "a feasible library upgrade must land: {:?}",
        upgrade.applied_names()
    );

    // Each directly declared name must keep exactly one resolved line.
    assert_eq!(lock_versions(fixture, "rayon")?, [rayon_version]);
    assert_eq!(lock_versions(fixture, "rayon-core")?, [core_version]);
    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert!(check.ok(), "the settled graph should pass check");
    assert_eq!(check.summary_violations(), 0);

    // Frozen reapplication must preserve both the graph and the declared companion requirement.
    let lock = fixture.read_bytes("Cargo.lock");
    let manifest = fixture.read_bytes("Cargo.toml");
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(second.ok(), "converged upgrade should succeed");
    assert_eq!(second.summary_applied(), 0);
    assert_eq!(lock, fixture.read_bytes("Cargo.lock"));
    assert_eq!(manifest, fixture.read_bytes("Cargo.toml"));
    Ok(())
}

#[test]
fn caret_companion_reconciles_fresh_transitives_and_converges() -> eyre::Result<()> {
    skip_if_missing!("cargo", Ok(()));
    // Core ^1.11 admits the September pair and today's newer patches and minor releases.
    // Cargo has no date cutoff, so floated core and crossbeam nodes must be cooled after resolving.
    let fixture = pair_fixture("^1.11.0");
    assert_converged_pair(&fixture, "1.8.0", "1.12.0")
}
