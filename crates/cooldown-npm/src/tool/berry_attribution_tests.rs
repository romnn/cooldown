//! Yarn Berry's per-member attribution through the adapter: each member owns the copies its own
//! declarations bind, a declaration no lock evidence proves leaves its name direct but owned by
//! no member, and the warning, the landing check, and the candidate decision all read the same
//! bindings.

use super::*;
use crate::lock::YarnBerry;
use crate::lock::yarn_berry_fixtures::{
    BerryProject, EVERY_SOURCE, MEMBERS, ProjectFile, RESOLUTIONS, REWRITES,
};
use crate::manifest::workspaces::YarnEnvironment;
use color_eyre::eyre;

fn tool() -> eyre::Result<(tempfile::TempDir, NpmTool<YarnBerry>)> {
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

/// The project `fixture` records, with each `(path, contents)` of `extra` written beside it.
fn berry_project(fixture: &BerryProject, extra: &[(&str, &str)]) -> eyre::Result<TempProject> {
    let guard = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(guard.path().to_path_buf())
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    let files = fixture
        .files
        .iter()
        .map(|ProjectFile { path, contents }| (*path, *contents))
        .chain([("yarn.lock", fixture.lock)])
        .chain(extra.iter().copied());
    for (rel, contents) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    let project = Project::detected(root.clone(), YarnBerry::ID, root.join("package.json"));
    Ok(TempProject {
        _guard: guard,
        project,
    })
}

/// An rc file configuring a plugin, which makes the bindings unreadable.
const PLUGIN_RC: (&str, &str) = (
    ".yarnrc.yml",
    "plugins:\n  - path: .yarn/plugins/noop.cjs\n    spec: noop\n",
);

/// One direct row as these tests compare it: `name@version [members]`, with ` pinned` when every
/// declaration binding the copy pins it exactly.
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

/// A Berry change of `name` declared by `members`, given as `(name, path)`.
fn berry_change(name: &str, from: &str, to: &str, members: &[(&str, &str)]) -> Change {
    Change {
        package: PackageId::new(YarnBerry::ID, name, Some(NPM.to_string())),
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

/// The rows of a Berry workspace follow each member's own declarations: a member's pin is its own
/// direct copy, a transitive duplicate of a declared name stays transitive, and a name declared
/// through a binding no lock evidence proves is direct in every copy but owned by no member, so
/// it never reads as a transitive dependency.
/// With the bindings unreadable, every name a workspace's entry records is such a name.
#[tokio::test]
async fn berry_rows_follow_each_members_declarations() -> eyre::Result<()> {
    let (_cache, tool) = tool()?;

    let TempProject { _guard, project } = berry_project(&MEMBERS, &[])?;
    let graph = tool.dependencies(&project, DepScope::Graph).await?;
    assert_eq!(
        direct_rows(&graph),
        [
            "chalk@4.1.0 [packages/a] pinned",
            "chalk@4.1.2 [.]",
            "chalk@5.6.2 [packages/b]",
            "debug@4.3.4 [packages/b] pinned",
            "debug@4.4.3 [packages/a]",
            "has-flag@4.0.0 [packages/a]",
            "is-number@6.0.0 [packages/b/nested/c]",
            "is-number@7.0.0 [packages/a]",
            "is-odd@3.0.1 [packages/b]",
            "ms@2.0.0 [packages/b] pinned",
            "semver@7.5.4 []",
            "typescript@5.9.3 [.]",
        ]
    );
    // `debug`'s newer `ms` copies stay transitive beside `packages/b`'s pin.
    assert!(
        graph
            .iter()
            .filter(|dep| dep.package.name == "ms" && dep.current.as_str() != "2.0.0")
            .all(|dep| !dep.direct && dep.members.is_empty()),
        "{graph:?}"
    );

    let TempProject { _guard, project } = berry_project(&REWRITES, &[])?;
    let graph = tool.dependencies(&project, DepScope::Graph).await?;
    assert_eq!(
        direct_rows(&graph),
        [
            "chalk@4.1.0 []",
            "chalk@4.1.2 []",
            "has-flag@4.0.0 [packages/a]",
            "react@18.3.1 []",
        ]
    );

    let TempProject { _guard, project } = berry_project(&MEMBERS, &[PLUGIN_RC])?;
    let graph = tool.dependencies(&project, DepScope::Graph).await?;
    assert_eq!(
        direct_rows(&graph),
        [
            "chalk@4.1.0 []",
            "chalk@4.1.2 []",
            "chalk@5.6.2 []",
            "debug@4.3.4 []",
            "debug@4.4.3 []",
            "has-flag@4.0.0 []",
            "is-number@6.0.0 []",
            "is-number@7.0.0 []",
            "is-odd@3.0.1 []",
            "ms@2.0.0 []",
            "ms@2.1.2 []",
            "ms@2.1.3 []",
            "semver@7.5.4 []",
            "typescript@5.9.3 []",
        ]
    );
    Ok(())
}

/// One `is-number` release under two locators, the root's own `is-number` binding one and only
/// its `flag` alias the other, with the root `resolutions` leaving the root's own declaration
/// unproven.
///
/// Synthetic: Yarn binds `__archiveUrl` only for a registry whose tarball URL breaks npm's
/// convention, which the public registry never does; the spelling follows `bindLocator` in Yarn
/// 4.18.1's `NpmSemverResolver`.
const ARCHIVE_TWINS: BerryProject = BerryProject {
    files: &[ProjectFile {
        path: "package.json",
        contents: indoc::indoc! {r#"
            {
              "name": "archive-twins",
              "private": true,
              "dependencies": { "flag": "npm:is-number@7.0.0", "is-number": "7.0.0" },
              "resolutions": { "is-number": "7.0.0" }
            }
        "#},
    }],
    lock: indoc::indoc! {r#"
        __metadata:
          version: 10
          cacheKey: 10c0

        "archive-twins@workspace:.":
          version: 0.0.0-use.local
          resolution: "archive-twins@workspace:."
          dependencies:
            flag: "npm:is-number@7.0.0"
            is-number: "npm:7.0.0"
          languageName: unknown
          linkType: soft

        "flag@npm:is-number@7.0.0":
          version: 7.0.0
          resolution: "is-number@npm:7.0.0::__archiveUrl=https%3A%2F%2Fnpm.example.com%2Fis-number%2F7.0.0.tgz"
          languageName: node
          linkType: hard

        "is-number@npm:7.0.0":
          version: 7.0.0
          resolution: "is-number@npm:7.0.0"
          languageName: node
          linkType: hard
    "#},
};

/// A release that its own name binds under any of its locators is a declared copy, however an
/// alias binds its other locators, so a name whose declaration is unproven keeps the release in
/// the direct inventory, owned by no member.
#[tokio::test]
async fn a_release_its_own_name_binds_under_any_locator_stays_direct() -> eyre::Result<()> {
    assert!(!YarnBerry::alias_only_versions(ARCHIVE_TWINS.lock).contains("is-number", "7.0.0"));
    // Without the root's own locator, only the alias binds the release.
    let alias_only = ARCHIVE_TWINS.lock.replace(
        "\"is-number@npm:7.0.0\":\n  version: 7.0.0\n  resolution: \"is-number@npm:7.0.0\"\n",
        "\"other@npm:1.0.0\":\n  version: 1.0.0\n  resolution: \"other@npm:1.0.0\"\n",
    );
    assert_ne!(alias_only, ARCHIVE_TWINS.lock);
    assert!(YarnBerry::alias_only_versions(&alias_only).contains("is-number", "7.0.0"));

    let (_cache, tool) = tool()?;
    let TempProject { _guard, project } = berry_project(&ARCHIVE_TWINS, &[])?;
    let direct = tool.dependencies(&project, DepScope::Direct).await?;
    assert_eq!(direct_rows(&direct), ["is-number@7.0.0 []"], "{direct:?}");
    Ok(())
}

/// `ARCHIVE_TWINS` with its two `is-number` locators in the other order, the plain one first.
fn archive_twins_reversed() -> String {
    let lock = ARCHIVE_TWINS.lock;
    let alias = lock.find("\"flag@npm:").expect("the alias entry");
    let plain = lock
        .find("\"is-number@npm:7.0.0\":")
        .expect("the plain entry");
    let reversed = format!(
        "{}{}\n{}",
        &lock[..alias],
        lock[plain..].trim_end(),
        lock[alias..plain].trim_end()
    );
    assert_ne!(reversed, lock);
    reversed + "\n"
}

/// A release with one locator fetched from a private archive URL and another from the
/// configured registry grants no advisory identity, whichever entry the lock lists first: every
/// locator of the release must prove public routing.
#[tokio::test]
async fn a_release_grants_identity_only_when_every_locator_proves_it() -> eyre::Result<()> {
    let (_cache, tool) = tool()?;
    let identity = async |lock: &str| -> eyre::Result<Option<String>> {
        let fixture = BerryProject {
            files: ARCHIVE_TWINS.files,
            lock: "",
        };
        let TempProject { _guard, project } = berry_project(&fixture, &[("yarn.lock", lock)])?;
        let graph = tool.dependencies(&project, DepScope::Graph).await?;
        let [row] = graph.as_slice() else {
            eyre::bail!("one is-number row: {graph:?}")
        };
        Ok(row.advisory_identity.clone())
    };

    assert_eq!(identity(ARCHIVE_TWINS.lock).await?, None);
    assert_eq!(identity(&archive_twins_reversed()).await?, None);

    // The same release whose archive URL is the public registry's keeps its identity.
    let public = ARCHIVE_TWINS
        .lock
        .replace("npm.example.com", "registry.npmjs.org");
    assert_eq!(identity(&public).await?.as_deref(), Some("is-number"));
    Ok(())
}

/// Yarn Berry never reads npm's configuration, so an `.npmrc` routing every package to a
/// private registry vetoes no Berry identity; Yarn's own routing decides at feed time.
#[tokio::test]
async fn an_npmrc_vetoes_no_berry_identity() -> eyre::Result<()> {
    let (_cache, tool) = tool()?;
    let TempProject { _guard, project } = berry_project(
        &EVERY_SOURCE,
        &[(".npmrc", "registry=https://npm.example.com/\n")],
    )?;

    let graph = tool.dependencies(&project, DepScope::Graph).await?;

    let chalk: Vec<Option<&str>> = graph
        .iter()
        .filter(|dep| dep.package.name == "chalk")
        .map(|dep| dep.advisory_identity.as_deref())
        .collect();
    assert_eq!(chalk, [Some("chalk"), Some("chalk")], "{graph:?}");
    Ok(())
}

/// Dropping an excluded member from a shared copy recomputes its pin from the remaining owners:
/// the root's caret no longer keeps `packages/a`'s exact `chalk` pin from pinning the copy.
#[tokio::test]
async fn berry_rescoping_recomputes_pins_without_excluded_members() -> eyre::Result<()> {
    let TempProject { _guard, project } = berry_project(&EVERY_SOURCE, &[])?;
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
            name: "berry-root".to_string(),
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

/// Each member's declaration reports the copy its own descriptor binds, and an unproven one
/// reports none.
#[tokio::test]
async fn berry_declarations_report_each_members_bound_copy() -> eyre::Result<()> {
    let (_cache, tool) = tool()?;
    let rows = async |project: &Project| -> eyre::Result<Vec<String>> {
        let mut rows: Vec<String> = tool
            .declarations(project, "chalk")
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
        Ok(rows)
    };

    let TempProject { _guard, project } = berry_project(&MEMBERS, &[])?;
    assert_eq!(
        rows(&project).await?,
        [
            ". dependencies ^4.1.0 -> 4.1.2",
            "packages/a dependencies 4.1.0 -> 4.1.0",
            "packages/b dependencies ^5.3.0 -> 5.6.2",
        ]
    );

    let TempProject { _guard, project } = berry_project(&REWRITES, &[])?;
    assert_eq!(
        rows(&project).await?,
        [
            ". dependencies ^4.1.0 -> -",
            "packages/a dependencies ^4.1.0 -> -",
        ]
    );
    Ok(())
}

/// A fully proven Berry workspace warns of nothing, an unproven declaration is named with its
/// members and why, and unreadable bindings warn of what is lost.
#[tokio::test]
async fn berry_notices_name_every_unproven_declaration() -> eyre::Result<()> {
    let (_cache, tool) = tool()?;
    for fixture in [&EVERY_SOURCE, &RESOLUTIONS] {
        let TempProject { _guard, project } = berry_project(fixture, &[])?;
        assert_eq!(tool.manifest_notices(&project).await?, []);
    }

    let TempProject { _guard, project } = berry_project(&REWRITES, &[])?;
    let [notice] = tool
        .manifest_notices(&project)
        .await?
        .try_into()
        .map_err(|notices: Vec<Diagnostic>| eyre::eyre!("one notice: {notices:?}"))?;
    assert_eq!(notice.kind, DiagnosticKind::Config);
    assert_eq!(
        notice.message,
        "cooldown cannot prove which locked copy these workspace declarations bind: chalk in ., packages/a (the root `resolutions` field may rewrite it); react in . (it is a `catalog:` reference); every locked copy of these names is reported as a direct dependency of no member, none is landed, and their exact manifest pins are not protected"
    );
    assert_eq!(notice.path.as_deref(), Some("package.json"));

    let TempProject { _guard, project } = berry_project(&MEMBERS, &[PLUGIN_RC])?;
    let [notice] = tool
        .manifest_notices(&project)
        .await?
        .try_into()
        .map_err(|notices: Vec<Diagnostic>| eyre::eyre!("one notice: {notices:?}"))?;
    assert_eq!(notice.kind, DiagnosticKind::Config);
    assert_eq!(
        notice.message,
        "how Yarn binds its workspace members' declarations could not be established (.yarnrc.yml configures Yarn plugins, which may rewrite how a member's dependencies resolve), so cooldown attributes no dependency to a workspace member: every locked copy of a name a workspace declares is reported as a direct dependency of no member, no landing is proven, and no member's exact manifest pin is protected from another candidate's resolve"
    );
    Ok(())
}

/// A landing is proven only at each declaring member's own binding: a newer copy another member
/// binds neither defeats nor stands in for it, and an unproven declaration never lands.
#[test]
fn berry_landing_is_verified_at_each_members_binding() -> eyre::Result<()> {
    let TempProject { _guard, project } = berry_project(&MEMBERS, &[])?;
    let workspace = WorkspaceManifests::read_in(
        &project.root,
        WorkspaceDialect::Berry,
        &YarnEnvironment::isolated(&project.root),
    );
    let reached =
        |change: &Change| exact_target_reached::<YarnBerry>(&project, change, Some(&workspace));

    // The root binds 4.1.2 though `packages/b` binds the newer 5.6.2.
    assert!(reached(&berry_change(
        "chalk",
        "4.1.0",
        "4.1.2",
        &[("root", ".")]
    ))?);
    // `packages/a` still binds its own 4.1.0.
    assert!(!reached(&berry_change(
        "chalk",
        "4.1.0",
        "4.1.2",
        &[("root", "."), ("a", "packages/a")]
    ))?);
    // The newest `debug` is `packages/a`'s, not the target `packages/b` binds.
    assert!(!reached(&berry_change(
        "debug",
        "4.3.4",
        "4.4.3",
        &[("b", "packages/b")]
    ))?);
    // The root's `semver` is unproven, so even its only copy proves no landing.
    assert!(!reached(&berry_change(
        "semver",
        "7.5.3",
        "7.5.4",
        &[("root", ".")]
    ))?);
    Ok(())
}

/// A candidate of a name declared through an unproven binding is held before any landing is
/// planned, while a proven one is judged as usual.
#[test]
fn berry_unattributed_candidates_are_not_eligible() -> eyre::Result<()> {
    let TempProject { _guard, project } = berry_project(&REWRITES, &[])?;
    let workspace = WorkspaceManifests::read_in(
        &project.root,
        WorkspaceDialect::Berry,
        &YarnEnvironment::isolated(&project.root),
    );
    let baseline = LandingBaseline::of::<YarnBerry>(Some(REWRITES.lock), Some(&workspace));
    let decide = |change: Change| {
        let plan = Plan {
            changes: vec![change.clone()],
            ..Plan::default()
        };
        decide_candidate::<YarnBerry>(&project, &plan, &change, &baseline)
    };

    let CandidateDecision::Skip(skipped) = decide(berry_change("chalk", "4.1.2", "4.1.3", &[]))?
    else {
        eyre::bail!("an unattributed candidate is held");
    };
    assert_eq!(skipped.reason, SkipReason::NotEligible);
    assert_eq!(
        skipped.detail.as_deref(),
        Some(
            "cooldown cannot prove which copy of chalk its declaring workspace members bind, so no declaration is known to move chalk@4.1.2"
        )
    );

    let proven = decide(berry_change(
        "has-flag",
        "4.0.0",
        "4.0.1",
        &[("a", "packages/a")],
    ))?;
    if let CandidateDecision::Skip(skipped) = proven {
        assert!(
            !skipped
                .detail
                .is_some_and(|detail| detail.starts_with("cooldown cannot prove")),
            "a proven candidate passes the guard"
        );
    }
    Ok(())
}

/// The mutation baseline records the members the lock lists and the configuration their
/// bindings depend on, so a plugin or a `defaultProtocol` configured after the capture changes
/// no binding the baseline snapshot reads, while a live read sees it.
#[tokio::test]
async fn berry_baseline_records_its_configuration() -> eyre::Result<()> {
    let TempProject { _guard, project } = berry_project(&MEMBERS, &[])?;
    let (_cache, tool) = tool()?;
    let plan = Plan {
        changes: vec![berry_change("chalk", "4.1.2", "4.1.3", &[("root", ".")])],
        ..Plan::default()
    };

    let baseline = tool.mutation_baseline(&project, &plan).await?;
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
            "packages/b/nested/c/package.json",
            "packages/b/package.json",
            "packages/is-odd/package.json",
        ]
    );

    std::fs::write(
        project.root.join(".yarnrc.yml"),
        "defaultProtocol: \"file:\"\nplugins:\n  - .yarn/plugins/noop.cjs\n",
    )?;
    let captured = WorkspaceManifests::from_baseline(&baseline, WorkspaceDialect::Berry);
    assert!(
        captured.is_complete(),
        "{:?}",
        captured.unavailable_reason()
    );
    assert!(captured.npm_default_protocol());
    assert!(
        !WorkspaceManifests::read(&project.root, WorkspaceDialect::Berry).is_complete(),
        "a live read sees the plugin"
    );
    Ok(())
}
