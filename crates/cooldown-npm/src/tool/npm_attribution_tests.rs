//! npm's per-member attribution through the install tree: a locked copy belongs to the members
//! whose own lookup binds it, so a nested duplicate of a declared name is transitive in the
//! dependency rows, their pins and bounds, the landing check, and the per-package apply.

use super::*;
use crate::lock::Npm;
use color_eyre::eyre;
use indoc::{formatdoc, indoc};

fn tool() -> NpmTool<Npm> {
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    NpmTool::from_http(
        SharedHttp::new(cache_dir.path(), cooldown_registry::HttpOptions::default()).expect("http"),
    )
}

/// A project written into a temporary directory.
struct TempProject {
    /// Owns the temporary root directory; dropping it deletes the project on disk, so it must
    /// stay bound for as long as the project is used.
    _guard: tempfile::TempDir,
    project: Project,
}

/// An npm project of `files`, each a project-relative path and its contents.
fn npm_project(files: &[(&str, &str)]) -> eyre::Result<TempProject> {
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
        kind: Npm::ID,
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

/// A `package-lock.json` of `lockfile_version` whose `packages` map holds `packages`.
/// A v2 lock also carries the legacy `dependencies` tree, here contradicting every copy the map
/// records, so a reader that consulted it would attribute the wrong ones.
fn tree_lock(lockfile_version: u8, packages: &str) -> String {
    let legacy = if lockfile_version == 2 {
        indoc! {r#"
            "dependencies": {
              "other": { "version": "9.0.0" },
              "react": { "version": "9.0.0" }
            },
        "#}
    } else {
        ""
    };
    formatdoc! {r#"
        {{
          "name": "root",
          "lockfileVersion": {lockfile_version},
          {legacy}
          "packages": {{
            {packages}
          }}
        }}
    "#}
}

/// One dependency row as these tests compare it.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    name: String,
    version: String,
    direct: bool,
    declared_bound: Option<String>,
    members: Vec<String>,
    pinned: bool,
}

/// The row of `name@version` owned by `members`, direct exactly when some member owns it.
fn row(
    name: &str,
    version: &str,
    members: &[&str],
    declared_bound: Option<&str>,
    pinned: bool,
) -> Row {
    Row {
        name: name.to_string(),
        version: version.to_string(),
        direct: !members.is_empty(),
        declared_bound: declared_bound.map(str::to_string),
        members: members.iter().map(ToString::to_string).collect(),
        pinned,
    }
}

impl From<Dependency> for Row {
    fn from(dep: Dependency) -> Self {
        Row {
            name: dep.package.name,
            version: dep.current.as_str().to_string(),
            direct: dep.direct,
            declared_bound: dep.declared_bound,
            members: dep.members.into_iter().map(|member| member.path).collect(),
            pinned: dep.pinned,
        }
    }
}

/// The sorted rows of `deps`.
fn rows(deps: Vec<Dependency>) -> Vec<Row> {
    let mut rows: Vec<Row> = deps.into_iter().map(Row::from).collect();
    rows.sort();
    rows
}

/// The sorted graph rows and the sorted `name@version` of the direct scope of `project`.
async fn graph_and_direct(project: &Project) -> eyre::Result<(Vec<Row>, Vec<String>)> {
    let graph = rows(tool().dependencies(project, DepScope::Graph).await?);
    let mut direct: Vec<String> = tool()
        .dependencies(project, DepScope::Direct)
        .await?
        .iter()
        .map(|dep| format!("{}@{}", dep.package.name, dep.current.as_str()))
        .collect();
    direct.sort();
    Ok((graph, direct))
}

/// The root's lock, declaring `other` at `range` and `unrelated`, which nests `other@2.0.0`.
fn nested_duplicate_lock(lockfile_version: u8, range: &str) -> String {
    tree_lock(
        lockfile_version,
        &formatdoc! {r#"
            "": {{ "name": "root", "dependencies": {{ "other": "{range}", "unrelated": "^1.0.0" }} }},
            "node_modules/other": {{ "version": "1.0.0" }},
            "node_modules/unrelated": {{ "version": "1.0.0", "dependencies": {{ "other": "^2.0.0" }} }},
            "node_modules/unrelated/node_modules/other": {{ "version": "2.0.0" }}
        "#},
    )
}

/// The root's own declaration owns only the copy its lookup binds: the `2.0.0` an unrelated
/// dependency nests is transitive, with no member, bound, or pin, so a move of it can never be
/// planned against the root's range.
#[tokio::test]
async fn a_nested_duplicate_is_transitive_beside_the_members_own_copy() -> eyre::Result<()> {
    for lockfile_version in [2, 3] {
        for (range, bound, pinned) in [
            (">=1.0.0 <1.5.0", Some(">=1.0.0 <1.5.0"), false),
            ("1.0.0", None, true),
        ] {
            let manifest = formatdoc! {r#"
                {{ "name": "root", "dependencies": {{ "other": "{range}", "unrelated": "^1.0.0" }} }}
            "#};
            let lock = nested_duplicate_lock(lockfile_version, range);
            let TempProject { _guard, project } =
                npm_project(&[("package.json", &manifest), ("package-lock.json", &lock)])?;

            let (graph, direct) = graph_and_direct(&project).await?;

            assert_eq!(
                graph,
                [
                    row("other", "1.0.0", &["."], bound, pinned),
                    row("other", "2.0.0", &[], None, false),
                    row("unrelated", "1.0.0", &["."], None, false),
                ],
                "v{lockfile_version} {range}"
            );
            assert_eq!(
                direct,
                ["other@1.0.0", "unrelated@1.0.0"],
                "v{lockfile_version} {range}"
            );
        }
    }
    Ok(())
}

/// Members on different copies each own their own, with the bound their own manifests declare:
/// a hoisted copy, one nested in the member's `node_modules`, and the hoisted copy a member
/// without a `node_modules` of its own binds.
#[tokio::test]
async fn workspace_members_own_their_own_copies() -> eyre::Result<()> {
    for lockfile_version in [2, 3] {
        let lock = tree_lock(
            lockfile_version,
            indoc! {r#"
                "": { "name": "root", "workspaces": ["apps/a", "apps/b", "apps/c"] },
                "apps/a": { "name": "a", "version": "0.1.0", "dependencies": { "other": ">=1.0.0 <1.5.0" } },
                "apps/b": { "name": "b", "version": "0.1.0", "dependencies": { "other": ">=2.0.0 <2.5.0" } },
                "apps/c": { "name": "c", "version": "0.1.0", "dependencies": { "other": ">=1.0.0 <1.2.0" } },
                "node_modules/a": { "resolved": "apps/a", "link": true },
                "node_modules/b": { "resolved": "apps/b", "link": true },
                "node_modules/c": { "resolved": "apps/c", "link": true },
                "node_modules/other": { "version": "1.0.0" },
                "apps/b/node_modules/other": { "version": "2.0.0" }
            "#},
        );
        let member = |name: &str, range: &str| {
            formatdoc! {r#"
                {{ "name": "{name}", "version": "0.1.0", "dependencies": {{ "other": "{range}" }} }}
            "#}
        };
        let TempProject { _guard, project } = npm_project(&[
            (
                "package.json",
                r#"{ "name": "root", "workspaces": ["apps/a", "apps/b", "apps/c"] }"#,
            ),
            ("apps/a/package.json", &member("a", ">=1.0.0 <1.5.0")),
            ("apps/b/package.json", &member("b", ">=2.0.0 <2.5.0")),
            ("apps/c/package.json", &member("c", ">=1.0.0 <1.2.0")),
            ("package-lock.json", &lock),
        ])?;

        let (graph, direct) = graph_and_direct(&project).await?;

        assert_eq!(
            graph,
            [
                row(
                    "other",
                    "1.0.0",
                    &["apps/a", "apps/c"],
                    Some(">=1.0.0 <1.2.0"),
                    false
                ),
                row("other", "2.0.0", &["apps/b"], Some(">=2.0.0 <2.5.0"), false),
            ],
            "v{lockfile_version}"
        );
        assert_eq!(
            direct,
            ["other@1.0.0", "other@2.0.0"],
            "v{lockfile_version}"
        );
    }
    Ok(())
}

/// An alias's release gets no editable member even where a genuine registry copy of the same
/// release is nested under another dependency, which made the alias safeguard step aside.
#[tokio::test]
async fn an_alias_target_beside_a_genuine_nested_copy_gets_no_editable_members() -> eyre::Result<()>
{
    for lockfile_version in [2, 3] {
        let manifest = indoc! {r#"
            { "name": "root", "dependencies": { "foo": "npm:other@2.0.0", "other": "1.0.0", "x": "^1.0.0" } }
        "#};
        let lock = tree_lock(
            lockfile_version,
            indoc! {r#"
                "": {
                  "name": "root",
                  "dependencies": { "foo": "npm:other@2.0.0", "other": "1.0.0", "x": "^1.0.0" }
                },
                "node_modules/foo": { "name": "other", "version": "2.0.0" },
                "node_modules/other": { "version": "1.0.0" },
                "node_modules/x": { "version": "1.0.0" },
                "node_modules/x/node_modules/other": { "version": "2.0.0" }
            "#},
        );
        let TempProject { _guard, project } =
            npm_project(&[("package.json", manifest), ("package-lock.json", &lock)])?;

        let (graph, direct) = graph_and_direct(&project).await?;

        assert_eq!(
            graph,
            [
                row("other", "1.0.0", &["."], None, true),
                row("other", "2.0.0", &[], None, false),
                row("x", "1.0.0", &["."], None, false),
            ],
            "v{lockfile_version}"
        );
        assert_eq!(direct, ["other@1.0.0", "x@1.0.0"], "v{lockfile_version}");
    }
    Ok(())
}

/// A peer-only declaration is direct for the copy its lookup binds, with the peer range as its
/// bound, while a copy nested under another dependency stays transitive.
#[tokio::test]
async fn a_peer_only_declaration_owns_only_the_copy_it_binds() -> eyre::Result<()> {
    for lockfile_version in [2, 3] {
        let member = indoc! {r#"
            { "name": "a", "version": "0.1.0", "peerDependencies": { "react": ">=18.0.0 <18.3.0" } }
        "#};
        let lock = tree_lock(
            lockfile_version,
            indoc! {r#"
                "": { "name": "root", "workspaces": ["apps/a"], "dependencies": { "x": "^1.0.0" } },
                "apps/a": { "name": "a", "version": "0.1.0", "peerDependencies": { "react": ">=18.0.0 <18.3.0" } },
                "node_modules/a": { "resolved": "apps/a", "link": true },
                "node_modules/react": { "version": "18.2.0", "peer": true },
                "node_modules/x": { "version": "1.0.0" },
                "node_modules/x/node_modules/react": { "version": "17.0.2" }
            "#},
        );
        let TempProject { _guard, project } = npm_project(&[
            (
                "package.json",
                r#"{ "name": "root", "workspaces": ["apps/a"], "dependencies": { "x": "^1.0.0" } }"#,
            ),
            ("apps/a/package.json", member),
            ("package-lock.json", &lock),
        ])?;

        let (graph, direct) = graph_and_direct(&project).await?;

        assert_eq!(
            graph,
            [
                row("react", "17.0.2", &[], None, false),
                row(
                    "react",
                    "18.2.0",
                    &["apps/a"],
                    Some(">=18.0.0 <18.3.0"),
                    false
                ),
                row("x", "1.0.0", &["."], None, false),
            ],
            "v{lockfile_version}"
        );
        assert_eq!(direct, ["react@18.2.0", "x@1.0.0"], "v{lockfile_version}");
    }
    Ok(())
}

/// An excluded member's declarations neither release a pin nor bound a row, and a range binding
/// another copy never released this copy's pin to begin with.
#[tokio::test]
async fn exclusions_recompute_pins_and_bounds_per_copy() -> eyre::Result<()> {
    for lockfile_version in [2, 3] {
        let lock = tree_lock(
            lockfile_version,
            indoc! {r#"
                "": { "name": "root", "workspaces": ["apps/a", "apps/b", "apps/c"] },
                "apps/a": { "name": "a", "version": "0.1.0", "dependencies": { "other": "1.0.0" } },
                "apps/b": { "name": "b", "version": "0.1.0", "dependencies": { "other": "^2.0.0" } },
                "apps/c": { "name": "c", "version": "0.1.0", "dependencies": { "other": ">=1.0.0 <1.2.0" } },
                "node_modules/other": { "version": "1.0.0" },
                "apps/b/node_modules/other": { "version": "2.0.0" }
            "#},
        );
        let member = |name: &str, range: &str| {
            formatdoc! {r#"
                {{ "name": "{name}", "version": "0.1.0", "dependencies": {{ "other": "{range}" }} }}
            "#}
        };
        let TempProject { _guard, project } = npm_project(&[
            (
                "package.json",
                r#"{ "name": "root", "workspaces": ["apps/a", "apps/b", "apps/c"] }"#,
            ),
            ("apps/a/package.json", &member("a", "1.0.0")),
            ("apps/b/package.json", &member("b", "^2.0.0")),
            ("apps/c/package.json", &member("c", ">=1.0.0 <1.2.0")),
            ("package-lock.json", &lock),
        ])?;
        let mut deps = tool().dependencies(&project, DepScope::Graph).await?;
        let excluded = [MemberRef {
            name: "c".to_string(),
            path: "apps/c".to_string(),
        }];
        // The scoping chokepoint drops the excluded member from every row before rescoping.
        for dep in &mut deps {
            dep.members.retain(|member| member.path != "apps/c");
        }

        tool()
            .rescope_members(&project, &mut deps, &excluded)
            .await?;

        assert_eq!(
            rows(deps),
            [
                row("other", "1.0.0", &["apps/a"], None, true),
                row("other", "2.0.0", &["apps/b"], None, false),
            ],
            "v{lockfile_version}"
        );
    }
    Ok(())
}

/// A landing is judged on every declaring member's own registry binding: a member left behind on
/// its own copy, or bound to nothing, is not stood in for by a copy at the target elsewhere, and
/// neither a link nor a remote tarball at the target is a registry landing.
#[test]
fn a_landing_requires_every_member_to_bind_the_target() -> eyre::Result<()> {
    let TempProject { _guard, project } = npm_project(&[])?;
    let lock_declaring = |b_declares: &str, slots: &str| {
        tree_lock(
            3,
            &formatdoc! {r#"
                "": {{ "name": "root", "workspaces": ["apps/a", "apps/b", "packages/other"] }},
                "apps/a": {{ "name": "a", "version": "0.1.0", "dependencies": {{ "other": "^1.0.0" }} }},
                "apps/b": {{ "name": "b", "version": "0.1.0", "dependencies": {{ "other": "{b_declares}" }} }},
                "packages/other": {{ "name": "other", "version": "1.1.0" }},
                "node_modules/x": {{ "version": "1.0.0" }},
                "node_modules/x/node_modules/other": {{ "version": "1.1.0" }},
                {slots}
            "#},
        )
    };
    let member = |path: &str| MemberRef {
        name: path.to_string(),
        path: path.to_string(),
    };
    let change = Change {
        package: PackageId::new(Npm::ID, "other", Some(NPM.to_string())),
        from: Version::new("1.0.0"),
        to: Version::new("1.1.0"),
        kind: UpdateKind::Minor,
        downgrade: false,
        direct: true,
        members: vec![member("apps/a"), member("apps/b")],
    };
    let reached_declaring = |b_declares: &str, slots: &str| -> eyre::Result<bool> {
        std::fs::write(
            project.root.join("package-lock.json"),
            lock_declaring(b_declares, slots),
        )?;
        Ok(exact_target_reached::<Npm>(&project, &change)?)
    };
    let reached = |slots: &str| reached_declaring("^1.0.0", slots);

    assert!(reached(r#""node_modules/other": { "version": "1.1.0" }"#)?);
    assert!(
        !reached(indoc! {r#"
            "apps/a/node_modules/other": { "version": "1.1.0" },
            "apps/b/node_modules/other": { "version": "1.0.0" }
        "#})?,
        "a member left behind on its own copy"
    );
    assert!(
        !reached(r#""apps/a/node_modules/other": { "version": "1.1.0" }"#)?,
        "a member bound to nothing"
    );
    assert!(
        !reached(indoc! {r#"
            "node_modules/other": { "version": "1.1.0" },
            "apps/b/node_modules/other": { "resolved": "packages/other", "link": true }
        "#})?,
        "a member bound to a link at the target"
    );
    assert!(
        !reached_declaring(
            "https://example.org/other-1.1.0.tgz",
            indoc! {r#"
                "node_modules/other": { "version": "1.1.0" },
                "apps/b/node_modules/other": { "version": "1.1.0", "resolved": "https://example.org/other-1.1.0.tgz" }
            "#}
        )?,
        "a member bound to a remote tarball at the target"
    );
    Ok(())
}

/// A fake `npm` that logs each invocation to `legs.log` and changes nothing.
#[cfg(unix)]
fn logging_npm(root: &Utf8Path) -> eyre::Result<NpmTool<Npm>> {
    use std::os::unix::fs::PermissionsExt as _;
    let script = root.join("fake-npm.sh");
    std::fs::write(
        &script,
        indoc! {r#"
            #!/bin/sh
            echo "$*" >> legs.log
            exit 0
        "#},
    )?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    let mut tool = tool();
    tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
    Ok(tool)
}

/// A transitive candidate has no member, so the manifest fallback would rewrite the root's
/// declaration of the same name, which binds another copy: it is skipped as not eligible before
/// any manifest is inspected or npm runs.
/// A direct candidate without members, which a v1 lock plans, still lands through the root.
#[cfg(unix)]
#[tokio::test]
async fn a_transitive_copy_never_lands_through_the_root_declaration() -> eyre::Result<()> {
    let manifest = indoc! {r#"
        { "name": "root", "dependencies": { "other": "^1.0.0", "unrelated": "^1.0.0" } }
    "#};
    let v1_lock = indoc! {r#"
        { "name": "root", "lockfileVersion": 1, "dependencies": { "other": { "version": "1.0.0" } } }
    "#};
    for lockfile_version in [1, 2, 3] {
        let lock = if lockfile_version == 1 {
            v1_lock.to_string()
        } else {
            nested_duplicate_lock(lockfile_version, "^1.0.0")
        };
        let TempProject { _guard, project } =
            npm_project(&[("package.json", manifest), ("package-lock.json", &lock)])?;
        let tool = logging_npm(&project.root)?;
        let direct = lockfile_version == 1;
        let (from, to) = if direct {
            ("1.0.0", "1.0.1")
        } else {
            ("2.0.0", "2.0.1")
        };
        let plan = Plan {
            changes: vec![Change {
                package: PackageId::new(Npm::ID, "other", Some(NPM.to_string())),
                from: Version::new(from),
                to: Version::new(to),
                kind: UpdateKind::Patch,
                downgrade: false,
                direct,
                members: Vec::new(),
            }],
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        };

        let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
        let report = tool.apply(&mutation).await?;

        let legs = std::fs::read_to_string(project.root.join("legs.log")).unwrap_or_default();
        assert!(report.applied.is_empty(), "v{lockfile_version}: {report:?}");
        let [skip] = report.skipped.as_slice() else {
            eyre::bail!("v{lockfile_version}: one skip: {:?}", report.skipped)
        };
        if direct {
            // The legacy candidate reached npm, which left the lock where it was.
            assert!(legs.contains("install other@1.0.1"), "{legs}");
            assert_eq!(skip.reason, SkipReason::GraphCeilingHeld);
        } else {
            assert_eq!(skip.reason, SkipReason::NotEligible, "v{lockfile_version}");
            assert_eq!(
                skip.detail.as_deref(),
                Some(
                    "other@2.0.0 is a transitive copy that no member's declaration binds, so no \
                     manifest edit may move it"
                ),
                "v{lockfile_version}"
            );
            assert_eq!(legs, "", "v{lockfile_version}: npm never ran");
        }
        assert_eq!(
            std::fs::read_to_string(project.root.join("package.json"))?,
            manifest,
            "v{lockfile_version}"
        );
        assert_eq!(
            std::fs::read_to_string(project.root.join("package-lock.json"))?,
            lock,
            "v{lockfile_version}"
        );
    }
    Ok(())
}

/// The root and `apps/a` declaring dependencies installed from git, local and remote tarballs, and
/// the registry, where `apps/a` binds a git `other` of the same version as the root's registry
/// one.
fn non_registry_lock(lockfile_version: u8) -> String {
    tree_lock(
        lockfile_version,
        indoc! {r#"
            "": {
              "name": "root",
              "workspaces": ["apps/a"],
              "dependencies": {
                "repo": "github:user/repo#v3.0.0",
                "vendored": "file:vendor/vendored-1.0.0.tgz",
                "remote": "https://example.org/remote-1.0.0.tgz",
                "other": ">=1.0.0 <1.5.0"
              }
            },
            "apps/a": {
              "name": "a",
              "version": "0.1.0",
              "dependencies": { "plugin": "github:user/plugin", "other": "github:user/other#v1.0.0" }
            },
            "node_modules/a": { "resolved": "apps/a", "link": true },
            "node_modules/repo": { "version": "3.0.0", "resolved": "git+ssh://git@github.com/user/repo.git#0123abc" },
            "node_modules/vendored": { "version": "1.0.0", "resolved": "file:vendor/vendored-1.0.0.tgz" },
            "node_modules/remote": { "version": "1.0.0", "resolved": "https://example.org/remote-1.0.0.tgz" },
            "node_modules/plugin": { "version": "2.0.0", "resolved": "git+ssh://git@github.com/user/plugin.git#4567def" },
            "node_modules/other": { "version": "1.0.0", "resolved": "https://registry.npmjs.org/other/-/other-1.0.0.tgz" },
            "apps/a/node_modules/other": { "version": "1.0.0", "resolved": "git+ssh://git@github.com/user/other.git#89abcde" }
        "#},
    )
}

/// The manifests behind [`non_registry_lock`].
const NON_REGISTRY_MANIFESTS: [(&str, &str); 2] = [
    (
        "package.json",
        indoc! {r#"
            {
              "name": "root",
              "workspaces": ["apps/a"],
              "dependencies": {
                "repo": "github:user/repo#v3.0.0",
                "vendored": "file:vendor/vendored-1.0.0.tgz",
                "remote": "https://example.org/remote-1.0.0.tgz",
                "other": ">=1.0.0 <1.5.0"
              }
            }
        "#},
    ),
    (
        "apps/a/package.json",
        indoc! {r#"
            {
              "name": "a",
              "version": "0.1.0",
              "dependencies": { "plugin": "github:user/plugin", "other": "github:user/other#v1.0.0" }
            }
        "#},
    ),
];

/// A dependency a member declares from git or a local or remote tarball is the member's direct
/// dependency, reported and scoped like any other, and leaves with the member when the member is
/// excluded rather than lingering as a transitive row.
/// Only registry declarations bound it: a git copy of the same version a member declares beside
/// the root's registry range takes the range's bound, and a copy with no registry owner has none.
#[tokio::test]
async fn direct_non_registry_dependencies_stay_direct() -> eyre::Result<()> {
    for lockfile_version in [2, 3] {
        let lock = non_registry_lock(lockfile_version);
        let mut files = NON_REGISTRY_MANIFESTS.to_vec();
        files.push(("package-lock.json", &lock));
        let TempProject { _guard, project } = npm_project(&files)?;

        let (graph, direct) = graph_and_direct(&project).await?;

        assert_eq!(
            graph,
            [
                row(
                    "other",
                    "1.0.0",
                    &[".", "apps/a"],
                    Some(">=1.0.0 <1.5.0"),
                    false
                ),
                row("plugin", "2.0.0", &["apps/a"], None, false),
                row("remote", "1.0.0", &["."], None, false),
                row("repo", "3.0.0", &["."], None, false),
                row("vendored", "1.0.0", &["."], None, false),
            ],
            "v{lockfile_version}"
        );
        assert_eq!(
            direct,
            [
                "other@1.0.0",
                "plugin@2.0.0",
                "remote@1.0.0",
                "repo@3.0.0",
                "vendored@1.0.0"
            ],
            "v{lockfile_version}"
        );

        // Excluding `apps/a` leaves its git copies without an owner, so the scoping chokepoint
        // drops their rows with it, and the shared row keeps only the root.
        let index = Npm::member_sources_excluding(&lock, &HashSet::from(["apps/a".to_string()]));
        assert_eq!(index.members_for("plugin", "2.0.0"), [] as [String; 0]);
        assert_eq!(index.members_for("other", "1.0.0"), ["."]);
        assert_eq!(index.editable_members_for("other", "1.0.0"), ["."]);
    }
    Ok(())
}

/// A direct dependency bound through a source of its own is not eligible for the per-package
/// landing: npm's exact pin would swap the source for a registry release, so the candidate is
/// skipped with the source named before any manifest is read or npm runs, also when another
/// member binds the same version from the registry.
#[cfg(unix)]
#[tokio::test]
async fn a_non_registry_direct_dependency_is_not_eligible_for_landing() -> eyre::Result<()> {
    for lockfile_version in [2, 3] {
        let lock = non_registry_lock(lockfile_version);
        let mut files = NON_REGISTRY_MANIFESTS.to_vec();
        files.push(("package-lock.json", &lock));
        let TempProject { _guard, project } = npm_project(&files)?;
        let tool = logging_npm(&project.root)?;
        let member = |name: &str, path: &str| MemberRef {
            name: name.to_string(),
            path: path.to_string(),
        };
        let candidate = |name: &str, from: &str, to: &str, members: Vec<MemberRef>| Change {
            package: PackageId::new(Npm::ID, name, Some(NPM.to_string())),
            from: Version::new(from),
            to: Version::new(to),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members,
        };
        let plan = Plan {
            changes: vec![
                candidate("repo", "3.0.0", "3.1.0", vec![member("root", ".")]),
                candidate("remote", "1.0.0", "1.1.0", vec![member("root", ".")]),
                candidate(
                    "other",
                    "1.0.0",
                    "1.1.0",
                    vec![member("root", "."), member("a", "apps/a")],
                ),
            ],
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        };

        let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
        let report = tool.apply(&mutation).await?;

        assert!(report.applied.is_empty(), "v{lockfile_version}: {report:?}");
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
        let detail = |copy: &str, members: &str| {
            format!(
                "{copy} is installed for {members} from a source of its own (git, or a local or \
                 remote tarball), so no registry release may replace it through that declaration"
            )
        };
        assert_eq!(
            skips,
            [
                (
                    "repo",
                    SkipReason::NotEligible,
                    Some(detail("repo@3.0.0", "root").as_str())
                ),
                (
                    "remote",
                    SkipReason::NotEligible,
                    Some(detail("remote@1.0.0", "root").as_str())
                ),
                (
                    "other",
                    SkipReason::NotEligible,
                    Some(detail("other@1.0.0", "a").as_str())
                ),
            ],
            "v{lockfile_version}"
        );
        let legs = std::fs::read_to_string(project.root.join("legs.log")).unwrap_or_default();
        assert_eq!(legs, "", "v{lockfile_version}: npm never ran");
        for (rel, contents) in &files {
            assert_eq!(
                std::fs::read_to_string(project.root.join(rel))?,
                *contents,
                "v{lockfile_version} {rel}"
            );
        }
    }
    Ok(())
}

/// One own-name source of `plugin@3.0.0` that an alias's registry release of the same version
/// shares a row with.
struct OwnSource {
    /// The root manifest's specifier.
    specifier: &'static str,
    /// The `resolved` the own-name slot records.
    resolved: &'static str,
}

/// The plugin installed from git and from a local tarball.
const OWN_SOURCES: [OwnSource; 2] = [
    OwnSource {
        specifier: "github:user/plugin#v3.0.0",
        resolved: "git+ssh://git@github.com/user/plugin.git#0123abc",
    },
    OwnSource {
        specifier: "file:vendor/plugin-3.0.0.tgz",
        resolved: "file:vendor/plugin-3.0.0.tgz",
    },
];

/// The root manifest and lock declaring `plugin` from `source` beside the alias
/// `plugin-alias` of the registry `plugin@3.0.0`.
fn aliased_own_source(lockfile_version: u8, source: &OwnSource) -> (String, String) {
    let OwnSource {
        specifier,
        resolved,
    } = source;
    let manifest = formatdoc! {r#"
        {{
          "name": "root",
          "dependencies": {{ "plugin": "{specifier}", "plugin-alias": "npm:plugin@3.0.0" }}
        }}
    "#};
    let lock = tree_lock(
        lockfile_version,
        &formatdoc! {r#"
            "": {{
              "name": "root",
              "dependencies": {{ "plugin": "{specifier}", "plugin-alias": "npm:plugin@3.0.0" }}
            }},
            "node_modules/plugin": {{ "version": "3.0.0", "resolved": "{resolved}" }},
            "node_modules/plugin-alias": {{
              "name": "plugin",
              "version": "3.0.0",
              "resolved": "https://registry.npmjs.org/plugin/-/plugin-3.0.0.tgz"
            }}
        "#},
    );
    (manifest, lock)
}

/// An alias installing a registry release of the same version as a member's own git or tarball
/// copy leaves that copy its owner: the shared row is the root's direct dependency, without a
/// pin or bound.
#[tokio::test]
async fn an_alias_beside_an_own_non_registry_copy_keeps_it_direct() -> eyre::Result<()> {
    for lockfile_version in [2, 3] {
        for source in &OWN_SOURCES {
            let specifier = source.specifier;
            let (manifest, lock) = aliased_own_source(lockfile_version, source);
            let TempProject { _guard, project } =
                npm_project(&[("package.json", &manifest), ("package-lock.json", &lock)])?;

            let (graph, direct) = graph_and_direct(&project).await?;

            assert_eq!(
                graph,
                [row("plugin", "3.0.0", &["."], None, false)],
                "v{lockfile_version} {specifier}"
            );
            assert_eq!(direct, ["plugin@3.0.0"], "v{lockfile_version} {specifier}");
        }
    }
    Ok(())
}

/// The copy an alias shares its version with is not eligible for a landing for its source's
/// sake, never the alias's, and is skipped before any manifest is read or npm runs.
#[cfg(unix)]
#[tokio::test]
async fn an_own_non_registry_copy_beside_an_alias_is_skipped_for_its_source() -> eyre::Result<()> {
    for lockfile_version in [2, 3] {
        for source in &OWN_SOURCES {
            let specifier = source.specifier;
            let (manifest, lock) = aliased_own_source(lockfile_version, source);
            let TempProject { _guard, project } =
                npm_project(&[("package.json", &manifest), ("package-lock.json", &lock)])?;
            let tool = logging_npm(&project.root)?;
            let plan = Plan {
                changes: vec![Change {
                    package: PackageId::new(Npm::ID, "plugin", Some(NPM.to_string())),
                    from: Version::new("3.0.0"),
                    to: Version::new("3.1.0"),
                    kind: UpdateKind::Minor,
                    downgrade: false,
                    direct: true,
                    members: vec![MemberRef {
                        name: "root".to_string(),
                        path: ".".to_string(),
                    }],
                }],
                rewrite: RewriteMode::Auto,
                ..Plan::default()
            };

            let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
            let report = tool.apply(&mutation).await?;

            let [skip] = report.skipped.as_slice() else {
                eyre::bail!("v{lockfile_version} {specifier}: one skip: {report:?}")
            };
            assert_eq!(skip.reason, SkipReason::NotEligible);
            assert_eq!(
                skip.detail.as_deref(),
                Some(
                    "plugin@3.0.0 is installed for root from a source of its own (git, or a local \
                     or remote tarball), so no registry release may replace it through that \
                     declaration"
                ),
                "v{lockfile_version} {specifier}"
            );
            let legs = std::fs::read_to_string(project.root.join("legs.log")).unwrap_or_default();
            assert_eq!(legs, "", "v{lockfile_version} {specifier}: npm never ran");
            assert_eq!(
                std::fs::read_to_string(project.root.join("package.json"))?,
                manifest
            );
            assert_eq!(
                std::fs::read_to_string(project.root.join("package-lock.json"))?,
                lock
            );
        }
    }
    Ok(())
}
