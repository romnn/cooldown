//! Colorful TTY tables. Rendering is deterministic when `use_color` is false (snapshot tests use
//! that), and styled for the terminal otherwise.

use crate::model::{
    CheckItem, CheckMeta, CheckStatus, CheckSummary, ExplainMeta, ExplainStep, OutdatedItem,
    OutdatedStatus, OutdatedSummary, UpgradeItem, UpgradeMeta, UpgradeSummary, Window,
};
use comfy_table::{Cell, Color, ContentArrangement, Table};
use cooldown_core::{Diagnostic, MemberRef, SkipReason, Status};
use std::fmt::Write as _;

/// The color of the package-name column (every listed dependency is actionable).
const PACKAGE_COLOR: Color = Color::Cyan;

/// The color of the "Adoptable" version — the version takeable now. Deliberately *not* the green of
/// the adoptable *status*: held rows can show a matured manual-pin target too, so this must read as
/// "this version is takeable" independent of the row's status.
const ADOPTABLE_COLOR: Color = Color::Magenta;

/// Presentation flags shared by the dependency-table renderers.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions {
    /// Colorize output for the terminal.
    pub use_color: bool,
    /// List every source package on its own line instead of `first (+N others)`.
    pub list_packages: bool,
    /// Show the "Used by" column as workspace paths instead of package names.
    pub paths: bool,
    /// Add the "Project" column attributing each row to its project. Hidden by default; even when
    /// set, the column only appears if some row's project is not the repo root.
    pub show_projects: bool,
    /// Suppress actionable tips (e.g. the `--major` command after an `upgrade` holds a major back).
    pub no_suggestions: bool,
}

/// Whether the "Used by" column should appear: at least one row attributes a source package.
fn has_attribution<T>(items: &[T], members: impl Fn(&T) -> &[MemberRef]) -> bool {
    items.iter().any(|it| !members(it).is_empty())
}

/// Whether some row's project is not just the root — a precondition for the opt-in "Project" column
/// (`--show-projects`). A repo whose only project is the root (`.`) has nothing to attribute, so the
/// column stays hidden even when the flag is set; multi-project trees (uv packages) can show it.
fn has_distinct_project<T>(items: &[T], project: impl Fn(&T) -> &str) -> bool {
    items.iter().any(|it| !is_root_path(project(it)))
}

fn is_root_path(path: &str) -> bool {
    matches!(path, "." | "./" | "")
}

/// Render a project/path label, showing the root as `./` so it reads clearly as a path.
fn path_label(path: &str) -> String {
    if path == "." {
        "./".to_string()
    } else {
        path.to_string()
    }
}

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

/// A muted gray (256-color) for the table's rule lines, so the borders don't glare against the text.
const BORDER_COLOR: &str = "\x1b[38;5;240m";
const FG_RESET: &str = "\x1b[39m";

/// Recolor the table's horizontal rule lines to a muted gray when coloring is on. comfy-table has no
/// border-color API, so this post-processes the rendered string: a line made entirely of box-drawing
/// characters is a separator and gets dimmed; content rows (which carry the only real text) are left
/// untouched, so their own cell colors are never disturbed.
fn dim_borders(table: &str, use_color: bool) -> String {
    if !use_color {
        return table.to_string();
    }
    let mut out = String::with_capacity(table.len());
    for (index, line) in table.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if !line.is_empty() && line.chars().all(is_rule_char) {
            let _ = write!(out, "{BORDER_COLOR}{line}{FG_RESET}");
        } else {
            out.push_str(line);
        }
    }
    out
}

/// A box-drawing or spacing character — the only characters a separator rule is built from.
fn is_rule_char(c: char) -> bool {
    c == ' ' || ('\u{2500}'..='\u{257f}').contains(&c)
}

