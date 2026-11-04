//! Yarn Berry's native age gate, which `sync` writes into the project's rc file.
//!
//! Yarn 4.10.0 and later refuse to resolve a registry release younger than `npmMinimalAgeGate`
//! minutes unless an `npmPreapprovedPackages` entry approves it ([npmConfigUtils.ts][config]).
//! `sync` writes both settings into the project's `.yarnrc.yml` (or the file `YARN_RC_FILENAME`
//! names), and the gate of each scope the file configures, from cooldown's resolved policy:
//!
//! - the default window as a whole number of minutes, rounded up so the gate is never shorter;
//! - the `latest`/`allow` selectors Yarn reads as cooldown does ([`selectors`]), and the young
//!   locked versions cooldown admits as exact entries;
//! - the list in Yarn's `onConflict: reset` form, since Yarn concatenates the lists of every rc
//!   file it reads, so an inherited entry would otherwise exempt what cooldown gates.
//!
//! The gate applies when Yarn picks a new candidate, while a release the lock already resolves is
//! reused without one, so an exact entry matters only once the release is resolved afresh.
//! `sync` runs no Yarn command but `yarn --version`, and replaces the rc file atomically with an
//! edit of its own text that keeps every other line ([`rc_edit`]), after checking that the edit
//! sets exactly the intended settings.
//!
//! [config]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/plugin-npm/sources/npmConfigUtils.ts

mod rc_edit;
mod scopes;
mod selectors;
mod support;

use crate::manifest::workspaces::YarnEnvironment;
use crate::nodecmd::NodeCmd;
use camino::Utf8Path;
use cooldown_core::{
    CoreError, Diagnostic, DiagnosticKind, Project, ResolvedPolicy, Result, SyncReport, WindowSpec,
};
use rc_edit::{EditError, RcText};
use selectors::{Entry, SelectorReading};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use support::GateSupport;
pub(super) use support::declares_ungated_yarn;

/// The global age gate, in minutes.
const GATE_KEY: &str = "npmMinimalAgeGate";

/// The entries Yarn's age gate approves.
const PREAPPROVED_KEY: &str = "npmPreapprovedPackages";

/// The per-scope settings, each scope with a gate of its own from Yarn 4.17.0.
const SCOPES_KEY: &str = "npmScopes";

/// The rc file name Yarn reads when `YARN_RC_FILENAME` names none.
const DEFAULT_RC_FILENAME: &str = ".yarnrc.yml";

/// The process environment a Berry sync reads, captured once where the adapter syncs a project.
#[derive(Debug, Clone, Default)]
pub(super) struct SyncEnvironment {
    /// The environment Yarn's configuration reads.
    pub(super) yarn: YarnEnvironment,
    /// `YARN_NPM_MINIMAL_AGE_GATE`, which overrides the gate of every rc file.
    pub(super) minimal_age_gate: Option<String>,
    /// `YARN_NPM_PREAPPROVED_PACKAGES`, which overrides the list of every rc file.
    pub(super) preapproved_packages: Option<String>,
}

impl SyncEnvironment {
    /// The environment of this process.
    pub(super) fn from_process() -> Self {
        let mut environment = SyncEnvironment {
            yarn: YarnEnvironment::from_process(),
            ..SyncEnvironment::default()
        };
        for (key, value) in std::env::vars_os() {
            let value = value.to_string_lossy().into_owned();
            match key.to_string_lossy().to_ascii_lowercase().as_str() {
                "yarn_npm_minimal_age_gate" => environment.minimal_age_gate = Some(value),
                "yarn_npm_preapproved_packages" => environment.preapproved_packages = Some(value),
                _ => {}
            }
        }
        environment
    }
}

/// Writes `policy` into the age gate of the Berry project `project`, as [`ToolWrite::write_native`]
/// does, with `driver` running the `yarn --version` that tells which settings Yarn reads.
///
/// Returns [`SyncReport::Unsupported`], writing nothing, when the project's Yarn predates the
/// gate.
///
/// # Errors
///
/// Returns [`CoreError::System`] or [`CoreError::ToolSpawn`] when the Yarn reading the file cannot
/// be established ([`support::gate_support`]), and [`CoreError::Parse`] when the rc file cannot be
/// read as Yarn reads it or edited without changing another setting; nothing is written then.
///
/// [`ToolWrite::write_native`]: cooldown_core::ToolWrite::write_native
pub(super) async fn write(
    driver: &NodeCmd,
    project: &Project,
    policy: &ResolvedPolicy,
    environment: &SyncEnvironment,
    dry_run: bool,
) -> Result<SyncReport> {
    let Some(support) = support::gate_support(driver, &project.root).await? else {
        return Ok(SyncReport::Unsupported);
    };
    write_supported(&project.root, policy, environment, support, dry_run)
}

