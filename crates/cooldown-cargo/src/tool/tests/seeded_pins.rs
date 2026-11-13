//! Seeded Cargo pins preserve exact targets and isolate resolver conflicts without linear resolves.

use super::*;
use std::os::unix::fs::PermissionsExt as _;

const SCRIPT: &str = indoc! {r#"
    #!/bin/sh
    set -eu
    printf '%s\n' "$*" >> invocations
    crates_io='registry+https://github.com/rust-lang/crates.io-index'

    rewrite() {
      awk -v selected="$1" -v selected_source="$2" -v target="$3" '
        function quoted(line) {
          sub(/^[^"]*"/, "", line)
          sub(/"[^"]*$/, "", line)
          return line
        }
        function emit(  position, skip) {
          skip = name == selected && source == selected_source && emitted_selected
          if (name == selected && source == selected_source) emitted_selected = 1
          for (position = 1; position <= count; position++) {
            if (name == selected && source == selected_source && lines[position] ~ /^version = /)
              lines[position] = "version = \"" target "\""
            if (!skip) print lines[position]
          }
          count = 0
          name = ""
          source = ""
        }
        /^\[\[package\]\]/ { emit() }
        {
          lines[++count] = $0
          if ($0 ~ /^name = /) name = quoted($0)
          if ($0 ~ /^source = /) source = quoted($0)
        }
        END { emit() }
      ' Cargo.lock > rewritten.lock
      mv rewritten.lock Cargo.lock
    }

    refused_moved() {
      for file in refuse-*; do
        [ -f "$file" ] || continue
        if awk -v name="${file#refuse-}" '
          /^\[\[package\]\]/ { selected = 0 }
          $0 == "name = \"" name "\"" { selected = 1 }
          selected && /^version = / && $0 != "version = \"1.0.0\"" { found = 1 }
          END { exit !found }
        ' Cargo.lock; then
          return 0
        fi
      done
      return 1
    }

    case "$1" in
      metadata)
        if [ -f interrupt ]; then kill -TERM "$$"; fi
        if refused_moved; then
          echo 'error: joint seed cannot resolve' >&2
          exit 101
        fi
        if [ -f permission-failure ]; then
          echo 'error: Permission denied reading the registry cache' >&2
          exit 101
        fi
        if [ -f bad-name ]; then
          bad=$(cat bad-name)
          if awk -v bad="$bad" '
            /^\[\[package\]\]/ { selected = 0 }
            $0 == "name = \"" bad "\"" { selected = 1 }
            selected && $0 == "version = \"1.1.0\"" { found = 1 }
            END { exit !found }
          ' Cargo.lock; then
            echo 'error: joint seed cannot resolve' >&2
            exit 101
          fi
        fi
        if [ -f drift-name ]; then
          drift=$(cat drift-name)
          if awk -v drift="$drift" '
            /^\[\[package\]\]/ { selected = 0 }
            $0 == "name = \"" drift "\"" { selected = 1 }
            selected && $0 == "version = \"1.1.0\"" { found = 1 }
            END { exit !found }
          ' Cargo.lock; then
            rewrite "$drift" "$crates_io" '1.0.5'
          fi
        fi
        if [ -f leave-name ]; then rewrite "$(cat leave-name)" "$crates_io" '1.0.0'; fi
        if [ -f driveby-name ]; then rewrite "$(cat driveby-name)" "$(cat driveby-source)" "$(cat driveby-target)"; fi
        if [ -f replace-lock ]; then
          mv Cargo.lock foreign.lock
          ln -s foreign.lock Cargo.lock
          echo 'error: joint seed cannot resolve' >&2
          exit 101
        fi
        echo '{"packages": [], "workspace_members": [], "workspace_root": "", "resolve": null}'
        exit 0
        ;;
      update)
        if [ -f require-restored ] && [ ! -f restore-verified ]; then
          if ! cmp -s Cargo.lock original.lock; then
            echo 'error: fallback saw an unrestored seed' >&2
            exit 101
          fi
          touch restore-verified
        fi
        spec=''
        target=''
        while [ "$#" -gt 0 ]; do
          case "$1" in
            -p) shift; spec="$1" ;;
            --precise) shift; target="$1" ;;
          esac
          shift
        done
        name="${spec##*#}"
        name="${name%@*}"
        if [ -f "refuse-$name" ]; then
          party=$(cat "refuse-$name")
          if [ -n "$party" ]; then
            printf 'error: failed to select a version for `%s`.\n' "$party" >&2
            echo '    ... required by package `holder v1.0.0`' >&2
            echo 'versions that meet the requirements `^1.0.0` (locked to 1.0.0) are: 1.0.0' >&2
            echo 'all possible versions conflict with previously selected packages' >&2
            printf '  previously selected package `%s v0.9.0`\n' "$party" >&2
            printf '    ... which satisfies dependency `%s = "=0.9.0"` of package `%s v%s`\n' \
              "$party" "$name" "$target" >&2
            printf 'failed to select a version for `%s` which could resolve this conflict\n' \
              "$party" >&2
          else
            printf 'error: failed to select a version for the requirement `%s = "^1.0"`\n' \
              "$name" >&2
          fi
          exit 101
        fi
        if [ -f drift-name ] && [ "$(cat drift-name)" = "$name" ]; then
          printf 'error: failed to select a version for the requirement `%s = "^1.0"`\n' "$name" >&2
          exit 101
        fi
        if [ -f bad-name ] && [ "$(cat bad-name)" = "$name" ]; then
          printf 'error: failed to select a version for the requirement `%s = "=1.0.0"`\n' "$name" >&2
          echo 'required by package `holder v1.0.0`' >&2
          exit 101
        fi
        rewrite "$name" "$crates_io" "$target"
        if [ -f displace-trigger ] && [ "$(cat displace-trigger)" = "$name" ]; then
          rewrite 'pkg000' "$crates_io" '1.2.0'
          rm displace-trigger
        fi
        exit 0
        ;;
    esac
    exit 102
