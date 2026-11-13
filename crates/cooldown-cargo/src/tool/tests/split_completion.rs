//! A step that splits a declared crate lands together with the planned moves that heal the split.

use super::*;
use std::os::unix::fs::PermissionsExt as _;

/// The fake `cargo`: the lock it writes depends only on `holder`'s version and on the major of
/// `app`'s declared `dep` requirement.
///
/// `holder` 1.1.0 requires `dep` 2, so moving only one of `holder` and `app`'s requirement forks
/// `dep` into two lines, the way cargo keeps the old line for the requirer left behind.
/// A precise pin of `dep` 2.0.0 is refused while `app` still declares `dep = "1"`.
/// Every move of `fresh` is refused, and a seed carrying one fails without naming it.
/// With an `overshoot` file, a resolve unifying both requirers lands `dep` 2.1.0 instead.
const SCRIPT: &str = indoc! {r#"
    #!/bin/sh
    set -eu
    printf '%s\n' "$*" >> invocations
    if grep -q 'dep = "2' Cargo.toml; then app=2; else app=1; fi
    holder=$(awk '
      /^\[\[package\]\]/ { selected = 0 }
      $0 == "name = \"holder\"" { selected = 1 }
      selected && /^version = / { gsub(/version = |"/, ""); print; exit }
    ' Cargo.lock)
    command="$1"
    case "$command" in
      metadata)
        if ! grep -A1 '^name = "fresh"$' Cargo.lock | grep -q '^version = "1.0.0"$'; then
          echo 'error: joint seed cannot resolve' >&2
          exit 101
        fi
        ;;
      update)
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
        case "$name" in
          fresh)
            echo 'error: failed to select a version for the requirement `fresh = "^1.0"`' >&2
            exit 101
            ;;
          holder) holder="$target" ;;
          dep)
            if [ "$app" = 1 ]; then
              echo 'error: failed to select a version for the requirement `dep = "^1"`' >&2
              echo 'required by package `app v0.1.0 (/repo/app)`' >&2
              exit 101
            fi
            ;;
        esac
        ;;
      *) exit 102 ;;
    esac
    resolved="lock-$holder-$app"
    if [ "$resolved" = lock-1.1.0-2 ] && [ -f overshoot ]; then resolved=overshot.lock; fi
    cp "$resolved" Cargo.lock
    if [ "$command" = metadata ]; then
      echo '{"packages": [], "workspace_members": [], "workspace_root": "", "resolve": null}'
    fi
"#};

