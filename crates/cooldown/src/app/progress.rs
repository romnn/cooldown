//! Human-facing progress for slow dependency operations.

use super::change_key::{ChangeTargetKey, change_target_key};
use cooldown_core::{Change, ToolId};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// Run-scoped progress reporting.
///
/// Interactive terminals get a stable multi-line display. Redirected stderr and diagnostic-log
/// runs get plain, non-colored lines so automation retains an interpretable transcript. The
/// default is silent, which keeps library callers and tests free of unsolicited output.
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
    tracker: Mutex<Tracker>,
}

impl Drop for ProgressInner {
    fn drop(&mut self) {
        let mut tracker = lock_tracker(self);
        clear_interactive(self, &mut tracker);
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
    tool: ProgressBar,
    phase: ProgressBar,
    packages: ProgressBar,
    candidates: ProgressBar,
}

#[derive(Default)]
struct Tracker {
    remaining_projects: HashMap<&'static str, usize>,
    completed_tools: u64,
    total_tools: u64,
    current_tool: &'static str,
    current_project: String,
    active_packages: BTreeMap<String, usize>,
    completed_packages: u64,
    package_total: u64,
    candidates: CandidateTracker,
    cleared: bool,
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

/// Marks one project as complete even when its command path returns early.
pub(crate) struct ProjectProgress {
    progress: Progress,
    tool: &'static str,
}

impl Drop for ProjectProgress {
    fn drop(&mut self) {
        self.progress.finish_project(self.tool);
    }
}

impl Progress {
    /// Create a multi-line terminal display, optionally with ANSI colors.
    #[must_use]
    pub fn interactive(colors: bool) -> Self {
        console::set_colors_enabled_stderr(colors);
        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        let tools = multi.add(ProgressBar::no_length());
        let tool = multi.add(ProgressBar::no_length());
        let phase = multi.add(ProgressBar::no_length());
        let packages = multi.add(ProgressBar::no_length());
        let candidates = multi.add(ProgressBar::no_length());

        tools.set_prefix("tools");
        tool.set_prefix("tool");
        phase.set_prefix("phase");
        packages.set_prefix("packages");
        candidates.set_prefix("candidates");
        tools.set_message("discovering work");
        tool.set_message("waiting");
        phase.set_message("waiting");
        packages.set_message("waiting");
        candidates.set_message("waiting");
        multi.set_move_cursor(true);
        multi.set_draw_target(ProgressDrawTarget::stderr_with_hz(20));
        tools.set_style(status_style("tools", "cyan", colors));
        tool.set_style(status_style("tool", "magenta", colors));
        phase.set_style(status_style("phase", "yellow", colors));
        packages.set_style(status_style("packages", "blue", colors));
        candidates.set_style(candidate_style("green", colors));
        candidates.enable_steady_tick(Duration::from_secs(1));

        Self {
            inner: Some(Arc::new(ProgressInner {
                output: Output::Interactive(Interactive {
                    multi,
                    colors,
                    tools,
                    tool,
                    phase,
                    packages,
                    candidates,
                }),
                tracker: Mutex::new(Tracker::default()),
            })),
        }
    }

    /// Create a non-colored, line-oriented progress transcript on stderr.
    #[must_use]
    pub fn plain() -> Self {
        Self {
            inner: Some(Arc::new(ProgressInner {
                output: Output::Plain,
                tracker: Mutex::new(Tracker::default()),
            })),
        }
    }

    pub(crate) fn start_run(&self, projects: &[(ToolId, String)]) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock_tracker(inner);
        tracker.remaining_projects.clear();
        for (tool, _) in projects {
            *tracker.remaining_projects.entry(tool.as_str()).or_default() += 1;
        }
        tracker.completed_tools = 0;
        tracker.total_tools = u64::try_from(tracker.remaining_projects.len()).unwrap_or(u64::MAX);
        tracker.cleared = false;
        match &inner.output {
            Output::Interactive(ui) => {
                ui.tools.set_style(bar_style("tools", "cyan", ui.colors));
                ui.tools.set_length(tracker.total_tools);
                ui.tools.set_position(0);
                ui.tools.set_message("0 tools complete");
            }
            Output::Plain => write_line(&format!(
                "progress     {:>3}/{:<3} tools complete",
                0, tracker.total_tools
            )),
        }
    }

