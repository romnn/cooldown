//! `check` — the CI gate. Fail-closed: a stale lock, a too-fresh non-baselined pin, or any error
//! attributable to a dependency you couldn't evaluate forces a non-zero exit. Evaluates the
//! resolved graph (direct + transitive) by default.

use super::{age_days, diag_from_error, render_window, Exit, RunOpts, Workspace};
use cooldown_core::{
    check_pin, resolve, ArtifactScope, DepScope, Dependency, Diagnostic, DiagnosticKind, Origin,
    Resolution, ResolveKind, ResolveQuery, Status, TargetContext,
};
use cooldown_render as render;
use cooldown_render::tty::check_status_of;
use futures::stream::{self, StreamExt};

/// If a `Native`-origin layer declared a STRICTER (larger) bare window than the one that won, the
/// repo/global policy has weakened the project's stated intent. Returns the native window's days.
fn stricter_native_days(res: &Resolution) -> Option<f64> {
    let applied = res
        .trace
        .iter()
        .find(|s| s.field == "default" && s.applied)
        .and_then(|s| s.min_age_days)?;
    let native_max = res
        .trace
        .iter()
        .filter(|s| s.layer == Origin::Native && s.field == "default")
        .filter_map(|s| s.min_age_days)
        .fold(None, |acc: Option<f64>, d| {
            Some(acc.map_or(d, |a| a.max(d)))
        });
    match native_max {
        Some(n) if n > applied + 1e-9 => Some(n),
        _ => None,
    }
}

pub struct CheckOutcome {
    pub meta: render::CheckMeta,
    pub summary: render::CheckSummary,
    pub items: Vec<render::CheckItem>,
    pub warnings: Vec<Diagnostic>,
    pub errors: Vec<Diagnostic>,
    pub exit: Exit,
}

