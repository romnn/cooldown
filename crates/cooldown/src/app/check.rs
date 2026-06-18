//! `check` — the CI gate. Fail-closed: a stale lock, a too-fresh non-baselined pin, or any error
//! attributable to a dependency you couldn't evaluate forces a non-zero exit. Evaluates the
//! resolved graph (direct + transitive) by default.

use super::{Exit, RunOpts, Workspace, age_days, diag_from_error, render_window};
use cooldown_core::{
    ArtifactScope, DepScope, Dependency, Diagnostic, DiagnosticKind, Origin, Resolution,
    ResolveKind, ResolveQuery, Status, TargetContext, check_pin, resolve,
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

/// The result of `check`: the gate verdict, the findings, and the exit code that encodes it.
pub struct CheckOutcome {
    /// The scope of the evaluation (graph vs direct-only, environment vs all artifacts).
    pub meta: render::CheckMeta,
    /// Per-status counts across all evaluated pins.
    pub summary: render::CheckSummary,
    /// The findings: violations, acknowledged pins, unknown-age pins, and per-dependency errors.
    pub items: Vec<render::CheckItem>,
    /// Non-fatal diagnostics (stale lock under `--allow-stale-lock`, yanked pins, stricter-native).
    pub warnings: Vec<Diagnostic>,
    /// Project-level errors that abort evaluation of that project.
    pub errors: Vec<Diagnostic>,
    /// The gate verdict as a process exit; see [`Exit`].
    pub exit: Exit,
}

/// The mutable state accumulated while gating a run: the per-status tallies, the findings, and the
/// non-fatal diagnostics. Finalized into a [`CheckOutcome`].
#[derive(Default)]
struct CheckAccum {
    checked: usize,
    direct: usize,
    exempt: usize,
    acknowledged: usize,
    unknown_age: usize,
    violations: usize,
    /// Set when a stricter-native override tripped under `strict-native`.
    stricter_native_tripped: bool,
    items: Vec<render::CheckItem>,
    warnings: Vec<Diagnostic>,
    errors: Vec<Diagnostic>,
}

/// The outcome of the fail-closed lock-currency probe: continue evaluating, or skip this project.
enum LockProbe {
    /// The lock is current (or a stale lock was downgraded to a warning); continue.
    Continue,
    /// The lock could not be soundly evaluated; this project is skipped.
    Skip,
}

impl Workspace {
    /// Gate the resolved graph: exit non-zero if anything is younger than its cooldown (the CI
    /// gate).
    ///
    /// Fail-closed: a stale lock, a per-dependency evaluation error, or (under
    /// `--fail-on-unknown-age`) an unknown publish time forces a non-zero [`Exit`]. Evaluates the
    /// full graph by default, or direct deps under `--direct-only`.
    pub async fn check(&self, opts: &RunOpts) -> CheckOutcome {
        let mut acc = CheckAccum::default();

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

            match self
                .probe_lock(adapter, pctx, opts, &project_label, &mut acc)
                .await
            {
                LockProbe::Continue => {}
                LockProbe::Skip => continue,
            }

            let deps = match adapter.dependencies(&pctx.project, scope).await {
                Ok(d) => d,
                Err(e) => {
                    acc.errors
                        .push(diag_from_error(&e, pctx.ecosystem, &project_label, None));
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

            let rctx = Self::resolve_ctx(pctx, opts);
            for (dep, result) in fetched {
                self.gate_pin(pctx, &project_label, &dep, result, &rctx, &mut acc);
            }
        }

        let err_count = acc.items.iter().filter(|i| i.error.is_some()).count() + acc.errors.len();
        let summary = render::CheckSummary {
            checked: acc.checked,
            direct: acc.direct,
            exempt: acc.exempt,
            acknowledged: acc.acknowledged,
            unknown_age: acc.unknown_age,
            errors: err_count,
            violations: acc.violations,
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
        let exit = check_exit(&acc, err_count, opts);

        CheckOutcome {
            meta,
            summary,
            items: acc.items,
            warnings: acc.warnings,
            errors: acc.errors,
            exit,
        }
    }

    /// Run the fail-closed lock-currency probe for one project, recording any diagnostic.
    ///
    /// `--allow-stale-lock` downgrades a genuine stale/absent-lock result to a warning; a
    /// tool/transient failure stays fail-closed.
    async fn probe_lock(
        &self,
        adapter: &dyn cooldown_core::Ecosystem,
        pctx: &super::ProjectCtx,
        opts: &RunOpts,
        project_label: &str,
        acc: &mut CheckAccum,
    ) -> LockProbe {
        match adapter.verify_lock_current(&pctx.project).await {
            Ok(v) if v.ok => LockProbe::Continue,
            Ok(v) => {
                let diag = Diagnostic::new(DiagnosticKind::StaleLock, v.detail)
                    .with_ecosystem(pctx.ecosystem.as_str())
                    .with_project(project_label)
                    .with_path(pctx.project.manifest.as_str());
                if opts.allow_stale_lock {
                    acc.warnings.push(diag);
                    LockProbe::Continue
                } else {
                    acc.errors.push(diag);
                    LockProbe::Skip // cannot soundly evaluate a stale lock
                }
            }
            Err(e) => {
                let diag = diag_from_error(&e, pctx.ecosystem, project_label, None)
                    .with_path(pctx.project.manifest.as_str());
                let downgradable = matches!(
                    diag.kind,
                    DiagnosticKind::StaleLock | DiagnosticKind::NotFound
                );
                if opts.allow_stale_lock && downgradable {
                    acc.warnings.push(diag);
                    LockProbe::Continue
                } else {
                    acc.errors.push(diag);
                    LockProbe::Skip
                }
            }
        }
    }

    /// Evaluate one fetched pin: tally it, emit any finding, and surface yanked/stricter-native
    /// warnings.
    fn gate_pin(
        &self,
        pctx: &super::ProjectCtx,
        project_label: &str,
        dep: &Dependency,
        result: cooldown_core::Result<cooldown_core::Release>,
        rctx: &cooldown_core::ResolveContext<'_>,
        acc: &mut CheckAccum,
    ) {
        acc.checked += 1;
        if dep.direct {
            acc.direct += 1;
        }
        let locked = match result {
            Ok(l) => l,
            Err(e) => {
                // A failure attributable to one dependency → an item with status:"error".
                let diag =
                    diag_from_error(&e, pctx.ecosystem, project_label, Some(&dep.package.name));
                acc.items.push(error_item(
                    dep,
                    project_label,
                    pctx.ecosystem.as_str(),
                    diag,
                ));
                return;
            }
        };
        if locked.yanked {
            acc.warnings.push(
                Diagnostic::new(DiagnosticKind::Yanked, "locked version is yanked")
                    .with_ecosystem(pctx.ecosystem.as_str())
                    .with_project(project_label)
                    .with_package(&dep.package.name)
                    .with_version(dep.current.as_str()),
            );
        }

        let pv = check_pin(dep, &locked, &pctx.policy.layers, rctx, self.now);
        if let Some(diag) = self.stricter_native_warning(pctx, project_label, dep) {
            acc.warnings.push(diag);
            if pctx.policy.strict_native {
                acc.stricter_native_tripped = true;
            }
        }

        if pv.status == Status::Exempt {
            acc.exempt += 1;
            return;
        }

        let is_ack = pv.status == Status::CurrentInCooldown
            && self.baseline.is_acknowledged(
                pctx.ecosystem.as_str(),
                project_label,
                &dep.package.name,
                dep.current.as_str(),
                dep.package.registry.as_deref(),
                self.now,
            );

        let Some(status) = check_status_of(pv.status, is_ack) else {
            return; // mature pass → counted in `checked`, not a finding
        };

        match status {
            render::CheckStatus::Violation => acc.violations += 1,
            render::CheckStatus::Acknowledged => acc.acknowledged += 1,
            render::CheckStatus::UnknownAge => acc.unknown_age += 1,
            render::CheckStatus::Error => {}
        }

        acc.items.push(render::CheckItem {
            name: dep.package.name.clone(),
            ecosystem: pctx.ecosystem.as_str().to_string(),
            project: project_label.to_string(),
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

    /// The stricter-native diagnostic for a pin, when repo/global policy overrides a stricter
    /// declared native window. Returns `None` when no native layer is stricter.
    fn stricter_native_warning(
        &self,
        pctx: &super::ProjectCtx,
        project_label: &str,
        dep: &Dependency,
    ) -> Option<Diagnostic> {
        let q = ResolveQuery {
            ecosystem: pctx.ecosystem,
            package: &dep.package.name,
            registry: dep.package.registry.as_deref(),
            project: &pctx.rel_path,
            kind: ResolveKind::CurrentPin,
        };
        let res = resolve(&pctx.policy.layers, &q, self.now);
        let native_days = stricter_native_days(&res)?;
        Some(
            Diagnostic::new(
                DiagnosticKind::StricterNative,
                format!(
                    "repo/global policy ({:.0}d) overrides a stricter native min-age ({:.0}d)",
                    res.window.effective_min_age_days(self.now),
                    native_days
                ),
            )
            .with_ecosystem(pctx.ecosystem.as_str())
            .with_project(project_label)
            .with_package(&dep.package.name),
        )
    }
}

/// Map the tallies to the fail-closed exit code: errors/unknown-age first, then a tripped
/// stricter-native gate, then any violation.
fn check_exit(acc: &CheckAccum, err_count: usize, opts: &RunOpts) -> Exit {
    let unknown_fail = opts.fail_on_unknown_age && acc.unknown_age > 0;
    if err_count > 0 || unknown_fail {
        Exit::Environment
    } else if acc.stricter_native_tripped {
        Exit::Usage // --fail-on-stricter-native / strict-native tripped
    } else if acc.violations > 0 {
        Exit::Policy
    } else {
        Exit::Ok
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
