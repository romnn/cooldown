//! The age gate Yarn 4.17 and later apply to each configured `npmScopes` entry.
//!
//! Yarn fills every setting a scope entry leaves out with its default, so a configured scope is
//! gated by its own `npmMinimalAgeGate`, or by the one-day default, never by the global gate
//! (`getMinimalAgeGate` in [npmConfigUtils.ts][npm-config-utils], `parseShape` in
//! [Configuration.ts][configuration]).
//! Entries from every rc file Yarn reads merge key by key, and the most specific file setting a
//! scope's gate decides it ([configUtils.ts][config-utils]).
//! A value that is no mapping, an empty entry or an empty `npmScopes` included, stops the merge:
//! the less specific files' entries below it count no more.
//!
//! [npm-config-utils]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/plugin-npm/sources/npmConfigUtils.ts
//! [configuration]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/Configuration.ts
//! [config-utils]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/configUtils.ts

use crate::manifest::workspaces::{YarnEnvironment, rc_files, read_settings};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The gate, in minutes, Yarn gives a configured scope whose entries set none.
pub(super) const DEFAULT_SCOPE_GATE_MINUTES: f64 = 1440.0;

/// The `onConflict` key that makes Yarn read a mapping's `value` in its place.
const CONFLICT_KEY: &str = "onConflict";

/// The age gate Yarn applies to one configured scope.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ScopeGate {
    /// The gate in minutes, or `None` when cooldown cannot read the value Yarn would.
    pub(super) minutes: Option<f64>,
    /// Whether an entry sets the gate, rather than leaving Yarn's default.
    pub(super) explicit: bool,
    /// The rc file whose entry sets the gate, or the most specific one configuring the scope.
    pub(super) source: Utf8PathBuf,
}

/// The gate of every scope the rc files around a project configure, and why any of them could
/// not be read.
#[derive(Debug, Default)]
pub(super) struct ScopeGates {
    /// The gates by scope name, without its `@`.
    pub(super) gates: BTreeMap<String, ScopeGate>,
    /// Why some scope's gate could not be read, one reason per occurrence.
    pub(super) unreadable: Vec<String>,
}

/// Reads the scope gates of every rc file Yarn reads for the project at `root`, whose own rc file
/// `project_rc` holds `project`.
pub(super) fn scope_gates(
    root: &Utf8Path,
    project_rc: &Utf8Path,
    project: &Map<String, Value>,
    environment: &YarnEnvironment,
) -> ScopeGates {
    let mut gates = ScopeGates::default();
    let files = if let Some(home) = &environment.home {
        rc_files(root, home, environment)
    } else {
        gates.unreadable.push(
            "no home directory is established, so the rc file Yarn reads there is unknown"
                .to_string(),
        );
        vec![project_rc.to_path_buf()]
    };
    // The scopes whose entry no less specific file can extend any more.
    let mut sealed = BTreeSet::new();
    // The most specific file comes first, so the first gate found for a scope is Yarn's.
    for file in files {
        let settings = if file == project_rc {
            project.clone()
        } else {
            match read_settings(root, &file) {
                Ok(Some(settings)) => settings,
                Ok(None) => continue,
                Err(reason) => {
                    gates.unreadable.push(reason);
                    continue;
                }
            }
        };
        let shown = super::shown(root, &file);
        if settings.contains_key(CONFLICT_KEY) {
            gates.unreadable.push(format!(
                "{shown} wraps its settings in `onConflict`, which changes what Yarn inherits"
            ));
            continue;
        }
        let scopes = match settings.get(super::SCOPES_KEY) {
            None => continue,
            Some(Value::Object(scopes)) if !scopes.contains_key(CONFLICT_KEY) => scopes,
            // An empty `npmScopes` is a value of its own, which hides every less specific file's.
            Some(Value::Null) => break,
            Some(_) => {
                gates.unreadable.push(format!(
                    "`npmScopes` in {shown} is no plain mapping of scopes"
                ));
                break;
            }
        };
        for (scope, entry) in scopes {
            if sealed.contains(scope) {
                continue;
            }
            let gate = match entry {
                Value::Object(entry) if !entry.contains_key(CONFLICT_KEY) => {
                    entry.get(super::GATE_KEY)
                }
                // An empty entry resets the scope to Yarn's defaults, so no less specific entry
                // for it counts, as no mapping does below a value of another kind.
                Value::Null => {
                    sealed.insert(scope.clone());
                    None
                }
                _ => {
                    gates.unreadable.push(format!(
                        "`npmScopes.{scope}` in {shown} is no plain mapping of settings"
                    ));
                    sealed.insert(scope.clone());
                    continue;
                }
            };
            let found = gates
                .gates
                .entry(scope.clone())
                .or_insert_with(|| ScopeGate {
                    minutes: Some(DEFAULT_SCOPE_GATE_MINUTES),
                    explicit: false,
                    source: file.clone(),
                });
            if let Some(gate) = gate
                && !found.explicit
            {
                *found = ScopeGate {
                    minutes: duration_minutes(gate),
                    explicit: true,
                    source: file.clone(),
                };
            }
        }
    }
    gates
}

