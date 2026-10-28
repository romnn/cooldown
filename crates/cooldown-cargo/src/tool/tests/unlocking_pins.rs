//! A pin or lock refresh that another locked crate blocks lands by releasing that crate.

use super::*;
use std::os::unix::fs::PermissionsExt as _;

/// Cargo's refusal when `pkg`'s target pins `blocker = "=0.9.0"` while `holder` keeps the locked
/// blocker 1.0.0, in the shape cargo 1.99 prints it.
const BLOCKED_PIN: &str = indoc! {r#"
    error: failed to select a version for `blocker`.
        ... required by package `holder v1.0.0`
    versions that meet the requirements `^1.0.0` (locked to 1.0.0) are: 1.0.0

    all possible versions conflict with previously selected packages

      previously selected package `blocker v0.9.0`
        ... which satisfies dependency `blocker = "=0.9.0"` of package `pkg v1.2.0`

    failed to select a version for `blocker` which could resolve this conflict
"#};

/// The fake `cargo`: a precise pin of `pkg` lands only beside a released `blocker`, every other
/// pin and every seeded resolve is refused, and `displace` makes the landing drag `sibling` off
/// its version too.
const SCRIPT: &str = indoc! {r#"
    #!/bin/sh
    set -eu
    printf '%s\n' "$*" >> invocations
    case "$1" in
      metadata)
        echo 'error: failed to select a version for `pkg`.' >&2
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
          ' pkg@1.0.0 blocker@1.0.0')
            if [ -f displace ]; then cp displaced.lock Cargo.lock; else cp landed.lock Cargo.lock; fi
            exit 0
            ;;
          ' pkg@1.0.0')
            cat blocked.txt >&2
            exit 101
            ;;
        esac
        echo 'error: failed to select a version for the requirement `sibling = "=1.0.0"`' >&2
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
    pkg = "1"
    sibling = "1"
    holder = "1"
"#};

