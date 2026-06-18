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
    /// Scan-exclude globs. Meaningful only under `[lang.<name>]` (added to the scan exclude list
    /// for that ecosystem); ignored on registry/package/project selectors, which are policy-only.
    exclude: Option<Vec<String>>,
}

/// A per-subcommand (or `[global]`) settings section: CLI-flag defaults that live in the config.
/// A CLI flag, when given, always overrides the value here; a per-command section overrides
/// `[global]`. New fields are added here as more flags become config-driven.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandToml {
    /// Extra scan-exclude globs for this command (added to `[global]` and `[lang.*]` excludes).
    exclude: Option<Vec<String>>,
    /// Whether `.gitignore` is honored during detection (overridable by `--no-gitignore`).
    gitignore: Option<bool>,
    /// Whether cross-major candidates are in scope (overridable by `--major` / `--no-major`).
    major: Option<bool>,
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
    /// Shared CLI-flag defaults across all subcommands.
    global: Option<CommandToml>,
    /// Per-subcommand CLI-flag defaults; each overrides `[global]`.
    outdated: Option<CommandToml>,
    upgrade: Option<CommandToml>,
    check: Option<CommandToml>,
    baseline: Option<CommandToml>,
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

/// Resolved CLI-flag defaults from one config section (`[global]` or a `[<command>]` section).
#[derive(Debug, Clone, Default)]
pub struct CommandConfig {
    /// Scan-exclude globs contributed by this section.
    pub exclude: Vec<String>,
    /// `.gitignore` honoring, when the section sets it.
    pub gitignore: Option<bool>,
    /// Cross-major scope, when the section sets it.
    pub major: Option<bool>,
}

/// The non-policy, CLI-flag-shaped config: `[global]` defaults, per-subcommand overrides, and
/// per-language scan excludes. Separate from the policy [`PolicyLayer`] because these settings tune
/// *how* a command runs (scanning, scope) rather than the cooldown window itself.
#[derive(Debug, Clone, Default)]
pub struct ScanConfig {
    /// Shared `[global]` defaults.
    pub global: CommandConfig,
    /// Per-subcommand sections, keyed by command name (`"outdated"`, `"upgrade"`, …).
    pub commands: BTreeMap<String, CommandConfig>,
    /// `[lang.<name>].exclude` lists, keyed by ecosystem name.
    pub lang_exclude: BTreeMap<String, Vec<String>>,
}

impl ScanConfig {
    /// Merge a higher-precedence layer (`other`) over `self`: exclude lists concatenate; scalar
    /// overrides (`gitignore`/`major`) from `other` win when set.
    #[must_use]
    pub fn merge(mut self, other: ScanConfig) -> ScanConfig {
        self.global = merge_command(self.global, other.global);
        for (key, value) in other.commands {
            let slot = self.commands.entry(key).or_default();
            *slot = merge_command(std::mem::take(slot), value);
        }
        for (key, value) in other.lang_exclude {
            self.lang_exclude.entry(key).or_default().extend(value);
        }
        self
    }

    /// Scan-exclude globs for `command` + `ecosystem`: `[global]` + `[lang.<eco>]` + `[<command>]`.
    #[must_use]
    pub fn exclude_for(&self, command: &str, ecosystem: &str) -> Vec<String> {
        let mut out = self.global.exclude.clone();
        if let Some(lang) = self.lang_exclude.get(ecosystem) {
            out.extend(lang.iter().cloned());
        }
        if let Some(cmd) = self.commands.get(command) {
            out.extend(cmd.exclude.iter().cloned());
        }
        out
    }

    /// Whether `.gitignore` is honored for `command` (a `[<command>]` value overrides `[global]`).
    #[must_use]
    pub fn gitignore_for(&self, command: &str) -> Option<bool> {
        self.commands
            .get(command)
            .and_then(|c| c.gitignore)
            .or(self.global.gitignore)
    }

    /// The configured cross-major default for `command` (a `[<command>]` value overrides `[global]`).
    #[must_use]
    pub fn major_for(&self, command: &str) -> Option<bool> {
        self.commands
            .get(command)
            .and_then(|c| c.major)
            .or(self.global.major)
    }
}

fn merge_command(mut base: CommandConfig, other: CommandConfig) -> CommandConfig {
    base.exclude.extend(other.exclude);
    if other.gitignore.is_some() {
        base.gitignore = other.gitignore;
    }
    if other.major.is_some() {
        base.major = other.major;
    }
    base
}

