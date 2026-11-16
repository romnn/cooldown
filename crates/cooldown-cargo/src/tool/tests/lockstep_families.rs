//! A pin a locked lockstep family blocks lands by releasing the family with it.

use super::*;
use std::os::unix::fs::PermissionsExt as _;

/// Cargo's refusal when `futures`'s target pins `js-sys = "=0.3.107"` while the locked `web-sys`
/// keeps `js-sys` at 0.3.106, in the shape cargo prints it.
const BLOCKED_PIN: &str = indoc! {r#"
    error: failed to select a version for `js-sys`.
        ... required by package `web-sys v0.3.106`
    versions that meet the requirements `=0.3.106` are: 0.3.106

    all possible versions conflict with previously selected packages

      previously selected package `js-sys v0.3.107`
        ... which satisfies dependency `js-sys = "=0.3.107"` of package `futures v0.4.80`

    failed to select a version for `js-sys` which could resolve this conflict
"#};

/// Cargo's refusal of a precise pin of `js-sys` itself, which its locked exact requirers hold.
const HELD_SUBJECT: &str = indoc! {r"
    error: failed to select a version for `js-sys`.
        ... required by package `futures v0.4.79`
    versions that meet the requirements `=0.3.106` (locked to 0.3.106) are: 0.3.106

    failed to select a version for `js-sys` which could resolve this conflict
"};

/// The fake `cargo`: a precise pin of `futures` lands only beside a released `web-sys`, and one
/// of `js-sys` only beside both its released requirers; either moves the whole family to its next
/// snapshot, and every other update and every resolve is refused.
const SCRIPT: &str = indoc! {r#"
    #!/bin/sh
    set -eu
    printf '%s\n' "$*" >> invocations
    case "$1" in
      metadata)
        echo 'error: failed to select a version for `js-sys`.' >&2
        exit 101
        ;;
      update)
        shift
        specs=''
        while [ "$#" -gt 0 ]; do
          case "$1" in
            -p) shift; specs="$specs ${1##*#}" ;;
            --precise) shift ;;
          esac
          shift
        done
        case "$specs" in
          ' futures@0.4.79'*'web-sys@0.3.106'* | ' js-sys@0.3.106'*'futures@0.4.79'*'web-sys@0.3.106'*)
            cp landed.lock Cargo.lock
            exit 0
            ;;
          ' js-sys@0.3.106'*)
            cat held.txt >&2
            exit 101
            ;;
        esac
        cat blocked.txt >&2
        exit 101
        ;;
    esac
    exit 102
"#};

