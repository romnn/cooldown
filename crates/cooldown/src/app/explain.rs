//! `explain <pkg>` — the decision for a package (the `outdated` verdict with the reason behind a
//! blocked one, and every member's declaration) beside the field-by-field derivation of its window
//! (every layer and rule that applied), and `config` — the fully-resolved policy with the origin
//! of each value. Together they keep the override system from being a black box.

use super::{
    ConfigItem, ConfigSummary, EffectiveInfo, Exit, ExplainMeta, ExplainStep, ProjectCtx, RunOpts,
    Workspace, round2,
};
use cooldown_core::{Declaration, DepScope, Diagnostic, ResolveKind, ResolveQuery, resolve};

/// The result of `explain <pkg>`: the package's verdict and declarations, its effective window,
/// and the ordered derivation steps.
#[derive(Debug)]
pub struct ExplainOutcome {
    /// The verdict, the declarations, the resolved window, and the project/registry it was
    /// derived for.
    pub meta: ExplainMeta,
    /// Each layer-and-rule step that contributed to (or was shadowed in) the derivation.
    pub steps: Vec<ExplainStep>,
    /// Non-fatal diagnostics the verdict's evaluation raised (a yanked pin, a preview note).
    pub warnings: Vec<Diagnostic>,
    /// Errors the verdict's evaluation raised; the window trace stands regardless, so they do not
    /// change the exit.
    pub errors: Vec<Diagnostic>,
    /// The process exit (`Ok`, or `NoTool` when no project is in scope).
    pub exit: Exit,
}

/// The result of `config`: the fully-resolved policy per project as typed data.
pub struct ConfigOutcome {
    /// The aggregate project count.
    pub summary: ConfigSummary,
    /// One resolved policy row per project.
    pub items: Vec<ConfigItem>,
    /// The process exit (always `Ok`).
    pub exit: Exit,
}

struct ExplainService<'a> {
    ws: &'a Workspace,
    opts: &'a RunOpts,
}

impl Workspace {
    /// Explains `pkg` in the first in-scope project: the `outdated` verdict (with the reason behind
    /// a blocked one), every member's declaration, and the window's derivation.
    ///
    /// A resolved dependency's registry participates in registry-scoped rules.
    /// Missing dependency data falls back to registry-less resolution, but a conflicting project
    /// access lease or pending mutation fails closed.
    ///
    /// # Errors
    ///
    /// Returns a project-access error when package state cannot be read consistently.
    pub async fn explain(
        &self,
        pkg: &str,
        opts: &RunOpts,
    ) -> cooldown_core::Result<ExplainOutcome> {
        ExplainService::new(self, opts).explain(pkg).await
    }

    /// The fully-resolved config per project (effective default window + provenance + strict-native).
    #[must_use]
    pub fn config(&self, opts: &RunOpts) -> ConfigOutcome {
        ExplainService::new(self, opts).config()
    }
}

impl<'a> ExplainService<'a> {
    fn new(ws: &'a Workspace, opts: &'a RunOpts) -> Self {
        ExplainService { ws, opts }
    }

    async fn explain(&self, pkg: &str) -> cooldown_core::Result<ExplainOutcome> {
        let Some(pctx) = self.ws.scoped_projects(self.opts).next() else {
            return Ok(ExplainOutcome {
                meta: empty_meta(),
                steps: Vec::new(),
                warnings: Vec::new(),
                errors: Vec::new(),
                exit: Exit::NoTool,
            });
        };

        let _progress = self
            .opts
            .progress
            .project(pctx.tool, pctx.rel_path.as_str());
        self.opts
            .progress
            .phase(format!("resolving dependency context for {pkg}"));
        let ExplainedDependency {
            registry,
            excluded_members,
            declarations,
        } = self.dependency_context(pctx, pkg).await?;
        // The decision itself, after the read guard is released: the verdict runs `outdated`'s
        // evaluation for this one package, whose upgrade-policy preview takes its own guard.
        let verdict = self.ws.package_verdict(pctx, self.opts, pkg).await?;
        let q = ResolveQuery {
            tool: pctx.tool,
            package: pkg,
            registry: registry.as_deref(),
            project: &pctx.rel_path,
            kind: ResolveKind::CurrentPin,
        };
        let res = resolve(&pctx.policy.layers, &q, self.ws.now());

        // The `[advisories]` policy steps join the trace when the feed is configured anywhere
        // in the stack, so the security window's provenance is as auditable as the ordinary
        // one's.
        let advisory_policy = cooldown_core::resolve_advisory_policy(&pctx.policy.layers);
        let steps = res
            .trace
            .iter()
            .chain(advisory_policy.trace.iter())
            .map(|step| ExplainStep {
                layer: step.layer.token(),
                field: step.field.clone(),
                selector: step
                    .selector
                    .as_ref()
                    .and_then(cooldown_core::Selector::token),
                min_age_days: step.min_age_days.map(round2),
                applied: step.applied,
                note: step.note.clone(),
            })
            .collect();

        let declarations = declarations
            .into_iter()
            .map(|declaration| {
                let excluded = excluded_members
                    .iter()
                    .any(|member| member.path == declaration.member.path);
                super::ExplainDeclaration {
                    member: declaration.member,
                    range: declaration.range,
                    resolved: declaration.resolved,
                    fields: declaration.fields,
                    excluded,
                }
            })
            .collect();
        let meta = ExplainMeta {
            project: pctx.rel_path.to_string(),
            registry,
            effective: EffectiveInfo {
                min_age_days: round2(res.window.effective_min_age_days(self.ws.now())),
                decided_by: res.window.source(),
            },
            excluded_members,
            verdicts: verdict.items,
            declarations,
        };

        Ok(ExplainOutcome {
            meta,
            steps,
            warnings: verdict.warnings,
            errors: verdict.errors,
            exit: Exit::Ok,
        })
    }

