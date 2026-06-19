//! Lockfile parsers for the npm-compatible package managers. Each manager resolves from the same
//! registry but records the resolved graph in its own format; the [`NodeLock`] trait abstracts the
//! per-manager differences (lockfile name, driver binary, parse, and apply args) so a single
//! generic adapter can serve all of them.
//!
//! Every parser returns the flat list of resolved `(name, version)` pairs the lock pins. Where the
//! lock records importer/member declarations (npm v2/v3, pnpm), the adapter uses that same data for
//! both direct/transitive classification and source attribution; older formats fall back to the root
//! manifest's declared dependency names.

use cooldown_core::{CoreError, Result, ToolId};
use std::collections::{HashMap, HashSet};

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

    /// The workspace member package(s) that declare each dependency, for attributing a dependency to
    /// its source package(s) in reports. Default: empty (yarn classic and bun record no per-member
    /// data in their locks, so their `members` column stays blank).
    #[must_use]
    fn member_sources(_content: &str) -> MemberIndex {
        MemberIndex::default()
    }

    /// The driver args that re-pin `name` to `version`, re-resolving the lock.
    fn upgrade_args(name: &str, version: &str) -> Vec<String>;

    /// The driver args that install/verify the resolved graph (the opt-in `--build` step).
    fn build_args() -> Vec<String>;
}

/// Maps a resolved dependency to the workspace member packages that declare it.
///
/// pnpm records the resolved version per importer, so its entries are keyed exactly by
/// `(name, version)`. npm records only version ranges per member, not the resolved version, so its
/// entries are keyed by name and apply to every resolved version of that name.
#[derive(Debug, Default)]
pub struct MemberIndex {
    by_version: HashMap<(String, String), Vec<String>>,
    by_name: HashMap<String, Vec<String>>,
    authoritative: bool,
}

impl MemberIndex {
    fn version_exact(by_version: HashMap<(String, String), Vec<String>>) -> Self {
        Self {
            by_version,
            by_name: HashMap::new(),
            authoritative: true,
        }
    }

    fn name_only(by_name: HashMap<String, Vec<String>>) -> Self {
        Self {
            by_version: HashMap::new(),
            by_name,
            authoritative: true,
        }
    }

    /// Whether this lock carries authoritative importer/member data for classifying direct deps.
    #[must_use]
    pub fn is_authoritative(&self) -> bool {
        self.authoritative
    }

    /// Every distinct member path recorded in the index, for resolving paths to package names once.
    #[must_use]
    pub fn all_paths(&self) -> HashSet<String> {
        self.by_version
            .values()
            .flatten()
            .chain(self.by_name.values().flatten())
            .cloned()
            .collect()
    }

    /// The member packages declaring `name` at `version`, sorted and deduplicated. Empty when the
    /// lock carries no per-member attribution for this dependency.
    #[must_use]
    pub fn members_for(&self, name: &str, version: &str) -> Vec<String> {
        let mut members: Vec<String> = self
            .by_version
            .get(&(name.to_string(), version.to_string()))
            .into_iter()
            .flatten()
            .chain(self.by_name.get(name).into_iter().flatten())
            .cloned()
            .collect();
        members.sort();
        members.dedup();
        members
    }
}

/// Splits a `name@version` (or scoped `@scope/name@version`) specifier into its parts. The version
/// is taken after the last `@`, so the leading `@` of a scope is preserved in the name.
pub(crate) fn split_name_version(spec: &str) -> Option<(String, String)> {
    let at = spec.rfind('@').filter(|&i| i > 0)?;
    let (name, version) = spec.split_at(at);
    Some((name.to_string(), version[1..].to_string()))
}

