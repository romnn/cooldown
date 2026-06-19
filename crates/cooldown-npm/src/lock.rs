//! Lockfile parsers for the npm-compatible package managers. Each manager resolves from the same
//! registry but records the resolved graph in its own format; the [`NodeLock`] trait abstracts the
//! per-manager differences (lockfile name, driver binary, parse, and apply args) so a single
//! generic adapter can serve all of them.
//!
//! Every parser returns the flat list of resolved `(name, version)` pairs the lock pins. The
//! adapter intersects that list with the manifest's directly-declared names to recover the
//! direct/transitive split.

use cooldown_core::{CoreError, Result, ToolId};
use std::collections::HashSet;

/// The per-package-manager knobs the generic adapter needs: identity, the lockfile/driver it reads
/// and shells out to, how to parse its lock, and how to ask it to re-pin a dependency.
pub trait NodeLock: Send + Sync + 'static {
    /// The tool's canonical [`ToolId`] (e.g. `ToolId("npm")`).
    const ID: ToolId;
    /// The lockfile that marks a project for this manager (e.g. `package-lock.json`).
    const LOCKFILE: &'static str;
    /// The driver binary, shelled out to for apply/build (e.g. `npm`).
    const BIN: &'static str;
    /// The native cooldown config `sync` writes for this manager: pnpm bakes a `minimumReleaseAge`
    /// (minutes) into `pnpm-workspace.yaml`. `None` for managers with no native cooldown knob, whose
    /// `sync` is then `unsupported`.
    const NATIVE_MIN_AGE_FILE: Option<&'static str> = None;

    /// Parses the lockfile body into the flat list of resolved `(name, version)` pairs.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] if the lockfile cannot be parsed.
    fn parse(content: &str) -> Result<Vec<(String, String)>>;

    /// The direct-dependency names declared across the whole workspace, recovered from the lock's
    /// importer/workspace section (every member, not just the root project).
    ///
    /// Returns `None` for lock formats that don't record per-importer direct deps (yarn classic,
    /// bun); the adapter then falls back to reading the root `package.json` alone. A single-package
    /// repo is just a one-member workspace, so the lock-based answer also covers the non-workspace
    /// case.
    #[must_use]
    fn workspace_direct_names(_content: &str) -> Option<HashSet<String>> {
        None
    }

    /// The driver args that re-pin `name` to `version`, re-resolving the lock.
    fn upgrade_args(name: &str, version: &str) -> Vec<String>;

    /// The driver args that install/verify the resolved graph (the opt-in `--build` step).
    fn build_args() -> Vec<String>;
}

/// Splits a `name@version` (or scoped `@scope/name@version`) specifier into its parts. The version
/// is taken after the last `@`, so the leading `@` of a scope is preserved in the name.
pub(crate) fn split_name_version(spec: &str) -> Option<(String, String)> {
    let at = spec.rfind('@').filter(|&i| i > 0)?;
    let (name, version) = spec.split_at(at);
    Some((name.to_string(), version[1..].to_string()))
}

/// The npm package manager: `package-lock.json` (lockfile v2/v3) backed by the npm registry.
pub struct Npm;
/// The pnpm package manager: `pnpm-lock.yaml` backed by the npm registry.
pub struct Pnpm;
/// The Yarn (classic, v1) package manager: `yarn.lock` backed by the npm registry.
pub struct Yarn;
/// The Bun package manager: `bun.lock` (text lockfile) backed by the npm registry.
pub struct Bun;

impl NodeLock for Npm {
    const ID: ToolId = ToolId("npm");
    const LOCKFILE: &'static str = "package-lock.json";
    const BIN: &'static str = "npm";

    fn parse(content: &str) -> Result<Vec<(String, String)>> {
        parse_npm(content)
    }

    fn workspace_direct_names(content: &str) -> Option<HashSet<String>> {
        parse_npm_direct(content)
    }

    fn upgrade_args(name: &str, version: &str) -> Vec<String> {
        // `--package-lock-only` re-resolves the lock (and manifest pin) without touching
        // node_modules, keeping apply fast and side-effect-light.
        vec![
            "install".into(),
            format!("{name}@{version}"),
            "--package-lock-only".into(),
            "--no-audit".into(),
            "--no-fund".into(),
        ]
    }

