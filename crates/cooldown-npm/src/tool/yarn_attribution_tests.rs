//! Yarn Classic's per-member attribution through manifest selectors: each workspace member binds
//! the lock entry its own effective selector names, so the dependency rows, the exact-pin guard,
//! and the landing check judge each member on its own copy.

use super::*;
use crate::lock::yarn_fixtures::{self, YarnWorkspace};
use crate::lock::{Npm, Yarn};
use color_eyre::eyre;
use indoc::indoc;

fn tool() -> eyre::Result<(tempfile::TempDir, NpmTool<Yarn>)> {
    let cache = tempfile::tempdir()?;
    let tool = NpmTool::from_http(SharedHttp::new(
        cache.path(),
        cooldown_registry::HttpOptions::default(),
    )?);
    Ok((cache, tool))
}

/// A project written into a temporary directory.
struct TempProject {
    /// Owns the temporary root directory; dropping it deletes the project on disk, so it must
    /// stay bound for as long as the project is used.
    _guard: tempfile::TempDir,
    project: Project,
}

/// A Yarn project of `files`, each a project-relative path and its contents.
fn yarn_project(files: &[(&str, &str)]) -> eyre::Result<TempProject> {
    let guard = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(guard.path().to_path_buf())
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    for (rel, contents) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    let project = Project {
        root: root.clone(),
        kind: Yarn::ID,
        manifest: root.join("package.json"),
        exclude_newer: None,
        exclude_newer_package: std::collections::BTreeMap::new(),
        generated_members: cooldown_core::GeneratedMembers::undeclared(),
    };
    Ok(TempProject {
        _guard: guard,
        project,
    })
}

/// The project a [`YarnWorkspace`] fixture records: its manifests and its lock.
fn fixture_project(fixture: &YarnWorkspace) -> eyre::Result<TempProject> {
    let mut files: Vec<(&str, &str)> = fixture.manifests.to_vec();
    files.push(("yarn.lock", fixture.lock));
    yarn_project(&files)
}