"#};

struct SeedFixture {
    directory: tempfile::TempDir,
    project: Project,
    tool: CargoTool,
    changes: Vec<Change>,
}

impl SeedFixture {
    fn new(count: usize) -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n")?;
        let changes: Vec<_> = (0..count)
            .map(|index| Change {
                direct: false,
                ..change(&format!("pkg{index:03}"), "1.0.0", "1.1.0", false)
            })
            .collect();
        let mut content = String::from("version = 4\n\n");
        for planned in &changes {
            let name = &planned.package.name;
            content.push_str(&formatdoc! {r#"
                [[package]]
                name = "{name}"
                version = "1.0.0"
                source = "registry+https://github.com/rust-lang/crates.io-index"

            "#});
        }
        std::fs::write(root.join("Cargo.lock"), &content)?;
        std::fs::write(root.join("original.lock"), content)?;
        let script = root.join("fake-cargo");
        std::fs::write(&script, SCRIPT)?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let mut tool = CargoTool::from_http(SharedHttp::new(
            root.join("cache"),
            cooldown_registry::HttpOptions::default(),
        )?);
        tool.cargo = Cargo::with_bin(script.as_str());
        let project = Project {
            root: root.clone(),
            kind: CARGO_ID,
            manifest: root.join("Cargo.toml"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        Ok(Self {
            directory,
            project,
            tool,
            changes,
        })
    }

    async fn journal(&self) -> Result<ProjectMutationJournal> {
        self.tool
            .mutation_journal(
                &self.project,
                &Plan {
                    changes: self.changes.clone(),
                    ..Default::default()
                },
            )
            .await
    }

    fn invocations(&self) -> eyre::Result<Vec<Vec<String>>> {
        let path = self.directory.path().join("invocations");
        if !path.exists() {
            return Ok(Vec::new());
        }
        std::fs::read_to_string(path)?
            .lines()
            .map(|line| Ok(line.split_whitespace().map(str::to_string).collect()))
            .collect()
    }

    fn report(&self, rejections: &PinRejections) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        classify_planned_changes(
            &Plan {
                changes: self.changes.clone(),
                ..Default::default()
            },
            &read_lock(&self.project)?.crates_io_locked_versions(),
            None,
            rejections,
            &mut report,
        );
        Ok(report)
    }
}

#[tokio::test]
async fn two_hundred_independent_targets_use_at_most_three_resolves() -> eyre::Result<()> {
    let fixture = SeedFixture::new(200)?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await?;
    assert!(rejections.is_empty());
    let invocations = fixture.invocations()?;
    assert!(invocations.len() <= 3, "{} subprocesses", invocations.len());
    assert!(invocations.iter().all(|arguments| {
        arguments
            .first()
            .is_some_and(|command| command == "metadata")
    }));
    let seeded_versions = read_lock(&fixture.project)?.crates_io_locked_versions();
    assert_eq!(seeded_versions.len(), 200);
    assert!(seeded_versions.values().all(|version| version == "1.1.0"));
    let seeded_report = fixture.report(&rejections)?;

    // The precise-pin path is the reporting oracle, with the same original lock and targets.
    journal.restore()?;
    let mut stats = PinBatchStats::default();
    let worklist: Vec<_> = fixture.changes.iter().collect();
    fixture
        .tool
        .pin_individually(
            PinPass {
                scope: PinScope {
                    project: &fixture.project,
                    plan: &fixture.changes,
                    journal: &journal,
                    observer: None,
                },
                batch: &worklist,
            },
            &worklist,
            &mut rejections,
            &mut stats,
        )
        .await?;
    let precise_report = fixture.report(&rejections)?;
    assert_eq!(
        read_lock(&fixture.project)?.crates_io_locked_versions(),
        seeded_versions
    );
    assert_eq!(seeded_report.applied, precise_report.applied);
    assert!(seeded_report.skipped.is_empty() && precise_report.skipped.is_empty());
    assert_eq!(seeded_report.applied, fixture.changes);
    Ok(())
}

#[tokio::test]
async fn one_incompatible_target_bisects_without_linear_precise_pins() -> eyre::Result<()> {
    let fixture = SeedFixture::new(200)?;
    std::fs::write(fixture.project.root.join("bad-name"), "pkg137")?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await?;
    let invocations = fixture.invocations()?;
    assert!(
        invocations.len() <= 22,
        "{} subprocesses",
        invocations.len()
    );
    let precise: Vec<_> = invocations
        .iter()
        .filter(|arguments| arguments.first().is_some_and(|command| command == "update"))
        .collect();
    assert!((1..=2).contains(&precise.len()), "{precise:?}");
    assert!(precise.iter().all(|arguments| {
        arguments
            .iter()
            .any(|argument| argument.contains("pkg137@"))
    }));
    assert_eq!(rejections.len(), 1);
    let rejection = rejections
        .values()
        .next()
        .expect("one per-crate explanation");
    assert!(
        rejection.contains("pkg137") && rejection.contains("holder"),
        "{rejection}"
    );
    assert!(!rejection.contains("joint seed"), "{rejection}");
    let report = fixture.report(&rejections)?;
    assert_eq!(report.applied.len(), 199);
    assert_eq!(report.skipped.len(), 1);
    let versions = read_lock(&fixture.project)?.crates_io_locked_versions();
    for planned in &fixture.changes {
        assert_eq!(
            versions
                .get(&(planned.package.name.clone(), "1".to_string()))
                .map(String::as_str),
            Some(if planned.package.name == "pkg137" {
                "1.0.0"
            } else {
                "1.1.0"
            })
        );
    }
    Ok(())
}

/// A seed in which one target drifts within its line names that target, so it is pinned alone and
/// the rest seed again at once, instead of halving the group until the target is alone.
#[tokio::test]
async fn a_drifting_seed_target_is_split_out_without_bisection() -> eyre::Result<()> {
    let fixture = SeedFixture::new(200)?;
    std::fs::write(fixture.project.root.join("drift-name"), "pkg137")?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await?;
    let invocations = fixture.invocations()?;
    // The drifted seed, the drifting target's own refused pin, and the seed of the other 199.
    let resolves = invocations
        .iter()
        .filter(|arguments| {
            arguments
                .first()
                .is_some_and(|command| command == "metadata")
        })
        .count();
    assert_eq!(resolves, 2, "{invocations:?}");
    let precise: Vec<_> = invocations
        .iter()
        .filter(|arguments| arguments.first().is_some_and(|command| command == "update"))
        .collect();
    assert!(
        !precise.is_empty()
            && precise.iter().all(|arguments| {
                arguments
                    .iter()
                    .any(|argument| argument.contains("pkg137@"))
            }),
        "{precise:?}"
    );
    // Every other target lands, and the drifting one keeps its original version.
    let report = fixture.report(&rejections)?;
    assert_eq!(report.applied.len(), 199);
    assert_eq!(report.skipped.len(), 1);
    let versions = read_lock(&fixture.project)?.crates_io_locked_versions();
    assert_eq!(
        versions
            .get(&("pkg137".to_string(), "1".to_string()))
            .map(String::as_str),
        Some("1.0.0")
    );
    Ok(())
}

/// A target refused on its own stays out of the seeds of every later batch in the run, so a
/// planner that hands the same candidates over again does not bisect down to it a second time.
#[tokio::test]
async fn a_target_refused_alone_skips_later_seeds() -> eyre::Result<()> {
    let fixture = SeedFixture::new(200)?;
    std::fs::write(fixture.project.root.join("bad-name"), "pkg137")?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await?;

    // Replay the same batch from the original lock, as a later trial of the run would.
    std::fs::copy(
        fixture.project.root.join("original.lock"),
        fixture.project.root.join("Cargo.lock"),
    )?;
    std::fs::remove_file(fixture.directory.path().join("invocations"))?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await?;

    // One seed carries the other 199 without bisecting; the remembered target gets at most its
    // precise pins, and none while the conflict that refused it in the first batch still stands.
    let invocations = fixture.invocations()?;
    let (precise, seeds): (Vec<_>, Vec<_>) = invocations
        .iter()
        .partition(|arguments| arguments.first().is_some_and(|command| command == "update"));
    assert_eq!(seeds.len(), 1, "{invocations:?}");
    assert!(
        precise.len() <= 2
            && precise.iter().all(|arguments| arguments
                .iter()
                .any(|argument| argument.contains("pkg137@"))),
        "{invocations:?}"
    );
    let report = fixture.report(&rejections)?;
    assert_eq!(report.applied.len(), 199);
    assert_eq!(report.skipped.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_seed_uses_only_the_still_unlanded_source_nodes() -> eyre::Result<()> {
    let fixture = SeedFixture::new(200)?;
    let original = std::fs::read_to_string(fixture.project.root.join("Cargo.lock"))?;
    let moved = rewrite_planned_nodes(
        &original,
        &[PlannedNodeMove {
            name: "pkg000".to_string(),
            from: "1.0.0".to_string(),
            to: "1.1.0".to_string(),
        }],
    )
    .expect("first source node exists");
    std::fs::write(fixture.project.root.join("Cargo.lock"), moved)?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    let observer = CallbackRecorder::default();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            Some(&observer),
            &mut rejections,
        )
        .await?;
    assert!(fixture.invocations()?.len() <= 3);
    let callbacks = observer.changes.lock().expect("callback recorder lock");
    assert_eq!(callbacks.len(), fixture.invocations()?.len());
    assert!(
        callbacks
            .iter()
            .all(|change| change.package.name != "pkg000")
    );
    assert_eq!(fixture.report(&rejections)?.applied.len(), 200);
    assert!(
        read_lock(&fixture.project)?
            .crates_io_locked_versions()
            .values()
            .all(|version| version == "1.1.0")
    );
    Ok(())
}

#[tokio::test]
async fn an_unseeded_dependency_move_is_kept_and_reported() -> eyre::Result<()> {
    let fixture = SeedFixture::new(8)?;
    let path = fixture.project.root.join("Cargo.lock");
    let mut original = std::fs::read_to_string(&path)?;
    original.push_str(&formatdoc! {r#"
        [[package]]
        name = "rogue"
        version = "1.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
    "#});
    std::fs::write(&path, &original)?;
    std::fs::write(fixture.project.root.join("driveby-name"), "rogue")?;
    std::fs::write(
        fixture.project.root.join("driveby-source"),
        crate::lockfile::CRATES_IO_SOURCE,
    )?;
    std::fs::write(fixture.project.root.join("driveby-target"), "1.1.0")?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await?;
    assert_eq!(fixture.invocations()?.len(), 1);
    let after = read_lock(&fixture.project)?;
    let mut report = fixture.report(&rejections)?;
    add_collateral_changes(
        Some(&CargoLock::parse(&original)?),
        &after,
        &fixture.project.generated_members,
        &mut report,
    );
    assert_eq!(report.applied.len(), 9);
    assert!(
        report
            .applied
            .iter()
            .any(|change| change.package.name == "rogue" && change.to.as_str() == "1.1.0")
    );
    Ok(())
}

#[tokio::test]
async fn a_seeded_third_version_restores_exact_bytes_before_fallback() -> eyre::Result<()> {
    // Two changes, because a lone change goes straight to its precise pin without a seed.
    let fixture = SeedFixture::new(2)?;
    std::fs::write(fixture.project.root.join("driveby-name"), "pkg000")?;
    std::fs::write(
        fixture.project.root.join("driveby-source"),
        crate::lockfile::CRATES_IO_SOURCE,
    )?;
    std::fs::write(fixture.project.root.join("driveby-target"), "1.2.0")?;
    std::fs::write(fixture.project.root.join("require-restored"), "")?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await?;
    // The rejected seed is restored byte for byte, then each half lands through its precise pin.
    assert!(fixture.project.root.join("restore-verified").exists());
    assert_eq!(fixture.invocations()?.len(), 3);
    let lock = read_lock(&fixture.project)?;
    assert!(lock.has_crates_io_package("pkg000", "1.1.0"));
    assert!(lock.has_crates_io_package("pkg001", "1.1.0"));
    Ok(())
}

#[tokio::test]
async fn a_rejected_seed_with_changed_topology_never_restores_through_a_symlink() -> eyre::Result<()>
{
    let fixture = SeedFixture::new(2)?;
    std::fs::write(fixture.project.root.join("replace-lock"), "")?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    let error = fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await
        .expect_err("coordination failure must propagate");
    assert!(matches!(error, CoreError::LockConflict(_)));
    assert_eq!(fixture.invocations()?.len(), 1);
    let foreign = CargoLock::parse(&std::fs::read_to_string(
        fixture.project.root.join("foreign.lock"),
    )?)?;
    assert!(foreign.has_crates_io_package("pkg000", "1.1.0"));
    assert!(
        std::fs::symlink_metadata(fixture.project.root.join("Cargo.lock"))?
            .file_type()
            .is_symlink()
    );
    Ok(())
}

#[tokio::test]
async fn interrupted_seed_propagates_without_bisection_or_precise_fallback() -> eyre::Result<()> {
    let fixture = SeedFixture::new(200)?;
    let original = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
    std::fs::write(fixture.project.root.join("interrupt"), "")?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    let error = fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await
        .expect_err("signal interruption must propagate");
    assert!(matches!(
        error,
        CoreError::Tool {
            termination: cooldown_core::ToolTermination::Signal(15),
            ..
        }
    ));
    assert_eq!(fixture.invocations()?.len(), 1);
    assert_eq!(
        std::fs::read(fixture.project.root.join("Cargo.lock"))?,
        original
    );
    assert!(rejections.is_empty());
    Ok(())
}

#[tokio::test]
async fn missing_seed_binary_propagates_without_bisection() -> eyre::Result<()> {
    let mut fixture = SeedFixture::new(200)?;
    fixture.tool.cargo = Cargo::with_bin(fixture.project.root.join("missing-cargo").as_str());
    let original = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    let error = fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await
        .expect_err("spawn failure must propagate");
    assert!(matches!(error, CoreError::ToolSpawn { .. }));
    assert_eq!(fixture.invocations()?, [] as [Vec<String>; 0]);
    assert_eq!(
        std::fs::read(fixture.project.root.join("Cargo.lock"))?,
        original
    );
    assert!(rejections.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_partial_seed_pins_only_its_leftover_from_node() -> eyre::Result<()> {
    let fixture = SeedFixture::new(200)?;
    std::fs::write(fixture.project.root.join("leave-name"), "pkg137")?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    let observer = CallbackRecorder::default();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            Some(&observer),
            &mut rejections,
        )
        .await?;
    let invocations = fixture.invocations()?;
    assert_eq!(invocations.len(), 2, "{invocations:?}");
    let callbacks = observer.changes.lock().expect("callback recorder lock");
    assert_eq!(callbacks.len(), invocations.len());
    assert_eq!(
        callbacks.last().map(|change| change.package.name.as_str()),
        Some("pkg137")
    );
    assert_eq!(
        invocations
            .first()
            .and_then(|arguments| arguments.first())
            .map(String::as_str),
        Some("metadata")
    );
    let precise = invocations.last().expect("leftover precise pin");
    assert_eq!(precise.first().map(String::as_str), Some("update"));
    assert!(
        precise
            .iter()
            .any(|argument| argument.ends_with("#pkg137@1.0.0")),
        "{precise:?}"
    );
    assert!(rejections.is_empty());
    assert_eq!(fixture.report(&rejections)?.applied.len(), 200);
    assert!(
        read_lock(&fixture.project)?
            .crates_io_locked_versions()
            .values()
            .all(|version| version == "1.1.0")
    );
    Ok(())
}

#[derive(Default)]
struct CallbackRecorder {
    operations: std::sync::atomic::AtomicUsize,
    changes: std::sync::Mutex<Vec<Change>>,
}

impl ApplyObserver for CallbackRecorder {
    fn resolver_started(&self, change: Option<&Change>) {
        self.operations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(change) = change {
            self.candidate_started(change);
        }
    }

    fn candidate_started(&self, change: &Change) {
        self.changes
            .lock()
            .expect("callback recorder lock")
            .push(change.clone());
    }
}

#[tokio::test]
async fn seeded_batch_notifies_once_per_native_invocation() -> eyre::Result<()> {
    let mut fixture = SeedFixture::new(4)?;
    for member in ["app-a", "app-b"] {
        let original = fixture.changes.first().expect("first planned package");
        fixture.changes.push(Change {
            members: vec![MemberRef {
                name: member.to_string(),
                path: member.to_string(),
            }],
            ..original.clone()
        });
    }
    let journal = fixture.journal().await?;
    let observer = CallbackRecorder::default();
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            Some(&observer),
            &mut rejections,
        )
        .await?;
    let callbacks = observer.changes.lock().expect("callback recorder lock");
    assert_eq!(callbacks.len(), fixture.invocations()?.len());
    assert_eq!(callbacks.len(), 1);
    assert!(fixture.invocations()?.len() <= 3);
    assert_eq!(fixture.report(&rejections)?.applied.len(), 6);
    Ok(())
}

#[tokio::test]
async fn seed_filesystem_failure_propagates_without_bisection() -> eyre::Result<()> {
    let fixture = SeedFixture::new(200)?;
    let original = std::fs::read(fixture.project.root.join("Cargo.lock"))?;
    std::fs::write(fixture.project.root.join("permission-failure"), "")?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    let error = fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await
        .expect_err("filesystem failure must propagate");
    assert!(error.is_local_environment_failure(), "{error:?}");
    assert_eq!(fixture.invocations()?.len(), 1);
    assert_eq!(
        std::fs::read(fixture.project.root.join("Cargo.lock"))?,
        original
    );
    assert!(rejections.is_empty());
    Ok(())
}

/// An existing target copy does not prove the original source copy has left the lock.
#[tokio::test]
async fn an_existing_from_and_target_converge_through_one_precise_pin() -> eyre::Result<()> {
    let fixture = SeedFixture::new(1)?;
    let mut original = std::fs::read_to_string(fixture.project.root.join("Cargo.lock"))?;
    original.push_str(&formatdoc! {r#"
        [[package]]
        name = "pkg000"
        version = "1.1.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
    "#});
    std::fs::write(fixture.project.root.join("Cargo.lock"), original)?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await?;
    let invocations = fixture.invocations()?;
    assert_eq!(invocations.len(), 1, "{invocations:?}");
    let precise = invocations.first().expect("one precise convergence pin");
    assert_eq!(precise.first().map(String::as_str), Some("update"));
    assert!(
        precise
            .iter()
            .any(|argument| argument.ends_with("#pkg000@1.0.0")),
        "{precise:?}"
    );
    let settled = read_lock(&fixture.project)?;
    assert_eq!(settled.package.len(), 1);
    assert!(settled.has_crates_io_package("pkg000", "1.1.0"));
    assert!(!settled.has_crates_io_package("pkg000", "1.0.0"));
    assert!(rejections.is_empty());
    Ok(())
}

/// Whether any recorded `cargo` invocation re-pinned `pkg000` from a displaced 1.2.0.
fn repinned_displaced_seed(invocations: &[Vec<String>]) -> bool {
    invocations.iter().any(|arguments| {
        arguments
            .iter()
            .any(|argument| argument.ends_with("#pkg000@1.2.0"))
    })
}

/// A leftover pin that would displace an earlier seed's reached target is refused and restored
/// instead of being kept for a later pass to repair; the leftover lands once its pin no longer
/// moves the seeded crate.
#[tokio::test]
async fn a_leftover_pin_that_displaces_a_seeded_target_is_refused() -> eyre::Result<()> {
    let fixture = SeedFixture::new(2)?;
    std::fs::write(fixture.project.root.join("leave-name"), "pkg001")?;
    std::fs::write(fixture.project.root.join("displace-trigger"), "pkg001")?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await?;

    // The displaced lock was never kept, so nothing had to pin pkg000 back from 1.2.0.
    let invocations = fixture.invocations()?;
    assert!(!repinned_displaced_seed(&invocations), "{invocations:?}");
    // The refusal names the target the pin would have displaced.
    let detail = rejections
        .get(&("pkg001".to_owned(), "1.0.0".to_owned(), "1.1.0".to_owned()))
        .ok_or_else(|| eyre::eyre!("the refused leftover records why: {rejections:?}"))?;
    assert!(detail.contains("pkg000 off its target 1.1.0"), "{detail}");
    // The script displaces only once, so the fixed point's next pass lands the leftover cleanly.
    let settled = read_lock(&fixture.project)?;
    assert!(settled.has_crates_io_package("pkg000", "1.1.0"));
    assert!(settled.has_crates_io_package("pkg001", "1.1.0"));
    assert!(!settled.has_crates_io_package("pkg000", "1.2.0"));
    assert_eq!(fixture.report(&rejections)?.applied.len(), 2);
    Ok(())
}

/// A later bisected group's leftover cannot leave an earlier accepted group off its target: its
/// displacing pin is refused, and the earlier seed keeps its landing.
#[tokio::test]
async fn a_bisected_right_group_cannot_displace_an_earlier_seed() -> eyre::Result<()> {
    let fixture = SeedFixture::new(8)?;
    std::fs::write(fixture.project.root.join("bad-name"), "pkg007")?;
    std::fs::write(fixture.project.root.join("leave-name"), "pkg006")?;
    std::fs::write(fixture.project.root.join("displace-trigger"), "pkg006")?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await?;

    let invocations = fixture.invocations()?;
    assert!(!repinned_displaced_seed(&invocations), "{invocations:?}");
    // Every target but the resolver-refused pkg007 landed, the displaced pkg000 included.
    let settled = read_lock(&fixture.project)?;
    for planned in &fixture.changes {
        assert!(
            settled.has_crates_io_package(
                &planned.package.name,
                if planned.package.name == "pkg007" {
                    "1.0.0"
                } else {
                    "1.1.0"
                }
            ),
            "{planned:?}"
        );
    }
    assert!(!settled.has_crates_io_package("pkg000", "1.2.0"));
    assert!(
        rejections.contains_key(&("pkg007".to_owned(), "1.0.0".to_owned(), "1.1.0".to_owned())),
        "{rejections:?}"
    );
    assert_eq!(fixture.report(&rejections)?.applied.len(), 7);
    Ok(())
}

#[tokio::test]
async fn held_child_projection_stays_original_beside_an_independent_landing() -> eyre::Result<()> {
    let mut fixture = SeedFixture::new(2)?;
    let root = &fixture.project.root;
    let mut graph = hakari_workspace(root);
    graph.generated_roots.insert("hack".to_string());
    let follower = root.join("crates/hack/Cargo.toml");
    let original = indoc! {r#"
        [package]
        name = "hack"
        ### BEGIN HAKARI SECTION
        [dependencies]
        pkg000 = "=1.0.0"
        pkg001 = "=1.0.0"
        ### END HAKARI SECTION
    "#};
    std::fs::write(&follower, original)?;
    fixture.project.generated_members =
        cooldown_core::GeneratedMembers::declared(vec!["hack".to_string()]);
    fixture
        .tool
        .remember_generated_facts(&fixture.project, &graph)
        .await?;
    std::fs::write(root.join("leave-name"), "pkg001")?;
    let journal = fixture.journal().await?;
    // Install the rejection only after the seed has landed its independent sibling.
    let script = SCRIPT.replace(
        "if [ -f leave-name ]; then rewrite",
        "if [ -f leave-name ]; then echo pkg001 > bad-name; rewrite",
    );
    let metadata = serde_json::json!({"packages": [{"id": "hack", "name": "hack", "version": "0.1.0", "manifest_path": root.join("crates/hack/Cargo.toml")}], "workspace_members": ["hack"], "workspace_root": root, "resolve": null});
    let script = script.replace("echo '{\"packages\": [], \"workspace_members\": [], \"workspace_root\": \"\", \"resolve\": null}'", &format!("echo '{metadata}'"));
    std::fs::write(root.join("fake-cargo"), script)?;
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await?;
    let manifest = std::fs::read_to_string(follower)?;
    assert!(manifest.contains("pkg000 = \"=1.1.0\""), "{manifest}");
    assert!(manifest.contains("pkg001 = \"=1.0.0\""), "{manifest}");
    assert_eq!(fixture.report(&rejections)?.applied.len(), 1);
    Ok(())
}

#[tokio::test]
async fn full_apply_counts_unattributed_resolves_without_naming_landed_targets() -> eyre::Result<()>
{
    let mut fixture = SeedFixture::new(2)?;
    // A direct-member reach check needs metadata after the initial seed already landed.
    let first = fixture
        .changes
        .first_mut()
        .ok_or_else(|| eyre::eyre!("first target"))?;
    first.direct = true;
    first.members = vec![MemberRef {
        name: "app".to_string(),
        path: ".".to_string(),
    }];
    let plan = Plan {
        changes: fixture.changes.clone(),
        rewrite: RewriteMode::Auto,
        ..Plan::default()
    };
    let journal = fixture.journal().await?;
    let observer = CallbackRecorder::default();
    fixture
        .tool
        .apply_plan(&fixture.project, &plan, &journal, Some(&observer))
        .await?;
    assert_eq!(
        observer
            .operations
            .load(std::sync::atomic::Ordering::Relaxed),
        fixture.invocations()?.len()
    );
    assert!(
        observer
            .operations
            .load(std::sync::atomic::Ordering::Relaxed)
            > observer
                .changes
                .lock()
                .expect("callback recorder lock")
                .len()
    );
    Ok(())
}

/// Joint widens retry contextual refusals together without routing them to precise pins.
#[tokio::test]
async fn previously_refused_candidates_land_in_one_joint_widen_seed() -> eyre::Result<()> {
    let fixture = SeedFixture::new(2)?;
    let root = &fixture.project.root;
    std::fs::write(
        root.join("Cargo.toml"),
        indoc! {r#"
            [workspace]
            [workspace.dependencies]
            pkg000 = "=1.0.0"
            pkg001 = "=1.0.0"
        "#},
    )?;
    let reject_unwidened = indoc! {r"
        if grep -q '=1.0.0' Cargo.toml; then
          echo 'error: failed to select a version for the original manifest constraints' >&2
          exit 101
        fi
    "};
    let script = SCRIPT
        .replace("metadata)", &format!("metadata)\n{reject_unwidened}"))
        .replace("update)", &format!("update)\n{reject_unwidened}"));
    std::fs::write(root.join("fake-cargo"), script)?;
    let journal = fixture.journal().await?;
    let mut rejections = PinRejections::new();
    fixture
        .tool
        .pin_batch(
            &fixture.project,
            &fixture.changes,
            &journal,
            None,
            &mut rejections,
        )
        .await?;
    assert_eq!(fixture.invocations()?.len(), 3);
    assert!(
        fixture
            .changes
            .iter()
            .all(|planned| fixture.tool.was_refused_alone(planned))
    );
    assert_eq!(rejections.len(), 2);
    std::fs::remove_file(root.join("invocations"))?;

    let observer = CallbackRecorder::default();
    fixture
        .tool
        .whole_graph_resolve(
            &fixture.project,
            &Plan {
                changes: fixture.changes.clone(),
                rewrite: RewriteMode::Auto,
                ..Plan::default()
            },
            &journal,
            Some(&observer),
        )
        .await?;
    let invocations = fixture.invocations()?;
    // The initial batch reuses its two rejections; one joint metadata resolve lands both.
    assert_eq!(invocations.len(), 1, "{invocations:?}");
    assert_eq!(
        invocations
            .iter()
            .filter(|arguments| arguments
                .first()
                .is_some_and(|command| command == "metadata"))
            .count(),
        1
    );
    assert_eq!(
        observer
            .operations
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    let lock = read_lock(&fixture.project)?;
    for planned in &fixture.changes {
        assert!(lock.has_crates_io_package(&planned.package.name, "1.1.0"));
    }
    Ok(())
}

/// The plan of a lower-target fallback: every change of `changes` except `refused`, which moves
/// to `alternative` instead.
fn fallback_plan(changes: &[Change], refused: &str, alternative: &str) -> Plan {
    Plan {
        changes: changes
            .iter()
            .map(|change| {
                if change.package.name == refused {
                    change_with_target(change, alternative)
                } else {
                    change.clone()
                }
            })
            .collect(),
        ..Default::default()
    }
}

fn change_with_target(change: &Change, target: &str) -> Change {
    Change {
        to: Version::new(target),
        ..change.clone()
    }
}

/// A later apply from the same inputs whose plan keeps every change an earlier apply landed starts
/// from that landing, so only the change the earlier apply could not land is resolved again.
#[tokio::test]
async fn a_fallback_apply_warm_starts_from_the_earlier_landing() -> eyre::Result<()> {
    let fixture = SeedFixture::new(20)?;
    std::fs::write(fixture.project.root.join("bad-name"), "pkg013")?;
    let journal = fixture.journal().await?;
    let plan = Plan {
        changes: fixture.changes.clone(),
        ..Default::default()
    };
    let rejections = fixture
        .tool
        .whole_graph_resolve(&fixture.project, &plan, &journal, None)
        .await?;
    assert_eq!(rejections.len(), 1);

    // The executor restores the baseline and retries the refused crate at a lower target.
    journal.restore()?;
    std::fs::remove_file(fixture.project.root.join("bad-name"))?;
    std::fs::remove_file(fixture.project.root.join("invocations"))?;
    let rejections = fixture
        .tool
        .whole_graph_resolve(
            &fixture.project,
            &fallback_plan(&fixture.changes, "pkg013", "1.0.5"),
            &journal,
            None,
        )
        .await?;
    assert!(rejections.is_empty(), "{rejections:?}");
    // Only the alternative is pinned; the nineteen earlier landings are not resolved again.
    let invocations = fixture.invocations()?;
    assert_eq!(invocations.len(), 1, "{invocations:?}");
    assert!(
        invocations
            .iter()
            .flatten()
            .any(|argument| argument.contains("pkg013@1.0.0")),
        "{invocations:?}"
    );
    let versions = read_lock(&fixture.project)?.crates_io_locked_versions();
    for planned in &fixture.changes {
        let expected = if planned.package.name == "pkg013" {
            "1.0.5"
        } else {
            "1.1.0"
        };
        assert_eq!(
            versions
                .get(&(planned.package.name.clone(), "1".to_string()))
                .map(String::as_str),
            Some(expected),
            "{}",
            planned.package.name
        );
    }
    Ok(())
}

/// A plan that no longer carries a change the earlier apply landed starts cold, so the dropped
/// change never reaches the lock through the earlier landing.
#[tokio::test]
async fn a_plan_dropping_a_landed_change_does_not_warm_start() -> eyre::Result<()> {
    let fixture = SeedFixture::new(20)?;
    let journal = fixture.journal().await?;
    let plan = Plan {
        changes: fixture.changes.clone(),
        ..Default::default()
    };
    fixture
        .tool
        .whole_graph_resolve(&fixture.project, &plan, &journal, None)
        .await?;

    journal.restore()?;
    let without_pkg007 = Plan {
        changes: fixture
            .changes
            .iter()
            .filter(|change| change.package.name != "pkg007")
            .cloned()
            .collect(),
        ..Default::default()
    };
    fixture
        .tool
        .whole_graph_resolve(&fixture.project, &without_pkg007, &journal, None)
        .await?;
    let versions = read_lock(&fixture.project)?.crates_io_locked_versions();
    assert_eq!(
        versions
            .get(&("pkg007".to_string(), "1".to_string()))
            .map(String::as_str),
        Some("1.0.0")
    );
    assert_eq!(
        versions
            .get(&("pkg008".to_string(), "1".to_string()))
            .map(String::as_str),
        Some("1.1.0")
    );
    Ok(())
}

/// A fallback apply that lands nothing beyond its warm start attests the holds it would meet again
/// beside exactly what it applied, but not one whose refusal named another planned candidate.
#[tokio::test]
async fn an_unmoved_fallback_apply_attests_its_independent_holds() -> eyre::Result<()> {
    let fixture = SeedFixture::new(20)?;
    let root = &fixture.project.root;
    // `pkg013`'s pin is refused over the planned `pkg014`, and `pkg014`'s over its own requirement.
    std::fs::write(root.join("refuse-pkg013"), "pkg014")?;
    std::fs::write(root.join("refuse-pkg014"), "")?;
    let journal = fixture.journal().await?;
    let plan = Plan {
        changes: fixture.changes.clone(),
        ..Plan::default()
    };
    let cold = fixture
        .tool
        .apply_plan(&fixture.project, &plan, &journal, None)
        .await?;
    // A cold apply's refusals were met in states a narrower plan does not start from.
    assert_eq!(cold.independent_holds, []);
    assert_eq!(cold.skipped.len(), 2);

    journal.restore()?;
    let fallback = Plan {
        changes: fallback_plan(
            &fallback_plan(&fixture.changes, "pkg013", "1.0.5").changes,
            "pkg014",
            "1.0.5",
        )
        .changes,
        ..Plan::default()
    };
    let warm = fixture
        .tool
        .apply_plan(&fixture.project, &fallback, &journal, None)
        .await?;
    let attested: Vec<_> = warm
        .independent_holds
        .iter()
        .map(|change| (change.package.name.as_str(), change.to.as_str()))
        .collect();
    assert_eq!(attested, [("pkg014", "1.0.5")]);

    // The attested hold is met again in exactly the narrower plan it was attested for.
    journal.restore()?;
    let narrowed = Plan {
        changes: warm
            .applied
            .iter()
            .filter(|change| fallback.changes.contains(change))
            .chain(&warm.independent_holds)
            .cloned()
            .collect(),
        ..Plan::default()
    };
    let alone = fixture
        .tool
        .apply_plan(&fixture.project, &narrowed, &journal, None)
        .await?;
    assert!(
        alone
            .skipped
            .iter()
            .any(|skipped| skipped.change.package.name == "pkg014")
    );
    Ok(())
}
