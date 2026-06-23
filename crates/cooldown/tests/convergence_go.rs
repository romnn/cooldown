//! End-to-end convergence tests that drive the REAL `go` toolchain against fixtures generated on
//! the fly in temp dirs. These guard the Go adapter's whole-graph re-resolve: the adapter now hands
//! every planned `module@version` target to a single batched `go get module1@v1 module2@v2 …`
//! invocation (one minimal-version-selection pass over the whole graph), runs one `go mod tidy`, and
//! builds the report from the full before/after `go.mod` diff — so a candidate can never silently
//! move another module and a converged graph re-applies to a byte-stable fixed point.
//!
//! # Determinism
//!
//! Go has **no** publish-date cutoff flag (no `--exclude-newer`; GOPROXY has no server-side date
//! filter), so the window cannot be handed to `go`. cooldown realizes it out-of-band: each module's
//! available tags come from `go list -m -versions` and their publish instants from the GOPROXY
//! `@v/<ver>.info` timestamps, the core computes each module's newest-within-window target, and the
//! adapter hands that concrete `module@<version>` to `go get`. Every test pins the resolution clock
//! with `--freeze <FREEZE>` (an absolute cutoff the core applies to those publish instants), so the
//! set of matured versions — and therefore the concrete targets cooldown computes — is reproducible
//! from the immutable module proxy history. The starting `go.mod`/`go.sum` is seeded with the real
//! `go mod tidy`; the assertions check INVARIANTS (convergence, no-silent-change, cross-command
//! agreement, build-gate rejection), never hard-coded versions.
//!
//! # The conflict
//!
//! A SAT-style mutual-exclusion ping-pong cannot exist in MVS: there are no `<` upper bounds, so two
//! modules can never force each other down forever — `go get A@vA` only raises floors. The realistic
//! Go conflict is a COMPILE/API incompatibility surfaced by `go build`, not by the resolver. The
//! `k8s.io/*` family is the canonical case: `k8s.io/client-go` must move in lockstep with
//! `k8s.io/api`/`k8s.io/apimachinery`, and a `client-go` pinned older than the `apimachinery` the
//! graph selects fails to compile (the `structured-merge-diff/v4` vs `/v6` mismatch). The build-gate
//! test seeds exactly that incompatible pair and asserts `go build ./...` (wired via `--build`)
//! catches the break.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use support::{Fixture, changed_packages, go_mod_pins};

/// The absolute resolution cutoff for the upgrade tests. The module proxy's release history before
/// this instant is immutable, so the matured-version set — and the concrete targets cooldown computes
/// — reproduce forever. At this instant the `k8s.io` v0.30/v0.31 line is matured and the seed v0.29.0
/// is upgradable.
const FREEZE: &str = "2024-08-01T00:00:00Z";

/// An earlier cutoff for the `fix` test: the seed is resolved at a newer `k8s.io` line, so several
/// deps are younger than this instant and are cooldown violations to mature down.
const FREEZE_FIX: &str = "2024-05-01T00:00:00Z";

/// The `main.go` that imports the seeded modules so `go mod tidy` keeps them in the require graph
/// (an unused require is pruned). It touches `k8s.io/api` and `k8s.io/apimachinery` types directly.
const MAIN_GO: &str = r#"package main