const MANIFEST: &str = indoc! {r#"
    [package]
    name = "app"
    version = "0.1.0"

    [dependencies]
    futures = "0.4"
"#};

/// The graph the dependency read records: `futures` and `web-sys` both pin `js-sys` exactly.
const GRAPH: &str = indoc! {r#"
    {
        "packages": [
            {"id": "app", "name": "app", "version": "0.1.0",
             "dependencies": [{"name": "futures", "req": "^0.4"}]},
            {"id": "futures", "name": "futures", "version": "0.4.79",
             "source": "registry+https://github.com/rust-lang/crates.io-index",
             "dependencies": [{"name": "js-sys", "req": "=0.3.106"},
                              {"name": "web-sys", "req": "^0.3"}]},
            {"id": "web-sys", "name": "web-sys", "version": "0.3.106",
             "source": "registry+https://github.com/rust-lang/crates.io-index",
             "dependencies": [{"name": "js-sys", "req": "=0.3.106"}]},
            {"id": "js-sys", "name": "js-sys", "version": "0.3.106",
             "source": "registry+https://github.com/rust-lang/crates.io-index"}
        ],
        "workspace_members": ["app"],
        "workspace_root": "",
        "resolve": {"nodes": [
            {"id": "app", "deps": [{"pkg": "futures"}]},
            {"id": "futures", "deps": [{"pkg": "js-sys"}, {"pkg": "web-sys"}]},
            {"id": "web-sys", "deps": [{"pkg": "js-sys"}]},
            {"id": "js-sys", "deps": []}
        ]}
    }
"#};

fn lock(futures: &str, web_sys: &str, js_sys: &str) -> String {
    let mut lock = String::from("version = 4\n");
    for (name, version) in [
        ("futures", futures),
        ("js-sys", js_sys),
        ("web-sys", web_sys),
    ] {
        lock.push_str(&formatdoc! {r#"

            [[package]]
            name = "{name}"
            version = "{version}"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        "#});
    }
    lock
}

struct FamilyFixture {
    directory: tempfile::TempDir,
    project: Project,
    tool: CargoTool,
}

impl FamilyFixture {
    fn new() -> eyre::Result<Self> {
        let fixture = Self::without_edges()?;
        fixture.record_edges()?;
        Ok(fixture)
    }

    /// The fixture before any graph read recorded the family's exact edges.
    fn without_edges() -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(root.join("Cargo.toml"), MANIFEST)?;
        std::fs::write(
            root.join("Cargo.lock"),
            lock("0.4.79", "0.3.106", "0.3.106"),
        )?;
        std::fs::write(
            root.join("landed.lock"),
            lock("0.4.80", "0.3.107", "0.3.107"),
        )?;
        std::fs::write(root.join("blocked.txt"), BLOCKED_PIN)?;
        std::fs::write(root.join("held.txt"), HELD_SUBJECT)?;
        let fake = root.join("fake-cargo");
        std::fs::write(&fake, SCRIPT)?;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
        let mut tool = CargoTool::from_http(SharedHttp::new(
            root.join("cache"),
            cooldown_registry::HttpOptions::default(),
        )?);
        tool.cargo = Cargo::with_bin(fake.as_str());
        let project = Project {
            manifest: root.join("Cargo.toml"),
            root,
            kind: CARGO_ID,
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        Ok(Self {
            directory,
            project,
            tool,
        })
    }

    /// Records the family's exact edges, as the dependency read before planning does.
    fn record_edges(&self) -> eyre::Result<()> {
        self.tool
            .lockstep
            .record(&Cargo::parse_graph(GRAPH, &self.project.generated_members)?);
        Ok(())
    }

    fn plan(changes: Vec<Change>) -> Plan {
        Plan {
            changes,
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        }
    }

    async fn resolve(&self, plan: &Plan) -> eyre::Result<PinRejections> {
        let journal = self.tool.mutation_journal(&self.project, plan).await?;
        Ok(self
            .tool
            .whole_graph_resolve(&self.project, plan, &journal, None)
            .await?)
    }

    fn lock(&self) -> eyre::Result<String> {
        Ok(std::fs::read_to_string(
            self.project.root.join("Cargo.lock"),
        )?)
    }

    fn invocations(&self) -> eyre::Result<Vec<String>> {
        Ok(
            std::fs::read_to_string(self.directory.path().join("invocations"))?
                .lines()
                .map(str::to_owned)
                .collect(),
        )
    }

    /// Whether some pin of `futures` released `web-sys`, its exact co-requirer of `js-sys`.
    fn released_co_requirer(&self) -> eyre::Result<bool> {
        Ok(self.invocations()?.iter().any(|invocation| {
            invocation.contains("#futures@0.4.79 -p") && invocation.contains("#web-sys@0.3.106")
        }))
    }
}

/// Two planned crates that pin the same crate exactly, each refused alone because the other still
/// pins the old version, land together: the pin of one releases the other as a member of the
/// family the refusal names.
#[tokio::test]
async fn mutually_pinning_planned_crates_land_together() -> eyre::Result<()> {
    let fixture = FamilyFixture::new()?;
    let plan = FamilyFixture::plan(vec![
        change("futures", "0.4.79", "0.4.80", false),
        change("web-sys", "0.3.106", "0.3.107", false),
    ]);

    let rejections = fixture.resolve(&plan).await?;

    // The whole family reached the next snapshot in the pin that released `web-sys`.
    assert!(
        fixture.released_co_requirer()?,
        "{:?}",
        fixture.invocations()?
    );
    assert_eq!(fixture.lock()?, lock("0.4.80", "0.3.107", "0.3.107"));
    assert!(rejections.is_empty(), "{rejections:?}");
    Ok(())
}

/// A refusal that names only the pinned crate is not remembered when a family holds it: while the
/// plan protects `futures` at 0.4.79 the family cannot move, but the same pin from identical
/// files lands once the plan no longer protects it.
#[tokio::test]
async fn a_refusal_naming_only_a_held_family_member_is_retried_in_another_plan() -> eyre::Result<()>
{
    let fixture = FamilyFixture::new()?;
    let candidate = change("js-sys", "0.3.106", "0.3.107", false);
    let releases_both = |fixture: &FamilyFixture| -> eyre::Result<bool> {
        Ok(fixture.invocations()?.iter().any(|invocation| {
            invocation.contains("#js-sys@0.3.106 -p")
                && invocation.contains("#futures@0.4.79")
                && invocation.contains("#web-sys@0.3.106")
        }))
    };

    // `futures` sits at a planned target, so only `web-sys` may be released, which is not enough.
    fixture
        .resolve(&FamilyFixture::plan(vec![
            candidate.clone(),
            change("futures", "0.4.78", "0.4.79", false),
        ]))
        .await?;
    assert!(!releases_both(&fixture)?, "{:?}", fixture.invocations()?);
    assert_eq!(fixture.lock()?, lock("0.4.79", "0.3.106", "0.3.106"));

    // Without that target both requirers are released and the family moves.
    fixture
        .resolve(&FamilyFixture::plan(vec![candidate]))
        .await?;
    assert!(releases_both(&fixture)?, "{:?}", fixture.invocations()?);
    assert_eq!(fixture.lock()?, lock("0.4.80", "0.3.107", "0.3.107"));
    Ok(())
}

/// A refusal remembered before the run knew the family's edges is not recalled once it does: the
/// same pin from identical files is tried again and lands with the family released.
#[tokio::test]
async fn a_refusal_remembered_before_the_family_was_known_is_retried() -> eyre::Result<()> {
    let fixture = FamilyFixture::without_edges()?;
    let plan = FamilyFixture::plan(vec![change("js-sys", "0.3.106", "0.3.107", false)]);

    // Unaware of the family, the refused pin of `js-sys` releases nothing.
    fixture.resolve(&plan).await?;
    assert_eq!(fixture.lock()?, lock("0.4.79", "0.3.106", "0.3.106"));

    fixture.record_edges()?;
    fixture.resolve(&plan).await?;
    assert_eq!(fixture.lock()?, lock("0.4.80", "0.3.107", "0.3.107"));
    Ok(())
}
