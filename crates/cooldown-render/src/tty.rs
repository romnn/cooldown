//! Colorful TTY tables. Rendering is deterministic when `use_color` is false (snapshot tests use
//! that), and styled for the terminal otherwise.

use crate::model::{
    CheckItem, CheckMeta, CheckStatus, CheckSummary, ExplainMeta, ExplainStep, OutdatedItem,
    OutdatedStatus, OutdatedSummary, UpgradeItem, UpgradeMeta, UpgradeSummary, Window,
};
use comfy_table::{Cell, Color, ContentArrangement, Table};
use cooldown_core::{Diagnostic, MemberRef, Status};
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
}

/// Whether the "Used by" column should appear: at least one row attributes a source package.
fn has_attribution<T>(items: &[T], members: impl Fn(&T) -> &[MemberRef]) -> bool {
    items.iter().any(|it| !members(it).is_empty())
}

/// Whether the "Project" column should appear: some row's project is not just the root. A repo whose
/// only project is the root (`.`) gains no column; multi-project trees (uv packages) keep it.
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

/// The `outdated` "Cooldown" cell: the shown candidate's `age/window` (e.g. `13d/14d`) when a
/// newer version exists, or the bare window when there is no candidate. A stricter native /
/// registry clamp is appended as `(≥<source>)`, matching the rest of the report. When the countdown
/// refers to a version no other column names (under `--countdown soonest`), that version is appended
/// in parentheses, e.g. `4d/7d (0.15.17)`.
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
/// `(+N others)` count, keeping the summarized cell narrow.
fn members_cell(members: &[MemberRef], list_all: bool, paths: bool) -> String {
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
    if list_all {
        return labels.join("\n");
    }
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
        let project = has_distinct_project(items, |it| it.project.as_str());
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
            let cooldown = cooldown_cell(
                &it.window,
                it.candidate_age_days,
                it.cooldown_version.as_deref(),
            );
            let mut row = vec![cell_colored(it.name.clone(), PACKAGE_COLOR, use_color)];
            if used_by {
                row.push(Cell::new(members_cell(&it.members, list_packages, paths)));
            }
            if project {
                row.push(Cell::new(path_label(&it.project)));
            }
            row.push(Cell::new(&it.current));
            row.push(adoptable);
            row.push(Cell::new(latest));
            row.push(Cell::new(cooldown));
            row.push(cell_colored(
                status_label(it.status),
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
        let project = has_distinct_project(items, |it| it.project.as_str());
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
                row.push(Cell::new(members_cell(&it.members, list_packages, paths)));
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
    } = opts;
    let mut out = String::new();
    if items.is_empty() {
        let _ = writeln!(out, "Nothing to {verb}.");
    } else {
        let used_by = has_attribution(items, |it| &it.members);
        let project = has_distinct_project(items, |it| it.project.as_str());
        let mut t = base_table(use_color);
        let mut header = vec!["Package"];
        if used_by {
            header.push("Used by");
        }
        if project {
            header.push("Project");
        }
        header.extend(["From", "To", "Kind", "Result"]);
        t.set_header(header);
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
            let mut row = vec![cell_colored(it.name.clone(), PACKAGE_COLOR, use_color)];
            if used_by {
                row.push(Cell::new(members_cell(&it.members, list_packages, paths)));
            }
            if project {
                row.push(Cell::new(path_label(&it.project)));
            }
            row.push(Cell::new(&it.from));
            row.push(Cell::new(&it.to));
            row.push(Cell::new(format!("{:?}", it.kind).to_lowercase()));
            row.push(cell_colored(label, color, use_color));
            t.add_row(row);
        }
        out.push_str(&dim_borders(&t.to_string(), use_color));
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
        render_check, render_fix, render_outdated,
    };
    use crate::{
        BuildInfo, CheckItem, CheckMeta, CheckStatus, CheckSummary, LatestInfo, OutdatedItem,
        OutdatedStatus, OutdatedSummary, UpgradeMeta, UpgradeSummary, Window,
    };
    use cooldown_core::{Diagnostic, DiagnosticKind, MemberRef};

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
        assert_eq!(members_cell(&[], false, false), "");
        assert_eq!(members_cell(&[], true, true), "");
    }

    #[test]
    fn members_cell_shows_shortest_name_first_then_count() {
        // The shortest label leads (keeps the column narrow); alphabetical breaks length ties.
        assert_eq!(members_cell(&members(&["solo"]), false, false), "solo");
        assert_eq!(
            members_cell(&members(&["bbb", "aa"]), false, false),
            "aa (+1 other)"
        );
        assert_eq!(
            members_cell(&members(&["apps/admin", "zz", "apps/web"]), false, false),
            "zz (+2 others)"
        );
    }

    #[test]
    fn members_cell_lists_all_sorted_on_separate_lines() {
        assert_eq!(
            members_cell(&members(&["b", "a", "c"]), true, false),
            "a\nb\nc"
        );
    }

    #[test]
    fn members_cell_paths_mode_uses_path() {
        assert_eq!(members_cell(&members(&["pkg"]), false, true), "path/pkg");
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
    fn empty_filtered_outdated_table_does_not_claim_up_to_date() {
        let summary = OutdatedSummary {
            total: 6,
            adoptable: 0,
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
            },
        );

        assert!(out.starts_with("No dependencies match the current display filters."));
    }

    #[test]
    fn empty_fix_report_uses_fix_wording() {
        let out = render_fix(
            &UpgradeMeta {
                applied: false,
                lock_verified: Some(true),
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
            },
        );

        assert!(out.starts_with("No dependencies could be evaluated."));
    }
}
