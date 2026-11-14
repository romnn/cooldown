//! The parts of a `go.mod` that the Go adapter reads: the `go` directive, every `require` with its
//! `// indirect` marker, and every `replace` rule.
//!
//! The facts come from `go mod edit -json`, Go's own parse of the file, rather than from reading the
//! text here.
//! `go.mod` tokens may be Go string literals with any `strconv.Unquote` escape, replacement
//! directories may contain spaces inside quotes, and the indirect marker has field-level rules, so
//! only Go's parser classifies every directive the way the resolver does.

use cooldown_core::CoreError;
use serde::Deserialize;
use std::collections::HashMap;

/// One `require` directive of a `go.mod`.
pub(crate) struct Require {
    /// The required version.
    pub(crate) version: String,
    /// Whether the directive carries Go's `// indirect` marker.
    pub(crate) indirect: bool,
}

/// One `replace` directive: `path [version] => target`.
struct ReplaceRule {
    path: String,
    /// The replaced version, or `None` when the rule replaces every version.
    version: Option<String>,
    target: ModuleVersion,
}

/// The `go`, `require` and `replace` directives of a `go.mod`.
#[derive(Default)]
pub(crate) struct GoModFacts {
    /// The `go` directive's version, or `None` when the file has none.
    go_version: Option<String>,
    pub(crate) requires: HashMap<String, Require>,
    replaces: Vec<ReplaceRule>,
}

impl GoModFacts {
    /// Whether the module's `go.mod` requires every module that provides a package to its build.
    ///
    /// From `go 1.17` on, the module graph is pruned, and `go.mod` then lists every module providing
    /// a package that a package or test of the main module imports, directly or transitively.
    /// Below that, or without a `go` directive, which Go reads as `go 1.16`, a transitively imported
    /// module may be required only by the module that imports it.
    pub(crate) fn requires_every_build_module(&self) -> bool {
        self.go_version
            .as_deref()
            .is_some_and(|version| language_at_least(version, 17))
    }

    /// The replacement that serves `path` at `version`, as Go selects it: a rule for exactly that
    /// version, else a rule for every version, else none.
    fn effective_replacement(&self, path: &str, version: &str) -> Option<&ModuleVersion> {
        let rule_for = |wanted: Option<&str>| {
            self.replaces
                .iter()
                .find(|rule| rule.path == path && rule.version.as_deref() == wanted)
        };
        rule_for(Some(version))
            .or_else(|| rule_for(None))
            .map(|rule| &rule.target)
    }

    /// Whether the same `replace` rule target serves `path` at both versions, so changing the
    /// required version between them leaves the module's source where it was.
    /// Without a replacement the required version itself selects the source, so this is `false`.
    pub(crate) fn same_replacement(&self, path: &str, from: &str, to: &str) -> bool {
        self.effective_replacement(path, from)
            .is_some_and(|replacement| self.effective_replacement(path, to) == Some(replacement))
    }
}

