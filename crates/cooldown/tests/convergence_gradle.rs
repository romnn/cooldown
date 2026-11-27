//! End-to-end convergence tests that drive the REAL `gradle` against `gradle.lockfile` fixtures
//! generated on the fly in temp dirs.
//!
//! They hold the Gradle adapter to the contract the other suites hold their adapters to: an
//! `upgrade` or `fix` lands every planned change exactly on cooldown's target, moves nothing it
//! did not report, converges, and its `--dry-run` reports the same rows without touching a file.
//! Gradle's own resolution of every locked classpath is the check that Gradle accepts the lock
//! cooldown wrote, run through an init script in the fixture's Gradle home (see
//! [`gradle_resolved`]).
//! Every fixture gives Gradle a private user home with the daemon off (see the harness's
//! `tool_env`), so the caches Gradle fills and any init script or `gradle.properties` it reads are
//! never the user's, and no daemon outlives a test.
//!
//! # Determinism
//!
//! Every test pins the cooldown window with `--freeze`, and the Maven Central history before that
//! instant is immutable, so cooldown's targets reproduce forever.
//! A `build.gradle` that declares exact versions resolves the same graph forever, so Gradle's own
//! `dependencies --write-locks` seeds every lock.
//! The assertions check invariants (landing on the planned target, no silent change, convergence,
//! cross-command agreement, cooldown reading what Gradle resolves), never hard-coded versions,
//! except where a test is about one release.
//!
//! # Gradle's locking
//!
//! Gradle applies each `gradle.lockfile` entry as a strict constraint on its dependency, while a
//! version `build.gradle` declares is only the lowest one conflict resolution may select.
//! A lock entry above the declared version therefore lands on its own, but one below it leaves
//! Gradle no version that satisfies both, and it fails the resolve.
//!
//! # The fixtures
//!
//! `commons-text` depends on `commons-lang3`, neither has other runtime dependencies, and both have
//! published within their major line on both sides of `FREEZE`.
//! Declared at `commons-text` 1.10.0 and `commons-lang3` 3.12.0, both have a matured within-major
//! upgrade (1.12.0 and 3.14.0) and newer releases still in cooldown (1.13.0 and 3.15.0).
//! Declared at `commons-text` 1.13.0 and `commons-lang3` 3.17.0, both are younger than `FREEZE`, so
//! `fix` has to mature them back down.
//! Declared alone at 1.10.0, `commons-text` locks `commons-lang3` 3.12.0 as a transitive
//! dependency, while its target 1.12.0 requires `commons-lang3` 3.14.0.
//! Nothing caps an upgrade in this graph, so a hold or a blocked verdict is never legitimate here.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use std::collections::{BTreeMap, BTreeSet};

use indoc::{formatdoc, indoc};
use support::{CapturedOutput, Envelope, Fixture, project_files};

/// The absolute cooldown cutoff every test evaluates under.
const FREEZE: &str = "2024-06-01T00:00:00Z";

const TEXT: &str = "org.apache.commons:commons-text";
const LANG: &str = "org.apache.commons:commons-lang3";

/// The versions a fixture's `build.gradle` declares, as `(coordinate, version)` pairs.
type Seed = &'static [(&'static str, &'static str)];

/// A build old enough that both dependencies have a matured upgrade at `FREEZE`.
const SEED_OLD: Seed = &[(TEXT, "1.10.0"), (LANG, "3.12.0")];

/// A build whose dependencies were both published after `FREEZE`, so both violate the cooldown.
const SEED_TOO_FRESH: Seed = &[(TEXT, "1.13.0"), (LANG, "3.17.0")];

/// The `settings.gradle` of a single-project build; Gradle 9 refuses a build without one.
const SETTINGS: &str = "rootProject.name = 'demo'\n";

