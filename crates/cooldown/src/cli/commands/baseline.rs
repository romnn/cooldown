use super::CommandContext;
use super::common::emit_envelope;
use crate::app::{Baseline, Exit};
use cooldown_core::CoreError;
use cooldown_render as render;
use std::fmt::Write as _;

pub(super) async fn run_baseline(ctx: &CommandContext<'_>, prune: bool) -> Result<Exit, CoreError> {
    let path = ctx.repo_root.join(crate::app::baseline::BASELINE_FILE);
    let existing = Baseline::load(&path)?;
    let young = ctx.ws.baseline_entries(ctx.opts).await?;

    let key = |entry: &crate::app::baseline::AckEntry| {
        (
            entry.ecosystem.clone(),
            entry.project.clone(),
            entry.package.clone(),
            entry.version.clone(),
            entry.registry.clone(),
        )
    };
    let young_keys: std::collections::HashSet<_> = young.iter().map(key).collect();

    let merged = if prune {
        young
            .into_iter()
            .map(|young_entry| {
                existing
                    .entries
                    .iter()
                    .find(|entry| key(entry) == key(&young_entry))
                    .map(|entry| crate::app::baseline::AckEntry {
                        reason: entry.reason.clone(),
                        until: entry.until.clone(),
                        ..young_entry.clone()
                    })
                    .unwrap_or(young_entry)
            })
            .collect::<Vec<_>>()
    } else {
        let mut out = existing.entries.clone();
        for young_entry in young {
            if !out.iter().any(|entry| key(entry) == key(&young_entry)) {
                out.push(young_entry);
            }
        }
        out
    };

    let removed = existing.entries.len().saturating_sub(
        existing
            .entries
            .iter()
            .filter(|entry| young_keys.contains(&key(entry)) || !prune)
            .count(),
    );

    let new_baseline = Baseline { entries: merged };
    new_baseline.save(&path)?;
    let items: Vec<render::BaselineItem> = new_baseline
        .entries
        .iter()
        .map(|entry| render::BaselineItem {
            ecosystem: entry.ecosystem.clone(),
            project: entry.project.clone(),
            package: entry.package.clone(),
            version: entry.version.clone(),
            registry: entry.registry.clone(),
        })
        .collect();
    let summary = render::BaselineSummary {
        acknowledged: items.len(),
        pruned: removed,
    };
    let env = render::Envelope::new(
        "baseline",
        true,
        ctx.generated_at.to_owned(),
        render::BaselineMeta {
            path: path.to_string(),
        },
        summary.clone(),
        items,
    );

    emit_envelope(ctx.global.json, &env, || {
        let mut text = format!(
            "wrote {path}: {} acknowledged entr{}",
            summary.acknowledged,
            if summary.acknowledged == 1 {
                "y"
            } else {
                "ies"
            }
        );
        if prune && summary.pruned > 0 {
            text.push('\n');
            let _ = write!(
                text,
                "pruned {} stale entr{}",
                summary.pruned,
                if summary.pruned == 1 { "y" } else { "ies" }
            );
        }
        text.push('\n');
        text
    })?;

    Ok(Exit::Ok)
}