import (
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

func main() {
	_ = corev1.Pod{}
	_ = metav1.ObjectMeta{}
}
"#;

/// The upgrade fixture: the `k8s.io/api` + `k8s.io/apimachinery` lockstep seeded at an older line
/// (v0.29.0). Under `FREEZE` the whole-graph re-resolve raises them and floor-raises several shared
/// indirects (`k8s.io/klog`, `golang.org/x/net`, …) — the collateral the before/after `go.mod` diff
/// must surface.
fn go_mod(api: &str) -> String {
    format!(
        "module example.com/cooldown-go-fixture\n\ngo 1.23\n\nrequire (\n\tk8s.io/api {api}\n\tk8s.io/apimachinery {api}\n)\n"
    )
}

/// Seed a `go.mod`/`go.sum` by resolving the fixture with the real `go mod tidy`. `GOFLAGS` is
/// cleared so an ambient `-mod=mod`/`-mod=vendor` does not change the seed shape.
fn seed(fixture: &Fixture, api_version: &str) {
    fixture.write("go.mod", &go_mod(api_version));
    fixture.write("main.go", MAIN_GO);
    fixture
        .run_tool("go", &["mod", "tidy"], &[("GOFLAGS", "")])
        .expect_success();
}

fn upgrade_fixture() -> Fixture {
    let fixture = Fixture::new();
    seed(&fixture, "v0.29.0");
    fixture
}

#[test]
fn upgrade_converges_to_a_fixed_point() {
    skip_if_missing!("go");
    let fixture = upgrade_fixture();

    // First upgrade: cooldown re-resolves the whole graph under the freeze cutoff, applying every
    // planned `module@version` in one batched `go get` pass and reporting the full `go.mod` diff.
    let first = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(
        first.ok(),
        "first upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE])
            .stderr_str()
    );
    assert_eq!(first.lock_verified(), Some(true), "first upgrade re-locks");
    assert!(
        first.summary_applied() >= 2,
        "first upgrade should apply the k8s lockstep moves, got {}",
        first.summary_applied()
    );
    let mod_after_first = fixture.read_bytes("go.mod");
    let sum_after_first = fixture.read_bytes("go.sum");

    // Second upgrade: already at the fixed point, so nothing moves and go.mod/go.sum are
    // byte-identical — MVS settled to a unique fixed point, no oscillation.
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a no-op (fixed point)"
    );
    assert_eq!(
        mod_after_first,
        fixture.read_bytes("go.mod"),
        "go.mod must be byte-identical across the two converged runs"
    );
    assert_eq!(
        sum_after_first,
        fixture.read_bytes("go.sum"),
        "go.sum must be byte-identical across the two converged runs"
    );
}

#[test]
fn upgrade_reports_every_moved_version_no_silent_change() {
    skip_if_missing!("go");
    let fixture = upgrade_fixture();

    let mod_before = fixture.read_bytes("go.mod");
    let report = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    let mod_after = fixture.read_bytes("go.mod");

    // The set of modules whose require version changed in go.mod, computed independently of the
    // report, must equal the report's applied set. This includes the collateral floor-raises another
    // candidate (or `go mod tidy`) forced on indirects the plan never named — never silent.
    let moved_in_mod = changed_packages(&mod_before, &mod_after, go_mod_pins);
    assert!(
        !moved_in_mod.is_empty(),
        "the upgrade should have moved at least one module"
    );
    let reported = report.applied_names();
    assert_eq!(
        reported, moved_in_mod,
        "report set must equal the go.mod-diff set (no silent change)\nreported={reported:?}\ngo.mod-diff={moved_in_mod:?}"
    );
}

#[test]
fn outdated_agrees_with_upgrade() {
    skip_if_missing!("go");
    let fixture = upgrade_fixture();

    // Converge first so `outdated` and `upgrade` describe the same stable state.
    fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--transitive"]);
    let blocked = outdated.outdated_with_status("blocked");
    let adoptable = outdated.outdated_with_status("adoptable");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let held = upgrade.held_conflict_names();

    // Everything `upgrade` reports held, `outdated` must mark blocked. (Go's `outdated --transitive`
    // can additionally flag candidates `upgrade` never plans — e.g. tooling-only modules outside the
    // compile graph — so `blocked` is a superset, not a strict equal.)
    assert!(
        held.is_subset(&blocked),
        "every held candidate must be blocked by outdated\nheld={held:?}\nblocked={blocked:?}"
    );
    // Nothing `outdated` calls adoptable may be one `upgrade` holds.
    assert!(
        adoptable.is_disjoint(&held),
        "nothing outdated calls adoptable may be held by upgrade\nadoptable={adoptable:?}\nheld={held:?}"
    );
}

#[test]
fn upgrade_dry_run_agrees_with_real_upgrade() {
    skip_if_missing!("go");

    // Real upgrade converges one fixture; the held set on the converged state is the real held set.
    let real_fixture = upgrade_fixture();
    real_fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();
    let real = real_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let real_held = real.held_conflict_names();

    // Dry-run on a separate converged fixture: the held set must match and go.mod is untouched.
    let dry_fixture = upgrade_fixture();
    dry_fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();
    let mod_before = dry_fixture.read_bytes("go.mod");
    let dry = dry_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let dry_held = dry.held_conflict_names();
    let mod_after = dry_fixture.read_bytes("go.mod");

    assert_eq!(
        real_held, dry_held,
        "dry-run held set must equal the real upgrade held set\nreal={real_held:?}\ndry={dry_held:?}"
    );
    assert_eq!(
        mod_before, mod_after,
        "--dry-run must leave go.mod byte-identical"
    );
    assert_eq!(
        dry.lock_verified(),
        None,
        "--dry-run never re-locks, so lockVerified is null"
    );
}

