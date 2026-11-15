//! The gating policy fields a tool's native config does not carry.
//!
//! `sync` writes one default window, and for some tools a table of literal package windows, so
//! every other field that gates a release reaches only cooldown's own gate.
//! [`omitted_native_fields`] classifies each field by where it is declared, never by comparing its
//! value with the written one: a field the native config does not encode is reported even when it
//! is looser than the written default.

use super::super::model::{
    Origin, PolicyLayer, ResolveKind, ResolveQuery, ResolvedWindow, Rule, Selector, WindowSpec,
};
use super::super::resolve::resolve;
use super::{PolicyContext, native_default_resolution};
use crate::model::ToolId;
use camino::Utf8Path;
use jiff::{SignedDuration, Timestamp};

/// What a tool's native config encodes of the policy `sync` writes into it.
#[derive(Debug, Clone, Copy)]
pub struct NativeEncoding<'a> {
    /// The layers the written default window was resolved from, low to high authority.
    ///
    /// A project-scoped tool resolves it from the project's own layers, a repo-scoped tool from
    /// the repo-root cascade.
    pub default_layers: &'a [PolicyLayer],
    /// The project path the written default window was resolved for, which
    /// [`Selector::Project`] rules match: the project's own path, or `.` for a repo-level file.
    pub default_project: &'a Utf8Path,
    /// Whether the native config carries the window of each package a literal package selector
    /// names, as uv's `exclude-newer-package` does.
    pub package_windows: bool,
}

/// One of the per-kind window fields of a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowField {
    /// The bare `min-age`, `freeze` or `[min-age] default`.
    Default,
    /// `[min-age] major`.
    Major,
    /// `[min-age] minor`.
    Minor,
    /// `[min-age] patch`.
    Patch,
}

impl WindowField {
    /// The field's name in an `explain` trace step: `default`, `major`, `minor` or `patch`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            WindowField::Default => "default",
            WindowField::Major => "major",
            WindowField::Minor => "minor",
            WindowField::Patch => "patch",
        }
    }
}

/// A rule field that can gate a release, with the value the rule declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatingField {
    /// A window field set to anything but [`WindowSpec::Latest`].
    Window {
        /// Which window field.
        kind: WindowField,
        /// The declared window.
        spec: WindowSpec,
    },
    /// A `floor`.
    Floor(SignedDuration),
}

impl GatingField {
    /// The field's name in an `explain` trace step: a [`WindowField::name`] or `floor`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            GatingField::Window { kind, .. } => kind.name(),
            GatingField::Floor(_) => "floor",
        }
    }

    /// The field as `cooldown.toml` spells it, such as `major = "30d"`.
    fn declaration(&self) -> String {
        match self {
            GatingField::Window {
                kind: WindowField::Default,
                spec: WindowSpec::Freeze(instant),
            } => format!("freeze = \"{instant}\""),
            GatingField::Window {
                kind: WindowField::Default,
                spec,
            } => format!("min-age = \"{}\"", spec_text(spec)),
            GatingField::Window { kind, spec } => {
                format!("{} = \"{}\"", kind.name(), spec_text(spec))
            }
            GatingField::Floor(floor) => format!("floor = \"{}\"", duration_text(*floor)),
        }
    }
}

/// Why a field the native config would otherwise encode is reported anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmissionReason {
    /// The field is a freeze that a binding floor replaces in the written default.
    ///
    /// The native config holds the rolling floor, which matches cooldown only while the floor's
    /// cutoff lands before the freeze.
    FreezeUnderFloor {
        /// The floor written in place of the freeze.
        floor: SignedDuration,
        /// The instant the written floor stops matching cooldown: the freeze plus the floor.
        until: Timestamp,
    },
}

/// A gating policy field a tool's native config does not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmittedField {
    /// The layer that declares the field.
    pub origin: Origin,
    /// The selector of the rule that declares the field.
    pub selector: Selector,
    /// The field and its declared value.
    pub field: GatingField,
    /// Why a field the native config would otherwise encode is reported, or `None` for a field
    /// the native config has no place for.
    pub reason: Option<OmissionReason>,
}

impl OmittedField {
    /// The `<origin>` or `<origin>:<selector>` token that names where the field is declared, as
    /// [`ResolvedWindow::source`] spells it.
    #[must_use]
    pub fn source(&self) -> String {
        match self.selector.token() {
            Some(selector) => format!("{}:{selector}", self.origin.token()),
            None => self.origin.token(),
        }
    }

    /// The notice `sync` reports for this field when it syncs `target`'s native config.
    #[must_use]
    pub fn notice(&self, target: ToolId) -> String {
        let declaration = self.field.declaration();
        let source = self.source();
        match self.reason {
            None => format!(
                "`{declaration}` from `{source}` is not represented in {target}'s native config, \
                 so a bare {target} command does not apply it"
            ),
            Some(OmissionReason::FreezeUnderFloor { floor, until }) => format!(
                "`{declaration}` from `{source}` is written to {target}'s native config as the \
                 {floor} floor that binds today, so from {until} a bare {target} command admits \
                 releases published after the freeze",
                floor = duration_text(floor),
            ),
        }
    }
}