impl Workspace {
    pub async fn check(&self, opts: &RunOpts) -> CheckOutcome {
        let mut items: Vec<render::CheckItem> = Vec::new();
        let mut warnings: Vec<Diagnostic> = Vec::new();
        let mut errors: Vec<Diagnostic> = Vec::new();

        let mut checked = 0usize;
        let mut direct = 0usize;
        let mut exempt = 0usize;
        let mut acknowledged = 0usize;
        let mut unknown_age = 0usize;
        let mut violations = 0usize;
        let mut stricter_native_tripped = false;

        let scope = if opts.direct_only {
            DepScope::Direct
        } else {
            DepScope::Graph
        };

        for pctx in self.scoped_projects(opts) {
            let Some(adapter) = self.adapter(pctx.ecosystem) else {
                continue;
            };
            let project_label = pctx.rel_path.to_string();

            // Fail-closed lock-currency probe (unless --allow-stale-lock downgrades to a warning).
            match adapter.verify_lock_current(&pctx.project).await {
                Ok(v) if v.ok => {}
                Ok(v) => {
                    let diag = Diagnostic::new(DiagnosticKind::StaleLock, v.detail)
                        .with_ecosystem(pctx.ecosystem.as_str())
                        .with_project(&project_label)
                        .with_path(pctx.project.manifest.as_str());
                    if opts.allow_stale_lock {
                        warnings.push(diag);
                    } else {
                        errors.push(diag);
                        continue; // cannot soundly evaluate a stale lock
                    }
                }
                Err(e) => {
                    let diag = diag_from_error(&e, pctx.ecosystem, &project_label, None)
                        .with_path(pctx.project.manifest.as_str());
                    // `--allow-stale-lock` only downgrades a genuine stale/absent-lock probe; a
                    // tool/transient failure (e.g. the lock tool errored) stays fail-closed.
                    let downgradable = matches!(
                        diag.kind,
                        DiagnosticKind::StaleLock | DiagnosticKind::NotFound
                    );
                    if opts.allow_stale_lock && downgradable {
                        warnings.push(diag);
                    } else {
                        errors.push(diag);
                        continue;
                    }
                }
            }

            let deps = match adapter.dependencies(&pctx.project, scope).await {
                Ok(d) => d,
                Err(e) => {
                    errors.push(diag_from_error(&e, pctx.ecosystem, &project_label, None));
                    continue;
                }
            };
            let deps: Vec<Dependency> = deps
                .into_iter()
                .filter(|d| Self::package_in_scope(opts, &d.package.name))
                .collect();

            let tctx = TargetContext {
                project: &pctx.project,
                environments: &[],
                artifacts: if opts.all_artifacts {
                    ArtifactScope::All
                } else {
                    ArtifactScope::Environment
                },
            };

            let fetched: Vec<(Dependency, cooldown_core::Result<cooldown_core::Release>)> =
                stream::iter(deps)
                    .map(|dep| {
                        let tctx = &tctx;
                        async move {
                            let r = adapter.locked_release(&dep, tctx).await;
                            (dep, r)
                        }
                    })
                    .buffer_unordered(opts.fanout())
                    .collect()
                    .await;

            let rctx = self.resolve_ctx(pctx, opts);
            for (dep, result) in fetched {
                checked += 1;
                if dep.direct {
                    direct += 1;
                }
                let locked = match result {
                    Ok(l) => l,
                    Err(e) => {
                        // A failure attributable to one dependency → an item with status:"error".
                        let diag = diag_from_error(
                            &e,
                            pctx.ecosystem,
                            &project_label,
                            Some(&dep.package.name),
                        );
                        items.push(error_item(
                            &dep,
                            &project_label,
                            pctx.ecosystem.as_str(),
                            diag,
                        ));
                        continue;
                    }
                };
                if locked.yanked {
                    warnings.push(
                        Diagnostic::new(DiagnosticKind::Yanked, "locked version is yanked")
                            .with_ecosystem(pctx.ecosystem.as_str())
                            .with_project(&project_label)
                            .with_package(&dep.package.name)
                            .with_version(dep.current.as_str()),
                    );
                }

                let pv = check_pin(&dep, &locked, &pctx.policy.layers, &rctx, self.now);

                // Stricter-native: warn (or fail under strict-native) when repo/global policy
                // overrides a stricter declared native window.
                {
                    let q = ResolveQuery {
                        ecosystem: pctx.ecosystem,
                        package: &dep.package.name,
                        registry: dep.package.registry.as_deref(),
                        project: &pctx.rel_path,
                        kind: ResolveKind::CurrentPin,
                    };
                    let res = resolve(&pctx.policy.layers, &q, self.now);
                    if let Some(nd) = stricter_native_days(&res) {
                        warnings.push(
                            Diagnostic::new(
                                DiagnosticKind::StricterNative,
                                format!(
                                    "repo/global policy ({:.0}d) overrides a stricter native min-age ({:.0}d)",
                                    res.window.effective_min_age_days(self.now),
                                    nd
                                ),
                            )
                            .with_ecosystem(pctx.ecosystem.as_str())
                            .with_project(&project_label)
                            .with_package(&dep.package.name),
                        );
                        if pctx.policy.strict_native {
                            stricter_native_tripped = true;
                        }
                    }
                }

                if pv.status == Status::Exempt {
                    exempt += 1;
                    continue;
                }

                let is_ack = pv.status == Status::CurrentInCooldown
                    && self.baseline.is_acknowledged(
                        pctx.ecosystem.as_str(),
                        &project_label,
                        &dep.package.name,
                        dep.current.as_str(),
                        dep.package.registry.as_deref(),
                        self.now,
                    );

                let Some(status) = check_status_of(pv.status, is_ack) else {
                    continue; // mature pass → counted in `checked`, not a finding
                };

                match status {
                    render::CheckStatus::Violation => violations += 1,
                    render::CheckStatus::Acknowledged => acknowledged += 1,
                    render::CheckStatus::UnknownAge => unknown_age += 1,
                    render::CheckStatus::Error => {}
                }

                items.push(render::CheckItem {
                    name: dep.package.name.clone(),
                    ecosystem: pctx.ecosystem.as_str().to_string(),
                    project: project_label.clone(),
                    registry: dep.package.registry.clone(),
                    direct: dep.direct,
                    current: dep.current.to_string(),
                    published_at: pv.published_at.map(|p| p.to_string()),
                    age_days: pv.published_at.map(|p| age_days(p, self.now)),
                    window: render_window(&pv.window, self.now),
                    status,
                    graph_held: pv.graph_held,
                    graph_floor: pv.graph_floor.map(|v| v.to_string()),
                    error: None,
                });
            }
        }

        let err_count = items.iter().filter(|i| i.error.is_some()).count() + errors.len();
        // Fail-closed: any violation, any error item, or any top-level error → non-zero.
        let unknown_fail = opts.fail_on_unknown_age && unknown_age > 0;
        let exit = if err_count > 0 || unknown_fail {
            Exit::Environment
        } else if stricter_native_tripped {
            Exit::Usage // --fail-on-stricter-native / strict-native tripped
        } else if violations > 0 {
            Exit::Policy
        } else {
            Exit::Ok
        };

        let summary = render::CheckSummary {
            checked,
            direct,
            exempt,
            acknowledged,
            unknown_age,
            errors: err_count,
            violations,
        };
        let meta = render::CheckMeta {
            scope: if scope == DepScope::Graph {
                "lockfile-graph".into()
            } else {
                "direct-only".into()
            },
            artifact_scope: if opts.all_artifacts {
                "all".into()
            } else {
                "environment".into()
            },
        };

        CheckOutcome {
            meta,
            summary,
            items,
            warnings,
            errors,
            exit,
        }
    }
}

fn error_item(
    dep: &Dependency,
    project: &str,
    ecosystem: &str,
    diag: Diagnostic,
) -> render::CheckItem {
    render::CheckItem {
        name: dep.package.name.clone(),
        ecosystem: ecosystem.to_string(),
        project: project.to_string(),
        registry: dep.package.registry.clone(),
        direct: dep.direct,
        current: dep.current.to_string(),
        published_at: None,
        age_days: None,
        window: render::Window {
            min_age_days: 0.0,
            source: "n/a".into(),
            clamped_by: None,
        },
        status: render::CheckStatus::Error,
        graph_held: false,
        graph_floor: None,
        error: Some(diag),
    }
}
