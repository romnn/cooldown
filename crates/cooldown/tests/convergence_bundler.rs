//! End-to-end convergence tests that drive the REAL `bundle` against `Gemfile.lock` fixtures
//! generated on the fly in temp dirs.
//!
//! They hold the Bundler adapter to the contract the other suites hold their adapters to: an
//! `upgrade` or `fix` lands every planned change exactly on cooldown's target, moves nothing it
//! did not report, converges, and its `--dry-run` reports the same rows without touching a file.
//! None of them installs a gem: every fixture points Bundler at a private gem install path, which
//! the tests check stays empty, and an [`InstallProbe`] records any gem installation inside the
//! Ruby cooldown runs.
//!
//! # Determinism
//!
//! Every test pins the cooldown window with `--freeze`, and the `RubyGems` history before that
//! instant is immutable, so cooldown's targets reproduce forever.
//! Bundler itself has no publish-date cutoff, so the seed lock is pinned instead: the fixture first
//! locks a `Gemfile` that pins every gem exactly, then relaxes it to the real requirements and
//! relocks, which keeps the seeded versions because they still satisfy them.
//! The assertions check invariants (landing on the planned target, no silent change, convergence,
//! cross-command agreement), never hard-coded versions, except where a test is about one release.
//!
//! # The fixtures
//!
//! `i18n` is a pure-Ruby gem that depends on `concurrent-ruby (~> 1.0)`, and both have published
//! within their major line on both sides of `FREEZE`.
//! Seeded at `i18n` 1.8.11 and `concurrent-ruby` 1.1.9, `i18n` has a matured within-major upgrade
//! (1.14.5) and newer releases still in cooldown (1.14.6 and the 1.15 line).
//! Seeded at `i18n` 1.14.6 and `concurrent-ruby` 1.3.4, both are younger than `FREEZE`, so `fix`
//! has to mature them back down (to 1.14.5 and 1.3.1).
//! Nothing in this graph caps either gem, so a hold or a blocked verdict is never legitimate here.
//!
//! `i18n` 1.14.3 alone also requires `racc (~> 1.7)`, which `RACC_FREEZE` makes its newest matured
//! release, while `racc` 1.8 is still in cooldown then.
//!
//! `rspec-core` pins `rspec-support` to its own minor line, so the two only move to a new minor
//! together.
//! `sorbet` pins `sorbet-static` exactly, which has no build for the `ruby` platform, and its first
//! build for `aarch64-linux` is 0.5.11267.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use std::collections::{BTreeMap, BTreeSet};

use indoc::{formatdoc, indoc};
use support::{Envelope, Fixture, changed_packages, project_files};

/// The absolute cooldown cutoff most tests evaluate under.
const FREEZE: &str = "2024-06-01T00:00:00Z";

/// A cutoff between `i18n` 1.14.3 and 1.14.4, so 1.14.3 is `i18n`'s newest matured release.
const RACC_FREEZE: &str = "2024-03-06T12:00:00Z";

