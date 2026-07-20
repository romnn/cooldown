//! Lockfile parsers for the npm-compatible package managers. Each manager resolves from the same
//! registry but records the resolved graph in its own format; the [`NodeLock`] trait abstracts the
//! per-manager differences (lockfile name, driver binary, parse, and lock refresh args) so a single
//! generic adapter can serve all of them.
//!
//! Every parser returns the flat list of resolved `(name, version)` pairs the lock pins. Where the
//! lock records importer/member declarations (npm v2/v3, pnpm), the adapter uses that same data for
//! both direct/transitive classification and source attribution; older formats fall back to the root
//! manifest's declared dependency names.

use cooldown_core::{CoreError, Result, ToolId};
use std::collections::{HashMap, HashSet};

/// The per-package-manager knobs the generic adapter needs: identity, the lockfile/driver it reads
/// and shells out to, how to parse its lock, and how to refresh the lock after a manifest edit.
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

    /// The driver args that refresh the lock after cooldown has rewritten the declaring
    /// `package.json` range itself.
    ///
    /// `before` is the project's absolute publish-time cutoff when the manager can constrain the
    /// complete resolved graph by release date. `None` omits that constraint.
    fn relock_args(_before: Option<&str>) -> Vec<String>;

    /// The read-only driver args that prove the existing lock matches the current manifests.
    ///
    /// `None` means this manager has no supported frozen/check mode wired yet, so `check` must keep
    /// failing closed with an unknown lock-currency result.
    #[must_use]
    fn verify_current_args() -> Option<Vec<String>> {
        None
    }

    /// The mutating driver args that refresh the lock before a read-only command evaluates it.
    ///
    /// `window_minutes` carries the project-default cooldown floor when the package manager can
    /// express one during resolve. `None` means no resolver floor should be passed.
    #[must_use]
    fn refresh_lock_args(_window_minutes: Option<i64>) -> Option<Vec<String>> {
        None
    }

    /// Whether this manager supports a standalone lock refresh before a read-only command.
    #[must_use]
    fn supports_lock_refresh() -> bool {
        false
    }

    /// Whether this manager re-resolves the whole importer graph jointly in a single pass, so cooldown
    /// drives the whole-graph re-resolve/diff path rather than the per-package relock loop.
    ///
    /// Only pnpm does: one filtered `pnpm update <pkg>@<target> …` re-resolves the selected importer
    /// graph at once — direct *and* transitive — pinning each planned candidate to its exact
    /// per-package target. This is the prerequisite for settling mutually-exclusive peer conflicts
    /// at a single fixed point instead of ping-ponging between per-package pins. npm/yarn/bun have no
    /// equivalent joint resolve, so they keep the per-package relock path.
    #[must_use]
    fn supports_whole_graph_resolve() -> bool {
        false
    }

    /// The single command that re-resolves the whole graph under cooldown's window, pinning each
    /// eligible candidate to its exact per-package target — pnpm's
    /// `update <pkg>@<target> … --lockfile-only --no-save` (the forward `upgrade` and the `fix`
    /// rollback both pass their `change.to` targets). `filters` selects only importers that declare a
    /// planned candidate; an empty list falls back to recursive resolution when importer attribution
    /// is unavailable.
    ///
    /// Each `pin` is `(name, target)`: the per-package target the core computed for that candidate's
    /// own window. The resolve lands it at exactly that version, never overshooting a package whose
    /// stricter per-package window admits an older version than the global one (the gap a bare
    /// `--latest` left, since pnpm's `minimumReleaseAge` is a single global value). Multi-version
    /// candidates must be held out by the caller before this point: pnpm's bare `update <name>` can
    /// write out-of-range lock entries while `--no-save` leaves manifests unchanged. `minimumReleaseAge`
    /// is still passed as the *transitive* floor: a fresh transitive the pins drag in is capped to the
    /// project-default window, so the uniform-window case lands the same lock as before while the
    /// per-package targets are honored exactly. Transitives the pins float past the global window are
    /// reconciled down by the caller's transitive-cooldown gate, exactly as for cargo/go (which have no
    /// global cutoff at all). `--no-save`/`--lockfile-only` keep `package.json` and `node_modules`
    /// untouched. A `None` `window_minutes` (a true `Latest` opt-out) omits the cap. `None` for managers
    /// without a joint resolve or when there are no exact pins to apply.
    #[must_use]
    fn whole_graph_args(
        _pins: &[(String, String)],
        _filters: &[String],
        _window_minutes: Option<i64>,
    ) -> Option<Vec<String>> {
        None
    }

    /// A cheap lockfile self-consistency check after a mutating resolve.
    ///
    /// `None` means the adapter has no local check beyond the package manager's final frozen-lock
    /// verification.
    #[must_use]
    fn lock_consistency_error(_content: &str) -> Option<String> {
        None
    }

    /// The driver args that move **only** the lock to an exact, already-in-range `version`, leaving
    /// the declared `package.json` range untouched — the lock-only path for `RewriteMode::Auto`.
    ///
    /// `None` (the default) means the package manager has no such command, so it always rewrites the
    /// manifest. The caller must only use this when `version` already satisfies the declared range:
    /// these commands re-pin whatever version they are given without validating it against the range,
    /// so an out-of-range version would leave the lock inconsistent with `package.json`.
    #[must_use]
    fn lockonly_update_args(_name: &str, _version: &str) -> Option<Vec<String>> {
        None
    }

    /// The driver args that refresh the lock pinned to an exact `version`, for the manifest-rewrite
    /// path (so the lock lands on exactly the cooldown-approved target instead of re-resolving the
    /// widened range to its newest member).
    ///
    /// Unlike [`lockonly_update_args`](NodeLock::lockonly_update_args) this *does* save the
    /// `^version` range to the **root** manifest as a side effect — npm's `install <name>@<version>`
    /// has no manifest-free exact pin (its `--no-save` is a no-op for the lock). The caller must
    /// therefore only use it when the root manifest already declares the dependency (the entry the
    /// rewrite just widened); for a member-only dependency it would add a spurious root dependency.
    /// `None` (the default) means the manager has no exact-pin install, so the caller re-resolves.
    #[must_use]
    fn pinned_relock_args(
        _name: &str,
        _version: &str,
        _before: Option<&str>,
    ) -> Option<Vec<String>> {
        None
    }

    /// The driver args that install/verify the resolved graph (the opt-in `--build` step).
    ///
    /// `before` has the same meaning as in [`NodeLock::relock_args`].
    fn build_args(_before: Option<&str>) -> Vec<String>;
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
    /// `(name, version)` pairs every declaring importer pins exactly (pnpm).
    exact_version: HashSet<(String, String)>,
    /// Names pinned exactly by every declaring member manifest (npm, which records ranges per name).
    exact_name: HashSet<String>,
    /// The distinct declared *range specifiers* per name across importers (pnpm records a `specifier:`
    /// per importer). A name two members declare with different ranges (`~7.3.0` vs `^7.0.0`, `<4` vs
    /// `^4`) is a version split even when the lock happens to resolve both to one version, so cooldown
    /// must not exact-pin it across the workspace.
    declared_specifiers: HashMap<String, HashSet<String>>,
    authoritative: bool,
}

