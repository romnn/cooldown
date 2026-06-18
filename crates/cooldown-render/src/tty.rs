//! Colorful TTY tables. Rendering is deterministic when `use_color` is false (snapshot tests use
//! that), and styled for the terminal otherwise.

use crate::model::{
    CheckItem, CheckMeta, CheckStatus, CheckSummary, ExplainMeta, ExplainStep, OutdatedItem,
    OutdatedStatus, OutdatedSummary, UpgradeItem, UpgradeMeta, UpgradeSummary,
};
use comfy_table::{Cell, Color, ContentArrangement, Table};
use cooldown_core::{Diagnostic, Status};
use std::fmt::Write as _;

fn base_table(use_color: bool) -> Table {
    let mut t = Table::new();
    t.load_preset(comfy_table::presets::UTF8_HORIZONTAL_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic);
    // The caller has already decided whether to colorize (TTY / `--color`); enforce it so comfy-
    // table's own TTY check can't strip ANSI when the output is piped (e.g. into a screenshot tool).
    if use_color {
        t.enforce_styling();
    } else {
        t.force_no_tty();
    }
    t
}

fn status_color(s: OutdatedStatus) -> Color {
    match s {
        OutdatedStatus::Adoptable => Color::Green,
        OutdatedStatus::UpToDate => Color::DarkGreen,
        OutdatedStatus::InCooldown => Color::Yellow,
        OutdatedStatus::CurrentInCooldown | OutdatedStatus::Error => Color::Red,
        OutdatedStatus::Exempt => Color::Cyan,
        OutdatedStatus::Held => Color::DarkGrey,
        OutdatedStatus::UnknownAge => Color::Magenta,
    }
}

fn status_label(s: OutdatedStatus) -> &'static str {
    match s {
        OutdatedStatus::Adoptable => "adoptable",
        OutdatedStatus::UpToDate => "up-to-date",
        OutdatedStatus::InCooldown => "in cooldown",
        OutdatedStatus::CurrentInCooldown => "current in cooldown",
        OutdatedStatus::Exempt => "exempt",
        OutdatedStatus::Held => "held",
        OutdatedStatus::UnknownAge => "unknown age",
        OutdatedStatus::Error => "error",
    }
}

fn cell_colored(text: impl Into<String>, color: Color, use_color: bool) -> Cell {
    let c = Cell::new(text.into());
    if use_color { c.fg(color) } else { c }
}

fn fmt_days(d: f64) -> String {
    if d == 0.0 {
        "0d".to_string()
    } else if d < 1.0 {
        format!("{d:.1}d")
    } else {
        format!("{d:.0}d")
    }
}

/// Render the `outdated` report.
#[must_use]
pub fn render_outdated(
    summary: &OutdatedSummary,
    items: &[OutdatedItem],
    warnings: &[Diagnostic],
    errors: &[Diagnostic],
    use_color: bool,
) -> String {
    let mut out = String::new();
    if items.is_empty() {
        out.push_str("All dependencies are up to date.\n");
    } else {
        let mut t = base_table(use_color);
        t.set_header(vec![
            "Package",
            "Project",
            "Current",
            "Adoptable",
            "Latest",
            "Window",
            "Status",
        ]);
        for it in items {
            let adoptable = it.adoptable_target.clone().unwrap_or_else(|| "—".into());
            let latest = it
                .latest
                .as_ref()
                .map_or_else(|| "—".into(), |l| l.version.clone());
            let window = match &it.window.clamped_by {
                Some(by) => format!("{} (≥{by})", fmt_days(it.window.min_age_days)),
                None => fmt_days(it.window.min_age_days),
            };
            t.add_row(vec![
                Cell::new(&it.name),
                Cell::new(&it.project),
                Cell::new(&it.current),
                Cell::new(adoptable),
                Cell::new(latest),
                Cell::new(window),
                cell_colored(status_label(it.status), status_color(it.status), use_color),
            ]);
        }
        out.push_str(&t.to_string());
        out.push('\n');
    }
    let _ = write!(
        out,
        "\n{} adoptable · {} in cooldown · {} up-to-date · {} exempt · {} held · {} unknown-age",
        summary.adoptable,
        summary.in_cooldown,
        summary.up_to_date,
        summary.exempt,
        summary.held,
        summary.unknown_age,
    );
    if summary.errors > 0 {
        let _ = write!(out, " · {} errors", summary.errors);
    }
    out.push('\n');
    push_diagnostics(&mut out, warnings, errors, use_color);
    out
}

