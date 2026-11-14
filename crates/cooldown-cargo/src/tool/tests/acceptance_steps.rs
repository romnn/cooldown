//! Every lock-changing step is judged by the acceptance rule: a step that moves a directly declared
//! crate outside the plan is refused and rolled back, and a pin that overshoots its own target is
//! re-pinned before it is judged.

use super::*;
use std::os::unix::fs::PermissionsExt as _;

/// The fake `cargo`: every precise pin of `pkg`, and every unlocked resolve of a lock seeded away
/// from the original, lands the target and replaces `declared` 1.0.0 by 0.9.0, the way Cargo
/// floats a dependency of the target.
const SCRIPT: &str = indoc! {r#"
    #!/bin/sh
    set -eu
    printf '%s\n' "$*" >> invocations
    case "$1" in
      metadata)
        if ! cmp -s Cargo.lock original.lock; then cp moved.lock Cargo.lock; fi
        echo '{"packages": [], "workspace_members": [], "workspace_root": "", "resolve": null}'
        exit 0
        ;;
      update)
        cp moved.lock Cargo.lock
        exit 0
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
    declared = "1"
"#};

/// A lock of `app` at `pkg` and `dependency` (the crate `pkg` requires), where `app` lists
/// `app_dependencies`.
fn lock(app_dependencies: &str, pkg: &str, dependency: &str) -> String {
    formatdoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = [{app_dependencies}]

        [[package]]
        name = "declared"
        version = "{dependency}"
        source = "registry+https://github.com/rust-lang/crates.io-index"

        [[package]]
        name = "pkg"
        version = "{pkg}"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        dependencies = ["declared"]
    "#}
}

/// `app` declares both crates.
const DECLARING: &str = r#""declared", "pkg""#;

/// `app` declares only `pkg`, so `declared` is a transitive dependency of it.
const TRANSITIVE: &str = r#""pkg""#;

struct DeclaredFixture {
    directory: tempfile::TempDir,
    project: Project,
    tool: CargoTool,
}

impl DeclaredFixture {
    fn new(app_dependencies: &str, target: &str) -> eyre::Result<Self> {
        Self::with_locks(
            &lock(app_dependencies, "1.0.0", "1.0.0"),
            &lock(app_dependencies, target, "0.9.0"),
        )
    }

    /// A fixture starting from the `original` lock, which every pin and every resolve of a seeded
    /// lock replaces by `moved`.
    fn with_locks(original: &str, moved: &str) -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(root.join("Cargo.toml"), MANIFEST)?;
        std::fs::write(root.join("Cargo.lock"), original)?;
        std::fs::write(root.join("original.lock"), original)?;
        std::fs::write(root.join("moved.lock"), moved)?;
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

    fn plan(change: &Change) -> Plan {
        Plan {
            changes: vec![change.clone()],
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        }
    }

    fn read(&self, relative: &str) -> eyre::Result<String> {
        Ok(std::fs::read_to_string(
            self.directory.path().join(relative),
        )?)
    }

    /// Resolves the plan of `planned` and returns the rejections it recorded.
    async fn resolve(&self, planned: &Change) -> eyre::Result<PinRejections> {
        let plan = DeclaredFixture::plan(planned);
        let journal = self.tool.mutation_journal(&self.project, &plan).await?;
        Ok(self
            .tool
            .whole_graph_resolve(&self.project, &plan, &journal, None)
            .await?)
    }
}

/// The precise pin Cargo completes is still refused when it floats the declared crate: the lock
/// is restored byte for byte, and the candidate is held with the move it would have made.
#[tokio::test]
async fn a_pin_that_moves_a_declared_crate_is_refused_and_held() -> eyre::Result<()> {
    let fixture = DeclaredFixture::new(DECLARING, "1.2.0")?;
    let planned = change("pkg", "1.0.0", "1.2.0", false);
    let plan = DeclaredFixture::plan(&planned);
    let journal = fixture
        .tool
        .mutation_journal(&fixture.project, &plan)
        .await?;

    let rejections = fixture
        .tool
        .whole_graph_resolve(&fixture.project, &plan, &journal, None)
        .await?;

    assert_eq!(
        fixture.read("Cargo.lock")?,
        lock(DECLARING, "1.0.0", "1.0.0")
    );
    let detail = rejections
        .get(&rejection_key(&planned))
        .ok_or_else(|| eyre::eyre!("the refused pin records why: {rejections:?}"))?;
    assert_eq!(
        detail,
        "would move directly declared declared 1.0.0 to 0.9.0"
    );
    let mut report = ApplyReport::default();
    classify_planned_changes(
        &plan,
        &read_lock(&fixture.project)?.crates_io_locked_versions(),
        None,
        &rejections,
        &mut report,
    );
    assert!(report.applied.is_empty(), "{:?}", report.applied);
    let held: Vec<_> = report
        .skipped
        .iter()
        .map(|skipped| (skipped.change.package.name.as_str(), skipped.reason))
        .collect();
    assert_eq!(held, [("pkg", SkipReason::ResolverConflict)]);
    Ok(())
}