/// The JSON `go mod edit -json` prints, reduced to the fields read here.
/// Go prints `null` for a section the file does not have.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GoModJson {
    #[serde(default)]
    go: Option<String>,
    #[serde(default)]
    require: Option<Vec<RequireJson>>,
    #[serde(default)]
    replace: Option<Vec<ReplaceJson>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RequireJson {
    path: String,
    version: String,
    #[serde(default)]
    indirect: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReplaceJson {
    old: ModuleVersion,
    new: ModuleVersion,
}

/// A module path with an optional version, as `go mod edit -json` prints both sides of a `replace`.
/// A replacement directory has no version.
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
struct ModuleVersion {
    path: String,
    #[serde(default)]
    version: Option<String>,
}

/// Reads the facts from the output of `go mod edit -json`.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] if `json` is not the shape `go mod edit -json` prints.
pub(crate) fn from_json(json: &str) -> Result<GoModFacts, CoreError> {
    let parsed: GoModJson = serde_json::from_str(json)
        .map_err(|err| CoreError::LockUnreadable(format!("go mod edit -json output: {err}")))?;
    let requires = parsed
        .require
        .unwrap_or_default()
        .into_iter()
        .map(|require| {
            (
                require.path,
                Require {
                    version: require.version,
                    indirect: require.indirect,
                },
            )
        })
        .collect();
    let replaces = parsed
        .replace
        .unwrap_or_default()
        .into_iter()
        .map(|replace| ReplaceRule {
            path: replace.old.path,
            version: replace.old.version,
            target: replace.new,
        })
        .collect();
    Ok(GoModFacts {
        go_version: parsed.go,
        requires,
        replaces,
    })
}

/// The path the `module` directive of the `go.mod` text `content` declares.
///
/// The result only ever excludes a version, so the reading is deliberately narrow: it returns a
/// path only for exactly one `module` line holding one bare or double-quoted path, optionally
/// followed by a `//` comment, and `None` for anything else, which keeps the version.
pub(crate) fn declared_module_path(content: &str) -> Option<&str> {
    let mut declared = None;
    let mut in_block = false;
    for line in content.lines() {
        let line = line.split_once("//").map_or(line, |(code, _)| code).trim();
        // A `require (` block can hold a line that starts with `module`, so blocks are skipped.
        if in_block {
            in_block = line != ")";
            continue;
        }
        if line.ends_with('(') && !line.starts_with("module") {
            in_block = true;
            continue;
        }
        let Some(argument) = line.strip_prefix("module") else {
            continue;
        };
        if !argument.starts_with([' ', '\t']) {
            continue;
        }
        if declared.is_some() {
            return None;
        }
        let argument = argument.trim();
        let path = match argument.strip_prefix('"') {
            Some(quoted) => quoted.strip_suffix('"')?,
            None => argument,
        };
        let plain = !path.is_empty()
            && !path.contains(|c: char| c.is_whitespace() || "\"`()\\".contains(c));
        if !plain {
            return None;
        }
        declared = Some(path);
    }
    declared
}

/// Whether the Go language version `version` (`1.21`, `1.21.3`, `1.21rc1`) is at least `1.<minor>`.
/// A version that does not parse is treated as older.
fn language_at_least(version: &str, minor: u32) -> bool {
    let mut parts = version.split('.');
    let major = parts.next().and_then(|major| major.parse::<u32>().ok());
    let release = parts.next().and_then(|release| {
        let digits = release
            .find(|c: char| !c.is_ascii_digit())
            .map_or(release, |end| release.get(..end).unwrap_or_default());
        digits.parse::<u32>().ok()
    });
    match (major, release) {
        (Some(major), _) if major > 1 => true,
        (Some(1), Some(release)) => release >= minor,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINNED: &str = "v0.0.0-20240101000000-aaaaaaaaaaaa";
    const OTHER_COMMIT: &str = "v0.0.0-20250101000000-bbbbbbbbbbbb";

    #[test]
    fn from_json_reads_requires_with_their_indirect_marker() -> color_eyre::eyre::Result<()> {
        // `Indirect` is omitted for a direct requirement, as `go mod edit -json` prints it.
        let facts = from_json(indoc::indoc! {r#"
            {
                "Module": {"Path": "example.com/demo"},
                "Go": "1.24",
                "Require": [
                    {"Path": "example.com/direct", "Version": "v1.4.0"},
                    {"Path": "example.com/transitive", "Version": "v0.9.1", "Indirect": true}
                ],
                "Exclude": null,
                "Replace": null,
                "Retract": null
            }
        "#})?;
        let direct = facts.requires.get("example.com/direct");
        assert_eq!(
            direct.map(|require| require.version.as_str()),
            Some("v1.4.0")
        );
        assert_eq!(direct.map(|require| require.indirect), Some(false));
        assert_eq!(
            facts
                .requires
                .get("example.com/transitive")
                .map(|require| require.indirect),
            Some(true)
        );
        Ok(())
    }

    #[test]
    fn a_version_qualified_replacement_wins_over_an_unversioned_one() -> color_eyre::eyre::Result<()>
    {
        let facts = from_json(&format!(
            r#"{{
                "Require": [{{"Path": "example.com/both", "Version": "{PINNED}"}}],
                "Replace": [
                    {{"Old": {{"Path": "example.com/both"}}, "New": {{"Path": "../default"}}}},
                    {{"Old": {{"Path": "example.com/both", "Version": "{PINNED}"}},
                      "New": {{"Path": "../pinned fork"}}}},
                    {{"Old": {{"Path": "example.com/local"}}, "New": {{"Path": "../local"}}}}
                ]
            }}"#
        ))?;
        // Moving off the exactly replaced version switches the source to the unversioned rule.
        assert!(!facts.same_replacement("example.com/both", PINNED, OTHER_COMMIT));
        // An unversioned rule alone serves every version.
        assert!(facts.same_replacement("example.com/local", PINNED, OTHER_COMMIT));
        // Without a rule the required version selects the source.
        assert!(!facts.same_replacement("example.com/none", PINNED, OTHER_COMMIT));
        Ok(())
    }

    #[test]
    fn a_pruned_go_directive_requires_every_build_module() -> color_eyre::eyre::Result<()> {
        let requires_all = |go: &str| -> color_eyre::eyre::Result<bool> {
            Ok(from_json(&format!(r#"{{"Go": {go}}}"#))?.requires_every_build_module())
        };
        // Graph pruning starts at `go 1.17`, whatever the patch or prerelease suffix.
        assert!(requires_all(r#""1.17""#)?);
        assert!(requires_all(r#""1.21.3""#)?);
        assert!(requires_all(r#""1.27rc1""#)?);
        // An older or missing directive, or one that does not parse, keeps the unpruned layout.
        assert!(!requires_all(r#""1.16""#)?);
        assert!(!requires_all("null")?);
        assert!(!requires_all(r#""go1.21""#)?);
        Ok(())
    }

    #[test]
    fn the_module_directive_names_the_declared_path() {
        // Comments, other directives and a quoted path all read the same declaration.
        let declared = declared_module_path(indoc::indoc! {r#"
            // Deprecated: use cel.dev/expr instead.
            module "cel.dev/cel-go" // moved

            go 1.22
            require example.com/modulefoo v1.0.0
        "#});
        assert_eq!(declared, Some("cel.dev/cel-go"));
        assert_eq!(
            declared_module_path("go 1.21\nmodule\texample.com/m/v2\n"),
            Some("example.com/m/v2")
        );
    }

    #[test]
    fn an_unusual_module_directive_declares_nothing() {
        // Anything but one plain path keeps the version, so the apply has the last word on it.
        for content in [
            "",
            "go 1.21\n",
            "module (\n\texample.com/m\n)\n",
            "module \"example.com/\\u006d\"\n",
            "module example.com/a example.com/b\n",
            "module example.com/a\nmodule example.com/b\n",
            "require (\n\tmodule v1.0.0\n)\n",
        ] {
            assert_eq!(declared_module_path(content), None, "{content:?}");
        }
    }

    #[test]
    fn output_that_is_not_go_mod_json_is_unreadable() {
        assert!(matches!(
            from_json("go: errors parsing go.mod"),
            Err(CoreError::LockUnreadable(_))
        ));
    }
}
