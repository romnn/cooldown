//! End-to-end convergence tests that drive the REAL `mvn` against `pom.xml` fixtures generated on
//! the fly in temp dirs.
//!
//! They hold the Maven adapter to the contract the other suites hold their adapters to: an
//! `upgrade` or `fix` lands every planned change exactly on cooldown's target, moves nothing it
//! did not report, converges, and its `--dry-run` reports the same rows without touching a file.
//! Maven has no lock: cooldown reads the versions the POMs declare and rewrites exactly the element
//! that spells each one, so the POMs play the lock's part here, and Maven's own resolution of the
//! project (`dependency:list`, or `dependency:tree` for a reactor whose modules are not built) is
//! the check that Maven accepts what cooldown wrote.
//! Every fixture gives Maven a private user home (see the harness's `tool_env`), so the plugins
//! and dependencies it downloads and any `settings.xml` it reads are never the user's.
//!
//! # Determinism
//!
//! Every test pins the cooldown window with `--freeze`, and the Maven Central history before that
//! instant is immutable, so cooldown's targets reproduce forever.
//! A pom that declares exact versions resolves the same graph forever, so the fixtures need no seed
//! lock: writing the pom is the seed.
//! The assertions check invariants (landing on the planned target, no silent change, convergence,
//! cross-command agreement, cooldown reading what Maven resolves), never hard-coded versions,
//! except where a test is about one release.
//!
//! # The fixtures
//!
//! `commons-text` depends on `commons-lang3`, neither has other runtime dependencies, and both have
//! published within their major line on both sides of `FREEZE`.
//! Declared at `commons-text` 1.10.0 and `commons-lang3` 3.12.0, both have a matured within-major
//! upgrade (1.12.0 and 3.14.0) and newer releases still in cooldown (1.13.0 and 3.15.0).
//! Declared at `commons-text` 1.13.0 and `commons-lang3` 3.17.0, both are younger than `FREEZE`, so
//! `fix` has to mature them back down.
//! A pom's exact version is a soft requirement Maven never caps, so a hold or a blocked verdict is
//! never legitimate here, unless a test is about a version cooldown declines to move.
//! `commons-lang3` also publishes a `test-jar`, a second artifact of the same `group:artifact`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use std::collections::{BTreeMap, BTreeSet};

use indoc::{formatdoc, indoc};
use support::{Envelope, Fixture, project_files};

/// The absolute cooldown cutoff every test evaluates under.
const FREEZE: &str = "2024-06-01T00:00:00Z";

const TEXT: &str = "org.apache.commons:commons-text";
const LANG: &str = "org.apache.commons:commons-lang3";

/// The goal the tests resolve a project with, at a fixed plugin release so the check itself never
/// drifts.
const DEPENDENCY_LIST: &str = "org.apache.maven.plugins:maven-dependency-plugin:3.8.1:list";

/// The goal the tests collect a reactor's modules with, which needs no module built.
const DEPENDENCY_TREE: &str = "org.apache.maven.plugins:maven-dependency-plugin:3.8.1:tree";

/// The versions a fixture's pom declares, as `(coordinate, version)` pairs.
type Seed = &'static [(&'static str, &'static str)];

/// A pom old enough that both dependencies have a matured upgrade at `FREEZE`.
const SEED_OLD: Seed = &[(TEXT, "1.10.0"), (LANG, "3.12.0")];

/// A pom whose dependencies were both published after `FREEZE`, so both violate the cooldown.
const SEED_TOO_FRESH: Seed = &[(TEXT, "1.13.0"), (LANG, "3.17.0")];

