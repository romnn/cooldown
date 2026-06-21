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
    /// `[tool.<name>].exclude-folders` lists, keyed by tool name.
    pub tool_exclude_folders: BTreeMap<String, Vec<String>>,
    /// `[tool.<name>].exclude-packages` lists, keyed by tool name.
    pub tool_exclude_packages: BTreeMap<String, Vec<String>>,
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
        for (key, value) in other.tool_exclude_folders {
            self.tool_exclude_folders
                .entry(key)
                .or_default()
                .extend(value);
        }
        for (key, value) in other.tool_exclude_packages {
            self.tool_exclude_packages
                .entry(key)
                .or_default()
                .extend(value);
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

    /// Combine a resolved folder-exclude `base` (`[global]`+`[<command>]`, possibly overridden by a
    /// CLI `--exclude-folders`) with the `[tool.<eco>].exclude-folders` list for `tool`. The base is
    /// passed in rather than re-resolved here so the CLI override — applied to the resolved
    /// [`CommandConfig`](CommandConfig::override_excludes), not to this shared config — is honored.
    #[must_use]
    pub fn exclude_folders_for(&self, base: &[String], tool: &str) -> Vec<String> {
        let mut out = base.to_vec();
        if let Some(per_tool) = self.tool_exclude_folders.get(tool) {
            out.extend(per_tool.iter().cloned());
        }
        out
    }

    /// Compile every folder/package glob across `[global]`, each `[<command>]`, and each
    /// `[tool.<name>]`, so an invalid pattern is rejected when the config is parsed rather than deep
    /// inside a later scan.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] if any pattern is not a valid glob.
    fn validate(&self) -> Result<(), CoreError> {
        for section in std::iter::once(&self.global).chain(self.commands.values()) {
            super::compile_folder_globset(&section.exclude_folders)?;
            super::compile_package_globset(&section.exclude_packages)?;
        }
        for folders in self.tool_exclude_folders.values() {
            super::compile_folder_globset(folders)?;
        }
        for packages in self.tool_exclude_packages.values() {
            super::compile_package_globset(packages)?;
        }
        Ok(())
    }
}

/// Parse the non-policy [`ScanConfig`] (the `[global]`/`[<command>]`/`[tool.*]` scan settings) from
/// one config document. Returns an empty config when none of those sections are present.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if `content` is not valid config TOML, if a `[tool.<name>]`
/// carrying an `exclude-folders`/`exclude-packages` names an unknown tool, or if any exclude glob
/// is invalid.
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
        let folders = selector
            .exclude_folders
            .filter(|entries| !entries.is_empty());
        let packages = selector
            .exclude_packages
            .filter(|entries| !entries.is_empty());
        if folders.is_none() && packages.is_none() {
            continue;
        }
        let tool = tool_id(&name).ok_or_else(|| {
            CoreError::Config(format!(
                "unknown tool `{name}` in [tool.{name}]; recognised: {}",
                recognized_tool_names()
            ))
        })?;
        let key = tool.as_str().to_string();
        if let Some(folders) = folders {
            scan.tool_exclude_folders
                .entry(key.clone())
                .or_default()
                .extend(folders);
        }
        if let Some(packages) = packages {
            scan.tool_exclude_packages
                .entry(key)
                .or_default()
                .extend(packages);
        }
    }
    scan.validate()?;
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
    use indoc::indoc;

    fn scan(content: &str) -> ScanConfig {
        parse_scan_config(content, &Origin::Default).expect("valid scan config")
    }

    #[test]
    fn exclude_folders_combine_resolved_base_and_per_tool() {
        let cfg = scan(indoc! {r#"

            [global]
            exclude-folders = ["build"]

            [tool.cargo]
            exclude-folders = ["vendor"]

            [outdated]
            exclude-folders = ["fixtures"]
        "#});
        // [global] + [<command>] resolve into the base; `exclude_folders_for` adds the per-tool list
        // (order is irrelevant — it is a prune set).
        let base = cfg.resolved("outdated").exclude_folders;
        assert_eq!(base, vec!["build", "fixtures"]);
        assert_eq!(
            cfg.exclude_folders_for(&base, "cargo"),
            vec!["build", "fixtures", "vendor"]
        );
        // A different tool doesn't pick up cargo's per-tool excludes.
        assert_eq!(
            cfg.exclude_folders_for(&base, "go"),
            vec!["build", "fixtures"]
        );
        // Another command's base omits the [outdated] entry.
        assert_eq!(cfg.resolved("upgrade").exclude_folders, vec!["build"]);
    }

    #[test]
    fn exclude_packages_resolve_global_and_hold_per_tool() {
        let cfg = scan(indoc! {r#"

            [global]
            exclude-packages = ["internal-*"]

            [tool.npm]
            exclude-packages = ["@scope/*"]
        "#});
        // A `[global]` `exclude-packages` resolves into every command's base; the per-tool list is
        // held separately and combined at the member-filter site (workspace::dependencies_in_scope).
        assert_eq!(
            cfg.resolved("outdated").exclude_packages,
            vec!["internal-*"]
        );
        assert_eq!(cfg.tool_exclude_packages["npm"], vec!["@scope/*"]);
        assert!(!cfg.tool_exclude_packages.contains_key("cargo"));
        // Folders and packages are independent surfaces.
        assert!(cfg.resolved("outdated").exclude_folders.is_empty());
    }

    #[test]
    fn invalid_exclude_glob_is_rejected_at_parse() {
        assert!(
            parse_scan_config(
                "[global]\nexclude-folders = [\"a/**/[\"]\n",
                &Origin::Default
            )
            .is_err()
        );
        assert!(
            parse_scan_config("[tool.npm]\nexclude-packages = [\"[\"]\n", &Origin::Default)
                .is_err()
        );
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
        let base = scan("[global]\nexclude-folders = [\"a\"]\ngitignore = true\n");
        let over = scan("[global]\nexclude-folders = [\"b\"]\ngitignore = false\n");
        let merged = base.merge(over);
        assert_eq!(merged.resolved("outdated").exclude_folders, vec!["a", "b"]);
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
        let cfg = scan(indoc! {r#"

            [global]
            exclude-folders = ["dist"]

            [fix]
            exclude-folders = ["fixtures"]
        "#});

        assert_eq!(
            cfg.resolved("fix").exclude_folders,
            vec!["dist", "fixtures"]
        );
        assert_eq!(cfg.resolved("upgrade").exclude_folders, vec!["dist"]);
    }

    #[test]
    fn empty_config_is_inert() {
        let cfg = scan("min-age = \"7d\"\n");
        assert!(cfg.exclude_folders_for(&[], "cargo").is_empty());
        assert!(cfg.resolved("outdated").exclude_folders.is_empty());
        assert!(cfg.resolved("outdated").exclude_packages.is_empty());
        assert_eq!(cfg.resolved("outdated").gitignore, None);
        assert_eq!(cfg.resolved("outdated").major, None);
        assert_eq!(cfg.resolved("outdated").strict, None);
    }
}
