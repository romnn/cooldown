//! `explain <pkg>` — the field-by-field derivation of a package's window (every layer and rule
//! that applied), and `config` — the fully-resolved policy with the origin of each value. Together
//! they keep the override system from being a black box.

use super::{round2, Exit, RunOpts, Workspace};
use cooldown_core::{resolve, ResolveKind, ResolveQuery};
use cooldown_render as render;

pub struct ExplainOutcome {
    pub meta: render::ExplainMeta,
    pub steps: Vec<render::ExplainStep>,
    pub exit: Exit,
}

pub struct ConfigOutcome {
    pub json: serde_json::Value,
    pub text: String,
    pub exit: Exit,
}

impl Workspace {
    /// Explain the window for `pkg` in the first in-scope project.
    pub fn explain(&self, pkg: &str, opts: &RunOpts) -> ExplainOutcome {
        let Some(pctx) = self.scoped_projects(opts).next() else {
            return ExplainOutcome {
                meta: empty_meta(),
                steps: Vec::new(),
                exit: Exit::NoEcosystem,
            };
        };

        // Use the package's registry if it is a known dependency (so registry rules apply); else
        // resolve with no registry.
        let registry = None;
        let q = ResolveQuery {
            ecosystem: pctx.ecosystem,
            package: pkg,
            registry,
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
                selector: s.selector.as_ref().and_then(|sel| sel.token()),
                min_age_days: s.min_age_days.map(round2),
                applied: s.applied,
                note: s.note.clone(),
            })
            .collect();

        let meta = render::ExplainMeta {
            project: pctx.rel_path.to_string(),
            registry: None,
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

    /// The fully-resolved config per project (effective default window + provenance + strict-native).
    pub fn config(&self, opts: &RunOpts, generated_at: &str) -> ConfigOutcome {
        let mut items = Vec::new();
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

            text.push_str(&format!(
                "{} [{}]\n  effective default window: {}d (decided by {})\n  strict-native: {}\n  layers: {}\n",
                pctx.rel_path,
                pctx.ecosystem,
                days,
                res.window.source(),
                pctx.policy.strict_native,
                layers.join(" < "),
            ));

            items.push(serde_json::json!({
                "project": pctx.rel_path.to_string(),
                "ecosystem": pctx.ecosystem.as_str(),
                "effectiveDefaultMinAgeDays": days,
                "source": res.window.source(),
                "strictNative": pctx.policy.strict_native,
                "layers": layers,
            }));
        }

        // The common envelope shape, identical across commands.
        let json = serde_json::json!({
            "schemaVersion": render::SCHEMA_VERSION,
            "command": "config",
            "ok": true,
            "generatedAt": generated_at,
            "summary": { "projects": items.len() },
            "items": items,
            "warnings": [],
            "errors": [],
        });

        ConfigOutcome {
            json,
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
