use super::{Command, GlobalArgs};
use crate::app::{Baseline, Exit, RunOpts, Workspace};
use camino::Utf8Path;
use cooldown_core::CoreError;
use cooldown_render as render;
use serde::Serialize;
use std::fmt::Write as _;

pub(crate) async fn dispatch(
    command: Command,
    ws: &Workspace,
    opts: &RunOpts,
    repo_root: &Utf8Path,
    g: &GlobalArgs,
    color: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    let exit = match command {
        Command::Outdated => run_outdated(ws, opts, g.json, color, generated_at).await?,
        Command::Check => run_check(ws, opts, g.json, color, generated_at).await?,
        Command::Upgrade => run_upgrade(ws, opts, g, color, generated_at).await?,
        Command::Explain { package } => {
            run_explain(ws, opts, &package, g.json, color, generated_at).await?
        }
        Command::Config => run_config(ws, opts, g.json, generated_at)?,
        Command::Baseline { prune } => {
            run_baseline(ws, opts, repo_root, prune, g.json, generated_at).await?
        }
        #[allow(
            clippy::unreachable,
            reason = "schema/init/sync are dispatched by run_workspace_free before any workspace exists"
        )]
        Command::Schema | Command::Init | Command::Sync => unreachable!("handled earlier"),
    };

    Ok(exit)
}

pub(crate) fn no_ecosystem_json(command: &str) -> String {
    serde_json::json!({
        "schemaVersion": render::SCHEMA_VERSION,
        "command": command,
        "ok": false,
        "generatedAt": super::generated_at(jiff::Timestamp::now()),
        "summary": {},
        "items": [],
        "warnings": [],
        "errors": [{ "kind": "not_found", "message": "no supported ecosystem detected" }],
    })
    .to_string()
}

async fn run_outdated(
    ws: &Workspace,
    opts: &RunOpts,
    json: bool,
    color: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    let out = ws.outdated(opts).await;
    let env = with_diags(
        render::Envelope::new(
            "outdated",
            out.exit.is_ok(),
            generated_at.to_owned(),
            render::OutdatedMeta {},
            out.summary.clone(),
            out.items.clone(),
        ),
        out.warnings.clone(),
        out.errors.clone(),
    );
    emit_envelope(json, &env, || {
        render::tty::render_outdated(&out.summary, &out.items, &out.warnings, &out.errors, color)
    })?;
    Ok(out.exit)
}

async fn run_check(
    ws: &Workspace,
    opts: &RunOpts,
    json: bool,
    color: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    let out = ws.check(opts).await;
    let env = with_diags(
        render::Envelope::new(
            "check",
            out.exit.is_ok(),
            generated_at.to_owned(),
            out.meta.clone(),
            out.summary.clone(),
            out.items.clone(),
        ),
        out.warnings.clone(),
        out.errors.clone(),
    );
    emit_envelope(json, &env, || {
        render::tty::render_check(
            &out.meta,
            &out.summary,
            &out.items,
            &out.warnings,
            &out.errors,
            color,
        )
    })?;
    Ok(out.exit)
}

async fn run_upgrade(
    ws: &Workspace,
    opts: &RunOpts,
    g: &GlobalArgs,
    color: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    if g.include_indirect {
        return Err(CoreError::Config(
            "`upgrade --include-indirect` is not allowed: acting on transitive deps is a non-goal"
                .into(),
        ));
    }
    if g.major && g.package.is_empty() && !g.major_all {
        return Err(CoreError::Config(
            "`upgrade --major` rewrites import paths repo-wide; pass --package or --major-all"
                .into(),
        ));
    }
    let out = ws.upgrade(opts).await;
    let env = with_diags(
        render::Envelope::new(
            "upgrade",
            out.exit.is_ok(),
            generated_at.to_owned(),
            out.meta.clone(),
            out.summary.clone(),
            out.items.clone(),
        ),
        out.warnings.clone(),
        out.errors.clone(),
    );
    emit_envelope(g.json, &env, || {
        render::tty::render_upgrade(
            &out.meta,
            &out.summary,
            &out.items,
            &out.warnings,
            &out.errors,
            color,
        )
    })?;
    Ok(out.exit)
}

