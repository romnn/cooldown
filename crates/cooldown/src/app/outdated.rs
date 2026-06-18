//! `outdated` — what could update, split into "adoptable now" vs "in cooldown". Reasons over the
//! candidate set; informational, so per-dep failures never change the exit code.

use super::{Exit, RunOpts, Workspace, age_days, diag_from_error, render_window};
use cooldown_core::{
    DepScope, Dependency, Diagnostic, Release, ResolveKind, ResolveQuery, evaluate, resolve,
};
use cooldown_render as render;
use futures::stream::{self, StreamExt};

/// The result of `outdated`: every reported dependency split by status, plus diagnostics.
///
/// `outdated` is informational, so per-dependency failures become `Error` items rather than
/// changing the exit code; [`exit`](Self::exit) is therefore always [`Exit::Ok`].
pub struct OutdatedOutcome {
    /// Per-status counts across all reported items.
    pub summary: render::OutdatedSummary,
    /// One entry per in-scope dependency (including any that failed to evaluate).
    pub items: Vec<render::OutdatedItem>,
    /// Non-fatal diagnostics not attributable to a single dependency.
    pub warnings: Vec<Diagnostic>,
    /// Project-level errors (e.g. a dependency graph that could not be enumerated).
    pub errors: Vec<Diagnostic>,
    /// Always [`Exit::Ok`]: this command never gates.
    pub exit: Exit,
}

impl Workspace {
    /// Report what could update, split into "adoptable now" vs "in cooldown".
    ///
    /// Scopes to direct deps unless `--include-indirect` is set (and `--direct-only` is not), and
    /// to packages matching `--package`. Surfaces a yanked locked version as a warning.
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

            let deps = match self.dependencies_in_scope(adapter, pctx, scope, opts).await {
                Ok(d) => d,
                Err(e) => {
                    errors.push(diag_from_error(&e, pctx.ecosystem, &project_label, None));
                    continue;
                }
            };
            let fctx = Self::fetch_context(pctx, opts);

            let fetched: Vec<(Dependency, cooldown_core::Result<Vec<Release>>)> =
                stream::iter(deps)
                    .map(|dep| {
                        let fctx = &fctx;
                        let candidate_scope = opts.candidate_scope();
                        async move {
                            let r = adapter.releases(&dep, fctx, candidate_scope).await;
                            (dep, r)
                        }
                    })
                    .buffer_unordered(opts.fanout())
                    .collect()
                    .await;

            let rctx = Self::resolve_ctx(pctx, opts);
            for (dep, result) in fetched {
                match result {
                    Ok(releases) => {
                        if is_yanked_locked(&releases, &dep) {
                            warnings.push(yanked_warning(pctx, &project_label, &dep));
                        }
                        items.push(self.outdated_item(
                            pctx,
                            &project_label,
                            &dep,
                            &releases,
                            &rctx,
                        ));
                    }
                    Err(e) => {
                        let diag = diag_from_error(
                            &e,
                            pctx.ecosystem,
                            &project_label,
                            Some(&dep.package.name),
                        );
                        items.push(error_item(pctx, &project_label, &dep, diag));
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

    /// Build the report item for a dependency whose releases were fetched successfully.
    fn outdated_item(
        &self,
        pctx: &super::ProjectCtx,
        project_label: &str,
        dep: &Dependency,
        releases: &[Release],
        rctx: &cooldown_core::ResolveContext<'_>,
    ) -> render::OutdatedItem {
        let verdict = evaluate(dep, releases, &pctx.policy.layers, rctx, self.now);

        // Prefer a candidate's window; fall back to a current-pin resolution when there is none.
        let window = if let Some(c) = verdict.candidates.last() {
            render_window(&c.window, self.now)
        } else {
            let q = ResolveQuery {
                ecosystem: pctx.ecosystem,
                package: &dep.package.name,
                registry: dep.package.registry.as_deref(),
                project: &pctx.rel_path,
                kind: ResolveKind::CurrentPin,
            };
            render_window(&resolve(&pctx.policy.layers, &q, self.now).window, self.now)
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

        render::OutdatedItem {
            name: dep.package.name.clone(),
            ecosystem: pctx.ecosystem.as_str().to_string(),
            project: project_label.to_string(),
            registry: dep.package.registry.clone(),
            direct: dep.direct,
            current: dep.current.to_string(),
            window,
            status: verdict.status.into(),
            adoptable_target: verdict.adoptable_target.map(|v| v.to_string()),
            latest,
            error: None,
        }
    }
}

/// Whether the dependency's currently-locked version is marked yanked among `releases`.
fn is_yanked_locked(releases: &[Release], dep: &Dependency) -> bool {
    releases
        .iter()
        .any(|r| r.version == dep.current && r.yanked)
}

fn yanked_warning(pctx: &super::ProjectCtx, project_label: &str, dep: &Dependency) -> Diagnostic {
    Diagnostic::new(
        cooldown_core::DiagnosticKind::Yanked,
        "locked version is yanked",
    )
    .with_ecosystem(pctx.ecosystem.as_str())
    .with_project(project_label)
    .with_package(&dep.package.name)
    .with_version(dep.current.as_str())
}

fn error_item(
    pctx: &super::ProjectCtx,
    project_label: &str,
    dep: &Dependency,
    diag: Diagnostic,
) -> render::OutdatedItem {
    render::OutdatedItem {
        name: dep.package.name.clone(),
        ecosystem: pctx.ecosystem.as_str().to_string(),
        project: project_label.to_string(),
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
            render::OutdatedStatus::InCooldown | render::OutdatedStatus::CurrentInCooldown => {
                s.in_cooldown += 1;
            }
            render::OutdatedStatus::UpToDate => s.up_to_date += 1,
            render::OutdatedStatus::Exempt => s.exempt += 1,
            render::OutdatedStatus::Held => s.held += 1,
            render::OutdatedStatus::UnknownAge => s.unknown_age += 1,
            render::OutdatedStatus::Error => s.errors += 1,
        }
    }
    s
}