/// One direct dependency row as these tests compare it: `name@version [members]`, with ` pinned`
/// when every declaration binding the copy pins it exactly.
fn direct_rows(deps: &[Dependency]) -> Vec<String> {
    let mut rows: Vec<String> = deps
        .iter()
        .filter(|dep| dep.direct)
        .map(|dep| {
            let members = dep
                .members
                .iter()
                .map(|member| member.path.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let pinned = if dep.pinned { " pinned" } else { "" };
            format!("{}@{} [{members}]{pinned}", dep.package.name, dep.current)
        })
        .collect();
    rows.sort();
    rows
}

/// A Yarn change declared by `members`, given as `(name, path)`.
fn yarn_change(name: &str, from: &str, to: &str, members: &[(&str, &str)]) -> Change {
    Change {
        package: PackageId::new(Yarn::ID, name, Some(NPM.to_string())),
        from: Version::new(from),
        to: Version::new(to),
        kind: cooldown_core::UpdateKind::Minor,
        downgrade: false,
        direct: true,
        members: members
            .iter()
            .map(|(name, path)| MemberRef {
                name: (*name).to_string(),
                path: (*path).to_string(),
            })
            .collect(),
    }
}

/// Two members pinning different versions of one package each own their own copy, and a copy the
/// root's caret shares with a member's pin belongs to both, while the root's hidden dev pin beside
/// that caret leaves it unpinned.
#[tokio::test]
async fn yarn_members_pinning_different_versions_each_own_their_copy() -> eyre::Result<()> {
    let TempProject { _guard, project } = fixture_project(&yarn_fixtures::PINNED_MEMBERS)?;
    let (_cache, tool) = tool()?;

    let graph = tool.dependencies(&project, DepScope::Graph).await?;

    // Each copy belongs to the members whose effective selector names it; the alias, the linked
    // sibling, and every transitive are no member's.
    assert_eq!(
        direct_rows(&graph),
        [
            "@sindresorhus/is@4.0.0 [packages/b] pinned",
            "chalk@4.1.0 [packages/a] pinned",
            "chalk@4.1.2 [.,packages/b]",
            "is-number@7.0.0 [.]",
        ]
    );
    Ok(())
}

/// A copy a member's selector identifies but whose binding no edit is proven to move stays that
/// member's direct dependency: a selector the root `resolutions` can rewrite, a local directory,
/// a name a workspace package carries, and git, URL, and `link:` sources.
/// The entry only a nested resolution names stays transitive.
#[tokio::test]
async fn yarn_unproven_bindings_stay_direct() -> eyre::Result<()> {
    let (_cache, tool) = tool()?;

    let TempProject { _guard, project } = fixture_project(&yarn_fixtures::RESOLUTIONS_AND_SOURCES)?;
    let graph = tool.dependencies(&project, DepScope::Graph).await?;
    assert_eq!(
        direct_rows(&graph),
        [
            "@sindresorhus/is@4.6.0 [packages/a]",
            "ansi-regex@6.5.0 [.]",
            "color-name@1.1.3 [.]",
            "has-flag@5.0.1 [.,tools/group/c]",
            "is-number@7.0.0 [.,tools/group/c]",
            "local-thing@0.1.0 [packages/a]",
            "ms@2.1.3 [packages/a]",
        ]
    );
    assert!(
        graph.iter().any(|dep| !dep.direct
            && dep.package.name == "@sindresorhus/is"
            && dep.current.as_str() == "4.0.1"),
        "{graph:?}"
    );

    let TempProject { _guard, project } = fixture_project(&yarn_fixtures::SINGLE_PACKAGE_SOURCES)?;
    let graph = tool.dependencies(&project, DepScope::Graph).await?;
    assert_eq!(
        direct_rows(&graph),
        [
            "color-name@1.1.4 [.]",
            "has-flag@4.0.0 [.]",
            "is-number@7.0.0 [.]",
            "linked@0.0.0 [.]",
            "ms@2.1.2 [.]",
        ]
    );
    Ok(())
}

/// A duplicate nested under another dependency stays transitive though a member declares its
/// name, when the member's own selector identifies the other copy, proven or not.
#[tokio::test]
async fn yarn_a_nested_duplicate_of_an_unproven_binding_stays_transitive() -> eyre::Result<()> {
    let (_cache, tool) = tool()?;
    // Synthetic: the root's `chalk` caret is rewritten by `resolutions`, and `legacy` requires an
    // older `chalk` of its own.
    let TempProject { _guard, project } = yarn_project(&[
        (
            "package.json",
            r#"{ "name": "app", "dependencies": { "chalk": "^4.1.0", "legacy": "^1.0.0" }, "resolutions": { "chalk": "4.1.2" } }"#,
        ),
        (
            "yarn.lock",
            indoc! {r#"
                # yarn lockfile v1


                chalk@4.1.2, chalk@^4.1.0:
                  version "4.1.2"
                  resolved "https://registry.yarnpkg.com/chalk/-/chalk-4.1.2.tgz"

                chalk@^3.0.0:
                  version "3.0.0"
                  resolved "https://registry.yarnpkg.com/chalk/-/chalk-3.0.0.tgz"

                legacy@^1.0.0:
                  version "1.0.0"
                  resolved "https://registry.yarnpkg.com/legacy/-/legacy-1.0.0.tgz"
                  dependencies:
                    chalk "^3.0.0"
            "#},
        ),
    ])?;

    let graph = tool.dependencies(&project, DepScope::Graph).await?;

    assert_eq!(direct_rows(&graph), ["chalk@4.1.2 [.]", "legacy@1.0.0 [.]"]);
    assert!(
        graph.iter().any(|dep| !dep.direct
            && dep.package.name == "chalk"
            && dep.current.as_str() == "3.0.0"),
        "{graph:?}"
    );
    Ok(())
}

/// `upgrade` holds every unproven direct copy `not_eligible`, naming each member with why no edit
/// of its declaration is proven to move it, before Yarn ever runs or a manifest is touched.
#[cfg(unix)]
#[tokio::test]
async fn yarn_unproven_bindings_are_not_eligible() -> eyre::Result<()> {
    for (fixture, changes, expected) in [
        (
            &yarn_fixtures::RESOLUTIONS_AND_SOURCES,
            vec![
                yarn_change(
                    "is-number",
                    "7.0.0",
                    "7.0.1",
                    &[("root", "."), ("ms", "tools/group/c")],
                ),
                yarn_change("local-thing", "0.1.0", "0.2.0", &[("pkg-a", "packages/a")]),
                yarn_change("ms", "2.1.3", "2.1.4", &[("pkg-a", "packages/a")]),
            ],
            vec![
                (
                    "is-number",
                    "is-number@7.0.0 is resolved for root, ms under the root `resolutions` \
                     field, which overrides any range a manifest edit writes; edit the \
                     resolution to move it",
                ),
                (
                    "local-thing",
                    "local-thing@0.1.0 is installed for pkg-a from a local path (`file:` or \
                     `link:`), so no registry release may replace it through that declaration",
                ),
                (
                    "ms",
                    "ms is also a workspace package's name, which Yarn links in place of a \
                     registry release whenever its version satisfies the range, so no manifest \
                     edit is proven to move ms@2.1.3 for pkg-a",
                ),
            ],
        ),
        (
            &yarn_fixtures::SINGLE_PACKAGE_SOURCES,
            vec![
                yarn_change("is-number", "7.0.0", "7.0.1", &[("single", ".")]),
                yarn_change("linked", "0.0.0", "0.0.1", &[("single", ".")]),
                yarn_change("ms", "2.1.2", "2.1.3", &[("single", ".")]),
            ],
            vec![
                (
                    "is-number",
                    "is-number@7.0.0 is installed for single from a source of its own (git, or \
                     a local or remote tarball), so no registry release may replace it through \
                     that declaration",
                ),
                (
                    "linked",
                    "linked@0.0.0 is installed for single from a local path (`file:` or \
                     `link:`), so no registry release may replace it through that declaration",
                ),
                (
                    "ms",
                    "ms@2.1.2 is installed for single from a source of its own (git, or a local \
                     or remote tarball), so no registry release may replace it through that \
                     declaration",
                ),
            ],
        ),
    ] {
        let TempProject { _guard, project } = fixture_project(fixture)?;
        let (_cache, tool) = scripted_yarn(&project.root, "")?;
        let plan = Plan {
            changes,
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        };

        let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
        let report = tool.apply(&mutation).await?;

        assert!(report.applied.is_empty(), "{report:?}");
        let skips: Vec<(&str, SkipReason, Option<&str>)> = report
            .skipped
            .iter()
            .map(|skip| {
                (
                    skip.change.package.name.as_str(),
                    skip.reason,
                    skip.detail.as_deref(),
                )
            })
            .collect();
        let expected: Vec<(&str, SkipReason, Option<&str>)> = expected
            .into_iter()
            .map(|(name, detail)| (name, SkipReason::NotEligible, Some(detail)))
            .collect();
        assert_eq!(skips, expected);
        assert!(!project.root.join("legs.log").exists(), "Yarn never ran");
        for (rel, contents) in fixture.manifests {
            assert_eq!(
                std::fs::read_to_string(project.root.join(rel))?,
                *contents,
                "{rel}"
            );
        }
    }
    Ok(())
}

/// A declaration whose selector the lock no longer records is reported without a copy, and no
/// copy of its name is credited to its member.
#[tokio::test]
async fn yarn_a_vanished_selector_binds_no_copy() -> eyre::Result<()> {
    let (_cache, tool) = tool()?;
    // Synthetic: the manifest's caret was widened after the lock was written.
    let TempProject { _guard, project } = yarn_project(&[
        (
            "package.json",
            r#"{ "name": "app", "dependencies": { "chalk": "^4.2.0" } }"#,
        ),
        ("yarn.lock", &yarn_lock_of("chalk@^4.1.0", "chalk", "4.1.2")),
    ])?;

    let graph = tool.dependencies(&project, DepScope::Graph).await?;
    let declarations: Vec<String> = tool
        .declarations(&project, "chalk")
        .await?
        .into_iter()
        .map(|declaration| {
            format!(
                "{} {} -> {}",
                declaration.member.path,
                declaration.range.as_deref().unwrap_or("-"),
                declaration.resolved.as_deref().unwrap_or("-"),
            )
        })
        .collect();

    assert!(direct_rows(&graph).is_empty(), "{graph:?}");
    assert_eq!(declarations, [". ^4.2.0 -> -"]);
    Ok(())
}

/// A one-entry Yarn Classic lock resolving `descriptors` (the key line, verbatim) to
/// `name@version` from the public registry.
fn yarn_lock_of(descriptors: &str, name: &str, version: &str) -> String {
    indoc::formatdoc! {r#"
        # yarn lockfile v1


        {descriptors}:
          version "{version}"
          resolved "https://registry.yarnpkg.com/{name}/-/{name}-{version}.tgz"
    "#}
}

/// Losing the selector evidence warns, naming why and what it costs: a workspace whose members
/// cannot all be discovered, and a lock Yarn's own reader refuses.
/// A complete workspace beside a readable lock warns of nothing, and neither does npm, whose
/// attribution reads no workspace manifest.
#[tokio::test]
async fn yarn_warns_when_the_selector_evidence_is_lost() -> eyre::Result<()> {
    let (_cache, tool) = tool()?;
    let lost = "so cooldown attributes no dependency to a workspace member: the rows follow the \
                root package.json alone, a landing is judged by the name's newest locked copy, \
                and no member's exact manifest pin is protected from another candidate's resolve";
    let TempProject { _guard, project } = fixture_project(&yarn_fixtures::PINNED_MEMBERS)?;
    assert_eq!(tool.manifest_notices(&project).await?, []);

    // A lock Yarn refuses
    let conflicted = yarn_fixtures::PINNED_MEMBERS.lock.replace(
        "chalk@4.1.0:",
        indoc! {"
            <<<<<<< HEAD
            chalk@4.1.0:
            =======
            >>>>>>> other"},
    );
    std::fs::write(project.root.join("yarn.lock"), &conflicted)?;
    let [notice] = tool
        .manifest_notices(&project)
        .await?
        .try_into()
        .map_err(|notices: Vec<Diagnostic>| eyre::eyre!("one notice: {notices:?}"))?;
    assert_eq!(notice.kind, DiagnosticKind::LockfileUnreadable);
    assert_eq!(
        notice.message,
        format!("yarn.lock is no lock yarn itself would read, {lost}")
    );
    assert_eq!(notice.path.as_deref(), Some("package.json"));

    // Members discovery cannot establish
    std::fs::write(
        project.root.join("yarn.lock"),
        yarn_fixtures::PINNED_MEMBERS.lock,
    )?;
    std::fs::write(
        project.root.join("package.json"),
        r#"{ "name": "root", "private": true, "workspaces": ["packages/{a,b}"] }"#,
    )?;
    let reason = WorkspaceManifests::read(&project.root)
        .unavailable_reason()
        .map(str::to_string)
        .ok_or_else(|| eyre::eyre!("a brace pattern is undiscoverable"))?;
    let [notice] = tool
        .manifest_notices(&project)
        .await?
        .try_into()
        .map_err(|notices: Vec<Diagnostic>| eyre::eyre!("one notice: {notices:?}"))?;
    assert_eq!(notice.kind, DiagnosticKind::Config);
    assert_eq!(
        notice.message,
        format!("its workspace members could not all be discovered ({reason}), {lost}")
    );
    assert_eq!(notice.path.as_deref(), Some("package.json"));

    // npm reads its members from the lock alone.
    let cache = tempfile::tempdir()?;
    let npm = NpmTool::<Npm>::from_http(SharedHttp::new(
        cache.path(),
        cooldown_registry::HttpOptions::default(),
    )?);
    let npm_project = Project {
        kind: Npm::ID,
        ..project.clone()
    };
    assert_eq!(npm.manifest_notices(&npm_project).await?, []);
    Ok(())
}

/// The lock of [`yarn_sibling_pin_fixture`] before any candidate lands.
///
/// Hand-written in the shape Yarn 1.22.22 writes, since its packages are fictitious: the root and
/// `pkg-a` share the `pinned@^1.0.0` entry, and the `resolved` URLs follow registry.yarnpkg.com's
/// tarball layout.
#[cfg(unix)]
const SIBLING_PIN_LOCK: &str = indoc! {r#"
    # THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
    # yarn lockfile v1


    first@^1.0.0:
      version "1.0.0"
      resolved "https://registry.yarnpkg.com/first/-/first-1.0.0.tgz"

    pinned@^1.0.0:
      version "1.0.0"
      resolved "https://registry.yarnpkg.com/pinned/-/pinned-1.0.0.tgz"

    second@^1.0.0:
      version "1.0.0"
      resolved "https://registry.yarnpkg.com/second/-/second-1.0.0.tgz"
"#};

/// A Yarn workspace whose root declares `first`, `second`, and `pinned` at carets and whose
/// excluded-to-be member `packages/a` (`pkg-a`) declares `pinned` at the same caret beside an
/// exact dev pin, which Yarn normalizes to the caret, driven by a scripted `yarn install`.
///
/// The script lands each widened root caret at 1.1.0 under its rewritten descriptor.
/// Landing `second` also moves the shared `pinned@^1.0.0` entry to 1.1.0, the synthetic stand-in
/// for a resolve that re-resolves a sibling's caret, so `pkg-a`'s dev pin no longer holds.
#[cfg(unix)]
fn yarn_sibling_pin_fixture(root: &Utf8Path) -> eyre::Result<(tempfile::TempDir, NpmTool<Yarn>)> {
    std::fs::write(
        root.join("package.json"),
        indoc! {r#"
            {
              "name": "app",
              "version": "1.0.0",
              "private": true,
              "workspaces": ["packages/*"],
              "dependencies": { "first": "^1.0.0", "second": "^1.0.0", "pinned": "^1.0.0" }
            }
        "#},
    )?;
    std::fs::create_dir_all(root.join("packages/a"))?;
    std::fs::write(
        root.join("packages/a/package.json"),
        indoc! {r#"
            {
              "name": "pkg-a",
              "version": "1.0.0",
              "dependencies": { "pinned": "^1.0.0" },
              "devDependencies": { "pinned": "1.0.0" }
            }
        "#},
    )?;
    std::fs::write(root.join("yarn.lock"), SIBLING_PIN_LOCK)?;
    scripted_yarn(
        root,
        indoc! {r#"
            if grep -q '"first": "^1.1.0"' package.json; then land first 1.0.0 1.1.0; fi
            if grep -q '"second": "^1.1.0"' package.json; then
              land second 1.0.0 1.1.0
              move pinned 1.0.0 1.1.0
            fi
        "#},
    )
}

/// The tool driving a scripted `yarn` at `root` whose `install` runs `on_install`, a shell snippet
/// rewriting `yarn.lock` the way a resolve would.
///
/// The snippet's `move <name> <from> <to>` moves the entry of `name` at `from` to `to`, and
/// `land <name> <from> <to>` also moves its `^<from>` descriptor to `^<to>`, the key a widened
/// caret requests.
#[cfg(unix)]
fn scripted_yarn(
    root: &Utf8Path,
    on_install: &str,
) -> eyre::Result<(tempfile::TempDir, NpmTool<Yarn>)> {
    use std::os::unix::fs::PermissionsExt as _;
    let script = root.join("fake-yarn.sh");
    std::fs::write(
        &script,
        indoc::formatdoc! {r#"
            #!/bin/sh
            echo "$*" >> legs.log
            move() {{
                sed -e "/^$1@/,/^\$/s|version \"$2\"|version \"$3\"|" \
                    -e "s|/$1-$2.tgz|/$1-$3.tgz|" \
                  yarn.lock > yarn.next && mv yarn.next yarn.lock
            }}
            land() {{
                move "$1" "$2" "$3"
                sed -e "s|^$1@^$2:|$1@^$3:|" yarn.lock > yarn.next && mv yarn.next yarn.lock
            }}
            case "$*" in
              install)
            {on_install}
                exit 0 ;;
            esac
            echo "unexpected args: $*" >&2; exit 1
        "#},
    )?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    let (cache, mut tool) = tool()?;
    tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
    Ok((cache, tool))
}

/// Yarn lands candidates one at a time, judging each against the state the earlier landings left:
/// the first lands under its rewritten selector, while the second's install moves an excluded
/// member's hidden pin, absent from the plan, and alone is rolled back.
#[cfg(unix)]
#[tokio::test]
async fn yarn_rolls_back_only_the_candidate_that_moves_an_excluded_members_pin() -> eyre::Result<()>
{
    let TempProject { _guard, project } = yarn_project(&[])?;
    let (_cache, tool) = yarn_sibling_pin_fixture(&project.root)?;
    let plan = Plan {
        changes: vec![
            yarn_change("first", "1.0.0", "1.1.0", &[("app", ".")]),
            yarn_change("second", "1.0.0", "1.1.0", &[("app", ".")]),
        ],
        excluded_members: vec![MemberRef {
            name: "pkg-a".to_string(),
            path: "packages/a".to_string(),
        }],
        ..Plan::default()
    };

    let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
    let report = tool.apply(&mutation).await?;

    // The accepted candidate stays applied.
    assert_eq!(
        report
            .applied
            .iter()
            .map(|change| format!("{}@{}", change.package.name, change.to))
            .collect::<Vec<_>>(),
        ["first@1.1.0"]
    );

    // The later candidate is refused on the excluded member's dev pin, which it alone moved.
    let [skip] = report.skipped.as_slice() else {
        eyre::bail!("one rolled-back candidate: {:?}", report.skipped)
    };
    assert_eq!(skip.change.package.name, "second");
    assert_eq!(skip.reason, SkipReason::ResolverConflict);
    assert_eq!(
        skip.detail.as_deref(),
        Some(
            "the resolve moved an exact pin it was not asked to rewrite: pinned resolves to \
             1.1.0 for packages/a/package.json, whose devDependencies pins 1.0.0"
        )
    );

    // The lock and manifests are the first candidate's post-state: the shared `pinned` entry and
    // the second caret are restored.
    let first_landed = SIBLING_PIN_LOCK
        .replace(
            "first@^1.0.0:\n  version \"1.0.0\"",
            "first@^1.1.0:\n  version \"1.1.0\"",
        )
        .replace("/first-1.0.0.tgz", "/first-1.1.0.tgz");
    assert_eq!(
        std::fs::read_to_string(project.root.join("yarn.lock"))?,
        first_landed
    );
    let declared = std::fs::read_to_string(project.root.join("package.json"))?;
    assert!(declared.contains(r#""first": "^1.1.0""#), "{declared}");
    assert!(declared.contains(r#""second": "^1.0.0""#), "{declared}");
    Ok(())
}

/// A `--rewrite` pin edit is the selector the later candidates start from: the first candidate
/// rewrites `own`'s caret and its exact dev pin and lands, so the second's resolve moving the
/// rewritten entry off the replacement pin is refused, though the baseline selector would no
/// longer find that entry at all.
#[cfg(unix)]
#[tokio::test]
async fn yarn_keeps_an_earlier_authorized_rewrite_protected() -> eyre::Result<()> {
    let TempProject { _guard, project } = yarn_project(&[
        (
            "package.json",
            indoc! {r#"
                {
                  "name": "app",
                  "dependencies": { "own": "^1.0.0", "second": "^1.0.0" },
                  "devDependencies": { "own": "1.0.0" }
                }
            "#},
        ),
        (
            "yarn.lock",
            // Synthetic, in the shape Yarn 1.22.22 writes, since the packages are fictitious.
            indoc! {r#"
                # yarn lockfile v1


                own@1.0.0, own@^1.0.0:
                  version "1.0.0"
                  resolved "https://registry.yarnpkg.com/own/-/own-1.0.0.tgz"

                second@^1.0.0:
                  version "1.0.0"
                  resolved "https://registry.yarnpkg.com/second/-/second-1.0.0.tgz"
            "#},
        ),
    ])?;
    let (_cache, tool) = scripted_yarn(
        &project.root,
        indoc! {r#"
            if grep -q '"own": "^1.1.0"' package.json; then
              move own 1.0.0 1.1.0
              sed -e "s|^own@1.0.0, own@^1.0.0:|own@1.1.0, own@^1.1.0:|" yarn.lock > yarn.next \
                && mv yarn.next yarn.lock
            fi
            if grep -q '"second": "^1.1.0"' package.json; then
              land second 1.0.0 1.1.0
              move own 1.1.0 1.2.0
            fi
        "#},
    )?;
    let plan = Plan {
        changes: vec![
            yarn_change("own", "1.0.0", "1.1.0", &[("app", ".")]),
            yarn_change("second", "1.0.0", "1.1.0", &[("app", ".")]),
        ],
        rewrite: RewriteMode::Always,
        ..Plan::default()
    };

    let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
    let report = tool.apply(&mutation).await?;

    // The rewrite lands under the descriptors it rewrote.
    assert_eq!(
        report
            .applied
            .iter()
            .map(|change| format!("{}@{}", change.package.name, change.to))
            .collect::<Vec<_>>(),
        ["own@1.1.0"]
    );

    // The second candidate moved the rewritten entry off the replacement pin, so it alone is
    // refused.
    let [skip] = report.skipped.as_slice() else {
        eyre::bail!("one rolled-back candidate: {:?}", report.skipped)
    };
    assert_eq!(skip.change.package.name, "second");
    assert_eq!(skip.reason, SkipReason::ResolverConflict);
    assert_eq!(
        skip.detail.as_deref(),
        Some(
            "the resolve moved an exact pin it was not asked to rewrite: own resolves to 1.2.0 \
             for package.json, whose devDependencies pins 1.1.0"
        )
    );
    let declared = std::fs::read_to_string(project.root.join("package.json"))?;
    assert!(
        declared.contains(r#""devDependencies": { "own": "1.1.0" }"#),
        "{declared}"
    );
    assert!(declared.contains(r#""second": "^1.0.0""#), "{declared}");
    Ok(())
}

/// A Yarn change lands only where every declaring member binds the target through its own
/// selector: a member's copy left behind is not masked by another member's, a newer copy under
/// another selector does not defeat a member's valid lower target, a rewritten selector is read
/// after cooldown's edit, and a binding the lock does not prove never falls back to the newest
/// copy.
#[test]
fn yarn_landing_is_verified_at_each_members_selector() -> eyre::Result<()> {
    let TempProject { _guard, project } = yarn_project(&[
        (
            "package.json",
            r#"{ "name": "root", "private": true, "workspaces": ["packages/*"], "dependencies": { "chalk": "^4.1.0" } }"#,
        ),
        (
            "packages/a/package.json",
            r#"{ "name": "pkg-a", "version": "1.0.0", "dependencies": { "chalk": "4.1.0" } }"#,
        ),
        (
            "packages/b/package.json",
            r#"{ "name": "pkg-b", "version": "1.0.0", "dependencies": { "chalk": "^4.1.0", "pkg-a": "^1.0.0" } }"#,
        ),
    ])?;
    let workspace = WorkspaceManifests::read(&project.root);
    assert!(
        workspace.is_complete(),
        "{:?}",
        workspace.unavailable_reason()
    );
    // Synthetic locks in Yarn's shape: each `(descriptors, version)` entry as written.
    let write_lock = |entries: &[(&str, &str)]| -> eyre::Result<()> {
        let entries = entries
            .iter()
            .map(|(descriptors, version)| format!("{descriptors}:\n  version \"{version}\"\n"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            project.root.join("yarn.lock"),
            format!("# yarn lockfile v1\n\n\n{entries}"),
        )?;
        Ok(())
    };
    let both = yarn_change(
        "chalk",
        "4.1.0",
        "4.1.2",
        &[("root", "."), ("pkg-a", "packages/a")],
    );

    // The root's caret moved while pkg-a's pin kept its own entry.
    write_lock(&[("chalk@4.1.0", "4.1.0"), ("chalk@^4.1.0", "4.1.2")])?;
    assert!(!exact_target_reached::<Yarn>(
        &project,
        &both,
        Some(&workspace)
    )?);

    // After cooldown rewrote pkg-a's pin, its new selector binds the target too.
    let mut edits = ManifestEdits::default();
    edits.record(&manifest::ManifestRewrite {
        modified: vec![Utf8PathBuf::from("packages/a/package.json")],
        rewritten: vec![manifest::RewrittenDeclaration {
            key: manifest::DeclarationKey {
                member: "packages/a".to_string(),
                field: "dependencies",
                dependency: "chalk".to_string(),
            },
            replacement: "4.1.2".to_string(),
        }],
    });
    let edited = edits.apply_to(&workspace);
    write_lock(&[("chalk@4.1.2, chalk@^4.1.0", "4.1.2")])?;
    assert!(exact_target_reached::<Yarn>(
        &project,
        &both,
        Some(&edited)
    )?);
    assert!(!exact_target_reached::<Yarn>(
        &project,
        &both,
        Some(&workspace)
    )?);

    // A newer copy under another request beside the root's lower target
    let lower = yarn_change("chalk", "4.1.0", "4.1.1", &[("root", ".")]);
    write_lock(&[("chalk@^4.1.0", "4.1.1"), ("chalk@^4.2.0", "4.2.0")])?;
    assert!(exact_target_reached::<Yarn>(
        &project,
        &lower,
        Some(&workspace)
    )?);

    // A sibling's name binds no proven copy, even where the newest copy is the target.
    let sibling = yarn_change("pkg-a", "1.0.0", "1.1.0", &[("pkg-b", "packages/b")]);
    write_lock(&[("pkg-a@^1.0.0", "1.1.0")])?;
    assert!(!exact_target_reached::<Yarn>(
        &project,
        &sibling,
        Some(&workspace)
    )?);
    Ok(())
}

/// A member's landing needs its selector read through the lock: without the workspace snapshot,
/// with its members undiscovered, or with a lock Yarn's own reader refuses, no change lands, even
/// where the newest copy is the target.
#[test]
fn yarn_landing_without_selector_evidence_is_never_proven() -> eyre::Result<()> {
    let TempProject { _guard, project } = yarn_project(&[
        (
            "package.json",
            r#"{ "name": "root", "private": true, "workspaces": ["packages/*"], "dependencies": { "chalk": "^4.1.0" } }"#,
        ),
        (
            "packages/a/package.json",
            r#"{ "name": "pkg-a", "version": "1.0.0" }"#,
        ),
    ])?;
    let complete = WorkspaceManifests::read(&project.root);
    assert!(
        complete.is_complete(),
        "{:?}",
        complete.unavailable_reason()
    );
    let change = yarn_change("chalk", "4.1.0", "4.1.2", &[("root", ".")]);
    let lock = indoc::formatdoc! {r#"
        # yarn lockfile v1


        chalk@^4.1.0:
          version "4.1.2"
          resolved "https://registry.yarnpkg.com/chalk/-/chalk-4.1.2.tgz"
    "#};
    let write_lock = |content: &str| std::fs::write(project.root.join("yarn.lock"), content);

    // The evidence the negative cases each take away
    write_lock(&lock)?;
    assert!(exact_target_reached::<Yarn>(
        &project,
        &change,
        Some(&complete)
    )?);

    // No snapshot at all
    assert!(!exact_target_reached::<Yarn>(&project, &change, None)?);

    // A snapshot whose members could not all be discovered
    std::fs::write(
        project.root.join("package.json"),
        r#"{ "name": "root", "private": true, "workspaces": ["packages/{a,b}"], "dependencies": { "chalk": "^4.1.0" } }"#,
    )?;
    let undiscovered = WorkspaceManifests::read(&project.root);
    assert!(!undiscovered.is_complete());
    assert!(!exact_target_reached::<Yarn>(
        &project,
        &change,
        Some(&undiscovered)
    )?);

    // Locks Yarn's own reader refuses, though cooldown's lenient listing still finds the target
    for refused in [
        lock.replace("yarn lockfile v1", "yarn lockfile v2"),
        indoc::formatdoc! {r#"
            {lock}
            <<<<<<< HEAD
            ms@^2.0.0:
              version "2.1.3"
            =======
            >>>>>>> other
        "#},
    ] {
        write_lock(&refused)?;
        assert_eq!(
            landing_versions::<Yarn>(&refused)?
                .get("chalk")
                .map(String::as_str),
            Some("4.1.2"),
            "{refused}"
        );
        assert!(
            !exact_target_reached::<Yarn>(&project, &change, Some(&complete))?,
            "{refused}"
        );
    }
    Ok(())
}

/// Rescoping drops an excluded member's declarations from a shared copy, so the root's caret no
/// longer keeps the copy a member pins exactly from being pinned.
#[tokio::test]
async fn yarn_rescoping_recomputes_pins_without_excluded_members() -> eyre::Result<()> {
    let TempProject { _guard, project } = fixture_project(&yarn_fixtures::PINNED_MEMBERS)?;
    let (_cache, tool) = tool()?;
    let mut deps: Vec<Dependency> = tool
        .dependencies(&project, DepScope::Direct)
        .await?
        .into_iter()
        .filter(|dep| dep.package.name == "chalk" && dep.current.as_str() == "4.1.2")
        .collect();
    let [shared] = deps.as_slice() else {
        eyre::bail!("one shared chalk copy: {deps:?}")
    };
    assert!(!shared.pinned);

    tool.rescope_members(
        &project,
        &mut deps,
        &[MemberRef {
            name: "root".to_string(),
            path: ".".to_string(),
        }],
    )
    .await?;

    let [shared] = deps.as_slice() else {
        eyre::bail!("one shared chalk copy: {deps:?}")
    };
    assert!(shared.pinned);
    Ok(())
}

/// Each member's declarations report the copy its own selector binds, one row per distinct range,
/// though the lock records no importer entry.
#[tokio::test]
async fn yarn_declarations_report_each_members_bound_copy() -> eyre::Result<()> {
    let TempProject { _guard, project } = fixture_project(&yarn_fixtures::PINNED_MEMBERS)?;
    let (_cache, tool) = tool()?;

    let mut rows: Vec<String> = tool
        .declarations(&project, "chalk")
        .await?
        .into_iter()
        .map(|declaration| {
            format!(
                "{} {} {} -> {}",
                declaration.member.path,
                declaration.fields.join(","),
                declaration.range.as_deref().unwrap_or("-"),
                declaration.resolved.as_deref().unwrap_or("-"),
            )
        })
        .collect();
    rows.sort();

    assert_eq!(
        rows,
        [
            ". dependencies ^4.1.0 -> 4.1.2",
            ". devDependencies 4.1.0 -> 4.1.2",
            "packages/a dependencies 4.1.0 -> 4.1.0",
            "packages/b dependencies 4.1.2 -> 4.1.2",
        ]
    );
    Ok(())
}

/// The mutation baseline captures every workspace member's manifest, an excluded member's and one
/// absent from the plan included, before any lock exists, while the journal's write set stays the
/// plan's own.
#[tokio::test]
async fn yarn_baseline_captures_every_member_without_a_lock() -> eyre::Result<()> {
    let manifests: Vec<(&str, &str)> = yarn_fixtures::PINNED_MEMBERS.manifests.to_vec();
    let TempProject { _guard, project } = yarn_project(&manifests)?;
    let (_cache, tool) = tool()?;
    let plan = Plan {
        changes: vec![yarn_change("is-number", "7.0.0", "7.0.1", &[("root", ".")])],
        excluded_members: vec![MemberRef {
            name: "pkg-a".to_string(),
            path: "packages/a".to_string(),
        }],
        ..Plan::default()
    };

    let baseline = tool.mutation_baseline(&project, &plan).await?;
    let journal = tool.mutation_journal(&project, &plan).await?;

    let mut captured: Vec<String> = baseline
        .files()
        .iter()
        .map(|file| manifest::display_rel(file.path()))
        .collect();
    captured.sort();
    assert_eq!(
        captured,
        [
            "package.json",
            "packages/a/package.json",
            "packages/b/package.json"
        ]
    );
    assert!(
        !journal
            .files()
            .iter()
            .any(|file| file.path().starts_with("packages")),
        "{:?}",
        journal.files()
    );
    Ok(())
}

/// A workspace whose members cannot all be discovered attributes nothing, so the root manifest's
/// names stay the direct set, exactly as for a lock without member data.
#[tokio::test]
async fn an_undiscoverable_yarn_workspace_keeps_the_root_manifest_reading() -> eyre::Result<()> {
    let manifests: Vec<(&str, &str)> = yarn_fixtures::PINNED_MEMBERS
        .manifests
        .iter()
        .map(|(rel, content)| {
            let content: &str = if *rel == "package.json" {
                r#"{ "name": "root", "private": true, "workspaces": ["packages/{a,b}"], "dependencies": { "chalk": "^4.1.0", "is-number": "^7.0.0" } }"#
            } else {
                content
            };
            (*rel, content)
        })
        .collect();
    let mut files = manifests;
    files.push(("yarn.lock", yarn_fixtures::PINNED_MEMBERS.lock));
    let TempProject { _guard, project } = yarn_project(&files)?;
    let (_cache, tool) = tool()?;

    let graph = tool.dependencies(&project, DepScope::Graph).await?;

    assert_eq!(
        direct_rows(&graph),
        ["chalk@4.1.0 []", "chalk@4.1.2 []", "is-number@7.0.0 []"]
    );
    Ok(())
}
