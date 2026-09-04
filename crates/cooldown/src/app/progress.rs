//! Human-facing progress for slow dependency operations.
//!
//! A run drives its tools in concurrent lanes, so several projects can be live at once.
//! [`Progress`] owns the run-level display (the tools bar and the per-tool completion counts);
//! each live project gets its own [`ProjectProgress`] block, which owns that project's rows and
//! counters and clears them when it drops.
//! Interactive terminals get a stable multi-line display.
//! Redirected stderr and diagnostic-log runs get plain, non-colored lines; every per-project
//! line names its tool and project, so a transcript stays interpretable when lanes interleave.
//! The default is silent, which keeps library callers and tests free of unsolicited output.

use super::change_key::{ChangeTargetKey, change_target_key};
use cooldown_core::{Change, ToolId};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// Run-scoped progress reporting: the run-level rows plus the factory for per-project blocks.
#[derive(Clone, Default)]
pub struct Progress {
    inner: Option<Arc<ProgressInner>>,
}

impl fmt::Debug for Progress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Progress")
            .field("enabled", &self.inner.is_some())
            .finish()
    }
}

struct ProgressInner {
    output: Output,
    run: Mutex<RunTracker>,
}

impl Drop for ProgressInner {
    fn drop(&mut self) {
        let mut run = lock(&self.run);
        clear_interactive(self, &mut run);
    }
}

enum Output {
    Interactive(Interactive),
    Plain,
}

struct Interactive {
    multi: MultiProgress,
    colors: bool,
    tools: ProgressBar,
}

/// The run-level counters: how many projects each tool still has to finish, and how many tools
/// have finished.
#[derive(Default)]
struct RunTracker {
    remaining_projects: HashMap<&'static str, usize>,
    completed_tools: u64,
    total_tools: u64,
    cleared: bool,
    /// How many project blocks finished, counted before any guard so a test sees an over-count.
    #[cfg(test)]
    finished_blocks: u64,
}

/// One project's counters, owned by its [`ProjectProgress`] so concurrent projects never share
/// or reset each other's.
#[derive(Default)]
struct ProjectTracker {
    active_packages: BTreeMap<String, usize>,
    completed_packages: u64,
    package_total: u64,
    candidates: CandidateTracker,
}

#[derive(Default)]
struct CandidateTracker {
    targets: HashSet<ChangeTargetKey>,
    decided: HashSet<ChangeTargetKey>,
    policy_passes: u64,
    resolver_operations: u64,
}

impl CandidateTracker {
    fn start(changes: &[Change]) -> Self {
        Self {
            targets: changes.iter().map(change_target_key).collect(),
            ..Self::default()
        }
    }

    fn decide(&mut self, changes: &[Change]) {
        for change in changes {
            let key = change_target_key(change);
            if self.targets.contains(&key) {
                self.decided.insert(key);
            }
        }
    }

    fn begin_policy_pass(&mut self) {
        self.policy_passes = self.policy_passes.saturating_add(1);
    }

    fn begin_resolver_operation(&mut self) {
        self.resolver_operations = self.resolver_operations.saturating_add(1);
    }

    fn is_complete(&self) -> bool {
        self.decided.len() == self.targets.len()
    }

    fn status(&self, detail: &str) -> String {
        let decisions = match (self.decided.len(), self.targets.len()) {
            (0, 0) => "no candidates".to_string(),
            (0, total) => format!("{total} decisions pending"),
            (decided, total) => format!("{decided}/{total} decided"),
        };
        let mut parts = vec![decisions];
        if self.policy_passes > 0 {
            parts.push(format!("policy pass {}", self.policy_passes));
        }
        if self.resolver_operations > 0 {
            parts.push(format!("resolver op {}", self.resolver_operations));
        }
        if !detail.is_empty() {
            parts.push(detail.to_string());
        }
        parts.join(" · ")
    }
}

/// One live project's progress: its rows on the display and its own package and candidate
/// counters.
///
/// Cloning shares the block, so a fetch fan-out can report into it from many tasks.
/// The block is removed, and the project counted complete for its tool, when the last clone
/// drops — so a command path that returns early still finishes the project.
#[derive(Clone, Default)]
pub(crate) struct ProjectProgress {
    inner: Option<Arc<ProjectInner>>,
}

