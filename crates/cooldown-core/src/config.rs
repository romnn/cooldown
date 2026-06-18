//! The TOML config schema and its conversion into [`PolicyLayer`]s.
//!
//! One schema is used everywhere (the global file and every `cooldown.toml`). `min-age` is either a
//! duration scalar or a per-kind table — never both in one selector. Within any single selector,
//! `latest`, `freeze`, and `min-age` are mutually exclusive (a config-validation error, exit 2),
//! the same rule the CLI enforces for `--latest`/`--freeze`/`--min-age`.

use crate::duration::{parse_duration, parse_freeze};
use crate::error::CoreError;
use crate::model::ecosystem_id;
use crate::policy::{ByKind, Origin, PatternGlob, PolicyLayer, Rule, Selector, WindowSpec};
use jiff::SignedDuration;
use std::collections::BTreeMap;

const SEVEN_DAYS: SignedDuration = SignedDuration::from_hours(24 * 7);

/// Returns the built-in default policy layer: a single [`Selector::Default`] rule of `min-age = 7d`.
///
/// This is the lowest-authority layer ([`Origin::Default`]) that every cascade starts from, so an
/// unconfigured project still enforces a one-week cooldown.
///
/// # Examples
///
/// ```
/// use cooldown_core::config::builtin_default_layer;
/// use cooldown_core::Origin;
///
/// let layer = builtin_default_layer();
/// assert_eq!(layer.origin, Origin::Default);
/// assert_eq!(layer.rules.len(), 1);
/// ```
#[must_use]
pub fn builtin_default_layer() -> PolicyLayer {
    let mut layer = PolicyLayer::new(Origin::Default);
    let mut rule = Rule::new(Selector::Default);
    rule.window = ByKind::scalar(WindowSpec::MinAge(SEVEN_DAYS));
    layer.rules.push(rule);
    layer
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum MinAgeToml {
    Scalar(String),
    Table(MinAgeTable),
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MinAgeTable {
    default: Option<String>,
    major: Option<String>,
    minor: Option<String>,
    patch: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectorToml {
    #[serde(rename = "min-age")]
    min_age: Option<MinAgeToml>,
    latest: Option<bool>,
    freeze: Option<String>,
    floor: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigToml {
    #[serde(rename = "min-age")]
    min_age: Option<MinAgeToml>,
    latest: Option<bool>,
    freeze: Option<String>,
    floor: Option<String>,
    allow: Option<Vec<String>>,
    #[serde(rename = "strict-native")]
    strict_native: Option<bool>,
    lang: Option<BTreeMap<String, SelectorToml>>,
    registry: Option<BTreeMap<String, SelectorToml>>,
    package: Option<BTreeMap<String, SelectorToml>>,
    project: Option<BTreeMap<String, SelectorToml>>,
}

/// Builds the [`ByKind`] window for one selector, enforcing the `latest`/`freeze`/`min-age`
/// exclusivity.
///
/// `ctx` is a human-readable label (e.g. `"top-level"` or `"[lang.rust]"`) interpolated into any
/// error message so the user can locate the offending selector.
///
/// # Errors
///
/// Returns [`CoreError::Config`] when more than one of `min-age`, `latest`, and `freeze` is set on
/// the same selector, or when a `freeze` timestamp or a `min-age` duration (scalar or any per-kind
/// table entry) fails to parse.
fn build_window(
    min_age: Option<&MinAgeToml>,
    latest: Option<bool>,
    freeze: Option<&str>,
    ctx: &str,
) -> Result<ByKind, CoreError> {
    let latest_set = latest == Some(true);
    let set = [min_age.is_some(), latest_set, freeze.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if set > 1 {
        return Err(CoreError::Config(format!(
            "{ctx}: `min-age`, `latest`, and `freeze` are mutually exclusive; set at most one"
        )));
    }
    if latest_set {
        return Ok(ByKind::scalar(WindowSpec::Latest));
    }
    if let Some(f) = freeze {
        return Ok(ByKind::scalar(WindowSpec::Freeze(parse_freeze(f)?)));
    }
    match min_age {
        None => Ok(ByKind::default()),
        Some(MinAgeToml::Scalar(s)) => Ok(ByKind::scalar(WindowSpec::MinAge(parse_duration(s)?))),
        Some(MinAgeToml::Table(t)) => {
            let conv = |o: &Option<String>| -> Result<Option<WindowSpec>, CoreError> {
                o.as_ref()
                    .map(|s| parse_duration(s).map(WindowSpec::MinAge))
                    .transpose()
            };
            Ok(ByKind {
                default: conv(&t.default)?,
                major: conv(&t.major)?,
                minor: conv(&t.minor)?,
                patch: conv(&t.patch)?,
            })
        }
    }
}

/// Builds a [`Rule`] for `selector` from its parsed [`SelectorToml`] block.
///
/// `ctx` labels the selector in any error message (see [`build_window`]).
///
/// # Errors
///
/// Returns [`CoreError::Config`] when [`build_window`] rejects the window settings or when the
/// `floor` duration fails to parse.
fn selector_rule(selector: Selector, s: &SelectorToml, ctx: &str) -> Result<Rule, CoreError> {
    let mut rule = Rule::new(selector);
    rule.window = build_window(s.min_age.as_ref(), s.latest, s.freeze.as_deref(), ctx)?;
    if let Some(f) = &s.floor {
        rule.floor = Some(parse_duration(f)?);
    }
    Ok(rule)
}

/// Parses a `cooldown.toml`'s `content` into a [`PolicyLayer`] tagged with `origin`.
///
/// The top-level `min-age`/`latest`/`freeze`/`floor` keys become a [`Selector::Default`] rule, each
/// `allow` glob becomes an exempt [`Selector::Package`] rule, and every `[lang.*]`, `[registry.*]`,
/// `[package.*]`, and `[project.*]` table becomes its own selector rule. `strict-native` is carried
/// onto the layer as-is.
///
/// # Errors
///
/// Returns [`CoreError::Config`] when `content` is not valid TOML or has unknown fields, when a
/// `[lang.*]` key is not a recognised ecosystem (`go`, `rust`, `python`, `node`), when a `min-age`
/// duration, `freeze` timestamp, or `floor` duration fails to parse, when a `package`/`project`
/// glob is invalid, or when a selector sets more than one of `min-age`, `latest`, and `freeze`.
///
/// # Examples
///
/// ```
/// use cooldown_core::config::parse_config;
/// use cooldown_core::Origin;
///
/// let layer = parse_config("min-age = \"14d\"\n", Origin::Global).unwrap();
/// assert_eq!(layer.rules.len(), 1);
/// ```
pub fn parse_config(content: &str, origin: Origin) -> Result<PolicyLayer, CoreError> {
    let cfg: ConfigToml = toml::from_str(content)
        .map_err(|e| CoreError::Config(format!("{}: {e}", origin.token())))?;
    // Consume `origin` (rather than clone) so the by-value parameter is genuinely used.
    let mut layer = PolicyLayer::new(origin);

    // Top-level default rule (only if it sets anything).
    let top_window = build_window(
        cfg.min_age.as_ref(),
        cfg.latest,
        cfg.freeze.as_deref(),
        "top-level",
    )?;
    let top_floor = cfg.floor.as_ref().map(|f| parse_duration(f)).transpose()?;
    if top_window != ByKind::default() || top_floor.is_some() {
        let mut rule = Rule::new(Selector::Default);
        rule.window = top_window;
        rule.floor = top_floor;
        layer.rules.push(rule);
    }

    // `allow` expands to package-selector exemptions.
    for pat in cfg.allow.unwrap_or_default() {
        let mut rule = Rule::new(Selector::Package(PatternGlob::new(&pat)?));
        rule.allow = true;
        layer.rules.push(rule);
    }

    if let Some(langs) = cfg.lang {
        for (name, s) in langs {
            let eco = ecosystem_id(&name).ok_or_else(|| {
                CoreError::Config(format!(
                    "unknown ecosystem `{name}` in [lang.{name}]; recognised: go, rust, python, node"
                ))
            })?;
            layer.rules.push(selector_rule(
                Selector::Lang(eco),
                &s,
                &format!("[lang.{name}]"),
            )?);
        }
    }
    if let Some(regs) = cfg.registry {
        for (name, s) in regs {
            layer.rules.push(selector_rule(
                Selector::Registry(name.clone()),
                &s,
                &format!("[registry.{name:?}]"),
            )?);
        }
    }
    if let Some(pkgs) = cfg.package {
        for (pat, s) in pkgs {
            layer.rules.push(selector_rule(
                Selector::Package(PatternGlob::new(&pat)?),
                &s,
                &format!("[package.{pat:?}]"),
            )?);
        }
    }
    if let Some(projs) = cfg.project {
        for (pat, s) in projs {
            layer.rules.push(selector_rule(
                Selector::Project(PatternGlob::new(&pat)?),
                &s,
                &format!("[project.{pat:?}]"),
            )?);
        }
    }

    layer.strict_native = cfg.strict_native;
    Ok(layer)
}

/// Policy fields gathered from env vars or CLI flags (the same shape for both).
///
/// Strings are kept unparsed here; [`layer_from_fields`] parses them when it builds the
/// [`PolicyLayer`], so an invalid duration or glob surfaces as a [`CoreError::Config`] at that
/// point rather than at collection time.
#[derive(Debug, Clone, Default)]
pub struct WindowFields {
    /// The bare `min-age` duration string (e.g. `"7d"`), used as the per-kind fallback.
    pub min_age: Option<String>,
    /// The `min-age` override for major-version updates, when set.
    pub min_age_major: Option<String>,
    /// The `min-age` override for minor-version updates, when set.
    pub min_age_minor: Option<String>,
    /// The `min-age` override for patch-version updates, when set.
    pub min_age_patch: Option<String>,
    /// Whether `--latest` (or its env var) requests the newest version with no cooldown.
    pub latest: bool,
    /// The `freeze` cutoff timestamp string, admitting only versions published on or before it.
    pub freeze: Option<String>,
    /// Glob patterns exempted from the cooldown, each becoming an `allow` package rule.
    pub allow: Vec<String>,
}

impl WindowFields {
    fn is_empty(&self) -> bool {
        self.min_age.is_none()
            && self.min_age_major.is_none()
            && self.min_age_minor.is_none()
            && self.min_age_patch.is_none()
            && !self.latest
            && self.freeze.is_none()
            && self.allow.is_empty()
    }
}

/// Builds a [`PolicyLayer`] from env/CLI [`WindowFields`], tagged with `origin`.
///
/// Returns `None` when `f` sets nothing at all. Any window settings become a
/// [`Selector::Default`] rule and each `allow` glob becomes an exempt [`Selector::Package`] rule.
/// The `latest`/`freeze`/`min-age` exclusivity is enforced here as a backstop (clap also enforces
/// it for CLI flags).
///
/// # Errors
///
/// Returns [`CoreError::Config`] when more than one of `min-age`, `latest`, and `freeze` is set,
/// when a `min-age` or `freeze` string fails to parse, or when an `allow` glob is invalid.
pub fn layer_from_fields(
    origin: Origin,
    f: &WindowFields,
) -> Result<Option<PolicyLayer>, CoreError> {
    if f.is_empty() {
        return Ok(None);
    }
    let ctx = origin.token();
    let any_min_age = f.min_age.is_some()
        || f.min_age_major.is_some()
        || f.min_age_minor.is_some()
        || f.min_age_patch.is_some();
    let set = [any_min_age, f.latest, f.freeze.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if set > 1 {
        return Err(CoreError::Config(format!(
            "{ctx}: `min-age`, `latest`, and `freeze` are mutually exclusive"
        )));
    }

    let mut layer = PolicyLayer::new(origin);

    let window = if f.latest {
        Some(ByKind::scalar(WindowSpec::Latest))
    } else if let Some(fr) = &f.freeze {
        Some(ByKind::scalar(WindowSpec::Freeze(parse_freeze(fr)?)))
    } else if any_min_age {
        let conv = |o: &Option<String>| -> Result<Option<WindowSpec>, CoreError> {
            o.as_ref()
                .map(|s| parse_duration(s).map(WindowSpec::MinAge))
                .transpose()
        };
        Some(ByKind {
            default: conv(&f.min_age)?,
            major: conv(&f.min_age_major)?,
            minor: conv(&f.min_age_minor)?,
            patch: conv(&f.min_age_patch)?,
        })
    } else {
        None
    };

    if let Some(w) = window {
        let mut rule = Rule::new(Selector::Default);
        rule.window = w;
        layer.rules.push(rule);
    }
    for pat in &f.allow {
        let mut rule = Rule::new(Selector::Package(PatternGlob::new(pat)?));
        rule.allow = true;
        layer.rules.push(rule);
    }

    if layer.rules.is_empty() {
        Ok(None)
    } else {
        Ok(Some(layer))
    }
}