/// [`write`] once the Yarn reading the rc file is known to support the gate as `support` says.
fn write_supported(
    root: &Utf8Path,
    policy: &ResolvedPolicy,
    environment: &SyncEnvironment,
    support: GateSupport,
    dry_run: bool,
) -> Result<SyncReport> {
    let path = root.join(
        environment
            .yarn
            .rc_filename
            .as_deref()
            .unwrap_or(DEFAULT_RC_FILENAME),
    );
    let shown_path = shown(root, &path);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(CoreError::Filesystem(format!("{path}: {err}"))),
    };
    let settings = parse_settings(&content)
        .map_err(|reason| CoreError::Parse(format!("{shown_path}: {reason}")))?;
    // Yarn reads a wrapped document's `value` in its place, so a setting written beside the
    // wrapper would be ignored.
    if settings.contains_key("onConflict") {
        return Err(CoreError::Parse(format!(
            "{shown_path} wraps its settings in `onConflict`, which sync cannot edit in place, so \
             it wrote nothing"
        )));
    }

    let selectors = Selectors::classify(&policy.exempt_packages);
    let mut warnings = selectors.warnings(&policy.withheld_exemptions);
    let mut edit = Edit {
        text: RcText::new(&content),
        expected: settings.clone(),
        shown_path: &shown_path,
    };
    if let Some(minutes) = policy.default_window.as_ref().and_then(written_minutes) {
        edit.set_gate(minutes)?;
        edit.set_preapproved(&writable_entries(&selectors, policy, &settings))?;
        if support.scope_gates_apply {
            warnings.extend(edit.set_scope_gates(
                root,
                &path,
                &settings,
                environment,
                support,
                minutes,
            ));
        }
        warnings.extend(environment_warnings(environment, true));
    } else {
        if let Some(WindowSpec::Freeze(cutoff)) = &policy.default_window {
            warnings.push(Diagnostic::new(
                DiagnosticKind::Config,
                format!(
                    "the default window is a freeze at {cutoff}, which Yarn's rolling \
                     npmMinimalAgeGate cannot express, so sync leaves npmMinimalAgeGate as it is"
                ),
            ));
        }
        if let Some(retained) = pruned_entries(&selectors, policy, &settings) {
            edit.set_preapproved(&retained)?;
            warnings.extend(environment_warnings(environment, false));
        }
    }

    let Edit { text, expected, .. } = edit;
    let edited = text.render();
    if edited == content {
        return Ok(SyncReport::Unchanged { path, warnings });
    }
    verified(&shown_path, &edited, &expected)?;
    if !dry_run {
        cooldown_core::fs::atomic_write(path.as_std_path(), edited.as_bytes())?;
    }
    Ok(SyncReport::Written { path, warnings })
}

/// cooldown's exempt selectors split by how Yarn reads them ([`selectors::reading`]), each list
/// sorted and deduplicated.
struct Selectors {
    /// The selectors written as entries, the [`SelectorReading::UnscopedOnly`] ones included.
    written: Vec<String>,
    /// The written selectors Yarn reads for unscoped names only.
    narrowed: Vec<String>,
    /// The selectors left out, since Yarn would read them otherwise.
    omitted: Vec<String>,
}

impl Selectors {
    fn classify(exempt: &[String]) -> Self {
        let mut selectors = Selectors {
            written: Vec::new(),
            narrowed: Vec::new(),
            omitted: Vec::new(),
        };
        for selector in exempt.iter().collect::<BTreeSet<_>>() {
            match selectors::reading(selector) {
                SelectorReading::Exact => selectors.written.push(selector.clone()),
                SelectorReading::UnscopedOnly => {
                    selectors.written.push(selector.clone());
                    selectors.narrowed.push(selector.clone());
                }
                SelectorReading::Unreadable => selectors.omitted.push(selector.clone()),
            }
        }
        selectors
    }