fn unquote_yaml_scalar(value: &str) -> &str {
    value.trim().trim_matches('\'').trim_matches('"')
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

    fn member_sources(content: &str) -> MemberIndex {
        parse_npm_member_sources(content)
            .map(MemberIndex::name_only)
            .unwrap_or_default()
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

    fn member_sources(content: &str) -> MemberIndex {
        MemberIndex::version_exact(parse_pnpm_importer_members(content))
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
            let key = unquote_yaml_scalar(stripped.trim_end().trim_end_matches(':'));
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

/// Maps each resolved `(name, version)` dependency to the workspace member importers that declare it,
/// read from `pnpm-lock.yaml`'s `importers:` section. The resolved `version:` line under each
/// dependency gives the exact version (its `(peer)` suffix stripped to match the `packages:` keys);
/// internal `link:`/`file:`/`workspace:` versions are skipped — they are not registry packages.
/// Importer paths (the workspace root is `.`) name the source packages.
fn parse_pnpm_importer_members(content: &str) -> HashMap<(String, String), Vec<String>> {
    let mut map: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut in_importers = false;
    let mut member: Option<String> = None;
    let mut in_group = false;
    let mut dep_name: Option<String> = None;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        match indent {
            0 => {
                in_importers = trimmed == "importers:";
                member = None;
                in_group = false;
                dep_name = None;
            }
            2 if in_importers => {
                member = Some(unquote_yaml_scalar(trimmed.trim_end_matches(':')).to_string());
                in_group = false;
                dep_name = None;
            }
            4 if in_importers => {
                in_group = DIRECT_GROUPS.contains(&trimmed.trim_end_matches(':'));
                dep_name = None;
            }
            6 if in_importers && in_group => {
                let name = unquote_yaml_scalar(trimmed.trim_end_matches(':'));
                dep_name = (!name.is_empty()).then(|| name.to_string());
            }
            8 if in_importers && in_group => {
                if let (Some(member), Some(name)) = (member.as_ref(), dep_name.as_ref())
                    && let Some(raw) = trimmed.strip_prefix("version:")
                {
                    let value = unquote_yaml_scalar(raw);
                    if !value.starts_with("link:")
                        && !value.starts_with("file:")
                        && !value.starts_with("workspace:")
                    {
                        // Strip the `(peer@x)` suffix so the version matches the `packages:` keys.
                        let version = unquote_yaml_scalar(value.split('(').next().unwrap_or(value));
                        if !version.is_empty() {
                            map.entry((name.clone(), version.to_string()))
                                .or_default()
                                .push(member.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    map
}

/// Maps each dependency name to the workspace member packages that declare it, read from
/// `package-lock.json`'s `packages` map. Member entries — the root `""` and any key not under
/// `node_modules/` — list their direct deps as ranges, not resolved versions, so attribution is by
/// name (applied to every resolved version of that name). Members are keyed by their workspace path
/// (the root as `.`), matching pnpm's importer paths.
fn parse_npm_member_sources(content: &str) -> Option<HashMap<String, Vec<String>>> {
    let doc = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let packages = doc.get("packages")?.as_object()?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (key, entry) in packages {
        if key.contains("node_modules/") {
            continue;
        }
        let member = if key.is_empty() { "." } else { key.as_str() };
        for field in DIRECT_GROUPS {
            if let Some(obj) = entry.get(field).and_then(serde_json::Value::as_object) {
                for name in obj.keys() {
                    map.entry(name.clone())
                        .or_default()
                        .push(member.to_string());
                }
            }
        }
    }
    Some(map)
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

    #[test]
    fn pnpm_importer_members_attributes_by_resolved_version() {
        // The same dep at different versions across importers must attribute to the right members;
        // a `(peer)` suffix is stripped, and an internal `workspace:*` link is excluded.
        let lock = "\
importers:

  apps/a:
    dependencies:
      vite:
        specifier: 6.0.0
        version: 6.0.0

  apps/b:
    dependencies:
      vite:
        specifier: 7.0.0
        version: 7.0.0(typescript@5.4.5)

  packages/x:
    dependencies:
      vite:
        specifier: 6.0.0
        version: 6.0.0
      '@airtype/api':
        specifier: workspace:*
        version: link:../api

packages:

  vite@6.0.0:
    resolution: {integrity: sha512-x}
";
        let index = MemberIndex::version_exact(parse_pnpm_importer_members(lock));
        assert_eq!(
            index.members_for("vite", "6.0.0"),
            vec!["apps/a", "packages/x"]
        );
        assert_eq!(index.members_for("vite", "7.0.0"), vec!["apps/b"]);
        // The internal workspace link is not a registry package, so it is never attributed.
        assert!(index.members_for("@airtype/api", "0.0.0").is_empty());
    }

    #[test]
    fn pnpm_importer_members_unquotes_yaml_scalars() {
        let lock = "\
importers:

  'apps/a':
    dependencies:
      '@scope/pkg':
        specifier: '^1.2.3'
        version: '1.2.3(react@19.0.0)'

packages:

  '@scope/pkg@1.2.3':
    resolution: {integrity: sha512-x}
";
        let index = MemberIndex::version_exact(parse_pnpm_importer_members(lock));

        assert_eq!(index.members_for("@scope/pkg", "1.2.3"), vec!["apps/a"]);
    }

    #[test]
    fn npm_member_sources_attributes_by_name() {
        let lock = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "devDependencies": { "turbo": "^2" } },
                "packages/api": { "dependencies": { "zod": "^3" } },
                "node_modules/zod": { "version": "3.22.0" }
            }
        }"#;
        let index =
            MemberIndex::name_only(parse_npm_member_sources(lock).expect("v3 lock has members"));
        // The root is keyed as `.`; a member by its workspace path. Range-only locks attribute by
        // name, so any resolved version of `zod` maps to its declaring member.
        assert_eq!(index.members_for("turbo", "2.9.16"), vec!["."]);
        assert_eq!(index.members_for("zod", "3.22.0"), vec!["packages/api"]);
    }

    #[test]
    fn npm_member_sources_are_absent_for_v1_lock() {
        // A v1 lock has no `packages` map, so direct-ness falls back to the root manifest.
        let lock =
            r#"{ "lockfileVersion": 1, "dependencies": { "lodash": { "version": "4.17.15" } } }"#;
        assert!(parse_npm_member_sources(lock).is_none());
        assert!(!Npm::member_sources(lock).is_authoritative());
    }

    #[test]
    fn member_index_is_empty_by_default() {
        // yarn/bun and the unparsable case: no attribution, so the column stays blank.
        let index = MemberIndex::default();
        assert!(index.members_for("anything", "1.0.0").is_empty());
    }
}
