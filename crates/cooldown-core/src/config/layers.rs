use super::document::ConfigDocument;
use super::schema::{ConfigToml, MinAgeToml, SelectorToml, WindowFields};
use crate::duration::{parse_duration, parse_freeze};
use crate::error::CoreError;
use crate::model::tool_id;
use crate::policy::{ByKind, Origin, PatternGlob, PolicyLayer, Rule, Selector, WindowSpec};
use jiff::SignedDuration;

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

/// Builds the [`ByKind`] window for one selector, enforcing the `latest`/`freeze`/`min-age`
/// exclusivity.
///
/// `ctx` is a human-readable label (e.g. `"top-level"` or `"[tool.cargo]"`) interpolated into any
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
        .filter(|is_set| **is_set)
        .count();
    if set > 1 {
        return Err(CoreError::Config(format!(
            "{ctx}: `min-age`, `latest`, and `freeze` are mutually exclusive; set at most one"
        )));
    }
    if latest_set {
        return Ok(ByKind::scalar(WindowSpec::Latest));
    }
    if let Some(freeze) = freeze {
        return Ok(ByKind::scalar(WindowSpec::Freeze(parse_freeze(freeze)?)));
    }
    match min_age {
        None => Ok(ByKind::default()),
        Some(MinAgeToml::Scalar(duration)) => Ok(ByKind::scalar(WindowSpec::MinAge(
            parse_duration(duration)?,
        ))),
        Some(MinAgeToml::Table(table)) => {
            let conv = |value: &Option<String>| -> Result<Option<WindowSpec>, CoreError> {
                value
                    .as_ref()
                    .map(|duration| parse_duration(duration).map(WindowSpec::MinAge))
                    .transpose()
            };
            Ok(ByKind {
                default: conv(&table.default)?,
                major: conv(&table.major)?,
                minor: conv(&table.minor)?,
                patch: conv(&table.patch)?,
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
fn selector_rule(
    selector: Selector,
    selector_toml: &SelectorToml,
    ctx: &str,
) -> Result<Rule, CoreError> {
    let mut rule = Rule::new(selector);
    rule.window = build_window(
        selector_toml.min_age.as_ref(),
        selector_toml.latest,
        selector_toml.freeze.as_deref(),
        ctx,
    )?;
    if let Some(floor) = &selector_toml.floor {
        rule.floor = Some(parse_duration(floor)?);
    }
    Ok(rule)
}

/// Parses a `cooldown.toml`'s `content` into a [`PolicyLayer`] tagged with `origin`.
///
/// The top-level `min-age`/`latest`/`freeze`/`floor` keys become a [`Selector::Default`] rule, each
/// `allow` glob becomes an exempt [`Selector::Package`] rule, and every `[tool.*]`, `[registry.*]`,
/// `[package.*]`, and `[project.*]` table becomes its own selector rule. `strict-native` is carried
/// onto the layer as-is.
///
/// # Errors
///
/// Returns [`CoreError::Config`] when `content` is not valid TOML or has unknown fields, when a
/// `[tool.*]` key is not a recognised tool (`go`, `rust`, `python`, `node`), when a `min-age`
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
pub(crate) fn policy_layer_from_config(
    config: ConfigToml,
    origin: Origin,
) -> Result<PolicyLayer, CoreError> {
    let mut layer = PolicyLayer::new(origin);

    // Top-level default rule (only if it sets anything).
    let top_window = build_window(
        config.min_age.as_ref(),
        config.latest,
        config.freeze.as_deref(),
        "top-level",
    )?;
    let top_floor = config
        .floor
        .as_ref()
        .map(|floor| parse_duration(floor))
        .transpose()?;
    if top_window != ByKind::default() || top_floor.is_some() {
        let mut rule = Rule::new(Selector::Default);
        rule.window = top_window;
        rule.floor = top_floor;
        layer.rules.push(rule);
    }

    // `allow` expands to package-selector exemptions.
    for pattern in config.allow.unwrap_or_default() {
        let mut rule = Rule::new(Selector::Package(PatternGlob::new(&pattern)?));
        rule.allow = true;
        layer.rules.push(rule);
    }

    if let Some(tools) = config.tool {
        for (name, selector) in tools {
            let tool = tool_id(&name).ok_or_else(|| {
                CoreError::Config(format!(
                    "unknown tool `{name}` in [tool.{name}]; recognised: cargo, go, uv, node"
                ))
            })?;
            layer.rules.push(selector_rule(
                Selector::Tool(tool),
                &selector,
                &format!("[tool.{name}]"),
            )?);
        }
    }
    if let Some(registries) = config.registry {
        for (name, selector) in registries {
            layer.rules.push(selector_rule(
                Selector::Registry(name.clone()),
                &selector,
                &format!("[registry.{name:?}]"),
            )?);
        }
    }
    if let Some(packages) = config.package {
        for (pattern, selector) in packages {
            layer.rules.push(selector_rule(
                Selector::Package(PatternGlob::new(&pattern)?),
                &selector,
                &format!("[package.{pattern:?}]"),
            )?);
        }
    }
    if let Some(projects) = config.project {
        for (pattern, selector) in projects {
            layer.rules.push(selector_rule(
                Selector::Project(PatternGlob::new(&pattern)?),
                &selector,
                &format!("[project.{pattern:?}]"),
            )?);
        }
    }

    layer.strict_native = config.strict_native;
    Ok(layer)
}

/// Parse the policy view of one config document into a unified [`PolicyLayer`].
///
/// # Errors
///
/// Returns [`CoreError::Config`] if `content` is not valid config TOML or any selector/duration
/// in the policy view fails validation.
pub fn parse_config(content: &str, origin: Origin) -> Result<PolicyLayer, CoreError> {
    ConfigDocument::parse(content, &origin)?.policy_layer(origin)
}

/// Builds a [`PolicyLayer`] from env/CLI [`WindowFields`], tagged with `origin`.
///
/// Returns `None` when `fields` sets nothing at all. Any window settings become a
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
    fields: &WindowFields,
) -> Result<Option<PolicyLayer>, CoreError> {
    if fields.is_empty() {
        return Ok(None);
    }
    let ctx = origin.token();
    let any_min_age = fields.min_age.is_some()
        || fields.min_age_major.is_some()
        || fields.min_age_minor.is_some()
        || fields.min_age_patch.is_some();
    let set = [any_min_age, fields.latest, fields.freeze.is_some()]
        .iter()
        .filter(|is_set| **is_set)
        .count();
    if set > 1 {
        return Err(CoreError::Config(format!(
            "{ctx}: `min-age`, `latest`, and `freeze` are mutually exclusive"
        )));
    }

    let mut layer = PolicyLayer::new(origin);

    let window = if fields.latest {
        Some(ByKind::scalar(WindowSpec::Latest))
    } else if let Some(freeze) = &fields.freeze {
        Some(ByKind::scalar(WindowSpec::Freeze(parse_freeze(freeze)?)))
    } else if any_min_age {
        let conv = |value: &Option<String>| -> Result<Option<WindowSpec>, CoreError> {
            value
                .as_ref()
                .map(|duration| parse_duration(duration).map(WindowSpec::MinAge))
                .transpose()
        };
        Some(ByKind {
            default: conv(&fields.min_age)?,
            major: conv(&fields.min_age_major)?,
            minor: conv(&fields.min_age_minor)?,
            patch: conv(&fields.min_age_patch)?,
        })
    } else {
        None
    };

    if let Some(window) = window {
        let mut rule = Rule::new(Selector::Default);
        rule.window = window;
        layer.rules.push(rule);
    }
    for pattern in &fields.allow {
        let mut rule = Rule::new(Selector::Package(PatternGlob::new(pattern)?));
        rule.allow = true;
        layer.rules.push(rule);
    }

    if layer.rules.is_empty() {
        Ok(None)
    } else {
        Ok(Some(layer))
    }
}
