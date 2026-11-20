//! pnpm native cooldown configuration parsing and text-preserving updates.

use crate::exclusions::{configured_rules, drop_versionless_rules, is_pnpm_safe_selector};
use camino::Utf8Path;
use cooldown_core::{CoreError, Result, WindowSpec};
use std::collections::{BTreeMap, BTreeSet};

/// cooldown's package selectors split by whether `sync` can write them into pnpm's
/// `minimumReleaseAgeExclude`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct NativeSelectors {
    /// The selectors pnpm reads exactly as cooldown does ([`is_pnpm_safe_selector`]), sorted and
    /// deduplicated.
    pub(crate) written: Vec<String>,
    /// The selectors outside the syntax both matchers read identically, sorted and deduplicated.
    ///
    /// They are left out of the native list, so pnpm's age gate keeps governing the packages they
    /// exempt unless another written rule covers them: stricter than cooldown, never looser.
    pub(crate) omitted: Vec<String>,
}

impl NativeSelectors {
    pub(crate) fn partition(selectors: &[String]) -> Self {
        let (written, omitted) = selectors
            .iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .cloned()
            .partition(|selector| is_pnpm_safe_selector(selector));
        NativeSelectors { written, omitted }
    }
}

/// Drops every existing `minimumReleaseAgeExclude` rule at `path` that exempts every version of
/// its packages and is not a `written` selector ([`drop_versionless_rules`]).
///
/// This is the cleanup for a sync that cannot write pnpm's window: a selector the policy withholds
/// or omits must not stay exempt natively, while the version-qualified rules are kept verbatim
/// because no admission was evaluated against the window pnpm still applies.
/// A list that keeps every rule is not rewritten.
/// An empty result is written as `empty` says: an existing key always stays as `key: []`, so the
/// exclusions pnpm inherits stay masked, and an [`EmptyList::Explicit`] list is written even where
/// the key or the file was absent, as a sync with a writable window does.
/// A scalar setting containing `${` is left alone, since pnpm may expand it into rules this cannot
/// read.
/// Returns whether the list changed.
///
/// # Errors
///
/// Returns a [`CoreError`] when the file cannot be read, parsed, or written.
pub(crate) fn prune_exclusions(
    path: &Utf8Path,
    written: &[String],
    empty: EmptyList,
    dry_run: bool,
) -> Result<bool> {
    let rules = match NativeAge::read(path)?.exclusions {
        ConfigStringList::One(scalar) if scalar.contains("${") => return Ok(false),
        configured => configured_rules(configured),
    };
    let retained = drop_versionless_rules(&rules, written);
    if !retained.is_empty() && retained == rules {
        return Ok(false);
    }
    set_yaml_block_list(path, "minimumReleaseAgeExclude", &retained, empty, dry_run)
}

#[derive(serde::Deserialize, Clone)]
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

#[derive(serde::Deserialize, Default)]
pub(crate) struct NativeAge {
    #[serde(rename = "minimumReleaseAge")]
    pub(crate) minutes: Option<i64>,
    #[serde(rename = "minimumReleaseAgeExclude", default)]
    pub(crate) exclusions: ConfigStringList,
}

impl NativeAge {
    pub(crate) fn read(path: &Utf8Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(content) => Self::parse(&content, path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(CoreError::Filesystem(format!("{path}: {err}"))),
        }
    }

    /// Reads the settings from `content`, the text of the file at `path`.
    pub(crate) fn parse(content: &str, path: &Utf8Path) -> Result<Self> {
        serde_saphyr::from_str(content).map_err(|err| CoreError::Parse(format!("{path}: {err}")))
    }
}

/// Converts a cooldown window into pnpm's rolling whole-minute representation, or `None` when the
/// window is no positive rolling age.
///
/// A partial minute rounds up, as Yarn's gate does, so the written gate is never shorter than the
/// window: a 90-second window becomes 2 minutes and a 30-second one 1 minute.
pub(crate) fn window_minutes(spec: &WindowSpec) -> Option<i64> {
    let WindowSpec::MinAge(duration) = spec else {
        return None;
    };
    let seconds = duration.as_secs() + i64::from(duration.subsec_nanos() > 0);
    let seconds = u64::try_from(seconds).ok().filter(|seconds| *seconds > 0)?;
    i64::try_from(seconds.div_ceil(60)).ok()
}