/// Names every gating policy field in `contexts` that `target`'s native config does not carry.
///
/// Each context is a stack whose packages read the native config: the project for a
/// project-scoped tool, every project that consumes the repo-level file for a repo-scoped one.
/// A gating field is a window field set to anything but [`WindowSpec::Latest`], or a `floor`.
/// `allow` and `latest` are left to the native exemption lists, and `max-major` is no window.
///
/// A field is encoded, and so not reported, only when it is a window `default` or a `floor`
/// declared in one of [`NativeEncoding::default_layers`], matched by origin, on a selector the
/// written default folds in:
///
/// - [`Selector::Default`] or [`Selector::Tool`] for `target`;
/// - a [`Selector::Project`] matching [`NativeEncoding::default_project`];
/// - a [`Selector::Package`] whose pattern is exactly `*`;
/// - a literal [`Selector::Package`], when [`NativeEncoding::package_windows`] is set.
///
/// Every other gating field is reported: every per-kind window, any glob package rule but `*`
/// (even one matching the empty name), and every field of a layer only a consumer has, such as a
/// nested `cooldown.toml`.
/// A rule that cannot apply in a context, qualified for another tool or for another project, is
/// skipped there.
/// Registry rules are never reported, since adapters tag every package with a fixed registry.
/// Nor is a [`Origin::Native`] layer, the tool's own config read back: the tool applies it by
/// itself, whether it is the file `sync` writes or a consumer's nearer config that shadows it.
///
/// An encoded freeze is reported anyway when a binding floor replaces it in the written value,
/// with [`OmissionReason::FreezeUnderFloor`]: the written default, and for a tool with package
/// windows each literal package window, is checked.
///
/// The result holds one entry per origin, selector and field name, in the order the contexts,
/// layers and rules declare them, with the freezes last.
///
/// # Examples
///
/// ```
/// use camino::Utf8Path;
/// use cooldown_core::config::{builtin_default_layer, parse_config};
/// use cooldown_core::{NativeEncoding, Origin, PolicyContext, ToolId, omitted_native_fields};
/// use indoc::indoc;
///
/// let config = indoc! {r#"
///     [min-age]
///     default = "7d"
///     major = "30d"
/// "#};
/// let repo = parse_config(config, Origin::Repo("cooldown.toml".into()))?;
/// let layers = [builtin_default_layer(), repo];
/// let project = Utf8Path::new(".");
/// let encoding = NativeEncoding {
///     default_layers: &layers,
///     default_project: project,
///     package_windows: false,
/// };
/// let context = PolicyContext {
///     layers: &layers,
///     project,
/// };
///
/// let pnpm = ToolId("pnpm");
/// let now = "2026-01-15T00:00:00Z".parse()?;
/// let omitted = omitted_native_fields(&[context], pnpm, &encoding, now);
/// let notices: Vec<_> = omitted.iter().map(|field| field.notice(pnpm)).collect();
/// assert_eq!(
///     notices,
///     ["`major = \"30d\"` from `repo:cooldown.toml` is not represented in pnpm's native config, \
///       so a bare pnpm command does not apply it"]
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use]
pub fn omitted_native_fields(
    contexts: &[PolicyContext<'_>],
    target: ToolId,
    encoding: &NativeEncoding<'_>,
    now: Timestamp,
) -> Vec<OmittedField> {
    let mut omitted = Vec::new();
    for context in contexts {
        // A native layer is a tool's own config read back, which the tool applies by itself:
        // the file `sync` writes for a project-scoped tool, or a consumer's nearer config that
        // shadows the repo-level file.
        let policy_layers = context
            .layers
            .iter()
            .filter(|layer| layer.origin != Origin::Native);
        for layer in policy_layers {
            let default_layer = encoding
                .default_layers
                .iter()
                .any(|default| default.origin == layer.origin);
            for rule in &layer.rules {
                if !applies(&rule.selector, target, context.project) {
                    continue;
                }
                for field in gating_fields(rule) {
                    let carries_default = matches!(
                        field,
                        GatingField::Window {
                            kind: WindowField::Default,
                            ..
                        } | GatingField::Floor(_)
                    );
                    let encoded = carries_default
                        && default_layer
                        && folded_in(&rule.selector, target, encoding);
                    if !encoded {
                        record(
                            &mut omitted,
                            OmittedField {
                                origin: layer.origin.clone(),
                                selector: rule.selector.clone(),
                                field,
                                reason: None,
                            },
                        );
                    }
                }
            }
        }
    }
    for drift in freeze_drifts(target, encoding, now) {
        record(&mut omitted, drift);
    }
    omitted
}

/// Whether a rule with `selector` can gate a package of `target` in the project at `project`.
fn applies(selector: &Selector, target: ToolId, project: &Utf8Path) -> bool {
    match selector {
        Selector::Default => true,
        Selector::Tool(tool) => *tool == target,
        Selector::Registry(_) => false,
        Selector::Project(glob) => glob.is_match(project.as_str()),
        Selector::Package { tool, .. } => tool.is_none_or(|tool| tool == target),
    }
}

/// Whether the written default, or the native package table, takes a `default` or `floor` of a
/// rule with `selector` from one of the default's layers.
fn folded_in(selector: &Selector, target: ToolId, encoding: &NativeEncoding<'_>) -> bool {
    match selector {
        Selector::Default => true,
        Selector::Tool(tool) => *tool == target,
        Selector::Registry(_) => false,
        Selector::Project(glob) => glob.is_match(encoding.default_project.as_str()),
        Selector::Package { glob, .. } => {
            glob.raw() == "*" || (encoding.package_windows && glob.is_literal())
        }
    }
}

/// The gating fields `rule` sets, window fields first.
fn gating_fields(rule: &Rule) -> impl Iterator<Item = GatingField> + '_ {
    [
        (WindowField::Default, &rule.window.default),
        (WindowField::Major, &rule.window.major),
        (WindowField::Minor, &rule.window.minor),
        (WindowField::Patch, &rule.window.patch),
    ]
    .into_iter()
    .filter_map(|(kind, spec)| match spec {
        Some(spec) if *spec != WindowSpec::Latest => Some(GatingField::Window {
            kind,
            spec: spec.clone(),
        }),
        _ => None,
    })
    .chain(rule.floor.map(GatingField::Floor))
}