#[test]
fn fix_is_idempotent_and_does_not_error() {
    skip_if_missing!("go");

    // Seed at a newer k8s line (v0.31.0) and evaluate under the earlier FREEZE_FIX: several deps are
    // younger than that cutoff and are cooldown violations. `fix` re-resolves the whole graph down to
    // the matured set in one batched pass.
    let fixture = Fixture::new();
    seed(&fixture, "v0.31.0");

    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE_FIX]);
    assert!(
        fixed.ok(),
        "fix should succeed: {}",
        fixture
            .cooldown(&["fix", "--freeze", FREEZE_FIX])
            .stderr_str()
    );
    assert_eq!(fixed.lock_verified(), Some(true), "fix re-locks cleanly");
    assert_eq!(fixed.summary_errors(), 0, "fix should not error");

    let mod_after_fix = fixture.read_bytes("go.mod");
    let sum_after_fix = fixture.read_bytes("go.sum");

    // Re-running fix is idempotent: whatever fix could reduce it already reduced, and whatever the
    // graph holds (MVS floors that cannot be lowered) it leaves — so nothing new is applied and the
    // lock is byte-identical, the fixed point.
    let again = fixture.cooldown_json(&["fix", "--freeze", FREEZE_FIX]);
    assert_eq!(
        again.summary_applied(),
        0,
        "second fix must be a no-op (idempotent)"
    );
    assert_eq!(
        mod_after_fix,
        fixture.read_bytes("go.mod"),
        "second fix must leave go.mod byte-identical"
    );
    assert_eq!(
        sum_after_fix,
        fixture.read_bytes("go.sum"),
        "second fix must leave go.sum byte-identical"
    );
}

/// The build-gate fixture: `k8s.io/client-go` pinned at v0.29.0 against `k8s.io/apimachinery`
/// v0.34.0. The two are API-incompatible (client-go v0.29's apply-configuration code references
/// `structured-merge-diff/v4` while apimachinery v0.34 moved to `/v6`), so `go build ./...` fails on
/// the joint resolve. A real git repo is initialized so `go build` does not fail on VCS stamping for
/// an unrelated reason — isolating the failure to the genuine API break.
const CLIENT_GO_MAIN: &str = r#"package main

import "k8s.io/client-go/kubernetes"

func main() {
	var _ *kubernetes.Clientset
}
"#;

#[test]
fn build_gate_rejects_an_api_incompatible_joint_resolve() {
    skip_if_missing!("go");
    skip_if_missing!("git");

    let fixture = Fixture::new();
    fixture.write(
        "go.mod",
        "module example.com/cooldown-go-buildgate\n\ngo 1.23\n\nrequire (\n\tk8s.io/apimachinery v0.34.0\n\tk8s.io/client-go v0.29.0\n)\n",
    );
    fixture.write("main.go", CLIENT_GO_MAIN);
    fixture
        .run_tool("go", &["mod", "tidy"], &[("GOFLAGS", "")])
        .expect_success();
    // A git repo so `go build ./...` stamps VCS cleanly; the only build failure is the API break.
    fixture
        .run_tool("git", &["init", "-q"], &[])
        .expect_success();

    // The incompatible pair compiles-breaks: the `--build` gate (`go build ./...`, wired via
    // finalize) must report the failure rather than declaring success on an uncompilable lock.
    let report = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--build"]);
    assert_eq!(
        report.lock_verified(),
        Some(true),
        "the resolve itself re-locks; the failure is the compile gate, not the lock"
    );
    assert!(
        !report.ok(),
        "the API-incompatible joint resolve must not be reported ok"
    );
    assert!(
        report.summary_errors() >= 1,
        "the build gate must surface the compile break as an error, got {}",
        report.summary_errors()
    );
}