/// The document `content`, the text of the file at `path`, with the top-level scalar `key` set
/// to `value`, or `None` when the setting already reads as `value`, however it is spelled.
///
/// pnpm settings are top-level scalars, so a line-level edit keeps comments and key order where a
/// full YAML round trip would drop them.
/// An absent key is added at the top of the document.
///
/// # Errors
///
/// Returns a [`CoreError`] when the document cannot be parsed or `value` is no YAML scalar, and
/// [`CoreError::Config`] when the line edit would not read back as `key: value` beside the
/// document's other settings ([`verify_top_level_edit`]).
pub(crate) fn render_yaml_scalar(
    content: &str,
    path: &Utf8Path,
    key: &str,
    value: &str,
) -> Result<Option<String>> {
    let intended: serde_json::Value = serde_saphyr::from_str(value)
        .map_err(|error| CoreError::Serialization(format!("{key}: {error}")))?;
    if top_level_settings(content, path)?.get(key) == Some(&intended) {
        return Ok(None);
    }
    let target = format!("{key}: {value}");
    let prefix = format!("{key}:");
    let mut lines: Vec<String> = Vec::new();
    let mut found = false;
    for line in content.lines() {
        // The colon prevents a short key from matching a longer key with the same prefix.
        if !found && !line.starts_with(char::is_whitespace) && line.starts_with(&prefix) {
            found = true;
            lines.push(target.clone());
        } else {
            lines.push(line.to_string());
        }
    }
    let text = if found {
        let mut text = lines.join("\n");
        if content.ends_with('\n') {
            text.push('\n');
        }
        text
    } else {
        let mut text = target;
        text.push('\n');
        text.push_str(content);
        text
    };
    verify_top_level_edit(path, content, &text, key, Some(&intended))?;
    Ok(Some(text))
}

/// Writes `text` to the settings file at `path` atomically, through to the file a symbolic link
/// at `path` names, so a linked settings file stays linked as a plain write leaves it.
///
/// # Errors
///
/// Returns a [`CoreError`] when a link cannot be read, the links form a loop, or the file cannot
/// be written.
pub(crate) fn write_settings(path: &Utf8Path, text: &str) -> Result<()> {
    cooldown_core::fs::atomic_write(&link_target(path)?, text.as_bytes())
}

/// The file the symbolic links starting at `path` end at, which need not exist yet, or `path`
/// itself when it is no link.
fn link_target(path: &Utf8Path) -> Result<std::path::PathBuf> {
    // The bound the operating systems themselves put on a chain of links.
    const MAX_LINKS: usize = 40;
    let mut target = path.as_std_path().to_path_buf();
    for _ in 0..MAX_LINKS {
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = std::fs::read_link(&target)
                    .map_err(|error| CoreError::Filesystem(format!("{path}: {error}")))?;
                target = match target.parent() {
                    Some(parent) => parent.join(link),
                    None => link,
                };
            }
            Ok(_) => return Ok(target),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(target),
            Err(error) => return Err(CoreError::Filesystem(format!("{path}: {error}"))),
        }
    }
    Err(CoreError::Filesystem(format!(
        "{path}: too many levels of symbolic links"
    )))
}

/// The top-level settings of the YAML document `content`, the text of the file at `path`.
///
/// An empty document, or one of comments alone, has none.
fn top_level_settings(
    content: &str,
    path: &Utf8Path,
) -> Result<BTreeMap<String, serde_json::Value>> {
    let settings: Option<BTreeMap<String, serde_json::Value>> = serde_saphyr::from_str(content)
        .map_err(|error| CoreError::Parse(format!("{path}: {error}")))?;
    Ok(settings.unwrap_or_default())
}

/// Checks that `rendered`, a line edit of `original` (the text of the file at `path`), sets the
/// top-level `key` to `intended`, or removes it when that is `None`, and reads back with every
/// other top-level setting unchanged.
///
/// The line editors find a key only as a plain top-level key followed by its block, so a key they
/// miss, such as a quoted one, would gain a duplicate, and a value they cannot delimit, such as a
/// flow collection spanning lines, would be cut in half.
/// This check turns either into an error rather than a document pnpm reads differently or cannot
/// read at all.
///
/// # Errors
///
/// Returns [`CoreError::Config`], naming the file and the key, when the edit does not read back
/// as intended, and [`CoreError::Parse`] when `original` itself is no readable YAML mapping.
fn verify_top_level_edit(
    path: &Utf8Path,
    original: &str,
    rendered: &str,
    key: &str,
    intended: Option<&serde_json::Value>,
) -> Result<()> {
    let mut before = top_level_settings(original, path)?;
    before.remove(key);
    let unsupported = || {
        CoreError::Config(format!(
            "{path}: cooldown cannot rewrite `{key}` in the form it is written, such as a quoted \
             key or a flow collection spanning lines; write it as a plain top-level `{key}:` key \
             with a block value"
        ))
    };
    let mut after = top_level_settings(rendered, path).map_err(|_| unsupported())?;
    let actual = after.remove(key);
    if actual.as_ref() == intended && after == before {
        Ok(())
    } else {
        Err(unsupported())
    }
}