async fn run_explain(
    ws: &Workspace,
    opts: &RunOpts,
    package: &str,
    json: bool,
    color: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    let out = ws.explain(package, opts).await;
    let env = render::Envelope::new(
        "explain",
        out.exit.is_ok(),
        generated_at.to_owned(),
        out.meta.clone(),
        render::ExplainSummary {},
        out.steps.clone(),
    );
    emit_envelope(json, &env, || {
        render::tty::render_explain(&out.meta, &out.steps, color)
    })?;
    Ok(out.exit)
}

fn run_config(
    ws: &Workspace,
    opts: &RunOpts,
    json: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    let out = ws.config(opts);
    let env = render::Envelope::new(
        "config",
        out.exit.is_ok(),
        generated_at.to_owned(),
        render::ConfigMeta {},
        out.summary.clone(),
        out.items.clone(),
    );
    emit_envelope(json, &env, || out.text)?;
    Ok(out.exit)
}

async fn run_baseline(
    ws: &Workspace,
    opts: &RunOpts,
    repo_root: &Utf8Path,
    prune: bool,
    json: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    let path = repo_root.join(crate::app::baseline::BASELINE_FILE);
    let existing = Baseline::load(&path)?;
    let young = ws.baseline_entries(opts).await?;

    let key = |e: &crate::app::baseline::AckEntry| {
        (
            e.ecosystem.clone(),
            e.project.clone(),
            e.package.clone(),
            e.version.clone(),
            e.registry.clone(),
        )
    };
    let young_keys: std::collections::HashSet<_> = young.iter().map(key).collect();

    let merged = if prune {
        young
            .into_iter()
            .map(|y| {
                existing
                    .entries
                    .iter()
                    .find(|e| key(e) == key(&y))
                    .map(|e| crate::app::baseline::AckEntry {
                        reason: e.reason.clone(),
                        until: e.until.clone(),
                        ..y.clone()
                    })
                    .unwrap_or(y)
            })
            .collect::<Vec<_>>()
    } else {
        let mut out = existing.entries.clone();
        for y in young {
            if !out.iter().any(|e| key(e) == key(&y)) {
                out.push(y);
            }
        }
        out
    };

    let removed = existing.entries.len().saturating_sub(
        existing
            .entries
            .iter()
            .filter(|e| young_keys.contains(&key(e)) || !prune)
            .count(),
    );

    let new_baseline = Baseline { entries: merged };
    new_baseline.save(&path)?;
    let items: Vec<render::BaselineItem> = new_baseline
        .entries
        .iter()
        .map(|e| render::BaselineItem {
            ecosystem: e.ecosystem.clone(),
            project: e.project.clone(),
            package: e.package.clone(),
            version: e.version.clone(),
            registry: e.registry.clone(),
        })
        .collect();
    let summary = render::BaselineSummary {
        acknowledged: items.len(),
        pruned: removed,
    };
    let env = render::Envelope::new(
        "baseline",
        true,
        generated_at.to_owned(),
        render::BaselineMeta {
            path: path.to_string(),
        },
        summary.clone(),
        items,
    );

    emit_envelope(json, &env, || {
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

fn emit_envelope<M, S, I, F>(
    json: bool,
    env: &render::Envelope<M, S, I>,
    render_tty: F,
) -> Result<(), CoreError>
where
    M: Serialize,
    S: Serialize,
    I: Serialize,
    F: FnOnce() -> String,
{
    if json {
        let json = render::to_json(env)
            .map_err(|e| CoreError::Io(format!("serialize JSON output: {e}")))?;
        println!("{json}");
    } else {
        print!("{}", render_tty());
    }
    Ok(())
}

fn with_diags<M, S, I>(
    mut env: render::Envelope<M, S, I>,
    warnings: Vec<cooldown_core::Diagnostic>,
    errors: Vec<cooldown_core::Diagnostic>,
) -> render::Envelope<M, S, I>
where
    M: Serialize,
    S: Serialize,
    I: Serialize,
{
    env.warnings = warnings;
    env.errors = errors;
    env
}
