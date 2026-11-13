//! Cargo's native publish-age settings, read the way cargo itself reads them.
//!
//! Cargo 1.100 stabilized `registries.<name>.min-publish-age`, `registry.global-min-publish-age`
//! and `resolver.incompatible-publish-age`.
//! cooldown imports the age cargo would apply to crates.io as a native policy layer, and keeps it
//! out of the cargo commands it runs itself, because a hard resolver cutoff breaks the stepwise
//! executor (a too-new version that leaves the lock is no longer exempt and cannot come back).
//!
//! Only the crates.io age matters: cooldown classifies crates.io releases alone, so the global age
//! keeps guarding every other registry untouched.
//! `include`d config files are not followed; staging refuses them before any mutation, so the
//! only effect is a read-only import that misses an age set in an included file.

mod sync;

pub(crate) use sync::write_repo_native;

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::CoreError;
use jiff::SignedDuration;

/// The process environment variable cargo reads for `registry.global-min-publish-age`.
const GLOBAL_AGE_ENV: &str = "CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE";

/// The process environment variable cargo reads for `resolver.incompatible-publish-age`.
const INCOMPATIBLE_ENV: &str = "CARGO_RESOLVER_INCOMPATIBLE_PUBLISH_AGE";

/// The config key that sets crates.io's own publish age, which beats the global one.
pub(crate) const CRATES_IO_AGE_KEY: [&str; 3] = ["registries", "crates-io", "min-publish-age"];

const GLOBAL_AGE_KEY: [&str; 2] = ["registry", "global-min-publish-age"];
const INCOMPATIBLE_KEY: [&str; 2] = ["resolver", "incompatible-publish-age"];

/// The cargo-relevant parts of the process environment, captured so tests can supply their own.
#[derive(Debug, Clone, Default)]
pub(crate) struct CargoEnvironment {
    /// `CARGO_HOME`, or the platform default home's `.cargo` when it is unset.
    pub(crate) cargo_home: Option<Utf8PathBuf>,
    /// The raw `CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE`.
    pub(crate) global_age: Option<String>,
    /// The raw `CARGO_RESOLVER_INCOMPATIBLE_PUBLISH_AGE`.
    pub(crate) incompatible: Option<String>,
}

impl CargoEnvironment {
    /// Captures the environment of the running process.
    pub(crate) fn current() -> Self {
        // Cargo treats an empty `CARGO_HOME` as unset.
        let cargo_home = std::env::var_os("CARGO_HOME")
            .filter(|home| !home.is_empty())
            .map(std::path::PathBuf::from)
            // Cargo's default home follows the platform home directory, which on Windows is the
            // user profile rather than `HOME`.
            .or_else(|| std::env::home_dir().map(|home| home.join(".cargo")))
            .and_then(|path| Utf8PathBuf::from_path_buf(path).ok());
        Self {
            cargo_home,
            global_age: std::env::var(GLOBAL_AGE_ENV).ok(),
            incompatible: std::env::var(INCOMPATIBLE_ENV).ok(),
        }
    }
}

/// Where an effective publish age was configured, for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgeSource {
    /// A cargo config file.
    File(Utf8PathBuf),
    /// The `CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE` environment variable.
    Env,
}

impl std::fmt::Display for AgeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgeSource::File(path) => write!(f, "{path}"),
            AgeSource::Env => f.write_str(GLOBAL_AGE_ENV),
        }
    }
}

/// A raw setting together with where it came from.
#[derive(Debug, Clone)]
struct Setting {
    value: String,
    source: AgeSource,
}

/// The publish-age settings cargo would apply to crates.io in one directory.
#[derive(Debug, Clone, Default)]
pub(crate) struct PublishAgeSettings {
    crates_io_age: Option<Setting>,
    global_age: Option<Setting>,
    incompatible: Option<Setting>,
}

/// The publish age cargo enforces on crates.io, with the setting that supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectiveAge {
    pub(crate) age: SignedDuration,
    pub(crate) source: AgeSource,
}

impl PublishAgeSettings {
    /// Reads the settings cargo would apply to a command run in `dir`.
    ///
    /// Cargo merges the active config file of every ancestor's `.cargo/` directory, nearest first,
    /// then the Cargo home's, which always ranks last even when it is also an ancestor.
    /// A directory's active file is `config` when that exists, and `config.toml` otherwise.
    /// The environment then overrides the file value of the same key.
    pub(crate) fn discover(dir: &Utf8Path, env: &CargoEnvironment) -> Result<Self, CoreError> {
        // A relative `CARGO_HOME` resolves against the directory cargo runs in.
        let home = env.cargo_home.as_ref().map(|home| dir.join(home));
        let mut config_dirs: Vec<Utf8PathBuf> = dir
            .ancestors()
            .map(|ancestor| ancestor.join(".cargo"))
            .filter(|config_dir| Some(config_dir) != home.as_ref())
            .collect();
        config_dirs.extend(home);

        let mut settings = Self::default();
        for config_dir in &config_dirs {
            let Some(path) = active_config_file(config_dir) else {
                continue;
            };
            let content = std::fs::read_to_string(&path)
                .map_err(|err| CoreError::Filesystem(format!("{path}: {err}")))?;
            let value: toml::Value = toml::from_str(&content)
                .map_err(|err| CoreError::Config(format!("{path}: invalid TOML: {err}")))?;
            // Nearer files were read first, so a key already set keeps its value.
            fill(
                &mut settings.crates_io_age,
                &value,
                &CRATES_IO_AGE_KEY,
                &path,
            )?;
            fill(&mut settings.global_age, &value, &GLOBAL_AGE_KEY, &path)?;
            fill(&mut settings.incompatible, &value, &INCOMPATIBLE_KEY, &path)?;
        }
        if let Some(value) = &env.global_age {
            settings.global_age = Some(Setting {
                value: value.clone(),
                source: AgeSource::Env,
            });
        }
        if let Some(value) = &env.incompatible {
            settings.incompatible = Some(Setting {
                value: value.clone(),
                source: AgeSource::Env,
            });
        }
        Ok(settings)
    }

