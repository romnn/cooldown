//! `outdated` — what could update, split into "adoptable now" vs "in cooldown". Reasons over the
//! candidate set; informational, so per-dep failures never change the exit code.

use super::{Exit, RunOpts, Workspace, age_days, diag_from_error, render_window};
use cooldown_core::{
    ArtifactScope, DepScope, Dependency, Diagnostic, Release, ResolveKind, ResolveQuery,
    TargetContext, evaluate, resolve,
};
use cooldown_render as render;
use futures::stream::{self, StreamExt};

pub struct OutdatedOutcome {
    pub summary: render::OutdatedSummary,
    pub items: Vec<render::OutdatedItem>,
    pub warnings: Vec<Diagnostic>,
    pub errors: Vec<Diagnostic>,
    pub exit: Exit,
}

impl Workspace {
    pub async fn outdated(&self, opts: &RunOpts) -> OutdatedOutcome {
        let mut items: Vec<render::OutdatedItem> = Vec::new();
        let mut warnings: Vec<Diagnostic> = Vec::new();
        let mut errors: Vec<Diagnostic> = Vec::new();

        let scope = if opts.include_indirect && !opts.direct_only {
            DepScope::Graph
        } else {
            DepScope::Direct
        };

        for pctx in self.scoped_projects(opts) {
            let Some(adapter) = self.adapter(pctx.ecosystem) else {
                continue;
            };
            let project_label = pctx.rel_path.to_string();

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

            let fetched: Vec<(Dependency, cooldown_core::Result<Vec<Release>>)> =
                stream::iter(deps)
                    .map(|dep| {
                        let tctx = &tctx;
                        async move {
                            let r = adapter.releases(&dep, tctx).await;
                            (dep, r)
                        }
                    })
                    .buffer_unordered(opts.fanout())
                    .collect()
                    .await;

            let rctx = self.resolve_ctx(pctx, opts);
            for (dep, result) in fetched {
                match result {
                    Ok(releases) => {
                        let verdict =
                            evaluate(&dep, &releases, &pctx.policy.layers, &rctx, self.now);

                        let window = match verdict.candidates.last() {
                            Some(c) => render_window(&c.window, self.now),
                            None => {
                                let q = ResolveQuery {
                                    ecosystem: pctx.ecosystem,
                                    package: &dep.package.name,
                                    registry: dep.package.registry.as_deref(),
                                    project: &pctx.rel_path,
                                    kind: ResolveKind::CurrentPin,
                                };
                                render_window(
                                    &resolve(&pctx.policy.layers, &q, self.now).window,
                                    self.now,
                                )
                            }
                        };

                        let latest = verdict.latest.as_ref().map(|lv| {
                            let published = releases
                                .iter()
                                .find(|r| &r.version == lv)
                                .and_then(|r| r.published_at);
                            render::LatestInfo {
                                version: lv.to_string(),
                                published_at: published.map(|p| p.to_string()),
                                age_days: published.map(|p| age_days(p, self.now)),
                            }
                        });

                        // Surface a yanked locked version as a warning.
                        if releases
                            .iter()
                            .any(|r| r.version == dep.current && r.yanked)
                        {
                            warnings.push(
                                Diagnostic::new(
                                    cooldown_core::DiagnosticKind::Yanked,
                                    "locked version is yanked",
                                )
                                .with_ecosystem(pctx.ecosystem.as_str())
                                .with_project(&project_label)
                                .with_package(&dep.package.name)
                                .with_version(dep.current.as_str()),
                            );
                        }

                        items.push(render::OutdatedItem {
                            name: dep.package.name.clone(),
                            ecosystem: pctx.ecosystem.as_str().to_string(),
                            project: project_label.clone(),
                            registry: dep.package.registry.clone(),
                            direct: dep.direct,
                            current: dep.current.to_string(),
                            window,
                            status: verdict.status.into(),
                            adoptable_target: verdict.adoptable_target.map(|v| v.to_string()),
                            latest,
                            error: None,
                        });
                    }
                    Err(e) => {
                        let diag = diag_from_error(
                            &e,
                            pctx.ecosystem,
                            &project_label,
                            Some(&dep.package.name),
                        );
                        items.push(render::OutdatedItem {
                            name: dep.package.name.clone(),
                            ecosystem: pctx.ecosystem.as_str().to_string(),
                            project: project_label.clone(),
                            registry: dep.package.registry.clone(),
                            direct: dep.direct,
                            current: dep.current.to_string(),
                            window: render::Window {
                                min_age_days: 0.0,
                                source: "n/a".into(),
                                clamped_by: None,
                            },
                            status: render::OutdatedStatus::Error,
                            adoptable_target: None,
                            latest: None,
                            error: Some(diag),
                        });
                    }
                }
            }
        }

        let summary = summarize(&items);
        OutdatedOutcome {
            summary,
            items,
            warnings,
            errors,
            exit: Exit::Ok,
        }
    }
}

fn summarize(items: &[render::OutdatedItem]) -> render::OutdatedSummary {
    let mut s = render::OutdatedSummary {
        total: items.len(),
        adoptable: 0,
        in_cooldown: 0,
        up_to_date: 0,
        exempt: 0,
        held: 0,
        unknown_age: 0,
        errors: 0,
    };
    for it in items {
        match it.status {
            render::OutdatedStatus::Adoptable => s.adoptable += 1,
            render::OutdatedStatus::InCooldown => s.in_cooldown += 1,
            render::OutdatedStatus::UpToDate => s.up_to_date += 1,
            render::OutdatedStatus::Exempt => s.exempt += 1,
            render::OutdatedStatus::Held => s.held += 1,
            render::OutdatedStatus::UnknownAge => s.unknown_age += 1,
            render::OutdatedStatus::Error => s.errors += 1,
            render::OutdatedStatus::CurrentInCooldown => s.in_cooldown += 1,
        }
    }
    s
}
