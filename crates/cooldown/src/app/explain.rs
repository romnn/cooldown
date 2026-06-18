//! `explain <pkg>` — the field-by-field derivation of a package's window (every layer and rule
//! that applied), and `config` — the fully-resolved policy with the origin of each value. Together
//! they keep the override system from being a black box.

use super::{Exit, ProjectCtx, RunOpts, Workspace, round2};
use cooldown_core::{DepScope, ResolveKind, ResolveQuery, resolve};
use cooldown_render as render;
use std::fmt::Write as _;

/// The result of `explain <pkg>`: the package's effective window plus the ordered derivation steps.
pub struct ExplainOutcome {
    /// The resolved window and the project/registry it was derived for.
    pub meta: render::ExplainMeta,
    /// Each layer-and-rule step that contributed to (or was shadowed in) the derivation.
    pub steps: Vec<render::ExplainStep>,
    /// The process exit (`Ok`, or `NoEcosystem` when no project is in scope).
    pub exit: Exit,
}

/// The result of `config`: the fully-resolved policy per project in both JSON and text form.
pub struct ConfigOutcome {
    /// The aggregate project count.
    pub summary: render::ConfigSummary,
    /// One resolved policy row per project.
    pub items: Vec<render::ConfigItem>,
    /// The human-readable rendering.
    pub text: String,
    /// The process exit (always `Ok`).
    pub exit: Exit,
}

impl Workspace {
    /// Explain the window for `pkg` in the first in-scope project. If `pkg` is a resolved
    /// dependency, its registry is looked up from the dependency graph so that registry-scoped
    /// rules (`[registry."…"]`) participate in the derivation. The lookup is best-effort: a missing
    /// lock or a tool failure falls back to a registry-less resolution, so `explain` still answers
    /// for a package that is not (yet) a dependency.
    pub async fn explain(&self, pkg: &str, opts: &RunOpts) -> ExplainOutcome {
        let Some(pctx) = self.scoped_projects(opts).next() else {
            return ExplainOutcome {
                meta: empty_meta(),
                steps: Vec::new(),
                exit: Exit::NoEcosystem,
            };
        };

        let registry = self.registry_of(pctx, pkg).await;
        let q = ResolveQuery {
            ecosystem: pctx.ecosystem,
            package: pkg,
            registry: registry.as_deref(),
            project: &pctx.rel_path,
            kind: ResolveKind::CurrentPin,
        };
        let res = resolve(&pctx.policy.layers, &q, self.now);

        let steps = res
            .trace
            .iter()
            .map(|s| render::ExplainStep {
                layer: s.layer.token(),
                field: s.field.clone(),
                selector: s.selector.as_ref().and_then(cooldown_core::Selector::token),
                min_age_days: s.min_age_days.map(round2),
                applied: s.applied,
                note: s.note.clone(),
            })
            .collect();

        let meta = render::ExplainMeta {
            project: pctx.rel_path.to_string(),
            registry,
            effective: render::EffectiveInfo {
                min_age_days: round2(res.window.effective_min_age_days(self.now)),
                decided_by: res.window.source(),
            },
        };

        ExplainOutcome {
            meta,
            steps,
            exit: Exit::Ok,
        }
    }

    /// The registry a package resolves to within a project, if it is a known dependency. Reads the
    /// resolved graph (which may invoke the toolchain but never the registry network); any error or
    /// a no-match yields `None` so callers degrade to a registry-less resolution.
    async fn registry_of(&self, pctx: &ProjectCtx, pkg: &str) -> Option<String> {
        let adapter = self.adapter(pctx.ecosystem)?;
        let deps = adapter
            .dependencies(&pctx.project, DepScope::Graph)
            .await
            .ok()?;
        deps.into_iter()
            .find(|d| d.package.name == pkg)
            .and_then(|d| d.package.registry)
    }

    /// The fully-resolved config per project (effective default window + provenance + strict-native).
    #[must_use]
    pub fn config(&self, opts: &RunOpts) -> ConfigOutcome {
        let mut items: Vec<render::ConfigItem> = Vec::new();
        let mut text = String::new();
        for pctx in self.scoped_projects(opts) {
            // Resolve the bare default for a sentinel name unlikely to match a package glob.
            let q = ResolveQuery {
                ecosystem: pctx.ecosystem,
                package: "\u{0}default",
                registry: None,
                project: &pctx.rel_path,
                kind: ResolveKind::CurrentPin,
            };
            let res = resolve(&pctx.policy.layers, &q, self.now);
            let days = round2(res.window.effective_min_age_days(self.now));
            let layers: Vec<String> = pctx
                .policy
                .layers
                .iter()
                .map(|l| l.origin.token())
                .collect();

            // Infallible: writing to a String never errors.
            let _ = writeln!(
                text,
                "{} [{}]\n  effective default window: {}d (decided by {})\n  strict-native: {}\n  layers: {}",
                pctx.rel_path,
                pctx.ecosystem,
                days,
                res.window.source(),
                pctx.policy.strict_native,
                layers.join(" < "),
            );

            items.push(render::ConfigItem {
                project: pctx.rel_path.to_string(),
                ecosystem: pctx.ecosystem.as_str().to_string(),
                effective_default_min_age_days: days,
                source: res.window.source(),
                strict_native: pctx.policy.strict_native,
                layers,
            });
        }

        ConfigOutcome {
            summary: render::ConfigSummary {
                projects: items.len(),
            },
            items,
            text,
            exit: Exit::Ok,
        }
    }
}

fn empty_meta() -> render::ExplainMeta {
    render::ExplainMeta {
        project: String::new(),
        registry: None,
        effective: render::EffectiveInfo {
            min_age_days: 0.0,
            decided_by: "default".into(),
        },
    }
}