impl fmt::Debug for ProjectProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectProgress")
            .field("enabled", &self.inner.is_some())
            .finish()
    }
}

struct ProjectInner {
    run: Arc<ProgressInner>,
    tool: &'static str,
    project: String,
    rows: Option<ProjectRows>,
    tracker: Mutex<ProjectTracker>,
}

impl Drop for ProjectInner {
    fn drop(&mut self) {
        if let Some(rows) = &self.rows {
            rows.remove();
        }
        self.run.finish_project(self.tool);
    }
}

/// The display rows of one project block: a header carrying the tool, project, and phase, then
/// the packages bar and the candidates row, each inserted only once the project first reports
/// it.
/// A block therefore costs one to three rows rather than four, so six live tools still fit a
/// 24-line terminal; past what the terminal holds, the display drops the newest rows rather
/// than corrupting the screen.
struct ProjectRows {
    multi: MultiProgress,
    colors: bool,
    project: String,
    header: ProgressBar,
    packages: OnceLock<ProgressBar>,
    candidates: OnceLock<ProgressBar>,
}

impl ProjectRows {
    /// Appends a block below every row already on the display, so concurrent blocks stack in
    /// the order their projects started.
    fn add(ui: &Interactive, tool: &'static str, project: &str) -> Self {
        let header = ui.multi.add(ProgressBar::no_length());
        header.set_style(status_style("tool", "magenta", ui.colors));
        header.set_prefix(tool.to_string());
        let rows = ProjectRows {
            multi: ui.multi.clone(),
            colors: ui.colors,
            project: project.to_string(),
            header,
            packages: OnceLock::new(),
            candidates: OnceLock::new(),
        };
        rows.set_phase("starting");
        rows
    }

    fn set_phase(&self, phase: &str) {
        self.header
            .set_message(format!("{} · {phase}", display_project(&self.project)));
    }

    /// The packages row, inserted right below the header the first time a fetch starts.
    fn packages(&self) -> &ProgressBar {
        self.packages.get_or_init(|| {
            let bar = self
                .multi
                .insert_after(&self.header, ProgressBar::no_length());
            bar.set_style(status_style("packages", "blue", self.colors));
            bar.set_prefix("packages");
            bar
        })
    }

    /// The candidates row, inserted below the packages row (or the header) the first time
    /// candidates are judged.
    fn candidates(&self) -> &ProgressBar {
        self.candidates.get_or_init(|| {
            let anchor = self.packages.get().unwrap_or(&self.header);
            let bar = self.multi.insert_after(anchor, ProgressBar::no_length());
            bar.set_style(candidate_style("green", self.colors));
            bar.set_prefix("candidates");
            bar.enable_steady_tick(Duration::from_secs(1));
            bar
        })
    }

    fn remove(&self) {
        let rows = std::iter::once(&self.header)
            .chain(self.packages.get())
            .chain(self.candidates.get());
        for bar in rows {
            bar.finish_and_clear();
            self.multi.remove(bar);
        }
    }
}

impl Progress {
    /// Create a multi-line terminal display, optionally with ANSI colors.
    #[must_use]
    pub fn interactive(colors: bool) -> Self {
        console::set_colors_enabled_stderr(colors);
        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        let tools = multi.add(ProgressBar::no_length());
        tools.set_prefix("tools");
        tools.set_message("discovering work");
        multi.set_move_cursor(true);
        multi.set_draw_target(ProgressDrawTarget::stderr_with_hz(20));
        tools.set_style(status_style("tools", "cyan", colors));

        Self {
            inner: Some(Arc::new(ProgressInner {
                output: Output::Interactive(Interactive {
                    multi,
                    colors,
                    tools,
                }),
                run: Mutex::new(RunTracker::default()),
            })),
        }
    }

