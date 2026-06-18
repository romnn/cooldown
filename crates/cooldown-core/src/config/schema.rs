use std::collections::BTreeMap;

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum MinAgeToml {
    Scalar(String),
    Table(MinAgeTable),
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MinAgeTable {
    pub(crate) default: Option<String>,
    pub(crate) major: Option<String>,
    pub(crate) minor: Option<String>,
    pub(crate) patch: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
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
