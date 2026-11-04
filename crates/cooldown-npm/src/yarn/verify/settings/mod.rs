//! The Yarn settings a verification copy cannot reproduce, or under which Yarn could reach the
//! network or write outside the copy.
//!
//! Screening reads every rc file Yarn reads for the project and every `YARN_*` variable, and
//! declines before anything runs:
//!
//! - a configured plugin, whose hooks may read project files the copy leaves out;
//! - injected environment files other than the default `.env.yarn`, which the copy stages;
//! - a lockfile named other than `yarn.lock`;
//! - per-host network settings, which take precedence over the switch that keeps Yarn offline, or
//!   a global network switch that is not provably off;
//! - hardened mode set anywhere other than provably off, so the run only overrides Yarn 4's
//!   automatic default on a public pull request;
//! - a `packageExtensions` entry declaring an `exec:` dependency, whose generator may read files
//!   the copy leaves out;
//! - an output folder no override covers (`pnpmStoreFolder`, `pnpDataPath`, `bstatePath`) set
//!   anywhere but as a path inside a copied rc file, where it resolves into the copy.
//!
//! A value Yarn would interpolate (`${…}`) is never provably anything, so it declines too.
//!
//! Screening reads each rc file as written, which is what Yarn reads only while no value in it
//! is a conflict marker: an object with an `onConflict` key, which Yarn unwraps to its `value`
//! (or to the rest of the object) at any depth before it merges the files
//! ([configUtils.ts][config]).
//! An rc file using that form anywhere therefore declines, so every file screening admits is
//! the document Yarn merges.
//! Screening each file on its own is conservative across the merge, since Yarn's merged value at
//! any path is built only from the files' values at that path.
//!
//! [config]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/configUtils.ts

#[cfg(test)]
mod tests;

use crate::manifest::workspaces::{YarnEnvironment, configures_plugins};
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};

/// The key that makes an rc object a conflict marker, which Yarn unwraps before reading it.
const CONFLICT_KEY: &str = "onConflict";

/// The lockfile name verification supports, Yarn's default.
const LOCKFILE: &str = "yarn.lock";

/// The default of `injectEnvironmentFiles`: `.env.yarn` in the project, if it exists.
const DEFAULT_ENVIRONMENT_FILE: &str = ".env.yarn?";

/// The output folders Yarn writes that no override valid on every supported Yarn redirects, as
/// rc keys with their `YARN_*` variables.
const UNREDIRECTED_OUTPUTS: [Setting; 3] = [
    Setting {
        key: "pnpmStoreFolder",
        variable: "yarn_pnpm_store_folder",
    },
    Setting {
        key: "pnpDataPath",
        variable: "yarn_pnp_data_path",
    },
    Setting {
        key: "bstatePath",
        variable: "yarn_bstate_path",
    },
];

/// A setting's rc key and the lower-cased `YARN_*` variable that sets it.
struct Setting {
    key: &'static str,
    variable: &'static str,
}

/// One rc file Yarn reads for the project, with its top-level settings.
#[derive(Debug, Clone)]
pub(super) struct RcFile {
    /// Where the source holds it.
    pub(super) path: Utf8PathBuf,
    /// Whether the copy holds a copy of it, so a relative path in it resolves inside the copy.
    pub(super) staged: bool,
    pub(super) settings: Map<String, Value>,
}

/// Whether a descriptor range is or wraps an `exec:` range, compared without case and through
/// one level of URL encoding, as a `patch:` range nests its source.
pub(super) fn is_exec(range: &str) -> bool {
    let range = range.to_ascii_lowercase();
    range.contains("exec:") || range.contains("exec%3a")
}

/// Why Yarn's configuration for the project cannot be verified in a copy, or `Ok` when it can.
///
/// `shown` spells an rc file's path for the reason.
pub(super) fn screen(
    rc_files: &[RcFile],
    environment: &YarnEnvironment,
    shown: impl Fn(&Utf8Path) -> String,
) -> Result<(), String> {
    screen_environment(environment)?;
    for rc_file in rc_files {
        screen_rc_file(rc_file, &shown(&rc_file.path))?;
    }
    Ok(())
}

fn screen_environment(environment: &YarnEnvironment) -> Result<(), String> {
    if let Some(plugins) = environment
        .plugins
        .iter()
        .find(|plugins| !plugins.is_empty())
    {
        return Err(format!(
            "YARN_PLUGINS loads the Yarn plugins `{plugins}`, whose hooks may read files the \
             verification copy leaves out"
        ));
    }
    if environment
        .variable("yarn_inject_environment_files")
        .next()
        .is_some()
    {
        return Err(
            "YARN_INJECT_ENVIRONMENT_FILES names environment files the verification copy does \
             not reproduce"
                .to_string(),
        );
    }
    if let Some(lockfile) = environment
        .variable("yarn_lockfile_filename")
        .find(|lockfile| *lockfile != LOCKFILE)
    {
        return Err(format!(
            "YARN_LOCKFILE_FILENAME names the lockfile `{lockfile}`, and cooldown verifies only \
             `{LOCKFILE}`"
        ));
    }
    if let Some(hardened) = environment
        .variable("yarn_enable_hardened_mode")
        .find(|hardened| !provably_false(&Value::String((*hardened).to_string())))
    {
        return Err(hardened_mode("YARN_ENABLE_HARDENED_MODE", hardened));
    }
    if environment
        .variables
        .iter()
        .any(|variable| variable.name.starts_with("yarn_network_settings"))
    {
        return Err(
            "a YARN_NETWORK_SETTINGS variable sets per-host network settings, which may reach \
             the network past the switch that keeps verification offline"
                .to_string(),
        );
    }
    for output in &UNREDIRECTED_OUTPUTS {
        if environment.variable(output.variable).next().is_some() {
            return Err(format!(
                "{} sets `{}`, a folder Yarn writes that the verification copy cannot redirect",
                output.variable.to_ascii_uppercase(),
                output.key
            ));
        }
    }
    Ok(())
}