/// The exclusive end of the block belonging to a top-level key, scanning `lines` from `start`
/// (the line after the key).
///
/// YAML block content may legally contain blank lines and comment lines at any indentation —
/// including column 0 — between its entries, so stopping at the first line without a leading
/// space would truncate the block: a comparison against the truncated prefix misses trailing
/// entries, and a removal leaves an orphaned fragment that breaks the document. A blank/comment
/// run counts as interior only while further indented content follows it; a trailing run before
/// the next top-level key (or the end of the document) is left with the following section, so
/// replacing or removing the block never swallows a comment that documents what comes next.
fn block_end(lines: &[&str], start: usize) -> usize {
    let mut end = start;
    while let Some(line) = lines.get(end) {
        if continues_block(line) {
            end += 1;
            continue;
        }
        if !line.is_empty() && !line.starts_with('#') {
            // The next top-level key bounds the block.
            break;
        }
        let mut probe = end + 1;
        let continues = loop {
            match lines.get(probe) {
                Some(next) if continues_block(next) => break true,
                Some(next) if next.is_empty() || next.starts_with('#') => probe += 1,
                _ => break false,
            }
        };
        if !continues {
            break;
        }
        // The run is interior: consume it and continue at the indented line that follows.
        end = probe;
    }
    end
}

/// Whether `line` belongs to the value of the top-level key above it: an indented line, or an
/// item of an indentless block sequence (`key:` followed by `- item` at column zero).
///
/// A top-level mapping cannot hold a sequence item of its own, so a column-zero `-` followed by
/// whitespace or nothing is always the value of the key before it; `---` is a document marker.
fn continues_block(line: &str) -> bool {
    line.starts_with(char::is_whitespace)
        || line
            .strip_prefix('-')
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Replaces the block of the first top-level `prefix` key in `lines` with `desired`, returning
/// `(rendered document, replaced block)` — or `None` when the key is absent. Shared by the
/// block-sequence and string-map setters, whose edits differ only in how `desired` is built.
fn replace_top_level_block(
    lines: &[&str],
    prefix: &str,
    desired: &[String],
) -> Option<(Vec<String>, Vec<String>)> {
    let mut out: Vec<String> = Vec::new();
    let mut existing: Vec<String> = Vec::new();
    let mut found = false;
    let mut index = 0;
    while let Some(&line) = lines.get(index) {
        if !found && !line.starts_with(char::is_whitespace) && line.starts_with(prefix) {
            // A top-level key has no indentation; its block is the following indented lines,
            // interior blank/comment lines included (see [`block_end`]).
            found = true;
            let end = block_end(lines, index + 1);
            existing.extend(
                lines
                    .get(index..end)
                    .into_iter()
                    .flatten()
                    .map(|block_line| (*block_line).to_string()),
            );
            // Replace the old block in place, or remove it when the desired block is empty.
            out.extend(desired.iter().cloned());
            index = end;
        } else {
            out.push(line.to_string());
            index += 1;
        }
    }
    found.then_some((out, existing))
}

/// How [`set_yaml_block_list`] writes an empty item list.
///
/// pnpm reads an explicit empty list as the setting itself, which masks a list inherited from user
/// or global configuration, while an absent key exposes it.
/// An existing key, whatever its value, is therefore never removed: an empty list always keeps it
/// as `key: []`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmptyList {
    /// An existing key becomes `key: []`, and an absent key stays absent.
    IfPresent,
    /// The key is written as an explicit empty sequence (`key: []`), even where it was absent.
    Explicit,
}

/// Sets one top-level YAML block sequence while preserving the rest of the document.
///
/// An empty item list is written as `key: []` where the key exists, and where it is absent only
/// for [`EmptyList::Explicit`]; a key is never removed.
/// Items are emitted as double-quoted scalars in caller-provided order, which safely preserves
/// scoped names such as `@scope/pkg` and glob patterns such as `@scope/*`.
/// A missing file is created only when something is written and `dry_run` is disabled.
///
/// # Errors
///
/// Returns a [`CoreError`] when the file cannot be read or written.
pub(crate) fn set_yaml_block_list(
    path: &Utf8Path,
    key: &str,
    items: &[String],
    empty: EmptyList,
    dry_run: bool,
) -> Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(CoreError::Filesystem(format!("{path}: {error}"))),
    };
    let Some(text) = render_yaml_block_list(&content, path, key, items, empty)? else {
        return Ok(false);
    };
    if !dry_run {
        std::fs::write(path, text)
            .map_err(|error| CoreError::Filesystem(format!("{path}: {error}")))?;
    }
    Ok(true)
}

