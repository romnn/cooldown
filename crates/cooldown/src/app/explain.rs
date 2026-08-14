//! `explain <pkg>` — the field-by-field derivation of a package's window (every layer and rule
//! that applied), and `config` — the fully-resolved policy with the origin of each value. Together
//! they keep the override system from being a black box.

use super::{
    ConfigItem, ConfigSummary, EffectiveInfo, Exit, ExplainMeta, ExplainStep, ProjectCtx, RunOpts,
    Workspace, round2,
};
use cooldown_core::{DepScope, ResolveKind, ResolveQuery, resolve};

/// The result of `explain <pkg>`: the package's effective window plus the ordered derivation steps.
#[derive(Debug)]
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
    /// Explains the window for `pkg` in the first in-scope project.
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
        let registry = self.registry_of(pctx, pkg).await?;
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

        let meta = ExplainMeta {
            project: pctx.rel_path.to_string(),
            registry,
            effective: EffectiveInfo {
                min_age_days: round2(res.window.effective_min_age_days(self.ws.now())),
                decided_by: res.window.source(),
            },
        };

        Ok(ExplainOutcome {
            meta,
            steps,
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
    /// no advisory database covers can enable the feed and see nothing happen, and `config` is
    /// where one looks to find out why.
    fn advisory_config(&self, pctx: &ProjectCtx) -> crate::app::AdvisoryConfigInfo {
        let policy = cooldown_core::resolve_advisory_policy(&pctx.policy.layers);
        crate::app::AdvisoryConfigInfo {
            enabled: policy.enabled,
            source: match policy.source {
                cooldown_core::AdvisorySourceKind::Osv => "osv",
                cooldown_core::AdvisorySourceKind::Github => "github",
                cooldown_core::AdvisorySourceKind::None => "none",
            }
            .to_string(),
            mode: match policy.mode {
                cooldown_core::AdvisoryMode::Flag => "flag",
                cooldown_core::AdvisoryMode::Shorten => "shorten",
            }
            .to_string(),
            min_age_days: round2(cooldown_core::duration::duration_as_days(policy.min_age)),
            severity: policy.severity.as_str().to_string(),
            ecosystem: self
                .ws
                .adapter(pctx.tool)
                .and_then(|adapter| adapter.capabilities().advisory_ecosystem)
                .map(ToString::to_string),
        }
    }

    /// The registry a package resolves to within a project, if it is a known dependency.
    async fn registry_of(
        &self,
        pctx: &ProjectCtx,
        pkg: &str,
    ) -> cooldown_core::Result<Option<String>> {
        let Some(adapter) = self.ws.adapter(pctx.tool) else {
            return Ok(None);
        };
        let _guard = self.ws.project_read_guard(pctx).await?;
        // The raw graph on purpose: this finds one package's registry by name (never displayed and
        // not list output), so `exclude`/`-p` scoping is irrelevant and would only hide the target.
        let deps = match adapter.dependencies(&pctx.project, DepScope::Graph).await {
            Ok(deps) => deps,
            Err(
                error @ (cooldown_core::CoreError::StaleLock(_)
                | cooldown_core::CoreError::LockConflict(_)),
            ) => return Err(error),
            Err(_) => return Ok(None),
        };
        Ok(deps
            .into_iter()
            .find(|dep| dep.package.name == pkg)
            .and_then(|dep| dep.package.registry))
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