/// The minutes a duration setting holds, read as Yarn 4.11 and later read it, or `None` when
/// cooldown cannot tell.
///
/// A bare number counts minutes, and a number may carry one of the units `ms`, `s`, `m`, `h`,
/// `d` and `w` (`parseDuration` in [miscUtils.ts][misc-utils]).
/// A value with `${…}` depends on the environment Yarn runs in.
///
/// [misc-utils]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/miscUtils.ts
pub(super) fn duration_minutes(value: &Value) -> Option<f64> {
    let text = match value {
        Value::Number(number) => return number.as_f64().filter(|minutes| *minutes >= 0.0),
        Value::String(text) => text.as_str(),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => return None,
    };
    let number_end = text
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(number_end);
    // Yarn's pattern is `\d*\.?\d+`: at most one point, and a digit after it.
    if number.matches('.').count() > 1
        || !number.ends_with(|character: char| character.is_ascii_digit())
    {
        return None;
    }
    let number: f64 = number.parse().ok()?;
    let minutes_per_unit = match unit {
        "" | "m" => 1.0,
        "ms" => 1.0 / 60_000.0,
        "s" => 1.0 / 60.0,
        "h" => 60.0,
        "d" => 1440.0,
        "w" => 10_080.0,
        _ => return None,
    };
    Some(number * minutes_per_unit)
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_SCOPE_GATE_MINUTES, ScopeGate, duration_minutes, scope_gates};
    use crate::manifest::workspaces::YarnEnvironment;
    use camino::Utf8PathBuf;
    use color_eyre::eyre;
    use serde_json::{Value, json};

    /// A gate reads as Yarn's duration parser reads it: a bare number counts minutes, and a
    /// unit converts.
    #[test]
    fn gates_read_as_yarn_durations() {
        let read = |value: Value| duration_minutes(&value);
        assert_eq!(read(json!(10080)), Some(10080.0));
        assert_eq!(read(json!("10080")), Some(10080.0));
        assert_eq!(read(json!("7d")), Some(10080.0));
        assert_eq!(read(json!("1w")), Some(10080.0));
        assert_eq!(read(json!("1.5h")), Some(90.0));
        assert_eq!(read(json!(".5d")), Some(720.0));
        assert_eq!(read(json!("90s")), Some(1.5));
        for value in [
            json!("7 days"),
            json!("1.d"),
            json!("1.2.3"),
            json!("${GATE}"),
            json!("-5"),
            json!(true),
            json!(null),
        ] {
            assert_eq!(read(value.clone()), None, "{value}");
        }
    }

    /// The most specific rc file that sets a scope's gate decides it, a scope no file gives a
    /// gate keeps Yarn's default, and a wrapped entry is reported instead of read.
    #[test]
    fn the_most_specific_gate_decides_each_scope() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let top = Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let root = top.join("project");
        std::fs::create_dir_all(&root)?;
        let home = top.join("home");
        std::fs::create_dir_all(&home)?;
        std::fs::write(
            home.join(".yarnrc.yml"),
            "npmScopes:\n  corp:\n    npmMinimalAgeGate: 3d\n  home-only:\n    npmAlwaysAuth: true\n",
        )?;
        std::fs::write(
            top.join(".yarnrc.yml"),
            "npmScopes:\n  wrapped:\n    onConflict: reset\n    value: {}\n",
        )?;
        let project_rc = root.join(".yarnrc.yml");
        let project = json!({
            "npmScopes": {
                "corp": { "npmRegistryServer": "https://npm.corp.example" },
                "own": { "npmMinimalAgeGate": "10080" },
            }
        });
        let Value::Object(project) = project else {
            eyre::bail!("the project settings are a mapping");
        };
        let environment = YarnEnvironment {
            home: Some(home.clone()),
            ..YarnEnvironment::default()
        };

        let gates = scope_gates(&root, &project_rc, &project, &environment);

        // The home file's gate for `corp` applies beneath the project's routing-only entry.
        assert_eq!(
            gates.gates.get("corp"),
            Some(&ScopeGate {
                minutes: Some(4320.0),
                explicit: true,
                source: home.join(".yarnrc.yml"),
            })
        );
        assert_eq!(
            gates.gates.get("own"),
            Some(&ScopeGate {
                minutes: Some(10080.0),
                explicit: true,
                source: project_rc.clone(),
            })
        );
        // A scope configured without a gate anywhere has Yarn's one-day default.
        assert_eq!(
            gates.gates.get("home-only"),
            Some(&ScopeGate {
                minutes: Some(DEFAULT_SCOPE_GATE_MINUTES),
                explicit: false,
                source: home.join(".yarnrc.yml"),
            })
        );
        assert!(!gates.gates.contains_key("wrapped"));
        assert_eq!(gates.unreadable.len(), 1, "{:?}", gates.unreadable);
        assert!(
            gates
                .unreadable
                .iter()
                .all(|reason| reason.contains("npmScopes.wrapped"))
        );

        // An empty entry resets its scope to the defaults, hiding the home file's gate, and an
        // empty `npmScopes` hides every scope the home file configures.
        let Value::Object(reset) = json!({ "npmScopes": { "corp": null } }) else {
            eyre::bail!("the project settings are a mapping");
        };
        let gates = scope_gates(&root, &project_rc, &reset, &environment);
        assert_eq!(
            gates.gates.get("corp"),
            Some(&ScopeGate {
                minutes: Some(DEFAULT_SCOPE_GATE_MINUTES),
                explicit: false,
                source: project_rc.clone(),
            })
        );
        let Value::Object(hidden) = json!({ "npmScopes": null }) else {
            eyre::bail!("the project settings are a mapping");
        };
        let gates = scope_gates(&root, &project_rc, &hidden, &environment);
        assert!(gates.gates.is_empty(), "{:?}", gates.gates);
        Ok(())
    }
}