/// The document `content`, the text of the file at `path`, with one top-level YAML block
/// sequence set as [`set_yaml_block_list`] sets it, or `None` when the setting already reads as
/// those items, however it is spelled.
///
/// # Errors
///
/// Returns a [`CoreError`] when the document cannot be parsed or an item cannot be serialized,
/// and [`CoreError::Config`] when the line edit would not read back as the intended list beside
/// the document's other settings ([`verify_top_level_edit`]).
pub(crate) fn render_yaml_block_list(
    content: &str,
    path: &Utf8Path,
    key: &str,
    items: &[String],
    empty: EmptyList,
) -> Result<Option<String>> {
    let current = top_level_settings(content, path)?.remove(key);
    let present = current.is_some();
    // An empty list leaves an absent key absent unless it must be written explicitly.
    let intended = (present || !items.is_empty() || empty == EmptyList::Explicit).then(|| {
        serde_json::Value::Array(
            items
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
        )
    });
    if current == intended {
        return Ok(None);
    }
    let prefix = format!("{key}:");
    let lines: Vec<&str> = content.lines().collect();
    // An empty desired block leaves an absent key absent; `key: []` keeps it explicitly empty.
    let desired: Vec<String> = if items.is_empty() {
        match empty {
            EmptyList::IfPresent if !present => Vec::new(),
            EmptyList::IfPresent | EmptyList::Explicit => vec![format!("{key}: []")],
        }
    } else {
        std::iter::once(format!("{key}:"))
            .chain(
                items
                    .iter()
                    .map(|item| serde_json::to_string(item).map(|item| format!("  - {item}")))
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| CoreError::Serialization(error.to_string()))?,
            )
            .collect()
    };

    let replaced = replace_top_level_block(&lines, &prefix, &desired);
    let found = replaced.is_some();
    let (out, existing) = replaced.unwrap_or_default();

    let changed = if found {
        existing != desired
    } else {
        !desired.is_empty()
    };
    if !changed {
        return Ok(None);
    }

    let mut text = if found {
        out.join("\n")
    } else {
        // New keys follow the existing document without disturbing its text.
        let mut text = content.to_string();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&desired.join("\n"));
        text
    };
    if content.ends_with('\n') || !found {
        text.push('\n');
    }
    verify_top_level_edit(path, content, &text, key, intended.as_ref())?;
    Ok(Some(text))
}