    pub(crate) fn project(&self, tool: ToolId, project: &str) -> ProjectProgress {
        let tool_name = tool.as_str();
        if let Some(inner) = &self.inner {
            let mut tracker = lock_tracker(inner);
            tracker.current_tool = tool_name;
            tracker.current_project.clear();
            tracker.current_project.push_str(project);
            tracker.active_packages.clear();
            tracker.completed_packages = 0;
            tracker.package_total = 0;
            tracker.candidates = CandidateTracker::default();
            match &inner.output {
                Output::Interactive(ui) => {
                    ui.tool.set_prefix(tool_name.to_string());
                    ui.tool.set_message(project.to_string());
                    ui.phase.set_message("starting");
                    ui.packages
                        .set_style(status_style("packages", "blue", ui.colors));
                    ui.packages.set_position(0);
                    ui.packages.set_message("waiting");
                    ui.candidates.set_style(candidate_style("green", ui.colors));
                    ui.candidates.set_position(0);
                    ui.candidates.set_message("waiting");
                    ui.candidates.reset_elapsed();
                }
                Output::Plain => write_line(&format!(
                    "{tool_name:>12}  {:<20}  starting",
                    display_project(project)
                )),
            }
        }
        ProjectProgress {
            progress: self.clone(),
            tool: tool_name,
        }
    }

    pub(crate) fn phase(&self, message: impl AsRef<str>) {
        let Some(inner) = &self.inner else {
            return;
        };
        let tracker = lock_tracker(inner);
        let message = message.as_ref();
        match &inner.output {
            Output::Interactive(ui) => ui.phase.set_message(message.to_string()),
            Output::Plain => write_line(&plain_status(&tracker, "phase", message)),
        }
    }

    pub(crate) fn packages(&self, total: usize, message: impl AsRef<str>) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock_tracker(inner);
        tracker.active_packages.clear();
        tracker.completed_packages = 0;
        tracker.package_total = usize_to_u64(total);
        match &inner.output {
            Output::Interactive(ui) => {
                ui.packages
                    .set_style(bar_style("packages", "blue", ui.colors));
                ui.packages.set_length(tracker.package_total);
                ui.packages.set_position(0);
                ui.packages.set_message(if total == 0 {
                    "complete".to_string()
                } else {
                    message.as_ref().to_string()
                });
            }
            Output::Plain => write_line(&plain_status(
                &tracker,
                "packages",
                &format!("{} ({total})", message.as_ref()),
            )),
        }
    }

    pub(crate) fn package_started(&self, name: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock_tracker(inner);
        *tracker.active_packages.entry(name.to_string()).or_default() += 1;
        if let Output::Interactive(ui) = &inner.output {
            ui.packages
                .set_message(active_message(&tracker.active_packages));
        }
    }

    pub(crate) fn package_finished(&self, name: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock_tracker(inner);
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
        if let Output::Interactive(ui) = &inner.output {
            ui.packages.set_position(tracker.completed_packages);
            if tracker.completed_packages == tracker.package_total {
                ui.packages.set_message("complete");
            } else if tracker.active_packages.is_empty() {
                ui.packages.set_message("waiting");
            } else {
                ui.packages
                    .set_message(active_message(&tracker.active_packages));
            }
        } else {
            write_line(&plain_status(
                &tracker,
                "fetched",
                &format!(
                    "{}/{} {name}",
                    tracker.completed_packages, tracker.package_total
                ),
            ));
        }
    }