fn status_color(s: OutdatedStatus) -> Color {
    match s {
        OutdatedStatus::Adoptable => Color::Green,
        OutdatedStatus::UpToDate => Color::DarkGreen,
        OutdatedStatus::InCooldown => Color::Yellow,
        OutdatedStatus::CurrentInCooldown | OutdatedStatus::Error => Color::Red,
        OutdatedStatus::Exempt => Color::Cyan,
        OutdatedStatus::Blocked | OutdatedStatus::Held => Color::DarkGrey,
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
        OutdatedStatus::Blocked => "blocked",
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
    let days = d.round();
    if days == 0.0 {
        "0d".to_string()
    } else {
        format!("{days:.0}d")
    }
}

fn with_window_clamp(mut cell: String, window: &Window) -> String {
    if let Some(by) = &window.clamped_by {
        let _ = write!(cell, " (≥{by})");
    }
    cell
}

fn age_over_window_cell(window: &Window, age: &str) -> String {
    with_window_clamp(format!("{age}/{}", fmt_days(window.min_age_days)), window)
}

/// The `outdated` "Cooldown" cell: the shown version's `age/window` (e.g. `13d/14d`) — the upgrade
/// candidate's age when a newer version exists, or the current pin's age when up to date. Falls back
/// to the bare window when no age applies — a commit pin, or an unknown publish time. A stricter
/// native / registry clamp is appended as `(≥<source>)`, matching the rest of the report. When the
/// countdown refers to a version no other column names (under `--countdown soonest`), that version is
/// appended in parentheses, e.g. `4d/7d (0.15.17)`.
fn cooldown_cell(
    window: &Window,
    candidate_age_days: Option<f64>,
    version: Option<&str>,
) -> String {
    let cell = match candidate_age_days {
        Some(age) => age_over_window_cell(window, &fmt_days(age)),
        None => with_window_clamp(fmt_days(window.min_age_days), window),
    };
    match version {
        Some(version) => format!("{cell} ({version})"),
        None => cell,
    }
}

fn check_cooldown_cell(window: &Window, age_days: Option<f64>) -> String {
    let age = age_days.map_or_else(|| "?".to_string(), fmt_days);
    age_over_window_cell(window, &age)
}

/// Render the "Used by" column for one dependency: the member packages by name (or by path under
/// `paths`). Blank when unattributed; with `list_all`, every member on its own line (a multi-line
/// cell, so the row's other columns stay on the first line); otherwise the shortest label plus a
/// `(+N others)` count, keeping the summarized cell narrow. A transitive dependency (`!direct`) is
/// the members that *reach* it, prefixed `via …` so it does not read as a direct declarer.
fn members_cell(members: &[MemberRef], list_all: bool, paths: bool, direct: bool) -> String {
    let mut labels: Vec<String> = members
        .iter()
        .map(|member| {
            if paths {
                path_label(&member.path)
            } else {
                member.name.clone()
            }
        })
        .collect();
    labels.sort();
    labels.dedup();
    if labels.is_empty() {
        return String::new();
    }
    let cell = if list_all {
        labels.join("\n")
    } else {
        // Show the shortest label first to keep the column narrow; alphabetical breaks length ties.
        let first = labels
            .iter()
            .min_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
            .cloned()
            .unwrap_or_default();
        match labels.len() - 1 {
            0 => first,
            1 => format!("{first} (+1 other)"),
            n => format!("{first} (+{n} others)"),
        }
    };
    if direct { cell } else { format!("via {cell}") }
}

/// Render the `outdated` report.
#[must_use]
pub fn render_outdated(
    summary: &OutdatedSummary,
    items: &[OutdatedItem],
    warnings: &[Diagnostic],
    errors: &[Diagnostic],
    opts: &RenderOptions,
) -> String {
    let RenderOptions {
        use_color,
        list_packages,
        paths,
        show_projects,
        // Tips only appear in the mutation reports (`upgrade`/`fix`).
        no_suggestions: _,
    } = *opts;
    let mut out = String::new();
    if items.is_empty() {
        if summary.total == 0 && !errors.is_empty() {
            out.push_str("No dependencies could be evaluated.\n");
        } else if summary.total == 0 || summary.total == summary.up_to_date {
            out.push_str("All dependencies are up to date.\n");
        } else {
            out.push_str("No dependencies match the current display filters.\n");
        }
    } else {
        let used_by = has_attribution(items, |it| &it.members);
        let project = show_projects && has_distinct_project(items, |it| it.project.as_str());
        let mut t = base_table(use_color);
        let mut header = vec!["Package"];
        if used_by {
            header.push("Used by");
        }
        if project {
            header.push("Project");
        }
        // "Cooldown" shows the newest candidate's age against its window as `age/window`
        // (e.g. `13d/14d` — almost adoptable); rows with no candidate show the bare window.
        header.extend(["Current", "Adoptable", "Latest", "Cooldown", "Status"]);
        t.set_header(header);
        for it in items {
            // Draw the eye to the version that can be taken now — including on a held row, where it
            // is the newest version safe to manually pin to.
            let adoptable = match &it.adoptable_target {
                Some(version) => cell_colored(version.clone(), ADOPTABLE_COLOR, use_color),
                None => Cell::new("—"),
            };
            let latest = it
                .latest
                .as_ref()
                .map_or_else(|| "—".into(), |l| l.version.clone());
            // No upgrade candidate: when the dep is up to date the `latest` column *is* the current
            // pin, so its age is the pin's — show that over the window for a consistent `age/window`
            // cell instead of a bare window. Keyed on `latest == current` (the real invariant), so a
            // commit pin (whose `latest` differs from the pin) and the rare up-to-date-with-an-
            // unclassifiable-newer-release case (whose `latest` is some other version) both fall
            // through to the bare window rather than borrowing the wrong version's age.
            let current_age_days = it.candidate_age_days.or_else(|| {
                it.latest
                    .as_ref()
                    .filter(|latest| latest.version == it.current)
                    .and_then(|latest| latest.age_days)
            });
            let cooldown =
                cooldown_cell(&it.window, current_age_days, it.cooldown_version.as_deref());
            let mut row = vec![cell_colored(it.name.clone(), PACKAGE_COLOR, use_color)];
            if used_by {
                row.push(Cell::new(members_cell(
                    &it.members,
                    list_packages,
                    paths,
                    it.direct,
                )));
            }
            if project {
                row.push(Cell::new(path_label(&it.project)));
            }
            row.push(Cell::new(&it.current));
            row.push(adoptable);
            row.push(Cell::new(latest));
            row.push(Cell::new(cooldown));
            // A blocked row names the conflicting requirer inline ("blocked by <pkg>") so the matured
            // target reads as "wanted but held out of the graph by …", mirroring the `upgrade` skip.
            let status_text = match (it.status, &it.blocked_by) {
                (OutdatedStatus::Blocked, Some(blocker)) => format!("blocked by {blocker}"),
                _ => status_label(it.status).to_string(),
            };
            row.push(cell_colored(
                status_text,
                status_color(it.status),
                use_color,
            ));
            t.add_row(row);
        }
        out.push_str(&dim_borders(&t.to_string(), use_color));
        out.push('\n');
    }
    let _ = write!(
        out,
        "\n{} adoptable · {} blocked · {} in cooldown · {} up-to-date · {} exempt · {} held · {} unknown-age",
        summary.adoptable,
        summary.blocked,
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
#[must_use]
pub fn render_check(
    meta: &CheckMeta,
    summary: &CheckSummary,
    items: &[CheckItem],
    warnings: &[Diagnostic],
    errors: &[Diagnostic],
    opts: &RenderOptions,
) -> String {
    let RenderOptions {
        use_color,
        list_packages,
        paths,
        show_projects,
        // Tips only appear in the mutation reports (`upgrade`/`fix`).
        no_suggestions: _,
    } = *opts;
    let mut out = String::new();
    if items.is_empty() && errors.is_empty() {
        let _ = writeln!(
            out,
            "✓ {} dependencies pass the cooldown gate ({} scope).",
            summary.checked, meta.scope
        );
    } else {
        let used_by = has_attribution(items, |it| &it.members);
        let project = show_projects && has_distinct_project(items, |it| it.project.as_str());
        let mut t = base_table(use_color);
        let mut header = vec!["Package"];
        if used_by {
            header.push("Used by");
        }
        if project {
            header.push("Project");
        }
        header.extend(["Version", "Cooldown", "Status", "Notes"]);
        t.set_header(header);
        for it in items {
            let (label, color) = match it.status {
                CheckStatus::Violation => ("violation", Color::Red),
                CheckStatus::Acknowledged => ("acknowledged", Color::Cyan),
                CheckStatus::Allowed => ("allowed", Color::Yellow),
                CheckStatus::UnknownAge => ("unknown age", Color::Magenta),
                CheckStatus::Error => ("error", Color::Red),
            };
            let cooldown = check_cooldown_cell(&it.window, it.age_days);
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
            let mut row = vec![cell_colored(it.name.clone(), PACKAGE_COLOR, use_color)];
            if used_by {
                row.push(Cell::new(members_cell(
                    &it.members,
                    list_packages,
                    paths,
                    it.direct,
                )));
            }
            if project {
                row.push(Cell::new(path_label(&it.project)));
            }
            row.push(Cell::new(&it.current));
            row.push(Cell::new(cooldown));
            row.push(cell_colored(label, color, use_color));
            row.push(Cell::new(notes.join("; ")));
            t.add_row(row);
        }
        out.push_str(&dim_borders(&t.to_string(), use_color));
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "\nchecked {} ({} direct) · {} violations · {} acknowledged · {} allowed · {} exempt · {} unknown-age · {} errors",
        summary.checked,
        summary.direct,
        summary.violations,
        summary.acknowledged,
        summary.allowed,
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
    opts: &RenderOptions,
) -> String {
    render_mutation("upgrade", meta, summary, items, warnings, errors, *opts)
}

/// Render the `fix` report.
#[must_use]
pub fn render_fix(
    meta: &UpgradeMeta,
    summary: &UpgradeSummary,
    items: &[UpgradeItem],
    warnings: &[Diagnostic],
    errors: &[Diagnostic],
    opts: &RenderOptions,
) -> String {
    render_mutation("fix", meta, summary, items, warnings, errors, *opts)
}

/// The (status word, reason detail, color) for one mutation row — both cells share the color. The
/// applied word is per-item, not per-command: a too-fresh pin an `upgrade` rolls back is `downgraded`,
/// not `upgraded`. A held-back cross-major is a `skipped` row whose reason is `needs --major`; a clean
/// apply has an empty reason (Status says it).
fn mutation_status(it: &UpgradeItem) -> (&'static str, String, Color) {
    if it.applied {
        let word = if it.downgrade {
            "downgraded"
        } else {
            "upgraded"
        };
        (word, String::new(), Color::Green)
    } else if let Some(sk) = &it.skipped {
        let detail = if sk.reason == SkipReason::NeedsMajor {
            "needs --major".to_string()
        } else {
            sk.message.clone()
        };
        ("skipped", detail, Color::Yellow)
    } else if let Some(e) = &it.error {
        ("failed", e.message.clone(), Color::Red)
    } else {
        ("planned", String::new(), Color::Cyan)
    }
}

fn render_mutation(
    verb: &str,
    meta: &UpgradeMeta,
    summary: &UpgradeSummary,
    items: &[UpgradeItem],
    warnings: &[Diagnostic],
    errors: &[Diagnostic],
    opts: RenderOptions,
) -> String {
    let RenderOptions {
        use_color,
        list_packages,
        paths,
        show_projects,
        no_suggestions,
    } = opts;
    let mut out = String::new();
    if items.is_empty() {
        let _ = writeln!(out, "Nothing to {verb}.");
    } else {
        let used_by = has_attribution(items, |it| &it.members);
        let project = show_projects && has_distinct_project(items, |it| it.project.as_str());
        let mut t = base_table(use_color);
        let mut header = vec!["Package"];
        if used_by {
            header.push("Used by");
        }
        if project {
            header.push("Project");
        }
        // No "Kind" column: for a `0.x` dep the semver-field kind (e.g. `minor`) contradicts a
        // `needs --major` result, and From/To already shows the size of the jump. "Status" is the
        // colored one-word outcome; "Reason" is the detail — empty for a clean upgrade (both share
        // the row's color).
        header.extend(["From", "To", "Status", "Reason"]);
        t.set_header(header);
        for it in items {
            let (status, detail, color) = mutation_status(it);
            let mut row = vec![cell_colored(it.name.clone(), PACKAGE_COLOR, use_color)];
            if used_by {
                row.push(Cell::new(members_cell(
                    &it.members,
                    list_packages,
                    paths,
                    it.direct,
                )));
            }
            if project {
                row.push(Cell::new(path_label(&it.project)));
            }
            row.push(Cell::new(&it.from));
            row.push(Cell::new(&it.to));
            row.push(cell_colored(status, color, use_color));
            row.push(cell_colored(detail, color, use_color));
            t.add_row(row);
        }
        out.push_str(&dim_borders(&t.to_string(), use_color));
        out.push('\n');
    }
    let lock = match meta.lock_status {
        Some(cooldown_core::LockStatus::Current) => "lock re-verified",
        Some(cooldown_core::LockStatus::Stale) => "lock stale",
        Some(cooldown_core::LockStatus::Unknown) => "lock currency unknown",
        None => "dry-run (lock untouched)",
    };
    // The held-back `needs --major` rows are counted in `skipped`; break out how many so the user
    // sees what is merely a flag away.
    let names = major_held_back(items);
    let needs_major = items
        .iter()
        .filter(|it| {
            it.skipped
                .as_ref()
                .is_some_and(|s| s.reason == SkipReason::NeedsMajor)
        })
        .count();
    let major_note = if needs_major == 0 {
        String::new()
    } else {
        format!(" ({needs_major} need --major)")
    };
    let _ = writeln!(
        out,
        "\n{} applied · {} skipped{} · {} errors · {}",
        summary.applied, summary.skipped, major_note, summary.errors, lock
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
    if !no_suggestions {
        push_major_card(&mut out, &names, use_color);
    }
    push_diagnostics(&mut out, warnings, errors, use_color);
    out
}

/// The distinct package names of the cross-major updates held back as `needs --major` rows (sorted,
/// deduped across projects) — the count and subjects the suggestion card reports.
fn major_held_back(items: &[UpgradeItem]) -> Vec<&str> {
    let mut names: Vec<&str> = items
        .iter()
        .filter(|it| {
            it.skipped
                .as_ref()
                .is_some_and(|s| s.reason == SkipReason::NeedsMajor)
        })
        .map(|it| it.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Colors for the suggestion card (applied only when coloring is on).
const CARD_BORDER: &str = "\x1b[36m"; // cyan box
const CARD_TITLE: &str = "\x1b[1;36m"; // bold-cyan "Tip" title
const CARD_CMD: &str = "\x1b[1;32m"; // bold-green command
const ANSI_RESET: &str = "\x1b[0m";
/// Horizontal padding inside the card, and how far the whole card is indented from the margin.
const CARD_PAD: usize = 2;
const CARD_INDENT: &str = "  ";

fn paint(text: &str, ansi: &str, use_color: bool) -> String {
    if use_color {
        format!("{ansi}{text}{ANSI_RESET}")
    } else {
        text.to_string()
    }
}

/// Render a free-floating "Tip" card suggesting the `cooldown upgrade --major` command that adopts the
/// held-back cross-major updates (plain `--major` takes them all; the user scopes with `-p` to subset).
///
/// It is a hand-drawn rounded box with a titled top border, a colored frame, and the command set
/// apart in green — there is no lightweight Rust equivalent of JS's `boxen`, and drawing it directly
/// keeps full control of color without fighting a table renderer's ANSI-vs-width accounting. Indented
/// so it reads as an aside. Suppressed by `--no-suggestions` (the caller checks that).
fn push_major_card(out: &mut String, names: &[&str], use_color: bool) {
    if names.is_empty() {
        return;
    }
    // Plain `--major` adopts every held-back cross-major (the rows above); a `-p` list would only be
    // needed to take a subset, which the table already makes visible.
    let cmd = String::from("cooldown upgrade --major");
    let subject = if names.len() == 1 {
        "1 package has".to_string()
    } else {
        format!("{} packages have", names.len())
    };
    let them = if names.len() == 1 { "it" } else { "them" };

    // (line, is_command); empty lines are vertical padding inside the box.
    let rows = [
        (String::new(), false),
        (format!("{subject} a major update available."), false),
        (format!("To upgrade {them}, run:"), false),
        (String::new(), false),
        (cmd, true),
        (String::new(), false),
    ];
    let inner = rows
        .iter()
        .map(|(line, _)| line.chars().count())
        .max()
        .unwrap_or(0);
    let span = inner + CARD_PAD * 2;

    // Top border carrying the title: `╭─ Tip ─────╮`.
    let title = "─ Tip ";
    let fill = "─".repeat(span.saturating_sub(title.chars().count()));
    let top = if use_color {
        format!(
            "{CARD_BORDER}╭─ {ANSI_RESET}{CARD_TITLE}Tip{ANSI_RESET}{CARD_BORDER} {fill}╮{ANSI_RESET}"
        )
    } else {
        format!("╭{title}{fill}╮")
    };
    let bar = paint("│", CARD_BORDER, use_color);
    let bottom = paint(&format!("╰{}╯", "─".repeat(span)), CARD_BORDER, use_color);

    let _ = writeln!(out);
    let _ = writeln!(out, "{CARD_INDENT}{top}");
    for (line, is_cmd) in &rows {
        let body = if *is_cmd {
            paint(line, CARD_CMD, use_color)
        } else {
            line.clone()
        };
        let right = " ".repeat(inner - line.chars().count());
        let pad = " ".repeat(CARD_PAD);
        let _ = writeln!(out, "{CARD_INDENT}{bar}{pad}{body}{right}{pad}{bar}");
    }
    let _ = writeln!(out, "{CARD_INDENT}{bottom}");
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
    out.push_str(&dim_borders(&t.to_string(), use_color));
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

#[cfg(test)]
mod tests {
    use super::{
        RenderOptions, check_cooldown_cell, has_distinct_project, members_cell, path_label,
        render_check, render_fix, render_outdated, render_upgrade,
    };
    use crate::{
        BuildInfo, CheckItem, CheckMeta, CheckStatus, CheckSummary, LatestInfo, OutdatedItem,
        OutdatedStatus, OutdatedSummary, SkippedInfo, UpgradeItem, UpgradeMeta, UpgradeSummary,
        Window,
    };
    use cooldown_core::{Diagnostic, DiagnosticKind, MemberRef, SkipReason, UpdateKind};

    /// Members whose name is the given string and whose path is `path/<name>`.
    fn members(names: &[&str]) -> Vec<MemberRef> {
        names
            .iter()
            .map(|name| MemberRef {
                name: (*name).to_string(),
                path: format!("path/{name}"),
            })
            .collect()
    }

    #[test]
    fn cooldown_cell_shows_age_over_window_then_falls_back_to_bare_window() {
        use super::cooldown_cell;
        use crate::Window;

        let window = |clamped_by: Option<&str>| Window {
            min_age_days: 14.0,
            source: "default".into(),
            clamped_by: clamped_by.map(str::to_string),
        };

        // A newer candidate reads as `age/window` — 13 days into a 14-day cooldown.
        assert_eq!(cooldown_cell(&window(None), Some(13.0), None), "13d/14d");
        assert_eq!(cooldown_cell(&window(None), Some(0.02), None), "0d/14d");
        assert_eq!(cooldown_cell(&window(None), Some(1.5), None), "2d/14d");
        // No newer candidate (up to date, commit pin) → just the policy window.
        assert_eq!(cooldown_cell(&window(None), None, None), "14d");
        // A stricter native/registry clamp is appended either way.
        assert_eq!(
            cooldown_cell(&window(Some("native")), Some(2.0), None),
            "2d/14d (≥native)"
        );
        assert_eq!(
            cooldown_cell(&window(Some("native")), None, None),
            "14d (≥native)"
        );
        // Under `--countdown soonest` the cooldown refers to a version no other column names, so it
        // is appended in parentheses (after any clamp).
        assert_eq!(
            cooldown_cell(&window(None), Some(4.0), Some("0.15.17")),
            "4d/14d (0.15.17)"
        );
        assert_eq!(
            cooldown_cell(&window(Some("native")), Some(4.0), Some("0.15.17")),
            "4d/14d (≥native) (0.15.17)"
        );
    }

    #[test]
    fn check_cooldown_cell_always_shows_age_over_window() {
        let window = |clamped_by: Option<&str>| Window {
            min_age_days: 14.0,
            source: "default".into(),
            clamped_by: clamped_by.map(str::to_string),
        };

        assert_eq!(check_cooldown_cell(&window(None), Some(0.02)), "0d/14d");
        assert_eq!(check_cooldown_cell(&window(None), None), "?/14d");
        assert_eq!(
            check_cooldown_cell(&window(Some("native")), Some(2.0)),
            "2d/14d (≥native)"
        );
    }

    #[test]
    fn members_cell_blank_when_unattributed() {
        assert_eq!(members_cell(&[], false, false, true), "");
        assert_eq!(members_cell(&[], true, true, false), "");
    }

    #[test]
    fn members_cell_shows_shortest_name_first_then_count() {
        // The shortest label leads (keeps the column narrow); alphabetical breaks length ties.
        assert_eq!(
            members_cell(&members(&["solo"]), false, false, true),
            "solo"
        );
        assert_eq!(
            members_cell(&members(&["bbb", "aa"]), false, false, true),
            "aa (+1 other)"
        );
        assert_eq!(
            members_cell(
                &members(&["apps/admin", "zz", "apps/web"]),
                false,
                false,
                true
            ),
            "zz (+2 others)"
        );
    }

    #[test]
    fn members_cell_transitive_is_prefixed_via() {
        // A transitive dep is attributed to the members that pull it in, prefixed `via`.
        assert_eq!(
            members_cell(&members(&["bbb", "aa"]), false, false, false),
            "via aa (+1 other)"
        );
        // An unattributed transitive is still blank.
        assert_eq!(members_cell(&[], false, false, false), "");
    }

    #[test]
    fn members_cell_lists_all_sorted_on_separate_lines() {
        assert_eq!(
            members_cell(&members(&["b", "a", "c"]), true, false, true),
            "a\nb\nc"
        );
    }

    #[test]
    fn members_cell_paths_mode_uses_path() {
        assert_eq!(
            members_cell(&members(&["pkg"]), false, true, true),
            "path/pkg"
        );
    }

    #[test]
    fn path_label_renders_root_as_dot_slash() {
        assert_eq!(path_label("."), "./");
        assert_eq!(path_label("apps/admin"), "apps/admin");
    }

    #[test]
    fn distinct_project_only_hides_actual_root_path() {
        assert!(!has_distinct_project(&["."], |path| *path));
        assert!(has_distinct_project(&["root"], |path| *path));
    }

    #[test]
    fn check_table_uses_single_cooldown_column() {
        let out = render_check(
            &CheckMeta {
                scope: "lockfile-graph".into(),
                artifact_scope: "environment".into(),
            },
            &CheckSummary {
                checked: 1,
                direct: 1,
                exempt: 0,
                acknowledged: 0,
                allowed: 0,
                unknown_age: 0,
                errors: 0,
                violations: 1,
            },
            &[CheckItem {
                name: "github.com/example/pkg".into(),
                tool: "go".into(),
                project: ".".into(),
                members: Vec::new(),
                registry: Some("proxy.golang.org".into()),
                direct: true,
                current: "v1.2.3".into(),
                published_at: None,
                age_days: Some(13.0),
                window: Window {
                    min_age_days: 14.0,
                    source: "default".into(),
                    clamped_by: None,
                },
                status: CheckStatus::Violation,
                graph_held: false,
                graph_floor: None,
                error: None,
            }],
            &[],
            &[],
            &RenderOptions::default(),
        );

        assert!(out.contains("Cooldown"));
        assert!(!out.contains(" Age "));
        assert!(!out.contains(" Window "));
        assert!(out.contains("13d/14d"));
    }

    #[test]
    fn outdated_cooldown_cell_labels_a_non_latest_countdown() {
        // The ruff `--countdown soonest` row: the cooldown counts down to 0.15.17, which no other
        // column names, so the cell labels it — `4d/7d (0.15.17)`.
        let summary = OutdatedSummary {
            total: 1,
            adoptable: 1,
            blocked: 0,
            in_cooldown: 0,
            up_to_date: 0,
            exempt: 0,
            held: 0,
            unknown_age: 0,
            errors: 0,
        };
        let item = OutdatedItem {
            name: "ruff".into(),
            tool: "uv".into(),
            project: ".".into(),
            registry: None,
            direct: true,
            current: "0.15.15".into(),
            members: Vec::new(),
            window: Window {
                min_age_days: 7.0,
                source: "default".into(),
                clamped_by: None,
            },
            candidate_age_days: Some(4.0),
            cooldown_version: Some("0.15.17".into()),
            status: OutdatedStatus::Adoptable,
            adoptable_target: Some("0.15.16".into()),
            blocked_by: None,
            latest: Some(LatestInfo {
                version: "0.15.18".into(),
                published_at: None,
                age_days: None,
            }),
            error: None,
        };
        let out = render_outdated(&summary, &[item], &[], &[], &RenderOptions::default());
        assert!(
            out.contains("4d/7d (0.15.17)"),
            "cooldown cell should label the soonest version:\n{out}"
        );
    }

    #[test]
    fn outdated_up_to_date_shows_current_age_over_window() {
        // An up-to-date dep has no upgrade candidate, but its cooldown cell still reads `age/window`
        // (the current pin's age, always older than the window) instead of a bare window — `latest`
        // is the current version when up to date, so its age is the pin's.
        let summary = OutdatedSummary {
            total: 1,
            adoptable: 0,
            blocked: 0,
            in_cooldown: 0,
            up_to_date: 1,
            exempt: 0,
            held: 0,
            unknown_age: 0,
            errors: 0,
        };
        let item = OutdatedItem {
            name: "once_cell".into(),
            tool: "cargo".into(),
            project: ".".into(),
            registry: None,
            direct: true,
            current: "1.21.4".into(),
            members: Vec::new(),
            window: Window {
                min_age_days: 7.0,
                source: "default".into(),
                clamped_by: None,
            },
            candidate_age_days: None,
            cooldown_version: None,
            status: OutdatedStatus::UpToDate,
            adoptable_target: None,
            blocked_by: None,
            latest: Some(LatestInfo {
                version: "1.21.4".into(),
                published_at: None,
                age_days: Some(102.0),
            }),
            error: None,
        };
        let out = render_outdated(&summary, &[item], &[], &[], &RenderOptions::default());
        assert!(
            out.contains("102d/7d"),
            "up-to-date cooldown should read age/window, not a bare window:\n{out}"
        );
    }

    #[test]
    fn outdated_project_column_is_opt_in() {
        // Two rows in distinct (non-root) projects: there *is* a project to attribute, but the
        // column only appears once the user opts in with `--show-projects`.
        let summary = OutdatedSummary {
            total: 2,
            adoptable: 2,
            blocked: 0,
            in_cooldown: 0,
            up_to_date: 0,
            exempt: 0,
            held: 0,
            unknown_age: 0,
            errors: 0,
        };
        let item = |project: &str| OutdatedItem {
            name: "ruff".into(),
            tool: "uv".into(),
            project: project.into(),
            registry: None,
            direct: true,
            current: "0.15.15".into(),
            members: Vec::new(),
            window: Window {
                min_age_days: 7.0,
                source: "default".into(),
                clamped_by: None,
            },
            candidate_age_days: Some(4.0),
            cooldown_version: None,
            status: OutdatedStatus::Adoptable,
            adoptable_target: Some("0.15.16".into()),
            blocked_by: None,
            latest: None,
            error: None,
        };
        let items = [item("maintenance/rag"), item("packages/api")];

        // Hidden by default, even though the projects are distinct.
        let hidden = render_outdated(&summary, &items, &[], &[], &RenderOptions::default());
        assert!(
            !hidden.contains("Project"),
            "Project column should be hidden by default:\n{hidden}"
        );
        assert!(!hidden.contains("maintenance/rag"));

        // `--show-projects` brings the column (and the project paths) back.
        let shown = render_outdated(
            &summary,
            &items,
            &[],
            &[],
            &RenderOptions {
                show_projects: true,
                ..RenderOptions::default()
            },
        );
        assert!(
            shown.contains("Project"),
            "Project column should appear under --show-projects:\n{shown}"
        );
        assert!(shown.contains("maintenance/rag"));
    }

    #[test]
    fn empty_filtered_outdated_table_does_not_claim_up_to_date() {
        let summary = OutdatedSummary {
            total: 6,
            adoptable: 0,
            blocked: 0,
            in_cooldown: 0,
            up_to_date: 0,
            exempt: 0,
            held: 6,
            unknown_age: 0,
            errors: 0,
        };

        let out = render_outdated(
            &summary,
            &[],
            &[],
            &[],
            &RenderOptions {
                use_color: false,
                list_packages: false,
                paths: false,
                show_projects: false,
                no_suggestions: false,
            },
        );

        assert!(out.starts_with("No dependencies match the current display filters."));
    }

    /// An `upgrade` item for `name` held back by the major gate (`needs --major`).
    fn needs_major_item(
        name: &str,
        from: &str,
        to: &str,
        kind: UpdateKind,
        project: &str,
    ) -> UpgradeItem {
        UpgradeItem {
            name: name.into(),
            tool: "cargo".into(),
            project: project.into(),
            direct: true,
            downgrade: false,
            members: Vec::new(),
            registry: None,
            from: from.into(),
            to: to.into(),
            kind,
            applied: false,
            skipped: Some(SkippedInfo {
                reason: SkipReason::NeedsMajor,
                message: SkipReason::NeedsMajor.message().to_string(),
                offending: None,
            }),
            error: None,
        }
    }

    fn render_upgrade_of(items: &[UpgradeItem]) -> String {
        render_upgrade(
            &UpgradeMeta {
                applied: false,
                lock_status: None,
                build: BuildInfo {
                    requested: false,
                    ok: None,
                },
            },
            &UpgradeSummary {
                applied: 0,
                skipped: 0,
                errors: 0,
            },
            items,
            &[],
            &[],
            &RenderOptions::default(),
        )
    }

    fn applied_item(name: &str, from: &str, to: &str, downgrade: bool) -> UpgradeItem {
        UpgradeItem {
            name: name.into(),
            tool: "cargo".into(),
            project: ".".into(),
            direct: true,
            downgrade,
            members: Vec::new(),
            registry: None,
            from: from.into(),
            to: to.into(),
            kind: UpdateKind::Minor,
            applied: true,
            skipped: None,
            error: None,
        }
    }

    #[test]
    fn applied_status_word_is_per_item_direction() {
        // A too-fresh pin an `upgrade` rolls back is a downgrade, not an upgrade, even though the
        // command is `upgrade` — so both words can appear in one report.
        let items = [
            applied_item("fs4", "0.13.1", "1.1.0", false),
            applied_item("insta", "1.48.0", "1.47.2", true),
        ];
        let out = render_upgrade_of(&items);
        assert!(
            out.contains("upgraded"),
            "forward move says upgraded:\n{out}"
        );
        assert!(
            out.contains("downgraded"),
            "rollback says downgraded:\n{out}"
        );
    }

    #[test]
    fn upgrade_renders_held_back_majors_as_needs_major_rows_with_a_command() {
        // The dogfooding scenario: a default (major-off) upgrade holds back two cross-major updates.
        // They render as `skipped` rows whose Result is `needs --major`, are broken out in the
        // summary, and the exact command to adopt them is offered in the suggestion card.
        let items = [
            needs_major_item("fs4", "0.13.1", "1.1.0", UpdateKind::Major, "."),
            needs_major_item(
                "toml_edit",
                "0.23.10+spec-1.0.0",
                "0.25.12+spec-1.1.0",
                UpdateKind::Minor,
                ".",
            ),
        ];
        let out = render_upgrade_of(&items);
        assert!(
            out.contains("needs --major"),
            "Result detail missing:\n{out}"
        );
        assert!(
            out.contains("2 need --major"),
            "summary breakout missing:\n{out}"
        );
        assert!(out.contains("fs4") && out.contains("1.1.0"), "{out}");
        // The card offers the plain `--major` command, which adopts both held-back majors at once.
        assert!(
            out.contains("cooldown upgrade --major"),
            "suggestion command missing:\n{out}"
        );
    }

    #[test]
    fn no_suggestions_hides_the_tip_but_keeps_the_rows_and_tally() {
        let items = [needs_major_item(
            "fs4",
            "0.13.1",
            "1.1.0",
            UpdateKind::Major,
            ".",
        )];
        let out = render_upgrade(
            &UpgradeMeta {
                applied: false,
                lock_status: None,
                build: BuildInfo {
                    requested: false,
                    ok: None,
                },
            },
            &UpgradeSummary {
                applied: 0,
                skipped: 0,
                errors: 0,
            },
            &items,
            &[],
            &[],
            &RenderOptions {
                no_suggestions: true,
                ..RenderOptions::default()
            },
        );
        // The row and the tally still appear — only the suggestion card is suppressed.
        assert!(out.contains("needs --major"), "{out}");
        assert!(out.contains("1 need --major"), "{out}");
        assert!(
            !out.contains("cooldown upgrade --major"),
            "the suggestion card must be hidden under --no-suggestions:\n{out}"
        );
        assert!(
            !out.contains("major update available"),
            "the card text must be hidden too:\n{out}"
        );
    }

    #[test]
    fn major_command_dedups_a_package_held_back_in_several_projects() {
        // The same package held back in two projects counts twice in the tally but is ONE distinct
        // name — so the card's subject says "1 package", not "2".
        let items = [
            needs_major_item("widget", "1.0.0", "2.0.0", UpdateKind::Major, "apps/a"),
            needs_major_item("widget", "1.0.0", "2.0.0", UpdateKind::Major, "apps/b"),
        ];
        let out = render_upgrade_of(&items);
        assert!(out.contains("2 need --major"), "{out}");
        assert!(out.contains("cooldown upgrade --major"), "{out}");
        assert!(
            out.contains("1 package has a major update available"),
            "the card subject must dedup the package name:\n{out}"
        );
    }

    #[test]
    fn suggestion_card_is_titled_and_colored_when_color_is_on() {
        let items = [needs_major_item(
            "fs4",
            "0.13.1",
            "1.1.0",
            UpdateKind::Major,
            ".",
        )];
        let out = render_upgrade(
            &UpgradeMeta {
                applied: false,
                lock_status: None,
                build: BuildInfo {
                    requested: false,
                    ok: None,
                },
            },
            &UpgradeSummary {
                applied: 0,
                skipped: 1,
                errors: 0,
            },
            &items,
            &[],
            &[],
            &RenderOptions {
                use_color: true,
                ..RenderOptions::default()
            },
        );
        // The border and the title are separated by color escapes, so check them independently.
        assert!(
            out.contains('╭') && out.contains("Tip"),
            "titled rounded box missing:\n{out}"
        );
        assert!(
            out.contains("cooldown upgrade --major"),
            "command missing:\n{out}"
        );
        assert!(
            out.contains('\u{1b}'),
            "the card should carry color when use_color is on:\n{out}"
        );
    }

    #[test]
    fn empty_fix_report_uses_fix_wording() {
        let out = render_fix(
            &UpgradeMeta {
                applied: false,
                lock_status: Some(cooldown_core::LockStatus::Current),
                build: BuildInfo {
                    requested: false,
                    ok: None,
                },
            },
            &UpgradeSummary {
                applied: 0,
                skipped: 0,
                errors: 0,
            },
            &[],
            &[],
            &[],
            &RenderOptions::default(),
        );

        assert!(out.starts_with("Nothing to fix."));
    }

    #[test]
    fn empty_outdated_table_with_errors_does_not_claim_up_to_date() {
        let summary = OutdatedSummary {
            total: 0,
            adoptable: 0,
            blocked: 0,
            in_cooldown: 0,
            up_to_date: 0,
            exempt: 0,
            held: 0,
            unknown_age: 0,
            errors: 0,
        };
        let errors = [Diagnostic::new(
            DiagnosticKind::LockfileUnreadable,
            "lock unreadable",
        )];

        let out = render_outdated(
            &summary,
            &[],
            &[],
            &errors,
            &RenderOptions {
                use_color: false,
                list_packages: false,
                paths: false,
                show_projects: false,
                no_suggestions: false,
            },
        );

        assert!(out.starts_with("No dependencies could be evaluated."));
    }
}
