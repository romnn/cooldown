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
    /// Scan-exclude globs. Meaningful only under `[tool.<name>]` (added to the scan exclude list
    /// for that tool); ignored on registry/package/project selectors, which are policy-only.
    pub(crate) exclude: Option<Vec<String>>,
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
    /// Fail `upgrade`/`fix` if a mutation cannot complete cleanly (`--strict`).
    pub strict: Option<bool>,
    /// Compile/sync after re-locking in `upgrade` (`--build`).
    pub build: Option<bool>,
    /// Include transitive deps in `fix` (`--transitive`).
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
    /// Concurrency for the registry fan-out (no CLI flag; defaults to 8).
    pub concurrency: Option<usize>,
}

impl CommandConfig {
    /// Merge a higher-precedence config-file layer over `self`.
    ///
    /// List-valued fields concatenate so lower-precedence defaults are preserved, while scalar
    /// fields take the higher-precedence value when set.
    #[must_use]
    pub fn merge_layer(mut self, mut other: CommandConfig) -> CommandConfig {
        self.exclude.append(&mut other.exclude);
        self.tool.append(&mut other.tool);
        self.package.append(&mut other.package);
        self.gitignore = other.gitignore.or(self.gitignore);
        self.major = other.major.or(self.major);
        self.major_all = other.major_all.or(self.major_all);
        self.all = other.all.or(self.all);
        self.direct_only = other.direct_only.or(self.direct_only);
        self.include_indirect = other.include_indirect.or(self.include_indirect);
        self.all_artifacts = other.all_artifacts.or(self.all_artifacts);
        self.allow_stale_lock = other.allow_stale_lock.or(self.allow_stale_lock);
        self.fail_on_unknown_age = other.fail_on_unknown_age.or(self.fail_on_unknown_age);
        self.strict = other.strict.or(self.strict);
        self.build = other.build.or(self.build);
        self.transitive = other.transitive.or(self.transitive);
        self.downgrade_pinned = other.downgrade_pinned.or(self.downgrade_pinned);
        self.dry_run = other.dry_run.or(self.dry_run);
        self.offline = other.offline.or(self.offline);
        self.fresh = other.fresh.or(self.fresh);
        self.json = other.json.or(self.json);
        self.exit_code = other.exit_code.or(self.exit_code);
        self.concurrency = other.concurrency.or(self.concurrency);
        self
    }

    /// Apply explicit invocation overrides on top of `self`.
    ///
    /// Unlike config-file layering, explicit invocation lists replace lower-precedence defaults
    /// rather than concatenating with them.
    #[must_use]
    pub fn apply_explicit(mut self, explicit: &CommandConfig) -> CommandConfig {
        if !explicit.tool.is_empty() {
            self.tool.clone_from(&explicit.tool);
        }
        if !explicit.package.is_empty() {
            self.package.clone_from(&explicit.package);
        }
        self.gitignore = explicit.gitignore.or(self.gitignore);
        self.major = explicit.major.or(self.major);
        self.major_all = explicit.major_all.or(self.major_all);
        self.all = explicit.all.or(self.all);
        self.direct_only = explicit.direct_only.or(self.direct_only);
        self.include_indirect = explicit.include_indirect.or(self.include_indirect);
        self.all_artifacts = explicit.all_artifacts.or(self.all_artifacts);
        self.allow_stale_lock = explicit.allow_stale_lock.or(self.allow_stale_lock);
        self.fail_on_unknown_age = explicit.fail_on_unknown_age.or(self.fail_on_unknown_age);
        self.strict = explicit.strict.or(self.strict);
        self.build = explicit.build.or(self.build);
        self.transitive = explicit.transitive.or(self.transitive);
        self.downgrade_pinned = explicit.downgrade_pinned.or(self.downgrade_pinned);
        self.dry_run = explicit.dry_run.or(self.dry_run);
        self.offline = explicit.offline.or(self.offline);
        self.fresh = explicit.fresh.or(self.fresh);
        self.json = explicit.json.or(self.json);
        self.exit_code = explicit.exit_code.or(self.exit_code);
        self.concurrency = explicit.concurrency.or(self.concurrency);
        self
    }
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
    pub(crate) tool: Option<BTreeMap<String, SelectorToml>>,
    pub(crate) registry: Option<BTreeMap<String, SelectorToml>>,
    pub(crate) package: Option<BTreeMap<String, SelectorToml>>,
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
    }
}
