//! Lockfile parsers for the npm-compatible package managers. Each manager resolves from the same
//! registry but records the resolved graph in its own format; the [`NodeLock`] trait abstracts the
//! per-manager differences (lockfile name, driver binary, parse, and lock refresh args) so a single
//! generic adapter can serve all of them.
//!
//! Every parser returns the flat list of resolved [`NameVersion`] pairs the lock pins.
//! Where the lock records importer/member declarations (npm v2/v3, pnpm), the adapter uses that same
//! data for both direct/transitive classification and source attribution; older formats fall back to
//! the root manifest's declared dependency names.

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

    /// Parses the lockfile body into the flat list of resolved [`NameVersion`] pairs.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] if the lockfile cannot be parsed.
    fn parse(content: &str) -> Result<Vec<NameVersion>>;

    /// The workspace member package(s) that declare each dependency, for attributing a dependency to
    /// its source package(s) in reports. Default: empty (yarn classic and bun record no per-member
    /// data in their locks, so their `members` column stays blank).
    #[must_use]
    fn member_sources(_content: &str) -> MemberIndex {
        MemberIndex::default()
    }

    /// The peer requirements the lock records: each resolved package's `peerDependencies` entries
    /// (optional peers included — see [`PeerRequirement`]), used to hold a cross-major target that
    /// would break a dependent's peer contract. Default: empty (yarn classic and bun record no peer
    /// metadata in their locks), which fails open — the peer-feasibility gate simply never fires
    /// for those managers. The lock is NOT authoritative for *workspace-local* packages' peer
    /// contracts (a symlinked package's peers live only in its own `package.json`, and an injected
    /// package's importer attribution is skipped); the gate reads those from the member manifests
    /// separately.
    #[must_use]
    fn peer_requirements(_content: &str) -> Vec<PeerRequirement> {
        Vec::new()
    }

    /// Which workspace member importers consume each *local* package, keyed by the local package's
    /// importer path (e.g. `packages/plugin` → `["."]`). Covers every encoding of a locally
    /// consumed package — pnpm symlinks (`link:`) and injected copies (`file:` via
    /// `dependenciesMeta.*.injected`) alike. A local package is present in its consumers'
    /// contexts, so its manifest-declared peer ranges bind there even though the lock's importer
    /// records carry no peer metadata for it. Default: empty (fails open for lock formats without
    /// local-package records).
    #[must_use]
    fn local_package_consumers(_content: &str) -> HashMap<String, Vec<String>> {
        HashMap::new()
    }

    /// Every workspace member directory the lock records, whatever it declares — pnpm's
    /// `importers:` keys, npm's non-`node_modules` `packages` keys (the root as `.`). A member
    /// that declares no dependencies appears nowhere in [`member_sources`](NodeLock::member_sources)
    /// yet still owns a `package.json` whose `peerDependencies` bind, so peer evidence enumerates
    /// members from here rather than from what they declare. Default: empty (yarn classic and bun
    /// record no member data).
    #[must_use]
    fn member_paths(_content: &str) -> HashSet<String> {
        HashSet::new()
    }

    /// The physical install layout the lock records, when this manager materializes a hoisted
    /// `node_modules` tree the lock describes path-by-path (npm's package-lock v2/v3). Hoisting is
    /// why the layout matters for peers: packages declared by *disjoint* workspace members still
    /// meet at the root `node_modules`, so declaration paths cannot bound what a dependent
    /// resolves. Default: `None` — a layout isolated by declaration (pnpm importers) or a lock
    /// format that records no layout (yarn classic, bun); peer visibility then falls back to the
    /// declaring-member overlap rule.
    #[must_use]
    fn install_paths(_content: &str) -> Option<InstallPaths> {
        None
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
    /// planned candidate and makes an empty selection fail instead of silently doing nothing; an
    /// empty list falls back to recursive resolution when importer attribution is unavailable.
    ///
    /// Each `pin` is `(name, target)`: the per-package target the core computed for that candidate's
    /// own window. The resolve lands it at exactly that version, never overshooting a package whose
    /// stricter per-package window admits an older version than the global one (the gap a bare
    /// `--latest` left, since pnpm's `minimumReleaseAge` is a single global value). Multi-version
    /// candidates must be held out by the caller before this point: pnpm's bare `update <name>` can
    /// write out-of-range lock entries while `--no-save` leaves manifests unchanged.
    ///
    /// A positive `window_minutes` is the transitive floor: a fresh transitive the pins drag in is
    /// capped to the project-default window. Transitives the pins float past it are reconciled down
    /// by the caller's transitive-cooldown gate, exactly as for cargo/go (which have no global
    /// cutoff at all). `None` omits the command-line age setting.
    ///
    /// `--no-save`/`--lockfile-only` keep `package.json` and `node_modules` untouched. Returns `None`
    /// for managers without a joint resolve or when there are no exact pins to apply.
    #[must_use]
    fn whole_graph_args(
        _pins: &[(String, String)],
        _filters: &[String],
        _window_minutes: Option<i64>,
    ) -> Option<Vec<String>> {
        None
    }

    /// A lock-only install used while repairing a lock that pnpm's persisted release-age preflight
    /// rejects. `resolution_only` forces every edge through temporary exact overrides; the settling
    /// pass clears the temporary override metadata after the original native config is restored.
    ///
    /// `--trust-lockfile` skips only pnpm's starting-lock verification. The age floor and exact
    /// exclusions still govern the resolution that replaces the rejected entries. Returns `None`
    /// for managers without this preflight.
    #[must_use]
    fn policy_repair_args(
        _window_minutes: Option<i64>,
        _minimum_age_excludes: &[String],
        _resolution_only: bool,
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
    /// Unlike [`lockonly_update_args`](NodeLock::lockonly_update_args), this may save a range as a
    /// side effect. It is only safe inside a preserving transaction that restores cooldown's
    /// authorized manifest bytes. `None` (the default) means the manager has no exact-pin install,
    /// so the caller re-resolves.
    #[must_use]
    fn pinned_relock_args(
        _name: &str,
        _version: &str,
        _before: Option<&str>,
    ) -> Option<Vec<String>> {
        None
    }

    /// How this manager lands the EXACT planned version while preserving the manifest bytes
    /// authorized by cooldown (see [`PreservingPin`]). `None` for a manager with no such
    /// capability, which leaves the caller re-resolving.
    ///
    /// `workspaces` scopes managers whose root-level pin cannot safely reach a member-owned
    /// declaration; managers with intrinsically manifest-free pins may ignore it.
    #[must_use]
    fn preserving_pin(
        _name: &str,
        _version: &str,
        _before: Option<&str>,
        _workspaces: &[String],
    ) -> Option<PreservingPin> {
        None
    }

    /// The driver args that install/verify the resolved graph (the opt-in `--build` step).
    ///
    /// `before` has the same meaning as in [`NodeLock::relock_args`].
    fn build_args(_before: Option<&str>) -> Vec<String>;
}

/// How a manager lands one exact planned version while preserving cooldown's manifest edits.
///
/// The manifest state may be unchanged or contain a range widening authorized by the rewrite mode.
/// A package manager must not broaden or relocate those edits while landing the lock target.
///
/// A plain relock cannot serve as the exact operation:
///
/// - it never moves a lock that still satisfies its range, so nothing happens;
/// - a "move within the declared range" update (`npm update <name>`) is not target-directed. It
///   lands the range *maximum*, so a plan that deliberately stays below it — a patch move under a
///   `>=5 <7` range with `--major` off, or under a `max-major` ceiling — overshoots and is
///   rejected by the exact-target check, and it cannot move *downward* at all, so a `fix` rollback
///   silently does nothing.
///
/// So the operation is modeled by what the manager can actually do, not by one argv.
#[derive(Debug)]
pub enum PreservingPin {
    /// One command pins the exact version and writes no manifest (pnpm's `update --no-save`).
    Direct(Vec<String>),
    /// The manager's only exact pin also *saves* the range (npm's `install <name>@<version>`, which
    /// writes into whichever field already declares the package), so it runs bracketed: pin, restore
    /// cooldown's authorized manifest bytes, then run a plain relock. The final step makes the
    /// restore consistent: the pin also rewrote the lock's own copy of the manifest metadata, and
    /// synchronization recopies the restored ranges while keeping the pinned version, which
    /// satisfies them.
    PinRestoreResync {
        /// The exact pin, which may edit manifests.
        pin: Vec<String>,
        /// The manifest-preserving lock synchronization that follows the restore.
        resync: Vec<String>,
    },
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

    /// Whether `name`@`version` is attributed per exact resolved version (a pnpm importer
    /// declaration), as opposed to npm's name-only attribution, which cannot single out one
    /// resolved copy of a name. The peer-feasibility gate needs the distinction: name-only
    /// attribution of a name resolved at several versions may be pointing at a nested transitive
    /// copy, which must not gate.
    #[must_use]
    pub fn version_attributed(&self, name: &str, version: &str) -> bool {
        self.by_version
            .contains_key(&(name.to_string(), version.to_string()))
    }

    /// Every distinct resolved version of `name` across the workspace's importer declarations,
    /// ascending — the divergent lines a multi-version hold keeps apart. Empty for the name-only
    /// (npm) index, which records no per-importer version.
    #[must_use]
    pub fn resolved_versions_of(&self, name: &str) -> Vec<&str> {
        let mut versions: Vec<&str> = self
            .by_version
            .keys()
            .filter(|(entry, _)| entry == name)
            .map(|(_, version)| version.as_str())
            .collect();
        versions.sort_by(|a, b| crate::version::compare(a, b));
        versions.dedup();
        versions
    }

    /// Every distinct range specifier the workspace's importers declare for `name`, sorted — the
    /// divergent declarations behind a range-only split, where the lock resolves every copy to one
    /// version but the ranges themselves disagree (`~7.3.0` vs `^7.0.0`). Empty for the name-only
    /// (npm) index, which records no per-importer specifiers.
    #[must_use]
    pub fn declared_specifiers_of(&self, name: &str) -> Vec<&str> {
        let mut specifiers: Vec<&str> = self
            .declared_specifiers
            .get(name)
            .map(|set| set.iter().map(String::as_str).collect())
            .unwrap_or_default();
        specifiers.sort_unstable();
        specifiers
    }

    /// Whether ANY member declares `name`, at whatever version — the veto-eligibility test for a
    /// violated peer package: a purely transitive peer (an auto-installed
    /// `@typescript-eslint/parser`) is the resolver's to place and re-place, so it may never veto
    /// a move, no matter how it is physically bound.
    #[must_use]
    pub fn declares(&self, name: &str) -> bool {
        self.by_name.contains_key(name) || self.by_version.keys().any(|(entry, _)| entry == name)
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

/// One resolved package's declared peer requirement on another package, read from the lock.
///
/// `dependent`/`dependent_version` identify the resolved package declaring the peer; `package` is
/// the peer's target and `range` its verbatim declared range. Optional peers
/// (`peerDependenciesMeta.<name>.optional`) are reported like any other: optionality only tolerates
/// the peer's *absence* (npm skips auto-installing it), not a present copy outside the declared
/// range — and the gate only ever queries the requirement for a package that is present, since the
/// queried peer is the package being upgraded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRequirement {
    /// The resolved package declaring the peer requirement (e.g. `fumadocs-mdx`).
    pub dependent: String,
    /// The dependent's resolved version, whose metadata declared the range.
    pub dependent_version: String,
    /// The package the peer requirement targets (e.g. `fumadocs-core`).
    pub package: String,
    /// The verbatim declared peer range (e.g. `^16.0.0`).
    pub range: String,
}

/// Parses the `packages:` section of `pnpm-lock.yaml` (v9) into every resolved package's peer
/// requirements. Package entries carry `peerDependencies:` (name → range); the same line-based walk
/// as [`parse_pnpm`], since the section's shape is fixed and shallow. A `(peer@x)` disambiguation
/// suffix on the entry key is stripped, so one package resolved under several peer contexts reports
/// its requirement once per resolved copy — duplicates are harmless to the gate. Optional peers are
/// reported too (see [`PeerRequirement`]); the walk still tracks the `peerDependenciesMeta:` field
/// only so its children are never misread as `peerDependencies:` entries.
fn parse_pnpm_peer_requirements(content: &str) -> Vec<PeerRequirement> {
    /// Which nested field of the current package entry the walk is inside.
    #[derive(PartialEq)]
    enum Field {
        None,
        Peers,
        Meta,
    }

    let mut out = Vec::new();
    let mut in_packages = false;
    let mut entry: Option<NameVersion> = None;
    let mut field = Field::None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue; // blank lines punctuate the file but never end a section
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        match indent {
            0 => {
                in_packages = trimmed == "packages:";
                entry = None;
                field = Field::None;
            }
            2 if in_packages => {
                let key = unquote_yaml_scalar(trimmed.trim_end_matches(':'));
                // Balanced trailing-group strip, not a truncate at the first `(`: an injected
                // key's `file:` path may itself contain parenthesized directory segments.
                let key = strip_pnpm_peer_suffixes(key);
                entry = split_name_version(key);
                field = Field::None;
            }
            4 if in_packages && entry.is_some() => {
                field = match trimmed.trim_end_matches(':') {
                    "peerDependencies" => Field::Peers,
                    "peerDependenciesMeta" => Field::Meta,
                    _ => Field::None,
                };
            }
            6 if in_packages && field == Field::Peers => {
                if let (Some(dependent), Some((name, range))) =
                    (entry.as_ref(), trimmed.split_once(':'))
                {
                    let range = unquote_yaml_scalar(range);
                    if !range.is_empty() {
                        out.push(PeerRequirement {
                            dependent: dependent.name.clone(),
                            dependent_version: dependent.version.clone(),
                            package: unquote_yaml_scalar(name).to_string(),
                            range: range.to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Parses `package-lock.json` (lockfileVersion 2/3) into every resolved package's peer
/// requirements, from the flat `packages` map's `peerDependencies` records — optional peers
/// included (see [`PeerRequirement`]). A workspace member entry (a key not under `node_modules/`)
/// is identified by its `name` field — npm copies the member's manifest, peers included, into the
/// lock — never by its path-shaped key. The v1 `dependencies` tree records no peer metadata, so an
/// old lock yields nothing (fail open).
fn parse_npm_peer_requirements(content: &str) -> Vec<PeerRequirement> {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(content) else {
        return Vec::new();
    };
    let Some(packages) = doc.get("packages").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, val) in packages {
        // The root project is keyed by the empty string; its own peers are reported via the
        // workspace-manifest source instead (the gate reads member manifests directly).
        if key.is_empty() {
            continue;
        }
        // An installed package's name is its path tail; a workspace member entry carries a
        // path-shaped key, so its real name comes from the copied manifest's `name` field.
        let name = if key.contains("node_modules/") {
            key.rsplit("node_modules/").next().filter(|s| !s.is_empty())
        } else {
            val.get("name").and_then(|v| v.as_str())
        };
        let Some(name) = name else {
            continue;
        };
        let Some(version) = val.get("version").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(peers) = val.get("peerDependencies").and_then(|v| v.as_object()) else {
            continue;
        };
        for (peer, range) in peers {
            if let Some(range) = range.as_str() {
                out.push(PeerRequirement {
                    dependent: name.to_string(),
                    dependent_version: version.to_string(),
                    package: peer.clone(),
                    range: range.to_string(),
                });
            }
        }
    }
    out
}

/// The physical install layout of a hoisted lock: every resolved instance keyed by the directory
/// it is installed at (`node_modules/react`, `apps/a/node_modules/react`, and a workspace
/// member's own directory such as `apps/a`) — the tree npm resolves `require` and peers against.
/// Peer visibility is a question about this layout, not about declaring members: hoisting lets
/// disjoint members' packages meet at the root, and a nested conflict copy shadows the hoisted
/// one, so neither declaration paths nor a graph-wide version can answer "which copy does this
/// dependent actually see".
pub struct InstallPaths {
    /// Install directory → the version installed there.
    versions: HashMap<String, String>,
    /// Package name → every `(version, directory)` instance of it.
    instances: HashMap<String, Vec<(String, String)>>,
}

/// One physical copy of a package, as a lookup on the install tree resolves it.
#[derive(Debug, PartialEq)]
pub struct ResolvedInstance<'a> {
    /// The version installed at this instance's directory.
    pub version: &'a str,
    /// The instance's install directory (`node_modules/react`,
    /// `apps/a/node_modules/react`) — the context its own peers then resolve from.
    pub directory: &'a str,
}

impl InstallPaths {
    /// The instance a workspace member's own directory resolves `name` to: its version and its
    /// install directory (the context that instance's peers then resolve from). This is what makes
    /// a member's *direct* copy identifiable even when other versions of the name exist nested
    /// elsewhere — declaration attribution is name-only in npm's lock, but the physical lookup is
    /// exact.
    #[must_use]
    pub fn member_resolution(&self, member: &str, name: &str) -> Option<ResolvedInstance<'_>> {
        // The root project's member path is `.`; its install-tree directory is the empty key.
        let dir = if member == "." { "" } else { member };
        self.resolve_from(dir, name)
    }

    /// The instance `name` resolves to from `dir` — the nearest enclosing `node_modules/<name>`,
    /// walking from the directory itself up to the workspace root — as a [`ResolvedInstance`].
    /// Intermediate ancestors that are not package directories probe keys no lock ever records
    /// (`…/node_modules/node_modules/x`), which is harmless.
    #[must_use]
    pub fn resolve_from(&self, dir: &str, name: &str) -> Option<ResolvedInstance<'_>> {
        let mut ancestor = dir;
        loop {
            let candidate = if ancestor.is_empty() {
                format!("node_modules/{name}")
            } else {
                format!("{ancestor}/node_modules/{name}")
            };
            if let Some((directory, version)) = self.versions.get_key_value(&candidate) {
                return Some(ResolvedInstance { version, directory });
            }
            if ancestor.is_empty() {
                return None;
            }
            ancestor = ancestor.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }

    /// The install directories holding `name` at exactly `version` — every physical copy a
    /// context-less move of that version could rewrite.
    #[must_use]
    pub fn instance_dirs(&self, name: &str, version: &str) -> Vec<&str> {
        self.instances
            .get(name)
            .into_iter()
            .flatten()
            .filter(|(instance_version, _)| instance_version == version)
            .map(|(_, dir)| dir.as_str())
            .collect()
    }
}

/// The workspace member directories `package-lock.json` (v2/v3) records: every `packages` key that
/// is not an install path, with the root's empty key normalized to `.`. A crafted or stale key that
/// could address a manifest outside the workspace is rejected at this parse boundary.
fn parse_npm_member_paths(content: &str) -> HashSet<String> {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(content) else {
        return HashSet::new();
    };
    let Some(packages) = doc.get("packages").and_then(serde_json::Value::as_object) else {
        return HashSet::new();
    };
    packages
        .keys()
        .filter(|key| !key.contains("node_modules/"))
        .map(|key| if key.is_empty() { "." } else { key.as_str() })
        .filter(|path| is_workspace_relative(path))
        .map(str::to_string)
        .collect()
}

/// Reads the physical layout from `package-lock.json` (v2/v3): every `packages` entry keyed by
/// its install directory. A workspace member's own entry (`apps/a`) counts as an instance of its
/// manifest `name`; a symlink entry (`"link": true`) is an instance of its *target's* name and
/// version at the link's directory — the hoisted `node_modules/<member>` alias consumers actually
/// resolve. A v1 lock has no `packages` map and yields `None` (the caller falls back to
/// declaring-member overlap).
fn parse_npm_install_paths(content: &str) -> Option<InstallPaths> {
    let doc = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let packages = doc.get("packages")?.as_object()?;
    let mut versions: HashMap<String, String> = HashMap::new();
    let mut instances: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (key, entry) in packages {
        // The root project itself is keyed by the empty string; it is not an installed instance.
        if key.is_empty() {
            continue;
        }
        // A link entry records only its target path; the instance's name and version live there.
        let entry = if entry.get("link").and_then(serde_json::Value::as_bool) == Some(true) {
            let Some(target) = entry
                .get("resolved")
                .and_then(serde_json::Value::as_str)
                .and_then(|target| packages.get(target))
            else {
                continue;
            };
            target
        } else {
            entry
        };
        // An installed package's name is its path tail; a workspace member entry carries a
        // path-shaped key, so its real name comes from the copied manifest's `name` field.
        let name = if let Some((_, tail)) = key.rsplit_once("node_modules/") {
            (!tail.is_empty()).then_some(tail)
        } else {
            entry.get("name").and_then(serde_json::Value::as_str)
        };
        let version = entry.get("version").and_then(serde_json::Value::as_str);
        let (Some(name), Some(version)) = (name, version) else {
            continue;
        };
        versions.insert(key.clone(), version.to_string());
        instances
            .entry(name.to_string())
            .or_default()
            .push((version.to_string(), key.clone()));
    }
    Some(InstallPaths {
        versions,
        instances,
    })
}

/// The two halves of a `name@version` specifier.
///
/// The derived ordering (name first, then version) gives resolved-package lists a stable sort key.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NameVersion {
    /// The package name, with a scope's leading `@` preserved (`@scope/name`).
    pub name: String,
    /// The version — everything after the specifier's last `@`.
    pub version: String,
}

impl NameVersion {
    /// Builds the pair from its already-split halves.
    pub(crate) fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

/// Splits a `name@version` (or scoped `@scope/name@version`) specifier into its parts. The version
/// is taken after the last `@`, so the leading `@` of a scope is preserved in the name.
pub(crate) fn split_name_version(spec: &str) -> Option<NameVersion> {
    let at = spec.rfind('@').filter(|&i| i > 0)?;
    let (name, version) = spec.split_at(at);
    Some(NameVersion {
        name: name.to_string(),
        version: version[1..].to_string(),
    })
}

fn unquote_yaml_scalar(value: &str) -> &str {
    let value = value.trim();
    if let Some(inner) = value
        .strip_prefix('\'')
        .and_then(|inner| inner.strip_suffix('\''))
    {
        return inner;
    }
    if let Some(inner) = value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        return inner;
    }
    value
}

/// Decodes a pnpm importer ID, whose path may contain a YAML-escaped apostrophe.
fn decode_pnpm_importer_path(value: &str) -> String {
    let value = value.trim();
    let Some(path) = value
        .strip_prefix('\'')
        .and_then(|inner| inner.strip_suffix('\''))
    else {
        return unquote_yaml_scalar(value).to_string();
    };
    path.replace("''", "'")
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

    fn parse(content: &str) -> Result<Vec<NameVersion>> {
        parse_npm(content)
    }

    fn member_sources(content: &str) -> MemberIndex {
        parse_npm_member_sources(content)
            .map(|by_name| {
                MemberIndex::name_only(by_name).with_exact_names(parse_npm_exact_pins(content))
            })
            .unwrap_or_default()
    }

    fn peer_requirements(content: &str) -> Vec<PeerRequirement> {
        parse_npm_peer_requirements(content)
    }

    fn member_paths(content: &str) -> HashSet<String> {
        parse_npm_member_paths(content)
    }

    fn install_paths(content: &str) -> Option<InstallPaths> {
        parse_npm_install_paths(content)
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
        // to the selected install scope, which the preserving transaction restores afterward).
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

    fn preserving_pin(
        name: &str,
        version: &str,
        before: Option<&str>,
        workspaces: &[String],
    ) -> Option<PreservingPin> {
        // npm has no exact pin that skips the save (`--no-save` makes the install a no-op whenever
        // the tree already satisfies the manifest), so cooldown restores its authorized manifest
        // bytes after the pin. A plain relock then recopies those ranges into package-lock metadata
        // while retaining the exact, compatible resolution. See [`PreservingPin::PinRestoreResync`].
        let mut pin = Self::pinned_relock_args(name, version, before)?;
        for workspace in workspaces {
            pin.push(format!("--workspace={workspace}"));
        }
        let resync = Self::relock_args(before);
        Some(PreservingPin::PinRestoreResync { pin, resync })
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

    fn parse(content: &str) -> Result<Vec<NameVersion>> {
        Ok(parse_pnpm(content))
    }

    fn member_sources(content: &str) -> MemberIndex {
        MemberIndex::version_exact(parse_pnpm_importer_members(content))
            .with_exact_versions(parse_pnpm_exact_pins(content))
            .with_declared_specifiers(parse_pnpm_importer_specifiers(content))
    }

    fn peer_requirements(content: &str) -> Vec<PeerRequirement> {
        parse_pnpm_peer_requirements(content)
    }

    fn member_paths(content: &str) -> HashSet<String> {
        pnpm_importer_paths(content)
    }

    fn local_package_consumers(content: &str) -> HashMap<String, Vec<String>> {
        parse_pnpm_local_package_consumers(content)
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
        // A positive `minimumReleaseAge` stays as the transitive floor.
        if pins.is_empty() {
            return None;
        }
        let mut args = Vec::new();
        for filter in filters {
            args.push("--filter".to_string());
            args.push(filter.clone());
        }
        if !filters.is_empty() {
            // A successful zero-selection update is indistinguishable from a resolver rejection in
            // the resulting lock diff, so an invalid location selector must fail explicitly.
            args.push("--fail-if-no-match".to_string());
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

    fn policy_repair_args(
        window_minutes: Option<i64>,
        minimum_age_excludes: &[String],
        resolution_only: bool,
    ) -> Option<Vec<String>> {
        let mut args = vec!["install".to_string(), "--lockfile-only".to_string()];
        if resolution_only {
            args.push("--resolution-only".to_string());
        }
        args.push("--trust-lockfile".to_string());
        if let Some(minutes) = window_minutes {
            args.push(format!("--config.minimumReleaseAge={minutes}"));
        }
        for exclusion in minimum_age_excludes {
            args.push(format!("--config.minimumReleaseAgeExclude={exclusion}"));
        }
        Some(args)
    }

    fn lock_consistency_error(content: &str) -> Option<String> {
        pnpm_lock_consistency_error(content)
    }

    fn preserving_pin(
        name: &str,
        version: &str,
        _before: Option<&str>,
        _workspaces: &[String],
    ) -> Option<PreservingPin> {
        // pnpm's exact pin already skips the manifest (`--no-save`), so one command suffices.
        Some(PreservingPin::Direct(Self::lockonly_update_args(
            name, version,
        )?))
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

    fn parse(content: &str) -> Result<Vec<NameVersion>> {
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

    fn parse(content: &str) -> Result<Vec<NameVersion>> {
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
fn parse_npm(content: &str) -> Result<Vec<NameVersion>> {
    let doc: serde_json::Value = serde_json::from_str(content)
        .map_err(|e| CoreError::Parse(format!("package-lock.json: {e}")))?;
    let mut out = Vec::new();
    if let Some(packages) = doc.get("packages").and_then(|v| v.as_object()) {
        for (key, val) in packages {
            // Only install-path keys name registry-resolved packages. The root project is keyed
            // by the empty string, and a workspace member's own entry by its directory path
            // (`apps/a`) — both are local packages, not resolved dependencies; treating a member
            // path as a package name would send `apps/a` to the registry as a lookup. (A member's
            // hoisted `node_modules/<name>` alias is a link entry without a `version`, so it is
            // skipped by the version filter below.)
            let Some((_, name)) = key.rsplit_once("node_modules/") else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            if let Some(version) = val.get("version").and_then(|v| v.as_str()) {
                out.push(NameVersion::new(name, version));
            }
        }
    } else if let Some(deps) = doc.get("dependencies").and_then(|v| v.as_object()) {
        for (name, val) in deps {
            if let Some(version) = val.get("version").and_then(|v| v.as_str()) {
                out.push(NameVersion::new(name.clone(), version));
            }
        }
    }
    Ok(out)
}

/// Parses `pnpm-lock.yaml` (v9): the top-level `packages:` section keys every resolved package by
/// its `name@version(...peers)` identity. We read those keys directly — line by line — rather than
/// pulling in a YAML dependency, since the keys are the only field we need.
fn parse_pnpm(content: &str) -> Vec<NameVersion> {
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
            // Drop the `(peer@x)` suffixes pnpm appends to disambiguate peer resolutions — as a
            // balanced trailing-group strip, since an injected key's `file:` path may itself
            // contain parenthesized directory segments.
            let key = strip_pnpm_peer_suffixes(key);
            if let Some(entry) = split_name_version(key) {
                out.push(entry);
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
fn walk_pnpm_importer_entries(
    content: &str,
    mut visit: impl FnMut(&str, &str, Option<&str>, Option<&str>),
) {
    let mut in_importers = false;
    let mut member: Option<String> = None;
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
                flush_pnpm_entry(member.as_deref(), dep_name, specifier, version, &mut visit);
                in_importers = trimmed == "importers:";
                member = None;
                in_group = false;
                dep_name = None;
                specifier = None;
                version = None;
            }
            2 if in_importers => {
                flush_pnpm_entry(member.as_deref(), dep_name, specifier, version, &mut visit);
                let path = decode_pnpm_importer_path(trimmed.trim_end_matches(':'));
                // A crafted or stale importer key could name a path outside the workspace, and
                // every consumer of these paths joins them onto the project root — reject
                // non-workspace-relative keys once, here at the parse boundary.
                member = is_workspace_relative(&path).then_some(path);
                in_group = false;
                dep_name = None;
                specifier = None;
                version = None;
            }
            4 if in_importers => {
                flush_pnpm_entry(member.as_deref(), dep_name, specifier, version, &mut visit);
                in_group = DIRECT_GROUPS.contains(&trimmed.trim_end_matches(':'));
                dep_name = None;
                specifier = None;
                version = None;
            }
            6 if in_importers && in_group => {
                flush_pnpm_entry(member.as_deref(), dep_name, specifier, version, &mut visit);
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
    flush_pnpm_entry(member.as_deref(), dep_name, specifier, version, &mut visit);
}

fn flush_pnpm_entry(
    member: Option<&str>,
    dep_name: Option<&str>,
    specifier: Option<&str>,
    version: Option<&str>,
    visit: &mut impl FnMut(&str, &str, Option<&str>, Option<&str>),
) {
    if let (Some(member), Some(name)) = (member, dep_name) {
        visit(member, name, specifier, version);
    }
}

/// Maps each *local* package (by its importer path) to the importers that consume it, read from
/// `pnpm-lock.yaml`'s `importers:` section — the entries [`parse_pnpm_importer_members`]
/// deliberately skips. Both pnpm encodings of a locally consumed package are covered: a symlinked
/// `link:` version, whose target is recorded relative to the consuming importer
/// (`link:../../packages/plugin` from `apps/app`), and an injected `file:` version
/// (`dependenciesMeta.*.injected`), whose target is recorded workspace-root-relative with trailing
/// peer-context groups and is recovered against the lock's own importer set (see
/// [`resolve_injected_target`]). Either form normalizes to a workspace-root-relative importer path
/// before keying; a target that escapes the workspace root is dropped rather than aliased onto a
/// workspace path (see [`normalize_local_target`]).
fn parse_pnpm_local_package_consumers(content: &str) -> HashMap<String, Vec<String>> {
    let importers = pnpm_importer_paths(content);
    let directories = pnpm_injected_directories(content);
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    walk_pnpm_importer_entries(content, |member, name, _specifier, version| {
        let target = match version {
            Some(value) => {
                if let Some(target) = value.strip_prefix("link:") {
                    normalize_local_target(member, target)
                } else if let Some(target) = value.strip_prefix("file:") {
                    resolve_injected_target(name, target, &importers, &directories)
                } else {
                    None
                }
            }
            None => None,
        };
        if let Some(target) = target {
            map.entry(target).or_default().push(member.to_string());
        }
    });
    for consumers in map.values_mut() {
        consumers.sort();
        consumers.dedup();
    }
    map
}

/// Every importer path in the lock's `importers:` section — including members that declare no
/// dependencies (`packages/shim: {}`), which [`walk_pnpm_importer_entries`] never visits. This is
/// the canonical set of workspace member directories, used as the ground truth when recovering a
/// local package path from an injected `file:` version whose peer-suffix boundary is ambiguous.
fn pnpm_importer_paths(content: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut in_importers = false;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        match indent {
            0 => in_importers = trimmed == "importers:",
            2 if in_importers => {
                let raw_key = if let Some(stripped) = trimmed.strip_suffix(':') {
                    stripped
                } else if let Some((key, _)) = trimmed.split_once(": ") {
                    // An importer with no dependencies is a flow-style `path: {}` line.
                    key
                } else {
                    continue;
                };
                let path = decode_pnpm_importer_path(raw_key);
                if is_workspace_relative(&path) {
                    out.insert(path);
                }
            }
            _ => {}
        }
    }
    out
}

/// Recovers the workspace member path from an injected `file:` version. The scalar alone is
/// ambiguous: pnpm appends `(peer@x)`/`(key=hash)` groups to the root-relative path, but a
/// directory name may itself end in a parenthesized group containing the same marks —
/// `file:packages/shim(foo@bar)(eslint@8.57.1)` is real pnpm 11 output for a member named
/// `shim(foo@bar)`, and no suffix heuristic can split it correctly. Two authority sources
/// disambiguate, in order: the lock's own `resolution: {directory: …, type: directory}` entry for
/// THIS dependency (keyed `<name>@file:<reference>` — an unrelated package's directory carries no
/// authority over this scalar's reading), then the importer set (locks predating the resolution
/// shape). In both, the scalar resolves only when exactly ONE reading matches — an ambiguous
/// scalar (both `packages/shim` and `packages/shim(eslint@8.57.1)` known) fails open rather than
/// resolving by preference, which could read the wrong manifest and fabricate a hold.
fn resolve_injected_target(
    name: &str,
    raw: &str,
    importers: &HashSet<String>,
    directories: &InjectedDirectories,
) -> Option<String> {
    directories
        .resolve(name, raw)
        .or_else(|| unique_match(&injected_interpretations(raw), importers))
}

/// Every reading of the scalar as reference-plus-peeled-trailing-groups, verbatim, least-peeled
/// first. The verbatim scalar is a reading too: a parenthesized final directory segment peels
/// like a suffix group, so only the full set of readings — never one preferred split — is sound.
fn peeled_readings(raw: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut candidate = raw;
    loop {
        out.push(candidate);
        if !candidate.ends_with(')') {
            break;
        }
        let Some(open) = trailing_group_start(candidate) else {
            break;
        };
        let Some(prefix) = candidate.get(..open) else {
            break;
        };
        candidate = prefix;
    }
    out
}

/// [`peeled_readings`] as normalized workspace-relative paths (unnormalizable readings dropped) —
/// the form the importer set is keyed by.
fn injected_interpretations(raw: &str) -> Vec<String> {
    peeled_readings(raw)
        .into_iter()
        .filter_map(|candidate| normalize_local_target(".", candidate))
        .collect()
}

/// The single interpretation the authority set names — `None` when none or several match, so an
/// ambiguous scalar never resolves by preference.
fn unique_match(interpretations: &[String], authority: &HashSet<String>) -> Option<String> {
    let mut matches = interpretations
        .iter()
        .filter(|interpretation| authority.contains(*interpretation));
    match (matches.next(), matches.next()) {
        (Some(only), None) => Some(only.clone()),
        _ => None,
    }
}

/// The lock's `packages:` entries that resolve as injected workspace copies, each associated with
/// the dependency identity that owns it: the entry key `<name>@file:<reference>` carries the
/// dependency name and the suffix-free spelling of its `file:` reference, and its
/// `resolution: {directory: …, type: directory}` line is the authoritative injected path — but
/// only for that dependency. Treating every directory as interchangeable authority would let an
/// unrelated package's entry decide another scalar's reading (and pick the wrong manifest
/// whenever that scalar's own entry went unparsed).
struct InjectedDirectories(HashMap<String, Vec<(String, String)>>);

impl InjectedDirectories {
    /// The directory this dependency's own resolution entry records for the scalar `raw`, trying
    /// every peeled reading as the reference (the entry key stores the reference without pnpm's
    /// appended peer/patch groups, while a parenthesized directory name keeps its groups — so the
    /// verbatim reading is tried too). Distinct directories for several readings would be
    /// contradictory authority: fail open.
    fn resolve(&self, name: &str, raw: &str) -> Option<String> {
        let entries = self.0.get(name)?;
        let mut found: Option<&str> = None;
        for reading in peeled_readings(raw) {
            for (reference, directory) in entries {
                if reference != reading {
                    continue;
                }
                if found.is_some_and(|previous| previous != directory) {
                    return None;
                }
                found = Some(directory);
            }
        }
        found.map(str::to_string)
    }
}

/// Scans the `packages:` section for injected entries (`resolution: {…, type: directory}`) and
/// associates each recorded directory with the `<name>@file:<reference>` identity of its own
/// entry key (see [`InjectedDirectories`]). A key without the `@file:` marker, or a directory
/// that is not workspace-relative, contributes nothing.
fn pnpm_injected_directories(content: &str) -> InjectedDirectories {
    let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut in_packages = false;
    let mut current: Option<(String, String)> = None;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        match indent {
            0 => {
                in_packages = trimmed == "packages:";
                current = None;
            }
            2 if in_packages => {
                current = trimmed
                    .strip_suffix(':')
                    .map(unquote_yaml_scalar)
                    // The first `@file:` is the name/reference boundary: a scope's `@` sits at the
                    // very start of the name, and a name can never contain `:`.
                    .and_then(|key| key.split_once("@file:"))
                    .filter(|(name, _)| !name.is_empty())
                    .map(|(name, reference)| (name.to_string(), reference.to_string()));
            }
            _ if in_packages && indent >= 4 => {
                if let Some((name, reference)) = &current
                    && let Some(rest) = trimmed.strip_prefix("resolution:")
                    && rest.contains("type: directory")
                    && let Some(after) = rest.split("directory:").nth(1)
                {
                    let value =
                        unquote_yaml_scalar(after.split([',', '}']).next().unwrap_or("").trim());
                    if is_workspace_relative(value) {
                        map.entry(name.clone())
                            .or_default()
                            .push((reference.clone(), value.to_string()));
                    }
                }
            }
            _ => {}
        }
    }
    InjectedDirectories(map)
}

/// Resolves a local-package target into a workspace-root-relative importer path (`apps/app` +
/// `../../packages/plugin` → `packages/plugin`; the workspace root importer is `.`). Returns
/// `None` for a target the workspace cannot contain — an absolute path, a drive/URL form, or `..`
/// traversal past the workspace root (root importer + `link:../shared/plugin`): silently clamping
/// such a target would alias an *outside* package onto an unrelated inside path whose manifest
/// could then produce a false hold, violating the gate's hold-only-on-proof rule.
fn normalize_local_target(importer: &str, target: &str) -> Option<String> {
    // Lock IDs use portable `/` separators exclusively, so a workspace-relative path never starts
    // at the filesystem root, contains `:` (a Windows drive or nested protocol form), or contains
    // `\` — on Windows, `..\outside` or `\\server\share` would read as one opaque segment here
    // yet escape the workspace when joined onto the root.
    if target.starts_with('/') || target.contains(':') || target.contains('\\') {
        return None;
    }
    let mut segments: Vec<&str> = if importer == "." {
        Vec::new()
    } else {
        importer.split('/').collect()
    };
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    Some(if segments.is_empty() {
        ".".to_string()
    } else {
        segments.join("/")
    })
}

/// Whether `path` is already a canonical workspace-root-relative importer path (`.` or `a/b` — no
/// absolute/drive/protocol form, no `.`/`..` segments, no trailing slash). Lock-sourced member
/// paths get joined onto the project root and read from disk, so any other form must be rejected
/// at the parse boundary before it can address a manifest outside the workspace.
pub(crate) fn is_workspace_relative(path: &str) -> bool {
    normalize_local_target(".", path).as_deref() == Some(path)
}

/// Strips the trailing parenthesized disambiguation groups pnpm appends to a **package key**
/// (`name@version(eslint@8.57.1)(patch_hash=…)`), leaving the `name@version` part. Only trailing
/// balanced groups carrying a `@` or `=` are removed — a registry version never contains
/// parentheses, so for registry keys this is exact. For an injected `file:` key the embedded
/// directory path may itself end in such a group, making the boundary ambiguous from the scalar
/// alone; key consumers only need the *name* (the version tail stays best-effort and is never
/// used as a filesystem path). Local *path* recovery must not use this heuristic — that is
/// [`resolve_injected_target`], which disambiguates against the lock's importer set.
fn strip_pnpm_peer_suffixes(value: &str) -> &str {
    let mut out = value;
    while out.ends_with(')') {
        let Some(open) = trailing_group_start(out) else {
            break;
        };
        let Some((path, group)) = out.get(..open).zip(out.get(open..)) else {
            break;
        };
        if group.contains('@') || group.contains('=') {
            out = path;
        } else {
            break;
        }
    }
    out
}

/// The byte index of the `(` opening the balanced group that closes at the end of `value`, or
/// `None` when the parentheses are unbalanced. Only meaningful when `value` ends with `)`.
fn trailing_group_start(value: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (index, byte) in value.bytes().enumerate().rev() {
        match byte {
            b')' => depth += 1,
            b'(' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
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
        // Member keys are workspace paths that consumers join onto the project root; reject a
        // crafted or stale key that could address a manifest outside the workspace.
        if !is_workspace_relative(member) {
            continue;
        }
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
fn parse_yarn(content: &str) -> Vec<NameVersion> {
    let mut out = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("  version ") {
            let version = rest.trim().trim_matches('"');
            for name in pending.drain(..) {
                out.push(NameVersion::new(name, version));
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
fn parse_bun(content: &str) -> Result<Vec<NameVersion>> {
    let normalised = strip_trailing_commas(content);
    let doc: serde_json::Value = serde_json::from_str(&normalised)
        .map_err(|e| CoreError::Parse(format!("bun.lock: {e}")))?;
    let mut out = Vec::new();
    if let Some(packages) = doc.get("packages").and_then(|v| v.as_object()) {
        for val in packages.values() {
            if let Some(spec) = val.get(0).and_then(|v| v.as_str())
                && let Some(entry) = split_name_version(spec)
            {
                out.push(entry);
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

    fn sorted(mut entries: Vec<NameVersion>) -> Vec<NameVersion> {
        entries.sort();
        entries
    }

    #[test]
    fn splits_scoped_and_plain_specifiers() {
        assert_eq!(
            split_name_version("lodash@4.17.15"),
            Some(NameVersion {
                name: "lodash".into(),
                version: "4.17.15".into()
            })
        );
        assert_eq!(
            split_name_version("@babel/core@7.1.0"),
            Some(NameVersion {
                name: "@babel/core".into(),
                version: "7.1.0".into()
            })
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
                NameVersion::new("lodash", "4.17.15"),
                NameVersion::new("@babel/core", "7.1.0"),
                NameVersion::new("b", "2.0.0"),
            ])
        );
    }

    #[test]
    fn pnpm_packages_section() {
        let lock = "lockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      lodash:\n        specifier: 4.17.15\n        version: 4.17.15\n\npackages:\n\n  lodash@4.17.15:\n    resolution: {integrity: sha512-x}\n\n  '@babel/core@7.1.0':\n    resolution: {integrity: sha512-y}\n\n  chalk@4.0.0(supports-color@7.2.0):\n    resolution: {integrity: sha512-z}\n";
        assert_eq!(
            sorted(parse_pnpm(lock)),
            sorted(vec![
                NameVersion::new("lodash", "4.17.15"),
                NameVersion::new("@babel/core", "7.1.0"),
                NameVersion::new("chalk", "4.0.0"),
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
                NameVersion::new("lodash", "4.17.15"),
                NameVersion::new("@babel/core", "7.1.0"),
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
                NameVersion::new("lodash", "4.17.15"),
                NameVersion::new("@babel/core", "7.1.0"),
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
        // The hold's report detail needs the disagreeing declarations themselves: with a single
        // resolved version, "declared at multiple versions" would be factually wrong, so the
        // specifiers are what the skip row must name.
        assert_eq!(
            index.resolved_versions_of("semver"),
            ["7.3.8"],
            "the lock resolves both declarations to one version"
        );
        assert_eq!(
            index.declared_specifiers_of("semver"),
            ["^7.0.0", "~7.3.0"],
            "the divergent specifiers are exposed for the hold's detail line"
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

  '''apps/a':
    dependencies:
      '@scope/pkg':
        specifier: '^1.2.3'
        version: '1.2.3(react@19.0.0)'

packages:

  '@scope/pkg@1.2.3':
    resolution: {integrity: sha512-x}
";
        let index = MemberIndex::version_exact(parse_pnpm_importer_members(lock));

        assert_eq!(index.members_for("@scope/pkg", "1.2.3"), vec!["'apps/a"]);
        assert_eq!(
            decode_pnpm_importer_path(r#""apps/has''two""#),
            "apps/has''two"
        );
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

    /// pnpm's `packages:` section records each resolved package's peer ranges. Scoped (quoted)
    /// keys, quoted ranges, and `(peer@x)` disambiguation suffixes all parse; a peer marked
    /// `optional: true` in `peerDependenciesMeta` is reported like any other — optionality only
    /// tolerates the peer's absence, not a present copy outside the range.
    #[test]
    fn pnpm_peer_requirements_include_optional_peers() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  eslint:
                    specifier: ^8.40.0
                    version: 8.57.0

            packages:

              '@typescript-eslint/eslint-plugin@6.21.0':
                resolution: {integrity: sha512-aaa}
                engines: {node: ^16.0.0 || >=18.0.0}
                peerDependencies:
                  '@typescript-eslint/parser': ^6.0.0 || ^6.0.0-alpha
                  eslint: ^7.0.0 || ^8.0.0
                peerDependenciesMeta:
                  typescript:
                    optional: true

              fumadocs-mdx@15.1.1(fumadocs-core@16.11.4):
                resolution: {integrity: sha512-bbb}
                peerDependencies:
                  fumadocs-core: ^16.0.0
                  typescript: '>=5'
                peerDependenciesMeta:
                  typescript:
                    optional: true
        "};

        let reqs = parse_pnpm_peer_requirements(lock);
        assert!(reqs.contains(&PeerRequirement {
            dependent: "@typescript-eslint/eslint-plugin".into(),
            dependent_version: "6.21.0".into(),
            package: "eslint".into(),
            range: "^7.0.0 || ^8.0.0".into(),
        }));
        // The peer-suffixed key strips down to its base name@version.
        assert!(reqs.contains(&PeerRequirement {
            dependent: "fumadocs-mdx".into(),
            dependent_version: "15.1.1".into(),
            package: "fumadocs-core".into(),
            range: "^16.0.0".into(),
        }));
        // Optional peers are reported too: a present copy outside the range still violates.
        assert!(reqs.contains(&PeerRequirement {
            dependent: "fumadocs-mdx".into(),
            dependent_version: "15.1.1".into(),
            package: "typescript".into(),
            range: ">=5".into(),
        }));
    }

    /// The `link:`/`file:` entries the importer-member parse skips are exactly the local-package
    /// consumer edges: each local package (keyed by its normalized workspace-root-relative path)
    /// maps to the importers that consume it, whether pnpm records the consumption as a symlink
    /// (`link:`, consumer-relative) or an injected copy (`file:`, root-relative with a `(peer@x)`
    /// context suffix, recovered against the importer set).
    #[test]
    fn pnpm_local_package_consumers_cover_linked_and_injected_entries() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  local-eslint-shim:
                    specifier: workspace:*
                    version: 'file:packages/shim(eslint@8.57.1)'

              apps/site:
                dependencies:
                  local-eslint-shim:
                    specifier: workspace:*
                    version: 'link:../../packages/shim'

              apps/docs:
                dependencies:
                  local-eslint-shim:
                    specifier: workspace:*
                    version: link:packages/shim

              packages/shim: {}
        "};

        let consumers = parse_pnpm_local_package_consumers(lock);
        assert_eq!(
            consumers.get("packages/shim"),
            Some(&vec![".".to_string(), "apps/site".to_string()]),
            "the injected root consumer and the symlinked consumer resolve to the same target"
        );
        assert_eq!(
            consumers.get("apps/docs/packages/shim"),
            Some(&vec!["apps/docs".to_string()]),
            "a link target without `..` stays relative to its consuming importer"
        );
    }

    /// An injected path may itself contain parenthesized directory segments — even ones carrying
    /// `@`/`=`, indistinguishable from pnpm's suffix grammar in the scalar alone
    /// (`file:packages/shim(foo@bar)(eslint@8.57.1)` is real pnpm 11 output for a member named
    /// `shim(foo@bar)`). The importer set disambiguates: the scalar resolves only when exactly
    /// one of its peeled readings names a known importer (see [`resolve_injected_target`]).
    #[test]
    fn pnpm_local_package_consumers_keep_parenthesized_path_segments() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  ambiguous-shim:
                    specifier: workspace:*
                    version: 'file:packages/shim(foo@bar)(eslint@8.57.1)'
                  nested-shim:
                    specifier: workspace:*
                    version: 'file:packages/shim(foo)/pkg(eslint@8.57.1)'
                  patched-widget:
                    specifier: workspace:*
                    version: 'file:packages/widget(a@1)(patch_hash=abc)'
                  plain-parens:
                    specifier: workspace:*
                    version: 'file:packages/plain(dir)'
                  stranger:
                    specifier: workspace:*
                    version: 'file:vendor/outsider(eslint@8.57.1)'

              'packages/shim(foo@bar)': {}

              packages/shim(foo)/pkg: {}

              packages/widget: {}

              packages/plain(dir): {}
        "};

        let consumers = parse_pnpm_local_package_consumers(lock);
        assert_eq!(
            consumers.get("packages/shim(foo@bar)"),
            Some(&vec![".".to_string()]),
            "a directory group carrying `@` survives because the importer set names it"
        );
        assert_eq!(
            consumers.get("packages/shim(foo)/pkg"),
            Some(&vec![".".to_string()]),
            "a trailing peer group is peeled without touching parenthesized path segments"
        );
        assert_eq!(
            consumers.get("packages/widget"),
            Some(&vec![".".to_string()]),
            "stacked peer and patch groups are all peeled down to the importer"
        );
        assert_eq!(
            consumers.get("packages/plain(dir)"),
            Some(&vec![".".to_string()]),
            "a parenthesized final directory matches its importer exactly"
        );
        assert!(
            !consumers.keys().any(|key| key.starts_with("vendor/")),
            "a target matching no importer is dropped, not guessed at"
        );
    }

    /// An ambiguous scalar — every interpretation names a *known* path — never resolves by
    /// preference: with both `packages/shim` and `packages/shim(eslint@8.57.1)` as importers, the
    /// scalar could be either package, and guessing could read the wrong manifest. It fails open
    /// unless the lock's authoritative `resolution: {directory: …}` entries single one out.
    #[test]
    fn pnpm_injected_targets_fail_open_on_ambiguity_unless_a_resolution_directory_decides() {
        let ambiguous = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  local-eslint-shim:
                    specifier: workspace:*
                    version: 'file:packages/shim(eslint@8.57.1)'

              packages/shim: {}

              'packages/shim(eslint@8.57.1)': {}
        "};
        assert!(
            parse_pnpm_local_package_consumers(ambiguous).is_empty(),
            "two plausible importer interpretations must fail open, not resolve by preference"
        );

        let decided = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  local-eslint-shim:
                    specifier: workspace:*
                    version: 'file:packages/shim(eslint@8.57.1)'

              packages/shim: {}

              'packages/shim(eslint@8.57.1)': {}

            packages:

              local-eslint-shim@file:packages/shim:
                resolution: {directory: packages/shim, type: directory}
                peerDependencies:
                  eslint: ^8.0.0
        "};
        assert_eq!(
            parse_pnpm_local_package_consumers(decided).get("packages/shim"),
            Some(&vec![".".to_string()]),
            "the authoritative resolution directory singles out the injected path"
        );

        // The resolution entry is authoritative only for its OWN dependency: an unrelated
        // package's directory must not decide this scalar's reading — with the shim's own entry
        // unparsed, the ambiguity stands and the edge is dropped, not resolved by proxy.
        let unrelated_authority = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  local-eslint-shim:
                    specifier: workspace:*
                    version: 'file:packages/shim(eslint@8.57.1)'
                  other-tool:
                    specifier: workspace:*
                    version: 'file:packages/shim'

              packages/shim: {}

              'packages/shim(eslint@8.57.1)': {}

            packages:

              other-tool@file:packages/shim:
                resolution: {directory: packages/shim, type: directory}
        "};
        let consumers = parse_pnpm_local_package_consumers(unrelated_authority);
        assert_eq!(
            consumers.get("packages/shim"),
            Some(&vec![".".to_string()]),
            "the unambiguous scalar still resolves through its own entry"
        );
        assert_eq!(
            consumers.get("packages/shim").map(Vec::len),
            Some(1),
            "the ambiguous shim scalar must not ride on other-tool's directory"
        );
        assert!(
            !consumers.contains_key("packages/shim(eslint@8.57.1)"),
            "no reading of the ambiguous scalar may resolve"
        );
    }

    /// A workspace member's own entry is a local package, not a registry-resolved dependency: its
    /// path-shaped key must not parse as a package *name* (`apps/a` would go to the registry as a
    /// lookup), and its hoisted alias is a versionless link entry that contributes nothing
    /// either.
    #[test]
    fn npm_member_entries_are_not_resolved_packages() {
        let lock = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root", "version": "0.1.0" },
                "apps/a": { "name": "app-a", "version": "0.1.0" },
                "node_modules/app-a": { "resolved": "apps/a", "link": true },
                "node_modules/react": { "version": "18.3.1" }
            }
        }"#};
        assert_eq!(
            crate::lock::Npm::parse(lock).expect("a v3 lock parses"),
            vec![NameVersion::new("react", "18.3.1")]
        );
    }

    /// npm resolves peers against the physical tree, so the layout — not the declaring member —
    /// answers what a dependent sees: a hoisted root copy is visible from every member, a nested
    /// conflict copy shadows it, and a workspace member (or its hoisted symlink) is an instance
    /// of its manifest name.
    #[test]
    fn npm_install_paths_resolve_by_nearest_ancestor() {
        let lock = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root" },
                "apps/a": { "name": "a", "version": "0.1.0", "dependencies": { "plugin": "^1.0.0" } },
                "packages/lib": { "name": "lib", "version": "2.0.0" },
                "node_modules/lib": { "resolved": "packages/lib", "link": true },
                "node_modules/plugin": { "version": "1.0.0" },
                "node_modules/host": { "version": "2.0.0" },
                "node_modules/shadowed": { "version": "1.0.0" },
                "node_modules/shadowed/node_modules/host": { "version": "1.0.0" }
            }
        }"#};
        let paths = crate::lock::Npm::install_paths(lock).expect("v3 lock records the layout");

        assert_eq!(
            paths.resolve_from("node_modules/plugin", "host"),
            Some(ResolvedInstance {
                version: "2.0.0",
                directory: "node_modules/host"
            }),
            "a hoisted instance resolves the hoisted peer"
        );
        assert_eq!(
            paths.resolve_from("node_modules/shadowed", "host"),
            Some(ResolvedInstance {
                version: "1.0.0",
                directory: "node_modules/shadowed/node_modules/host"
            }),
            "a nested copy shadows the hoisted one"
        );
        assert_eq!(
            paths.member_resolution("apps/a", "plugin"),
            Some(ResolvedInstance {
                version: "1.0.0",
                directory: "node_modules/plugin"
            }),
            "a workspace member resolves hoisted packages from its own directory"
        );
        assert_eq!(
            paths.resolve_from("node_modules/lib", "host"),
            Some(ResolvedInstance {
                version: "2.0.0",
                directory: "node_modules/host"
            }),
            "a link entry resolves peers from its own hoisted directory"
        );
        assert_eq!(
            paths.instance_dirs("lib", "2.0.0").len(),
            2,
            "a workspace member is an instance at its directory AND at its hoisted link"
        );
        assert_eq!(
            paths.resolve_from("node_modules/plugin", "absent"),
            None,
            "an uninstalled peer binds nowhere"
        );

        assert!(
            crate::lock::Npm::install_paths(r#"{"lockfileVersion": 1}"#).is_none(),
            "a v1 lock records no layout — the caller falls back to member overlap"
        );
    }

    /// A local-package target the workspace cannot contain is dropped, never clamped onto an
    /// inside path: root + `link:../shared/plugin` really points *outside* the workspace, and
    /// aliasing it to `shared/plugin` could read an unrelated manifest and fabricate a hold.
    #[test]
    fn pnpm_local_package_consumers_reject_targets_escaping_the_workspace() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  external-plugin:
                    specifier: link:../shared/plugin
                    version: 'link:../shared/plugin'
                  absolute-plugin:
                    specifier: file:/opt/plugin
                    version: 'file:/opt/plugin'

              apps/site:
                dependencies:
                  deep-escape:
                    specifier: workspace:*
                    version: 'link:../../../elsewhere/plugin'
        "};

        assert!(
            parse_pnpm_local_package_consumers(lock).is_empty(),
            "escaping and absolute targets must not produce consumer edges"
        );
    }

    /// Importer keys and member keys are joined onto the project root by their consumers, so the
    /// parse boundary admits only canonical workspace-relative paths — a crafted or stale
    /// `/tmp/app` or `../outside` key must never survive into attribution or a filesystem read.
    #[test]
    fn lock_member_paths_reject_non_workspace_relative_keys() {
        for valid in [".", "packages/shim", "apps/site(x)"] {
            assert!(is_workspace_relative(valid), "{valid} must be accepted");
        }
        for invalid in [
            "",
            "/tmp/app",
            "../outside",
            "a/../../b",
            "C:/app",
            "a/./b",
            "a/",
            r"..\outside",
            r"\outside",
            r"\\server\share",
            r"a\b",
        ] {
            assert!(
                !is_workspace_relative(invalid),
                "{invalid} must be rejected"
            );
        }

        let pnpm = indoc! {"
            lockfileVersion: '9.0'

            importers:

              /tmp/app:
                dependencies:
                  eslint:
                    specifier: ^8.0.0
                    version: 8.57.1

              ../outside:
                dependencies:
                  eslint:
                    specifier: ^8.0.0
                    version: 8.57.1
        "};
        assert!(
            Pnpm::member_sources(pnpm).all_paths().is_empty(),
            "escaping pnpm importer keys must not attribute members"
        );

        let npm = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root", "dependencies": { "eslint": "^8.0.0" } },
                "../outside": { "name": "evil", "dependencies": { "eslint": "^8.0.0" } }
            }
        }"#};
        assert_eq!(
            Npm::member_sources(npm).all_paths(),
            HashSet::from([".".to_string()]),
            "an escaping npm member key must be dropped while the root is kept"
        );
    }

    /// A workspace member entry in package-lock (a key not under `node_modules/`) is identified by
    /// its copied manifest's `name` field, never by its path-shaped key — otherwise its peer
    /// requirement could not be attributed to the real dependent.
    #[test]
    fn npm_peer_requirements_use_the_member_manifest_name() {
        let lock = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root-app" },
                "packages/shim": {
                    "name": "local-eslint-shim",
                    "version": "0.1.0",
                    "peerDependencies": { "eslint": "^8.0.0" }
                }
            }
        }"#};

        let reqs = parse_npm_peer_requirements(lock);
        assert_eq!(
            reqs,
            vec![PeerRequirement {
                dependent: "local-eslint-shim".into(),
                dependent_version: "0.1.0".into(),
                package: "eslint".into(),
                range: "^8.0.0".into(),
            }]
        );
    }

    /// package-lock.json (v2/v3) records peers per `packages` entry; the root project's own peers
    /// (the empty key) are not a dependent's, while `peerDependenciesMeta` optionals are kept.
    #[test]
    fn npm_peer_requirements_parse_and_skip_root() {
        let lock = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "fixture",
                    "peerDependencies": { "react": "^19.0.0" }
                },
                "node_modules/fumadocs-mdx": {
                    "version": "15.1.1",
                    "peerDependencies": { "fumadocs-core": "^16.0.0", "typescript": ">=5" },
                    "peerDependenciesMeta": { "typescript": { "optional": true } }
                }
            }
        }"#};

        let reqs = parse_npm_peer_requirements(lock);
        assert_eq!(
            reqs,
            vec![
                PeerRequirement {
                    dependent: "fumadocs-mdx".into(),
                    dependent_version: "15.1.1".into(),
                    package: "fumadocs-core".into(),
                    range: "^16.0.0".into(),
                },
                PeerRequirement {
                    dependent: "fumadocs-mdx".into(),
                    dependent_version: "15.1.1".into(),
                    package: "typescript".into(),
                    range: ">=5".into(),
                },
            ]
        );
    }
}
