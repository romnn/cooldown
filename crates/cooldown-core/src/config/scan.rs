use super::ExcludeList;
use super::document::ConfigDocument;
use super::schema::{CommandConfig, ConfigToml};
use crate::error::CoreError;
use crate::model::{recognized_tool_names, tool_id};
use crate::policy::Origin;
use std::collections::BTreeMap;

/// One config file's `[global]` and `[<command>]` sections.
#[derive(Debug, Clone, Default)]
pub struct CommandSections {
    /// Shared `[global]` defaults.
    pub global: CommandConfig,
    /// Per-subcommand sections, keyed by command name (`"outdated"`, `"upgrade"`, …).
    pub commands: BTreeMap<String, CommandConfig>,
}

impl CommandSections {
    /// This file's folder and package exclude lists for `command`: the `[<command>]` list folded
    /// over `[global]`.
    fn exclude_lists(&self, command: &str) -> (ExcludeList, ExcludeList) {
        let section = self.commands.get(command);
        let folders = self.global.exclude_folders.clone().merge(
            section
                .map(|section| section.exclude_folders.clone())
                .unwrap_or_default(),
        );
        let packages = self.global.exclude_packages.clone().merge(
            section
                .map(|section| section.exclude_packages.clone())
                .unwrap_or_default(),
        );
        (folders, packages)
    }

    /// Whether any section of this file sets an exclude list (an explicit `[]` counts).
    fn sets_exclude_lists(&self) -> bool {
        std::iter::once(&self.global)
            .chain(self.commands.values())
            .any(|section| {
                section.exclude_folders != ExcludeList::default()
                    || section.exclude_packages != ExcludeList::default()
            })
    }
}

/// The non-policy, CLI-flag-shaped config: `[global]` defaults, per-subcommand overrides, and
/// per-tool scan excludes. Separate from the policy [`PolicyLayer`](crate::PolicyLayer) because
/// these settings tune *how* a command runs (scanning, scope) rather than the cooldown window
/// itself.
///
/// The files stay separate layers rather than collapsing on merge, because the two precedence
/// axes — file nearness and section specificity — compose differently for the exclude lists than
/// for the scalar flags; see [`resolved`](Self::resolved).
#[derive(Debug, Clone, Default)]
pub struct ScanConfig {
    /// The `[global]`/`[<command>]` sections of every merged file, lowest precedence first.
    pub layers: Vec<CommandSections>,
    /// `[tool.<name>].exclude-folders` lists, keyed by canonical tool name.
    /// Each carries its [`ExcludeList`] merge mode so a nearer file can clear or replace the
    /// inherited list.
    pub tool_exclude_folders: BTreeMap<String, ExcludeList>,
    /// `[tool.<name>].exclude-packages` lists, keyed by canonical tool name; merges like
    /// [`tool_exclude_folders`](Self::tool_exclude_folders).
    pub tool_exclude_packages: BTreeMap<String, ExcludeList>,
}

impl ScanConfig {
    /// Merge a higher-precedence file (`other`) over `self`: its sections become the nearest
    /// layer, and its per-tool exclude lists fold per [`ExcludeList::merge`] (a plain list
    /// concatenates, `[]` clears, `{ replace = [...] }` replaces).
    #[must_use]
    pub fn merge(mut self, other: ScanConfig) -> ScanConfig {
        self.layers.extend(other.layers);
        merge_tool_lists(&mut self.tool_exclude_folders, other.tool_exclude_folders);
        merge_tool_lists(&mut self.tool_exclude_packages, other.tool_exclude_packages);
        self
    }

    /// The per-tool `exclude-folders` patterns after every layer has been merged, keyed by
    /// canonical tool name — the shape the run consumes once merge modes no longer matter.
    #[must_use]
    pub fn tool_exclude_folder_patterns(&self) -> BTreeMap<String, Vec<String>> {
        tool_patterns(&self.tool_exclude_folders)
    }

    /// The per-tool `exclude-packages` patterns after every layer has been merged, keyed by
    /// canonical tool name.
    #[must_use]
    pub fn tool_exclude_package_patterns(&self) -> BTreeMap<String, Vec<String>> {
        tool_patterns(&self.tool_exclude_packages)
    }

