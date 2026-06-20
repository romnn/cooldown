use super::document::ConfigDocument;
use super::schema::{CommandConfig, ConfigToml};
use crate::error::CoreError;
use crate::model::{recognized_tool_names, tool_id};
use crate::policy::Origin;
use std::collections::BTreeMap;

/// The non-policy, CLI-flag-shaped config: `[global]` defaults, per-subcommand overrides, and
/// per-tool scan excludes. Separate from the policy [`PolicyLayer`](crate::PolicyLayer) because
/// these settings tune *how* a command runs (scanning, scope) rather than the cooldown window
/// itself.
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
        self.global = self.global.merge_layer(other.global);
        for (key, value) in other.commands {
            let slot = self.commands.entry(key).or_default();
            *slot = std::mem::take(slot).merge_layer(value);
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
        let mut config = self.global.clone();
        if let Some(section) = self.commands.get(command) {
            config = config.merge_layer(section.clone());
        }
        config
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

/// Parse the non-policy [`ScanConfig`] (the `[global]`/`[<command>]`/`[tool.*]` scan settings) from
/// one config document. Returns an empty config when none of those sections are present.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if `content` is not valid config TOML, or if a `[tool.<name>]`
/// carrying an `exclude` names an unknown tool.
pub(crate) fn scan_config_from_config(
    config: ConfigToml,
    _origin: &Origin,
) -> Result<ScanConfig, CoreError> {
    let mut scan = ScanConfig {
        global: config.global.unwrap_or_default(),
        ..ScanConfig::default()
    };
    for (name, section) in [
        ("outdated", config.outdated),
        ("upgrade", config.upgrade),
        ("fix", config.fix),
        ("check", config.check),
        ("baseline", config.baseline),
    ] {
        if let Some(section) = section {
            scan.commands.insert(name.to_string(), section);
        }
    }
    for (name, selector) in config.tool.unwrap_or_default() {
        let Some(exclude) = selector.exclude.filter(|entries| !entries.is_empty()) else {
            continue;
        };
        let tool = tool_id(&name).ok_or_else(|| {
            CoreError::Config(format!(
                "unknown tool `{name}` in [tool.{name}]; recognised: {}",
                recognized_tool_names()
            ))
        })?;
        scan.tool_exclude
            .entry(tool.as_str().to_string())
            .or_default()
            .extend(exclude);
    }
    Ok(scan)
}

/// Parse the non-policy scan/runtime config view from one config document.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if `content` is not valid config TOML, or if a `[tool.<name>]`
/// scan setting names an unknown tool.
pub fn parse_scan_config(content: &str, origin: &Origin) -> Result<ScanConfig, CoreError> {
    ConfigDocument::parse(content, origin)?.scan_config(origin)
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

[fix]
strict = false
transitive = true
downgrade-pinned = true
dry-run = true
",
        );
        let upgrade = cfg.resolved("upgrade");
        assert_eq!(upgrade.strict, Some(false), "command overrides global");
        assert_eq!(upgrade.build, Some(true));
        assert_eq!(upgrade.offline, Some(true), "inherited from global");
        assert_eq!(upgrade.concurrency, Some(4));
        assert_eq!(
            cfg.resolved("check").strict,
            Some(true),
            "other commands see global"
        );
        let fix = cfg.resolved("fix");
        assert_eq!(fix.strict, Some(false), "fix overrides global");
        assert_eq!(fix.transitive, Some(true));
        assert_eq!(fix.downgrade_pinned, Some(true));
        assert_eq!(fix.dry_run, Some(true));
        assert_eq!(fix.offline, Some(true), "fix inherits global");
    }

    #[test]
    fn fix_section_contributes_to_scan_excludes() {
        let cfg = scan(
            r#"
[global]
exclude = ["dist"]

[fix]
exclude = ["fixtures"]
"#,
        );

        assert_eq!(cfg.exclude_for("fix", "cargo"), vec!["dist", "fixtures"]);
        assert_eq!(cfg.exclude_for("upgrade", "cargo"), vec!["dist"]);
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