    /// Create a non-colored, line-oriented progress transcript on stderr.
    #[must_use]
    pub fn plain() -> Self {
        Self {
            inner: Some(Arc::new(ProgressInner {
                output: Output::Plain,
                run: Mutex::new(RunTracker::default()),
            })),
        }
    }

    /// How many tools have had every project counted complete; `0` without progress.
    #[cfg(test)]
    pub(crate) fn completed_tools(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |inner| lock(&inner.run).completed_tools)
    }

    /// How many project blocks have finished, guarded or not; `0` without progress.
    #[cfg(test)]
    pub(crate) fn finished_blocks(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |inner| lock(&inner.run).finished_blocks)
    }

    /// `project_tools` carries one entry per in-scope project (its tool), so per-tool project
    /// counts fall out of the multiplicity.
    pub(crate) fn start_run(&self, project_tools: &[ToolId]) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut run = lock(&inner.run);
        run.remaining_projects.clear();
        for tool in project_tools {
            *run.remaining_projects.entry(tool.as_str()).or_default() += 1;
        }
        run.completed_tools = 0;
        run.total_tools = u64::try_from(run.remaining_projects.len()).unwrap_or(u64::MAX);
        run.cleared = false;
        match &inner.output {
            Output::Interactive(ui) => {
                ui.tools.set_style(bar_style("tools", "cyan", ui.colors));
                ui.tools.set_length(run.total_tools);
                ui.tools.set_position(0);
                ui.tools.set_message("0 tools complete");
            }
            Output::Plain => write_line(&format!(
                "progress     {:>3}/{:<3} tools complete",
                0, run.total_tools
            )),
        }
    }

    /// A run-level status line (discovery, policy loading) from before any project is live.
    pub(crate) fn phase(&self, message: impl AsRef<str>) {
        let Some(inner) = &self.inner else {
            return;
        };
        let message = message.as_ref();
        match &inner.output {
            Output::Interactive(ui) => ui.tools.set_message(message.to_string()),
            Output::Plain => write_line(&format!("progress     {:<10} {message}", "phase")),
        }
    }

    /// Opens one project's block; see [`ProjectProgress`] for its lifetime.
    pub(crate) fn project(&self, tool: ToolId, project: &str) -> ProjectProgress {
        let Some(inner) = &self.inner else {
            return ProjectProgress::default();
        };
        let tool_name = tool.as_str();
        let rows = match &inner.output {
            Output::Interactive(ui) => Some(ProjectRows::add(ui, tool_name, project)),
            Output::Plain => {
                write_line(&plain_status(tool_name, project, "starting", ""));
                None
            }
        };
        ProjectProgress {
            inner: Some(Arc::new(ProjectInner {
                run: Arc::clone(inner),
                tool: tool_name,
                project: project.to_string(),
                rows,
                tracker: Mutex::new(ProjectTracker::default()),
            })),
        }
    }

    pub(crate) fn finish_run(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut run = lock(&inner.run);
        clear_interactive(inner, &mut run);
    }
}

impl ProgressInner {
    fn finish_project(&self, tool: &'static str) {
        let mut run = lock(&self.run);
        #[cfg(test)]
        {
            run.finished_blocks += 1;
        }
        let Some(remaining) = run.remaining_projects.get_mut(tool) else {
            return;
        };
        if *remaining == 0 {
            return;
        }
        *remaining -= 1;
        if *remaining != 0 {
            return;
        }
        run.completed_tools = run.completed_tools.saturating_add(1);
        match &self.output {
            Output::Interactive(ui) => {
                ui.tools.set_position(run.completed_tools);
                ui.tools.set_message(format!("{tool} complete"));
            }
            Output::Plain => write_line(&format!(
                "progress     {:>3}/{:<3} tools complete ({tool})",
                run.completed_tools, run.total_tools
            )),
        }
        if run.completed_tools == run.total_tools {
            clear_interactive(self, &mut run);
        }
    }
}

impl ProjectProgress {
    pub(crate) fn phase(&self, message: impl AsRef<str>) {
        let Some(inner) = &self.inner else {
            return;
        };
        let message = message.as_ref();
        match &inner.rows {
            Some(rows) => rows.set_phase(message),
            None => write_line(&plain_status(inner.tool, &inner.project, "phase", message)),
        }
    }