/// An init script that adds a `resolveLocked` task to every project.
///
/// The task resolves each locked classpath of its project, which fails when Gradle rejects the
/// lock, and then prints one `resolved <project path> <group:artifact:version>` line per module on
/// the runtime classpath.
const RESOLVE_LOCKED: &str = indoc! {r#"
    allprojects {
        tasks.register('resolveLocked') {
            def path = project.path
            def classpaths = ['compileClasspath', 'runtimeClasspath', 'testCompileClasspath', 'testRuntimeClasspath']
                .collect { project.configurations.findByName(it) }
                .findAll { it != null }
            doLast {
                classpaths.each { it.resolvedConfiguration.rethrowFailure() }
                def runtime = classpaths.find { it.name == 'runtimeClasspath' }
                runtime?.incoming?.resolutionResult?.allComponents?.each { component ->
                    def id = component.id
                    if (id instanceof ModuleComponentIdentifier) {
                        println "resolved ${path} ${id.group}:${id.module}:${id.version}"
                    }
                }
            }
        }
    }
"#};

/// A `build.gradle` for a Java library that resolves from Maven Central, locks every
/// configuration, and declares `seed` on `implementation`.
fn build_gradle(seed: Seed) -> String {
    let declared: Vec<String> = seed
        .iter()
        .map(|(coordinate, version)| format!("implementation '{coordinate}:{version}'"))
        .collect();
    formatdoc! {"
        plugins {{
            id 'java-library'
        }}

        repositories {{
            mavenCentral()
        }}

        dependencyLocking {{
            lockAllConfigurations()
        }}

        dependencies {{
            {declared}
        }}
    ", declared = declared.join("\n    ")}
}

/// Runs `gradle` with `args` at the fixture root and expects it to succeed.
///
/// Gradle keeps its project cache in the private Gradle home instead of a `.gradle/` beside the
/// build, so the fixture's own Gradle runs leave the project tree alone, and every file a test
/// finds there is one cooldown wrote.
fn gradle(fixture: &Fixture, args: &[&str]) -> CapturedOutput {
    let cache = fixture.gradle_user_home().join("project-cache");
    let cache = cache.to_str().expect("a UTF-8 temp dir");
    let mut full = vec!["-q", "--project-cache-dir", cache];
    full.extend_from_slice(args);
    fixture.run_tool("gradle", &full, &[]).expect_success()
}

/// A single-project build declaring `seed`, whose `gradle.lockfile` Gradle itself wrote.
fn fixture(seed: Seed) -> Fixture {
    let fixture = Fixture::new();
    fixture.write("settings.gradle", SETTINGS);
    fixture.write("build.gradle", &build_gradle(seed));
    gradle(&fixture, &["dependencies", "--write-locks"]);
    fixture
}

/// The version a `gradle.lockfile` locks each module at, by `group:artifact` and then
/// configuration, so a move in one configuration alone shows.
///
/// Gradle writes one entry per line, `group:artifact:version=configurations`, after its `#`
/// comments and before an `empty=` line that lists the configurations locking nothing.
fn gradle_lock_pins(lock: &[u8]) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut pins: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for line in String::from_utf8_lossy(lock).lines() {
        let Some((coordinate, configurations)) = line.split_once('=') else {
            continue;
        };
        let Some((name, version)) = coordinate.rsplit_once(':') else {
            continue;
        };
        if line.starts_with('#') || !name.contains(':') {
            continue;
        }
        for configuration in configurations.split(',') {
            pins.entry(name.to_owned())
                .or_default()
                .insert(configuration.to_owned(), version.to_owned());
        }
    }
    pins
}

/// What Gradle resolves on each project's runtime classpath, by project path (`:` for the root)
/// and then `group:artifact`.
///
/// It resolves every locked classpath of every project first, and fails, as this does, when Gradle
/// rejects a lock: a locked version the build cannot select, a resolved module the lock lacks, or
/// a locked one nothing reaches.
/// The `dependencies` report cannot serve as this check, since it marks such a failure `FAILED`
/// and still succeeds.
fn gradle_resolved(fixture: &Fixture) -> BTreeMap<String, BTreeMap<String, String>> {
    let home = fixture.gradle_user_home();
    std::fs::create_dir_all(&home).expect("create the Gradle home");
    let script = home.join("resolve-locked.gradle");
    std::fs::write(&script, RESOLVE_LOCKED).expect("write the init script");
    let script = script.to_str().expect("a UTF-8 temp dir");
    let listed = gradle(fixture, &["--init-script", script, "resolveLocked"]);
    let mut resolved: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for line in listed.stdout_str().lines() {
        let Some(entry) = line.strip_prefix("resolved ") else {
            continue;
        };
        let Some((project, coordinate)) = entry.split_once(' ') else {
            continue;
        };
        let Some((name, version)) = coordinate.rsplit_once(':') else {
            continue;
        };
        resolved
            .entry(project.to_owned())
            .or_default()
            .insert(name.to_owned(), version.to_owned());
    }
    resolved
}

/// One reported change row, `(name, from, to)`.
type Row = (String, String, String);

/// Every `(name, from, to)` change row of an `upgrade`/`fix` report, held proposals included.
fn rows(report: &Envelope) -> BTreeSet<Row> {
    rows_where(report, |_| true)
}

/// The `(name, from, to)` change rows of an `upgrade`/`fix` report that it applied.
fn applied_rows(report: &Envelope) -> BTreeSet<Row> {
    rows_where(report, is_applied)
}

/// The applied rows of `report` in `project` (`.` for the scan root).
fn applied_rows_in(report: &Envelope, project: &str) -> BTreeSet<Row> {
    rows_where(report, |item| {
        is_applied(item) && item.get("project").and_then(serde_json::Value::as_str) == Some(project)
    })
}

fn is_applied(item: &serde_json::Value) -> bool {
    item.get("applied")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
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

/// The names of `rows`, in order.
fn names(rows: &BTreeSet<Row>) -> Vec<&str> {
    rows.iter().map(|(name, _, _)| name.as_str()).collect()
}

/// What a report did with one row: `(project, name, from, to, outcome)`.
type Disposition = (String, String, String, String, String);

/// The disposition of every row of `report`, sorted, its outcome being `applied`, the reason a
/// held row was skipped for, or `error`.
///
/// A dry run and a real run agree when these agree: the same moves, in the same projects, landing
/// or held alike, and as often.
fn dispositions(report: &Envelope) -> Vec<Disposition> {
    let mut dispositions: Vec<Disposition> = report
        .items()
        .iter()
        .filter_map(|item| {
            let field = |key: &str| item.get(key)?.as_str().map(str::to_owned);
            let outcome = if is_applied(item) {
                "applied".to_owned()
            } else {
                item.get("skipped")
                    .and_then(|skipped| skipped.get("reason"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("error")
                    .to_owned()
            };
            Some((
                field("project")?,
                field("name")?,
                field("from")?,
                field("to")?,
                outcome,
            ))
        })
        .collect();
    dispositions.sort();
    dispositions
}

/// Whether the first row of `report` for `name` is a dependency a build script declares.
fn is_direct(report: &Envelope, name: &str) -> Option<bool> {
    report.items().iter().find_map(|item| {
        if item.get("name").and_then(serde_json::Value::as_str) != Some(name) {
            return None;
        }
        item.get("direct").and_then(serde_json::Value::as_bool)
    })
}

/// Asserts that the project holds exactly the files of `before`, each byte-identical.
///
/// The file names are compared first, so a file that appears, such as a `.gradle/` cache, is
/// named in the failure rather than buried in a byte dump.
fn assert_files_unchanged(fixture: &Fixture, before: &BTreeMap<String, Vec<u8>>) {
    let after = project_files(fixture.root());
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>(),
        "no project file may appear or vanish"
    );
    assert!(
        after == *before,
        "every project file must stay byte-identical"
    );
}

/// Asserts that a lock moved from `before` to `after` by exactly `rows`, each from the version the
/// lock held onto its target in every configuration that held it there.
///
/// Any other difference is a dependency that stayed behind, overshot its target into the cooldown
/// window, or moved without a row reporting it, in any one configuration.
fn assert_lock_moved_exactly(rows: &BTreeSet<Row>, before: &[u8], after: &[u8]) {
    let held = gradle_lock_pins(before);
    let mut expected = held.clone();
    for (name, from, to) in rows {
        // Each row moves what the lock held before, never what another row already moved.
        let configurations: Vec<&String> = held
            .get(name)
            .into_iter()
            .flatten()
            .filter(|(_, version)| *version == from)
            .map(|(configuration, _)| configuration)
            .collect();
        assert!(!configurations.is_empty(), "the lock held {name} at {from}");
        for configuration in configurations {
            expected
                .entry(name.clone())
                .or_default()
                .insert(configuration.clone(), to.clone());
        }
    }
    assert_eq!(
        gradle_lock_pins(after),
        expected,
        "the lock must move exactly the applied rows onto their targets\n{}",
        String::from_utf8_lossy(after)
    );
}

/// Asserts that Gradle, which accepted every lock to produce `resolved`, selects each of `rows` at
/// its target in the project at `path`.
fn assert_gradle_selects(
    resolved: &BTreeMap<String, BTreeMap<String, String>>,
    path: &str,
    rows: &BTreeSet<Row>,
) {
    for (name, from, to) in rows {
        assert_eq!(
            resolved.get(path).and_then(|project| project.get(name)),
            Some(to),
            "Gradle must resolve {name} at its target, not {from}, in {path}\nresolved={resolved:?}"
        );
    }
}

/// `check` cannot evaluate a Gradle project yet: the adapter has no lock-currency verdict for a
/// `gradle.lockfile`, so it fails closed with `lock_unknown` alone, checks nothing, and leaves
/// every file as it was.
///
/// This pins today's behavior; it flips once Gradle's own verdict backs `check`.
#[test]
fn check_fails_closed_on_an_unverifiable_gradle_lockfile() {
    skip_if_missing!("gradle");
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
    assert_files_unchanged(&fixture, &before);
}

/// `outdated` and `upgrade` describe the same plan: what `upgrade` holds `outdated` blocks, and
/// each dependency is adoptable at exactly the target `upgrade` plans for it, with newer releases
/// still in cooldown past it.
/// Both read the versions Gradle resolves, and neither writes a file.
#[test]
fn outdated_agrees_with_upgrade_on_the_matured_target() {
    skip_if_missing!("gradle");
    let fixture = fixture(SEED_OLD);
    let before = project_files(fixture.root());

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);

    assert_files_unchanged(&fixture, &before);

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

    let resolved = gradle_resolved(&fixture);
    for name in [TEXT, LANG] {
        assert_eq!(
            outdated.item_field_str(name, "current").as_ref(),
            resolved.get(":").and_then(|project| project.get(name)),
            "outdated must read the version Gradle resolves for {name}"
        );
        assert!(
            adoptable.contains(name) && !blocked.contains(name),
            "{name} must be adoptable\n{outdated:#?}"
        );
        let target = outdated
            .item_field_str(name, "adoptableTarget")
            .expect("an adoptable target");
        assert_ne!(
            outdated.latest_version_for(name).as_deref(),
            Some(target.as_str()),
            "a newer {name} must still be in cooldown at the freeze"
        );
        let planned = upgrade.change_for(name).expect("upgrade plans it");
        assert_eq!(planned.to, target, "upgrade must plan outdated's target");
    }
}

/// A real `upgrade` lands every planned change exactly on its matured target, never past it into
/// the cooldown window, leaves no candidate held in a graph that caps nothing, and reports every
/// version it moved.
/// A lock entry above the declared version lands on its own, so the upgrade rewrites the lock
/// alone and writes no other file (no `.gradle/` cache in the project), and Gradle accepts the lock
/// and resolves the targets.
/// A second `upgrade` then finds nothing to do and rewrites nothing.
#[test]
fn upgrade_lands_each_planned_change_on_its_target_and_converges() {
    skip_if_missing!("gradle");
    let fixture = fixture(SEED_OLD);
    let build_before = fixture.read_bytes("build.gradle");
    let lock_before = fixture.read_bytes("gradle.lockfile");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.lock_status(),
        Some("unknown"),
        "cooldown cannot prove a gradle.lockfile current yet"
    );
    assert!(
        upgrade.held_conflict_names().is_empty(),
        "nothing in this graph caps a dependency, so nothing may be held\n{upgrade:#?}"
    );
    assert_eq!(
        upgrade.applied_names(),
        BTreeSet::from([TEXT.to_owned(), LANG.to_owned()]),
        "both matured upgrades must apply\n{upgrade:#?}"
    );
    let lock_after = fixture.read_bytes("gradle.lockfile");
    assert_lock_moved_exactly(&applied_rows(&upgrade), &lock_before, &lock_after);
    let files: Vec<String> = project_files(fixture.root()).into_keys().collect();
    assert_eq!(
        files,
        ["build.gradle", "gradle.lockfile", "settings.gradle"],
        "the upgrade must write nothing beside the lock"
    );
    assert_eq!(fixture.read_bytes("build.gradle"), build_before);
    assert_gradle_selects(&gradle_resolved(&fixture), ":", &applied_rows(&upgrade));

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
        fixture.read_bytes("gradle.lockfile"),
        lock_after,
        "the lock must be byte-identical across the two converged runs"
    );
}