/// One `<dependency>` element for `coordinate` (`group:artifact`) at `version`, with `extra`
/// elements such as a `<type>`.
fn dependency_with(coordinate: &str, version: &str, extra: &str) -> String {
    let (group, artifact) = coordinate
        .split_once(':')
        .expect("a group:artifact coordinate");
    formatdoc! {"
        <dependency>
          <groupId>{group}</groupId>
          <artifactId>{artifact}</artifactId>
          <version>{version}</version>{extra}
        </dependency>
    "}
}

/// One `<dependency>` element for `coordinate` (`group:artifact`) at `version`.
fn dependency(coordinate: &str, version: &str) -> String {
    dependency_with(coordinate, version, "")
}

/// A `<dependencies>` section declaring `seed`.
fn dependencies(seed: Seed) -> String {
    let declared: String = seed
        .iter()
        .map(|(coordinate, version)| dependency(coordinate, version))
        .collect();
    format!("<dependencies>\n{declared}</dependencies>\n")
}

/// A standalone pom for `demo:demo` with `sections` (whole top-level elements) inside `<project>`.
fn pom(sections: &str) -> String {
    formatdoc! {r#"
        <?xml version="1.0" encoding="UTF-8"?>
        <project xmlns="http://maven.apache.org/POM/4.0.0">
          <modelVersion>4.0.0</modelVersion>
          <groupId>demo</groupId>
          <artifactId>demo</artifactId>
          <version>0.1.0</version>
        {sections}</project>
    "#}
}

/// A project whose pom declares `seed`.
fn fixture(seed: Seed) -> Fixture {
    fixture_with_pom(&pom(&dependencies(seed)))
}

/// An empty fixture whose Maven user home `mvn` can take from `MAVEN_OPTS`.
fn maven_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.maven_user_home();
    fixture
}

/// A project whose `pom.xml` is `pom`.
fn fixture_with_pom(pom: &str) -> Fixture {
    let fixture = maven_fixture();
    fixture.write("pom.xml", pom);
    fixture
}

/// The key Maven gives an artifact, `group:artifact:type[:classifier]`, with the classifier
/// Maven's artifact handler gives a `test-jar` that names none.
fn artifact_key(coordinate: &str, kind: &str, classifier: &str) -> String {
    let classifier = match (kind, classifier) {
        ("test-jar", "") => "tests",
        (_, classifier) => classifier,
    };
    if classifier.is_empty() {
        format!("{coordinate}:{kind}")
    } else {
        format!("{coordinate}:{kind}:{classifier}")
    }
}

/// The `artifact key -> version` map of every dependency Maven resolves for the module in `dir`
/// (the root when `None`), transitive ones included.
///
/// The goal fails, and so does this, when Maven cannot build the module's model or resolve one of
/// its dependencies.
fn maven_resolved_in(fixture: &Fixture, dir: Option<&str>) -> BTreeMap<String, String> {
    let mut args = vec!["-B", "-ntp"];
    if let Some(dir) = dir {
        args.extend(["-f", dir]);
    }
    args.push(DEPENDENCY_LIST);
    let listed = fixture.run_tool("mvn", &args, &[]).expect_success();
    let mut resolved = BTreeMap::new();
    for line in listed.stdout_str().lines() {
        let Some(entry) = line.strip_prefix("[INFO]") else {
            continue;
        };
        let Some(token) = entry.split_whitespace().next() else {
            continue;
        };
        // `group:artifact:type[:classifier]:version:scope`
        let parts: Vec<&str> = token.split(':').collect();
        let (key, version) = match parts.as_slice() {
            [group, artifact, kind, version, _scope] => (
                artifact_key(&format!("{group}:{artifact}"), kind, ""),
                version,
            ),
            [group, artifact, kind, classifier, version, _scope] => (
                artifact_key(&format!("{group}:{artifact}"), kind, classifier),
                version,
            ),
            _ => continue,
        };
        resolved.insert(key, (*version).to_owned());
    }
    resolved
}

/// [`maven_resolved_in`] for the project at the fixture root.
fn maven_resolved(fixture: &Fixture) -> BTreeMap<String, String> {
    maven_resolved_in(fixture, None)
}

/// The version Maven resolves for the `jar` of `name`, from a [`maven_resolved_in`] map.
fn jar(resolved: &BTreeMap<String, String>, name: &str) -> Option<String> {
    resolved.get(&artifact_key(name, "jar", "")).cloned()
}

/// The `artifact key -> version` map of the direct dependencies Maven collects for each module of
/// the reactor at the fixture root, by the module's artifact id.
///
/// Collection reads POMs only, so it needs no module built, and the trees land outside the
/// project.
fn maven_collected(fixture: &Fixture) -> BTreeMap<String, BTreeMap<String, String>> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Node {
        group_id: String,
        artifact_id: String,
        version: String,
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        classifier: String,
        #[serde(default)]
        children: Vec<Node>,
    }
    let out = tempfile::tempdir().expect("tree dir");
    let file = out.path().join("trees.json");
    let output = format!("-DoutputFile={}", file.display());
    fixture
        .run_tool(
            "mvn",
            &[
                "-B",
                "-ntp",
                DEPENDENCY_TREE,
                "-DoutputType=json",
                "-DappendOutput=true",
                &output,
            ],
            &[],
        )
        .expect_success();
    let text = std::fs::read_to_string(&file).expect("read the trees");
    serde_json::Deserializer::from_str(&text)
        .into_iter::<Node>()
        .map(|module| {
            let module = module.expect("a module tree");
            let dependencies = module
                .children
                .into_iter()
                .map(|node| {
                    let name = format!("{}:{}", node.group_id, node.artifact_id);
                    (
                        artifact_key(&name, &node.kind, &node.classifier),
                        node.version,
                    )
                })
                .collect();
            (module.artifact_id, dependencies)
        })
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
    rows_where(report, is_applied)
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

/// The member paths (`.` for the reactor root) a report row attributes itself to.
fn member_paths(item: &serde_json::Value) -> BTreeSet<String> {
    item.get("members")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|member| member.get("path")?.as_str().map(str::to_owned))
        .collect()
}

/// The names of the rows of `report` applied to the reactor member at `member` (`.` for the
/// root), whatever project the row belongs to.
fn applied_names_for_member(report: &Envelope, member: &str) -> BTreeSet<String> {
    rows_where(report, |item| {
        is_applied(item) && member_paths(item).contains(member)
    })
    .into_iter()
    .map(|(name, _, _)| name)
    .collect()
}

/// The applied rows of `report` in `project` that name `member` (`.` for the project root).
fn applied_rows_in(report: &Envelope, project: &str, member: &str) -> BTreeSet<Row> {
    rows_where(report, |item| {
        is_applied(item)
            && item.get("project").and_then(serde_json::Value::as_str) == Some(project)
            && member_paths(item).contains(member)
    })
}

/// What a report did with one row: `(project, name, from, to, member paths, outcome)`.
type Disposition = (String, String, String, String, BTreeSet<String>, String);

/// The disposition of every row of `report`, sorted, its outcome being `applied`, the reason a
/// held row was skipped for, or `error`.
///
/// A dry run and a real run agree when these agree: the same moves, in the same projects,
/// attributed to the same members, landing or held alike, and as often.
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
                member_paths(item),
                outcome,
            ))
        })
        .collect();
    dispositions.sort();
    dispositions
}

/// Asserts that `after` is `before` with only the version text of `rows` rewritten, each from its
/// old version to its target, so the rewrite kept every other byte of the pom, its formatting
/// included.
fn assert_only_versions_rewritten(rows: &BTreeSet<Row>, before: &[u8], after: &[u8]) {
    let mut expected = String::from_utf8_lossy(before).into_owned();
    for (name, from, to) in rows {
        let old = format!("<version>{from}</version>");
        assert_eq!(
            expected.matches(&old).count(),
            1,
            "the fixture declares {name}'s {from} once"
        );
        expected = expected.replacen(&old, &format!("<version>{to}</version>"), 1);
    }
    assert_eq!(String::from_utf8_lossy(after), expected);
}

/// Asserts that Maven resolves the `jar` of each of `rows` at its target in the module at `dir`
/// (the root when `None`).
fn assert_maven_selects(fixture: &Fixture, dir: Option<&str>, rows: &BTreeSet<Row>) {
    let resolved = maven_resolved_in(fixture, dir);
    for (name, from, to) in rows {
        assert_eq!(
            jar(&resolved, name).as_ref(),
            Some(to),
            "Maven must resolve {name} at its target, not {from}\nresolved={resolved:?}"
        );
    }
}