    /// The age cargo applies when resolving crates.io packages, or `None` when none is configured.
    ///
    /// The crates.io key beats the global one whichever sources they come from.
    /// An explicit zero is kept, since it is a configured opt-out rather than an absent setting.
    /// `resolver.incompatible-publish-age = "allow"` turns enforcement off, so no age applies.
    pub(crate) fn effective_crates_io_age(&self) -> Result<Option<EffectiveAge>, CoreError> {
        if self.enforcement_disabled()? {
            return Ok(None);
        }
        let Some(setting) = self.crates_io_age.as_ref().or(self.global_age.as_ref()) else {
            return Ok(None);
        };
        let age = parse_age(&setting.value).map_err(|reason| {
            CoreError::Config(format!(
                "{}: invalid cargo min-publish-age {:?}: {reason}",
                setting.source, setting.value
            ))
        })?;
        Ok(Some(EffectiveAge {
            age,
            source: setting.source.clone(),
        }))
    }

    /// Whether `resolver.incompatible-publish-age = "allow"` keeps cargo from enforcing any age.
    pub(crate) fn enforcement_disabled(&self) -> Result<bool, CoreError> {
        let Some(setting) = &self.incompatible else {
            return Ok(false);
        };
        match setting.value.as_str() {
            "allow" => Ok(true),
            "deny" => Ok(false),
            other => Err(CoreError::Config(format!(
                "{}: invalid cargo resolver.incompatible-publish-age {other:?}: expected `allow` or `deny`",
                setting.source
            ))),
        }
    }
}

/// The file cargo reads in `config_dir`: `config` wins over `config.toml` when both exist.
pub(crate) fn active_config_file(config_dir: &Utf8Path) -> Option<Utf8PathBuf> {
    ["config", "config.toml"]
        .into_iter()
        .map(|name| config_dir.join(name))
        .find(|path| path.is_file())
}

/// The file `sync` writes in `config_dir`: the active one, or a new `config.toml`.
pub(crate) fn writable_config_file(config_dir: &Utf8Path) -> Utf8PathBuf {
    active_config_file(config_dir).unwrap_or_else(|| config_dir.join("config.toml"))
}

fn fill(
    slot: &mut Option<Setting>,
    value: &toml::Value,
    keys: &[&str],
    path: &Utf8Path,
) -> Result<(), CoreError> {
    if slot.is_some() {
        return Ok(());
    }
    let found = keys.iter().try_fold(value, |table, key| table.get(key));
    let Some(found) = found else {
        return Ok(());
    };
    let raw = found.as_str().ok_or_else(|| {
        CoreError::Config(format!("{path}: `{}` must be a string", keys.join(".")))
    })?;
    *slot = Some(Setting {
        value: raw.to_owned(),
        source: AgeSource::File(path.to_owned()),
    });
    Ok(())
}

const SECONDS_PER_UNIT: [(&str, i64); 6] = [
    ("second", 1),
    ("minute", 60),
    ("hour", 60 * 60),
    ("day", 24 * 60 * 60),
    ("week", 7 * 24 * 60 * 60),
    // Cargo counts a month as 30 days.
    ("month", 30 * 24 * 60 * 60),
];

/// Parses an age in cargo's grammar: `N seconds/minutes/hours/days/weeks/months`.
///
/// The space is optional, the unit may be singular or plural, and a bare `0` means no age.
/// Cargo rejects any other spelling, such as `14d` or an ISO-8601 span, so this does too.
pub(crate) fn parse_age(raw: &str) -> Result<SignedDuration, String> {
    const EXPECTED: &str = "expected \"N seconds/minutes/hours/days/weeks/months\"";
    let trimmed = raw.trim();
    let split = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let count: i64 = number.parse().map_err(|_| EXPECTED.to_owned())?;
    let unit = unit.trim();
    if unit.is_empty() {
        return if count == 0 {
            Ok(SignedDuration::ZERO)
        } else {
            Err(EXPECTED.to_owned())
        };
    }
    let singular = unit.strip_suffix('s').unwrap_or(unit);
    let (_, seconds) = SECONDS_PER_UNIT
        .iter()
        .find(|(name, _)| *name == singular)
        .ok_or_else(|| EXPECTED.to_owned())?;
    count
        .checked_mul(*seconds)
        .map(SignedDuration::from_secs)
        .ok_or_else(|| "age out of range".to_owned())
}

/// Spells `age` in cargo's grammar using the largest unit that divides it exactly.
///
/// Months are never used, because cargo's 30-day month would silently round a calendar reading.
/// Returns `None` for a negative or sub-second age, which cargo cannot express.
pub(crate) fn render_age(age: SignedDuration) -> Option<String> {
    if age.is_negative() || age.subsec_nanos() != 0 {
        return None;
    }
    let seconds = age.as_secs();
    let (name, per_unit) = SECONDS_PER_UNIT
        .iter()
        .filter(|(name, _)| *name != "month")
        .rev()
        .find(|(_, per_unit)| seconds % per_unit == 0)?;
    let count = seconds / per_unit;
    let plural = if count == 1 { "" } else { "s" };
    Some(format!("{count} {name}{plural}"))
}

#[cfg(test)]
mod tests;