/// `upgrade --dry-run` disposes of every row as the real run on an identical fixture does,
/// landing or holding alike, and leaves every project file byte-identical, while the real run lands
/// them all.
#[test]
fn upgrade_dry_run_agrees_with_real_upgrade() {
    skip_if_missing!("gradle");
    // The settings name no root project, so Gradle names it after the build's directory, in each
    // copy cooldown makes as in the project.
    let unnamed = || {
        let fixture = fixture(SEED_OLD);
        fixture.write("settings.gradle", "");
        fixture
    };

    let dry_fixture = unnamed();
    let before = project_files(dry_fixture.root());
    let dry = dry_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    assert_files_unchanged(&dry_fixture, &before);
    assert_eq!(dry.lock_status(), None, "--dry-run never re-locks");

    let real_fixture = unnamed();
    let lock_before = real_fixture.read_bytes("gradle.lockfile");
    let real = real_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    // The real run succeeds, so the two cannot merely agree on the same failure.
    assert!(real.ok(), "{real:#?}");
    assert!(
        !real.applied_names().is_empty(),
        "the real run must land the matured upgrades\n{real:#?}"
    );
    assert_lock_moved_exactly(
        &applied_rows(&real),
        &lock_before,
        &real_fixture.read_bytes("gradle.lockfile"),
    );

    assert_eq!(
        dispositions(&dry),
        dispositions(&real),
        "the dry run must dispose of every row as the real run does\ndry={dry:#?}\nreal={real:#?}"
    );
}