    pub(crate) fn packages(&self, total: usize, message: impl AsRef<str>) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock(&inner.tracker);
        tracker.active_packages.clear();
        tracker.completed_packages = 0;
        tracker.package_total = usize_to_u64(total);
        match &inner.rows {
            Some(rows) => {
                rows.packages()
                    .set_style(bar_style("packages", "blue", rows.colors));
                rows.packages().set_length(tracker.package_total);
                rows.packages().set_position(0);
                rows.packages().set_message(if total == 0 {
                    "complete".to_string()
                } else {
                    message.as_ref().to_string()
                });
            }
            None => write_line(&plain_status(
                inner.tool,
                &inner.project,
                "packages",
                &format!("{} ({total})", message.as_ref()),
            )),
        }
    }

    pub(crate) fn package_started(&self, name: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock(&inner.tracker);
        *tracker.active_packages.entry(name.to_string()).or_default() += 1;
        if let Some(rows) = &inner.rows {
            rows.packages()
                .set_message(active_message(&tracker.active_packages));
        }
    }

    pub(crate) fn package_finished(&self, name: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock(&inner.tracker);
        let Some(active) = tracker.active_packages.get_mut(name) else {
            return;
        };
        *active -= 1;
        if *active == 0 {
            tracker.active_packages.remove(name);
        }
        tracker.completed_packages = tracker
            .completed_packages
            .saturating_add(1)
            .min(tracker.package_total);
        match &inner.rows {
            Some(rows) => {
                rows.packages().set_position(tracker.completed_packages);
                if tracker.completed_packages == tracker.package_total {
                    rows.packages().set_message("complete");
                } else if tracker.active_packages.is_empty() {
                    rows.packages().set_message("waiting");
                } else {
                    rows.packages()
                        .set_message(active_message(&tracker.active_packages));
                }
            }
            None => write_line(&plain_status(
                inner.tool,
                &inner.project,
                "fetched",
                &format!(
                    "{}/{} {name}",
                    tracker.completed_packages, tracker.package_total
                ),
            )),
        }
    }

    pub(crate) fn candidates(&self, changes: &[Change], message: impl AsRef<str>) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock(&inner.tracker);
        tracker.candidates = CandidateTracker::start(changes);
        let detail = if changes.is_empty() {
            "complete"
        } else {
            message.as_ref()
        };
        let status = tracker.candidates.status(detail);
        match &inner.rows {
            Some(rows) => {
                rows.candidates()
                    .set_style(candidate_style("green", rows.colors));
                rows.candidates().reset_elapsed();
                rows.candidates().set_message(status);
            }
            None => write_line(&plain_status(
                inner.tool,
                &inner.project,
                "candidates",
                &status,
            )),
        }
    }

    fn resolver_operation(&self, change: &Change) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock(&inner.tracker);
        tracker.candidates.begin_resolver_operation();
        let detail = format!("{} {} → {}", change.package.name, change.from, change.to);
        let message = tracker.candidates.status(&detail);
        match &inner.rows {
            Some(rows) => rows.candidates().set_message(message),
            None => write_line(&plain_status(
                inner.tool,
                &inner.project,
                "candidate",
                &message,
            )),
        }
    }

    pub(crate) fn policy_pass(&self, changes: &[Change]) {
        let Some(first) = changes.first() else {
            return;
        };
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock(&inner.tracker);
        tracker.candidates.begin_policy_pass();
        let detail = if changes.len() == 1 {
            format!("{} {} → {}", first.package.name, first.from, first.to)
        } else {
            format!(
                "{} {} → {} (+{} targets)",
                first.package.name,
                first.from,
                first.to,
                changes.len() - 1
            )
        };
        let message = tracker.candidates.status(&detail);
        match &inner.rows {
            Some(rows) => rows.candidates().set_message(message),
            None => write_line(&plain_status(inner.tool, &inner.project, "pass", &message)),
        }
    }

    pub(crate) fn candidates_decided(&self, changes: &[Change]) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock(&inner.tracker);
        tracker.candidates.decide(changes);
        let detail = if tracker.candidates.is_complete() {
            "complete"
        } else {
            ""
        };
        let status = tracker.candidates.status(detail);
        match &inner.rows {
            Some(rows) => rows.candidates().set_message(status),
            None => write_line(&plain_status(
                inner.tool,
                &inner.project,
                "decided",
                &status,
            )),
        }
    }
}