fn lock(pkg: &str, sibling: &str, blocker: &str) -> String {
    let mut lock = String::from("version = 4\n");
    for (name, version) in [("blocker", blocker), ("pkg", pkg), ("sibling", sibling)] {
        lock.push_str(&formatdoc! {r#"

            [[package]]
            name = "{name}"
            version = "{version}"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        "#});
    }
    lock
}

struct UnlockFixture {
    directory: tempfile::TempDir,
    project: Project,
    tool: CargoTool,
}

impl UnlockFixture {
    fn new(script: &str) -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(root.join("Cargo.toml"), MANIFEST)?;
        std::fs::write(root.join("Cargo.lock"), lock("1.0.0", "1.0.0", "1.0.0"))?;
        std::fs::write(root.join("landed.lock"), lock("1.2.0", "1.0.0", "0.9.0"))?;
        std::fs::write(root.join("displaced.lock"), lock("1.2.0", "1.5.0", "0.9.0"))?;
        std::fs::write(root.join("blocked.txt"), BLOCKED_PIN)?;
        let fake = root.join("fake-cargo");
        std::fs::write(&fake, script)?;
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

    fn released_blocker(&self) -> eyre::Result<bool> {
        Ok(self.invocations()?.iter().any(|invocation| {
            invocation.contains("#pkg@1.0.0 -p") && invocation.contains("#blocker@1.0.0")
        }))
    }
}

/// The in-range pin Cargo refuses lands once the locked blocker it names is released, leaves the
/// requirement as written, and the blocker's move is the collateral row the apply reports.
#[tokio::test]
async fn a_pin_a_locked_crate_blocks_lands_with_that_crate_released() -> eyre::Result<()> {
    let fixture = UnlockFixture::new(SCRIPT)?;
    let planned = change("pkg", "1.0.0", "1.2.0", false);
    let plan = UnlockFixture::plan(vec![planned.clone()]);

    let rejections = fixture.resolve(&plan).await?;

    assert_eq!(fixture.lock()?, lock("1.2.0", "1.0.0", "0.9.0"));
    assert!(fixture.released_blocker()?, "{:?}", fixture.invocations()?);
    assert_eq!(
        std::fs::read_to_string(&fixture.project.manifest)?,
        MANIFEST
    );
    let mut report = ApplyReport::default();
    classify_planned_changes(
        &plan,
        &read_lock(&fixture.project)?.crates_io_locked_versions(),
        None,
        &rejections,
        &mut report,
    );
    assert_eq!(report.applied, vec![planned.clone()]);
    let collateral = collateral_changes(
        &CargoLock::parse(&lock("1.0.0", "1.0.0", "1.0.0"))?.locked_versions_by_source(),
        &read_lock(&fixture.project)?.locked_versions_by_source(),
        &report.applied,
    );
    assert_eq!(
        collateral,
        vec![collateral_change(CRATES_IO, "blocker", "1.0.0", "0.9.0")]
    );
    Ok(())
}

/// A release that would drag another planned crate off its version is not kept: the lock is
/// restored byte for byte, and the candidate is held with Cargo's own explanation.
#[tokio::test]
async fn an_unlocking_pin_that_displaces_a_planned_crate_is_restored() -> eyre::Result<()> {
    let fixture = UnlockFixture::new(SCRIPT)?;
    std::fs::write(fixture.project.root.join("displace"), "")?;
    let plan = UnlockFixture::plan(vec![
        change("pkg", "1.0.0", "1.2.0", false),
        change("sibling", "1.0.0", "1.1.0", false),
    ]);

    let rejections = fixture.resolve(&plan).await?;

    assert!(fixture.released_blocker()?, "{:?}", fixture.invocations()?);
    assert_eq!(fixture.lock()?, lock("1.0.0", "1.0.0", "1.0.0"));
    let detail = rejections
        .get(&("pkg".to_owned(), "1.0.0".to_owned(), "1.2.0".to_owned()))
        .ok_or_else(|| eyre::eyre!("the held pin keeps its rejection: {rejections:?}"))?;
    assert!(
        detail.contains("failed to select a version for `blocker`"),
        "{detail}"
    );
    Ok(())
}

/// A blocker the batch itself plans to move reaches its own target through its own pin, so it is
/// never released for another candidate's sake.
#[tokio::test]
async fn a_planned_blocker_is_not_released_for_another_pin() -> eyre::Result<()> {
    let fixture = UnlockFixture::new(SCRIPT)?;
    let plan = UnlockFixture::plan(vec![
        change("pkg", "1.0.0", "1.2.0", false),
        change("blocker", "1.0.0", "1.1.0", false),
    ]);

    fixture.resolve(&plan).await?;

    assert!(!fixture.released_blocker()?, "{:?}", fixture.invocations()?);
    assert_eq!(fixture.lock()?, lock("1.0.0", "1.0.0", "1.0.0"));
    Ok(())
}

/// A lock the run's own manifest edit left unresolvable is re-resolved by releasing each locked
/// crate Cargo's conflict names in turn, never the whole workspace.
#[tokio::test]
async fn a_conflicted_lock_refresh_releases_each_named_blocker_until_it_resolves()
-> eyre::Result<()> {
    let fixture = UnlockFixture::new(indoc! {r#"
        #!/bin/sh
        set -eu
        printf '%s\n' "$*" >> invocations
        conflict() {
          printf 'error: failed to select a version for `%s`.\n' "$1" >&2
          echo '    ... required by package `shared v0.1.0 (/repo/shared)`' >&2
          echo 'versions that meet the requirements `^1.2.0` are: 1.2.0' >&2
          echo 'all possible versions conflict with previously selected packages' >&2
          printf '  previously selected package `%s v1.0.0`\n' "$1" >&2
          exit 101
        }
        case "$*" in
          'metadata --all-features --locked --format-version 1') conflict pkg ;;
          *'#pkg@1.0.0 -p '*'#sibling@1.0.0') cp landed.lock Cargo.lock; exit 0 ;;
          *'#pkg@1.0.0') conflict sibling ;;
        esac
        exit 102
    "#})?;

    let report = fixture
        .tool
        .refresh_conflicted_lock(&fixture.project)
        .await?
        .ok_or_else(|| eyre::eyre!("the conflicted lock must be refreshed"))?;

    assert_eq!(report.status, LockStatus::Current);
    assert!(
        report.detail.contains("pkg@1.0.0, sibling@1.0.0"),
        "{}",
        report.detail
    );
    assert_eq!(fixture.lock()?, lock("1.2.0", "1.0.0", "0.9.0"));
    let updates: Vec<_> = fixture
        .invocations()?
        .into_iter()
        .filter(|invocation| invocation.starts_with("update"))
        .collect();
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert!(
        updates.iter().all(|update| !update.contains("--workspace")),
        "{updates:?}"
    );
    Ok(())
}
