//! `explain <pkg>` — the field-by-field derivation of a package's window (every layer and rule
//! that applied), and `config` — the fully-resolved policy with the origin of each value. Together
//! they keep the override system from being a black box.

use super::{
    ConfigItem, ConfigSummary, EffectiveInfo, Exit, ExplainMeta, ExplainStep, ProjectCtx, RunOpts,
    Workspace, round2,
};
use cooldown_core::{DepScope, ResolveKind, ResolveQuery, resolve};

/// The result of `explain <pkg>`: the package's effective window plus the ordered derivation steps.
pub struct ExplainOutcome {
    /// The resolved window and the project/registry it was derived for.
    pub meta: ExplainMeta,
    /// Each layer-and-rule step that contributed to (or was shadowed in) the derivation.
    pub steps: Vec<ExplainStep>,
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
    /// Explain the window for `pkg` in the first in-scope project. If `pkg` is a resolved
    /// dependency, its registry is looked up from the dependency graph so that registry-scoped
    /// rules (`[registry."…"]`) participate in the derivation. The lookup is best-effort: a missing
    /// lock or a tool failure falls back to a registry-less resolution, so `explain` still answers
    /// for a package that is not (yet) a dependency.
    pub async fn explain(&self, pkg: &str, opts: &RunOpts) -> ExplainOutcome {
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

    async fn explain(&self, pkg: &str) -> ExplainOutcome {
        let Some(pctx) = self.ws.scoped_projects(self.opts).next() else {
            return ExplainOutcome {
                meta: empty_meta(),
                steps: Vec::new(),
                exit: Exit::NoTool,
            };
        };

        let registry = self.registry_of(pctx, pkg).await;
        let q = ResolveQuery {
            tool: pctx.tool,
            package: pkg,
            registry: registry.as_deref(),
            project: &pctx.rel_path,
            kind: ResolveKind::CurrentPin,
        };
        let res = resolve(&pctx.policy.layers, &q, self.ws.now());

        let steps = res
            .trace
            .iter()
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

        let meta = ExplainMeta {
            project: pctx.rel_path.to_string(),
            registry,
            effective: EffectiveInfo {
                min_age_days: round2(res.window.effective_min_age_days(self.ws.now())),
                decided_by: res.window.source(),
            },
        };

        ExplainOutcome {
            meta,
            steps,
            exit: Exit::Ok,
        }
    }

    fn config(&self) -> ConfigOutcome {
        let mut items: Vec<ConfigItem> = Vec::new();
        for pctx in self.ws.scoped_projects(self.opts) {
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

    /// The registry a package resolves to within a project, if it is a known dependency. Reads the
    /// resolved graph (which may invoke the toolchain but never the registry network); any error or
    /// a no-match yields `None` so callers degrade to a registry-less resolution.
    async fn registry_of(&self, pctx: &ProjectCtx, pkg: &str) -> Option<String> {
        let adapter = self.ws.adapter(pctx.tool)?;
        // The raw graph on purpose: this finds one package's registry by name (never displayed and
        // not list output), so `exclude`/`-p` scoping is irrelevant and would only hide the target.
        let deps = adapter
            .dependencies(&pctx.project, DepScope::Graph)
            .await
            .ok()?;
        deps.into_iter()
            .find(|dep| dep.package.name == pkg)
            .and_then(|dep| dep.package.registry)
    }
}

fn empty_meta() -> ExplainMeta {
    ExplainMeta {
        project: String::new(),
        registry: None,
        effective: EffectiveInfo {
            min_age_days: 0.0,
            decided_by: "default".into(),
        },
    }
}
