use super::GO_ID;
use super::releases::{classify_quality, major_key_for_path};
use crate::gocmd::{Go, GoModule};
use crate::gomod;
use crate::proxy::GoProxy;
use cooldown_core::{
    DepScope, Dependency, LockStatus, LockVerifyReport, PackageId, PackageRegistry, Project,
    Release, ReleaseOrder, Result, Version,
};
use std::collections::HashMap;

pub(super) async fn dependencies(
    go: &Go,
    registry: Option<String>,
    project: &Project,
    scope: DepScope,
) -> Result<Vec<Dependency>> {
    let modules = build_modules(go, project).await?;
    let main_path = modules
        .iter()
        .find(|module| module.main)
        .map(|module| module.path.clone())
        .unwrap_or_default();
    let floors = go.mod_graph_floors(&project.root, &main_path).await?;

    let mut deps = Vec::new();
    for module in &modules {
        let Some(dep) = dependency_of(module, &floors, registry.as_deref()) else {
            continue;
        };
        if scope == DepScope::Direct && !dep.direct {
            continue;
        }
        deps.push(dep);
    }
    Ok(deps)
}

/// The modules of `go list -m all` whose versions reach the project's build, the main module
/// included.
///
/// A module that `go.mod` does not require is only in the module graph: its `go.mod` was read
/// while selecting versions, but no package of it is compiled, its version follows the modules
/// that require it, and `go mod tidy` drops a requirement an upgrade would add for it.
/// So when `go.mod` is known to require every module that provides a package (see
/// [`requires_every_build_module`](gomod::GoModFacts::requires_every_build_module)), the rest are
/// left out.
/// In workspace mode the workspace's other modules take part in version selection, so one module's
/// `go.mod` no longer names every module its build compiles, and every module stays.
async fn build_modules(go: &Go, project: &Project) -> Result<Vec<GoModule>> {
    let modules = go.list_modules(&project.root).await?;
    let facts = gomod::from_json(&go.mod_edit_json(&project.root).await?)?;
    if !facts.requires_every_build_module() || go.workspace_file(&project.root).await?.is_some() {
        return Ok(modules);
    }
    // `go.mod` requires a module by its own path, before any `replace`.
    Ok(modules
        .into_iter()
        .filter(|module| module.main || facts.requires.contains_key(&module.path))
        .collect())
}

pub(super) async fn locked_release(proxy: &GoProxy, dep: &Dependency) -> Result<Release> {
    let time = proxy.published_at(&dep.package, &dep.current, &[]).await?;
    Ok(Release {
        version: dep.current.clone(),
        order: ReleaseOrder(Vec::new()),
        major: major_key_for_path(&dep.package.name),
        major_number: crate::semver::major_number(dep.current.as_str()),
        kind_from_current: None,
        beyond_declared_bound: false,
        beyond_latest_tag: false,
        published_at: time,
        yanked: false,
        quality: dep.current_quality,
    })
}

pub(super) async fn verify_lock_current(go: &Go, project: &Project) -> Result<LockVerifyReport> {
    match go.mod_tidy_is_clean(&project.root).await {
        Ok(true) => Ok(LockVerifyReport {
            status: LockStatus::Current,
            detail: "go.mod/go.sum are tidy".to_string(),
        }),
        // The detail names the module so a repository with several go modules says which one to
        // tidy (the diagnostic's own path field is not printed on the TTY).
        Ok(false) => Ok(LockVerifyReport {
            status: LockStatus::Stale,
            detail: format!(
                "go.mod/go.sum are stale in {}; run `go mod tidy`",
                project.root
            ),
        }),
        Err(error) => Err(error),
    }
}