    fn build_args() -> Vec<String> {
        vec!["install".into(), "--no-audit".into(), "--no-fund".into()]
    }
}

impl NodeLock for Pnpm {
    const ID: ToolId = ToolId("pnpm");
    const LOCKFILE: &'static str = "pnpm-lock.yaml";
    const BIN: &'static str = "pnpm";
    const NATIVE_MIN_AGE_FILE: Option<&'static str> = Some("pnpm-workspace.yaml");

    fn parse(content: &str) -> Result<Vec<(String, String)>> {
        Ok(parse_pnpm(content))
    }

    fn workspace_direct_names(content: &str) -> Option<HashSet<String>> {
        Some(parse_pnpm_importers(content))
    }

    fn upgrade_args(name: &str, version: &str) -> Vec<String> {
        vec![
            "add".into(),
            format!("{name}@{version}"),
            "--lockfile-only".into(),
        ]
    }

    fn build_args() -> Vec<String> {
        vec!["install".into()]
    }
}

impl NodeLock for Yarn {
    const ID: ToolId = ToolId("yarn");
    const LOCKFILE: &'static str = "yarn.lock";
    const BIN: &'static str = "yarn";

    fn parse(content: &str) -> Result<Vec<(String, String)>> {
        Ok(parse_yarn(content))
    }

    fn upgrade_args(name: &str, version: &str) -> Vec<String> {
        vec!["add".into(), format!("{name}@{version}")]
    }

    fn build_args() -> Vec<String> {
        vec!["install".into()]
    }
}

impl NodeLock for Bun {
    const ID: ToolId = ToolId("bun");
    const LOCKFILE: &'static str = "bun.lock";
    const BIN: &'static str = "bun";

    fn parse(content: &str) -> Result<Vec<(String, String)>> {
        parse_bun(content)
    }

    fn upgrade_args(name: &str, version: &str) -> Vec<String> {
        vec!["add".into(), format!("{name}@{version}")]
    }

    fn build_args() -> Vec<String> {
        vec!["install".into()]
    }
}

/// Parses `package-lock.json` (lockfileVersion 2/3): the flat `packages` map keys every install
/// path (`node_modules/<name>`, possibly nested) to a record carrying its resolved `version`. The
/// v1 `dependencies` tree is handled as a fallback for older locks.
fn parse_npm(content: &str) -> Result<Vec<(String, String)>> {
    let doc: serde_json::Value = serde_json::from_str(content)
        .map_err(|e| CoreError::Parse(format!("package-lock.json: {e}")))?;
    let mut out = Vec::new();
    if let Some(packages) = doc.get("packages").and_then(|v| v.as_object()) {
        for (key, val) in packages {
            // The root project is keyed by the empty string; skip it.
            let Some(name) = key.rsplit("node_modules/").next().filter(|s| !s.is_empty()) else {
                continue;
            };
            if let Some(version) = val.get("version").and_then(|v| v.as_str()) {
                out.push((name.to_string(), version.to_string()));
            }
        }
    } else if let Some(deps) = doc.get("dependencies").and_then(|v| v.as_object()) {
        for (name, val) in deps {
            if let Some(version) = val.get("version").and_then(|v| v.as_str()) {
                out.push((name.clone(), version.to_string()));
            }
        }
    }
    Ok(out)
}

/// Parses `pnpm-lock.yaml` (v9): the top-level `packages:` section keys every resolved package by
/// its `name@version(...peers)` identity. We read those keys directly — line by line — rather than
/// pulling in a YAML dependency, since the keys are the only field we need.
fn parse_pnpm(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_packages = false;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue; // blank lines punctuate the file but never end a section
        }
        if let Some(stripped) = line.strip_prefix("  ") {
            if !in_packages || stripped.starts_with(' ') {
                continue; // outside the section, or a nested field of a package entry
            }
            let key = stripped
                .trim_end()
                .trim_end_matches(':')
                .trim_matches('\'')
                .trim_matches('"');
            // Drop the `(peer@x)` suffix pnpm appends to disambiguate peer resolutions.
            let key = key.split('(').next().unwrap_or(key);
            if let Some((name, version)) = split_name_version(key) {
                out.push((name, version));
            }
        } else {
            // A non-indented line begins a new top-level section; we only want `packages:`.
            in_packages = line.starts_with("packages:");
        }
    }
    out
}