impl cooldown_core::ApplyObserver for ProjectProgress {
    fn candidate_started(&self, change: &Change) {
        self.resolver_operation(change);
    }
}

fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    match state.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn clear_interactive(inner: &ProgressInner, run: &mut RunTracker) {
    if run.cleared {
        return;
    }
    run.cleared = true;
    if let Output::Interactive(ui) = &inner.output {
        ui.tools.finish_and_clear();
        if let Err(error) = ui.multi.clear() {
            tracing::debug!(error = %error, "could not clear progress display");
        }
    }
}

fn bar_style(prefix: &str, color: &str, colors: bool) -> ProgressStyle {
    let template = bar_template(color, colors);
    style_or_default(&template, prefix).progress_chars("━━╸─")
}

fn bar_template(color: &str, colors: bool) -> String {
    if colors {
        format!(
            "{{prefix:>12.bold.{color}}} [{{bar:32.{color}/black}}] \
             {{pos:>3}}/{{len:<3}} {{msg:.bold}}"
        )
    } else {
        "{prefix:>12} [{bar:32}] {pos:>3}/{len:<3} {msg}".to_string()
    }
}

fn status_style(prefix: &str, color: &str, colors: bool) -> ProgressStyle {
    let template = status_template(color, colors);
    style_or_default(&template, prefix)
}

fn status_template(color: &str, colors: bool) -> String {
    if colors {
        format!("{{prefix:>12.bold.{color}}} {{msg}}")
    } else {
        "{prefix:>12} {msg}".to_string()
    }
}

fn candidate_style(color: &str, colors: bool) -> ProgressStyle {
    let template = candidate_template(color, colors);
    style_or_default(&template, "candidates")
}

fn candidate_template(color: &str, colors: bool) -> String {
    if colors {
        format!("{{prefix:>12.bold.{color}}} [{{elapsed_precise:.dim}}] {{msg}}")
    } else {
        "{prefix:>12} [{elapsed_precise}] {msg}".to_string()
    }
}

fn style_or_default(template: &str, prefix: &str) -> ProgressStyle {
    match ProgressStyle::with_template(template) {
        Ok(style) => style,
        Err(error) => {
            tracing::debug!(%error, prefix, "invalid built-in progress style");
            ProgressStyle::default_bar()
        }
    }
}

/// One plain transcript line, naming the project it belongs to so interleaved lanes stay legible.
fn plain_status(tool: &str, project: &str, kind: &str, message: &str) -> String {
    format!(
        "{tool:>12}  {:<20}  {kind:<10} {message}",
        display_project(project)
    )
}

fn display_project(project: &str) -> &str {
    if project.is_empty() { "." } else { project }
}