fn dependency_of(
    module: &GoModule,
    floors: &HashMap<String, String>,
    registry: Option<&str>,
) -> Option<Dependency> {
    if module.main || module.is_local_replace() {
        return None;
    }
    let path = module.effective_path().to_string();
    let version = module.effective_version()?.to_string();
    let graph_floor = floors.get(&path).map(|v| Version::new(v.clone()));
    Some(Dependency {
        // A Go module's path *is* its global identity — OSV's `Go` ecosystem keys on it — so
        // origin is provable by construction, whatever proxy served the bytes.
        advisory_identity: Some(path.clone()),
        package: PackageId::new(GO_ID, path, registry.map(str::to_owned)),
        current: Version::new(version.clone()),
        current_quality: classify_quality(&version),
        direct: !module.indirect,
        artifacts: Vec::new(),
        graph_floor,
        // Go's module graph yields only a floor (MVS minimum); it does not impose an `==`-style
        // upper bound on a transitive module, so there is no ceiling to record.
        graph_ceiling: None,
        declared_bound: None,
        members: Vec::new(),
        pinned: false,
        hold_edges: Vec::new(),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use color_eyre::eyre;

    /// A project with a scripted `go` that prints a fixed module graph, `go.mod` and `GOWORK`.
    struct ScriptedGraph {
        #[expect(dead_code, reason = "dropping the guard deletes the project directory")]
        directory: tempfile::TempDir,
        project: Project,
        go: Go,
    }

    /// The scripted graph: the main module, a direct requirement, an indirect requirement, a
    /// module only the graph holds, and a requirement replaced by a fork.
    /// `go_directive` is the `go.mod`'s `go` line (JSON), and `gowork` what `go env GOWORK` prints.
    fn scripted_graph(go_directive: &str, gowork: &str) -> eyre::Result<ScriptedGraph> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let root = camino::Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
            .map_err(|path| eyre::eyre!("non-UTF-8 path {}", path.display()))?;
        std::fs::write(
            root.join("list.json"),
            indoc::indoc! {r#"
                {"Path": "example.com/demo", "Main": true}
                {"Path": "example.com/direct", "Version": "v1.0.0"}
                {"Path": "example.com/indirect", "Version": "v1.1.0", "Indirect": true}
                {"Path": "example.com/graph-only", "Version": "v0.21.0", "Indirect": true}
                {"Path": "example.com/forked", "Version": "v1.2.0",
                 "Replace": {"Path": "example.com/fork", "Version": "v1.2.1"}}
            "#},
        )?;
        std::fs::write(
            root.join("gomod.json"),
            format!(
                r#"{{
                    "Module": {{"Path": "example.com/demo"}},
                    "Go": {go_directive},
                    "Require": [
                        {{"Path": "example.com/direct", "Version": "v1.0.0"}},
                        {{"Path": "example.com/indirect", "Version": "v1.1.0", "Indirect": true}},
                        {{"Path": "example.com/forked", "Version": "v1.2.0"}}
                    ]
                }}"#
            ),
        )?;
        std::fs::write(root.join("gowork.txt"), gowork)?;
        let script = root.join("go");
        std::fs::write(
            &script,
            indoc::indoc! {r#"
                #!/bin/sh
                case "$1 $2" in
                  "list -m") cat list.json ;;
                  "mod edit") cat gomod.json ;;
                  "env GOWORK") cat gowork.txt ;;
                  "mod graph") ;;
                  *) exit 1 ;;
                esac
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let project = Project {
            root: root.clone(),
            kind: GO_ID,
            manifest: root.join("go.mod"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        Ok(ScriptedGraph {
            directory,
            project,
            go: Go::with_program(script.as_str()),
        })
    }

    async fn dependency_names(graph: &ScriptedGraph) -> eyre::Result<Vec<String>> {
        let deps = dependencies(&graph.go, None, &graph.project, DepScope::Graph).await?;
        Ok(deps.into_iter().map(|dep| dep.package.name).collect())
    }

    /// A pruned `go.mod` requires every module the build compiles, so a module only the graph
    /// holds is no dependency: nothing of it builds and `go mod tidy` drops any requirement on it.
    #[tokio::test]
    async fn a_module_go_mod_does_not_require_is_no_dependency() -> eyre::Result<()> {
        let graph = scripted_graph(r#""1.21""#, "\n")?;

        // The replaced requirement matches by its own path and is reported under the fork's.
        assert_eq!(
            dependency_names(&graph).await?,
            [
                "example.com/direct",
                "example.com/indirect",
                "example.com/fork"
            ]
        );
        Ok(())
    }

    /// Below `go 1.17`, and in workspace mode, one `go.mod` does not name every module the build
    /// compiles, so every module of the graph stays a dependency.
    #[tokio::test]
    async fn every_module_stays_when_go_mod_may_not_list_the_build() -> eyre::Result<()> {
        let all = [
            "example.com/direct",
            "example.com/indirect",
            "example.com/graph-only",
            "example.com/fork",
        ];
        for (go_directive, gowork) in [
            (r#""1.16""#, "\n"),
            ("null", "\n"),
            (r#""1.21""#, "/work/go.work\n"),
        ] {
            let graph = scripted_graph(go_directive, gowork)?;
            assert_eq!(
                dependency_names(&graph).await?,
                all,
                "go {go_directive}, GOWORK {gowork:?}"
            );
        }
        // `GOWORK=off` turns workspace mode off again.
        let graph = scripted_graph(r#""1.21""#, "off\n")?;
        assert!(
            !dependency_names(&graph)
                .await?
                .contains(&"example.com/graph-only".to_string())
        );
        Ok(())
    }
}