    pub(crate) fn candidates(&self, changes: &[Change], message: impl AsRef<str>) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock_tracker(inner);
        tracker.candidates = CandidateTracker::start(changes);
        let detail = if changes.is_empty() {
            "complete"
        } else {
            message.as_ref()
        };
        let status = tracker.candidates.status(detail);
        match &inner.output {
            Output::Interactive(ui) => {
                ui.candidates.set_style(candidate_style("green", ui.colors));
                ui.candidates.reset_elapsed();
                ui.candidates.set_message(status);
            }
            Output::Plain => write_line(&plain_status(&tracker, "candidates", &status)),
        }
    }

    fn resolver_operation(&self, change: &Change) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock_tracker(inner);
        tracker.candidates.begin_resolver_operation();
        let detail = format!("{} {} → {}", change.package.name, change.from, change.to);
        let message = tracker.candidates.status(&detail);
        match &inner.output {
            Output::Interactive(ui) => ui.candidates.set_message(message),
            Output::Plain => write_line(&plain_status(&tracker, "candidate", &message)),
        }
    }

    pub(crate) fn policy_pass(&self, changes: &[Change]) {
        let Some(first) = changes.first() else {
            return;
        };
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock_tracker(inner);
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
        match &inner.output {
            Output::Interactive(ui) => ui.candidates.set_message(message),
            Output::Plain => write_line(&plain_status(&tracker, "pass", &message)),
        }
    }

    pub(crate) fn candidates_decided(&self, changes: &[Change]) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock_tracker(inner);
        tracker.candidates.decide(changes);
        let detail = if tracker.candidates.is_complete() {
            "complete"
        } else {
            ""
        };
        let status = tracker.candidates.status(detail);
        match &inner.output {
            Output::Interactive(ui) => ui.candidates.set_message(status),
            Output::Plain => write_line(&plain_status(&tracker, "decided", &status)),
        }
    }

    pub(crate) fn finish_run(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock_tracker(inner);
        clear_interactive(inner, &mut tracker);
    }

    fn finish_project(&self, tool: &'static str) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut tracker = lock_tracker(inner);
        let Some(remaining) = tracker.remaining_projects.get_mut(tool) else {
            return;
        };
        if *remaining == 0 {
            return;
        }
        *remaining -= 1;
        if *remaining != 0 {
            return;
        }
        tracker.completed_tools = tracker.completed_tools.saturating_add(1);
        match &inner.output {
            Output::Interactive(ui) => {
                ui.tools.set_position(tracker.completed_tools);
                ui.tools.set_message(format!("{tool} complete"));
                ui.phase.set_message("complete");
            }
            Output::Plain => write_line(&format!(
                "progress     {:>3}/{:<3} tools complete ({tool})",
                tracker.completed_tools, tracker.total_tools
            )),
        }
        if tracker.completed_tools == tracker.total_tools {
            clear_interactive(inner, &mut tracker);
        }
    }
}

impl cooldown_core::ApplyObserver for Progress {
    fn candidate_started(&self, change: &Change) {
        self.resolver_operation(change);
    }
}