/// The Ruby library an [`InstallProbe`] loads into every Ruby `cooldown` starts.
///
/// Both `gem install` and Bundler start every gem installation with
/// `Gem::Installer#pre_install_checks`.
const INSTALL_PROBE: &str = indoc! {r#"
    # Leaves a marker beside this file when anything in the process starts installing a gem.
    require "rubygems/installer"
    Gem::Installer.prepend(Module.new do
      define_method(:pre_install_checks) do
        File.write(File.join(__dir__, "installed"), "")
        super()
      end
    end)
"#};

/// The real `Gemfile`: a within-major range on `i18n`, so cooldown may float it up to the newest
/// matured 1.x release.
const GEMFILE: &str = indoc! {r#"
    source "https://rubygems.org"

    gem "i18n", "~> 1.8"
"#};

/// [`GEMFILE`] with `racc` declared as well, so the lock holds it before `i18n` needs it.
const GEMFILE_WITH_RACC: &str = indoc! {r#"
    source "https://rubygems.org"

    gem "i18n", "~> 1.8"
    gem "racc"
"#};

/// The versions a fixture's starting lock resolves, as `(gem, version)` pairs.
type Seed = &'static [(&'static str, &'static str)];

/// A starting lock old enough that `i18n` has a matured upgrade at `FREEZE`.
const SEED_OLD: Seed = &[("i18n", "1.8.11"), ("concurrent-ruby", "1.1.9")];

/// A starting lock whose gems were both published after `FREEZE`, so both violate the cooldown.
const SEED_TOO_FRESH: Seed = &[("i18n", "1.14.6"), ("concurrent-ruby", "1.3.4")];

/// A starting lock whose `racc` is too old for `i18n` 1.14.3, while `concurrent-ruby` is already
/// at its newest release matured by `RACC_FREEZE`.
const SEED_RACC: Seed = &[
    ("i18n", "1.8.11"),
    ("concurrent-ruby", "1.2.3"),
    ("racc", "1.6.2"),
];

/// A project holding [`GEMFILE`] whose `Gemfile.lock` the real `bundle lock` resolved to `seed`.
fn fixture(seed: Seed) -> Fixture {
    fixture_with(GEMFILE, seed)
}

/// A project holding `gemfile` whose `Gemfile.lock` the real `bundle lock` resolved to `seed`.
fn fixture_with(gemfile: &str, seed: &[(&str, &str)]) -> Fixture {
    let fixture = Fixture::new();

    // Lock every gem at its seed version first, since Bundler cannot resolve as of a date.
    let gems: Vec<String> = seed
        .iter()
        .map(|(gem, version)| format!(r#"gem "{gem}", "= {version}""#))
        .collect();
    let pinned = formatdoc! {r#"
        source "https://rubygems.org"

        {gems}
    "#, gems = gems.join("\n")};
    fixture.write("Gemfile", &pinned);
    fixture.run_tool("bundle", &["lock"], &[]).expect_success();

    // Relax to the real requirements; Bundler keeps the locked versions since they still satisfy
    // them.
    fixture.write("Gemfile", gemfile);
    fixture.run_tool("bundle", &["lock"], &[]).expect_success();

    let pins = gemfile_lock_pins(&fixture.read_bytes("Gemfile.lock"));
    for (gem, version) in seed {
        assert_eq!(
            pins.get(*gem).map(String::as_str),
            Some(*version),
            "the relock must keep the seeded versions\n{pins:?}"
        );
    }
    fixture
}

/// A Ruby library that the `cooldown` runs it starts load into every Ruby they start, through
/// `RUBYOPT`, so that a gem installation anywhere in them leaves a marker.
///
/// The helper cooldown relocks with points Bundler at an install path of its own, which the
/// fixture's empty install path cannot speak for.
struct InstallProbe(tempfile::TempDir);

impl InstallProbe {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create the install probe directory");
        std::fs::write(dir.path().join("cooldown_install_probe.rb"), INSTALL_PROBE)
            .expect("write the install probe");
        Self(dir)
    }

    /// Runs `cooldown --json` in `fixture` with the probe loaded into every Ruby it starts.
    fn cooldown_json(&self, fixture: &Fixture, args: &[&str]) -> Envelope {
        self.cooldown_json_env(fixture, args, "", &[])
    }

    /// Runs `cooldown --json` in `fixture` with `envs` added, and with the probe loaded into every
    /// Ruby it starts after the Ruby options `preload`.
    fn cooldown_json_env(
        &self,
        fixture: &Fixture,
        args: &[&str],
        preload: &str,
        envs: &[(&str, &str)],
    ) -> Envelope {
        let lib = self.0.path().to_str().expect("a UTF-8 temp dir");
        let options = format!("{preload} -rcooldown_install_probe");
        let mut all = vec![("RUBYLIB", lib), ("RUBYOPT", options.trim_start())];
        all.extend_from_slice(envs);
        fixture.cooldown_json_env(args, &all)
    }

    /// Whether a Ruby the probe was loaded into started installing a gem.
    fn installed(&self) -> bool {
        self.0.path().join("installed").exists()
    }
}

/// The `name -> version` map of every gem a `Gemfile.lock` resolves.
///
/// A resolved gem is a `name (version)` line indented exactly four spaces inside a section's
/// `specs:` block, with one line per platform build (`name (version-platform)`); its own
/// requirements are indented further, and the `CHECKSUMS` section repeats `name (version)` at two
/// spaces.
fn gemfile_lock_pins(lock: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(lock);
    let mut pins = BTreeMap::new();
    let mut in_specs = false;
    for line in text.lines() {
        if line == "  specs:" {
            in_specs = true;
            continue;
        }
        // Any line indented less than a spec ends the block.
        let Some(spec) = line.strip_prefix("    ") else {
            in_specs = false;
            continue;
        };
        if !in_specs || spec.starts_with(' ') {
            continue;
        }
        if let Some((name, version)) = spec.split_once(" (") {
            let version = version.trim_end_matches(')');
            // A RubyGems version never holds a `-`, so one starts the platform.
            let version = version.split('-').next().unwrap_or(version);
            pins.insert(name.to_owned(), version.to_owned());
        }
    }
    pins
}

/// The requirements a `Gemfile.lock` records for the gem `spec` (`name (version)`), as the lines
/// indented under its spec line.
fn gemfile_lock_requirements(lock: &[u8], spec: &str) -> Vec<String> {
    let text = String::from_utf8_lossy(lock);
    let spec_line = format!("    {spec}");
    text.lines()
        .skip_while(|line| *line != spec_line)
        .skip(1)
        .take_while(|line| line.starts_with("      "))
        .map(|line| line.trim().to_owned())
        .collect()
}

/// One reported change row, `(name, from, to)`.
type Row = (String, String, String);

/// Every `(name, from, to)` change row of an `upgrade`/`fix` report, held proposals included.
fn rows(report: &Envelope) -> BTreeSet<Row> {
    rows_where(report, |_| true)
}

/// The `(name, from, to)` change rows of an `upgrade`/`fix` report that it applied.
fn applied_rows(report: &Envelope) -> BTreeSet<Row> {
    rows_where(report, |item| {
        item.get("applied")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    })
}

fn rows_where(report: &Envelope, keep: impl Fn(&serde_json::Value) -> bool) -> BTreeSet<Row> {
    report
        .items()
        .iter()
        .filter(|item| keep(item))
        .filter_map(|item| {
            let field = |key: &str| item.get(key)?.as_str().map(str::to_owned);
            Some((field("name")?, field("from")?, field("to")?))
        })
        .collect()
}

/// Asserts that every applied row of `report` landed in `lock` exactly at its target.
///
/// A gem the lock holds at any other version either stayed behind or overshot the target to a
/// release younger than the cooldown window.
fn assert_rows_landed_on_target(report: &Envelope, lock: &[u8]) {
    let pins = gemfile_lock_pins(lock);
    for (name, from, to) in applied_rows(report) {
        assert_eq!(
            pins.get(&name),
            Some(&to),
            "{name} was applied {from} -> {to} but the lock holds {:?}\n{report:#?}",
            pins.get(&name),
        );
    }
}

/// Asserts that nothing was installed into the gem install path every process of `fixture`,
/// cooldown included, is pointed at, and that no Ruby `probe` watched started installing a gem.
///
/// Locking a `Gemfile` of plain rubygems.org gems installs nothing, so any file there is a gem
/// cooldown installed.
fn assert_nothing_installed(fixture: &Fixture, probe: &InstallProbe) {
    let path = fixture.bundle_path();
    let installed: Vec<String> = if path.exists() {
        project_files(&path).into_keys().collect()
    } else {
        Vec::new()
    };
    assert!(
        installed.is_empty(),
        "cooldown installed into {}: {installed:?}",
        path.display()
    );
    assert!(!probe.installed(), "cooldown started installing a gem");
}

/// `check` cannot evaluate a Bundler project yet: the adapter has no lock-currency probe (#192), so
/// it fails closed with `lock_unknown` alone, checks nothing, and leaves every file as it was.
///
/// This pins today's behavior; it flips once Bundler's own verdict backs `check`.
#[test]
fn check_fails_closed_on_an_unverifiable_gemfile_lock() {
    skip_if_missing!("bundle");
    let fixture = fixture(SEED_OLD);
    let before = project_files(fixture.root());

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);

    assert!(!check.ok(), "{check:#?}");
    assert_eq!(
        check.error_kinds(),
        BTreeSet::from(["lock_unknown".to_owned()]),
        "{check:#?}"
    );
    assert_eq!(check.summary_checked(), 0, "{check:#?}");
    assert_eq!(project_files(fixture.root()), before);
}

/// `outdated` and `upgrade` describe the same plan: what `upgrade` holds `outdated` blocks, and the
/// unconstrained `i18n` is adoptable at exactly the target `upgrade` plans for it.
#[test]
fn outdated_agrees_with_upgrade_on_the_matured_target() {
    skip_if_missing!("bundle");
    let fixture = fixture(SEED_OLD);
    let before = project_files(fixture.root());
    let probe = InstallProbe::new();

    let outdated = probe.cooldown_json(&fixture, &["outdated", "--freeze", FREEZE, "--transitive"]);
    let upgrade = probe.cooldown_json(&fixture, &["upgrade", "--freeze", FREEZE, "--dry-run"]);

    // Both are read-only, and neither installs a gem to preview the upgrade.
    assert_eq!(project_files(fixture.root()), before);
    assert_nothing_installed(&fixture, &probe);

    // The agreement contract every suite asserts.
    let blocked = outdated.outdated_with_status("blocked");
    let adoptable = outdated.outdated_with_status("adoptable");
    let held = upgrade.held_conflict_names();
    assert!(
        held.is_subset(&blocked),
        "every held candidate must be blocked by outdated\nheld={held:?}\nblocked={blocked:?}"
    );
    assert!(
        adoptable.is_disjoint(&held),
        "nothing outdated calls adoptable may be held by upgrade\nadoptable={adoptable:?}\nheld={held:?}"
    );

    // Nothing in this graph caps `i18n`, so its matured upgrade is adoptable, not blocked, and
    // newer releases remain in cooldown past it.
    assert!(
        adoptable.contains("i18n") && !blocked.contains("i18n"),
        "i18n must be adoptable\nadoptable={adoptable:?}\nblocked={blocked:?}\n{outdated:#?}"
    );
    let target = outdated
        .item_field_str("i18n", "adoptableTarget")
        .expect("i18n has an adoptable target");
    assert_ne!(
        outdated.latest_version_for("i18n").as_deref(),
        Some(target.as_str()),
        "a newer i18n must still be in cooldown at the freeze"
    );
    let planned = upgrade.change_for("i18n").expect("upgrade plans i18n");
    assert_eq!(planned.to, target, "upgrade must plan outdated's target");
}

/// A real `upgrade` lands every planned change exactly on its matured target, never past it into
/// the cooldown window, and leaves no candidate held in a graph that caps nothing.
/// It rewrites only the lock and installs nothing.
#[test]
fn upgrade_lands_each_planned_change_on_its_target() {
    skip_if_missing!("bundle");
    let fixture = fixture(SEED_OLD);
    let probe = InstallProbe::new();

    let upgrade = probe.cooldown_json(&fixture, &["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.lock_status(),
        Some("unknown"),
        "Bundler re-locks, but cooldown cannot prove Gemfile.lock currency yet (#192)"
    );
    assert!(
        upgrade.applied_names().contains("i18n"),
        "the matured i18n upgrade must apply\n{upgrade:#?}"
    );
    assert!(
        upgrade.held_conflict_names().is_empty(),
        "nothing in this graph caps a gem, so nothing may be held\n{upgrade:#?}"
    );
    assert_rows_landed_on_target(&upgrade, &fixture.read_bytes("Gemfile.lock"));
    assert_eq!(fixture.read_bytes("Gemfile"), GEMFILE.as_bytes());
    assert_nothing_installed(&fixture, &probe);
}

/// Every gem whose locked version moved is a reported, applied change, and the move is not empty.
#[test]
fn upgrade_reports_every_moved_version_no_silent_change() {
    skip_if_missing!("bundle");
    let fixture = fixture(SEED_OLD);
    let lock_before = fixture.read_bytes("Gemfile.lock");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(upgrade.ok(), "{upgrade:#?}");

    let moved = changed_packages(
        &lock_before,
        &fixture.read_bytes("Gemfile.lock"),
        gemfile_lock_pins,
    );
    assert!(
        !moved.is_empty(),
        "the upgrade should have moved i18n\n{upgrade:#?}"
    );
    assert_eq!(
        upgrade.applied_names(),
        moved,
        "the applied set must equal the lock diff (no silent change)"
    );
}

/// A second `upgrade` after the first finds nothing to do and rewrites nothing.
#[test]
fn upgrade_converges_to_a_fixed_point() {
    skip_if_missing!("bundle");
    let fixture = fixture(SEED_OLD);

    let first = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(first.ok(), "{first:#?}");
    assert!(
        first.summary_applied() >= 1,
        "the first upgrade should apply the matured i18n move\n{first:#?}"
    );
    let lock_after_first = fixture.read_bytes("Gemfile.lock");

    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "the second upgrade must be a no-op\n{second:#?}"
    );
    assert!(
        rows(&second).is_empty(),
        "a converged lock leaves nothing to plan\n{second:#?}"
    );
    assert_eq!(
        lock_after_first,
        fixture.read_bytes("Gemfile.lock"),
        "the lock must be byte-identical across the two converged runs"
    );
}

/// `upgrade --dry-run` reports the same rows and holds as the real run on an identical fixture,
/// and leaves every project file byte-identical, while the real run lands them all.
#[test]
fn upgrade_dry_run_agrees_with_real_upgrade() {
    skip_if_missing!("bundle");

    let probe = InstallProbe::new();
    let dry_fixture = fixture(SEED_OLD);
    let before = project_files(dry_fixture.root());
    let dry = probe.cooldown_json(&dry_fixture, &["upgrade", "--freeze", FREEZE, "--dry-run"]);
    assert_eq!(project_files(dry_fixture.root()), before);
    assert_eq!(dry.lock_status(), None, "--dry-run never re-locks");
    assert_nothing_installed(&dry_fixture, &probe);

    let real_fixture = fixture(SEED_OLD);
    let real = probe.cooldown_json(&real_fixture, &["upgrade", "--freeze", FREEZE]);

    // The real run succeeds, so the two cannot merely agree on the same failure.
    assert!(real.ok(), "{real:#?}");
    assert!(
        real.applied_names().contains("i18n"),
        "the real run must land i18n\n{real:#?}"
    );
    assert_rows_landed_on_target(&real, &real_fixture.read_bytes("Gemfile.lock"));
    assert_eq!(real_fixture.read_bytes("Gemfile"), GEMFILE.as_bytes());
    assert_nothing_installed(&real_fixture, &probe);

    assert!(
        !rows(&dry).is_empty(),
        "the dry run must plan i18n\n{dry:#?}"
    );
    assert_eq!(
        rows(&dry),
        rows(&real),
        "the dry run must report the real run's rows\ndry={dry:#?}\nreal={real:#?}"
    );
    assert_eq!(
        dry.held_conflict_names(),
        real.held_conflict_names(),
        "the dry run must hold what the real run holds"
    );
}

/// `fix` matures every too-fresh gem down to its target, reports each move as a downgrade, and a
/// second `fix` finds nothing left to do.
#[test]
fn fix_matures_too_fresh_gems_to_their_targets_and_is_idempotent() {
    skip_if_missing!("bundle");
    let fixture = fixture(SEED_TOO_FRESH);
    let probe = InstallProbe::new();

    let fixed = probe.cooldown_json(&fixture, &["fix", "--freeze", FREEZE]);

    assert!(fixed.ok(), "{fixed:#?}");
    assert_eq!(fixed.summary_errors(), 0, "{fixed:#?}");
    let applied = fixed.applied_names();
    assert!(
        applied.contains("i18n") && applied.contains("concurrent-ruby"),
        "fix must mature both too-fresh gems\n{fixed:#?}"
    );
    assert_eq!(
        fixed.downgraded_names(),
        applied,
        "every fix move is a downgrade"
    );
    assert_rows_landed_on_target(&fixed, &fixture.read_bytes("Gemfile.lock"));
    assert_eq!(fixture.read_bytes("Gemfile"), GEMFILE.as_bytes());
    assert_nothing_installed(&fixture, &probe);
    let lock_after_fix = fixture.read_bytes("Gemfile.lock");

    let again = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert_eq!(
        again.summary_applied(),
        0,
        "the second fix must be a no-op\n{again:#?}"
    );
    assert_eq!(
        lock_after_fix,
        fixture.read_bytes("Gemfile.lock"),
        "the second fix must leave the lock byte-identical"
    );
}

/// `upgrade` advances a gem no `Gemfile` declares: with `i18n` already at its newest matured
/// release, only the transitive `concurrent-ruby` moves, and it lands exactly on its target.
#[test]
fn upgrade_advances_a_transitive_gem_on_its_own() {
    skip_if_missing!("bundle");
    let fixture = fixture(&[("i18n", "1.14.5"), ("concurrent-ruby", "1.1.9")]);
    let lock_before = fixture.read_bytes("Gemfile.lock");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.applied_names(),
        BTreeSet::from(["concurrent-ruby".to_owned()]),
        "only the transitive has a matured upgrade\n{upgrade:#?}"
    );
    let lock_after = fixture.read_bytes("Gemfile.lock");
    assert_rows_landed_on_target(&upgrade, &lock_after);
    assert_eq!(
        changed_packages(&lock_before, &lock_after, gemfile_lock_pins),
        upgrade.applied_names(),
        "the applied set must equal the lock diff (no silent change)"
    );
}

/// The lock records the requirements a target release declares, and its checksum, as Bundler
/// derives them from the release itself: `rspec-core` 3.12 holds `rspec-support` on 3.12, and 3.13
/// on 3.13.
#[test]
fn a_relock_records_the_requirements_and_checksum_of_each_target() {
    skip_if_missing!("bundle");
    let gemfile = indoc! {r#"
        source "https://rubygems.org"

        gem "rspec-core", "~> 3.12"
    "#};
    let fixture = fixture_with(
        gemfile,
        &[("rspec-core", "3.12.3"), ("rspec-support", "3.12.2")],
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.applied_names(),
        BTreeSet::from(["rspec-core".to_owned(), "rspec-support".to_owned()]),
        "{upgrade:#?}"
    );
    let lock = fixture.read_bytes("Gemfile.lock");
    assert_rows_landed_on_target(&upgrade, &lock);

    let core = upgrade
        .applied_change_for("rspec-core")
        .expect("rspec-core applied");
    let text = String::from_utf8_lossy(&lock);
    assert!(
        gemfile_lock_requirements(&lock, &format!("rspec-core ({})", core.to))
            .contains(&"rspec-support (~> 3.13.0)".to_owned()),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|line| line.starts_with(&format!("  rspec-core ({}) sha256=", core.to))),
        "{text}"
    );
}

/// A target that drags another gem into the cooldown window does not leave it there: upgrading
/// `i18n` alone to 1.14.3 makes Bundler move `racc` to a release still in cooldown, which cooldown
/// reports and matures back down, so the run leaves nothing for `fix` to do.
#[test]
fn a_too_fresh_collateral_move_is_reported_and_matured() {
    skip_if_missing!("bundle");
    let fixture = fixture_with(GEMFILE_WITH_RACC, SEED_RACC);
    let lock_before = fixture.read_bytes("Gemfile.lock");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", RACC_FREEZE, "--package", "i18n"]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert!(
        upgrade.applied_names().contains("i18n"),
        "the i18n upgrade must apply\n{upgrade:#?}"
    );
    let lock_after = fixture.read_bytes("Gemfile.lock");
    assert_rows_landed_on_target(&upgrade, &lock_after);

    // The forced `racc` move is no silent change.
    let moved = changed_packages(&lock_before, &lock_after, gemfile_lock_pins);
    assert!(moved.contains("racc"), "{upgrade:#?}");
    assert_eq!(upgrade.applied_names(), moved, "{upgrade:#?}");

    // Nothing the run left in the lock is younger than the cutoff.
    let fix = fixture.cooldown_json(&["fix", "--freeze", RACC_FREEZE, "--dry-run"]);
    assert!(rows(&fix).is_empty(), "{fix:#?}");
}

/// A target Bundler cannot land is held on its own, while the targets beside it still land:
/// the `Gemfile` keeps `racc` on 1.6, below the release cooldown plans for it.
#[test]
fn a_conflicting_target_is_held_while_the_others_land() {
    skip_if_missing!("bundle");
    let gemfile = indoc! {r#"
        source "https://rubygems.org"

        gem "i18n", "~> 1.8"
        gem "racc", "~> 1.6.0"
    "#};
    let fixture = fixture_with(
        gemfile,
        &[
            ("i18n", "1.8.11"),
            ("concurrent-ruby", "1.1.9"),
            ("racc", "1.6.2"),
        ],
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    let applied = upgrade.applied_names();
    assert!(
        applied.contains("i18n") && !applied.contains("racc"),
        "i18n must land and racc must not\n{upgrade:#?}"
    );
    assert!(
        !upgrade.skipped_reasons_for("racc").is_empty(),
        "racc must be reported held\n{upgrade:#?}"
    );
    let lock = fixture.read_bytes("Gemfile.lock");
    assert_rows_landed_on_target(&upgrade, &lock);
    assert_eq!(
        gemfile_lock_pins(&lock).get("racc").map(String::as_str),
        Some("1.6.2")
    );
}

/// Output Bundler or the `Gemfile` prints never reaches the lock: with Bundler verbose and a
/// `Gemfile` that writes to standard output, the upgrade lands and Bundler reads the lock it
/// published as current.
#[test]
fn bundler_and_gemfile_output_never_reaches_the_lock() {
    skip_if_missing!("bundle");
    let gemfile = indoc! {r#"
        source "https://rubygems.org"
        puts "======="

        gem "i18n", "~> 1.8"
    "#};
    let fixture = fixture_with(gemfile, SEED_OLD);

    let upgrade = fixture.cooldown_json_env(
        &["upgrade", "--freeze", FREEZE],
        &[("BUNDLE_VERBOSE", "true")],
    );

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert!(upgrade.applied_names().contains("i18n"), "{upgrade:#?}");
    let lock = fixture.read_bytes("Gemfile.lock");
    assert_rows_landed_on_target(&upgrade, &lock);
    assert!(
        lock.starts_with(b"GEM\n"),
        "{}",
        String::from_utf8_lossy(&lock)
    );
    // Bundler parses the published lock and finds nothing to change in it.
    fixture.run_tool("bundle", &["lock"], &[]).expect_success();
    assert_eq!(fixture.read_bytes("Gemfile.lock"), lock);
}

/// A `Gemfile` that names another lock is refused rather than relocked into `Gemfile.lock`, whose
/// `racc` would otherwise take the other lock's older pin.
#[test]
fn a_gemfile_naming_another_lock_is_refused() {
    skip_if_missing!("bundle");
    let seed = |racc| {
        fixture_with(
            GEMFILE_WITH_RACC,
            &[
                ("i18n", "1.8.11"),
                ("concurrent-ruby", "1.1.9"),
                ("racc", racc),
            ],
        )
    };
    let other = seed("1.7.3").read_bytes("Gemfile.lock");
    let fixture = seed("1.8.0");
    fixture.write(
        "other.lock",
        std::str::from_utf8(&other).expect("a UTF-8 lock"),
    );
    fixture.write(
        "Gemfile",
        &formatdoc! {r#"
            {GEMFILE_WITH_RACC}
            lockfile "other.lock"
        "#},
    );
    let before = project_files(fixture.root());

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.summary_errors() > 0, "{upgrade:#?}");
    assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
    assert_eq!(project_files(fixture.root()), before);
}

/// A project at [`SEED_OLD`] whose lock records Bundler `version`.
fn fixture_locked_with_bundler(version: &str) -> Fixture {
    let fixture = fixture(SEED_OLD);
    let lock = String::from_utf8(fixture.read_bytes("Gemfile.lock")).expect("a UTF-8 lock");
    // `BUNDLED WITH` is the last section Bundler writes.
    let (sections, _) = lock
        .split_once("BUNDLED WITH\n")
        .expect("the lock records its Bundler");
    fixture.write(
        "Gemfile.lock",
        &format!("{sections}BUNDLED WITH\n  {version}\n"),
    );
    fixture
}

/// A lock recording a Bundler version that is not installed still relocks with the installed one.
#[test]
fn upgrade_lands_when_the_locked_bundler_is_not_installed() {
    skip_if_missing!("bundle");
    // Bundler 4.0.1 is a real release that no tested Ruby ships.
    let fixture = fixture_locked_with_bundler("4.0.1");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert!(upgrade.applied_names().contains("i18n"), "{upgrade:#?}");
    assert_rows_landed_on_target(&upgrade, &fixture.read_bytes("Gemfile.lock"));
}

/// A lock recording another installed Bundler version relocks with that version, the one Ruby
/// loads for the project.
#[test]
fn upgrade_lands_when_the_locked_bundler_is_another_installed_one() {
    skip_if_missing!("bundle");
    skip_if_missing!("gem");
    let fixture = fixture_locked_with_bundler("4.0.9");
    let gems = tempfile::tempdir().expect("create a gem home");
    let gem_home = gems.path().to_str().expect("a UTF-8 temp dir");
    fixture
        .run_tool(
            "gem",
            &[
                "install",
                "bundler",
                "--version",
                "4.0.9",
                "--install-dir",
                gem_home,
                "--no-document",
            ],
            &[],
        )
        .expect_success();

    let upgrade =
        fixture.cooldown_json_env(&["upgrade", "--freeze", FREEZE], &[("GEM_HOME", gem_home)]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert!(upgrade.applied_names().contains("i18n"), "{upgrade:#?}");
    let lock = fixture.read_bytes("Gemfile.lock");
    assert_rows_landed_on_target(&upgrade, &lock);
    // A newer Bundler would have recorded its own release, so the lock still naming 4.0.9 shows
    // that RubyGems loaded the locked one.
    let lock = String::from_utf8_lossy(&lock);
    assert!(lock.ends_with("BUNDLED WITH\n  4.0.9\n"), "{lock}");
}

/// A `.bundle/config` naming another Gemfile is not followed: the relock resolves the project's
/// own `Gemfile`, so the other one's `rake` never reaches the lock.
#[test]
fn a_configured_gemfile_redirect_is_never_followed() {
    skip_if_missing!("bundle");
    let fixture = fixture(SEED_OLD);
    fixture.write(
        "other/Gemfile",
        indoc! {r#"
            source "https://rubygems.org"

            gem "i18n", "~> 1.8"
            gem "rake"
        "#},
    );
    fixture.write(
        ".bundle/config",
        indoc! {r#"
            ---
            BUNDLE_GEMFILE: "other/Gemfile"
        "#},
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    let lock = fixture.read_bytes("Gemfile.lock");
    assert_rows_landed_on_target(&upgrade, &lock);
    assert!(!gemfile_lock_pins(&lock).contains_key("rake"));
    assert!(!fixture.root().join("other/Gemfile.lock").exists());
}

/// A Git source, which Bundler checks out under its install path while it resolves, is checked out
/// outside both the install path the project configures and the one the environment names, even
/// when cooldown runs under `bundle exec`, whose preloaded Bundler has already settled on the
/// shared path.
#[test]
fn a_git_source_is_never_checked_out_into_a_configured_install_path() {
    skip_if_missing!("bundle");
    skip_if_missing!("git");
    let gemfile = indoc! {r#"
        source "https://rubygems.org"

        gem "i18n", "~> 1.8"
        gem "ostruct", git: "https://github.com/ruby/ostruct.git", tag: "v0.6.1"
    "#};
    let fixture = fixture_with(gemfile, SEED_OLD);
    // Seeding checked the source out into the fixture's install path, so it starts empty again,
    // and the project's own configuration, which outranks the environment, names another one.
    std::fs::remove_dir_all(fixture.bundle_path()).expect("empty the install path");
    fixture.write(
        ".bundle/config",
        indoc! {r#"
            ---
            BUNDLE_PATH: "vendor/bundle"
        "#},
    );
    // The bundle `bundle exec` would have set up: an empty one, so nothing has to be installed.
    let outer = Fixture::new();
    outer.write(
        "Gemfile",
        indoc! {r#"
            source "https://rubygems.org"
        "#},
    );
    outer.run_tool("bundle", &["lock"], &[]).expect_success();
    let outer_gemfile = outer.root().join("Gemfile");
    let probe = InstallProbe::new();

    let upgrade = probe.cooldown_json_env(
        &fixture,
        &["upgrade", "--freeze", FREEZE],
        "-rbundler/setup",
        &[(
            "BUNDLE_GEMFILE",
            outer_gemfile.to_str().expect("a UTF-8 temp dir"),
        )],
    );

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert!(upgrade.applied_names().contains("i18n"), "{upgrade:#?}");
    assert_rows_landed_on_target(&upgrade, &fixture.read_bytes("Gemfile.lock"));
    assert!(!fixture.root().join("vendor").exists());
    assert_nothing_installed(&fixture, &probe);
}

/// A seeded `sorbet` project locked for both `x86_64-linux` and `aarch64-linux`.
#[cfg(not(windows))]
fn sorbet_fixture(version: &str) -> Fixture {
    let fixture = fixture_with(
        indoc! {r#"
            source "https://rubygems.org"

            gem "sorbet"
        "#},
        &[("sorbet", version), ("sorbet-static", version)],
    );
    fixture
        .run_tool(
            "bundle",
            &[
                "lock",
                "--add-platform",
                "x86_64-linux",
                "--add-platform",
                "aarch64-linux",
            ],
            &[],
        )
        .expect_success();
    fixture
}

/// A target that has no build for a platform the lock lists is held, even when the gem lacking it
/// is a transitive one, for which Bundler itself only asks for a build on some platform.
/// Maturing `sorbet` to 0.5.11266 takes `sorbet-static` to 0.5.11266, which has no `aarch64-linux`
/// build.
#[cfg(not(windows))]
#[test]
fn a_target_missing_a_locked_platform_is_held() {
    skip_if_missing!("bundle");
    let fixture = sorbet_fixture("0.5.11268");
    let lock_before = fixture.read_bytes("Gemfile.lock");

    let fixed = fixture.cooldown_json(&["fix", "--freeze", "2024-02-23T12:00:00Z"]);

    assert!(fixed.applied_names().is_empty(), "{fixed:#?}");
    assert!(
        fixed
            .skip_detail_for("sorbet")
            .is_some_and(|detail| detail.contains("aarch64-linux")),
        "{fixed:#?}"
    );
    assert_eq!(fixture.read_bytes("Gemfile.lock"), lock_before);
}

/// A target with a build for every platform the lock lists lands with all of them.
#[cfg(not(windows))]
#[test]
fn a_target_supplying_every_locked_platform_lands() {
    skip_if_missing!("bundle");
    let fixture = sorbet_fixture("0.5.11276");

    let fixed = fixture.cooldown_json(&["fix", "--freeze", "2024-02-26T12:00:00Z"]);

    assert!(fixed.ok(), "{fixed:#?}");
    let applied = fixed.applied_names();
    assert!(
        applied.contains("sorbet") && applied.contains("sorbet-static"),
        "{fixed:#?}"
    );
    let lock = fixture.read_bytes("Gemfile.lock");
    assert_rows_landed_on_target(&fixed, &lock);
    let target = fixed
        .applied_change_for("sorbet-static")
        .expect("sorbet-static applied")
        .to;
    let text = String::from_utf8_lossy(&lock);
    for platform in ["x86_64-linux", "aarch64-linux"] {
        assert!(
            text.contains(&format!("    sorbet-static ({target}-{platform})\n")),
            "{text}"
        );
    }
}

/// The instant `rbi` 0.0.2 is too fresh and 0.0.1 is not.
#[cfg(not(windows))]
const RBI_FREEZE: &str = "2021-06-01T00:00:00Z";

/// A project whose `gems`, seeded at the given versions, Bundler needs only on Windows, locked for
/// `x64-mingw-ucrt` beside the platforms Bundler adds itself.
#[cfg(not(windows))]
fn windows_only_fixture(gems: &[(&str, &str)]) -> Fixture {
    let gemfile = |pinned: bool| {
        let lines: Vec<String> = gems
            .iter()
            .map(|(gem, version)| {
                let pin = if pinned {
                    format!(r#", "= {version}""#)
                } else {
                    String::new()
                };
                format!(r#"gem "{gem}"{pin}, platforms: :windows"#)
            })
            .collect();
        formatdoc! {r#"
            source "https://rubygems.org"

            {gems}
        "#, gems = lines.join("\n")}
    };
    let fixture = Fixture::new();
    // The platform is added while every gem is pinned, since adding one re-resolves the lock.
    fixture.write("Gemfile", &gemfile(true));
    fixture
        .run_tool("bundle", &["lock", "--add-platform", "x64-mingw-ucrt"], &[])
        .expect_success();
    fixture.write("Gemfile", &gemfile(false));
    fixture.run_tool("bundle", &["lock"], &[]).expect_success();
    let pins = gemfile_lock_pins(&fixture.read_bytes("Gemfile.lock"));
    for (gem, version) in gems {
        assert_eq!(
            pins.get(*gem).map(String::as_str),
            Some(*version),
            "{pins:?}"
        );
    }
    fixture
}

/// Asserts that maturing `rbi` in `fixture` was held for what it leaves missing on Windows, and
/// left the lock as `lock_before`.
#[cfg(not(windows))]
fn assert_held_for_windows(fixture: &Fixture, fixed: &Envelope, lock_before: &[u8]) {
    assert!(fixed.applied_names().is_empty(), "{fixed:#?}");
    assert!(
        fixed
            .skip_detail_for("rbi")
            .is_some_and(|detail| detail.contains("x64-mingw-ucrt")),
        "{fixed:#?}"
    );
    assert_eq!(fixture.read_bytes("Gemfile.lock"), lock_before);
}

/// A target can leave a gem missing on a platform the lock lists under a root that only that
/// platform needs, which Bundler, checking only the host's roots, lets through: maturing the
/// Windows-only `rbi` from 0.0.2 to 0.0.1 brings in `sorbet`, whose `sorbet-static` has no Windows
/// build, so the move is held.
#[cfg(not(windows))]
#[test]
fn a_windows_only_target_missing_a_windows_build_is_held() {
    skip_if_missing!("bundle");
    let fixture = windows_only_fixture(&[("rbi", "0.0.2")]);
    let lock_before = fixture.read_bytes("Gemfile.lock");

    let fixed = fixture.cooldown_json(&["fix", "--package", "rbi", "--freeze", RBI_FREEZE]);

    assert_held_for_windows(&fixture, &fixed, &lock_before);
}

/// A platform the lock already misses a gem on is no license to miss more there: the
/// Windows-only `sorbet` already misses `sorbet-static` on Windows, and maturing `rbi` to 0.0.1
/// makes `rbi` miss it as well, so the move is held.
#[cfg(not(windows))]
#[test]
fn a_target_missing_another_gem_on_an_incomplete_platform_is_held() {
    skip_if_missing!("bundle");
    let fixture = windows_only_fixture(&[("rbi", "0.0.2"), ("sorbet", "0.5.11268")]);
    let lock_before = fixture.read_bytes("Gemfile.lock");

    let fixed = fixture.cooldown_json(&["fix", "--package", "rbi", "--freeze", RBI_FREEZE]);

    assert_held_for_windows(&fixture, &fixed, &lock_before);
}

/// A mirror for rubygems.org can serve something else under a public gem's name and version, so
/// `upgrade` refuses to relock while one is configured, writes nothing, and never repeats the
/// mirror's address, which can carry credentials.
#[test]
fn a_rubygems_mirror_refuses_the_relock() {
    skip_if_missing!("bundle");
    let fixture = fixture(SEED_OLD);
    let before = project_files(fixture.root());
    let mirror = "https://user:secret@gems.example.invalid/";

    for key in [
        "BUNDLE_MIRROR__ALL",
        "BUNDLE_MIRROR__HTTPS://RUBYGEMS__ORG/",
        // Bundler reads a doubled trailing slash as the same address.
        "BUNDLE_MIRROR__HTTPS://RUBYGEMS__ORG//",
        "BUNDLE_MIRROR__RUBYGEMS__ORG",
    ] {
        let upgrade = fixture.cooldown_json_env(&["upgrade", "--freeze", FREEZE], &[(key, mirror)]);

        assert!(upgrade.summary_errors() > 0, "{key}: {upgrade:#?}");
        assert!(upgrade.applied_names().is_empty(), "{key}: {upgrade:#?}");
        assert!(
            !format!("{upgrade:?}").contains("secret"),
            "{key}: {upgrade:#?}"
        );
        assert_eq!(project_files(fixture.root()), before, "{key}");
    }
}

/// A mirror for another host does not stand in for rubygems.org, so the relock lands.
#[test]
fn a_mirror_for_another_host_leaves_the_relock_alone() {
    skip_if_missing!("bundle");
    let fixture = fixture(SEED_OLD);

    let upgrade = fixture.cooldown_json_env(
        &["upgrade", "--freeze", FREEZE],
        &[(
            "BUNDLE_MIRROR__HTTPS://GEMS__EXAMPLE__COM/",
            "https://gems.example.invalid/",
        )],
    );

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert!(upgrade.applied_names().contains("i18n"), "{upgrade:#?}");
    assert_rows_landed_on_target(&upgrade, &fixture.read_bytes("Gemfile.lock"));
}