/// Adds `field` unless an entry for its origin, selector and field name exists, in which case
/// only its reason is kept on that entry.
fn record(omitted: &mut Vec<OmittedField>, field: OmittedField) {
    let existing = omitted.iter_mut().find(|existing| {
        existing.origin == field.origin
            && existing.selector == field.selector
            && existing.field.name() == field.field.name()
    });
    match existing {
        Some(existing) => {
            if field.reason.is_some() {
                existing.reason = field.reason;
            }
        }
        None => omitted.push(field),
    }
}

/// The freezes a binding floor replaces in the written default or a written package window.
fn freeze_drifts(
    target: ToolId,
    encoding: &NativeEncoding<'_>,
    now: Timestamp,
) -> Vec<OmittedField> {
    let mut windows = vec![native_default_resolution(
        encoding.default_layers,
        target,
        encoding.default_project,
        now,
    )];
    if encoding.package_windows {
        let mut names: Vec<&str> = Vec::new();
        for rule in encoding
            .default_layers
            .iter()
            .flat_map(|layer| &layer.rules)
        {
            if let Selector::Package { glob, tool } = &rule.selector
                && tool.is_none_or(|tool| tool == target)
                && glob.is_literal()
                && !names.contains(&glob.raw())
            {
                names.push(glob.raw());
            }
        }
        windows.extend(names.into_iter().map(|name| {
            let query = ResolveQuery {
                tool: target,
                package: name,
                registry: None,
                project: encoding.default_project,
                kind: ResolveKind::CurrentPin,
            };
            resolve(encoding.default_layers, &query, now).window
        }));
    }
    windows
        .into_iter()
        .filter_map(|window| freeze_drift(window, now))
        .collect()
}

/// The freeze of `window` when its floor binds at `now`, so the written value is that rolling
/// floor.
///
/// A floor that does not bind now never will, since the freeze cutoff stays while the floor's
/// cutoff moves later.
fn freeze_drift(window: ResolvedWindow, now: Timestamp) -> Option<OmittedField> {
    let WindowSpec::Freeze(freeze) = window.spec else {
        return None;
    };
    let floor = window.floor?;
    if now - floor >= freeze {
        return None;
    }
    // A sum past the last representable instant means the written floor never stops matching.
    let until = freeze.checked_add(floor).ok()?;
    Some(OmittedField {
        origin: window.decided_by,
        selector: window.decided_selector,
        field: GatingField::Window {
            kind: WindowField::Default,
            spec: WindowSpec::Freeze(freeze),
        },
        reason: Some(OmissionReason::FreezeUnderFloor { floor, until }),
    })
}

/// A window as `cooldown.toml` spells it: an age as a duration, a freeze as its instant.
fn spec_text(spec: &WindowSpec) -> String {
    match spec {
        WindowSpec::MinAge(duration) => duration_text(*duration),
        WindowSpec::Freeze(instant) => instant.to_string(),
        WindowSpec::Latest => "0d".to_string(),
    }
}

/// A duration in the largest unit that divides it: `30d`, `36h`, `90m` or `45s`.
fn duration_text(duration: SignedDuration) -> String {
    const SECS_PER_DAY: i64 = 86_400;
    const SECS_PER_HOUR: i64 = 3_600;
    const SECS_PER_MINUTE: i64 = 60;
    let seconds = duration.as_secs();
    let (count, unit) = if seconds == 0 {
        (0, "d")
    } else if seconds % SECS_PER_DAY == 0 {
        (seconds / SECS_PER_DAY, "d")
    } else if seconds % SECS_PER_HOUR == 0 {
        (seconds / SECS_PER_HOUR, "h")
    } else if seconds % SECS_PER_MINUTE == 0 {
        (seconds / SECS_PER_MINUTE, "m")
    } else {
        (seconds, "s")
    };
    format!("{count}{unit}")
}

#[cfg(test)]
mod tests;
