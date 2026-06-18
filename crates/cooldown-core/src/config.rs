//! The TOML config schema and its conversion into [`PolicyLayer`]s.
//!
//! One schema is used everywhere (the global file and every `cooldown.toml`). `min-age` is either a
//! duration scalar or a per-kind table — never both in one selector. Within any single selector,
//! `latest`, `freeze`, and `min-age` are mutually exclusive (a config-validation error, exit 2),
//! the same rule the CLI enforces for `--latest`/`--freeze`/`--min-age`.

use crate::duration::{parse_duration, parse_freeze};
use crate::error::CoreError;
use crate::model::tool_id;
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
    /// Scan-exclude globs. Meaningful only under `[tool.<name>]` (added to the scan exclude list
    /// for that tool); ignored on registry/package/project selectors, which are policy-only.
    exclude: Option<Vec<String>>,
}

/// CLI-flag defaults from one config section: `[global]` (shared) or a `[<command>]` section.
///
/// Every field mirrors a CLI flag. Resolution is uniform: an explicit CLI flag always wins, then a
/// `[<command>]` value, then `[global]`, then the built-in default. `None`/empty means "unset", so a
/// section only overrides what it names. Keys are kebab-case (`major-all`, `direct-only`, …), the
/// same spelling as the flags. New config-driven flags are added here and nowhere else.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct CommandConfig {
    /// Extra scan-exclude globs (added to `[global]` and `[tool.*]` excludes). `--exclude` has no
    /// CLI form; this is the only way to set it.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Restrict to these tools (`--tool`); empty means "all detected".
    #[serde(default)]
    pub tool: Vec<String>,
    /// Scope to packages matching these globs (`--package`); empty means "all".
    #[serde(default)]
    pub package: Vec<String>,
    /// `.gitignore` honoring during detection (`--no-gitignore` forces off).
    pub gitignore: Option<bool>,
    /// Cross-major candidate scope (`--major` / `--no-major`).
    pub major: Option<bool>,
    /// Apply cross-major to all eligible deps (`--major-all`).
    pub major_all: Option<bool>,
    /// List up-to-date deps in `outdated` (`--all`).
    pub all: Option<bool>,
    /// Evaluate only direct deps (`--direct-only`).
    pub direct_only: Option<bool>,
    /// Include transitive deps in `outdated` (`--include-indirect`).
    pub include_indirect: Option<bool>,
    /// Gate every recorded artifact in `check` (`--all-artifacts`).
    pub all_artifacts: Option<bool>,
    /// Downgrade a stale/absent lock to a warning (`--allow-stale-lock`).
    pub allow_stale_lock: Option<bool>,
    /// Make `check` fail on deps with no publish time (`--fail-on-unknown-age`).
    pub fail_on_unknown_age: Option<bool>,
    /// Fail `upgrade` if any planned change was skipped (`--strict`).
    pub strict: Option<bool>,
    /// Compile/sync after re-locking in `upgrade` (`--build`).
    pub build: Option<bool>,
    /// Resolve and print the plan; never mutate (`--dry-run`).
    pub dry_run: Option<bool>,
    /// Cache only; a miss becomes `UnknownAge` (`--offline`).
    pub offline: Option<bool>,
    /// Ignore the local cache; always hit the registry (`--fresh`).
    pub fresh: Option<bool>,
    /// Machine-readable output (`--json`).
    pub json: Option<bool>,
    /// `outdated` CI gate exit code (`--exit-code`).
    pub exit_code: Option<u8>,
    /// Concurrency for the registry fan-out (no CLI flag; defaults to 8).
    pub concurrency: Option<usize>,
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
    tool: Option<BTreeMap<String, SelectorToml>>,
    registry: Option<BTreeMap<String, SelectorToml>>,
    package: Option<BTreeMap<String, SelectorToml>>,
    project: Option<BTreeMap<String, SelectorToml>>,
    /// Shared CLI-flag defaults across all subcommands.
    global: Option<CommandConfig>,
    /// Per-subcommand CLI-flag defaults; each overrides `[global]`.
    outdated: Option<CommandConfig>,
    upgrade: Option<CommandConfig>,
    check: Option<CommandConfig>,
    baseline: Option<CommandConfig>,
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

    if let Some(tools) = cfg.tool {
        for (name, s) in tools {
            let eco = tool_id(&name).ok_or_else(|| {
                CoreError::Config(format!(
                    "unknown tool `{name}` in [tool.{name}]; recognised: cargo, go, uv, node"
                ))
            })?;
            layer.rules.push(selector_rule(
                Selector::Tool(eco),
                &s,
                &format!("[tool.{name}]"),
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

/// The non-policy, CLI-flag-shaped config: `[global]` defaults, per-subcommand overrides, and
/// per-tool scan excludes. Separate from the policy [`PolicyLayer`] because these settings tune
/// *how* a command runs (scanning, scope) rather than the cooldown window itself.
#[derive(Debug, Clone, Default)]
pub struct ScanConfig {
    /// Shared `[global]` defaults.
    pub global: CommandConfig,
    /// Per-subcommand sections, keyed by command name (`"outdated"`, `"upgrade"`, …).
    pub commands: BTreeMap<String, CommandConfig>,
    /// `[tool.<name>].exclude` lists, keyed by tool name.
    pub tool_exclude: BTreeMap<String, Vec<String>>,
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
        for (key, value) in other.tool_exclude {
            self.tool_exclude.entry(key).or_default().extend(value);
        }
        self
    }

    /// The resolved flag defaults for `command`: `[global]` with the `[<command>]` section merged
    /// over it. The caller layers an explicit CLI flag on top of this.
    #[must_use]
    pub fn resolved(&self, command: &str) -> CommandConfig {
        let mut cfg = self.global.clone();
        if let Some(section) = self.commands.get(command) {
            cfg = merge_command(cfg, section.clone());
        }
        cfg
    }

    /// Scan-exclude globs for `command` + `tool`: `[global]` + `[<command>]` (via
    /// [`resolved`](Self::resolved)) plus the `[tool.<eco>].exclude` list.
    #[must_use]
    pub fn exclude_for(&self, command: &str, tool: &str) -> Vec<String> {
        let mut out = self.resolved(command).exclude;
        if let Some(per_tool) = self.tool_exclude.get(tool) {
            out.extend(per_tool.iter().cloned());
        }
        out
    }
}

/// Merge `other` (higher precedence) over `base`: list fields concatenate; scalar `Option` fields
/// take `other`'s value when it sets one.
fn merge_command(mut base: CommandConfig, mut other: CommandConfig) -> CommandConfig {
    base.exclude.append(&mut other.exclude);
    base.tool.append(&mut other.tool);
    base.package.append(&mut other.package);
    base.gitignore = other.gitignore.or(base.gitignore);
    base.major = other.major.or(base.major);
    base.major_all = other.major_all.or(base.major_all);
    base.all = other.all.or(base.all);
    base.direct_only = other.direct_only.or(base.direct_only);
    base.include_indirect = other.include_indirect.or(base.include_indirect);
    base.all_artifacts = other.all_artifacts.or(base.all_artifacts);
    base.allow_stale_lock = other.allow_stale_lock.or(base.allow_stale_lock);
    base.fail_on_unknown_age = other.fail_on_unknown_age.or(base.fail_on_unknown_age);
    base.strict = other.strict.or(base.strict);
    base.build = other.build.or(base.build);
    base.dry_run = other.dry_run.or(base.dry_run);
    base.offline = other.offline.or(base.offline);
    base.fresh = other.fresh.or(base.fresh);
    base.json = other.json.or(base.json);
    base.exit_code = other.exit_code.or(base.exit_code);
    base.concurrency = other.concurrency.or(base.concurrency);
    base
}

/// Parse the non-policy [`ScanConfig`] (the `[global]`/`[<command>]`/`[tool.*]` scan settings) from
/// one config document. Returns an empty config when none of those sections are present.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if `content` is not valid config TOML, or if a `[tool.<name>]`
/// carrying an `exclude` names an unknown tool.
pub fn parse_scan_config(content: &str, origin: &Origin) -> Result<ScanConfig, CoreError> {
    let cfg: ConfigToml = toml::from_str(content)
        .map_err(|e| CoreError::Config(format!("{}: {e}", origin.token())))?;
    let mut scan = ScanConfig {
        global: cfg.global.unwrap_or_default(),
        ..ScanConfig::default()
    };
    for (name, section) in [
        ("outdated", cfg.outdated),
        ("upgrade", cfg.upgrade),
        ("check", cfg.check),
        ("baseline", cfg.baseline),
    ] {
        if let Some(section) = section {
            scan.commands.insert(name.to_string(), section);
        }
    }
    for (name, selector) in cfg.tool.unwrap_or_default() {
        let Some(exclude) = selector.exclude.filter(|e| !e.is_empty()) else {
            continue;
        };
        let eco = tool_id(&name).ok_or_else(|| {
            CoreError::Config(format!(
                "unknown tool `{name}` in [tool.{name}]; recognised: cargo, go, uv, node"
            ))
        })?;
        scan.tool_exclude
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
    fn exclude_combines_global_tool_and_command() {
        let cfg = scan(
            r#"
[global]
exclude = ["build"]

[tool.cargo]
exclude = ["vendor"]

[outdated]
exclude = ["fixtures"]
"#,
        );
        // The scan exclude list combines [global] + [<command>] + [tool.<eco>] (order is
        // irrelevant — it is a prune set).
        assert_eq!(
            cfg.exclude_for("outdated", "cargo"),
            vec!["build", "fixtures", "vendor"]
        );
        // Another command gets [global] + [tool] but not the [outdated] entry.
        assert_eq!(cfg.exclude_for("upgrade", "cargo"), vec!["build", "vendor"]);
        // A different tool doesn't pick up cargo's per-tool excludes.
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
            cfg.resolved("outdated").gitignore,
            Some(false),
            "command overrides global"
        );
        assert_eq!(
            cfg.resolved("upgrade").gitignore,
            Some(true),
            "falls back to global"
        );
        assert_eq!(
            cfg.resolved("outdated").major,
            Some(true),
            "inherited from global"
        );
        assert_eq!(cfg.resolved("check").major, Some(true));
    }

    #[test]
    fn merge_concatenates_excludes_and_lets_later_scalars_win() {
        let base = scan("[global]\nexclude = [\"a\"]\ngitignore = true\n");
        let over = scan("[global]\nexclude = [\"b\"]\ngitignore = false\n");
        let merged = base.merge(over);
        assert_eq!(merged.exclude_for("outdated", "cargo"), vec!["a", "b"]);
        assert_eq!(merged.resolved("outdated").gitignore, Some(false));
    }

    #[test]
    fn all_flags_resolve_with_command_over_global() {
        let cfg = scan(
            r"
[global]
strict = true
offline = true
concurrency = 4

[upgrade]
strict = false
build = true
",
        );
        let up = cfg.resolved("upgrade");
        assert_eq!(up.strict, Some(false), "command overrides global");
        assert_eq!(up.build, Some(true));
        assert_eq!(up.offline, Some(true), "inherited from global");
        assert_eq!(up.concurrency, Some(4));
        assert_eq!(
            cfg.resolved("check").strict,
            Some(true),
            "other commands see global"
        );
    }

    #[test]
    fn empty_config_is_inert() {
        let cfg = scan("min-age = \"7d\"\n");
        assert!(cfg.exclude_for("outdated", "cargo").is_empty());
        assert_eq!(cfg.resolved("outdated").gitignore, None);
        assert_eq!(cfg.resolved("outdated").major, None);
        assert_eq!(cfg.resolved("outdated").strict, None);
    }
}