fn command_config(section: Option<CommandToml>) -> CommandConfig {
    let section = section.unwrap_or_default();
    CommandConfig {
        exclude: section.exclude.unwrap_or_default(),
        gitignore: section.gitignore,
        major: section.major,
    }
}

/// Parse the non-policy [`ScanConfig`] (the `[global]`/`[<command>]`/`[lang.*]` scan settings) from
/// one config document. Returns an empty config when none of those sections are present.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if `content` is not valid config TOML, or if a `[lang.<name>]`
/// carrying an `exclude` names an unknown ecosystem.
pub fn parse_scan_config(content: &str, origin: &Origin) -> Result<ScanConfig, CoreError> {
    let cfg: ConfigToml = toml::from_str(content)
        .map_err(|e| CoreError::Config(format!("{}: {e}", origin.token())))?;
    let mut scan = ScanConfig {
        global: command_config(cfg.global),
        ..ScanConfig::default()
    };
    for (name, section) in [
        ("outdated", cfg.outdated),
        ("upgrade", cfg.upgrade),
        ("check", cfg.check),
        ("baseline", cfg.baseline),
    ] {
        if section.is_some() {
            scan.commands
                .insert(name.to_string(), command_config(section));
        }
    }
    for (name, selector) in cfg.lang.unwrap_or_default() {
        let Some(exclude) = selector.exclude.filter(|e| !e.is_empty()) else {
            continue;
        };
        let eco = ecosystem_id(&name).ok_or_else(|| {
            CoreError::Config(format!(
                "unknown ecosystem `{name}` in [lang.{name}]; recognised: go, rust, python, node"
            ))
        })?;
        scan.lang_exclude
            .entry(eco.as_str().to_string())
            .or_default()
            .extend(exclude);
    }
    Ok(scan)
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

#[cfg(test)]
mod tests {
    use super::{ScanConfig, parse_scan_config};
    use crate::policy::Origin;

    fn scan(content: &str) -> ScanConfig {
        parse_scan_config(content, &Origin::Default).expect("valid scan config")
    }

    #[test]
    fn exclude_combines_global_lang_and_command() {
        let cfg = scan(
            r#"
[global]
exclude = ["build"]

[lang.rust]
exclude = ["vendor"]

[outdated]
exclude = ["fixtures"]
"#,
        );
        // The scan exclude list comes from [global] + [lang.<eco>] + [<command>], in that order.
        assert_eq!(
            cfg.exclude_for("outdated", "rust"),
            vec!["build", "vendor", "fixtures"]
        );
        // Another command gets [global] + [lang] but not the [outdated] entry.
        assert_eq!(cfg.exclude_for("upgrade", "rust"), vec!["build", "vendor"]);
        // A different ecosystem doesn't pick up rust's per-language excludes.
        assert_eq!(cfg.exclude_for("outdated", "go"), vec!["build", "fixtures"]);
    }

    #[test]
    fn command_section_overrides_global_scalars() {
        let cfg = scan(
            r"
[global]
gitignore = true
major = true

[outdated]
gitignore = false
",
        );
        assert_eq!(
            cfg.gitignore_for("outdated"),
            Some(false),
            "command overrides global"
        );
        assert_eq!(
            cfg.gitignore_for("upgrade"),
            Some(true),
            "falls back to global"
        );
        assert_eq!(
            cfg.major_for("outdated"),
            Some(true),
            "inherited from global"
        );
        assert_eq!(cfg.major_for("check"), Some(true));
    }

    #[test]
    fn merge_concatenates_excludes_and_lets_later_scalars_win() {
        let base = scan("[global]\nexclude = [\"a\"]\ngitignore = true\n");
        let over = scan("[global]\nexclude = [\"b\"]\ngitignore = false\n");
        let merged = base.merge(over);
        assert_eq!(merged.exclude_for("outdated", "rust"), vec!["a", "b"]);
        assert_eq!(merged.gitignore_for("outdated"), Some(false));
    }

    #[test]
    fn empty_config_is_inert() {
        let cfg = scan("min-age = \"7d\"\n");
        assert!(cfg.exclude_for("outdated", "rust").is_empty());
        assert_eq!(cfg.gitignore_for("outdated"), None);
        assert_eq!(cfg.major_for("outdated"), None);
    }
}