/// Sets one top-level YAML string map while preserving the rest of the document.
///
/// The repair path restores the original file after using this temporary map, so comments inside
/// the original block reappear unchanged.
///
/// # Errors
///
/// Returns a [`CoreError`] when values cannot be serialized or the file cannot be read, parsed, or
/// written, and [`CoreError::Config`] when the line edit would not read back as the intended map
/// beside the file's other settings ([`verify_top_level_edit`]).
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
    let intended = (!items.is_empty()).then(|| {
        serde_json::Value::Object(
            items
                .iter()
                .map(|(item_key, value)| {
                    (item_key.clone(), serde_json::Value::String(value.clone()))
                })
                .collect(),
        )
    });
    if top_level_settings(&content, path)?.remove(key) == intended {
        return Ok(false);
    }
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
    let lines: Vec<&str> = content.lines().collect();
    let replaced = replace_top_level_block(&lines, &prefix, &desired);
    let found = replaced.is_some();
    let (out, existing) = replaced.unwrap_or_default();

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
    verify_top_level_edit(path, &content, &text, key, intended.as_ref())?;
    std::fs::write(path, text)
        .map_err(|error| CoreError::Filesystem(format!("{path}: {error}")))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use color_eyre::eyre;
    use indoc::indoc;

    /// Only selectors pnpm reads exactly as cooldown does are written; the others are returned
    /// for reporting, both sorted and deduplicated.
    #[test]
    fn native_selectors_split_written_from_omitted() {
        let selectors = [
            "@types/{node,react}",
            "*-plugin",
            "**",
            "JSONStream",
            "*-plugin",
            "react-?om",
        ]
        .map(String::from);
        assert_eq!(
            NativeSelectors::partition(&selectors),
            NativeSelectors {
                written: vec!["*-plugin".to_string(), "JSONStream".to_string()],
                omitted: vec![
                    "**".to_string(),
                    "@types/{node,react}".to_string(),
                    "react-?om".to_string(),
                ],
            }
        );
    }

    #[test]
    fn configured_string_list_accepts_pnpm_singletons_and_arrays() -> eyre::Result<()> {
        let one = serde_json::from_str::<ConfigStringList>("\"nanoid\"")?;
        let many = serde_json::from_str::<ConfigStringList>("[\"nanoid\", \"eslint\"]")?;

        assert_eq!(one.into_vec(), vec!["nanoid".to_string()]);
        assert_eq!(
            many.into_vec(),
            vec!["nanoid".to_string(), "eslint".to_string()]
        );
        Ok(())
    }

    #[test]
    fn render_yaml_scalar_adds_updates_and_is_idempotent() -> eyre::Result<()> {
        let path = Utf8Path::new("pnpm-workspace.yaml");
        let original = "packages:\n  - \"a\"\n# keep me\n";

        // Absent key → prepended, comments and existing content preserved.
        let added = render_yaml_scalar(original, path, "minimumReleaseAge", "20160")?
            .ok_or_else(|| eyre::eyre!("the key is added"))?;
        assert!(added.contains("minimumReleaseAge: 20160"));
        assert!(added.contains("# keep me"), "comments preserved");
        assert!(added.contains("packages:"), "existing content preserved");

        // Idempotent.
        assert_eq!(
            render_yaml_scalar(&added, path, "minimumReleaseAge", "20160")?,
            None
        );

        // Update in place.
        let updated = render_yaml_scalar(&added, path, "minimumReleaseAge", "30")?
            .ok_or_else(|| eyre::eyre!("the value changes"))?;
        assert!(updated.contains("minimumReleaseAge: 30"));
        assert!(!updated.contains("20160"));

        // An empty document gains the key alone.
        assert_eq!(
            render_yaml_scalar("", path, "minimumReleaseAge", "20160")?.as_deref(),
            Some("minimumReleaseAge: 20160\n")
        );
        Ok(())
    }

    #[test]
    fn set_yaml_block_list_adds_updates_empties_and_is_idempotent() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            &path,
            "minimumReleaseAge: 20160\npackages:\n  - \"a\"\n# keep me\n",
        )?;

        // Absent key → block appended, the rest of the document (scalar, packages, comment) preserved.
        let items = vec![
            "@typescript/native-preview".to_string(),
            "@scope/*".to_string(),
        ];
        assert!(set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &items,
            EmptyList::IfPresent,
            false
        )?);
        let after = std::fs::read_to_string(&path)?;
        assert!(after.contains(
            "minimumReleaseAgeExclude:\n  - \"@typescript/native-preview\"\n  - \"@scope/*\""
        ));
        assert!(
            after.contains("minimumReleaseAge: 20160"),
            "scalar preserved"
        );
        assert!(after.contains("packages:"), "packages preserved");
        assert!(after.contains("# keep me"), "comment preserved");

        // Idempotent: the same items rewrite nothing.
        assert!(!set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &items,
            EmptyList::IfPresent,
            false
        )?);

        // Update in place: a different list replaces the block.
        let fewer = vec!["@typescript/native-preview".to_string()];
        assert!(set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &fewer,
            EmptyList::IfPresent,
            false
        )?);
        let updated = std::fs::read_to_string(&path)?;
        assert!(
            updated.contains("minimumReleaseAgeExclude:\n  - \"@typescript/native-preview\"\n")
        );
        assert!(!updated.contains("@scope/*"), "dropped item is gone");
        assert!(updated.contains("# keep me"), "comment still preserved");

        // Empty list → the existing key stays as an explicit empty sequence, so an inherited list
        // stays masked.
        assert!(set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &[],
            EmptyList::IfPresent,
            false
        )?);
        let emptied = std::fs::read_to_string(&path)?;
        assert!(
            emptied.contains("minimumReleaseAgeExclude: []\n"),
            "key kept empty"
        );
        assert!(
            emptied.contains("minimumReleaseAge: 20160"),
            "scalar untouched"
        );
        // Emptying again is a no-op.
        assert!(!set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &[],
            EmptyList::IfPresent,
            false
        )?);
        Ok(())
    }

    /// An explicit empty list is written as `key: []`: it creates the key, replaces an existing
    /// block, is idempotent, and reports without writing under `dry_run`.
    #[test]
    fn set_yaml_block_list_writes_an_explicit_empty_list() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let key = "minimumReleaseAgeExclude";
        std::fs::write(&path, "minimumReleaseAge: 20160\n")?;

        // Absent key, dry run: the change is reported and nothing is written.
        assert!(set_yaml_block_list(
            &path,
            key,
            &[],
            EmptyList::Explicit,
            true
        )?);
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "minimumReleaseAge: 20160\n"
        );

        // Absent key: the explicit empty sequence is appended.
        assert!(set_yaml_block_list(
            &path,
            key,
            &[],
            EmptyList::Explicit,
            false
        )?);
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "minimumReleaseAge: 20160\nminimumReleaseAgeExclude: []\n"
        );
        assert_eq!(
            NativeAge::read(&path)?.exclusions.into_vec(),
            [] as [String; 0]
        );

        // Present and already empty: nothing changes.
        assert!(!set_yaml_block_list(
            &path,
            key,
            &[],
            EmptyList::Explicit,
            false
        )?);

        // An existing block is replaced in place, and an empty list again empties it.
        assert!(set_yaml_block_list(
            &path,
            key,
            &["a".to_string()],
            EmptyList::Explicit,
            false
        )?);
        assert!(set_yaml_block_list(
            &path,
            key,
            &[],
            EmptyList::Explicit,
            false
        )?);
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "minimumReleaseAge: 20160\nminimumReleaseAgeExclude: []\n"
        );

        // An existing explicit empty sequence is never removed.
        assert!(!set_yaml_block_list(
            &path,
            key,
            &[],
            EmptyList::IfPresent,
            false
        )?);

        // Only an absent key stays absent.
        std::fs::write(&path, "minimumReleaseAge: 20160\n")?;
        assert!(!set_yaml_block_list(
            &path,
            key,
            &[],
            EmptyList::IfPresent,
            false
        )?);
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "minimumReleaseAge: 20160\n"
        );
        Ok(())
    }

    /// A blank line and an indented comment inside the block must not truncate it: syncing a
    /// shorter list still sees the entries after the gap, reports the change, and removes them.
    #[test]
    fn set_yaml_block_list_sees_entries_past_interior_blank_and_comment_lines() -> eyre::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            &path,
            indoc! {r#"
                minimumReleaseAgeExclude:
                  - "a"

                  # our own packages
                  - "internal-tool"
                packages:
                  - "x"
            "#},
        )?;

        // The truncated-prefix bug reported changed=false here, silently leaving `internal-tool`
        // exempt.
        assert!(set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &["a".to_string()],
            EmptyList::IfPresent,
            false
        )?);
        let after = std::fs::read_to_string(&path)?;
        assert!(!after.contains("internal-tool"), "stale entry removed");
        assert!(
            !after.contains("# our own packages"),
            "interior comment replaced with the block"
        );
        assert!(after.contains("minimumReleaseAgeExclude:\n  - \"a\"\npackages:"));
        assert!(
            after.contains("packages:\n  - \"x\""),
            "next section intact"
        );
        Ok(())
    }

    /// A column-0 `#` comment is legal inside a block sequence; it must not end the block early.
    #[test]
    fn set_yaml_block_list_consumes_column_zero_comments_inside_the_block() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            &path,
            indoc! {r#"
                minimumReleaseAgeExclude:
                  - "a"
                # interior note
                  - "b"
                packages:
                  - "x"
            "#},
        )?;

        assert!(set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &["a".to_string()],
            EmptyList::IfPresent,
            false
        )?);
        let after = std::fs::read_to_string(&path)?;
        assert!(!after.contains("- \"b\""), "entry past the comment removed");
        assert!(!after.contains("# interior note"));
        assert!(
            after.contains("packages:\n  - \"x\""),
            "next section intact"
        );
        Ok(())
    }

    /// A list written as an indentless block sequence, with its items at column zero, is the
    /// key's value: a rewrite replaces every item and keeps the keys after it, and the rewritten
    /// document reads back as the rules written beside the settings it had.
    #[test]
    fn set_yaml_block_list_replaces_an_indentless_sequence() -> eyre::Result<()> {
        let path = Utf8Path::new("pnpm-workspace.yaml");
        let original = indoc! {r#"
            packages: []
            minimumReleaseAgeExclude:
            - "a"
            - "b@1.0.0"
            minimumReleaseAge: 20160
        "#};
        assert_eq!(
            NativeAge::parse(original, path)?.exclusions.into_vec(),
            ["a", "b@1.0.0"]
        );

        let rendered = render_yaml_block_list(
            original,
            path,
            "minimumReleaseAgeExclude",
            &["c@2.0.0".to_string()],
            EmptyList::IfPresent,
        )?
        .ok_or_else(|| eyre::eyre!("the list changes"))?;
        assert_eq!(
            rendered,
            indoc! {r#"
                packages: []
                minimumReleaseAgeExclude:
                  - "c@2.0.0"
                minimumReleaseAge: 20160
            "#}
        );
        let after = NativeAge::parse(&rendered, path)?;
        assert_eq!(after.exclusions.into_vec(), ["c@2.0.0"]);
        assert_eq!(after.minutes, Some(20160));

        // The same items read back unchanged, so the indentless list is not rewritten.
        assert_eq!(
            render_yaml_block_list(
                original,
                path,
                "minimumReleaseAgeExclude",
                &["a".to_string(), "b@1.0.0".to_string()],
                EmptyList::IfPresent,
            )?,
            None
        );
        Ok(())
    }

    /// A list that already reads as the intended rules is never rewritten, whatever its quoting,
    /// spelling, or form, and an absent key stays absent for an empty list.
    #[test]
    fn an_unchanged_list_is_not_rewritten() -> eyre::Result<()> {
        let path = Utf8Path::new("pnpm-workspace.yaml");
        let render = |content: &str, items: &[&str]| {
            let items: Vec<String> = items.iter().map(ToString::to_string).collect();
            render_yaml_block_list(
                content,
                path,
                "minimumReleaseAgeExclude",
                &items,
                EmptyList::IfPresent,
            )
        };
        for content in [
            "minimumReleaseAgeExclude:\n  - legacy # kept by hand\n  - 'a@1.0.0'\n",
            "minimumReleaseAgeExclude: [legacy, \"a@1.0.0\"]\n",
            "\"minimumReleaseAgeExclude\": [\"legacy\", \"a@1.0.0\"]\n",
        ] {
            assert_eq!(render(content, &["legacy", "a@1.0.0"])?, None, "{content}");
        }
        assert_eq!(render("packages: []\n", &[])?, None);
        assert_eq!(render("", &[])?, None);
        assert_eq!(render("# settings follow\n", &[])?, None);
        assert_eq!(
            render("# settings follow\n", &["a@1.0.0"])?.as_deref(),
            Some("# settings follow\nminimumReleaseAgeExclude:\n  - \"a@1.0.0\"\n")
        );
        assert_eq!(render("minimumReleaseAgeExclude: []\n", &[])?, None);
        Ok(())
    }

    /// A quoted key the line editor cannot find is refused rather than duplicated, and so is a
    /// flow sequence spanning lines, which the editor would cut in half; a one-line flow
    /// sequence is rewritten as a block list.
    #[test]
    fn a_list_the_line_editor_cannot_rewrite_is_refused() -> eyre::Result<()> {
        let path = Utf8Path::new("pnpm-workspace.yaml");
        let items = ["legacy".to_string(), "new@1.0.0".to_string()];
        let render = |content: &str| {
            render_yaml_block_list(
                content,
                path,
                "minimumReleaseAgeExclude",
                &items,
                EmptyList::IfPresent,
            )
        };

        for content in [
            "packages: []\n\"minimumReleaseAgeExclude\": [\"legacy\"]\n",
            "minimumReleaseAgeExclude: [\n  \"legacy\"\n]\npackages: []\n",
        ] {
            let refused = render(content);
            std::assert_matches!(
                &refused,
                Err(CoreError::Config(message))
                    if message.contains("pnpm-workspace.yaml")
                        && message.contains("minimumReleaseAgeExclude"),
                "{content}"
            );
        }

        let flow = render("minimumReleaseAgeExclude: [\"legacy\"]\npackages: []\n")?
            .ok_or_else(|| eyre::eyre!("the list changes"))?;
        assert_eq!(
            flow,
            indoc! {r#"
                minimumReleaseAgeExclude:
                  - "legacy"
                  - "new@1.0.0"
                packages: []
            "#}
        );
        Ok(())
    }

    /// The window renderer leaves a setting that already reads as the value alone and refuses a
    /// quoted key it would otherwise duplicate.
    #[test]
    fn the_scalar_renderer_compares_and_verifies_structurally() -> eyre::Result<()> {
        let path = Utf8Path::new("pnpm-workspace.yaml");
        assert_eq!(
            render_yaml_scalar(
                "minimumReleaseAge: 20160 # two weeks\n",
                path,
                "minimumReleaseAge",
                "20160"
            )?,
            None
        );
        std::assert_matches!(
            render_yaml_scalar(
                "\"minimumReleaseAge\": 1440\npackages: []\n",
                path,
                "minimumReleaseAge",
                "20160"
            ),
            Err(CoreError::Config(_))
        );
        Ok(())
    }

    /// A column-zero document marker is no sequence item, so it still bounds the block.
    #[test]
    fn a_document_marker_bounds_a_block() {
        assert!(!continues_block("---"));
        assert!(continues_block("- \"a\""));
        assert!(continues_block("-"));
        assert!(continues_block("  - \"a\""));
        assert!(!continues_block("minimumReleaseAge: 1"));
    }

    /// A trailing blank/comment run before the next top-level key belongs to the *following*
    /// section: an idempotent sync must not count it into the block, and a rewrite must leave it
    /// in place.
    #[test]
    fn set_yaml_block_list_leaves_trailing_comments_with_the_next_section() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            &path,
            indoc! {r#"
                minimumReleaseAgeExclude:
                  - "a"

                # the workspace members
                packages:
                  - "x"
            "#},
        )?;

        // Idempotent: the trailing run is not part of the block, so the same item list changes
        // nothing.
        assert!(!set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &["a".to_string()],
            EmptyList::IfPresent,
            false
        )?);

        assert!(set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &["b".to_string()],
            EmptyList::IfPresent,
            false
        )?);
        let after = std::fs::read_to_string(&path)?;
        assert!(
            after.contains(
                "minimumReleaseAgeExclude:\n  - \"b\"\n\n# the workspace members\npackages:"
            ),
            "trailing comment stays with the next section: {after}"
        );
        Ok(())
    }

    /// Emptying the list must replace the *whole* block — an interior blank line or comment must
    /// not strand a fragment that turns the document into a YAML parse error (this is the sync
    /// path; no journal restores the file afterwards).
    #[test]
    fn set_yaml_block_list_empty_sync_replaces_interior_fragments() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            &path,
            indoc! {r#"
                minimumReleaseAgeExclude:
                  - "a"

                  # our own packages
                  - "internal-tool"
                packages:
                  - "x"
            "#},
        )?;

        assert!(set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &[],
            EmptyList::IfPresent,
            false
        )?);
        assert_eq!(
            std::fs::read_to_string(&path)?,
            indoc! {r#"
                minimumReleaseAgeExclude: []
                packages:
                  - "x"
            "#},
            "no orphaned fragment may remain"
        );
        Ok(())
    }

    #[test]
    fn set_yaml_string_map_replaces_only_the_requested_block() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            &path,
            "minimumReleaseAge: 20160\noverrides:\n  existing: \"1.0.0\"\npackages:\n  - \"a\"\n# keep me\n",
        )?;
        let items = BTreeMap::from([
            ("@scope/pkg".to_string(), "2.0.0".to_string()),
            ("existing".to_string(), "1.1.0".to_string()),
        ]);

        assert!(set_yaml_string_map(&path, "overrides", &items)?);
        let written = std::fs::read_to_string(&path)?;
        assert!(
            written
                .contains("overrides:\n  \"@scope/pkg\": \"2.0.0\"\n  \"existing\": \"1.1.0\"\n")
        );
        assert!(written.contains("minimumReleaseAge: 20160"));
        assert!(written.contains("packages:\n  - \"a\""));
        assert!(written.contains("# keep me"));
        assert!(!set_yaml_string_map(&path, "overrides", &items)?);
        Ok(())
    }

    /// A user `overrides:` block containing blank lines must be replaced wholesale: leftover
    /// entries merged after the desired block would be duplicate mapping keys, which pnpm rejects
    /// — failing the apply that installed the temporary overrides.
    #[test]
    fn set_yaml_string_map_replaces_a_block_containing_blank_lines() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            &path,
            indoc! {r#"
                overrides:
                  existing: "1.0.0"

                  legacy: "0.9.0"
                packages:
                  - "a"
            "#},
        )?;
        let items = BTreeMap::from([("@scope/pkg".to_string(), "2.0.0".to_string())]);

        assert!(set_yaml_string_map(&path, "overrides", &items)?);
        let written = std::fs::read_to_string(&path)?;
        assert!(
            written.contains("overrides:\n  \"@scope/pkg\": \"2.0.0\"\npackages:"),
            "the whole user block is replaced: {written}"
        );
        assert!(
            !written.contains("legacy"),
            "no leftover entry after the desired block"
        );
        assert!(
            !written.contains("existing"),
            "no duplicate overrides content"
        );
        assert!(
            written.contains("packages:\n  - \"a\""),
            "next section intact"
        );
        Ok(())
    }
}