fn lock_tracker(inner: &ProgressInner) -> MutexGuard<'_, Tracker> {
    match inner.tracker.lock() {
        Ok(tracker) => tracker,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn clear_interactive(inner: &ProgressInner, tracker: &mut Tracker) {
    if tracker.cleared {
        return;
    }
    tracker.cleared = true;
    if let Output::Interactive(ui) = &inner.output {
        for bar in [&ui.tools, &ui.tool, &ui.phase, &ui.packages, &ui.candidates] {
            bar.finish_and_clear();
        }
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

fn plain_status(tracker: &Tracker, kind: &str, message: &str) -> String {
    if tracker.current_tool.is_empty() {
        return format!("progress     {kind:<10} {message}");
    }
    format!(
        "{:>12}  {:<20}  {kind:<10} {message}",
        tracker.current_tool,
        display_project(&tracker.current_project)
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
    use super::Progress;
    use cooldown_core::{Change, MemberRef, PackageId, ToolId, UpdateKind, Version};

    const CARGO: ToolId = ToolId("cargo");
    const GO: ToolId = ToolId("go");

    #[test]
    fn tools_complete_only_after_their_last_project() {
        let progress = Progress::plain();
        progress.start_run(&[
            (CARGO, "a".to_string()),
            (CARGO, "b".to_string()),
            (GO, ".".to_string()),
        ]);

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
        progress.packages(2, "fetching");

        progress.package_started("shared");
        progress.package_started("shared");
        progress.package_finished("shared");
        assert_eq!(active_packages(&progress), 1);

        progress.package_finished("shared");
        assert_eq!(active_packages(&progress), 0);
        assert_eq!(completed_packages(&progress), 2);
    }

    #[test]
    fn a_gap_between_package_fetches_is_not_reported_as_completion() {
        let progress = Progress::plain();
        progress.packages(2, "fetching");

        progress.package_started("first");
        progress.package_finished("first");

        assert_eq!(completed_packages(&progress), 1);
        assert_eq!(package_total(&progress), 2);
    }

    #[test]
    fn direct_candidates_for_distinct_members_are_counted_separately() {
        let progress = Progress::plain();
        let first = member_change("first");
        let second = member_change("second");
        progress.candidates(&[first.clone(), second.clone()], "checking");

        progress.candidates_decided(&[first, second]);

        assert_eq!(decided_candidates(&progress), 2);
    }

    #[test]
    fn candidates_outside_the_current_operation_do_not_change_its_count() {
        let progress = Progress::plain();
        let expected = member_change("expected");
        let unrelated = member_change("unrelated");
        progress.candidates(std::slice::from_ref(&expected), "checking");

        progress.candidates_decided(&[unrelated]);

        assert_eq!(decided_candidates(&progress), 0);
    }

    #[test]
    fn resolver_operations_advance_while_candidate_decisions_remain_pending() {
        let progress = Progress::plain();
        let first = member_change("first");
        let second = member_change("second");
        progress.candidates(&[first.clone(), second.clone()], "checking");
        progress.policy_pass(&[first.clone(), second]);

        cooldown_core::ApplyObserver::candidate_started(&progress, &first);
        cooldown_core::ApplyObserver::candidate_started(&progress, &first);

        assert_eq!(decided_candidates(&progress), 0);
        assert_eq!(
            candidate_summary(&progress, "shared 1.0.0 → 2.0.0"),
            "2 decisions pending · policy pass 1 · resolver op 2 · shared 1.0.0 → 2.0.0"
        );

        progress.candidates_decided(std::slice::from_ref(&first));
        assert_eq!(
            candidate_summary(&progress, ""),
            "1/2 decided · policy pass 1 · resolver op 2"
        );
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

    fn completed_tools(progress: &Progress) -> u64 {
        let inner = progress.inner.as_ref().expect("plain progress is enabled");
        super::lock_tracker(inner).completed_tools
    }

    fn active_packages(progress: &Progress) -> usize {
        let inner = progress.inner.as_ref().expect("plain progress is enabled");
        super::lock_tracker(inner).active_packages.values().sum()
    }

    fn completed_packages(progress: &Progress) -> u64 {
        let inner = progress.inner.as_ref().expect("plain progress is enabled");
        super::lock_tracker(inner).completed_packages
    }

    fn package_total(progress: &Progress) -> u64 {
        let inner = progress.inner.as_ref().expect("plain progress is enabled");
        super::lock_tracker(inner).package_total
    }

    fn decided_candidates(progress: &Progress) -> usize {
        let inner = progress.inner.as_ref().expect("plain progress is enabled");
        super::lock_tracker(inner).candidates.decided.len()
    }

    fn candidate_summary(progress: &Progress, detail: &str) -> String {
        let inner = progress.inner.as_ref().expect("plain progress is enabled");
        super::lock_tracker(inner).candidates.status(detail)
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