fn screen_rc_file(rc_file: &RcFile, shown: &str) -> Result<(), String> {
    let settings = &rc_file.settings;
    if settings.contains_key(CONFLICT_KEY) || settings.values().any(holds_conflict_marker) {
        return Err(format!(
            "{shown} uses Yarn's `{CONFLICT_KEY}` form, whose unwrapped settings cooldown does \
             not screen"
        ));
    }
    if settings.get("plugins").is_some_and(configures_plugins) {
        return Err(format!(
            "{shown} configures Yarn plugins, whose hooks may read files the verification copy \
             leaves out"
        ));
    }
    if let Some(files) = settings.get("injectEnvironmentFiles")
        && !injects_only_the_default(files)
    {
        return Err(format!(
            "{shown} sets `injectEnvironmentFiles` to `{}`, which the verification copy does not \
             reproduce",
            spelled(files)
        ));
    }
    if let Some(lockfile) = settings.get("lockfileFilename")
        && lockfile.as_str() != Some(LOCKFILE)
    {
        return Err(format!(
            "{shown} names the lockfile `{}`, and cooldown verifies only `{LOCKFILE}`",
            spelled(lockfile)
        ));
    }
    if settings.contains_key("networkSettings") {
        return Err(format!(
            "{shown} sets `networkSettings`, whose per-host settings may reach the network past \
             the switch that keeps verification offline"
        ));
    }
    if let Some(network) = settings.get("enableNetwork")
        && !provably_false(network)
    {
        return Err(format!(
            "{shown} sets `enableNetwork` to `{}`, and verification runs only where the network \
             is off or left at its default",
            spelled(network)
        ));
    }
    if let Some(hardened) = settings.get("enableHardenedMode")
        && !provably_false(hardened)
    {
        return Err(hardened_mode(
            &format!("{shown} sets `enableHardenedMode`, which"),
            &spelled(hardened),
        ));
    }
    if settings.get("packageExtensions").is_some_and(declares_exec) {
        return Err(format!(
            "{shown} extends a package with an `exec:` dependency, whose generator may read \
             files the verification copy leaves out"
        ));
    }
    for output in &UNREDIRECTED_OUTPUTS {
        let Some(value) = settings.get(output.key) else {
            continue;
        };
        let inside_the_copy = rc_file.staged && value.as_str().is_some_and(stays_beside);
        if !inside_the_copy {
            return Err(format!(
                "{shown} sets `{}` to `{}`, a folder Yarn would write outside the verification \
                 copy",
                output.key,
                spelled(value)
            ));
        }
    }
    Ok(())
}

/// The reason for a hardened mode that is not provably off, `source` naming where it is set.
fn hardened_mode(source: &str, value: &str) -> String {
    format!(
        "{source} is `{value}`: hardened mode checks the lock against the registry, which \
         verification never reaches"
    )
}

/// Whether an object anywhere in `value`, inside arrays too, has an `onConflict` key.
///
/// Yarn treats only an object whose `onConflict` is a string as a marker, and only outside
/// arrays, so declining every such key is a superset.
fn holds_conflict_marker(value: &Value) -> bool {
    match value {
        Value::Object(entries) => {
            entries.contains_key(CONFLICT_KEY) || entries.values().any(holds_conflict_marker)
        }
        Value::Array(items) => items.iter().any(holds_conflict_marker),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

/// Whether Yarn reads `value` as the boolean `false`.
///
/// Yarn takes a YAML boolean as it is and parses a string as `true`, `1`, `false`, or `0`.
/// Anything else, an empty value or a `${…}` interpolation included, is not provably false.
fn provably_false(value: &Value) -> bool {
    match value {
        Value::Bool(enabled) => !enabled,
        Value::Number(number) => number.as_u64() == Some(0),
        Value::String(text) => matches!(text.as_str(), "false" | "0"),
        Value::Null | Value::Array(_) | Value::Object(_) => false,
    }
}

/// Whether an `injectEnvironmentFiles` value is Yarn's default, as a list or a lone entry.
fn injects_only_the_default(files: &Value) -> bool {
    match files {
        Value::String(file) => file == DEFAULT_ENVIRONMENT_FILE,
        Value::Array(files) => {
            matches!(files.as_slice(), [Value::String(file)] if file == DEFAULT_ENVIRONMENT_FILE)
        }
        _ => false,
    }
}

/// Whether any string anywhere in a `packageExtensions` value is an `exec:` range.
fn declares_exec(value: &Value) -> bool {
    match value {
        Value::String(range) => is_exec(range),
        Value::Array(values) => values.iter().any(declares_exec),
        Value::Object(entries) => entries.values().any(declares_exec),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

/// Whether a path setting resolves below the directory of the rc file that sets it: relative,
/// without a parent step, and not interpolated.
fn stays_beside(path: &str) -> bool {
    !path.contains("${")
        && !path.is_empty()
        && Utf8Path::new(path)
            .components()
            .all(|component| matches!(component, Utf8Component::Normal(_) | Utf8Component::CurDir))
}

/// A setting's value as a reason quotes it: a string as written, anything else as JSON.
fn spelled(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