    /// Whether any merged file sets an exclude list, in a section or a `[tool.*]` table.
    #[must_use]
    pub fn sets_exclude_lists(&self) -> bool {
        self.layers.iter().any(CommandSections::sets_exclude_lists)
            || !self.tool_exclude_folders.is_empty()
            || !self.tool_exclude_packages.is_empty()
    }

    /// The resolved flag defaults for `command`.
    /// The caller layers an explicit CLI flag on top of this.
    ///
    /// Scalars (and the `tool`/`package` scoping lists) fold per key across the files, then the
    /// `[<command>]` value overrides `[global]`, so a `[<command>]` value in any file beats a
    /// `[global]` value in every file.
    ///
    /// The exclude lists resolve `[<command>]` over `[global]` within each file first and fold
    /// the files second.
    /// Folding the files first would let a `[<command>]` replacement in a farther file (a user's
    /// global config) silently void a `[global]` exclusion in a nearer one (the repository's), so
    /// a replacement reaches only what the same file and the files below it contributed.
    #[must_use]
    pub fn resolved(&self, command: &str) -> CommandConfig {
        let global = self
            .layers
            .iter()
            .fold(CommandConfig::default(), |acc, layer| {
                acc.merge_layer(layer.global.clone())
            });
        let section = self
            .layers
            .iter()
            .filter_map(|layer| layer.commands.get(command))
            .fold(CommandConfig::default(), |acc, section| {
                acc.merge_layer(section.clone())
            });
        let mut config = global.merge_layer(section);
        let (folders, packages) = self.layers.iter().fold(
            (ExcludeList::default(), ExcludeList::default()),
            |(folders, packages), layer| {
                let (file_folders, file_packages) = layer.exclude_lists(command);
                (folders.merge(file_folders), packages.merge(file_packages))
            },
        );
        config.exclude_folders = folders;
        config.exclude_packages = packages;
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
            out.extend(per_tool.patterns().iter().cloned());
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
        for section in self
            .layers
            .iter()
            .flat_map(|layer| std::iter::once(&layer.global).chain(layer.commands.values()))
        {
            super::compile_folder_globset(section.exclude_folders.patterns())?;
            super::compile_package_globset(section.exclude_packages.patterns())?;
        }
        for folders in self.tool_exclude_folders.values() {
            super::compile_folder_globset(folders.patterns())?;
        }
        for packages in self.tool_exclude_packages.values() {
            super::compile_package_globset(packages.patterns())?;
        }
        Ok(())
    }
}

/// Folds each of `layer`'s per-tool lists over the same tool's list in `base`.
fn merge_tool_lists(
    base: &mut BTreeMap<String, ExcludeList>,
    layer: BTreeMap<String, ExcludeList>,
) {
    for (tool, list) in layer {
        let slot = base.entry(tool).or_default();
        *slot = std::mem::take(slot).merge(list);
    }
}

fn tool_patterns(lists: &BTreeMap<String, ExcludeList>) -> BTreeMap<String, Vec<String>> {
    lists
        .iter()
        .map(|(tool, list)| (tool.clone(), list.patterns().to_vec()))
        .collect()
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
    origin: &Origin,
) -> Result<ScanConfig, CoreError> {
    let mut sections = CommandSections {
        global: config.global.unwrap_or_default(),
        commands: BTreeMap::new(),
    };
    for (name, section) in [
        ("outdated", config.outdated),
        ("upgrade", config.upgrade),
        ("fix", config.fix),
        ("check", config.check),
        ("baseline", config.baseline),
    ] {
        if let Some(section) = section {
            sections.commands.insert(name.to_string(), section);
        }
    }
    let mut scan = ScanConfig {
        layers: vec![sections],
        ..ScanConfig::default()
    };
    // Aliases (`[tool.rust]`, `[tool.cargo]`) name one canonical tool.
    // Two such tables in one file are one layer, not two, and TOML gives them no author-visible
    // order, so a `[]` or `{ replace = [...] }` in one of them could only fold in an arbitrary
    // order against the same key in the other; refuse that instead of guessing.
    // Two tables setting different keys are unambiguous and stay accepted.
    let mut folder_tables: BTreeMap<String, String> = BTreeMap::new();
    let mut package_tables: BTreeMap<String, String> = BTreeMap::new();
    for (name, selector) in config.tool.unwrap_or_default() {
        // A present-but-empty list is kept: it is the "clear what was inherited" form.
        if selector.exclude_folders.is_none() && selector.exclude_packages.is_none() {
            continue;
        }
        let tool = tool_id(&name).ok_or_else(|| {
            CoreError::Config(format!(
                "{}: unknown tool `{name}` in [tool.{name}]; recognised: {}",
                origin.token(),
                recognized_tool_names()
            ))
        })?;
        let key = tool.as_str().to_string();
        if let Some(folders) = selector.exclude_folders {
            reject_alias_pair(origin, &mut folder_tables, &key, &name, "exclude-folders")?;
            scan.tool_exclude_folders.insert(key.clone(), folders);
        }
        if let Some(packages) = selector.exclude_packages {
            reject_alias_pair(origin, &mut package_tables, &key, &name, "exclude-packages")?;
            scan.tool_exclude_packages.insert(key, packages);
        }
    }
    scan.validate()?;
    Ok(scan)
}

