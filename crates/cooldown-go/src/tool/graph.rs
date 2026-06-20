use super::GO_ID;
use super::releases::{classify_quality, major_key_for_path};
use crate::gocmd::{Go, GoModule};
use crate::proxy::GoProxy;
use cooldown_core::{
    DepScope, Dependency, PackageId, PackageRegistry, Project, Release, ReleaseOrder, Result,
    VerifyReport, Version,
};
use std::collections::HashMap;

pub(super) async fn dependencies(
    go: &Go,
    registry: Option<String>,
    project: &Project,
    scope: DepScope,
) -> Result<Vec<Dependency>> {
    let modules = go.list_modules(&project.root).await?;
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

pub(super) async fn locked_release(proxy: &GoProxy, dep: &Dependency) -> Result<Release> {
    let time = proxy.published_at(&dep.package, &dep.current, &[]).await?;
    Ok(Release {
        version: dep.current.clone(),
        order: ReleaseOrder(Vec::new()),
        major: major_key_for_path(&dep.package.name),
        kind_from_current: None,
        published_at: time,
        yanked: false,
        quality: dep.current_quality,
    })
}

pub(super) async fn verify_lock_current(go: &Go, project: &Project) -> Result<VerifyReport> {
    match go.mod_tidy_is_clean(&project.root).await {
        Ok(true) => Ok(VerifyReport {
            ok: true,
            detail: "go.mod/go.sum are tidy".to_string(),
        }),
        Ok(false) => Ok(VerifyReport {
            ok: false,
            detail: "go.mod/go.sum are stale; run `go mod tidy`".to_string(),
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
        package: PackageId::new(GO_ID, path, registry.map(str::to_owned)),
        current: Version::new(version.clone()),
        current_quality: classify_quality(&version),
        direct: !module.indirect,
        artifacts: Vec::new(),
        graph_floor,
        members: Vec::new(),
        pinned: false,
    })
}
