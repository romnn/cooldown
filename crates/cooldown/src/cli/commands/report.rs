use super::CommandContext;
use super::common::{emit_envelope, with_diags};
use crate::app::Exit;
use crate::cli::present;
use cooldown_core::{CoreError, HeldReason};
use cooldown_render as render;

/// The shared presentation flags for the dependency-table renderers.
fn render_options(ctx: &CommandContext<'_>) -> render::tty::RenderOptions {
    render::tty::RenderOptions {
        use_color: ctx.color,
        list_packages: ctx.opts.list_packages,
        paths: ctx.opts.paths,
        show_projects: ctx.opts.show_projects,
        no_suggestions: ctx.opts.no_suggestions,
    }
}

fn is_pin_hold(held_by: Option<&HeldReason>) -> bool {
    matches!(held_by, Some(HeldReason::ExactPin | HeldReason::CommitPin))
}

pub(super) async fn run_outdated(ctx: &CommandContext<'_>) -> Result<Exit, CoreError> {
    let out = ctx.ws.outdated(ctx.opts).await;
    // `--exit-code N` turns the informational report into a CI gate: a non-zero exit when there is
    // at least one adoptable update (a newer version that has already cleared its cooldown window).
    // Items still in cooldown never trip it — they aren't adoptable yet, by design.
    let exit = match ctx.opts.outdated_exit_code {
        Some(code) if code != 0 && out.summary.adoptable > 0 => Exit::Gated(code),
        _ => out.exit,
    };
    let summary = out.summary.clone();
    let items = out.items.clone();
    let env = with_diags(
        render::Envelope::new(
            "outdated",
            exit.is_ok(),
            ctx.generated_at.to_owned(),
            render::OutdatedMeta {},
            summary.clone(),
            items.clone(),
        ),
        out.warnings.clone(),
        out.errors.clone(),
    );
    // The JSON envelope keeps every item (machine consumers filter themselves); the human table
    // hides up-to-date rows unless `--all`, and exact/commit pins under `--hide-pinned`, so the
    // common case is a short, actionable report. Other held rows remain visible because their bound
    // or policy ceiling is actionable. The summary line below still reflects every dep.
    let table_items: Vec<render::OutdatedItem> = items
        .iter()
        .filter(|i| ctx.opts.show_all || i.status != render::OutdatedStatus::UpToDate)
        .filter(|i| {
            !(ctx.opts.hide_pinned && is_pin_hold(i.held_by.as_ref().map(render::HeldBy::reason)))
        })
        .cloned()
        .collect();
    emit_envelope(ctx.opts.json, &env, || {
        render::tty::render_outdated(
            &summary,
            &table_items,
            &out.warnings,
            &out.errors,
            &render_options(ctx),
        )
    })?;
    Ok(exit)
}

pub(super) async fn run_check(ctx: &CommandContext<'_>) -> Result<Exit, CoreError> {
    let out = ctx.ws.check(ctx.opts).await;
    let meta = out.meta.clone();
    let summary = out.summary.clone();
    let items = out.items.clone();
    let env = with_diags(
        render::Envelope::new(
            "check",
            out.exit.is_ok(),
            ctx.generated_at.to_owned(),
            meta.clone(),
            summary.clone(),
            items.clone(),
        ),
        out.warnings.clone(),
        out.errors.clone(),
    );
    emit_envelope(ctx.opts.json, &env, || {
        render::tty::render_check(
            &meta,
            &summary,
            &items,
            &out.warnings,
            &out.errors,
            &render_options(ctx),
        )
    })?;
    Ok(out.exit)
}