/// The dependency-group keys a manifest/importer uses to declare a direct dependency.
const DIRECT_GROUPS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

/// Collects the direct-dependency names every workspace importer declares in `pnpm-lock.yaml` (v9).
///
/// The `importers:` section maps each workspace package — the root `.` and every member path — to
/// its dependency groups, each a map of `name: {specifier, version}`. The union of those names is
/// the workspace-wide direct set; the resolved versions still come from `packages:`. Internal
/// `workspace:*` deps resolve to `link:`/`file:` and never appear among the registry packages, so
/// their names drop out when the adapter intersects this set with the resolved graph.
///
/// Parsed line-by-line (like [`parse_pnpm`]) by indentation, avoiding a YAML dependency: importer
/// paths sit at 2 spaces, group keys at 4, dependency names at 6, and their fields at 8.
fn parse_pnpm_importers(content: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut in_importers = false;
    let mut in_group = false;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        match indent {
            0 => {
                in_importers = trimmed == "importers:";
                in_group = false;
            }
            2 if in_importers => in_group = false, // a new importer (member path)
            4 if in_importers => in_group = DIRECT_GROUPS.contains(&trimmed.trim_end_matches(':')),
            6 if in_importers && in_group => {
                let name = trimmed
                    .trim_end_matches(':')
                    .trim_matches('\'')
                    .trim_matches('"');
                if !name.is_empty() {
                    names.insert(name.to_string());
                }
            }
            _ => {}
        }
    }
    names
}

/// Collects direct-dependency names from `package-lock.json` (v2/v3). The flat `packages` map keys
/// the root project by `""` and each workspace member by its path; installed dependencies live under
/// `node_modules/`. Every non-`node_modules` entry declares its direct deps in the dependency-group
/// fields, so their union is the workspace-wide direct set. Returns `None` for a v1 lock (no
/// `packages` map) so the caller falls back to the root manifest.
fn parse_npm_direct(content: &str) -> Option<HashSet<String>> {
    let doc: serde_json::Value = serde_json::from_str(content).ok()?;
    let packages = doc.get("packages")?.as_object()?;
    let mut names = HashSet::new();
    for (key, entry) in packages {
        if key.contains("node_modules/") {
            continue; // an installed (transitive or hoisted) package, not a workspace member
        }
        for field in DIRECT_GROUPS {
            if let Some(obj) = entry.get(field).and_then(serde_json::Value::as_object) {
                names.extend(obj.keys().cloned());
            }
        }
    }
    Some(names)
}

/// Parses a classic (v1) `yarn.lock`: each entry is one or more comma-separated `name@range`
/// specifiers ending in `:`, followed by an indented `version "x.y.z"` line that resolves them.
fn parse_yarn(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("  version ") {
            let version = rest.trim().trim_matches('"');
            for name in pending.drain(..) {
                out.push((name, version.to_string()));
            }
        } else if !line.starts_with([' ', '#']) && line.trim_end().ends_with(':') {
            let key = line.trim_end().trim_end_matches(':');
            // One entry can list several ranges for the same name (`foo@^1, foo@~1.2`); they all
            // resolve to one version, so collapse them to a single name.
            pending = key
                .split(',')
                .filter_map(|spec| {
                    let spec = spec.trim().trim_matches('"');
                    let at = spec.rfind('@').filter(|&i| i > 0)?;
                    Some(spec[..at].to_string())
                })
                .fold(Vec::new(), |mut acc, name| {
                    if !acc.contains(&name) {
                        acc.push(name);
                    }
                    acc
                });
        }
    }
    out
}