/// The same move of a crate the workspace does not declare lands, and the dependency's move is
/// the transitive collateral row the apply reports.
#[tokio::test]
async fn the_same_move_of_a_transitive_crate_lands_as_collateral() -> eyre::Result<()> {
    let fixture = DeclaredFixture::new(TRANSITIVE, "1.2.0")?;
    let planned = change("pkg", "1.0.0", "1.2.0", false);
    let plan = DeclaredFixture::plan(&planned);
    let journal = fixture
        .tool
        .mutation_journal(&fixture.project, &plan)
        .await?;

    let rejections = fixture
        .tool
        .whole_graph_resolve(&fixture.project, &plan, &journal, None)
        .await?;

    assert!(rejections.is_empty(), "{rejections:?}");
    let before = CargoLock::parse(&lock(TRANSITIVE, "1.0.0", "1.0.0"))?;
    let after = read_lock(&fixture.project)?;
    let collateral = collateral_changes(
        &before.locked_versions_by_source(),
        &after.locked_versions_by_source(),
        std::slice::from_ref(&planned),
        &DeclaredSides {
            before: before.declared_endpoints(&fixture.project.generated_members),
            after: after.declared_endpoints(&fixture.project.generated_members),
        },
    );
    assert_eq!(
        collateral,
        [collateral_change(
            CRATES_IO, "declared", "1.0.0", "0.9.0", false
        )]
    );
    Ok(())
}

/// A joint widen whose resolve moves the declared crate restores the manifest it widened along
/// with the lock, not just the lock bytes.
#[tokio::test]
async fn a_joint_widen_that_moves_a_declared_crate_restores_its_manifest_edit() -> eyre::Result<()>
{
    let fixture = DeclaredFixture::new(DECLARING, "2.0.0")?;
    let planned = change("pkg", "1.0.0", "2.0.0", false);
    let plan = DeclaredFixture::plan(&planned);
    let journal = fixture
        .tool
        .mutation_journal(&fixture.project, &plan)
        .await?;
    let mut stats = WholeGraphStats::default();

    let retained = fixture
        .tool
        .joint_widen_probe(
            WidenProbe {
                project: &fixture.project,
                plan: &plan.changes,
                rewrite: RewriteMode::Auto,
                candidates: &[&planned],
                protected: &[],
                followers: &[],
                journal: &journal,
                observer: None,
            },
            CulpritDrop::First,
            &mut stats,
        )
        .await?;

    assert!(retained.is_none(), "the violating widen must not be kept");
    let invocations = fixture.read("invocations")?;
    assert!(invocations.starts_with("metadata"), "{invocations}");
    assert_eq!(fixture.read("Cargo.toml")?, MANIFEST);
    assert_eq!(
        fixture.read("Cargo.lock")?,
        lock(DECLARING, "1.0.0", "1.0.0")
    );
    Ok(())
}