pub(super) async fn run_upgrade(ctx: &CommandContext<'_>) -> Result<Exit, CoreError> {
    let out = ctx.ws.upgrade(ctx.opts).await;
    let meta = out.meta.clone();
    let summary = out.summary.clone();
    let items = out.items.clone();
    let env = with_diags(
        render::Envelope::new(
            "upgrade",
            out.exit.is_ok(),
            ctx.generated_at.to_owned(),
            meta.clone(),
            summary.clone(),
            items.clone(),
        ),
        out.warnings.clone(),
        out.errors.clone(),
    );
    emit_envelope(ctx.opts.json, &env, || {
        render::tty::render_upgrade(
            &meta,
            &summary,
            &items,
            &out.warnings,
            &out.errors,
            &render_options(ctx),
        )
    })?;
    Ok(out.exit)
}

pub(super) async fn run_fix(ctx: &CommandContext<'_>) -> Result<Exit, CoreError> {
    let out = ctx.ws.fix(ctx.opts).await;
    let meta = out.meta.clone();
    let summary = out.summary.clone();
    let items = out.items.clone();
    let env = with_diags(
        render::Envelope::new(
            "fix",
            out.exit.is_ok(),
            ctx.generated_at.to_owned(),
            meta.clone(),
            summary.clone(),
            items.clone(),
        ),
        out.warnings.clone(),
        out.errors.clone(),
    );
    emit_envelope(ctx.opts.json, &env, || {
        render::tty::render_fix(
            &meta,
            &summary,
            &items,
            &out.warnings,
            &out.errors,
            &render_options(ctx),
        )
    })?;
    Ok(out.exit)
}

pub(super) async fn run_explain(
    ctx: &CommandContext<'_>,
    package: &str,
) -> Result<Exit, CoreError> {
    let out = ctx.ws.explain(package, ctx.opts).await?;
    let meta = out.meta.clone();
    let steps = out.steps.clone();
    let env = render::Envelope::new(
        "explain",
        out.exit.is_ok(),
        ctx.generated_at.to_owned(),
        meta.clone(),
        render::ExplainSummary {},
        steps.clone(),
    );
    emit_envelope(ctx.opts.json, &env, || {
        render::tty::render_explain(&meta, &steps, ctx.color)
    })?;
    Ok(out.exit)
}

pub(super) async fn run_sync(ctx: &CommandContext<'_>) -> Result<Exit, CoreError> {
    let out = ctx.ws.sync(ctx.opts).await;
    let summary = present::sync_summary(&out.summary);
    let items = present::sync_items(&out.items);
    let env = with_diags(
        render::Envelope::new(
            "sync",
            out.exit.is_ok(),
            ctx.generated_at.to_owned(),
            present::SyncMeta {},
            summary.clone(),
            items.clone(),
        ),
        Vec::new(),
        out.errors.clone(),
    );
    emit_envelope(ctx.opts.json, &env, || {
        present::render_sync_text(&out.summary, &out.items)
    })?;
    Ok(out.exit)
}

pub(super) fn run_config(ctx: &CommandContext<'_>) -> Result<Exit, CoreError> {
    let out = ctx.ws.config(ctx.opts);
    let summary = out.summary.clone();
    let items = out.items.clone();
    let env = render::Envelope::new(
        "config",
        out.exit.is_ok(),
        ctx.generated_at.to_owned(),
        render::ConfigMeta {},
        summary.clone(),
        items.clone(),
    );
    emit_envelope(ctx.opts.json, &env, || {
        present::render_config_text(&out.items)
    })?;
    Ok(out.exit)
}

#[cfg(test)]
mod tests {
    use super::is_pin_hold;
    use cooldown_core::HeldReason;

    #[test]
    fn hide_pinned_recognizes_only_exact_and_commit_pin_holds() {
        assert!(is_pin_hold(Some(&HeldReason::ExactPin)));
        assert!(is_pin_hold(Some(&HeldReason::CommitPin)));
        assert!(!is_pin_hold(Some(&HeldReason::GraphCeiling)));
        assert!(!is_pin_hold(Some(&HeldReason::DeclaredBound(
            "<6".to_string()
        ))));
        assert!(!is_pin_hold(Some(&HeldReason::MaxMajor(5))));
        assert!(!is_pin_hold(None));
    }
}