    /// One [`DiagnosticKind::StricterNative`] warning per selector Yarn's gate still applies to in
    /// part or in whole: each `withheld` one, each omitted one, and each narrowed one.
    fn warnings(&self, withheld: &[String]) -> Vec<Diagnostic> {
        let withheld = withheld.iter().map(|selector| {
            format!(
                "cooldown cannot prove that every release `{selector}` matches is exempt, so it is \
                 not exempted natively"
            )
        });
        let omitted = self.omitted.iter().map(|selector| {
            format!(
                "selector `{selector}` uses syntax Yarn's npmPreapprovedPackages reads differently, \
                 so sync does not write it and Yarn's npmMinimalAgeGate applies to its packages \
                 unless another written entry approves them"
            )
        });
        let narrowed = self.narrowed.iter().map(|selector| {
            format!(
                "selector `{selector}` is written to npmPreapprovedPackages, but Yarn's `*` does not \
                 cross a scope's `/`, so Yarn's npmMinimalAgeGate still applies to the scoped \
                 packages it matches"
            )
        });
        withheld
            .chain(omitted)
            .chain(narrowed)
            .map(|message| Diagnostic::new(DiagnosticKind::StricterNative, message))
            .collect()
    }
}

/// The `npmPreapprovedPackages` entries a sync writing the gate writes: the written selectors,
/// then one `name@version` per admitted young locked version no written selector already
/// approves.
///
/// Without an evaluation the existing exact entries are kept verbatim instead, so the current
/// lock still resolves afresh.
fn writable_entries(
    selectors: &Selectors,
    policy: &ResolvedPolicy,
    settings: &Map<String, Value>,
) -> Vec<String> {
    let mut entries = selectors.written.clone();
    if let Some(admitted) = &policy.admitted_versions {
        let exact: BTreeSet<String> = admitted
            .iter()
            .filter(|admission| {
                !selectors
                    .written
                    .iter()
                    .any(|selector| selectors::yarn_matches(selector, &admission.name))
            })
            .map(|admission| format!("{}@{}", admission.name, admission.version.as_str()))
            .collect();
        entries.extend(exact);
    } else {
        let existing = settings
            .get(PREAPPROVED_KEY)
            .map(existing_entries)
            .unwrap_or_default();
        for entry in existing {
            let exact = Entry::parse(&entry)
                .and_then(|parsed| parsed.exact_versions())
                .is_some();
            if exact && !entries.contains(&entry) {
                entries.push(entry);
            }
        }
    }
    entries
}

/// The `npmPreapprovedPackages` entries a sync that leaves the gate alone writes, or `None` when
/// it leaves the list alone too.
///
/// The gate left in place may still be active, so a selector the policy no longer grants loses
/// its entry, while every ranged entry stays, since no admission was evaluated against that gate.
/// An entry spelling `${…}` may expand to any selector, so it goes.
/// An existing list is always rewritten in reset form, so no inherited entry exempts more, and an
/// absent one is added empty only when a selector was withheld or left out.
fn pruned_entries(
    selectors: &Selectors,
    policy: &ResolvedPolicy,
    settings: &Map<String, Value>,
) -> Option<Vec<String>> {
    let Some(existing) = settings.get(PREAPPROVED_KEY) else {
        let gated = !policy.withheld_exemptions.is_empty() || !selectors.omitted.is_empty();
        return gated.then(Vec::new);
    };
    Some(
        existing_entries(existing)
            .into_iter()
            .filter(|entry| {
                selectors.written.contains(entry)
                    || (!entry.contains('$')
                        && Entry::parse(entry).is_some_and(|entry| entry.range.is_some()))
            })
            .collect(),
    )
}

/// The entries an `npmPreapprovedPackages` value lists, read as Yarn reads them: a list, a list
/// inside an `onConflict` wrapper, or a string Yarn splits at each `,`.
fn existing_entries(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(entry) => Some(entry.clone()),
                Value::Number(number) => Some(number.to_string()),
                Value::Bool(flag) => Some(flag.to_string()),
                Value::Null | Value::Array(_) | Value::Object(_) => None,
            })
            .collect(),
        Value::Object(wrapper) => wrapper
            .get("value")
            .map(existing_entries)
            .unwrap_or_default(),
        Value::String(entries) => entries.split(',').map(str::to_string).collect(),
        Value::Number(number) => vec![number.to_string()],
        Value::Bool(_) | Value::Null => Vec::new(),
    }
}

/// The rc file's text as edited so far, and the settings the edit must leave it holding.
struct Edit<'a> {
    text: RcText,
    expected: Map<String, Value>,
    /// The rc file as a message names it.
    shown_path: &'a str,
}