/// Without the declared crate's move, the same widen is kept: the refusal above is the rule's.
#[tokio::test]
async fn a_joint_widen_that_keeps_declared_crates_is_retained() -> eyre::Result<()> {
    let fixture = DeclaredFixture::new(TRANSITIVE, "2.0.0")?;
    let planned = change("pkg", "1.0.0", "2.0.0", false);
    let plan = DeclaredFixture::plan(&planned);
    let journal = fixture
        .tool
        .mutation_journal(&fixture.project, &plan)
        .await?;
    let mut stats = WholeGraphStats::default();

    let retained = fixture
        .tool
        .joint_widen_probe(
            WidenProbe {
                project: &fixture.project,
                plan: &plan.changes,
                rewrite: RewriteMode::Auto,
                candidates: &[&planned],
                protected: &[],
                followers: &[],
                journal: &journal,
                observer: None,
            },
            CulpritDrop::First,
            &mut stats,
        )
        .await?
        .ok_or_else(|| eyre::eyre!("the admissible widen must be kept"))?;

    assert!(retained.reached(&planned));
    assert!(fixture.read("Cargo.toml")?.contains(r#"pkg = "2"#));
    Ok(())
}

/// The fake `cargo` of the overshoot tests: the pin of `pkg` 1.0.0 resolves the widened line
/// freshly to 2.1.0, and the re-pin from there lands the target 2.0.0 unless `refuse` exists.
const OVERSHOOT_SCRIPT: &str = indoc! {r#"
    #!/bin/sh
    set -eu
    printf '%s\n' "$*" >> invocations
    case "$*" in
      *'#pkg@1.0.0 --precise 2.0.0') cp overshot.lock Cargo.lock; exit 0 ;;
      *'#pkg@2.1.0 --precise 2.0.0')
        if [ -f refuse ]; then
          echo 'error: failed to select a version for the requirement `pkg = "^2.1"`' >&2
          exit 101
        fi
        cp landed.lock Cargo.lock
        exit 0
        ;;
    esac
    echo 'error: unexpected invocation' >&2
    exit 101
"#};

impl DeclaredFixture {
    fn overshooting() -> eyre::Result<Self> {
        let fixture = Self::new(DECLARING, "2.0.0")?;
        let root = &fixture.project.root;
        std::fs::write(
            root.join("overshot.lock"),
            lock(DECLARING, "2.1.0", "1.0.0"),
        )?;
        std::fs::write(root.join("landed.lock"), lock(DECLARING, "2.0.0", "1.0.0"))?;
        std::fs::write(root.join("fake-cargo"), OVERSHOOT_SCRIPT)?;
        Ok(fixture)
    }

    async fn pin(&self, planned: &Change) -> eyre::Result<PinRejections> {
        let plan = std::slice::from_ref(planned);
        let journal = self
            .tool
            .mutation_journal(&self.project, &DeclaredFixture::plan(planned))
            .await?;
        let mut rejections = PinRejections::new();
        self.tool
            .pin_individually(
                PinPass {
                    scope: PinScope {
                        project: &self.project,
                        plan,
                        journal: &journal,
                        observer: None,
                    },
                    batch: &[planned],
                },
                &[planned],
                &mut rejections,
                &mut PinBatchStats::default(),
            )
            .await?;
        Ok(rejections)
    }
}

/// A pin that leaves its own candidate past the target, as one into a widened major does, is
/// re-pinned to the target in the same step instead of being judged a drift.
#[tokio::test]
async fn a_pin_that_overshoots_its_own_target_is_repinned_to_it() -> eyre::Result<()> {
    let fixture = DeclaredFixture::overshooting()?;
    let planned = change("pkg", "1.0.0", "2.0.0", false);

    let rejections = fixture.pin(&planned).await?;

    assert!(rejections.is_empty(), "{rejections:?}");
    assert_eq!(
        fixture.read("Cargo.lock")?,
        lock(DECLARING, "2.0.0", "1.0.0")
    );
    let invocations = fixture.read("invocations")?;
    assert!(
        invocations.contains("#pkg@2.1.0 --precise 2.0.0"),
        "{invocations}"
    );
    Ok(())
}

/// When Cargo refuses the re-pin, the overshoot is not kept: the lock is restored and the
/// candidate is held with Cargo's refusal.
#[tokio::test]
async fn a_refused_repin_restores_the_lock_the_overshoot_replaced() -> eyre::Result<()> {
    let fixture = DeclaredFixture::overshooting()?;
    std::fs::write(fixture.project.root.join("refuse"), "")?;
    let planned = change("pkg", "1.0.0", "2.0.0", false);

    let rejections = fixture.pin(&planned).await?;

    assert_eq!(
        fixture.read("Cargo.lock")?,
        lock(DECLARING, "1.0.0", "1.0.0")
    );
    let detail = rejections
        .get(&rejection_key(&planned))
        .ok_or_else(|| eyre::eyre!("the refused re-pin records why: {rejections:?}"))?;
    assert!(detail.contains("failed to select a version"), "{detail}");
    Ok(())
}

/// A lock of the crates.io `packages`, given as `(name, version, dependencies)`, beside the local
/// `locals`, given the same way, which carry no source.
fn graph_lock(locals: &[(&str, &str, &[&str])], packages: &[(&str, &str, &[&str])]) -> String {
    let block = |name: &str, version: &str, dependencies: &[&str], source: Option<&str>| {
        let source = source
            .map(|source| format!("source = \"{source}\""))
            .unwrap_or_default();
        let entries: Vec<String> = dependencies
            .iter()
            .map(|entry| format!("\"{entry}\""))
            .collect();
        let entries = entries.join(", ");
        formatdoc! {r#"

            [[package]]
            name = "{name}"
            version = "{version}"
            {source}
            dependencies = [{entries}]
        "#}
    };
    let mut lock = String::from("version = 4\n");
    for (name, version, dependencies) in locals {
        lock.push_str(&block(name, version, dependencies, None));
    }
    for (name, version, dependencies) in packages {
        lock.push_str(&block(
            name,
            version,
            dependencies,
            Some("registry+https://github.com/rust-lang/crates.io-index"),
        ));
    }
    lock
}

/// A plan for one member's line of a crate authorizes only that member's binding: cargo unifying
/// the other member's declared 2.0.0 onto the planned 2.1.0 target, although it shares the
/// target's compatibility line, is refused and the lock restored.
#[tokio::test]
async fn a_planned_line_does_not_move_another_members_declared_major() -> eyre::Result<()> {
    let original = graph_lock(
        &[
            ("a", "0.1.0", &["foo 1.0.0"]),
            ("b", "0.1.0", &["foo 2.0.0"]),
        ],
        &[("foo", "1.0.0", &[]), ("foo", "2.0.0", &[])],
    );
    let unified = graph_lock(
        &[("a", "0.1.0", &["foo"]), ("b", "0.1.0", &["foo"])],
        &[("foo", "2.1.0", &[])],
    );
    let fixture = DeclaredFixture::with_locks(&original, &unified)?;
    let planned = change("foo", "1.0.0", "2.1.0", false);

    let rejections = fixture.resolve(&planned).await?;

    assert_eq!(fixture.read("Cargo.lock")?, original);
    let detail = rejections
        .get(&rejection_key(&planned))
        .ok_or_else(|| eyre::eyre!("the refused pin records why: {rejections:?}"))?;
    assert_eq!(detail, "would move directly declared foo 2.0.0 to 2.1.0");
    Ok(())
}

/// The control: the same plan lands when the other member keeps its own 2.0.0.
#[tokio::test]
async fn a_planned_line_lands_beside_another_members_declared_major() -> eyre::Result<()> {
    let original = graph_lock(
        &[
            ("a", "0.1.0", &["foo 1.0.0"]),
            ("b", "0.1.0", &["foo 2.0.0"]),
        ],
        &[("foo", "1.0.0", &[]), ("foo", "2.0.0", &[])],
    );
    let split = graph_lock(
        &[
            ("a", "0.1.0", &["foo 2.1.0"]),
            ("b", "0.1.0", &["foo 2.0.0"]),
        ],
        &[("foo", "2.0.0", &[]), ("foo", "2.1.0", &[])],
    );
    let fixture = DeclaredFixture::with_locks(&original, &split)?;
    let planned = change("foo", "1.0.0", "2.1.0", false);

    let rejections = fixture.resolve(&planned).await?;

    assert!(rejections.is_empty(), "{rejections:?}");
    assert_eq!(fixture.read("Cargo.lock")?, split);
    Ok(())
}

/// The `app → parent 1 → shim → leaf` graph, where `shim` is a path package (a
/// `[patch.crates-io]` replacement outside the workspace members) whose own declaration binds
/// `leaf`.
fn patched_shim_lock(parent: &str, leaf: &str) -> String {
    graph_lock(
        &[("app", "0.1.0", &["parent"]), ("shim", "0.1.0", &["leaf"])],
        &[("leaf", leaf, &[]), ("parent", parent, &["shim"])],
    )
}

/// Upgrading to a parent that no longer needs the patched `shim` drops it from the lock, and its
/// `leaf` declaration goes with it instead of refusing the cleanup.
#[tokio::test]
async fn an_upgrade_that_drops_a_patched_path_crate_lands() -> eyre::Result<()> {
    let original = patched_shim_lock("1.0.0", "1.0.0");
    let upgraded = format!(
        "{}{}",
        graph_lock(
            &[("app", "0.1.0", &["parent"])],
            &[("parent", "2.0.0", &[])]
        ),
        indoc! {r#"

            [[patch.unused]]
            name = "shim"
            version = "0.1.0"
        "#},
    );
    let fixture = DeclaredFixture::with_locks(&original, &upgraded)?;
    let planned = change("parent", "1.0.0", "2.0.0", false);

    let rejections = fixture.resolve(&planned).await?;

    assert!(rejections.is_empty(), "{rejections:?}");
    assert_eq!(fixture.read("Cargo.lock")?, upgraded);
    Ok(())
}

/// The control: while the parent still reaches `shim`, the path crate's declared `leaf` is
/// protected like a member's, so a pin that moves it is refused and the lock restored.
#[tokio::test]
async fn a_reachable_path_crates_declared_dependency_still_holds() -> eyre::Result<()> {
    let original = patched_shim_lock("1.0.0", "1.0.0");
    let fixture = DeclaredFixture::with_locks(&original, &patched_shim_lock("1.1.0", "1.2.0"))?;
    let planned = change("parent", "1.0.0", "1.1.0", false);

    let rejections = fixture.resolve(&planned).await?;

    assert_eq!(fixture.read("Cargo.lock")?, original);
    let detail = rejections
        .get(&rejection_key(&planned))
        .ok_or_else(|| eyre::eyre!("the refused pin records why: {rejections:?}"))?;
    assert_eq!(detail, "would move directly declared leaf 1.0.0 to 1.2.0");
    Ok(())
}