/// `check` cannot evaluate a Maven project yet: the adapter has no lock-currency verdict for a pom
/// (#197), so it fails closed with `lock_unknown` alone, checks nothing, and leaves every file as
/// it was.
///
/// This pins today's behavior; it flips once Maven's own verdict backs `check`.
#[test]
fn check_fails_closed_on_an_unverifiable_pom() {
    skip_if_missing!("mvn");
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

/// `outdated` and `upgrade` describe the same plan: what `upgrade` holds `outdated` blocks, and
/// each dependency is adoptable at exactly the target `upgrade` plans for it, with newer releases
/// still in cooldown past it.
/// Both read the versions Maven resolves, and neither writes a file.
#[test]
fn outdated_agrees_with_upgrade_on_the_matured_target() {
    skip_if_missing!("mvn");
    let fixture = fixture(SEED_OLD);
    let before = project_files(fixture.root());

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);

    assert_eq!(project_files(fixture.root()), before);

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

    let resolved = maven_resolved(&fixture);
    for name in [TEXT, LANG] {
        assert_eq!(
            outdated.item_field_str(name, "current"),
            jar(&resolved, name),
            "outdated must read the version Maven resolves for {name}"
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
/// It rewrites only the version elements of `pom.xml`, writes no other file (no `target/`, none of
/// Maven's reports), keeps Maven inside the fixture's private home, and Maven resolves the
/// targets.
/// A second `upgrade` then finds nothing to do and rewrites nothing.
#[test]
fn upgrade_lands_each_planned_change_on_its_target_and_converges() {
    skip_if_missing!("mvn");
    let fixture = fixture(SEED_OLD);
    let pom_before = fixture.read_bytes("pom.xml");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.lock_status(),
        Some("unknown"),
        "cooldown cannot prove a pom current yet (#197)"
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
    let pom_after = fixture.read_bytes("pom.xml");
    // The pom moves exactly the applied rows, so nothing moved in silence.
    assert_only_versions_rewritten(&applied_rows(&upgrade), &pom_before, &pom_after);
    let files: Vec<String> = project_files(fixture.root()).into_keys().collect();
    assert_eq!(
        files,
        ["pom.xml"],
        "the upgrade must write nothing beside the pom"
    );
    // The pinned plugins cooldown verifies with downloaded into the fixture's private home, so
    // Maven ran there and nowhere else.
    let repository = fixture.maven_user_home().join(".m2/repository");
    for plugin in ["maven-dependency-plugin/3.8.1", "maven-help-plugin/3.5.1"] {
        assert!(
            repository
                .join("org/apache/maven/plugins")
                .join(plugin)
                .is_dir(),
            "{plugin} must download into the fixture's private home"
        );
    }
    assert_maven_selects(&fixture, None, &applied_rows(&upgrade));

    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "the second upgrade must be a no-op\n{second:#?}"
    );
    assert!(
        rows(&second).is_empty(),
        "a converged pom leaves nothing to plan\n{second:#?}"
    );
    assert_eq!(
        fixture.read_bytes("pom.xml"),
        pom_after,
        "the pom must be byte-identical across the two converged runs"
    );
}

/// `upgrade --dry-run` disposes of every row as the real run on an identical fixture does, the
/// same moves attributed to the same members and landing or held alike, and leaves every project
/// file byte-identical, while the real run lands them all.
#[test]
fn upgrade_dry_run_agrees_with_real_upgrade() {
    skip_if_missing!("mvn");

    let dry_fixture = fixture(SEED_OLD);
    let before = project_files(dry_fixture.root());
    let dry = dry_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    assert_eq!(project_files(dry_fixture.root()), before);
    assert_eq!(dry.lock_status(), None, "--dry-run never re-locks");

    let real_fixture = fixture(SEED_OLD);
    let pom_before = real_fixture.read_bytes("pom.xml");
    let real = real_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    // The real run succeeds, so the two cannot merely agree on the same failure.
    assert!(real.ok(), "{real:#?}");
    assert!(
        !real.applied_names().is_empty(),
        "the real run must land the matured upgrades\n{real:#?}"
    );
    assert_only_versions_rewritten(
        &applied_rows(&real),
        &pom_before,
        &real_fixture.read_bytes("pom.xml"),
    );

    assert_eq!(
        dispositions(&dry),
        dispositions(&real),
        "the dry run must dispose of every row as the real run does\ndry={dry:#?}\nreal={real:#?}"
    );
}

/// `fix` matures every too-fresh dependency down to its target, reports each move as a downgrade,
/// writes nothing beside the pom, and a second `fix` finds nothing left to do.
#[test]
fn fix_matures_too_fresh_versions_to_their_targets_and_is_idempotent() {
    skip_if_missing!("mvn");
    let fixture = fixture(SEED_TOO_FRESH);
    let pom_before = fixture.read_bytes("pom.xml");

    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);

    assert!(fixed.ok(), "{fixed:#?}");
    assert_eq!(fixed.summary_errors(), 0, "{fixed:#?}");
    let applied = fixed.applied_names();
    assert_eq!(
        applied,
        BTreeSet::from([TEXT.to_owned(), LANG.to_owned()]),
        "fix must mature both too-fresh dependencies\n{fixed:#?}"
    );
    assert_eq!(
        fixed.downgraded_names(),
        applied,
        "every fix move is a downgrade"
    );
    let pom_after = fixture.read_bytes("pom.xml");
    assert_only_versions_rewritten(&applied_rows(&fixed), &pom_before, &pom_after);
    let files: Vec<String> = project_files(fixture.root()).into_keys().collect();
    assert_eq!(
        files,
        ["pom.xml"],
        "the fix must write nothing beside the pom"
    );

    let again = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert_eq!(
        again.summary_applied(),
        0,
        "the second fix must be a no-op\n{again:#?}"
    );
    assert_eq!(
        fixture.read_bytes("pom.xml"),
        pom_after,
        "the second fix must leave the pom byte-identical"
    );
    assert_maven_selects(&fixture, None, &applied_rows(&fixed));
}

/// A version the project's `dependencyManagement` declares for a dependency that omits its own is
/// read, and lands where it is declared.
#[test]
fn a_managed_version_lands_in_dependency_management() {
    skip_if_missing!("mvn");
    let (group, artifact) = TEXT.split_once(':').expect("coordinate");
    let fixture = fixture_with_pom(&pom(&formatdoc! {"
        <dependencyManagement>
        {managed}</dependencyManagement>
        <dependencies>
          <dependency>
            <groupId>{group}</groupId>
            <artifactId>{artifact}</artifactId>
          </dependency>
        </dependencies>
    ", managed = dependencies(&[(TEXT, "1.10.0")])}));
    let pom_before = fixture.read_bytes("pom.xml");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.applied_names(),
        BTreeSet::from([TEXT.to_owned()]),
        "{upgrade:#?}"
    );
    let pom_after = fixture.read_bytes("pom.xml");
    assert_only_versions_rewritten(&applied_rows(&upgrade), &pom_before, &pom_after);
    assert_maven_selects(&fixture, None, &applied_rows(&upgrade));
}

/// The pom of a project whose `commons-text` version is the property `commons-text.version` at
/// 1.10.0, with `more` inside `<properties>`.
fn property_pom(more: &str) -> String {
    let (group, artifact) = TEXT.split_once(':').expect("coordinate");
    pom(&formatdoc! {"
        <properties>
          <commons-text.version>1.10.0</commons-text.version>{more}
        </properties>
        <dependencies>
          <dependency>
            <groupId>{group}</groupId>
            <artifactId>{artifact}</artifactId>
            <version>${{commons-text.version}}</version>
          </dependency>
        </dependencies>
    "})
}

/// A version Maven resolves from a property (`${commons-text.version}`) is read like a literal one,
/// at the version Maven resolves, and when only that dependency uses the property, the property's
/// value is the text `upgrade` rewrites: it moves to the target, every other byte stays, and Maven
/// resolves the target.
///
/// Most real poms declare versions this way.
#[test]
fn an_exclusive_property_is_read_and_edited_where_it_is_defined() {
    skip_if_missing!("mvn");
    let fixture = fixture_with_pom(&property_pom(""));
    let pom_before = fixture.read_bytes("pom.xml");
    let resolved = maven_resolved(&fixture);

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert_eq!(
        outdated.item_field_str(TEXT, "current"),
        jar(&resolved, TEXT),
        "outdated must report commons-text at the version Maven resolves\n{outdated:#?}"
    );
    assert!(upgrade.ok(), "{upgrade:#?}");
    let landed = upgrade.applied_change_for(TEXT).expect("applied");
    let expected = String::from_utf8_lossy(&pom_before).replace(
        &format!("<commons-text.version>{}<", landed.from),
        &format!("<commons-text.version>{}<", landed.to),
    );
    assert_eq!(
        String::from_utf8_lossy(&fixture.read_bytes("pom.xml")),
        expected
    );
    assert_maven_selects(&fixture, None, &applied_rows(&upgrade));
}

/// A property something else reads too is no element cooldown may move for one dependency:
/// `upgrade` holds the dependency as not eligible, naming the property, and leaves the pom alone.
#[test]
fn a_shared_property_is_held_and_left_alone() {
    skip_if_missing!("mvn");
    let fixture = fixture_with_pom(&property_pom(
        "\n  <text.label>commons-text ${commons-text.version}</text.label>",
    ));
    let before = project_files(fixture.root());

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(
        upgrade.ok(),
        "a held dependency is no failure\n{upgrade:#?}"
    );
    assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
    assert_eq!(
        upgrade.skipped_reasons_for(TEXT),
        BTreeSet::from(["not_eligible".to_owned()]),
        "{upgrade:#?}"
    );
    let detail = upgrade.skip_detail_for(TEXT).expect("a detail");
    assert!(detail.contains("commons-text.version"), "{detail}");
    assert_eq!(project_files(fixture.root()), before);
}

/// A `-D` property in `.mvn/maven.config` outranks the pom's, so cooldown reads the version Maven
/// does and refuses to edit a property Maven would ignore.
#[test]
fn a_command_line_property_is_read_and_never_edited() {
    skip_if_missing!("mvn");
    let fixture = fixture_with_pom(&property_pom(""));
    fixture.write(".mvn/maven.config", "-Dcommons-text.version=1.11.0\n");
    let before = project_files(fixture.root());

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert_eq!(
        outdated.item_field_str(TEXT, "current"),
        jar(&maven_resolved(&fixture), TEXT),
        "outdated must read the version the command line sets\n{outdated:#?}"
    );
    assert_eq!(
        upgrade.skipped_reasons_for(TEXT),
        BTreeSet::from(["not_eligible".to_owned()]),
        "{upgrade:#?}"
    );
    let detail = upgrade.skip_detail_for(TEXT).expect("a detail");
    assert!(detail.contains("-Dcommons-text.version"), "{detail}");
    assert_eq!(project_files(fixture.root()), before);
}

/// A version range floats: Maven picks the newest release it finds in it, so cooldown neither
/// reports the range as a version nor plans it, and warns that it floats.
#[test]
fn a_version_range_floats_and_is_not_planned() {
    skip_if_missing!("mvn");
    let fixture = fixture(&[(TEXT, "[1.10.0,2.0)")]);
    let before = project_files(fixture.root());

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(outdated.item_names().is_empty(), "{outdated:#?}");
    assert!(
        outdated
            .warning_messages()
            .iter()
            .any(|message| message.contains(TEXT) && message.contains("[1.10.0,2.0)")),
        "outdated must say the range floats\n{outdated:#?}"
    );
    assert!(rows(&upgrade).is_empty(), "{upgrade:#?}");
    assert_eq!(project_files(fixture.root()), before);
}

/// A hard requirement `[v]` pins `v`: cooldown reads it at `v` and lands the target inside the
/// brackets, which Maven then resolves.
#[test]
fn a_hard_requirement_lands_inside_its_brackets() {
    skip_if_missing!("mvn");
    let fixture = fixture(&[(TEXT, "[1.10.0]")]);
    let pom_before = fixture.read_bytes("pom.xml");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    let landed = upgrade.applied_change_for(TEXT).expect("applied");
    assert_eq!(landed.from, "1.10.0");
    let expected =
        String::from_utf8_lossy(&pom_before).replace("[1.10.0]", &format!("[{}]", landed.to));
    assert_eq!(
        String::from_utf8_lossy(&fixture.read_bytes("pom.xml")),
        expected
    );
    assert_eq!(jar(&maven_resolved(&fixture), TEXT), Some(landed.to));
}

/// A local management entry lands where it is declared, while an imported BOM and a remote parent
/// are reported at their versions but never edited, and a version only the BOM supplies is not
/// planned at all.
#[test]
fn a_remote_bom_and_parent_stay_visible_and_unedited() {
    skip_if_missing!("mvn");
    let fixture = fixture_with_pom(indoc! {r#"
        <?xml version="1.0" encoding="UTF-8"?>
        <project xmlns="http://maven.apache.org/POM/4.0.0">
          <modelVersion>4.0.0</modelVersion>
          <parent>
            <groupId>org.sonatype.oss</groupId>
            <artifactId>oss-parent</artifactId>
            <version>7</version>
            <relativePath/>
          </parent>
          <groupId>demo</groupId>
          <artifactId>demo</artifactId>
          <version>0.1.0</version>
          <dependencyManagement>
            <dependencies>
              <dependency>
                <groupId>org.junit</groupId>
                <artifactId>junit-bom</artifactId>
                <version>5.10.0</version>
                <type>pom</type>
                <scope>import</scope>
              </dependency>
              <dependency>
                <groupId>org.apache.commons</groupId>
                <artifactId>commons-text</artifactId>
                <version>1.10.0</version>
              </dependency>
            </dependencies>
          </dependencyManagement>
          <dependencies>
            <dependency>
              <groupId>org.apache.commons</groupId>
              <artifactId>commons-text</artifactId>
            </dependency>
            <dependency>
              <groupId>org.junit.jupiter</groupId>
              <artifactId>junit-jupiter-api</artifactId>
              <scope>test</scope>
            </dependency>
          </dependencies>
        </project>
    "#});
    let pom_before = fixture.read_bytes("pom.xml");

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    // The BOM and the parent are reported, the BOM-managed API is not, and says why.
    let reported = outdated.item_names();
    for name in ["org.junit:junit-bom", "org.sonatype.oss:oss-parent", TEXT] {
        assert!(reported.contains(name), "{name}\n{outdated:#?}");
    }
    assert!(!reported.contains("org.junit.jupiter:junit-jupiter-api"));
    assert!(
        outdated
            .warning_messages()
            .iter()
            .any(|message| message.contains("org.junit.jupiter:junit-jupiter-api")),
        "{outdated:#?}"
    );

    // Only the local management entry moves, and the BOM is held as one cooldown never edits.
    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.skipped_reasons_for("org.junit:junit-bom"),
        BTreeSet::from(["not_eligible".to_owned()]),
        "{upgrade:#?}"
    );
    assert_eq!(
        upgrade.applied_names(),
        BTreeSet::from([TEXT.to_owned()]),
        "{upgrade:#?}"
    );
    let pom_after = fixture.read_bytes("pom.xml");
    assert_only_versions_rewritten(&applied_rows(&upgrade), &pom_before, &pom_after);
    assert_maven_selects(&fixture, None, &applied_rows(&upgrade));
}

/// The pom of a reactor root `demo:parent` aggregating `modules`, with `sections` inside
/// `<project>`.
fn reactor_root_pom(modules: &[&str], sections: &str) -> String {
    let modules = modules
        .iter()
        .map(|module| format!("    <module>{module}</module>\n"))
        .collect::<Vec<_>>()
        .concat();
    formatdoc! {r#"
        <?xml version="1.0" encoding="UTF-8"?>
        <project xmlns="http://maven.apache.org/POM/4.0.0">
          <modelVersion>4.0.0</modelVersion>
          <groupId>demo</groupId>
          <artifactId>parent</artifactId>
          <version>0.1.0</version>
          <packaging>pom</packaging>
          <modules>
        {modules}  </modules>
        {sections}</project>
    "#}
}

/// The pom of the reactor module `artifact`, inheriting from the root `demo:parent` and declaring
/// `declared` (whole `<dependency>` elements).
fn reactor_module_pom(artifact: &str, declared: &str) -> String {
    formatdoc! {r#"
        <?xml version="1.0" encoding="UTF-8"?>
        <project xmlns="http://maven.apache.org/POM/4.0.0">
          <modelVersion>4.0.0</modelVersion>
          <parent>
            <groupId>demo</groupId>
            <artifactId>parent</artifactId>
            <version>0.1.0</version>
          </parent>
          <artifactId>{artifact}</artifactId>
          <dependencies>
        {declared}  </dependencies>
        </project>
    "#}
}

/// A module that inherits from its reactor root disposes of its rows under `--dry-run` as for
/// real: the dry run must see the parent pom Maven reads beside the module.
/// The reactor is one project, and each row names the module it belongs to.
#[test]
fn a_reactor_module_dry_run_agrees_with_real_upgrade() {
    skip_if_missing!("mvn");
    let reactor = || {
        let fixture = maven_fixture();
        fixture.write("pom.xml", &reactor_root_pom(&["app"], ""));
        fixture.write(
            "app/pom.xml",
            &reactor_module_pom("app", &dependency(TEXT, "1.10.0")),
        );
        fixture
    };

    let dry_fixture = reactor();
    let before = project_files(dry_fixture.root());
    let dry = dry_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    assert_eq!(project_files(dry_fixture.root()), before);

    let real_fixture = reactor();
    let module_before = real_fixture.read_bytes("app/pom.xml");
    let real = real_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(real.ok(), "{real:#?}");
    assert_eq!(
        applied_names_for_member(&real, "app"),
        BTreeSet::from([TEXT.to_owned()]),
        "{real:#?}"
    );
    // The module is no project of its own, so neither run plans its dependency twice.
    assert_eq!(
        dry.item_projects_for(TEXT),
        ["."],
        "one project per reactor"
    );
    assert_eq!(
        real.item_projects_for(TEXT),
        ["."],
        "one project per reactor"
    );
    let landed = applied_rows_in(&real, ".", "app");
    assert_only_versions_rewritten(
        &landed,
        &module_before,
        &real_fixture.read_bytes("app/pom.xml"),
    );
    assert_maven_selects(&real_fixture, Some("app"), &landed);

    assert_eq!(
        dispositions(&dry),
        dispositions(&real),
        "the dry run must dispose of every row as the real run does\ndry={dry:#?}\nreal={real:#?}"
    );
}

/// A reactor whose module depends on a sibling module that was never built resolves in the dry
/// run and for real: the sibling is no row, and the module's own dependency lands.
#[test]
fn a_clean_reactor_with_a_sibling_dependency_resolves() {
    skip_if_missing!("mvn");
    let reactor = || {
        let fixture = maven_fixture();
        fixture.write("pom.xml", &reactor_root_pom(&["lib", "app"], ""));
        fixture.write("lib/pom.xml", &reactor_module_pom("lib", ""));
        let declared = format!(
            "{}{}",
            dependency("demo:lib", "${project.version}"),
            dependency(TEXT, "1.10.0")
        );
        fixture.write("app/pom.xml", &reactor_module_pom("app", &declared));
        fixture
    };

    let dry_fixture = reactor();
    let dry = dry_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let real_fixture = reactor();
    let real = real_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(dry.ok(), "{dry:#?}");
    assert!(real.ok(), "{real:#?}");
    assert_eq!(dispositions(&dry), dispositions(&real));
    assert_eq!(
        applied_names_for_member(&real, "app"),
        BTreeSet::from([TEXT.to_owned()]),
        "{real:#?}"
    );
    assert_eq!(real.item_names(), BTreeSet::from([TEXT.to_owned()]));
    let collected = maven_collected(&real_fixture);
    assert_eq!(
        collected["app"]
            .get(&artifact_key(TEXT, "jar", ""))
            .cloned(),
        real.applied_change_for(TEXT).map(|change| change.to),
        "{collected:?}"
    );
}

/// The root and a module that pin the same coordinate at different versions are two rows, each
/// attributed to its own pom: upgrading the root's never rewrites the module's newer one, `fix`
/// maturing the module's never rewrites the root's, and each pom moves exactly its own rows.
#[test]
fn root_and_module_pins_of_one_coordinate_move_apart() {
    skip_if_missing!("mvn");
    let fixture = maven_fixture();
    fixture.write(
        "pom.xml",
        &reactor_root_pom(&["app"], &dependencies(&[(LANG, "3.12.0")])),
    );
    let declared = format!(
        "{}{}",
        dependency(LANG, "3.17.0"),
        dependency(TEXT, "1.10.0")
    );
    fixture.write("app/pom.xml", &reactor_module_pom("app", &declared));
    let root_before = fixture.read_bytes("pom.xml");
    let module_before = fixture.read_bytes("app/pom.xml");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    let root_rows = applied_rows_in(&upgrade, ".", ".");
    let module_rows = applied_rows_in(&upgrade, ".", "app");
    assert_eq!(
        root_rows
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect::<Vec<_>>(),
        [LANG],
        "the root's matured commons-lang3 lands for the root alone\n{upgrade:#?}"
    );
    assert_eq!(
        module_rows
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect::<Vec<_>>(),
        [TEXT],
        "the module's commons-text lands, and its newer commons-lang3 stays\n{upgrade:#?}"
    );
    // Each pom moves exactly its own member's rows.
    let root_after = fixture.read_bytes("pom.xml");
    assert_only_versions_rewritten(&root_rows, &root_before, &root_after);
    let module_after = fixture.read_bytes("app/pom.xml");
    assert_only_versions_rewritten(&module_rows, &module_before, &module_after);

    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);

    assert!(fixed.ok(), "{fixed:#?}");
    let matured = applied_rows_in(&fixed, ".", "app");
    assert_eq!(
        matured
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect::<Vec<_>>(),
        [LANG],
        "fix matures the module's too-fresh commons-lang3\n{fixed:#?}"
    );
    assert_eq!(
        fixture.read_bytes("pom.xml"),
        root_after,
        "fix never rewrites the root's matured pin"
    );
    assert_only_versions_rewritten(&matured, &module_after, &fixture.read_bytes("app/pom.xml"));
    let collected = maven_collected(&fixture);
    assert_eq!(
        collected["app"]
            .get(&artifact_key(LANG, "jar", ""))
            .cloned(),
        fixed.applied_change_for(LANG).map(|change| change.to),
        "{collected:?}"
    );
}

/// A `jar` and a `test-jar` of one artifact are two dependencies: each version moves on its own,
/// and landing one never moves or hides the other.
#[test]
fn a_test_jar_beside_its_jar_moves_on_its_own() {
    skip_if_missing!("mvn");
    let declared = format!(
        "<dependencies>\n{}{}</dependencies>\n",
        dependency(LANG, "3.12.0"),
        dependency_with(
            LANG,
            "3.17.0",
            "\n  <type>test-jar</type>\n  <scope>test</scope>"
        )
    );
    let fixture = fixture_with_pom(&pom(&declared));
    let test_jar = artifact_key(LANG, "test-jar", "");
    let jar_key = artifact_key(LANG, "jar", "");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    let landed = upgrade.applied_change_for(LANG).expect("the jar lands");
    assert_eq!(landed.from, "3.12.0");
    let resolved = maven_resolved(&fixture);
    assert_eq!(resolved.get(&jar_key), Some(&landed.to), "{resolved:?}");
    assert_eq!(resolved.get(&test_jar).map(String::as_str), Some("3.17.0"));

    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);

    assert!(fixed.ok(), "{fixed:#?}");
    let matured = fixed
        .applied_change_for(LANG)
        .expect("the test-jar matures");
    assert_eq!(matured.from, "3.17.0");
    let resolved = maven_resolved(&fixture);
    assert_eq!(resolved.get(&test_jar), Some(&matured.to), "{resolved:?}");
    assert_eq!(resolved.get(&jar_key), Some(&landed.to), "{resolved:?}");
}

/// A pom below a reactor that no `<modules>` lists is a project of its own, planned and landed
/// apart from the reactor.
#[test]
fn a_nested_independent_pom_is_its_own_project() {
    skip_if_missing!("mvn");
    let fixture = maven_fixture();
    fixture.write("pom.xml", &reactor_root_pom(&["app"], ""));
    fixture.write(
        "app/pom.xml",
        &reactor_module_pom("app", &dependency(TEXT, "1.10.0")),
    );
    fixture.write("tools/pom.xml", &pom(&dependencies(&[(TEXT, "1.10.0")])));
    let module_before = fixture.read_bytes("app/pom.xml");
    let tools_before = fixture.read_bytes("tools/pom.xml");

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    // The reactor and the independent pom are the two projects, and the module is neither.
    for report in [&outdated, &upgrade] {
        let mut projects = report.item_projects_for(TEXT);
        projects.sort();
        assert_eq!(projects, [".", "tools"], "{report:#?}");
    }
    assert!(upgrade.ok(), "{upgrade:#?}");
    // Each project lands its own row, attributed to its own member.
    let module_rows = applied_rows_in(&upgrade, ".", "app");
    let tools_rows = applied_rows_in(&upgrade, "tools", ".");
    for landed in [&module_rows, &tools_rows] {
        assert_eq!(
            landed
                .iter()
                .map(|(name, _, _)| name.as_str())
                .collect::<Vec<_>>(),
            [TEXT],
            "{upgrade:#?}"
        );
    }
    assert_only_versions_rewritten(
        &module_rows,
        &module_before,
        &fixture.read_bytes("app/pom.xml"),
    );
    assert_only_versions_rewritten(
        &tools_rows,
        &tools_before,
        &fixture.read_bytes("tools/pom.xml"),
    );
    assert_maven_selects(&fixture, Some("tools"), &tools_rows);
}

/// A mirror standing in for Maven Central can serve other bytes under a release's coordinates, so
/// `upgrade` refuses the project without naming the mirror's address, and edits nothing.
#[test]
fn a_mirror_for_maven_central_is_refused() {
    skip_if_missing!("mvn");
    let fixture = fixture(SEED_OLD);
    let settings = fixture.maven_user_home().join(".m2/settings.xml");
    std::fs::create_dir_all(settings.parent().expect("a parent")).expect("create .m2");
    // The mirror serves Maven Central's own content, so Maven resolves through it.
    std::fs::write(
        &settings,
        indoc! {"
            <settings>
              <mirrors>
                <mirror>
                  <id>corp</id>
                  <mirrorOf>*</mirrorOf>
                  <url>https://repo1.maven.org/maven2</url>
                </mirror>
              </mirrors>
            </settings>
        "},
    )
    .expect("write settings");
    let before = project_files(fixture.root());

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(!upgrade.ok(), "{upgrade:#?}");
    assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
    // The refusal is a broken environment, a system error on each of the two rows it stopped,
    // that names the mirror but not its address.
    let errors: Vec<(String, String)> = upgrade
        .items()
        .iter()
        .filter_map(|item| {
            let error = item.get("error")?;
            let field = |key: &str| error.get(key)?.as_str().map(str::to_owned);
            Some((field("kind")?, field("message")?))
        })
        .collect();
    assert_eq!(errors.len(), 2, "{upgrade:#?}");
    for (kind, message) in &errors {
        assert_eq!(kind, "system", "{message}");
        assert!(message.contains("mirror"), "{message}");
        assert!(!message.contains("repo1.maven.org"), "{message}");
    }
    assert_eq!(project_files(fixture.root()), before);
}

/// A launcher option that builds only part of the reactor cannot vouch for an edit, so it fails
/// the run as a broken environment, naming the option but no path, and nothing is written.
#[test]
fn a_launcher_option_that_narrows_the_reactor_fails_the_run() {
    skip_if_missing!("mvn");
    let fixture = fixture(SEED_OLD);
    let before = project_files(fixture.root());

    let upgrade = fixture.cooldown_json_env(
        &["upgrade", "--freeze", FREEZE],
        &[("MAVEN_ARGS", "-B -pl demo:demo")],
    );

    assert!(!upgrade.ok(), "{upgrade:#?}");
    let errors: Vec<(String, String)> = upgrade
        .items()
        .iter()
        .filter_map(|item| {
            let error = item.get("error")?;
            let field = |key: &str| error.get(key)?.as_str().map(str::to_owned);
            Some((field("kind")?, field("message")?))
        })
        .collect();
    assert_eq!(errors.len(), 2, "{upgrade:#?}");
    for (kind, message) in &errors {
        assert_eq!(kind, "system", "{message}");
        assert!(message.contains("-pl"), "{message}");
        assert!(!message.contains("demo:demo"), "{message}");
    }
    assert_eq!(project_files(fixture.root()), before);
}

/// A pom that declares a repository other than Maven Central can have Maven fetch any dependency
/// from it, so cooldown leaves its dependencies unplanned and says why.
#[test]
fn a_pom_repository_leaves_its_dependencies_unplanned() {
    skip_if_missing!("mvn");
    let fixture = fixture_with_pom(&pom(&formatdoc! {"
        <repositories>
          <repository>
            <id>corp</id>
            <url>https://repo1.maven.org/maven2</url>
          </repository>
        </repositories>
        {declared}", declared = dependencies(SEED_OLD)}));
    let before = project_files(fixture.root());

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(outdated.item_names().is_empty(), "{outdated:#?}");
    assert!(
        outdated
            .warning_messages()
            .iter()
            .any(|message| message.contains("repository other than Maven Central")),
        "{outdated:#?}"
    );
    assert!(rows(&upgrade).is_empty(), "{upgrade:#?}");
    assert_eq!(project_files(fixture.root()), before);
}

/// A profile a file activates can be active in the project but not in a copy that lacks the
/// file, so cooldown neither reads nor moves a dependency a profile declares too, and says why.
#[test]
fn a_dependency_a_profile_declares_is_not_planned() {
    skip_if_missing!("mvn");
    let fixture = fixture_with_pom(&pom(&formatdoc! {"
        {declared}<profiles>
          <profile>
            <id>sources</id>
            <activation>
              <file>
                <exists>src/main/java</exists>
              </file>
            </activation>
            {profiled}</profile>
        </profiles>
    ", declared = dependencies(&[(TEXT, "1.10.0")]),
    profiled = dependencies(&[(TEXT, "1.11.0")])}));
    fixture.write(
        "src/main/java/demo/Demo.java",
        "package demo;\n\npublic class Demo {}\n",
    );
    let before = project_files(fixture.root());
    // The file activates the profile, so Maven resolves the profile's version, not the base one.
    assert_eq!(
        jar(&maven_resolved(&fixture), TEXT).as_deref(),
        Some("1.11.0")
    );

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(outdated.item_names().is_empty(), "{outdated:#?}");
    assert!(
        outdated
            .warning_messages()
            .iter()
            .any(|message| message.contains(TEXT) && message.contains("profile")),
        "{outdated:#?}"
    );
    assert!(rows(&upgrade).is_empty(), "{upgrade:#?}");
    assert_eq!(project_files(fixture.root()), before);
}

/// A module only a profile lists is no member cooldown reads, but where a file activates the
/// profile Maven builds it, so a version it inherits from a member cooldown edits would move
/// there too.
/// That collateral move refuses the edit whether the run covers the whole reactor or only the
/// member, and nothing is published.
#[test]
fn a_move_in_a_module_only_a_profile_lists_is_refused() {
    skip_if_missing!("mvn");
    let fixture = maven_fixture();
    let profile = indoc! {"
        <profiles>
          <profile>
            <id>extra</id>
            <activation>
              <file>
                <exists>extra.enabled</exists>
              </file>
            </activation>
            <modules>
              <module>extra</module>
            </modules>
          </profile>
        </profiles>
    "};
    fixture.write("pom.xml", &reactor_root_pom(&["app"], profile));
    // `app` is the parent of `extra`, which inherits its commons-text.
    let app = reactor_module_pom("app", &dependency(TEXT, "1.10.0")).replace(
        "<artifactId>app</artifactId>",
        "<artifactId>app</artifactId>\n  <packaging>pom</packaging>",
    );
    fixture.write("app/pom.xml", &app);
    let extra = reactor_module_pom("extra", "")
        .replace(
            "<artifactId>parent</artifactId>",
            "<artifactId>app</artifactId>",
        )
        .replace(
            "</parent>",
            "  <relativePath>../app/pom.xml</relativePath>\n  </parent>",
        );
    fixture.write("extra/pom.xml", &extra);
    fixture.write("extra.enabled", "");
    let before = project_files(fixture.root());
    let collected = maven_collected(&fixture);
    assert_eq!(
        collected
            .get("extra")
            .and_then(|module| module.get(&artifact_key(TEXT, "jar", "")))
            .map(String::as_str),
        Some("1.10.0"),
        "the file activates the profile, so Maven builds the module\n{collected:?}"
    );

    for dir in [None, Some("app")] {
        let upgrade = fixture.cooldown_json_in(dir, &["upgrade", "--freeze", FREEZE]);

        assert!(upgrade.applied_names().is_empty(), "{dir:?}\n{upgrade:#?}");
        // The reactor holds the move as a conflict that names the module it would also move.
        // Unscoped, the module is also a project of its own, which no reactor lists.
        let held: Vec<(&str, &str)> = upgrade
            .items()
            .iter()
            .filter(|item| {
                item.get("project").and_then(serde_json::Value::as_str) == Some(".")
                    && item.get("name").and_then(serde_json::Value::as_str) == Some(TEXT)
            })
            .filter_map(|item| {
                let skipped = item.get("skipped")?;
                let field = |key: &str| skipped.get(key)?.as_str();
                Some((field("reason")?, field("message")?))
            })
            .collect();
        assert!(
            matches!(held.as_slice(), [("resolver_conflict", detail)]
                if detail.contains("extra/pom.xml") && detail.contains("did not plan")),
            "{dir:?}\n{upgrade:#?}"
        );
        assert_eq!(project_files(fixture.root()), before, "{dir:?}");
    }
}

/// A dependency inside an XML comment and a build plugin's own dependency are no project
/// dependencies: cooldown neither reports nor plans them, so nothing is ever held over them, and
/// only the project's own declaration moves.
#[test]
fn comments_and_plugin_dependencies_are_not_read() {
    skip_if_missing!("mvn");
    let commented = dependency("org.slf4j:slf4j-api", "1.7.30");
    let plugin_dependencies = dependencies(&[(LANG, "3.12.0")]);
    let fixture = fixture_with_pom(&pom(&formatdoc! {"
        <dependencies>
        <!--
        {commented}-->
        {active}</dependencies>
        <build>
          <plugins>
            <plugin>
              <groupId>org.apache.maven.plugins</groupId>
              <artifactId>maven-antrun-plugin</artifactId>
              <version>3.1.0</version>
              {plugin_dependencies}
            </plugin>
          </plugins>
        </build>
    ", active = dependency(TEXT, "1.10.0")}));
    let pom_before = fixture.read_bytes("pom.xml");

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert_eq!(
        outdated.item_names(),
        BTreeSet::from([TEXT.to_owned()]),
        "{outdated:#?}"
    );
    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.item_names(),
        BTreeSet::from([TEXT.to_owned()]),
        "only the project's own dependency is planned, and nothing is held\n{upgrade:#?}"
    );
    assert_only_versions_rewritten(
        &applied_rows(&upgrade),
        &pom_before,
        &fixture.read_bytes("pom.xml"),
    );
    let resolved = maven_resolved(&fixture);
    assert!(
        jar(&resolved, "org.slf4j:slf4j-api").is_none(),
        "{resolved:?}"
    );
    assert_eq!(
        jar(&resolved, TEXT),
        upgrade.applied_change_for(TEXT).map(|change| change.to)
    );
}

/// `--build` compiles and packages the project, so it reports a compile error that Maven's
/// `validate` phase passes over, while a project that compiles builds.
#[test]
fn build_reports_a_compile_error_validate_misses() {
    skip_if_missing!("mvn");
    let project = |source: &str| {
        let fixture = fixture_with_pom(&pom(&formatdoc! {"
            <properties>
              <maven.compiler.release>17</maven.compiler.release>
            </properties>
            {declared}", declared = dependencies(&[(TEXT, "1.10.0")])}));
        fixture.write("src/main/java/demo/Demo.java", source);
        fixture
    };
    let compiles = project("package demo;\n\npublic class Demo {}\n");
    let broken = project("package demo;\n\npublic class Demo { int }\n");
    broken
        .run_tool("mvn", &["-B", "-ntp", "-q", "validate"], &[])
        .expect_success();

    let built = compiles.cooldown_json(&["upgrade", "--freeze", FREEZE, "--build"]);
    let failed = broken.cooldown_json(&["upgrade", "--freeze", FREEZE, "--build"]);

    assert_eq!(built.build_ok(), Some(true), "{built:#?}");
    assert!(built.ok(), "{built:#?}");
    assert_eq!(failed.build_ok(), Some(false), "{failed:#?}");
    assert!(!failed.ok(), "{failed:#?}");
}