fn active_message(active: &BTreeMap<String, usize>) -> String {
    let Some(first) = active.first_key_value().map(|(name, _)| name) else {
        return "complete".to_string();
    };
    let active_count = active.values().sum::<usize>();
    if active_count == 1 {
        first.clone()
    } else {
        format!("{first} (+{} active)", active_count - 1)
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn write_line(message: &str) {
    let result = writeln!(std::io::stderr().lock(), "{message}");
    if let Err(error) = result
        && error.kind() != std::io::ErrorKind::BrokenPipe
    {
        tracing::debug!(error = %error, "could not write progress message");
    }
}

#[cfg(test)]
mod tests {
    use super::{Progress, ProjectProgress};
    use cooldown_core::{Change, MemberRef, PackageId, ToolId, UpdateKind, Version};

    const CARGO: ToolId = ToolId("cargo");
    const GO: ToolId = ToolId("go");

    #[test]
    fn tools_complete_only_after_their_last_project() {
        let progress = Progress::plain();
        // One entry per project: two cargo projects, one go project.
        progress.start_run(&[CARGO, CARGO, GO]);

        drop(progress.project(CARGO, "a"));
        assert_eq!(completed_tools(&progress), 0);
        drop(progress.project(GO, "."));
        assert_eq!(completed_tools(&progress), 1);
        drop(progress.project(CARGO, "b"));
        assert_eq!(completed_tools(&progress), 2);
    }

    #[test]
    fn duplicate_package_names_remain_active_until_every_fetch_finishes() {
        let progress = Progress::plain();
        let project = progress.project(CARGO, ".");
        project.packages(2, "fetching");

        project.package_started("shared");
        project.package_started("shared");
        project.package_finished("shared");
        assert_eq!(active_packages(&project), 1);

        project.package_finished("shared");
        assert_eq!(active_packages(&project), 0);
        assert_eq!(completed_packages(&project), 2);
    }

    #[test]
    fn a_gap_between_package_fetches_is_not_reported_as_completion() {
        let progress = Progress::plain();
        let project = progress.project(CARGO, ".");
        project.packages(2, "fetching");

        project.package_started("first");
        project.package_finished("first");

        assert_eq!(completed_packages(&project), 1);
        assert_eq!(package_total(&project), 2);
    }

    /// Two lanes report into two live blocks at once; each keeps its own counters, and a block
    /// finishing while its sibling is mid-fetch leaves the sibling's counters alone.
    #[test]
    fn concurrent_projects_keep_separate_counters() {
        let progress = Progress::plain();
        progress.start_run(&[CARGO, GO]);
        let cargo = progress.project(CARGO, "crates/app");
        let go = progress.project(GO, "services/api");
        cargo.packages(2, "fetching");
        go.packages(3, "fetching");

        // Interleaved fetches land on their own project.
        cargo.package_started("serde");
        go.package_started("golang.org/x/net");
        go.package_finished("golang.org/x/net");
        cargo.package_finished("serde");
        assert_eq!(completed_packages(&cargo), 1);
        assert_eq!(package_total(&cargo), 2);
        assert_eq!(completed_packages(&go), 1);
        assert_eq!(package_total(&go), 3);

        // The cargo lane finishes first; the go lane's block and counters survive it.
        go.package_started("golang.org/x/text");
        drop(cargo);
        assert_eq!(completed_tools(&progress), 1);
        assert_eq!(active_packages(&go), 1);
        assert_eq!(completed_packages(&go), 1);
        drop(go);
        assert_eq!(completed_tools(&progress), 2);
    }

    /// A fetch fan-out clones the block; the project is complete only when every clone is gone.
    #[test]
    fn a_project_finishes_when_its_last_clone_drops() {
        let progress = Progress::plain();
        progress.start_run(&[CARGO]);
        let project = progress.project(CARGO, ".");
        let worker = project.clone();

        drop(project);
        assert_eq!(completed_tools(&progress), 0);
        drop(worker);
        assert_eq!(completed_tools(&progress), 1);
    }

    #[test]
    fn direct_candidates_for_distinct_members_are_counted_separately() {
        let progress = Progress::plain();
        let project = progress.project(CARGO, ".");
        let first = member_change("first");
        let second = member_change("second");
        project.candidates(&[first.clone(), second.clone()], "checking");

        project.candidates_decided(&[first, second]);

        assert_eq!(decided_candidates(&project), 2);
    }

    #[test]
    fn candidates_outside_the_current_operation_do_not_change_its_count() {
        let progress = Progress::plain();
        let project = progress.project(CARGO, ".");
        let expected = member_change("expected");
        let unrelated = member_change("unrelated");
        project.candidates(std::slice::from_ref(&expected), "checking");

        project.candidates_decided(&[unrelated]);

        assert_eq!(decided_candidates(&project), 0);
    }

    #[test]
    fn resolver_operations_advance_while_candidate_decisions_remain_pending() {
        let progress = Progress::plain();
        let project = progress.project(CARGO, ".");
        let first = member_change("first");
        let second = member_change("second");
        project.candidates(&[first.clone(), second.clone()], "checking");
        project.policy_pass(&[first.clone(), second]);

        cooldown_core::ApplyObserver::candidate_started(&project, &first);
        cooldown_core::ApplyObserver::candidate_started(&project, &first);

        assert_eq!(decided_candidates(&project), 0);
        assert_eq!(
            candidate_summary(&project, "shared 1.0.0 → 2.0.0"),
            "2 decisions pending · policy pass 1 · resolver op 2 · shared 1.0.0 → 2.0.0"
        );

        project.candidates_decided(std::slice::from_ref(&first));
        assert_eq!(
            candidate_summary(&project, ""),
            "1/2 decided · policy pass 1 · resolver op 2"
        );
    }

    /// A block opens as one header row; the packages and candidates rows appear only once the
    /// project first reports them, so a command that never judges candidates costs two rows.
    #[test]
    fn interactive_rows_appear_on_first_use() {
        let progress = Progress::interactive(false);
        let project = progress.project(CARGO, "crates/app");
        let rows = project
            .inner
            .as_ref()
            .and_then(|inner| inner.rows.as_ref())
            .expect("interactive progress has rows");
        assert!(rows.packages.get().is_none());
        assert!(rows.candidates.get().is_none());

        project.packages(1, "fetching");
        assert!(rows.packages.get().is_some());
        assert!(rows.candidates.get().is_none());

        project.candidates(&[], "checking");
        assert!(rows.candidates.get().is_some());
    }

    #[test]
    fn interactive_progress_rows_have_no_spinners() {
        for colors in [false, true] {
            for template in [
                super::bar_template("green", colors),
                super::status_template("green", colors),
                super::candidate_template("green", colors),
            ] {
                assert!(!template.contains("{spinner"));
                assert!(indicatif::ProgressStyle::with_template(&template).is_ok());
            }
        }
    }

    #[test]
    fn candidate_work_is_indeterminate_and_time_aware() {
        let template = super::candidate_template("green", false);

        assert!(template.contains("{elapsed_precise}"));
        assert!(!template.contains("{bar"));
        assert!(!template.contains("{pos"));
        assert!(!template.contains("{len"));
    }

    /// A plain transcript line names its tool and project, so interleaved lanes stay legible.
    #[test]
    fn plain_lines_name_their_project() {
        assert_eq!(
            super::plain_status("cargo", "crates/app", "phase", "resolving"),
            "       cargo  crates/app            phase      resolving"
        );
        assert_eq!(
            super::plain_status("go", "", "fetched", "1/3 x"),
            "          go  .                     fetched    1/3 x"
        );
    }

    fn completed_tools(progress: &Progress) -> u64 {
        progress.completed_tools()
    }

    fn active_packages(project: &ProjectProgress) -> usize {
        let inner = project.inner.as_ref().expect("plain progress is enabled");
        super::lock(&inner.tracker).active_packages.values().sum()
    }

    fn completed_packages(project: &ProjectProgress) -> u64 {
        let inner = project.inner.as_ref().expect("plain progress is enabled");
        super::lock(&inner.tracker).completed_packages
    }

    fn package_total(project: &ProjectProgress) -> u64 {
        let inner = project.inner.as_ref().expect("plain progress is enabled");
        super::lock(&inner.tracker).package_total
    }

    fn decided_candidates(project: &ProjectProgress) -> usize {
        let inner = project.inner.as_ref().expect("plain progress is enabled");
        super::lock(&inner.tracker).candidates.decided.len()
    }

    fn candidate_summary(project: &ProjectProgress, detail: &str) -> String {
        let inner = project.inner.as_ref().expect("plain progress is enabled");
        super::lock(&inner.tracker).candidates.status(detail)
    }

    fn member_change(member: &str) -> Change {
        Change {
            package: PackageId::new(CARGO, "shared", None),
            from: Version::new("1.0.0"),
            to: Version::new("2.0.0"),
            kind: UpdateKind::Major,
            downgrade: false,
            direct: true,
            members: vec![MemberRef {
                name: member.to_string(),
                path: member.to_string(),
            }],
        }
    }
}
