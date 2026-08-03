//! pnpm native cooldown configuration parsing and text-preserving updates.

use camino::Utf8Path;
use cooldown_core::{CoreError, Result, WindowSpec};
use std::collections::BTreeMap;

#[derive(serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum ConfigStringList {
    One(String),
    Many(Vec<String>),
}

impl ConfigStringList {
    pub(crate) fn into_vec(self) -> Vec<String> {
        match self {
            ConfigStringList::One(value) => vec![value],
            ConfigStringList::Many(values) => values,
        }
    }
}

impl Default for ConfigStringList {
    fn default() -> Self {
        ConfigStringList::Many(Vec::new())
    }
}

/// Converts a cooldown window into pnpm's rolling whole-minute representation.
pub(crate) fn window_minutes(spec: &WindowSpec) -> Option<i64> {
    match spec {
        WindowSpec::MinAge(duration) => {
            let minutes = duration.as_secs() / 60;
            (minutes > 0).then_some(minutes)
        }
        WindowSpec::Freeze(_) | WindowSpec::Latest => None,
    }
}

/// Sets one top-level YAML scalar while preserving comments and key order.
///
/// A missing file is created unless `dry_run` is enabled.
/// The return value reports whether the file changed or would change.
///
/// # Errors
///
/// Returns a [`CoreError`] when the file cannot be read or written.
pub(crate) fn set_yaml_scalar(
    path: &Utf8Path,
    key: &str,
    value: &str,
    dry_run: bool,
) -> Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(CoreError::Filesystem(format!("{path}: {error}"))),
    };
    let target = format!("{key}: {value}");
    let prefix = format!("{key}:");
    let mut lines: Vec<String> = Vec::new();
    let mut found = false;
    let mut changed = false;
    for line in content.lines() {
        // The colon prevents a short key from matching a longer key with the same prefix.
        if !line.starts_with(char::is_whitespace) && line.starts_with(&prefix) {
            found = true;
            if line == target {
                lines.push(line.to_string());
            } else {
                changed = true;
                lines.push(target.clone());
            }
        } else {
            lines.push(line.to_string());
        }
    }
    if !found {
        if !dry_run {
            let mut out = target;
            out.push('\n');
            out.push_str(&content);
            std::fs::write(path, out)
                .map_err(|error| CoreError::Filesystem(format!("{path}: {error}")))?;
        }
        return Ok(true);
    }
    if changed && !dry_run {
        let mut out = lines.join("\n");
        if content.ends_with('\n') {
            out.push('\n');
        }
        std::fs::write(path, out)
            .map_err(|error| CoreError::Filesystem(format!("{path}: {error}")))?;
    }
    Ok(changed)
}

/// Sets one top-level YAML block sequence while preserving the rest of the document.
///
/// An empty item list removes the key and its block.
/// Items are emitted as double-quoted scalars in caller-provided order.
/// A missing file is created only for a non-empty list and when `dry_run` is disabled.
///
/// # Errors
///
/// Returns a [`CoreError`] when the file cannot be read or written.
pub(crate) fn set_yaml_block_list(
    path: &Utf8Path,
    key: &str,
    items: &[String],
    dry_run: bool,
) -> Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(CoreError::Filesystem(format!("{path}: {error}"))),
    };
    let desired: Vec<String> = if items.is_empty() {
        Vec::new()
    } else {
        std::iter::once(format!("{key}:"))
            .chain(items.iter().map(|item| format!("  - \"{item}\"")))
            .collect()
    };

    let prefix = format!("{key}:");
    let mut out: Vec<String> = Vec::new();
    let mut existing: Vec<String> = Vec::new();
    let mut found = false;
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        if !found && !line.starts_with(char::is_whitespace) && line.starts_with(&prefix) {
            found = true;
            existing.push(line.to_string());
            while lines
                .peek()
                .is_some_and(|next| next.starts_with(char::is_whitespace))
            {
                existing.push(lines.next().unwrap_or_default().to_string());
            }
            out.extend(desired.iter().cloned());
        } else {
            out.push(line.to_string());
        }
    }

    let changed = if found {
        existing != desired
    } else {
        !desired.is_empty()
    };
    if !changed || dry_run {
        return Ok(changed);
    }

    let mut text = if found {
        out.join("\n")
    } else {
        let mut text = content.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&desired.join("\n"));
        text
    };
    if content.ends_with('\n') || !found {
        text.push('\n');
    }
    std::fs::write(path, text)
        .map_err(|error| CoreError::Filesystem(format!("{path}: {error}")))?;
    Ok(true)
}

/// Sets one top-level YAML string map while preserving the rest of the document.
///
/// The repair path restores the original file after using this temporary map, so comments inside
/// the original block reappear unchanged.
///
/// # Errors
///
/// Returns a [`CoreError`] when values cannot be serialized or the file cannot be read or written.
pub(crate) fn set_yaml_string_map(
    path: &Utf8Path,
    key: &str,
    items: &BTreeMap<String, String>,
) -> Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(CoreError::Filesystem(format!("{path}: {error}"))),
    };
    let mut desired = Vec::new();
    if !items.is_empty() {
        desired.push(format!("{key}:"));
        for (item_key, value) in items {
            let item_key = serde_json::to_string(item_key)
                .map_err(|error| CoreError::Serialization(error.to_string()))?;
            let value = serde_json::to_string(value)
                .map_err(|error| CoreError::Serialization(error.to_string()))?;
            desired.push(format!("  {item_key}: {value}"));
        }
    }

    let prefix = format!("{key}:");
    let mut out = Vec::new();
    let mut existing = Vec::new();
    let mut found = false;
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        if !found && !line.starts_with(char::is_whitespace) && line.starts_with(&prefix) {
            found = true;
            existing.push(line.to_string());
            while lines
                .peek()
                .is_some_and(|next| next.starts_with(char::is_whitespace))
            {
                existing.push(lines.next().unwrap_or_default().to_string());
            }
            out.extend(desired.iter().cloned());
        } else {
            out.push(line.to_string());
        }
    }

    let changed = if found {
        existing != desired
    } else {
        !desired.is_empty()
    };
    if !changed {
        return Ok(false);
    }

    let mut text = if found {
        out.join("\n")
    } else {
        let mut text = content.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&desired.join("\n"));
        text
    };
    if content.ends_with('\n') || !found {
        text.push('\n');
    }
    std::fs::write(path, text)
        .map_err(|error| CoreError::Filesystem(format!("{path}: {error}")))?;
    Ok(true)
}