    fn config(&self) -> ConfigOutcome {
        let mut items: Vec<ConfigItem> = Vec::new();
        for pctx in self.ws.scoped_projects(self.opts) {
            let _progress = self
                .opts
                .progress
                .project(pctx.tool, pctx.rel_path.as_str());
            self.opts.progress.phase("resolving effective policy");
            let q = ResolveQuery {
                tool: pctx.tool,
                package: "",
                registry: None,
                project: &pctx.rel_path,
                kind: ResolveKind::EffectiveDefault,
            };
            let res = resolve(&pctx.policy.layers, &q, self.ws.now());
            let days = round2(res.window.effective_min_age_days(self.ws.now()));
            let layers: Vec<String> = pctx
                .policy
                .layers
                .iter()
                .map(|layer| layer.origin.token())
                .collect();

            items.push(ConfigItem {
                project: pctx.rel_path.to_string(),
                tool: pctx.tool.as_str().to_string(),
                effective_default_min_age_days: days,
                source: res.window.source(),
                strict_native: pctx.policy.strict_native,
                layers,
                advisories: self.advisory_config(pctx),
            });
        }

        ConfigOutcome {
            summary: ConfigSummary {
                projects: items.len(),
            },
            items,
            exit: Exit::Ok,
        }
    }

    /// The resolved `[advisories]` policy plus the project tool's feed coverage.
    ///
    /// Coverage is reported here rather than only as a run-time warning: a project whose tool
    /// has no safe project-wide database mapping can enable the feed and see nothing happen, and
    /// `config` is where one looks to find out why.
    fn advisory_config(&self, pctx: &ProjectCtx) -> crate::app::AdvisoryConfigInfo {
        let policy = cooldown_core::resolve_advisory_policy(&pctx.policy.layers);
        crate::app::AdvisoryConfigInfo {
            enabled: policy.enabled,
            source: policy.source,
            mode: policy.mode,
            min_age_days: round2(cooldown_core::duration::duration_as_days(policy.min_age)),
            severity: policy.severity,
            ecosystem: self
                .ws
                .adapter(pctx.tool)
                .and_then(|adapter| adapter.capabilities().advisory_ecosystem)
                .map(ToString::to_string),
        }
    }

    /// What the project's dependency graph says about `pkg`, if it is a known dependency: the
    /// registry it resolves to, the declaring members the run's scope and exclude policy ignore
    /// — the declarations that no longer count as workspace evidence, which the trace must show
    /// rather than drop silently — and every member's declaration as the adapter reads it.
    async fn dependency_context(
        &self,
        pctx: &ProjectCtx,
        pkg: &str,
    ) -> cooldown_core::Result<ExplainedDependency> {
        let Some(adapter) = self.ws.adapter(pctx.tool) else {
            return Ok(ExplainedDependency::default());
        };
        let _guard = self.ws.project_read_guard(pctx).await?;
        // The raw graph on purpose: this finds one package by name (never displayed and not list
        // output), so `exclude`/`-p` scoping would only hide the target — the exclusions are
        // reported beside it instead.
        let deps = match adapter.dependencies(&pctx.project, DepScope::Graph).await {
            Ok(deps) => deps,
            Err(
                error @ (cooldown_core::CoreError::StaleLock(_)
                | cooldown_core::CoreError::LockConflict(_)),
            ) => return Err(error),
            Err(_) => return Ok(ExplainedDependency::default()),
        };
        Ok(ExplainedDependency {
            registry: deps
                .iter()
                .find(|dep| dep.package.name == pkg)
                .and_then(|dep| dep.package.registry.clone()),
            excluded_members: Workspace::excluded_members_of(pctx, self.opts, &deps, pkg),
            declarations: adapter.declarations(&pctx.project, pkg).await?,
        })
    }
}

/// The dependency-graph context of the explained package (see
/// [`ExplainService::dependency_context`]).
#[derive(Default)]
struct ExplainedDependency {
    registry: Option<String>,
    excluded_members: Vec<cooldown_core::MemberRef>,
    declarations: Vec<Declaration>,
}

fn empty_meta() -> ExplainMeta {
    ExplainMeta {
        project: String::new(),
        registry: None,
        effective: EffectiveInfo {
            min_age_days: 0.0,
            decided_by: "default".into(),
        },
        excluded_members: Vec::new(),
        verdicts: Vec::new(),
        declarations: Vec::new(),
    }
}
