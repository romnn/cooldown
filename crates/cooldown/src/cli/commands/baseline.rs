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
            entry.tool.clone(),
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
    // `--dry-run`: report the baseline that would be written without touching the file.
    if !ctx.opts.dry_run {
        new_baseline.save(&path)?;
    }
    let items: Vec<render::BaselineItem> = new_baseline
        .entries
        .iter()
        .map(|entry| render::BaselineItem {
            tool: entry.tool.clone(),
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
            dry_run: ctx.opts.dry_run,
        },
        summary.clone(),
        items,
    );

    let dry_run = ctx.opts.dry_run;
    emit_envelope(ctx.opts.json, &env, || {
        baseline_text(&path, &summary, prune, dry_run)
    })?;

    Ok(Exit::Ok)
}

/// The human (non-`--json`) summary line(s). Under `dry_run` the verbs become "would write" /
/// "would prune", since the file is left untouched.
fn baseline_text(
    path: &camino::Utf8Path,
    summary: &render::BaselineSummary,
    prune: bool,
    dry_run: bool,
) -> String {
    let mut text = format!(
        "{} {path}: {} acknowledged {}",
        if dry_run { "would write" } else { "wrote" },
        summary.acknowledged,
        entry_word(summary.acknowledged),
    );
    if prune && summary.pruned > 0 {
        let _ = write!(
            text,
            "\n{} {} stale {}",
            if dry_run { "would prune" } else { "pruned" },
            summary.pruned,
            entry_word(summary.pruned),
        );
    }
    text.push('\n');
    text
}

fn entry_word(n: usize) -> &'static str {
    if n == 1 { "entry" } else { "entries" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_text_uses_dry_run_verbs() {
        let summary = render::BaselineSummary {
            acknowledged: 2,
            pruned: 1,
        };

        assert_eq!(
            baseline_text(
                camino::Utf8Path::new("cooldown.baseline.toml"),
                &summary,
                true,
                true
            ),
            "would write cooldown.baseline.toml: 2 acknowledged entries\nwould prune 1 stale entry\n"
        );
    }

    #[test]
    fn baseline_text_uses_written_verbs() {
        let summary = render::BaselineSummary {
            acknowledged: 1,
            pruned: 0,
        };

        assert_eq!(
            baseline_text(
                camino::Utf8Path::new("cooldown.baseline.toml"),
                &summary,
                false,
                false
            ),
            "wrote cooldown.baseline.toml: 1 acknowledged entry\n"
        );
    }
}