/// Render the `check` report.
pub fn render_check(
    meta: &CheckMeta,
    summary: &CheckSummary,
    items: &[CheckItem],
    warnings: &[Diagnostic],
    errors: &[Diagnostic],
    use_color: bool,
) -> String {
    let mut out = String::new();
    if items.is_empty() && errors.is_empty() {
        let _ = writeln!(
            out,
            "✓ {} dependencies pass the cooldown gate ({} scope).",
            summary.checked, meta.scope
        );
    } else {
        let mut t = base_table(use_color);
        t.set_header(vec![
            "Package", "Project", "Version", "Age", "Window", "Status", "Notes",
        ]);
        for it in items {
            let (label, color) = match it.status {
                CheckStatus::Violation => ("violation", Color::Red),
                CheckStatus::Acknowledged => ("acknowledged", Color::Cyan),
                CheckStatus::UnknownAge => ("unknown age", Color::Magenta),
                CheckStatus::Error => ("error", Color::Red),
            };
            let age = it.age_days.map_or_else(|| "?".to_string(), fmt_days);
            let mut notes = Vec::new();
            if it.graph_held {
                notes.push("graph-held".to_string());
            }
            if let Some(gf) = &it.graph_floor {
                notes.push(format!("floor {gf}"));
            }
            if let Some(e) = &it.error {
                notes.push(e.message.clone());
            }
            t.add_row(vec![
                Cell::new(&it.name),
                Cell::new(&it.project),
                Cell::new(&it.current),
                Cell::new(age),
                Cell::new(fmt_days(it.window.min_age_days)),
                cell_colored(label, color, use_color),
                Cell::new(notes.join("; ")),
            ]);
        }
        out.push_str(&t.to_string());
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "\nchecked {} ({} direct) · {} violations · {} acknowledged · {} exempt · {} unknown-age · {} errors",
        summary.checked,
        summary.direct,
        summary.violations,
        summary.acknowledged,
        summary.exempt,
        summary.unknown_age,
        summary.errors,
    );
    push_diagnostics(&mut out, warnings, errors, use_color);
    out
}

/// Render the `upgrade` report.
#[must_use]
pub fn render_upgrade(
    meta: &UpgradeMeta,
    summary: &UpgradeSummary,
    items: &[UpgradeItem],
    warnings: &[Diagnostic],
    errors: &[Diagnostic],
    use_color: bool,
) -> String {
    let mut out = String::new();
    if items.is_empty() {
        out.push_str("Nothing to upgrade.\n");
    } else {
        let mut t = base_table(use_color);
        t.set_header(vec!["Package", "Project", "From", "To", "Kind", "Result"]);
        for it in items {
            let (label, color) = if it.applied {
                ("applied".to_string(), Color::Green)
            } else if let Some(sk) = &it.skipped {
                (format!("skipped: {}", sk.message), Color::Yellow)
            } else if let Some(e) = &it.error {
                (format!("error: {}", e.message), Color::Red)
            } else {
                ("planned".to_string(), Color::Cyan)
            };
            t.add_row(vec![
                Cell::new(&it.name),
                Cell::new(&it.project),
                Cell::new(&it.from),
                Cell::new(&it.to),
                Cell::new(format!("{:?}", it.kind).to_lowercase()),
                cell_colored(label, color, use_color),
            ]);
        }
        out.push_str(&t.to_string());
        out.push('\n');
    }
    let lock = match meta.lock_verified {
        Some(true) => "lock re-verified",
        Some(false) => "lock verification FAILED",
        None => "dry-run (lock untouched)",
    };
    let _ = writeln!(
        out,
        "\n{} applied · {} skipped · {} errors · {}",
        summary.applied, summary.skipped, summary.errors, lock
    );
    if meta.build.requested {
        let _ = writeln!(
            out,
            "build: {}",
            match meta.build.ok {
                Some(true) => "ok",
                Some(false) => "FAILED",
                None => "not run",
            }
        );
    }
    push_diagnostics(&mut out, warnings, errors, use_color);
    out
}

/// Render the `explain` derivation.
pub fn render_explain(meta: &ExplainMeta, steps: &[ExplainStep], use_color: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "effective window: {} (decided by {})",
        fmt_days(meta.effective.min_age_days),
        meta.effective.decided_by
    );
    let _ = writeln!(out, "project: {}", meta.project);
    if let Some(r) = &meta.registry {
        let _ = writeln!(out, "registry: {r}");
    }
    out.push('\n');
    let mut t = base_table(use_color);
    t.set_header(vec![
        "Layer", "Field", "Selector", "Window", "Applied", "Note",
    ]);
    for s in steps {
        let applied = if s.applied { "✓" } else { "" };
        let color = if s.applied {
            Color::Green
        } else {
            Color::DarkGrey
        };
        t.add_row(vec![
            Cell::new(&s.layer),
            Cell::new(&s.field),
            Cell::new(s.selector.clone().unwrap_or_default()),
            Cell::new(s.min_age_days.map(fmt_days).unwrap_or_default()),
            cell_colored(applied, color, use_color),
            Cell::new(&s.note),
        ]);
    }
    out.push_str(&t.to_string());
    out.push('\n');
    out
}

fn push_diagnostics(
    out: &mut String,
    warnings: &[Diagnostic],
    errors: &[Diagnostic],
    _use_color: bool,
) {
    for w in warnings {
        let pkg = w.package.as_deref().unwrap_or("");
        let _ = writeln!(out, "warning [{}] {} {}", w.kind, pkg, w.message);
    }
    for e in errors {
        let pkg = e.package.as_deref().unwrap_or("");
        let _ = writeln!(out, "error [{}] {} {}", e.kind, pkg, e.message);
    }
}

/// Map a core [`Status`] to a `check` finding status. `UpToDate`/`Exempt` are not findings.
#[must_use]
pub fn check_status_of(status: Status, acknowledged: bool) -> Option<CheckStatus> {
    if acknowledged {
        return Some(CheckStatus::Acknowledged);
    }
    match status {
        Status::CurrentInCooldown => Some(CheckStatus::Violation),
        Status::UnknownAge => Some(CheckStatus::UnknownAge),
        // `UpToDate`/`Exempt` are passing, not findings. The remaining variants
        // (`Adoptable`/`InCooldown`/`Held`) are not produced by check_pin, but
        // map defensively to "not a finding".
        Status::UpToDate
        | Status::Exempt
        | Status::Adoptable
        | Status::InCooldown
        | Status::Held => None,
    }
}