/// Parses `bun.lock`: a JSONC document whose `packages` map values are arrays of the form
/// `["name@version", registry, {...}, integrity]`. Bun writes trailing commas (valid JSONC but not
/// JSON), so the body is normalised before handing it to the JSON parser.
fn parse_bun(content: &str) -> Result<Vec<(String, String)>> {
    let normalised = strip_trailing_commas(content);
    let doc: serde_json::Value = serde_json::from_str(&normalised)
        .map_err(|e| CoreError::Parse(format!("bun.lock: {e}")))?;
    let mut out = Vec::new();
    if let Some(packages) = doc.get("packages").and_then(|v| v.as_object()) {
        for val in packages.values() {
            if let Some(spec) = val.get(0).and_then(|v| v.as_str())
                && let Some((name, version)) = split_name_version(spec)
            {
                out.push((name, version));
            }
        }
    }
    Ok(out)
}

/// Removes JSON-invalid trailing commas (a comma whose next non-whitespace character closes an
/// object or array). String contents are left untouched, so a comma inside a quoted value is never
/// mistaken for a structural one.
fn strip_trailing_commas(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escaped = false;
    // A comma is buffered (with any following whitespace) until we know whether it is structural or
    // a trailing comma to be dropped.
    let mut pending_comma = false;
    let mut pending_ws = String::new();
    let flush = |out: &mut String, comma: &mut bool, ws: &mut String| {
        if *comma {
            out.push(',');
            *comma = false;
        }
        out.push_str(ws);
        ws.clear();
    };
    for c in s.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            ',' => {
                flush(&mut out, &mut pending_comma, &mut pending_ws);
                pending_comma = true;
            }
            '}' | ']' => {
                pending_comma = false; // drop a trailing comma before the closer
                out.push_str(&pending_ws);
                pending_ws.clear();
                out.push(c);
            }
            c if c.is_whitespace() => pending_ws.push(c),
            '"' => {
                flush(&mut out, &mut pending_comma, &mut pending_ws);
                in_string = true;
                out.push(c);
            }
            _ => {
                flush(&mut out, &mut pending_comma, &mut pending_ws);
                out.push(c);
            }
        }
    }
    flush(&mut out, &mut pending_comma, &mut pending_ws);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut v: Vec<(String, String)>) -> Vec<(String, String)> {
        v.sort();
        v
    }

    #[test]
    fn splits_scoped_and_plain_specifiers() {
        assert_eq!(
            split_name_version("lodash@4.17.15"),
            Some(("lodash".into(), "4.17.15".into()))
        );
        assert_eq!(
            split_name_version("@babel/core@7.1.0"),
            Some(("@babel/core".into(), "7.1.0".into()))
        );
        assert_eq!(split_name_version("no-version"), None);
    }

    #[test]
    fn npm_packages_map() {
        let lock = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root", "version": "0.1.0" },
                "node_modules/lodash": { "version": "4.17.15" },
                "node_modules/@babel/core": { "version": "7.1.0" },
                "node_modules/a/node_modules/b": { "version": "2.0.0" }
            }
        }"#;
        assert_eq!(
            sorted(parse_npm(lock).unwrap()),
            sorted(vec![
                ("lodash".into(), "4.17.15".into()),
                ("@babel/core".into(), "7.1.0".into()),
                ("b".into(), "2.0.0".into()),
            ])
        );
    }

    #[test]
    fn pnpm_packages_section() {
        let lock = "lockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      lodash:\n        specifier: 4.17.15\n        version: 4.17.15\n\npackages:\n\n  lodash@4.17.15:\n    resolution: {integrity: sha512-x}\n\n  '@babel/core@7.1.0':\n    resolution: {integrity: sha512-y}\n\n  chalk@4.0.0(supports-color@7.2.0):\n    resolution: {integrity: sha512-z}\n";
        assert_eq!(
            sorted(parse_pnpm(lock)),
            sorted(vec![
                ("lodash".into(), "4.17.15".into()),
                ("@babel/core".into(), "7.1.0".into()),
                ("chalk".into(), "4.0.0".into()),
            ])
        );
    }

    #[test]
    fn yarn_classic_entries() {
        let lock = "# THIS IS AN AUTOGENERATED FILE.\n\n\nlodash@^4.17.0, lodash@~4.17.15:\n  version \"4.17.15\"\n  resolved \"https://x\"\n\n\"@babel/core@^7.0.0\":\n  version \"7.1.0\"\n  resolved \"https://y\"\n";
        assert_eq!(
            sorted(parse_yarn(lock)),
            sorted(vec![
                ("lodash".into(), "4.17.15".into()),
                ("@babel/core".into(), "7.1.0".into()),
            ])
        );
    }

    #[test]
    fn bun_text_lock_with_trailing_commas() {
        let lock = r#"{
            "lockfileVersion": 1,
            "packages": {
                "lodash": ["lodash@4.17.15", "", {}, "sha512-x"],
                "@babel/core": ["@babel/core@7.1.0", "", {}, "sha512-y"],
            },
        }"#;
        assert_eq!(
            sorted(parse_bun(lock).unwrap()),
            sorted(vec![
                ("lodash".into(), "4.17.15".into()),
                ("@babel/core".into(), "7.1.0".into()),
            ])
        );
    }

    #[test]
    fn strip_trailing_commas_preserves_string_commas() {
        let input = r#"{ "a": "x,y", "b": [1, 2,], }"#;
        assert_eq!(
            strip_trailing_commas(input),
            r#"{ "a": "x,y", "b": [1, 2] }"#
        );
    }

    fn sorted_names(set: HashSet<String>) -> Vec<String> {
        let mut v: Vec<String> = set.into_iter().collect();
        v.sort();
        v
    }

    #[test]
    fn pnpm_importers_collect_every_member_direct_dep() {
        // A workspace: root `.` plus two members, mixing dependency groups, a `workspace:*` internal
        // link, and a `dependenciesMeta` table (not a dependency group — must be ignored).
        let lock = "\
lockfileVersion: '9.0'

importers:

  .:
    devDependencies:
      prettier:
        specifier: 3.8.0
        version: 3.8.0
      turbo:
        specifier: 2.9.16
        version: 2.9.16

  apps/admin:
    dependencies:
      '@airtype/api':
        specifier: workspace:*
        version: link:../../packages/ts/airtype/api
      solid-js:
        specifier: 1.9.5
        version: 1.9.5
    devDependencies:
      vite:
        specifier: 6.0.0
        version: 6.0.0
    dependenciesMeta:
      '@airtype/api':
        injected: true

  packages/ts/api:
    optionalDependencies:
      fsevents:
        specifier: 2.3.3
        version: 2.3.3

packages:

  prettier@3.8.0:
    resolution: {integrity: sha512-x}
";
        // Every member's direct deps are collected (the internal `@airtype/api` link is included by
        // name; it falls away later when intersected with the resolved registry packages). Group
        // keys, the `specifier`/`version` fields, and `dependenciesMeta` never leak in as names.
        assert_eq!(
            sorted_names(parse_pnpm_importers(lock)),
            vec![
                "@airtype/api",
                "fsevents",
                "prettier",
                "solid-js",
                "turbo",
                "vite",
            ]
        );
    }

    #[test]
    fn pnpm_single_package_importer_is_a_one_member_workspace() {
        let lock = "\
importers:

  .:
    dependencies:
      lodash:
        specifier: 4.17.15
        version: 4.17.15

packages:

  lodash@4.17.15:
    resolution: {integrity: sha512-x}
";
        assert_eq!(sorted_names(parse_pnpm_importers(lock)), vec!["lodash"]);
    }

    #[test]
    fn npm_direct_collects_root_and_member_deps() {
        let lock = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root", "devDependencies": { "turbo": "^2" } },
                "packages/api": { "name": "@x/api", "dependencies": { "zod": "^3" } },
                "node_modules/zod": { "version": "3.22.0" },
                "node_modules/turbo": { "version": "2.9.16" }
            }
        }"#;
        assert_eq!(
            sorted_names(parse_npm_direct(lock).expect("v3 lock has a packages map")),
            vec!["turbo", "zod"]
        );
    }

    #[test]
    fn npm_direct_is_none_for_v1_lock() {
        // A v1 lock has no `packages` map, so there is no workspace direct set to read; the adapter
        // falls back to the root manifest.
        let lock = r#"{ "lockfileVersion": 1, "dependencies": { "lodash": { "version": "4.17.15" } } }"#;
        assert!(parse_npm_direct(lock).is_none());
    }
}