impl Edit<'_> {
    fn set_gate(&mut self, minutes: u64) -> Result<()> {
        self.set_top_level(GATE_KEY, vec![format!("{GATE_KEY}: {minutes}")])?;
        self.expected
            .insert(GATE_KEY.to_string(), Value::from(minutes));
        Ok(())
    }

    /// Writes `entries` as the whole effective `npmPreapprovedPackages` list.
    fn set_preapproved(&mut self, entries: &[String]) -> Result<()> {
        let mut block = vec![
            format!("{PREAPPROVED_KEY}:"),
            "  onConflict: reset".to_string(),
        ];
        if entries.is_empty() {
            block.push("  value: []".to_string());
        } else {
            block.push("  value:".to_string());
            for entry in entries {
                let quoted = serde_json::to_string(entry)
                    .map_err(|err| CoreError::Serialization(err.to_string()))?;
                block.push(format!("    - {quoted}"));
            }
        }
        self.set_top_level(PREAPPROVED_KEY, block)?;
        self.expected.insert(
            PREAPPROVED_KEY.to_string(),
            serde_json::json!({ "onConflict": "reset", "value": entries }),
        );
        Ok(())
    }

    fn set_top_level(&mut self, key: &str, block: Vec<String>) -> Result<()> {
        self.text
            .set_top_level(key, block)
            .map_err(|err| match err {
                EditError::Duplicate => CoreError::Parse(format!(
                    "{shown_path} sets {key} more than once, and Yarn reads only the last, so sync \
                 wrote nothing: keep one",
                    shown_path = self.shown_path
                )),
            })
    }

    /// Sets the gate of every scope the project's rc file configures in a shape it can edit to
    /// `minutes`, and reports every other configured scope whose gate differs.
    ///
    /// No scope entry is ever created: a scope with an entry stops inheriting the global
    /// `npmRegistryServer`, so a new entry would reroute the scope's packages.
    fn set_scope_gates(
        &mut self,
        root: &Utf8Path,
        project_rc: &Utf8Path,
        settings: &Map<String, Value>,
        environment: &SyncEnvironment,
        support: GateSupport,
        minutes: u64,
    ) -> Vec<Diagnostic> {
        let gates = scopes::scope_gates(root, project_rc, settings, &environment.yarn);
        let configured_here = settings
            .get(SCOPES_KEY)
            .and_then(Value::as_object)
            .map(|scopes| scopes.keys().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let mut warnings = Vec::new();
        for (scope, gate) in &gates.gates {
            let here = configured_here.contains(scope);
            if here && support.scope_gates_writable && self.text.set_scope_gate(scope, minutes) {
                // Only a mapping entry is edited, so anything else fails the verification.
                if let Some(Value::Object(scopes)) = self.expected.get_mut(SCOPES_KEY)
                    && let Some(Value::Object(entry)) = scopes.get_mut(scope)
                {
                    entry.insert(GATE_KEY.to_string(), Value::from(minutes));
                }
                continue;
            }
            let why = if !here {
                "sync never edits a scope entry outside the project's rc file"
            } else if support.scope_gates_writable {
                "sync edits only a block mapping of settings without `onConflict`, and an empty \
                 entry resets the scope's inherited settings, which a written gate would restore"
            } else {
                "a Yarn older than 4.17.0 also reads this file and refuses a scope's gate"
            };
            if let Some(warning) = scope_gate_warning(root, scope, gate, minutes, why) {
                warnings.push(warning);
            }
        }
        warnings.extend(gates.unreadable.into_iter().map(|reason| {
            Diagnostic::new(
                DiagnosticKind::Config,
                format!(
                    "cannot read every scope's npmMinimalAgeGate, since {reason}, so Yarn may gate \
                     some scope's packages by less than the npmMinimalAgeGate sync writes"
                ),
            )
        }));
        warnings
    }
}