/// `fix` relocks without editing a build script, so a target below the version `build.gradle`
/// declares is one Gradle cannot relock: each is held as the resolver's rejection, whose detail
/// carries the declaration Gradle names, and the project stays as it was, its violation still in
/// view.
#[test]
fn fix_holds_a_target_below_the_declared_version() {
    skip_if_missing!("gradle");
    let fixture = fixture(SEED_TOO_FRESH);
    let before = project_files(fixture.root());

    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);

    assert!(fixed.applied_names().is_empty(), "{fixed:#?}");
    for (name, declared) in SEED_TOO_FRESH {
        assert_eq!(
            fixed.skipped_reasons_for(name),
            BTreeSet::from(["resolver_conflict".to_owned()]),
            "{fixed:#?}"
        );
        let detail = fixed.skip_detail_for(name).expect("a detail");
        assert!(detail.contains(&format!("{name}:{declared}")), "{detail}");
    }
    assert_files_unchanged(&fixture, &before);
    // Both dependencies stay at their too-fresh versions, which `outdated` still reports.
    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    assert_eq!(
        outdated.outdated_with_status("in_cooldown"),
        BTreeSet::from([TEXT.to_owned(), LANG.to_owned()]),
        "{outdated:#?}"
    );
}

/// `fix` matures a too-fresh lock down to a target the build's declaration admits: with
/// `commons-text` declared at 1.10.0 and locked at 1.13.0, it lands on its target through the lock
/// alone, reports the move as a downgrade, moves no version it does not report, and Gradle accepts
/// the lock.
/// A second `fix` then finds nothing left to do and rewrites nothing.
#[test]
fn fix_matures_a_lock_down_to_a_target_its_declaration_admits() {
    skip_if_missing!("gradle");
    let fixture = fixture(&[(TEXT, "1.13.0")]);
    // A declaration below the locked version, as a lock moved on by a relock rather than by its
    // declaration leaves it.
    fixture.write("build.gradle", &build_gradle(&[(TEXT, "1.10.0")]));
    let build_before = fixture.read_bytes("build.gradle");
    let lock_before = fixture.read_bytes("gradle.lockfile");

    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);

    assert_eq!(fixed.summary_errors(), 0, "{fixed:#?}");
    assert!(
        fixed.downgraded_names().contains(TEXT),
        "fix must mature commons-text\n{fixed:#?}"
    );
    let lock_after = fixture.read_bytes("gradle.lockfile");
    assert_lock_moved_exactly(&applied_rows(&fixed), &lock_before, &lock_after);
    assert_eq!(fixture.read_bytes("build.gradle"), build_before);
    assert_gradle_selects(&gradle_resolved(&fixture), ":", &applied_rows(&fixed));

    let again = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert_eq!(
        again.summary_applied(),
        0,
        "the second fix must be a no-op\n{again:#?}"
    );
    assert_eq!(
        fixture.read_bytes("gradle.lockfile"),
        lock_after,
        "the second fix must leave the lock byte-identical"
    );
}