/// Records that `[tool.<name>]` sets `list` for the canonical tool `key`, refusing a second alias
/// table of the same tool that sets it too.
fn reject_alias_pair(
    origin: &Origin,
    seen: &mut BTreeMap<String, String>,
    key: &str,
    name: &str,
    list: &str,
) -> Result<(), CoreError> {
    if let Some(earlier) = seen.insert(key.to_string(), name.to_string()) {
        return Err(CoreError::Config(format!(
            "{}: [tool.{earlier}] and [tool.{name}] both name `{key}` and both set `{list}`; \
             keep it in one table",
            origin.token()
        )));
    }
    Ok(())
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
        let base = cfg.resolved("outdated").exclude_folders.patterns().to_vec();
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
        assert_eq!(
            cfg.resolved("upgrade").exclude_folders.patterns(),
            ["build"]
        );
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
            cfg.resolved("outdated").exclude_packages.patterns(),
            ["internal-*"]
        );
        assert_eq!(cfg.tool_exclude_packages["npm"].patterns(), ["@scope/*"]);
        assert!(!cfg.tool_exclude_packages.contains_key("cargo"));
        // Folders and packages are independent surfaces.
        assert!(
            cfg.resolved("outdated")
                .exclude_folders
                .patterns()
                .is_empty()
        );
    }

    #[test]
    fn edge_policy_is_rejected_under_non_cargo_tools() {
        let err = parse_scan_config("[tool.npm]\nedge-policy = \"preserve\"\n", &Origin::Default)
            .expect_err("edge-policy under a non-cargo tool must be rejected");
        assert!(
            err.to_string().contains("[tool.cargo]"),
            "the error points at the correct placement: {err}"
        );
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
    fn respect_dist_tags_resolves_command_over_global_over_absent() {
        let cfg = scan(indoc::indoc! {"
            [global]
            respect-dist-tags = false

            [upgrade]
            respect-dist-tags = true
        "});
        assert_eq!(
            cfg.resolved("upgrade").respect_dist_tags,
            Some(true),
            "command overrides global"
        );
        assert_eq!(
            cfg.resolved("outdated").respect_dist_tags,
            Some(false),
            "inherited from global"
        );
        // Absent everywhere: the built-in default (on) applies downstream.
        let bare = scan("[global]\n");
        assert_eq!(bare.resolved("outdated").respect_dist_tags, None);
    }

    #[test]
    fn merge_concatenates_excludes_and_lets_later_scalars_win() {
        let base = scan("[global]\nexclude-folders = [\"a\"]\ngitignore = true\n");
        let over = scan("[global]\nexclude-folders = [\"b\"]\ngitignore = false\n");
        let merged = base.merge(over);
        assert_eq!(
            merged.resolved("outdated").exclude_folders.patterns(),
            ["a", "b"]
        );
        assert_eq!(merged.resolved("outdated").gitignore, Some(false));
    }

    /// An empty list in a nearer file clears the list it inherits, for `[global]`, a command
    /// table, and a per-tool table alike, rather than leaving the concatenated parent list
    /// standing.
    #[test]
    fn empty_list_in_a_nearer_layer_clears_the_inherited_excludes() {
        let parent = scan(indoc! {r#"
            [global]
            exclude-folders = ["vendor"]
            exclude-packages = ["internal-*"]

            [outdated]
            exclude-folders = ["fixtures"]

            [tool.cargo]
            exclude-folders = ["fuzz"]
            exclude-packages = ["bench-*"]
        "#});
        let child = scan(indoc! {r"
            [global]
            exclude-folders = []

            [outdated]
            exclude-folders = []

            [tool.cargo]
            exclude-folders = []
            exclude-packages = []
        "});
        let merged = parent.merge(child);
        let outdated = merged.resolved("outdated");
        assert!(outdated.exclude_folders.patterns().is_empty());
        // Only the lists the child names are cleared.
        assert_eq!(outdated.exclude_packages.patterns(), ["internal-*"]);
        assert!(merged.exclude_folders_for(&[], "cargo").is_empty());
        assert!(merged.tool_exclude_packages["cargo"].patterns().is_empty());
        assert_eq!(
            merged.tool_exclude_package_patterns()["cargo"],
            Vec::<String>::new()
        );
    }

    /// `{ replace = [...] }` swaps the inherited list for the given one; a later plain list still
    /// adds to that replacement.
    #[test]
    fn replace_table_swaps_the_inherited_excludes() {
        let global_file = scan(indoc! {r#"
            [global]
            exclude-folders = ["vendor"]

            [tool.npm]
            exclude-packages = ["@org/*"]
        "#});
        let repo_file = scan(indoc! {r#"
            [global]
            exclude-folders = { replace = ["examples"] }

            [tool.npm]
            exclude-packages = { replace = ["@scope/*"] }
        "#});
        let explicit_file = scan(indoc! {r#"
            [tool.npm]
            exclude-packages = ["@extra/*"]
        "#});
        let merged = global_file.merge(repo_file).merge(explicit_file);
        assert_eq!(
            merged.resolved("outdated").exclude_folders.patterns(),
            ["examples"]
        );
        assert_eq!(
            merged.tool_exclude_package_patterns()["npm"],
            vec!["@scope/*", "@extra/*"]
        );
    }

    /// A command table's replacement shadows `[global]` for that command only; the explicit
    /// `{ extend = [...] }` spelling behaves exactly like a plain array.
    #[test]
    fn command_replace_shadows_global_for_that_command_only() {
        let cfg = scan(indoc! {r#"
            [global]
            exclude-folders = ["build"]

            [outdated]
            exclude-folders = { replace = ["fixtures"] }

            [check]
            exclude-folders = { extend = ["snapshots"] }
        "#});
        assert_eq!(
            cfg.resolved("outdated").exclude_folders.patterns(),
            ["fixtures"]
        );
        assert_eq!(
            cfg.resolved("check").exclude_folders.patterns(),
            ["build", "snapshots"]
        );
        assert_eq!(
            cfg.resolved("upgrade").exclude_folders.patterns(),
            ["build"]
        );
    }

    /// Sections resolve within each file before the files fold, so a `[<command>]` replacement
    /// in a farther file (a user's global config) cannot void a `[global]` exclusion in a nearer
    /// one (the repository's), and a nearer file's `[global]` addition survives a farther file's
    /// `[<command>]` replacement.
    #[test]
    fn a_command_replacement_reaches_only_its_own_file_and_the_files_below() {
        let user = scan(indoc! {r"
            [outdated]
            exclude-folders = { replace = [] }
        "});
        let repo = scan(indoc! {r#"
            [global]
            exclude-folders = ["crates/app"]
        "#});
        let merged = user.clone().merge(repo.clone());
        assert_eq!(
            merged.resolved("outdated").exclude_folders.patterns(),
            ["crates/app"],
            "the repository's exclusion survives the user's farther replacement"
        );

        let nearer_addition = repo.merge(scan(indoc! {r#"
            [global]
            exclude-folders = ["fixtures"]
        "#}));
        let with_replacement = user.merge(nearer_addition);
        assert_eq!(
            with_replacement
                .resolved("outdated")
                .exclude_folders
                .patterns(),
            ["crates/app", "fixtures"]
        );

        // Within one file the replacement still shadows that file's own `[global]` list.
        let same_file = scan(indoc! {r#"
            [global]
            exclude-folders = ["build"]

            [outdated]
            exclude-folders = { replace = [] }
        "#});
        assert!(
            same_file
                .resolved("outdated")
                .exclude_folders
                .patterns()
                .is_empty()
        );
        // Scalars keep folding per key: a `[<command>]` value in any file beats `[global]` in
        // every file.
        let scalar =
            scan("[outdated]\ngitignore = false\n").merge(scan("[global]\ngitignore = true\n"));
        assert_eq!(scalar.resolved("outdated").gitignore, Some(false));
    }

    /// Two alias tables for one tool that both set the same exclude list have no author-visible
    /// order to fold in, so the pair is refused; tables that set different keys, or none, are
    /// unambiguous and stay accepted.
    #[test]
    fn tool_alias_tables_with_exclude_lists_are_rejected() {
        let err = parse_scan_config(
            indoc! {r#"
                [tool.rust]
                exclude-folders = ["fuzz"]

                [tool.cargo]
                exclude-folders = []
            "#},
            &Origin::Default,
        )
        .expect_err("two exclude-bearing tables for cargo must be rejected");
        assert!(
            err.to_string().contains("[tool.rust]") && err.to_string().contains("[tool.cargo]"),
            "the error names both tables: {err}"
        );
        let cfg = scan(indoc! {r#"
            [tool.rust]
            min-age = "3d"

            [tool.cargo]
            exclude-folders = ["bench"]
        "#});
        assert_eq!(cfg.exclude_folders_for(&[], "cargo"), vec!["bench"]);
        // Different keys in the two tables fold nothing against each other.
        let disjoint = scan(indoc! {r#"
            [tool.rust]
            exclude-folders = ["fuzz"]

            [tool.cargo]
            exclude-packages = ["bench-*"]
        "#});
        assert_eq!(disjoint.exclude_folders_for(&[], "cargo"), vec!["fuzz"]);
        assert_eq!(
            disjoint.tool_exclude_package_patterns().get("cargo"),
            Some(&vec!["bench-*".to_string()])
        );
    }

    #[test]
    fn malformed_exclude_merge_table_is_a_config_error() {
        let err = parse_scan_config(
            "[tool.cargo]\nexclude-folders = { replac = [\"a\"] }\n",
            &Origin::Default,
        )
        .expect_err("a misspelt merge key must be rejected");
        assert!(
            err.to_string().contains("replace"),
            "the error names the accepted keys: {err}"
        );
        assert!(
            parse_scan_config(
                "[global]\nexclude-folders = { replace = [\"a\"], extend = [\"b\"] }\n",
                &Origin::Default,
            )
            .is_err()
        );
        // The replacement's own patterns are still validated as globs.
        assert!(
            parse_scan_config(
                "[global]\nexclude-folders = { replace = [\"a/**/[\"] }\n",
                &Origin::Default,
            )
            .is_err()
        );
    }

    #[test]
    fn all_flags_resolve_with_command_over_global() {
        let cfg = scan(
            r"
[global]
strict = true
offline = true
concurrency = 4
jobs = 2

[upgrade]
strict = false
build = true
jobs = 1

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
            upgrade.jobs,
            std::num::NonZeroUsize::new(1),
            "command overrides global"
        );
        assert_eq!(
            cfg.resolved("check").jobs,
            std::num::NonZeroUsize::new(2),
            "inherited from global"
        );
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
            cfg.resolved("fix").exclude_folders.patterns(),
            ["dist", "fixtures"]
        );
        assert_eq!(cfg.resolved("upgrade").exclude_folders.patterns(), ["dist"]);
    }

    #[test]
    fn empty_config_is_inert() {
        let cfg = scan("min-age = \"7d\"\n");
        assert!(cfg.exclude_folders_for(&[], "cargo").is_empty());
        assert!(
            cfg.resolved("outdated")
                .exclude_folders
                .patterns()
                .is_empty()
        );
        assert!(
            cfg.resolved("outdated")
                .exclude_packages
                .patterns()
                .is_empty()
        );
        assert_eq!(cfg.resolved("outdated").gitignore, None);
        assert_eq!(cfg.resolved("outdated").major, None);
        assert_eq!(cfg.resolved("outdated").strict, None);
    }
}