/// The warning for a scope whose gate sync leaves differing from the written `minutes`, `why`
/// saying why sync left it, or `None` when the gates agree.
fn scope_gate_warning(
    root: &Utf8Path,
    scope: &str,
    gate: &scopes::ScopeGate,
    minutes: u64,
    why: &str,
) -> Option<Diagnostic> {
    let source = shown(root, &gate.source);
    let Some(gate_minutes) = gate.minutes else {
        return Some(Diagnostic::new(
            DiagnosticKind::Config,
            format!(
                "cooldown cannot read the npmMinimalAgeGate of `npmScopes.{scope}` in {source}, \
                 which Yarn applies to `@{scope}/*` packages instead of the {minutes} minutes sync \
                 writes"
            ),
        ));
    };
    #[expect(
        clippy::cast_precision_loss,
        reason = "a gate in minutes stays far below 2^53, where an f64 is exact"
    )]
    let written = minutes as f64;
    let (kind, effect) = if gate_minutes < written {
        (
            DiagnosticKind::Config,
            format!(
                "shorter than the {minutes} minutes sync writes, so it admits releases cooldown \
                 still gates; set that entry's npmMinimalAgeGate to {minutes}"
            ),
        )
    } else if gate_minutes > written {
        (
            DiagnosticKind::StricterNative,
            format!("longer than the {minutes} minutes sync writes"),
        )
    } else {
        return None;
    };
    let gated = if gate.explicit {
        format!("by the {gate_minutes} minutes `npmScopes.{scope}` sets in {source}")
    } else {
        format!(
            "by its one-day default, since `npmScopes.{scope}` in {source} sets no \
             npmMinimalAgeGate"
        )
    };
    Some(Diagnostic::new(
        kind,
        format!("Yarn gates `@{scope}/*` packages {gated}, {effect} ({why})"),
    ))
}

/// A [`DiagnosticKind::Config`] warning for each age-gate setting the environment overrides,
/// the gate's only when `gate_written`.
fn environment_warnings(environment: &SyncEnvironment, gate_written: bool) -> Vec<Diagnostic> {
    let gate = environment
        .minimal_age_gate
        .as_ref()
        .filter(|_| gate_written)
        .map(|_| ("YARN_NPM_MINIMAL_AGE_GATE", GATE_KEY));
    let preapproved = environment
        .preapproved_packages
        .as_ref()
        .map(|_| ("YARN_NPM_PREAPPROVED_PACKAGES", PREAPPROVED_KEY));
    gate.into_iter()
        .chain(preapproved)
        .map(|(variable, key)| {
            Diagnostic::new(
                DiagnosticKind::Config,
                format!(
                    "{variable} is set in this environment and overrides the {key} sync writes \
                     for every Yarn run in it"
                ),
            )
        })
        .collect()
}

/// The whole minutes Yarn's gate holds for `window`, rounded up so the gate is never shorter
/// than the window, or `None` when the window is no positive rolling age.
///
/// Every Yarn from 4.10.0 reads a bare number as minutes: 4.10 with `parseInt`, which would read
/// `"7d"` as 7, and later releases as a duration.
/// cooldown evaluates the admissions it writes against whole minutes rounded down, so for a
/// window that is no whole number of minutes, a release whose age falls in the last minute of
/// the gate is gated without an entry for under a minute after the sync.
fn written_minutes(window: &WindowSpec) -> Option<u64> {
    let WindowSpec::MinAge(duration) = window else {
        return None;
    };
    let seconds = duration.as_secs() + i64::from(duration.subsec_nanos() > 0);
    let seconds = u64::try_from(seconds).ok().filter(|seconds| *seconds > 0)?;
    Some(seconds.div_ceil(60))
}

/// The top-level settings of an rc file's text, as Yarn would accept them.
///
/// An empty file sets nothing, and anything but a YAML mapping is refused, since Yarn refuses
/// it too.
fn parse_settings(content: &str) -> std::result::Result<Map<String, Value>, String> {
    if content.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_saphyr::from_str::<Value>(content) {
        Ok(Value::Object(settings)) => Ok(settings),
        Ok(Value::Null) => Ok(Map::new()),
        Ok(_) => Err("is no YAML mapping".to_string()),
        Err(err) => Err(err.to_string()),
    }
}

/// Confirms that the `edited` text holds exactly the `expected` settings.
///
/// The line edits read the document's layout from its lines, so a construct they do not model,
/// such as an anchor or a flow collection spanning lines, could otherwise change another setting
/// or leave the intended one unset.
fn verified(shown_path: &str, edited: &str, expected: &Map<String, Value>) -> Result<()> {
    let edited = parse_settings(edited).ok();
    if edited.as_ref() == Some(expected) {
        return Ok(());
    }
    Err(CoreError::Parse(format!(
        "{shown_path} is laid out in a way sync cannot edit without changing other settings, so it \
         wrote nothing: set {GATE_KEY} and {PREAPPROVED_KEY} there by hand"
    )))
}

/// `path` as a message names it: project-relative and `/`-separated inside `root`, and as it is
/// outside.
fn shown(root: &Utf8Path, path: &Utf8Path) -> String {
    path.strip_prefix(root)
        .map_or_else(|_| path.to_string(), crate::manifest::display_rel)
}

#[cfg(test)]
mod tests;