/// A dependency only the lock names is a transitive one, which `outdated --transitive` reports
/// as no declared dependency.
/// Gradle plans declared dependencies only, but the target `upgrade` lands for `commons-text`
/// requires a newer `commons-lang3` than the lock holds, so whatever the landing does to the
/// transitive must be reported: the lock moves exactly the applied rows, and Gradle accepts it.
#[test]
fn a_transitive_dependency_never_moves_unreported() {
    skip_if_missing!("gradle");
    let fixture = fixture(&[(TEXT, "1.10.0")]);
    let lock_before = fixture.read_bytes("gradle.lockfile");
    assert_eq!(
        gradle_lock_pins(&lock_before)
            .get(LANG)
            .and_then(|locked| locked.get("runtimeClasspath"))
            .map(String::as_str),
        Some("3.12.0"),
        "commons-text 1.10.0 locks commons-lang3 3.12.0 for itself"
    );

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--transitive"]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert_eq!(is_direct(&outdated, TEXT), Some(true), "{outdated:#?}");
    assert_eq!(is_direct(&outdated, LANG), Some(false), "{outdated:#?}");
    assert!(upgrade.ok(), "{upgrade:#?}");
    let landed = applied_rows(&upgrade);
    assert!(
        names(&landed).contains(&TEXT),
        "commons-text must land\n{upgrade:#?}"
    );
    assert_lock_moved_exactly(
        &landed,
        &lock_before,
        &fixture.read_bytes("gradle.lockfile"),
    );
    assert_gradle_selects(&gradle_resolved(&fixture), ":", &landed);
}

/// Gradle locks each project of a multi-project build in a `gradle.lockfile` of its own, which
/// `dependencies --write-locks` rewrites for the project it runs in alone.
/// So every locked project is a project to cooldown: `upgrade` at the root lands the root's rows
/// in the root's lock and the subproject's rows in the subproject's, and Gradle accepts both.
#[test]
fn each_project_of_a_multi_project_build_lands_in_its_own_lock() {
    skip_if_missing!("gradle");
    let fixture = Fixture::new();
    fixture.write(
        "settings.gradle",
        indoc! {"
            rootProject.name = 'demo'
            include 'app'
        "},
    );
    fixture.write("build.gradle", &build_gradle(&[(LANG, "3.12.0")]));
    fixture.write("app/build.gradle", &build_gradle(&[(TEXT, "1.10.0")]));
    // Gradle runs init scripts in a buildSrc build too, which holds no project at `:app`.
    fixture.write("buildSrc/build.gradle", "");
    gradle(
        &fixture,
        &["dependencies", ":app:dependencies", "--write-locks"],
    );
    let root_before = fixture.read_bytes("gradle.lockfile");
    let app_before = fixture.read_bytes("app/gradle.lockfile");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    // The root declares commons-lang3, which the subproject reaches only through commons-text.
    let root_rows = applied_rows_in(&upgrade, ".");
    let app_rows = applied_rows_in(&upgrade, "app");
    assert_eq!(names(&root_rows), [LANG], "{upgrade:#?}");
    assert!(names(&app_rows).contains(&TEXT), "{upgrade:#?}");
    assert_lock_moved_exactly(
        &root_rows,
        &root_before,
        &fixture.read_bytes("gradle.lockfile"),
    );
    assert_lock_moved_exactly(
        &app_rows,
        &app_before,
        &fixture.read_bytes("app/gradle.lockfile"),
    );
    let resolved = gradle_resolved(&fixture);
    assert_gradle_selects(&resolved, ":", &root_rows);
    assert_gradle_selects(&resolved, ":app", &app_rows);
}

/// The messages of the errors on the rows of `report`, which applies nothing and reports each as
/// the broken environment a refusal is.
fn refusals(report: &Envelope) -> Vec<String> {
    assert!(!report.ok(), "{report:#?}");
    assert!(report.applied_names().is_empty(), "{report:#?}");
    let errors: Vec<&serde_json::Value> = report
        .items()
        .iter()
        .filter_map(|item| item.get("error"))
        .collect();
    assert!(!errors.is_empty(), "{report:#?}");
    errors
        .into_iter()
        .map(|error| {
            assert_eq!(error["kind"], "system", "{error}");
            error["message"].as_str().expect("a message").to_owned()
        })
        .collect()
}

/// A repository other than Maven Central can serve any module in Central's place, so `upgrade`
/// refuses the project as a broken environment, without naming the repository's address, and
/// writes nothing.
#[test]
fn a_repository_besides_maven_central_is_refused() {
    skip_if_missing!("gradle");
    let fixture = Fixture::new();
    fixture.write("settings.gradle", SETTINGS);
    let build = build_gradle(SEED_OLD).replace("mavenCentral()", "google(); mavenCentral()");
    fixture.write("build.gradle", &build);
    gradle(&fixture, &["dependencies", "--write-locks"]);
    let before = project_files(fixture.root());

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    for message in refusals(&upgrade) {
        assert!(message.contains("Maven Central"), "{message}");
        assert!(!message.contains("dl.google.com"), "{message}");
    }
    assert_files_unchanged(&fixture, &before);
}

/// Gradle can route every repository through the mirrors a Maven settings file names, which the
/// repositories it reports do not show, so `upgrade` refuses a build that turns that on as a
/// broken environment, naming the property, and writes nothing.
#[test]
fn a_maven_settings_mirror_is_refused() {
    skip_if_missing!("gradle");
    let fixture = fixture(SEED_OLD);
    fixture.write(
        "gradle.properties",
        "org.gradle.mirror.maven.settings=true\n",
    );
    let before = project_files(fixture.root());

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    for message in refusals(&upgrade) {
        assert!(
            message.contains("org.gradle.mirror.maven.settings"),
            "{message}"
        );
    }
    assert_files_unchanged(&fixture, &before);
}

/// Under a lenient lock Gradle resolves past the lock, so the run that verifies a relocked lock
/// would not enforce it, and `upgrade` refuses the project as a broken environment and writes
/// nothing.
#[test]
fn a_lenient_lock_is_refused() {
    skip_if_missing!("gradle");
    let fixture = Fixture::new();
    fixture.write("settings.gradle", SETTINGS);
    let build = build_gradle(SEED_OLD).replace(
        "lockAllConfigurations()",
        "lockAllConfigurations(); lockMode = LockMode.LENIENT",
    );
    fixture.write("build.gradle", &build);
    gradle(&fixture, &["dependencies", "--write-locks"]);
    let before = project_files(fixture.root());

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    for message in refusals(&upgrade) {
        assert!(message.contains("leniently"), "{message}");
    }
    assert_files_unchanged(&fixture, &before);
}

/// A target pins its module only in the configurations that locked it at the version it moves
/// from, and in none that extends one of them: with `commons-text` locked at 1.10.0 on the main
/// classpaths and at 1.13.0 on the test ones and on one extending the runtime classpath,
/// `upgrade` lands the main copy on its target and leaves the others at the release the build
/// declares for them.
#[test]
fn a_target_moves_only_the_configurations_that_locked_its_old_version() {
    skip_if_missing!("gradle");
    let fixture = Fixture::new();
    fixture.write("settings.gradle", SETTINGS);
    // `integrationRuntime` extends `runtimeClasspath`, which the target pins, and requires the
    // newer release itself.
    let build = formatdoc! {"
        plugins {{
            id 'java-library'
        }}

        repositories {{
            mavenCentral()
        }}

        dependencyLocking {{
            lockAllConfigurations()
        }}

        configurations {{
            integrationRuntime {{
                extendsFrom runtimeClasspath
            }}
        }}

        dependencies {{
            implementation '{TEXT}:1.10.0'
            testImplementation '{TEXT}:1.13.0'
            integrationRuntime '{TEXT}:1.13.0'
        }}
    "};
    fixture.write("build.gradle", &build);
    gradle(&fixture, &["dependencies", "--write-locks"]);
    let lock_before = fixture.read_bytes("gradle.lockfile");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--package", TEXT]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    let landed = applied_rows(&upgrade);
    assert!(
        landed
            .iter()
            .any(|(name, from, _)| name == TEXT && from == "1.10.0"),
        "the main copy must land\n{upgrade:#?}"
    );
    let lock_after = fixture.read_bytes("gradle.lockfile");
    assert_lock_moved_exactly(&landed, &lock_before, &lock_after);
    for configuration in ["testRuntimeClasspath", "integrationRuntime"] {
        assert_eq!(
            gradle_lock_pins(&lock_after)
                .get(TEXT)
                .and_then(|locked| locked.get(configuration))
                .map(String::as_str),
            Some("1.13.0"),
            "{configuration}"
        );
    }
}

/// cooldown publishes only the project's lock, as the relock wrote it and the verifying run read
/// it, so a build that writes a file of its own while either run configures it holds the change,
/// naming the file, and leaves the project as it was.
#[test]
fn a_run_that_writes_another_file_publishes_nothing() {
    skip_if_missing!("gradle");
    // One build stamps a file whenever Gradle configures it, and the other appends to its lock
    // only where no lock is written, as in the verifying run.
    let builds = [
        (
            "stamp.txt",
            "file('stamp.txt').text = System.nanoTime().toString()",
        ),
        (
            "gradle.lockfile",
            "if (!gradle.startParameter.writeDependencyLocks && \
             gradle.startParameter.lockedDependenciesToUpdate.empty) { file('gradle.lockfile') << '\\n' }",
        ),
    ];
    for (written, statement) in builds {
        let fixture = Fixture::new();
        fixture.write("settings.gradle", SETTINGS);
        let build = formatdoc! {"
            {build}
            {statement}
        ", build = build_gradle(SEED_OLD)};
        fixture.write("build.gradle", &build);
        gradle(&fixture, &["dependencies", "--write-locks"]);
        let before = project_files(fixture.root());

        let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--package", TEXT]);

        assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
        let detail = upgrade.skip_detail_for(TEXT).expect("a held row");
        assert!(detail.contains(written), "{detail}");
        assert_files_unchanged(&fixture, &before);
    }
}

/// cooldown names a project by its directory, as Gradle does, so a subproject whose directory the
/// settings remap away from its path lands in its own lock all the same, and Gradle accepts it.
/// The settings remap it from a callback of their own, which runs after every callback cooldown
/// registers on the settings.
#[test]
fn a_remapped_project_directory_lands_in_its_lock() {
    skip_if_missing!("gradle");
    let fixture = Fixture::new();
    fixture.write(
        "settings.gradle",
        indoc! {"
            rootProject.name = 'demo'
            include 'app'
            gradle.settingsEvaluated { settings ->
                settings.project(':app').projectDir = new File(settings.settingsDir, 'modules/app')
            }
        "},
    );
    fixture.write(
        "modules/app/build.gradle",
        &build_gradle(&[(TEXT, "1.10.0")]),
    );
    gradle(&fixture, &[":app:dependencies", "--write-locks"]);
    let lock_before = fixture.read_bytes("modules/app/gradle.lockfile");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    let landed = applied_rows_in(&upgrade, "modules/app");
    assert!(names(&landed).contains(&TEXT), "{upgrade:#?}");
    assert_lock_moved_exactly(
        &landed,
        &lock_before,
        &fixture.read_bytes("modules/app/gradle.lockfile"),
    );
    assert_gradle_selects(&gradle_resolved(&fixture), ":app", &landed);
}

/// The settings may move the root project out of the settings directory, and cooldown finds it by
/// the directory they move it to, below the settings directory, as for any other project.
#[test]
fn a_moved_root_project_lands_in_its_lock() {
    skip_if_missing!("gradle");
    let fixture = Fixture::new();
    fixture.write(
        "settings.gradle",
        indoc! {"
            rootProject.name = 'demo'
            rootProject.projectDir = file('module')
        "},
    );
    fixture.write("module/build.gradle", &build_gradle(&[(TEXT, "1.10.0")]));
    gradle(&fixture, &[":dependencies", "--write-locks"]);
    let lock_before = fixture.read_bytes("module/gradle.lockfile");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    let landed = applied_rows_in(&upgrade, "module");
    assert!(names(&landed).contains(&TEXT), "{upgrade:#?}");
    assert_lock_moved_exactly(
        &landed,
        &lock_before,
        &fixture.read_bytes("module/gradle.lockfile"),
    );
    assert_gradle_selects(&gradle_resolved(&fixture), ":", &landed);
}

/// A build that configures on demand picks the tasks to run before it configures its projects,
/// and `--build` still assembles the project.
#[test]
fn build_runs_where_gradle_configures_on_demand() {
    skip_if_missing!("gradle");
    let fixture = fixture(&[(TEXT, "1.12.0")]);
    fixture.write("gradle.properties", "org.gradle.configureondemand=true\n");

    let built = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--build"]);

    assert_eq!(built.build_ok(), Some(true), "{built:#?}");
    assert!(built.ok(), "{built:#?}");
}

/// `--build` assembles the project alone: a subproject below it that does not compile is no part
/// of its build.
#[test]
fn build_leaves_out_a_broken_subproject_below_the_project() {
    skip_if_missing!("gradle");
    let fixture = Fixture::new();
    fixture.write(
        "settings.gradle",
        indoc! {"
            rootProject.name = 'demo'
            include 'app', 'app:examples'
        "},
    );
    fixture.write("app/build.gradle", &build_gradle(&[(TEXT, "1.10.0")]));
    fixture.write(
        "app/src/main/java/demo/Demo.java",
        indoc! {"
            package demo;

            public class Demo {}
        "},
    );
    fixture.write("app/examples/build.gradle", "plugins { id 'java' }\n");
    fixture.write(
        "app/examples/src/main/java/demo/Broken.java",
        indoc! {"
            package demo;

            public class Broken { int }
        "},
    );
    gradle(&fixture, &[":app:dependencies", "--write-locks"]);

    let built = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--build"]);

    assert_eq!(built.build_ok(), Some(true), "{built:#?}");
    assert!(built.ok(), "{built:#?}");
}

/// `--build` compiles the project, so it reports a compile error that the `dependencies` report
/// passes over, while a project that compiles builds.
#[test]
fn build_reports_a_compile_error_the_dependency_report_misses() {
    skip_if_missing!("gradle");
    let project = |source: &str| {
        let fixture = fixture(&[(TEXT, "1.10.0")]);
        fixture.write("src/main/java/demo/Demo.java", source);
        fixture
    };
    let compiles = project(indoc! {"
        package demo;

        public class Demo {}
    "});
    let broken = project(indoc! {"
        package demo;

        public class Demo { int }
    "});
    gradle(&broken, &["dependencies"]);

    let built = compiles.cooldown_json(&["upgrade", "--freeze", FREEZE, "--build"]);
    let failed = broken.cooldown_json(&["upgrade", "--freeze", FREEZE, "--build"]);

    assert_eq!(built.build_ok(), Some(true), "{built:#?}");
    assert!(built.ok(), "{built:#?}");
    assert_eq!(failed.build_ok(), Some(false), "{failed:#?}");
    assert!(!failed.ok(), "{failed:#?}");
}