impl MemberIndex {
    fn version_exact(by_version: HashMap<(String, String), Vec<String>>) -> Self {
        Self {
            by_version,
            authoritative: true,
            ..Default::default()
        }
    }

    fn name_only(by_name: HashMap<String, Vec<String>>) -> Self {
        Self {
            by_name,
            authoritative: true,
            ..Default::default()
        }
    }

    fn with_exact_versions(mut self, exact: HashSet<(String, String)>) -> Self {
        self.exact_version = exact;
        self
    }

    fn with_exact_names(mut self, exact: HashSet<String>) -> Self {
        self.exact_name = exact;
        self
    }

    fn with_declared_specifiers(mut self, specifiers: HashMap<String, HashSet<String>>) -> Self {
        self.declared_specifiers = specifiers;
        self
    }

    /// Whether `name`@`version` is exact-pinned by every member that declares it, so it is held: it
    /// cannot move without editing a manifest.
    #[must_use]
    pub fn is_exact_pinned(&self, name: &str, version: &str) -> bool {
        self.exact_name.contains(name)
            || self
                .exact_version
                .contains(&(name.to_string(), version.to_string()))
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

    /// Names that workspace importers DECLARE at more than one distinct line — a genuine split that
    /// must be held out of the joint update, NOT a transitive duplicate. A name splits when importers
    /// either resolve it to different versions (one member on `chalk@4.1.2`, another on `chalk@5.3.0`)
    /// OR declare it with different *range specifiers* (`semver@~7.3.0` vs `semver@^7.0.0`,
    /// `tailwindcss@"<4"` vs `@^4`) — the latter even when the lock happens to resolve both to one
    /// version. Either way an exact pin would collapse every copy onto a single target, dragging the
    /// narrower member off its own declared range; the whole-graph resolve skips these and exact-pins
    /// the rest.
    ///
    /// Derived from per-importer declarations only, so a direct dependency that merely shares a name
    /// with a transitive copy resolved at another version is single-declared and excluded. Only the
    /// version-keyed (pnpm) index carries per-importer data; the name-only (npm) index has none and
    /// yields nothing.
    #[must_use]
    pub fn names_declared_at_multiple_versions(&self) -> HashSet<String> {
        let mut split: HashSet<String> = HashSet::new();
        let mut versions: HashMap<&str, HashSet<&str>> = HashMap::new();
        for (name, version) in self.by_version.keys() {
            versions
                .entry(name.as_str())
                .or_default()
                .insert(version.as_str());
        }
        for (name, set) in versions {
            if set.len() > 1 {
                split.insert(name.to_string());
            }
        }
        // A name whose importers declare DIFFERENT range specifiers is a split too, even if the lock
        // currently resolves them to the same version: an exact pin to one member's target would force
        // the narrower-ranged sibling off its own range (the `~7.3.0`/`^7.0.0` and `<4`/`^4` cases).
        for (name, specifiers) in &self.declared_specifiers {
            if specifiers.len() > 1 {
                split.insert(name.clone());
            }
        }
        split
    }

    /// The version `member` (an importer path) resolves `name` to, if this index carries per-importer
    /// version data (pnpm) and the member declares the name. `None` for the name-only (npm) index,
    /// which records no per-importer version, and for a member that does not declare the name.
    ///
    /// Each importer declares a name at exactly one version, so the first match is the answer. Used to
    /// tell whether a candidate actually landed at *its declaring member*, not merely at the name's
    /// newest copy somewhere else in the graph (a multi-version dep's higher line).
    #[must_use]
    pub fn resolved_version(&self, member: &str, name: &str) -> Option<&str> {
        self.by_version
            .iter()
            .find_map(|((entry_name, version), members)| {
                (entry_name == name && members.iter().any(|path| path == member))
                    .then_some(version.as_str())
            })
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
            .map(|by_name| {
                MemberIndex::name_only(by_name).with_exact_names(parse_npm_exact_pins(content))
            })
            .unwrap_or_default()
    }

    fn relock_args(before: Option<&str>) -> Vec<String> {
        // `--package-lock-only` re-resolves the lock without touching node_modules, keeping apply
        // fast and side-effect-light.
        let mut args = vec![
            "install".into(),
            "--package-lock-only".into(),
            "--no-audit".into(),
            "--no-fund".into(),
        ];
        if let Some(before) = before {
            args.push(format!("--before={before}"));
        }
        args
    }

    fn pinned_relock_args(name: &str, version: &str, before: Option<&str>) -> Option<Vec<String>> {
        // `npm install <name>@<version>` pins the lock to exactly that version (and saves the range
        // to the root `package.json` — the caller gates this on the root declaring the dependency).
        let mut args = vec![
            "install".into(),
            format!("{name}@{version}"),
            "--package-lock-only".into(),
            "--no-audit".into(),
            "--no-fund".into(),
        ];
        if let Some(before) = before {
            args.push(format!("--before={before}"));
        }
        Some(args)
    }

    fn build_args(before: Option<&str>) -> Vec<String> {
        let mut args = vec!["install".into(), "--no-audit".into(), "--no-fund".into()];
        if let Some(before) = before {
            args.push(format!("--before={before}"));
        }
        args
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
            .with_exact_versions(parse_pnpm_exact_pins(content))
            .with_declared_specifiers(parse_pnpm_importer_specifiers(content))
    }

    fn relock_args(_before: Option<&str>) -> Vec<String> {
        vec!["install".into(), "--lockfile-only".into()]
    }

    fn verify_current_args() -> Option<Vec<String>> {
        Some(vec![
            "install".into(),
            "--lockfile-only".into(),
            "--frozen-lockfile".into(),
        ])
    }

    fn refresh_lock_args(window_minutes: Option<i64>) -> Option<Vec<String>> {
        let mut args = vec!["install".into(), "--lockfile-only".into()];
        if let Some(minutes) = window_minutes {
            args.push(format!("--config.minimumReleaseAge={minutes}"));
        }
        Some(args)
    }

    fn supports_lock_refresh() -> bool {
        true
    }

    fn supports_whole_graph_resolve() -> bool {
        true
    }

    fn whole_graph_args(
        pins: &[(String, String)],
        filters: &[String],
        window_minutes: Option<i64>,
    ) -> Option<Vec<String>> {
        // `pnpm update <name>@<target> …` pins each planned candidate to its EXACT per-package target
        // in one joint re-resolve, so a package whose stricter per-package window admits an older
        // version than the project default lands at its own target rather than overshooting onto the
        // global-window-newest (the gap a bare `--latest` left). `--no-save` keeps `package.json` ranges
        // as the author wrote them (the caller widens an out-of-range manifest itself first);
        // `--lockfile-only` skips `node_modules`.
        // A recursive update runs in importers that declare none of the requested packages, where pnpm
        // treats the unmatched request like an unscoped update and moves unrelated direct dependencies.
        // Filters restrict the command to the importers known to declare at least one planned pin. The
        // recursive fallback is reserved for graph-only changes with no declaring-member attribution.
        // `minimumReleaseAge` stays as the *transitive* floor — a fresh transitive the pins drag in is
        // capped to the project-default window, so the uniform-window case lands the same lock as
        // before. Transitives floated past the window are reconciled down by the caller's
        // transitive-cooldown gate, exactly as for cargo/go.
        if pins.is_empty() {
            return None;
        }
        let mut args = Vec::new();
        for filter in filters {
            args.push("--filter".to_string());
            args.push(filter.clone());
        }
        args.push("update".to_string());
        if filters.is_empty() {
            args.push("--recursive".to_string());
        }
        for (name, target) in pins {
            args.push(format!("{name}@{target}"));
        }
        args.push("--lockfile-only".to_string());
        args.push("--no-save".to_string());
        if let Some(minutes) = window_minutes {
            args.push(format!("--config.minimumReleaseAge={minutes}"));
        }
        Some(args)
    }

    fn lock_consistency_error(content: &str) -> Option<String> {
        pnpm_lock_consistency_error(content)
    }

    fn lockonly_update_args(name: &str, version: &str) -> Option<Vec<String>> {
        // `pnpm update <name>@<version>` re-pins the lock to exactly that version; `--no-save` keeps
        // the `package.json` range as the author wrote it, and `--lockfile-only` skips node_modules.
        Some(vec![
            "update".into(),
            format!("{name}@{version}"),
            "--lockfile-only".into(),
            "--no-save".into(),
        ])
    }

    fn build_args(_before: Option<&str>) -> Vec<String> {
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

    fn relock_args(_before: Option<&str>) -> Vec<String> {
        vec!["install".into()]
    }

    fn build_args(_before: Option<&str>) -> Vec<String> {
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

    fn relock_args(_before: Option<&str>) -> Vec<String> {
        vec!["install".into()]
    }

    fn build_args(_before: Option<&str>) -> Vec<String> {
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

/// Walks the `importers:` section of a pnpm lockfile and calls `visit` once per direct-dependency
/// entry with `(importer_path, dep_name, specifier, version)`, in file order. `specifier`/`version`
/// are the entry's unquoted scalar values (`None` when the entry lacks that line). Entry-level
/// delivery is order-agnostic within the entry, so consumers need no specifier-before-version
/// assumption. The four importer parsers share this so the indentation state machine lives once.
fn walk_pnpm_importer_entries<'a>(
    content: &'a str,
    mut visit: impl FnMut(&'a str, &'a str, Option<&'a str>, Option<&'a str>),
) {
    let mut in_importers = false;
    let mut member: Option<&str> = None;
    let mut in_group = false;
    let mut dep_name: Option<&str> = None;
    let mut specifier: Option<&str> = None;
    let mut version: Option<&str> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        match indent {
            0 => {
                flush_pnpm_importer_entry(member, dep_name, specifier, version, &mut visit);
                in_importers = trimmed == "importers:";
                member = None;
                in_group = false;
                dep_name = None;
                specifier = None;
                version = None;
            }
            2 if in_importers => {
                flush_pnpm_importer_entry(member, dep_name, specifier, version, &mut visit);
                member = Some(unquote_yaml_scalar(trimmed.trim_end_matches(':')));
                in_group = false;
                dep_name = None;
                specifier = None;
                version = None;
            }
            4 if in_importers => {
                flush_pnpm_importer_entry(member, dep_name, specifier, version, &mut visit);
                in_group = DIRECT_GROUPS.contains(&trimmed.trim_end_matches(':'));
                dep_name = None;
                specifier = None;
                version = None;
            }
            6 if in_importers && in_group => {
                flush_pnpm_importer_entry(member, dep_name, specifier, version, &mut visit);
                let name = unquote_yaml_scalar(trimmed.trim_end_matches(':'));
                dep_name = (!name.is_empty()).then_some(name);
                specifier = None;
                version = None;
            }
            8 if in_importers && in_group => {
                if let Some(raw) = trimmed.strip_prefix("specifier:") {
                    specifier = Some(unquote_yaml_scalar(raw));
                } else if let Some(raw) = trimmed.strip_prefix("version:") {
                    version = Some(unquote_yaml_scalar(raw));
                }
            }
            _ => {}
        }
    }
    flush_pnpm_importer_entry(member, dep_name, specifier, version, &mut visit);
}

fn flush_pnpm_importer_entry<'a>(
    member: Option<&'a str>,
    dep_name: Option<&'a str>,
    specifier: Option<&'a str>,
    version: Option<&'a str>,
    visit: &mut impl FnMut(&'a str, &'a str, Option<&'a str>, Option<&'a str>),
) {
    if let (Some(member), Some(name)) = (member, dep_name) {
        visit(member, name, specifier, version);
    }
}

/// Maps each resolved `(name, version)` dependency to the workspace member importers that declare it,
/// read from `pnpm-lock.yaml`'s `importers:` section. The resolved `version:` line under each
/// dependency gives the exact version (its `(peer)` suffix stripped to match the `packages:` keys);
/// internal `link:`/`file:`/`workspace:` versions are skipped — they are not registry packages.
/// Importer paths (the workspace root is `.`) name the source packages.
fn parse_pnpm_importer_members(content: &str) -> HashMap<(String, String), Vec<String>> {
    let mut map: HashMap<(String, String), Vec<String>> = HashMap::new();
    walk_pnpm_importer_entries(content, |member, name, _specifier, version| {
        let Some(value) = version else {
            return;
        };
        if !value.starts_with("link:")
            && !value.starts_with("file:")
            && !value.starts_with("workspace:")
        {
            // Strip the `(peer@x)` suffix so the version matches the `packages:` keys.
            let version = value.split('(').next().unwrap_or(value);
            if !version.is_empty() {
                map.entry((name.to_string(), version.to_string()))
                    .or_default()
                    .push(member.to_string());
            }
        }
    });
    map
}

/// Maps each dependency name to the set of distinct range *specifiers* its workspace-member importers
/// declare, read from `pnpm-lock.yaml`'s `importers:` section (each dependency records a `specifier:`
/// line — the declared range). A name two importers declare with different specifiers (`~7.3.0` vs
/// `^7.0.0`, `"<4"` vs `^4`) is a version split that must be held out of the joint update, even when
/// both currently resolve to the same version.
///
/// Only plain registry ranges count. A specifier carrying a protocol (`link:`, `file:`, `workspace:`,
/// `catalog:`, `npm:` aliases, `git+…`, a URL) is skipped — a semver range never contains a `:`, so
/// the colon test rejects every non-range form. Without it a `catalog:` reference or `npm:` alias on
/// one member alongside a plain range on another would read as two distinct "ranges" and force a
/// spurious split (collapsing the dep off its exact pin) even though both resolve to one version.
///
/// Specifiers are deduplicated PER IMPORTER (`(member, name)`): a single importer that lists the same
/// name in two groups (e.g. `dependencies` and `peerDependencies`) with different ranges is one
/// declaration, not a split — only the first group's specifier is kept, so a lone importer can never
/// split itself.
fn parse_pnpm_importer_specifiers(content: &str) -> HashMap<String, HashSet<String>> {
    let mut map: HashMap<String, HashSet<String>> = HashMap::new();
    let mut recorded: HashSet<(String, String)> = HashSet::new();
    walk_pnpm_importer_entries(content, |member, name, specifier, _version| {
        let Some(specifier) = specifier else {
            return;
        };
        // A semver range never contains `:`; every protocol/alias form (`workspace:`, `catalog:`,
        // `npm:`, `git+…`, a URL) does, so the colon test keeps only ranges.
        if !specifier.is_empty()
            && !specifier.contains(':')
            && recorded.insert((member.to_string(), name.to_string()))
        {
            map.entry(name.to_string())
                .or_default()
                .insert(specifier.to_string());
        }
    });
    map
}

fn pnpm_lock_consistency_error(content: &str) -> Option<String> {
    let mut error = None;
    walk_pnpm_importer_entries(content, |member, name, specifier, version| {
        if error.is_none() {
            error = pnpm_importer_entry_error(member, name, specifier, version);
        }
    });
    error
}

/// Validates one pnpm importer entry with a deliberately one-way semver approximation.
///
/// The Rust `semver` crate is not node-semver. Forms it cannot parse (`||` unions, hyphen ranges
/// `1.2 - 3.4`, space-separated comparator sets, dist tags like `latest`) fail
/// `VersionReq::parse` and skip the check, never causing a false flag. The one known semantic
/// divergence is a bare `6.0.0`: node-semver treats it as exact, while Rust semver treats it as
/// caret, so a real mismatch can be missed. Overall, divergences can only under-report stale locks;
/// they must never mark a healthy lock stale.
fn pnpm_importer_entry_error(
    member: &str,
    name: &str,
    specifier: Option<&str>,
    version: Option<&str>,
) -> Option<String> {
    let specifier = specifier?.trim();
    let raw_version = version?.trim();
    if specifier.is_empty()
        || specifier.contains(':')
        || raw_version.starts_with("link:")
        || raw_version.starts_with("file:")
        || raw_version.starts_with("workspace:")
    {
        return None;
    }
    let version = raw_version.split('(').next().unwrap_or(raw_version);
    let Ok(requirement) = semver::VersionReq::parse(specifier) else {
        return None;
    };
    let Ok(version) = semver::Version::parse(version) else {
        return None;
    };
    if requirement.matches(&version) {
        return None;
    }
    Some(format!(
        "pnpm-lock.yaml importer {member} dependency {name}: version {version} does not satisfy range {specifier}"
    ))
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

/// Whether an npm/pnpm specifier is an exact pin: a bare version (`2.11.0`, `1.0.0-rc.1`) or single
/// equals range (`=2.11.0`) with no range operator, wildcard, or union. A pinned dependency cannot
/// move without editing the manifest.
fn is_exact_npm_specifier(specifier: &str) -> bool {
    let specifier = specifier.trim();
    let specifier = specifier
        .strip_prefix('=')
        .filter(|version| !version.starts_with('='))
        .map_or(specifier, str::trim);
    semver::Version::parse(specifier).is_ok()
}

/// The `(name, version)` pairs every declaring importer pins exactly in `pnpm-lock.yaml`. The
/// importer records both the `specifier:` (the declared range) and the resolved `version:`; a
/// `(name, version)` is exact-pinned only when *every* importer that declares it used an exact
/// specifier (otherwise some importer's range could still move it).
fn parse_pnpm_exact_pins(content: &str) -> HashSet<(String, String)> {
    let mut total: HashMap<(String, String), usize> = HashMap::new();
    let mut exact: HashMap<(String, String), usize> = HashMap::new();
    walk_pnpm_importer_entries(content, |_member, name, specifier, version| {
        let Some(value) = version else {
            return;
        };
        if !value.starts_with("link:")
            && !value.starts_with("file:")
            && !value.starts_with("workspace:")
        {
            let version = value.split('(').next().unwrap_or(value);
            if !version.is_empty() {
                let key = (name.to_string(), version.to_string());
                *total.entry(key.clone()).or_insert(0) += 1;
                if specifier.is_some_and(is_exact_npm_specifier) {
                    *exact.entry(key).or_insert(0) += 1;
                }
            }
        }
    });
    total
        .into_iter()
        .filter(|(key, count)| exact.get(key) == Some(count))
        .map(|(key, _)| key)
        .collect()
}

/// The dependency names every declaring member pins exactly in `package-lock.json`. npm records a
/// range (not a resolved version) per member, so this is name-keyed: a name is pinned only when
/// every member entry that declares it used an exact specifier.
fn parse_npm_exact_pins(content: &str) -> HashSet<String> {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(content) else {
        return HashSet::new();
    };
    let Some(packages) = doc.get("packages").and_then(serde_json::Value::as_object) else {
        return HashSet::new();
    };
    let mut total: HashMap<String, usize> = HashMap::new();
    let mut exact: HashMap<String, usize> = HashMap::new();
    for (key, entry) in packages {
        if key.contains("node_modules/") {
            continue;
        }
        for field in DIRECT_GROUPS {
            if let Some(obj) = entry.get(field).and_then(serde_json::Value::as_object) {
                for (name, range) in obj {
                    *total.entry(name.clone()).or_insert(0) += 1;
                    if range.as_str().is_some_and(is_exact_npm_specifier) {
                        *exact.entry(name.clone()).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    total
        .into_iter()
        .filter(|(name, count)| exact.get(name) == Some(count))
        .map(|(name, _)| name)
        .collect()
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
    use indoc::indoc;

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
        let lock = indoc! {r#"
            {
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root", "version": "0.1.0" },
                    "node_modules/lodash": { "version": "4.17.15" },
                    "node_modules/@babel/core": { "version": "7.1.0" },
                    "node_modules/a/node_modules/b": { "version": "2.0.0" }
                }
            }"#};
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
    fn pnpm_lock_consistency_flags_importer_versions_outside_their_specifier() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/admin:
                peerDependencies:
                  vite:
                    specifier: ^6
                    version: 7.3.5(@types/node@22.19.20)

            packages:

              vite@7.3.5:
                resolution: {integrity: sha512-x}
        "};

        let error = pnpm_lock_consistency_error(lock).expect("stale importer");

        assert!(error.contains("apps/admin"), "{error}");
        assert!(error.contains("vite"), "{error}");
        assert!(error.contains("7.3.5"), "{error}");
        assert!(error.contains("^6"), "{error}");
    }

    #[test]
    fn pnpm_lock_consistency_accepts_matching_peer_suffixed_versions() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/admin:
                peerDependencies:
                  vite:
                    specifier: ^6
                    version: 6.4.3(@types/node@22.19.20)

            packages:

              vite@6.4.3:
                resolution: {integrity: sha512-x}
        "};

        assert_eq!(pnpm_lock_consistency_error(lock), None);
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
        let lock = indoc! {r#"
            {
                "lockfileVersion": 1,
                "packages": {
                    "lodash": ["lodash@4.17.15", "", {}, "sha512-x"],
                    "@babel/core": ["@babel/core@7.1.0", "", {}, "sha512-y"],
                },
            }"#};
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
    fn names_declared_at_multiple_versions_ignores_transitive_duplicates() {
        // `bar` is a genuine workspace split: apps/b declares 2.0.0, apps/c declares 3.0.0 — it must be
        // flagged, so neither line is collapsed. `foo` is declared at a SINGLE version
        // by importers but ALSO appears as a transitive copy at 2.0.0 in `packages:` — it must NOT be
        // flagged, so it stays exact-pinned and keeps its per-package window and any out-of-range
        // widen. Counting the whole resolved graph (the old behavior) would wrongly float `foo`.
        let lock = "\
importers:

  apps/a:
    dependencies:
      foo:
        specifier: ^1.0.0
        version: 1.0.0

  apps/b:
    dependencies:
      bar:
        specifier: ^2.0.0
        version: 2.0.0

  apps/c:
    dependencies:
      bar:
        specifier: ^3.0.0
        version: 3.0.0

packages:

  foo@1.0.0:
    resolution: {integrity: sha512-a}
  foo@2.0.0:
    resolution: {integrity: sha512-b}
  bar@2.0.0:
    resolution: {integrity: sha512-c}
  bar@3.0.0:
    resolution: {integrity: sha512-d}
";
        let index = MemberIndex::version_exact(parse_pnpm_importer_members(lock));
        let split = index.names_declared_at_multiple_versions();
        assert!(
            split.contains("bar"),
            "bar is declared at 2.0.0 and 3.0.0 across importers — a genuine split"
        );
        assert!(
            !split.contains("foo"),
            "foo is declared at one version by importers; its transitive 2.0.0 copy must not flag it"
        );
    }

    #[test]
    fn names_declared_at_multiple_specifiers_are_a_split_even_at_one_resolved_version() {
        // `semver` is declared with DIFFERENT ranges (`~7.3.0` and `^7.0.0`) by two importers that the
        // lock resolves to the SAME `7.3.8` — a specifier split that exact-pinning would collapse,
        // dragging the `~7.3.0` member off its own range. It must be flagged as a split even
        // though `by_version` sees a single resolved version. `chalk` is the control: both importers
        // declare the SAME `^5.0.0` at the same resolved version, so it stays exact-pinnable.
        let lock = indoc! {"
            importers:

              pkgs/tilde:
                dependencies:
                  semver:
                    specifier: ~7.3.0
                    version: 7.3.8
                  chalk:
                    specifier: ^5.0.0
                    version: 5.3.0

              pkgs/caret:
                dependencies:
                  semver:
                    specifier: ^7.0.0
                    version: 7.3.8
                  chalk:
                    specifier: ^5.0.0
                    version: 5.3.0

            packages:

              semver@7.3.8:
                resolution: {integrity: sha512-a}
              chalk@5.3.0:
                resolution: {integrity: sha512-b}
        "};
        let index = Pnpm::member_sources(lock);
        let split = index.names_declared_at_multiple_versions();
        assert!(
            split.contains("semver"),
            "semver is declared at ~7.3.0 and ^7.0.0 — a specifier split even at one resolved version"
        );
        assert!(
            !split.contains("chalk"),
            "chalk is declared with the same ^5.0.0 range at one version — not a split, stays exact-pinnable"
        );
    }

    #[test]
    fn specifier_split_ignores_protocols_and_single_importer_groups() {
        // `react`: one member references it via a pnpm `catalog:` and another via a plain `^18.0.0`,
        // both resolving to 18.2.0. The `catalog:` form is not a registry range, so it must be ignored
        // — leaving a single real specifier, NOT a split. `next`: a single importer lists it in BOTH
        // `dependencies` and `peerDependencies` with different ranges; one importer cannot split
        // itself, so only the first group's specifier counts. Neither may be flagged, or a uniformly
        // declared dependency would lose its exact pin (and its cross-major widen).
        let lock = indoc! {"
            importers:

              pkgs/app:
                dependencies:
                  react:
                    specifier: catalog:
                    version: 18.2.0
                  next:
                    specifier: ^14.0.0
                    version: 14.2.0
                peerDependencies:
                  next:
                    specifier: '>=13'
                    version: 14.2.0

              pkgs/lib:
                dependencies:
                  react:
                    specifier: ^18.0.0
                    version: 18.2.0

            packages:

              react@18.2.0:
                resolution: {integrity: sha512-a}
              next@14.2.0:
                resolution: {integrity: sha512-b}
        "};
        let split = Pnpm::member_sources(lock).names_declared_at_multiple_versions();
        assert!(
            !split.contains("react"),
            "react's catalog: reference is not a range; with one real specifier it must not split"
        );
        assert!(
            !split.contains("next"),
            "next is declared by a single importer (deps + peer); one importer cannot split itself"
        );
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
        let lock = indoc! {r#"
            {
                "lockfileVersion": 3,
                "packages": {
                    "": { "devDependencies": { "turbo": "^2" } },
                    "packages/api": { "dependencies": { "zod": "^3" } },
                    "node_modules/zod": { "version": "3.22.0" }
                }
            }"#};
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

    #[test]
    fn only_pnpm_supports_standalone_lock_refresh() {
        assert!(!Npm::supports_lock_refresh());
        assert!(Pnpm::supports_lock_refresh());
        assert!(!Yarn::supports_lock_refresh());
        assert!(!Bun::supports_lock_refresh());
    }

    #[test]
    fn exact_specifier_distinguishes_pins_from_ranges() {
        assert!(is_exact_npm_specifier("2.11.0"));
        assert!(is_exact_npm_specifier("=2.11.0"));
        assert!(is_exact_npm_specifier("1.0.0-rc.1"));
        assert!(!is_exact_npm_specifier("==2.11.0"));
        assert!(!is_exact_npm_specifier("1"));
        assert!(!is_exact_npm_specifier("1.2"));
        assert!(!is_exact_npm_specifier("^2.11.0"));
        assert!(!is_exact_npm_specifier("~2.11.0"));
        assert!(!is_exact_npm_specifier(">=2.0.0"));
        assert!(!is_exact_npm_specifier("2.x"));
        assert!(!is_exact_npm_specifier("workspace:*"));
    }

    #[test]
    fn pnpm_exact_pins_require_every_importer_to_pin() {
        // `pinned` is pinned exactly by both importers; `loose` is exact in one and a range in the
        // other, so it could still move — not a pin.
        let lock = "\
importers:

  apps/a:
    dependencies:
      pinned:
        specifier: 2.11.0
        version: 2.11.0
      loose:
        specifier: 1.0.0
        version: 1.0.0

  apps/b:
    dependencies:
      pinned:
        specifier: 2.11.0
        version: 2.11.0
      loose:
        specifier: ^1.0.0
        version: 1.0.0

packages:

  pinned@2.11.0:
    resolution: {integrity: sha512-x}
";
        let pins = parse_pnpm_exact_pins(lock);
        assert!(pins.contains(&("pinned".to_string(), "2.11.0".to_string())));
        assert!(!pins.contains(&("loose".to_string(), "1.0.0".to_string())));
    }

    #[test]
    fn pnpm_exact_pins_unquote_yaml_scalars() {
        let lock = "\
importers:

  'apps/a':
    dependencies:
      '@scope/pkg':
        specifier: '2.11.0'
        version: '2.11.0(react@19.0.0)'

packages:

  '@scope/pkg@2.11.0':
    resolution: {integrity: sha512-x}
";
        let pins = parse_pnpm_exact_pins(lock);

        assert!(pins.contains(&("@scope/pkg".to_string(), "2.11.0".to_string())));
    }
}