const MANIFEST: &str = indoc! {r#"
    [package]
    name = "app"
    version = "0.1.0"
    edition = "2024"

    [dependencies]
    dep = "1"
    fresh = "1"
    holder = "1"
"#};

/// The lock where `app` binds `app_dep` and `holder` at `holder` binds `holder_dep`.
fn lock(holder: &str, app_dep: &str, holder_dep: &str) -> String {
    let forked = app_dep != holder_dep;
    let binding = |version: &str| {
        if forked {
            format!("\"dep {version}\"")
        } else {
            "\"dep\"".to_string()
        }
    };
    let mut versions = vec![app_dep, holder_dep];
    versions.dedup();
    versions.sort_unstable();
    let mut content = formatdoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = [{app}, "fresh", "holder"]
    "#, app = binding(app_dep)};
    for version in versions {
        content.push_str(&formatdoc! {r#"

            [[package]]
            name = "dep"
            version = "{version}"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        "#});
    }
    content.push_str(&formatdoc! {r#"

        [[package]]
        name = "fresh"
        version = "1.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"

        [[package]]
        name = "holder"
        version = "{holder}"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        dependencies = [{bound}]
    "#, bound = binding(holder_dep)});
    content
}

struct SplitFixture {
    directory: tempfile::TempDir,
    project: Project,
    tool: CargoTool,
}

impl SplitFixture {
    fn new() -> eyre::Result<Self> {
        Self::with_files(
            SCRIPT,
            &[
                ("Cargo.toml".to_owned(), MANIFEST.to_owned()),
                ("Cargo.lock".to_owned(), lock("1.0.0", "1.0.0", "1.0.0")),
                ("lock-1.0.0-1".to_owned(), lock("1.0.0", "1.0.0", "1.0.0")),
                ("lock-1.1.0-1".to_owned(), lock("1.1.0", "1.0.0", "2.0.0")),
                ("lock-1.0.0-2".to_owned(), lock("1.0.0", "2.0.0", "1.0.0")),
                ("lock-1.1.0-2".to_owned(), lock("1.1.0", "2.0.0", "2.0.0")),
                ("overshot.lock".to_owned(), lock("1.1.0", "2.1.0", "2.1.0")),
            ],
        )
    }

    /// A project of `files` resolved by the fake `cargo` `script`.
    fn with_files(script: &str, files: &[(String, String)]) -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        for (name, contents) in files {
            std::fs::write(root.join(name), contents)?;
        }
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

    fn plan(changes: Vec<Change>, rewrite: RewriteMode) -> Plan {
        Plan {
            changes,
            rewrite,
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

    fn file(&self, name: &str) -> eyre::Result<String> {
        Ok(std::fs::read_to_string(self.project.root.join(name))?)
    }

    fn invocations(&self) -> eyre::Result<usize> {
        Ok(self.file("invocations")?.lines().count())
    }
}

fn holder_move() -> Change {
    change("holder", "1.0.0", "1.1.0", false)
}

fn dep_move() -> Change {
    change("dep", "1.0.0", "2.0.0", false)
}

/// A dependency move that switches `holder` to `dep`'s next line lands together with the planned
/// move of `app`'s own `dep` declaration.
///
/// Either move alone forks `dep` into two lines, which the acceptance rule refuses step by step,
/// so the plan lands only when one step moves both.
#[tokio::test]
async fn a_split_lands_beside_the_planned_moves_that_heal_it() -> eyre::Result<()> {
    for rewrite in [RewriteMode::Auto, RewriteMode::Always] {
        let fixture = SplitFixture::new()?;
        let plan = SplitFixture::plan(vec![dep_move(), holder_move()], rewrite);

        fixture.resolve(&plan).await?;

        // Both moves landed on one unified `dep` 2 line, with `app`'s declaration widened.
        assert_eq!(
            fixture.file("Cargo.lock")?,
            lock("1.1.0", "2.0.0", "2.0.0"),
            "{rewrite:?}"
        );
        assert!(
            fixture.file("Cargo.toml")?.contains(r#"dep = "2"#),
            "{rewrite:?}"
        );
        // The coordinated landing takes a bounded number of resolver calls, not one per subset.
        let invocations = fixture.invocations()?;
        assert!(invocations <= 12, "{rewrite:?}: {invocations} cargo calls");
    }
    Ok(())
}

/// A coordinated landing survives an unrelated refused candidate in the same plan.
///
/// The refused `fresh` fails the joint seed without naming itself, so the seed is bisected, and
/// the halves that move only one of `dep` and `holder` split `dep` until the healing moves join
/// them.
#[tokio::test]
async fn a_split_lands_beside_an_unrelated_refusal() -> eyre::Result<()> {
    for rewrite in [RewriteMode::Auto, RewriteMode::Always] {
        let fixture = SplitFixture::new()?;
        let plan = SplitFixture::plan(
            vec![
                dep_move(),
                change("fresh", "1.0.0", "1.1.0", false),
                holder_move(),
            ],
            rewrite,
        );

        let rejections = fixture.resolve(&plan).await?;

        // `dep` and `holder` land on one `dep` 2 line while `fresh` stays held.
        assert_eq!(
            fixture.file("Cargo.lock")?,
            lock("1.1.0", "2.0.0", "2.0.0"),
            "{rewrite:?}"
        );
        assert!(
            rejections.contains_key(&(
                "fresh".to_string(),
                "1.0.0".to_string(),
                "1.1.0".to_string(),
            )),
            "{rewrite:?}: {rejections:?}"
        );
        let invocations = fixture.invocations()?;
        assert!(invocations <= 16, "{rewrite:?}: {invocations} cargo calls");
    }
    Ok(())
}

/// A split with no planned move to heal it stays refused, and its candidate is held with the
/// split it would cause while the project files stay byte-identical.
#[tokio::test]
async fn a_split_no_planned_move_heals_is_held() -> eyre::Result<()> {
    let fixture = SplitFixture::new()?;
    let plan = SplitFixture::plan(vec![holder_move()], RewriteMode::Auto);

    let rejections = fixture.resolve(&plan).await?;

    assert_eq!(fixture.file("Cargo.lock")?, lock("1.0.0", "1.0.0", "1.0.0"));
    assert_eq!(fixture.file("Cargo.toml")?, MANIFEST);
    let detail = rejections
        .get(&(
            "holder".to_string(),
            "1.0.0".to_string(),
            "1.1.0".to_string(),
        ))
        .ok_or_else(|| eyre::eyre!("the held candidate keeps its rejection"))?;
    assert!(
        detail.contains("would resolve direct dependency dep at another incompatible line"),
        "{detail}"
    );
    Ok(())
}

/// A split no planned move heals is remembered for the same files and plan, but a plan that adds
/// a healing move tries the step again and lands it.
#[tokio::test]
async fn a_remembered_split_refusal_stands_only_while_nothing_heals_it() -> eyre::Result<()> {
    let fixture = SplitFixture::new()?;
    let holder_pins = || -> eyre::Result<usize> {
        Ok(fixture
            .file("invocations")?
            .lines()
            .filter(|invocation| invocation.contains("#holder@1.0.0 --precise"))
            .count())
    };
    let alone = SplitFixture::plan(vec![holder_move()], RewriteMode::Auto);

    // The same files and plan recall the refusal instead of pinning again.
    for _ in 0..2 {
        let journal = fixture
            .tool
            .mutation_journal(&fixture.project, &alone)
            .await?;
        fixture
            .tool
            .whole_graph_resolve(&fixture.project, &alone, &journal, None)
            .await?;
        journal.restore()?;
    }
    assert_eq!(holder_pins()?, 1);

    // A plan carrying `dep`'s move can heal the split, so the refusal is not recalled.
    let healed = SplitFixture::plan(vec![dep_move(), holder_move()], RewriteMode::Auto);
    fixture.resolve(&healed).await?;
    assert_eq!(fixture.file("Cargo.lock")?, lock("1.1.0", "2.0.0", "2.0.0"));
    assert_eq!(holder_pins()?, 2);
    Ok(())
}

/// A healing step that overshoots a partner's exact target is restored instead of kept.
#[tokio::test]
async fn a_split_completion_missing_a_target_is_restored() -> eyre::Result<()> {
    let fixture = SplitFixture::new()?;
    std::fs::write(fixture.directory.path().join("overshoot"), "")?;
    let plan = SplitFixture::plan(vec![dep_move(), holder_move()], RewriteMode::Auto);

    let rejections = fixture.resolve(&plan).await?;

    // Neither move lands, so the project is exactly as before.
    assert_eq!(fixture.file("Cargo.lock")?, lock("1.0.0", "1.0.0", "1.0.0"));
    assert_eq!(fixture.file("Cargo.toml")?, MANIFEST);
    assert!(
        rejections.contains_key(&(
            "holder".to_string(),
            "1.0.0".to_string(),
            "1.1.0".to_string(),
        )),
        "{rejections:?}"
    );
    Ok(())
}

/// The fake `cargo` of a pin that needs a blocker released: `holder` 1.1.0 requires `glue`
/// `=0.9.1` and `dep` 2, while `pal` keeps `glue` locked at 0.9.3 within its own `^0.9`.
///
/// The lock it writes depends on `holder`'s and `glue`'s versions and on the major of `app`'s
/// declared `dep` requirement.
/// A plain pin of `holder` is refused for `glue`, and a resolve that keeps `glue` at 0.9.3 beside
/// `holder` 1.1.0 is refused too, so `holder` lands only with `glue` released by name.
const UNLOCK_SCRIPT: &str = indoc! {r#"
    #!/bin/sh
    set -eu
    printf '%s\n' "$*" >> invocations
    version_of() {
      awk -v name="$1" '
        /^\[\[package\]\]/ { selected = 0 }
        $0 == "name = \"" name "\"" { selected = 1 }
        selected && /^version = / { gsub(/version = |"/, ""); print; exit }
      ' Cargo.lock
    }
    if grep -q 'dep = "2' Cargo.toml; then app=2; else app=1; fi
    holder=$(version_of holder)
    glue=$(version_of glue)
    command="$1"
    case "$command" in
      metadata)
        if [ "$holder" = 1.1.0 ] && [ "$glue" = 0.9.3 ]; then
          cat refusal.txt >&2
          exit 101
        fi
        ;;
      update)
        specs=''
        target=''
        while [ "$#" -gt 0 ]; do
          case "$1" in
            -p) shift; name="${1##*#}"; specs="$specs ${name%@*}" ;;
            --precise) shift; target="$1" ;;
          esac
          shift
        done
        case "$specs" in
          ' dep')
            echo 'error: failed to select a version for the requirement `dep = "^1"`' >&2
            echo 'required by package `app v0.1.0 (/repo/app)`' >&2
            exit 101
            ;;
          ' holder')
            cat refusal.txt >&2
            exit 101
            ;;
          ' holder glue') holder="$target"; glue=0.9.1 ;;
          ' glue') if [ "$holder" = 1.1.0 ]; then glue=0.9.1; fi ;;
          *) exit 102 ;;
        esac
        ;;
      *) exit 102 ;;
    esac
    cp "lock-$holder-$glue-$app" Cargo.lock
    if [ "$command" = metadata ]; then
      echo '{"packages": [], "workspace_members": [], "workspace_root": "", "resolve": null}'
    fi
"#};

const UNLOCK_MANIFEST: &str = indoc! {r#"
    [package]
    name = "app"
    version = "0.1.0"
    edition = "2024"

    [dependencies]
    dep = "1"
    holder = "1"
    pal = "1"
"#};

/// Cargo's refusal of `holder` 1.1.0 while `pal` keeps `glue` 0.9.3 locked.
const GLUE_REFUSAL: &str = indoc! {r#"
    error: failed to select a version for `glue`.
        ... required by package `pal v1.0.0`
    versions that meet the requirements `^0.9` (locked to 0.9.3) are: 0.9.3

    all possible versions conflict with previously selected packages

      previously selected package `glue v0.9.1`
        ... which satisfies dependency `glue = "=0.9.1"` of package `holder v1.1.0`

    failed to select a version for `glue` which could resolve this conflict
"#};

/// The lock of [`UNLOCK_SCRIPT`]'s state: `holder` at `holder` binds `dep` 2 from 1.1.0 on,
/// `app` binds `dep` at `app_dep`, and `glue` is at `glue`.
fn unlock_lock(holder: &str, glue: &str, app_dep: &str) -> String {
    let holder_dep = if holder == "1.1.0" { "2.0.0" } else { "1.0.0" };
    let forked = app_dep != holder_dep;
    let binding = |version: &str| {
        if forked {
            format!("\"dep {version}\"")
        } else {
            "\"dep\"".to_owned()
        }
    };
    let mut versions = vec![app_dep, holder_dep];
    versions.sort_unstable();
    versions.dedup();
    let mut content = formatdoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = [{app}, "holder", "pal"]
    "#, app = binding(app_dep)};
    for version in versions {
        content.push_str(&formatdoc! {r#"

            [[package]]
            name = "dep"
            version = "{version}"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        "#});
    }
    content.push_str(&formatdoc! {r#"

        [[package]]
        name = "glue"
        version = "{glue}"
        source = "registry+https://github.com/rust-lang/crates.io-index"

        [[package]]
        name = "holder"
        version = "{holder}"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        dependencies = [{bound}, "glue"]

        [[package]]
        name = "pal"
        version = "1.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        dependencies = ["glue"]
    "#, bound = binding(holder_dep)});
    content
}

/// A pin that lands only with a blocker released, and whose landing then splits a declared
/// crate, completes together with the planned move that heals the split.
///
/// The completion has to release the same blocker, since a resolve keeps it locked otherwise.
#[tokio::test]
async fn a_split_found_while_unlocking_completes_with_the_released_blocker() -> eyre::Result<()> {
    let mut files = vec![
        ("Cargo.toml".to_owned(), UNLOCK_MANIFEST.to_owned()),
        (
            "Cargo.lock".to_owned(),
            unlock_lock("1.0.0", "0.9.3", "1.0.0"),
        ),
        ("refusal.txt".to_owned(), GLUE_REFUSAL.to_owned()),
    ];
    for holder in ["1.0.0", "1.1.0"] {
        for glue in ["0.9.1", "0.9.3"] {
            for (major, app_dep) in [("1", "1.0.0"), ("2", "2.0.0")] {
                files.push((
                    format!("lock-{holder}-{glue}-{major}"),
                    unlock_lock(holder, glue, app_dep),
                ));
            }
        }
    }
    let fixture = SplitFixture::with_files(UNLOCK_SCRIPT, &files)?;
    let plan = SplitFixture::plan(vec![dep_move(), holder_move()], RewriteMode::Auto);

    fixture.resolve(&plan).await?;

    // Both moves landed on one `dep` 2 line, with `glue` moved where `holder` needs it.
    assert_eq!(
        fixture.file("Cargo.lock")?,
        unlock_lock("1.1.0", "0.9.1", "2.0.0"),
        "{}",
        fixture.file("invocations")?
    );
    Ok(())
}

/// The fake `cargo` of five declared crates `d1` to `d5`, each also required by a holder `h1` to
/// `h5` whose 1.1.0 release moves to the crate's line 2.
///
/// The lock it writes depends on each holder's version and on the majors `app` declares, so
/// every crate whose holder and declaration disagree forks into two lines.
/// Every move of `fresh` is refused, and a seed carrying one fails without naming it.
/// With a `block` file, every resolve that moves `h5` fails the same way.
const CHAIN_SCRIPT: &str = indoc! {r#"
    #!/bin/sh
    set -eu
    printf '%s\n' "$*" >> invocations
    crates_io='registry+https://github.com/rust-lang/crates.io-index'
    version_of() {
      awk -v name="$1" '
        /^\[\[package\]\]/ { selected = 0 }
        $0 == "name = \"" name "\"" { selected = 1 }
        selected && /^version = / { gsub(/version = |"/, ""); print; exit }
      ' Cargo.lock
    }
    command="$1"
    named=''
    target=''
    case "$command" in
      metadata)
        if [ "$(version_of fresh)" != 1.0.0 ] || { [ -f block ] && [ "$(version_of h5)" = 1.1.0 ]; }; then
          echo 'error: joint seed cannot resolve' >&2
          exit 101
        fi
        ;;
      update)
        while [ "$#" -gt 0 ]; do
          case "$1" in
            -p) shift; name="${1##*#}"; named="${name%@*}" ;;
            --precise) shift; target="$1" ;;
          esac
          shift
        done
        if [ "$named" = fresh ]; then
          echo 'error: failed to select a version for the requirement `fresh = "^1.0"`' >&2
          exit 101
        fi
        ;;
      *) exit 102 ;;
    esac
    for i in 1 2 3 4 5; do
      if grep -q "^d$i = \"2" Cargo.toml; then app=2; else app=1; fi
      holder=$(version_of "h$i")
      if [ "$named" = "h$i" ]; then holder="$target"; fi
      if [ "$named" = "d$i" ] && [ "$app" = 1 ]; then
        echo "error: failed to select a version for the requirement \`d$i = \"^1\"\`" >&2
        exit 101
      fi
      if [ "$holder" = 1.1.0 ]; then bound=2; else bound=1; fi
      eval "app$i=$app; holder$i=$holder; bound$i=$bound"
    done
    {
      printf 'version = 4\n\n[[package]]\nname = "app"\nversion = "0.1.0"\ndependencies = [\n'
      for i in 1 2 3 4 5; do
        eval "app=\$app$i; bound=\$bound$i"
        if [ "$app" = "$bound" ]; then printf ' "d%s",\n' "$i"; else printf ' "d%s %s.0.0",\n' "$i" "$app"; fi
      done
      printf ' "fresh",\n "h1",\n "h2",\n "h3",\n "h4",\n "h5",\n]\n'
      for i in 1 2 3 4 5; do
        eval "app=\$app$i; bound=\$bound$i"
        for major in 1 2; do
          if [ "$app" = "$major" ] || [ "$bound" = "$major" ]; then
            printf '\n[[package]]\nname = "d%s"\nversion = "%s.0.0"\nsource = "%s"\n' "$i" "$major" "$crates_io"
          fi
        done
      done
      printf '\n[[package]]\nname = "fresh"\nversion = "1.0.0"\nsource = "%s"\n' "$crates_io"
      for i in 1 2 3 4 5; do
        eval "app=\$app$i; bound=\$bound$i; holder=\$holder$i"
        if [ "$app" = "$bound" ]; then ref="\"d$i\""; else ref="\"d$i $bound.0.0\""; fi
        printf '\n[[package]]\nname = "h%s"\nversion = "%s"\nsource = "%s"\ndependencies = [\n %s,\n]\n' "$i" "$holder" "$crates_io" "$ref"
      done
    } > resolved.lock
    mv resolved.lock Cargo.lock
    if [ "$command" = metadata ]; then
      echo '{"packages": [], "workspace_members": [], "workspace_root": "", "resolve": null}'
    fi
"#};

const CHAIN_MANIFEST: &str = indoc! {r#"
    [package]
    name = "app"
    version = "0.1.0"
    edition = "2024"

    [dependencies]
    d1 = "1"
    d2 = "1"
    d3 = "1"
    d4 = "1"
    d5 = "1"
    fresh = "1"
    h1 = "1"
    h2 = "1"
    h3 = "1"
    h4 = "1"
    h5 = "1"
"#};

/// The starting lock of [`CHAIN_SCRIPT`]: every crate and holder on line 1.
fn chain_lock() -> String {
    let crates_io = "registry+https://github.com/rust-lang/crates.io-index";
    let mut content = String::from(indoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = [
         "d1",
         "d2",
         "d3",
         "d4",
         "d5",
         "fresh",
         "h1",
         "h2",
         "h3",
         "h4",
         "h5",
        ]
    "#});
    for name in ["d1", "d2", "d3", "d4", "d5", "fresh"] {
        content.push_str(&formatdoc! {r#"

            [[package]]
            name = "{name}"
            version = "1.0.0"
            source = "{crates_io}"
        "#});
    }
    for index in 1..=5 {
        content.push_str(&formatdoc! {r#"

            [[package]]
            name = "h{index}"
            version = "1.0.0"
            source = "{crates_io}"
            dependencies = [
             "d{index}",
            ]
        "#});
    }
    content
}

/// Under `Always` every declaration moves to line 2 at once, so each step forks every crate whose
/// holder has not moved yet, and a completion meets one more split per round.
///
/// With `fresh` refused the plan never lands in one seed, so it lands only when completion keeps
/// adding partners until all five holders move together, which takes more rounds than a pin's
/// blocker release does.
#[tokio::test]
async fn a_split_completion_grows_until_every_holder_moves() -> eyre::Result<()> {
    let fixture = SplitFixture::with_files(
        CHAIN_SCRIPT,
        &[
            ("Cargo.toml".to_owned(), CHAIN_MANIFEST.to_owned()),
            ("Cargo.lock".to_owned(), chain_lock()),
        ],
    )?;
    let mut changes: Vec<Change> = (1..=5)
        .flat_map(|index| {
            [
                change(&format!("d{index}"), "1.0.0", "2.0.0", false),
                change(&format!("h{index}"), "1.0.0", "1.1.0", false),
            ]
        })
        .collect();
    changes.push(change("fresh", "1.0.0", "1.1.0", false));
    let plan = SplitFixture::plan(changes, RewriteMode::Always);

    let rejections = fixture.resolve(&plan).await?;

    // Every crate is on line 2 alone and every holder moved, while `fresh` stays held.
    let lock = read_lock(&fixture.project)?;
    for index in 1..=5 {
        assert!(
            lock.has_crates_io_package(&format!("d{index}"), "2.0.0")
                && !lock.has_crates_io_package(&format!("d{index}"), "1.0.0")
                && lock.has_crates_io_package(&format!("h{index}"), "1.1.0"),
            "{}\n{}",
            fixture.file("Cargo.lock")?,
            fixture.file("invocations")?
        );
    }
    assert!(
        rejections.contains_key(&("fresh".to_owned(), "1.0.0".to_owned(), "1.1.0".to_owned())),
        "{rejections:?}"
    );
    Ok(())
}

/// A completion that failed is not resolved again for the same coalition from the same files,
/// whichever refused step meets it.
#[tokio::test]
async fn a_failed_split_completion_is_not_repeated() -> eyre::Result<()> {
    let fixture = SplitFixture::with_files(
        CHAIN_SCRIPT,
        &[
            ("Cargo.toml".to_owned(), CHAIN_MANIFEST.to_owned()),
            ("Cargo.lock".to_owned(), chain_lock()),
            ("block".to_owned(), String::new()),
        ],
    )?;
    let changes: Vec<Change> = (1..=5)
        .flat_map(|index| {
            [
                change(&format!("d{index}"), "1.0.0", "2.0.0", false),
                change(&format!("h{index}"), "1.0.0", "1.1.0", false),
            ]
        })
        .collect();
    let plan = SplitFixture::plan(changes, RewriteMode::Always);

    fixture.resolve(&plan).await?;

    // `h5` cannot move, so no completion lands it, and the steps that meet one of the same
    // failed coalitions skip its resolve: without that, this plan takes 62 Cargo calls.
    assert!(read_lock(&fixture.project)?.has_crates_io_package("h5", "1.0.0"));
    let invocations = fixture.invocations()?;
    assert!(invocations <= 52, "{invocations} cargo calls");
    Ok(())
}

/// A completion that failed without releasing a blocker is still tried when it releases one,
/// since the release changes what the completion runs.
#[tokio::test]
async fn a_completion_releasing_a_blocker_is_not_skipped_as_failed() -> eyre::Result<()> {
    let mut files = vec![
        ("Cargo.toml".to_owned(), UNLOCK_MANIFEST.to_owned()),
        (
            "Cargo.lock".to_owned(),
            unlock_lock("1.0.0", "0.9.3", "1.0.0"),
        ),
        ("refusal.txt".to_owned(), GLUE_REFUSAL.to_owned()),
    ];
    for holder in ["1.0.0", "1.1.0"] {
        for glue in ["0.9.1", "0.9.3"] {
            for (major, app_dep) in [("1", "1.0.0"), ("2", "2.0.0")] {
                files.push((
                    format!("lock-{holder}-{glue}-{major}"),
                    unlock_lock(holder, glue, app_dep),
                ));
            }
        }
    }
    let fixture = SplitFixture::with_files(UNLOCK_SCRIPT, &files)?;
    let plan = SplitFixture::plan(vec![dep_move(), holder_move()], RewriteMode::Auto);
    let journal = fixture
        .tool
        .mutation_journal(&fixture.project, &plan)
        .await?;
    let probe = WidenProbe {
        project: &fixture.project,
        plan: &plan.changes,
        rewrite: RewriteMode::Auto,
        candidates: &[],
        protected: &[],
        followers: &[],
        journal: &journal,
        observer: None,
    };
    let holder = holder_move();
    let heal = SplitHeal {
        name: "dep".to_owned(),
        retainers: vec![PackageKey {
            name: "app".to_owned(),
            version: "0.1.0".to_owned(),
        }],
    };

    // Without `glue` released, `holder` 1.1.0 cannot resolve beside `pal`.
    let kept = fixture
        .tool
        .complete_split(&probe, &holder, heal.clone(), &[])
        .await?;
    assert!(matches!(kept, SplitCompletion::Refused));
    assert_eq!(
        fixture.file("Cargo.lock")?,
        unlock_lock("1.0.0", "0.9.3", "1.0.0")
    );

    // The same coalition from the same files, releasing `glue`, lands.
    let released = [PackageKey {
        name: "glue".to_owned(),
        version: "0.9.3".to_owned(),
    }];
    let landed = fixture
        .tool
        .complete_split(&probe, &holder, heal, &released)
        .await?;
    assert!(matches!(landed, SplitCompletion::Landed(_)));
    assert_eq!(
        fixture.file("Cargo.lock")?,
        unlock_lock("1.1.0", "0.9.1", "2.0.0")
    );
    Ok(())
}
