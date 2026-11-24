//! `sync` for Cargo: the repo-wide default window written as crates.io's publish age in the repo
//! root's cargo config, so a bare `cargo update` honors the cooldown too.
//!
//! Cargo reads config files hierarchically, so one file at the repo root reaches every Cargo
//! project below it that does not set its own age.
//! Cargo has a single age per registry, with no per-package exemption or window and no absolute
//! cutoff, so whatever else the policy says is reported rather than written.
//! The application reports the rules the age leaves out for every tool alike; this module reports
//! only a default window it cannot write and settings that keep cargo from enforcing the age.

use super::{
    CRATES_IO_AGE_ENV, CRATES_IO_AGE_KEY, CargoEnvironment, PublishAgeSettings, parse_age,
    render_age, writable_config_file,
};
use camino::Utf8Path;
use cooldown_core::{
    CoreError, Diagnostic, DiagnosticKind, ResolvedPolicy, SyncReport, WindowSpec,
};

/// Writes `policy`'s default window into the repo root's active cargo config file.
///
/// The crates.io key is left as written when it already holds the same age, however it is spelled.
pub(crate) fn write_repo_native(
    repo_root: &Utf8Path,
    policy: &ResolvedPolicy,
    env: &CargoEnvironment,
    dry_run: bool,
) -> Result<SyncReport, CoreError> {
    let config_dir = repo_root.join(".cargo");
    let path = writable_config_file(&config_dir);
    let mut warnings = Vec::new();

    if PublishAgeSettings::discover(repo_root, env)?.enforcement_disabled()? {
        warnings.push(Diagnostic::new(
            DiagnosticKind::Config,
            "cargo's resolver.incompatible-publish-age is `allow`, so a bare cargo command does \
             not enforce the synced publish age"
                .to_owned(),
        ));
    }

    let rendered = match &policy.default_window {
        // An exempt default is written to no native config; the policy notices `sync` reports
        // name it.
        Some(WindowSpec::Latest) => return Ok(SyncReport::Unchanged { path, warnings }),
        Some(WindowSpec::MinAge(age)) if !age.is_zero() => {
            render_age(*age).map(|text| (*age, text))
        }
        _ => None,
    };
    let Some((age, text)) = rendered else {
        // A freeze date or a sub-second age has no spelling in cargo's grammar, and writing the
        // zero age a floor folds a future freeze into would drop the freeze, so the existing
        // setting stays as it is.
        warnings.push(Diagnostic::new(
            DiagnosticKind::StricterNative,
            "cargo's min-publish-age is a rolling age in whole seconds, so sync leaves it as it is \
             for this window"
                .to_owned(),
        ));
        return Ok(SyncReport::Unchanged { path, warnings });
    };
    // The written table names crates.io, which is what makes cargo read crates.io's own variable.
    warnings.extend(crates_io_env_warning(env, age, &text));

    if current_age(&path)? == Some(age) {
        return Ok(SyncReport::Unchanged { path, warnings });
    }
    let mut document = cooldown_toml_util::parse_document(&path)?.unwrap_or_default();
    set_crates_io_age(&mut document, &text)
        .map_err(|reason| CoreError::Config(format!("{path}: {reason}")))?;
    if !dry_run {
        std::fs::create_dir_all(&config_dir)
            .map_err(|err| CoreError::Filesystem(format!("{config_dir}: {err}")))?;
        cooldown_toml_util::write_document(&path, &document)?;
    }
    Ok(SyncReport::Written { path, warnings })
}

/// The warning for a crates.io age in `env` that replaces the `age`, spelled `text`, sync writes.
fn crates_io_env_warning(
    env: &CargoEnvironment,
    age: jiff::SignedDuration,
    text: &str,
) -> Option<Diagnostic> {
    let raw = env.registry_ages.get(CRATES_IO_AGE_ENV)?;
    let (kind, effect) = match parse_age(raw) {
        Ok(found) if found == age => return None,
        Ok(found) if found < age => (
            DiagnosticKind::Config,
            "so a bare cargo command admits crates younger than the cooldown".to_owned(),
        ),
        Ok(_) => (
            DiagnosticKind::StricterNative,
            "so a bare cargo command holds back crates cooldown admits".to_owned(),
        ),
        Err(reason) => (
            DiagnosticKind::Config,
            format!("and cargo cannot parse it: {reason}"),
        ),
    };
    Some(Diagnostic::new(
        kind,
        format!(
            "{CRATES_IO_AGE_ENV} = {raw:?} replaces the crates.io min-publish-age of {text} sync \
             writes, {effect}"
        ),
    ))
}

/// Sets `registries.crates-io.min-publish-age`, keeping the formatting of everything else.
///
/// A missing `[registries]` table is created implicit, so the file gains only the
/// `[registries.crates-io]` header a person would write.
fn set_crates_io_age(document: &mut toml_edit::DocumentMut, age: &str) -> Result<(), String> {
    let [registries_key, crates_io_key, age_key] = CRATES_IO_AGE_KEY;
    let registries = document
        .entry(registries_key)
        .or_insert_with(|| {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            toml_edit::Item::Table(table)
        })
        .as_table_like_mut()
        .ok_or_else(|| format!("`{registries_key}` is not a table"))?;
    let crates_io = registries
        .entry(crates_io_key)
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
        .ok_or_else(|| format!("`{registries_key}.{crates_io_key}` is not a table"))?;
    match crates_io.get_mut(age_key) {
        Some(existing) => cooldown_toml_util::replace_value(existing, age),
        None => {
            crates_io.insert(age_key, toml_edit::value(age));
        }
    }
    Ok(())
}

/// The crates.io age `path` sets, when it sets a valid one.
///
/// An unparsable value counts as absent, so sync replaces it with a valid one.
fn current_age(path: &Utf8Path) -> Result<Option<jiff::SignedDuration>, CoreError> {
    let Some(value) = cooldown_toml_util::read_toml_file::<toml::Value>(path, "cargo config")?
    else {
        return Ok(None);
    };
    let raw = CRATES_IO_AGE_KEY
        .iter()
        .try_fold(&value, |table, key| table.get(key))
        .and_then(toml::Value::as_str);
    Ok(raw.and_then(|raw| parse_age(raw).ok()))
}

#[cfg(test)]
mod tests;
