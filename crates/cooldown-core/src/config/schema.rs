use super::ExcludeList;
use std::collections::BTreeMap;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum MinAgeToml {
    Scalar(String),
    Table(MinAgeTable),
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MinAgeTable {
    pub(crate) default: Option<String>,
    pub(crate) major: Option<String>,
    pub(crate) minor: Option<String>,
    pub(crate) patch: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SelectorToml {
    #[serde(rename = "min-age")]
    pub(crate) min_age: Option<MinAgeToml>,
    pub(crate) latest: Option<bool>,
    pub(crate) freeze: Option<String>,
    pub(crate) floor: Option<String>,
    pub(crate) package: Option<BTreeMap<String, PackageRuleToml>>,
    /// `.gitignore`-style directories never scanned under `[tool.<name>]`.
    /// Present-but-empty clears the list inherited from a lower-precedence file; see
    /// [`ExcludeList`].
    #[serde(rename = "exclude-folders")]
    pub(crate) exclude_folders: Option<ExcludeList>,
    /// Package-name globs whose workspace members are dropped from reports under `[tool.<name>]`.
    /// Merges like [`exclude_folders`](Self::exclude_folders).
    #[serde(rename = "exclude-packages")]
    pub(crate) exclude_packages: Option<ExcludeList>,
    /// How `upgrade`/`fix` treat resolved lock edge bindings after the re-resolve.
    /// Cargo-specific: accepted only under `[tool.cargo]` and rejected under every other selector,
    /// so the tool-scoped placement is explicit rather than a global key that only one tool reads.
    #[serde(rename = "edge-policy")]
    pub(crate) edge_policy: Option<crate::EdgePolicy>,
    /// The packages a resolve must not add a second resolved copy of: `upgrade`/`fix` refuse a
    /// settlement that adds one instead of committing it with a warning.
    /// pnpm-specific, like `edge-policy` is cargo-specific: accepted only under `[tool.pnpm]`.
    /// Merges across config files like the exclude lists (a plain array adds to the inherited
    /// list, `[]` clears it, `{ replace = [...] }` replaces it), so a nested workspace cannot
    /// un-gate the root's runtimes by listing one name of its own.
    #[serde(rename = "single-copy")]
    pub(crate) single_copy: Option<ExcludeList>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackageRuleToml {
    #[serde(rename = "min-age")]
    pub(crate) min_age: Option<MinAgeToml>,
    pub(crate) latest: Option<bool>,
    pub(crate) freeze: Option<String>,
    pub(crate) floor: Option<String>,
    #[serde(rename = "max-major")]
    pub(crate) max_major: Option<u64>,
}

/// CLI-flag defaults from one config section: `[global]` (shared) or a `[<command>]` section.
///
/// Every field mirrors a CLI flag.
/// Resolution is uniform: an explicit CLI flag always wins, then a `[<command>]` value, then
/// `[global]`, then the built-in default.
/// `None`/empty means "unset", so a section only overrides what it names — except the exclude
/// lists, where an explicit `[]` clears what was inherited (see [`ExcludeList`]).
/// Keys are kebab-case (`all-artifacts`, `fail-on-unknown-age`, …), the same spelling as the
/// flags.
/// New config-driven flags are added here and nowhere else.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct CommandConfig {
    /// Directories never scanned, `.gitignore`-style.
    /// A plain array adds to the `[global]` list (and, across files, to the lower-precedence
    /// file's list); `[]` or `{ replace = [...] }` drops it.
    /// The per-tool `[tool.*]` list is combined on top at use time.
    /// See [`compile_folder_globset`].
    ///
    /// [`compile_folder_globset`]: crate::config::compile_folder_globset
    #[serde(default)]
    pub exclude_folders: ExcludeList,
    /// Workspace members dropped from reports when their package name matches one of these globs.
    /// Merges like [`exclude_folders`](Self::exclude_folders).
    /// See [`compile_package_globset`].
    ///
    /// [`compile_package_globset`]: crate::config::compile_package_globset
    #[serde(default)]
    pub exclude_packages: ExcludeList,
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
    /// Whether the npm-family `latest` dist-tag caps candidates
    /// (`--respect-dist-tags` / `--no-respect-dist-tags`; default on).
    pub respect_dist_tags: Option<bool>,
    /// List up-to-date deps in `outdated` (`--all`).
    pub all: Option<bool>,
    /// Gate every recorded artifact in `check` (`--all-artifacts`).
    pub all_artifacts: Option<bool>,
    /// Downgrade a stale/absent lock to a warning (`--allow-stale-lock`).
    pub allow_stale_lock: Option<bool>,
    /// Make `check` fail on deps with no publish time (`--fail-on-unknown-age`).
    pub fail_on_unknown_age: Option<bool>,
    /// Make `check` fail when the enabled advisory feed yields no usable evidence —
    /// unreachable, unimplemented, or too stale to shorten (`--fail-on-advisory-source`) — for
    /// a CI gate that refuses to certify without it.
    ///
    /// An uncovered ecosystem stays a warning.
    pub fail_on_advisory_source: Option<bool>,
    /// Make `upgrade`/`fix` refuse a resolve that gives any package a second resolved copy
    /// (`--fail-on-new-duplicate`), without naming the packages in `[tool.pnpm] single-copy`.
    pub fail_on_new_duplicate: Option<bool>,
    /// Fail `upgrade`/`fix` if a mutation cannot complete cleanly (`--strict`).
    pub strict: Option<bool>,
    /// Compile/sync after re-locking in `upgrade` (`--build`).
    pub build: Option<bool>,
    /// Include transitive deps in `outdated`/`fix` (`--transitive`).
    pub transitive: Option<bool>,
    /// Allow `fix` to downgrade exact-pinned deps too (`--downgrade-pinned`).
    pub downgrade_pinned: Option<bool>,
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
    /// Concurrency for the registry fan-out — also the per-host in-flight cap (`--concurrency`,
    /// `COOLDOWN_CONCURRENCY`; defaults to 16).
    pub concurrency: Option<usize>,
    /// How many ecosystems run at once (`--jobs`, `COOLDOWN_JOBS`; every detected tool by
    /// default, `1` runs them one after another).
    /// Non-zero like the flag, so `0` is rejected here as well rather than read as no cap.
    pub jobs: Option<std::num::NonZeroUsize>,
}

impl CommandConfig {
    /// Merge a higher-precedence config-file layer over `self`.
    ///
    /// List-valued fields concatenate so lower-precedence defaults are preserved (the exclude
    /// lists honor their own [`ExcludeList::merge`] mode, so a replacing layer wins), while scalar
    /// fields take the higher-precedence value when set.
    #[must_use]
    pub fn merge_layer(mut self, other: CommandConfig) -> CommandConfig {
        let CommandConfig {
            exclude_folders,
            exclude_packages,
            mut tool,
            mut package,
            gitignore,
            major,
            respect_dist_tags,
            all,
            all_artifacts,
            allow_stale_lock,
            fail_on_unknown_age,
            fail_on_advisory_source,
            fail_on_new_duplicate,
            strict,
            build,
            transitive,
            downgrade_pinned,
            dry_run,
            offline,
            fresh,
            json,
            exit_code,
            concurrency,
            jobs,
        } = other;

        self.exclude_folders = self.exclude_folders.merge(exclude_folders);
        self.exclude_packages = self.exclude_packages.merge(exclude_packages);
        self.tool.append(&mut tool);
        self.package.append(&mut package);
        self.gitignore = gitignore.or(self.gitignore);
        self.major = major.or(self.major);
        self.respect_dist_tags = respect_dist_tags.or(self.respect_dist_tags);
        self.all = all.or(self.all);
        self.all_artifacts = all_artifacts.or(self.all_artifacts);
        self.allow_stale_lock = allow_stale_lock.or(self.allow_stale_lock);
        self.fail_on_unknown_age = fail_on_unknown_age.or(self.fail_on_unknown_age);
        self.fail_on_advisory_source = fail_on_advisory_source.or(self.fail_on_advisory_source);
        self.fail_on_new_duplicate = fail_on_new_duplicate.or(self.fail_on_new_duplicate);
        self.strict = strict.or(self.strict);
        self.build = build.or(self.build);
        self.transitive = transitive.or(self.transitive);
        self.downgrade_pinned = downgrade_pinned.or(self.downgrade_pinned);
        self.dry_run = dry_run.or(self.dry_run);
        self.offline = offline.or(self.offline);
        self.fresh = fresh.or(self.fresh);
        self.json = json.or(self.json);
        self.exit_code = exit_code.or(self.exit_code);
        self.concurrency = concurrency.or(self.concurrency);
        self.jobs = jobs.or(self.jobs);
        self
    }

    /// Apply explicit invocation overrides on top of `self`.
    ///
    /// Unlike config-file layering, explicit invocation lists replace lower-precedence defaults
    /// rather than concatenating with them.
    #[must_use]
    pub fn apply_explicit(mut self, explicit: &CommandConfig) -> CommandConfig {
        let CommandConfig {
            // CLI `--exclude-folders`/`--exclude-packages` flow through `override_excludes` on the
            // resolved config — project detection reads them from there before RunOpts exists — and
            // explicit layers never carry them, so they are deliberately not merged here.
            exclude_folders: _,
            exclude_packages: _,
            tool,
            package,
            gitignore,
            major,
            respect_dist_tags,
            all,
            all_artifacts,
            allow_stale_lock,
            fail_on_unknown_age,
            fail_on_advisory_source,
            fail_on_new_duplicate,
            strict,
            build,
            transitive,
            downgrade_pinned,
            dry_run,
            offline,
            fresh,
            json,
            exit_code,
            concurrency,
            jobs,
        } = explicit;

        if !tool.is_empty() {
            self.tool.clone_from(tool);
        }
        if !package.is_empty() {
            self.package.clone_from(package);
        }
        self.gitignore = (*gitignore).or(self.gitignore);
        self.major = (*major).or(self.major);
        self.respect_dist_tags = (*respect_dist_tags).or(self.respect_dist_tags);
        self.all = (*all).or(self.all);
        self.all_artifacts = (*all_artifacts).or(self.all_artifacts);
        self.allow_stale_lock = (*allow_stale_lock).or(self.allow_stale_lock);
        self.fail_on_unknown_age = (*fail_on_unknown_age).or(self.fail_on_unknown_age);
        self.fail_on_advisory_source = (*fail_on_advisory_source).or(self.fail_on_advisory_source);
        self.fail_on_new_duplicate = (*fail_on_new_duplicate).or(self.fail_on_new_duplicate);
        self.strict = (*strict).or(self.strict);
        self.build = (*build).or(self.build);
        self.transitive = (*transitive).or(self.transitive);
        self.downgrade_pinned = (*downgrade_pinned).or(self.downgrade_pinned);
        self.dry_run = (*dry_run).or(self.dry_run);
        self.offline = (*offline).or(self.offline);
        self.fresh = (*fresh).or(self.fresh);
        self.json = (*json).or(self.json);
        self.exit_code = (*exit_code).or(self.exit_code);
        self.concurrency = (*concurrency).or(self.concurrency);
        self.jobs = (*jobs).or(self.jobs);
        self
    }

    /// Replace the folder/package excludes with CLI-provided lists (`--exclude-folders` /
    /// `--exclude-packages`) — the highest-precedence layer. Each list is a no-op when empty (flag
    /// not given); a non-empty list replaces this resolved value and is validated up front so a bad
    /// CLI glob fails fast, like the config ones. Per-tool `[tool.*]` excludes are carried separately
    /// (on [`ScanConfig`](super::ScanConfig)) and are unaffected.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`](crate::CoreError) if a pattern is not a valid glob.
    pub fn override_excludes(
        &mut self,
        folders: &[String],
        packages: &[String],
    ) -> Result<(), crate::CoreError> {
        if !folders.is_empty() {
            super::compile_folder_globset(folders)?;
            self.exclude_folders = ExcludeList::replace(folders.to_vec());
        }
        if !packages.is_empty() {
            super::compile_package_globset(packages)?;
            self.exclude_packages = ExcludeList::replace(packages.to_vec());
        }
        Ok(())
    }
}

/// The `[advisories]` table: the security-relevant signal (OSV feed) and what it may do.
///
/// Kept as raw strings here; [`policy_layer_from_config`](super::layers) parses and validates
/// them into a typed [`AdvisoryPolicy`](crate::advisory::AdvisoryPolicy) so a bad token fails
/// fast with the selector context in the message.
///
/// [`policy_layer_from_config`]: super::layers
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct AdvisoriesToml {
    /// Whether to consult the feed at all (off by default: a new network dependency).
    pub(crate) enabled: Option<bool>,
    /// `"osv"` (default) | `"github"` | `"none"`.
    pub(crate) source: Option<String>,
    /// `"flag"` (annotate only, default) | `"shorten"` (also apply `min-age` below).
    pub(crate) mode: Option<String>,
    /// The SECURITY window — used only under `mode = "shorten"`.
    pub(crate) min_age: Option<String>,
    /// The minimum normalized severity that earns it: `low|moderate|high|critical`.
    pub(crate) severity: Option<String>,
    /// Whether the security window may undercut a `floor` (honored only at the floor's layer).
    pub(crate) bypass_floor: Option<bool>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigToml {
    #[serde(rename = "min-age")]
    pub(crate) min_age: Option<MinAgeToml>,
    pub(crate) latest: Option<bool>,
    pub(crate) freeze: Option<String>,
    pub(crate) floor: Option<String>,
    pub(crate) allow: Option<Vec<String>>,
    #[serde(rename = "strict-native")]
    pub(crate) strict_native: Option<bool>,
    /// The advisory feed (security-relevant signal); see [`AdvisoriesToml`].
    pub(crate) advisories: Option<AdvisoriesToml>,
    pub(crate) tool: Option<BTreeMap<String, SelectorToml>>,
    pub(crate) registry: Option<BTreeMap<String, SelectorToml>>,
    pub(crate) package: Option<BTreeMap<String, PackageRuleToml>>,
    pub(crate) project: Option<BTreeMap<String, SelectorToml>>,
    /// Shared CLI-flag defaults across all subcommands.
    pub(crate) global: Option<CommandConfig>,
    /// Per-subcommand CLI-flag defaults; each overrides `[global]`.
    pub(crate) outdated: Option<CommandConfig>,
    pub(crate) upgrade: Option<CommandConfig>,
    pub(crate) fix: Option<CommandConfig>,
    pub(crate) check: Option<CommandConfig>,
    pub(crate) baseline: Option<CommandConfig>,
}

/// Policy fields gathered from env vars or CLI flags (the same shape for both).
///
/// Strings are kept unparsed here; [`layer_from_fields`](super::layer_from_fields) parses them when
/// it builds the [`PolicyLayer`](crate::PolicyLayer), so an invalid duration or glob surfaces as a
/// [`CoreError::Config`](crate::CoreError::Config) at that point rather than at collection time.
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
    /// `--advisories`/`--no-advisories` / `COOLDOWN_ADVISORIES`: consult the advisory feed.
    pub advisories: Option<bool>,
    /// `--advisory-min-age` / `COOLDOWN_ADVISORY_MIN_AGE`: the security window.
    ///
    /// Setting it also selects the shorten mode at this layer — declaring a security window on
    /// the command line means you want it applied.
    pub advisory_min_age: Option<String>,
    /// `--advisory-severity` / `COOLDOWN_ADVISORY_SEVERITY`: the minimum severity that earns
    /// the security window.
    pub advisory_severity: Option<String>,
}

impl WindowFields {
    pub(crate) fn is_empty(&self) -> bool {
        self.min_age.is_none()
            && self.min_age_major.is_none()
            && self.min_age_minor.is_none()
            && self.min_age_patch.is_none()
            && !self.latest
            && self.freeze.is_none()
            && self.allow.is_empty()
            && self.advisories.is_none()
            && self.advisory_min_age.is_none()
            && self.advisory_severity.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::{CommandConfig, ExcludeList};

    #[test]
    fn override_excludes_replaces_non_empty_validates_and_noops_on_empty() {
        let seed = CommandConfig {
            exclude_folders: ExcludeList::extend(vec!["build".to_string()]),
            exclude_packages: ExcludeList::extend(vec!["internal-*".to_string()]),
            ..CommandConfig::default()
        };

        // Non-empty lists replace the resolved value (the highest-precedence CLI layer).
        let mut replaced = seed.clone();
        replaced
            .override_excludes(&["dist".to_string()], &["@scope/*".to_string()])
            .expect("valid override");
        assert_eq!(replaced.exclude_folders.patterns(), ["dist"]);
        assert_eq!(replaced.exclude_packages.patterns(), ["@scope/*"]);

        // An empty list is a no-op (flag not given), leaving the config value intact; the two sides
        // are independent.
        let mut folders_only = seed.clone();
        folders_only
            .override_excludes(&["dist".to_string()], &[])
            .expect("valid override");
        assert_eq!(folders_only.exclude_folders.patterns(), ["dist"]);
        assert_eq!(folders_only.exclude_packages.patterns(), ["internal-*"]);

        // Bad CLI globs fail fast, like the config ones.
        let mut bad_folder = CommandConfig::default();
        assert!(
            bad_folder
                .override_excludes(&["a/**/[".to_string()], &[])
                .is_err()
        );
        let mut bad_package = CommandConfig::default();
        assert!(
            bad_package
                .override_excludes(&[], &["[".to_string()])
                .is_err()
        );
    }
}
