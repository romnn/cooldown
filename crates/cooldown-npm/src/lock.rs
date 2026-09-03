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
use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

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

    /// Whether this manager's apply engine can pin a package no importer declares — pnpm, through
    /// its temporary qualified-override resolve. The per-package landing engines (npm/yarn/bun)
    /// need a declared requirement, so graph-wide upgrade planning would only produce
    /// not-eligible skips for them.
    const SUPPORTS_TRANSITIVE_ADVANCE: bool = false;

    /// What the manager's own effective-configuration query (`<bin> config list --json`, every
    /// layer merged — builtin, global, user, project, environment) contributes to advisory identity
    /// at feed time.
    /// See [`EffectiveRegistryQuery`]; the default is the manager with no reliably parseable query
    /// (yarn classic, bun), whose identities never survive the feed.
    const EFFECTIVE_REGISTRY: EffectiveRegistryQuery = EffectiveRegistryQuery::Unavailable;

    /// Parses the lockfile body into the flat list of resolved [`NameVersion`] pairs.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] if the lockfile cannot be parsed.
    fn parse(content: &str) -> Result<Vec<NameVersion>>;

    /// The workspace member package(s) that declare each dependency, for attributing a dependency
    /// to its source package(s) in reports.
    /// Empty for yarn classic and bun, which record no per-member data in their locks, so their
    /// `members` column stays blank.
    #[must_use]
    fn member_sources(content: &str) -> MemberIndex {
        Self::member_sources_excluding(content, &HashSet::new())
    }

    /// [`member_sources`](NodeLock::member_sources) with the members at `excluded` paths left out
    /// entirely, as if the lock never recorded them — the index the whole-graph resolve derives its
    /// workspace-split evidence from, so an importer the run's exclude policy dropped
    /// ([`Plan::excluded_members`](cooldown_core::Plan::excluded_members)) contributes neither a
    /// declared range nor a resolved version.
    /// Default: empty (no per-member data).
    #[must_use]
    fn member_sources_excluding(_content: &str, _excluded: &HashSet<String>) -> MemberIndex {
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

    /// The dependency names whose *every* declaring importer outside `excluded` manages them
    /// through a pnpm catalog (a `catalog:` / `catalog:<name>` specifier).
    /// Their version pins live in `pnpm-workspace.yaml`'s catalog definitions, which cooldown does
    /// not edit: the manifest widen refuses protocol specifiers and `pnpm update <name>@<target>`
    /// re-resolves the importer back to the catalog pin, so the apply engine holds these
    /// candidates up front with a truthful skip instead of letting them surface as an eternal
    /// resolver conflict.
    /// An excluded importer's plain-range declaration is not the run's to land, so it must not talk
    /// an included catalog-only candidate out of that hold.
    /// Default: empty (only pnpm has catalogs).
    #[must_use]
    fn catalog_managed_names(_content: &str, _excluded: &HashSet<String>) -> HashSet<String> {
        HashSet::new()
    }

    /// Every requirement edge between resolved packages, keyed by the required `(name, version)`
    /// and listing the dependents as `name@version` — what names the package whose own
    /// requirement pulled a second copy of a name into the graph.
    /// Default: empty (only pnpm's lock records per-package edges in a form worth reading).
    #[must_use]
    fn graph_dependents(_content: &str) -> HashMap<(String, String), Vec<String>> {
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
    /// The distinct declared *range specifiers* per name across importers (pnpm records a
    /// `specifier:` per importer).
    /// Whether disagreeing ranges (`~7.3.0` vs `^7.0.0`, `<4` vs `^4`) split a name depends on the
    /// target being pinned — see [`splits_for`](MemberIndex::splits_for).
    declared_specifiers: HashMap<String, HashSet<String>>,
    /// Each importer's registry-resolved direct entries by name (pnpm records a `specifier:` and a
    /// `version:` per importer), the baseline the excluded-importer guard diffs.
    importer_entries: HashMap<String, BTreeMap<String, ImporterEntry>>,
    authoritative: bool,
}

/// One importer's record of one direct dependency in `pnpm-lock.yaml`: the declared range and the
/// version it resolved to (peer suffix stripped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImporterEntry {
    /// The `specifier:` line — the manifest's declared range, verbatim; `None` when the lock omits it.
    pub specifier: Option<String>,
    /// The resolved `version:`.
    pub version: String,
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

    fn with_importer_entries(
        mut self,
        entries: HashMap<String, BTreeMap<String, ImporterEntry>>,
    ) -> Self {
        self.importer_entries = entries;
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

    /// Whether exact-pinning `name` to `target` across every declaring importer would drag some
    /// importer off its own declared range — the genuine workspace split the whole-graph resolve
    /// must hold out of the joint update, NOT a transitive duplicate.
    ///
    /// Importers that resolve the name to different versions (one member on `chalk@4.1.2`, another
    /// on `chalk@5.3.0`) or declare it under different range specifiers (`~7.3.0` vs `^7.0.0`) are
    /// only a split *relative to the target*: when every distinct declared range provably admits
    /// it, pinning the whole name there is exactly what a plain `pnpm update` does and no manifest
    /// needs rewriting, so two importers on `^4.17.20` and `^4.17.21` both take `4.17.22`.
    /// The split is held whenever any declared range excludes the target — or cannot be judged at
    /// all.
    /// Node-semver forms the Rust parser cannot represent (`||` unions, hyphen ranges, dist tags)
    /// fail [`version_in_range`](crate::version::version_in_range) and so keep the split: the
    /// question is answered only on proof, never by default, since the alternative writes an
    /// out-of-range lock entry while `--no-save` leaves the manifest untouched.
    /// A name whose only declarations carry protocol specifiers (`catalog:`, `npm:` aliases)
    /// records no range at all, so several resolved versions of it stay split.
    ///
    /// Derived from per-importer declarations only, so a direct dependency that merely shares a
    /// name with a transitive copy resolved at another version is single-declared and never splits.
    /// Only the version-keyed (pnpm) index carries per-importer data; the name-only (npm) index has
    /// none and never splits.
    #[must_use]
    pub fn splits_for(&self, name: &str, target: &str) -> bool {
        let lines = self.resolved_versions_of(name).len();
        let specifiers = self.declared_specifiers_of(name);
        if lines <= 1 && specifiers.len() <= 1 {
            return false;
        }
        specifiers.is_empty()
            || specifiers
                .iter()
                .any(|specifier| !crate::version::version_in_range(specifier, target))
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
    /// A URL-resolved entry (a git or tarball dependency) is not a version line: it is no registry
    /// candidate, and counting it made a name with one registry line look split and hid the line
    /// from the resolver-introduced-split guard, which skips names already at several versions.
    #[must_use]
    pub fn resolved_versions_of(&self, name: &str) -> Vec<&str> {
        let mut versions: Vec<&str> = self
            .by_version
            .keys()
            .filter(|(entry, version)| entry == name && !is_url_resolution(version))
            .map(|(_, version)| version.as_str())
            .collect();
        // The string tiebreak keeps the order total: two versions `compare` cannot rank (build
        // metadata, unparsable strings) would otherwise fall in map order and make the hold detail
        // and the split verdict nondeterministic.
        versions.sort_by(|a, b| crate::version::compare(a, b).then_with(|| a.cmp(b)));
        versions.dedup();
        versions
    }

    /// Every name the workspace's importers declare, at any version — the set a transitive-advance
    /// override must stay clear of: an importer-declared name belongs to the targeted-update path
    /// and its peer unification, and a graph-wide override on it would drag the declared copy
    /// along with the transitive one.
    #[must_use]
    pub fn declared_names(&self) -> HashSet<String> {
        self.by_name
            .keys()
            .cloned()
            .chain(self.by_version.keys().map(|(name, _)| name.clone()))
            // An `npm:` alias records the alias under the dependency name and the real package
            // inside the resolved version (`"foo": "npm:bar@^1"` → `("foo", "bar@1.2.3")`). The
            // real name is importer-declared too: a graph-wide override on it would drag the
            // alias-declared copy exactly like a same-name declaration, so it joins the set the
            // transitive-advance engine stays clear of. The digit check keeps git/URL resolutions
            // (which also embed `@`) from minting phantom names.
            .chain(self.by_version.keys().filter_map(|(_, version)| {
                let (real, resolved) = version.rsplit_once('@')?;
                (!real.is_empty()
                    && !real.contains(':')
                    && resolved.starts_with(|c: char| c.is_ascii_digit()))
                .then(|| real.to_string())
            }))
            .collect()
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

    /// One importer's registry-resolved direct entries by name (pnpm's per-importer records); empty
    /// for a lock without per-importer records or an importer the lock does not list.
    /// `link:`/`workspace:`/`file:` entries are not versions and are absent, like everywhere in the
    /// index.
    #[must_use]
    pub fn entries_of(&self, member: &str) -> BTreeMap<&str, &ImporterEntry> {
        self.importer_entries
            .get(member)
            .into_iter()
            .flatten()
            .map(|(name, entry)| (name.as_str(), entry))
            .collect()
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

/// One parsed `pnpm-lock.yaml` document, typed for exactly the fields the pnpm readers consume.
///
/// Every other top-level section (`overrides:`, `patchedDependencies:`, `catalogs:`, `settings:`,
/// …) and every unmodeled entry field is deliberately ignored.
///
/// Each reader parses the whole document once per call instead of line-scanning just its own
/// section.
/// The cost is accepted: the readers run a handful of times per project per run, and the document
/// parse buys real YAML semantics (quoting and escapes, flow/block equivalence) instead of
/// per-reader re-implementations of them.
#[derive(Default, Deserialize)]
#[serde(default)]
pub(crate) struct PnpmLockDocument {
    /// The lock format's own version marker, kept so [`parse_pnpm_document_strict`] can reject a
    /// pre-v9 document instead of misreading its differently-shaped `packages:` keys (a v6 key is
    /// `/name@version(...)`, whose leading slash would flow into registry lookups as part of the
    /// name). `None` for a hand-crafted fixture that omits the field.
    #[serde(rename = "lockfileVersion")]
    lockfile_version: Option<LockfileVersion>,
    importers: YamlEntries<Tolerant<PnpmImporter>>,
    packages: YamlEntries<Tolerant<PnpmPackage>>,
    /// Only the keys are consumed ([`PnpmLockDocument::package_and_snapshot_keys`]); the edges
    /// under them are read by [`PnpmSnapshotsDocument`] on demand, since this document is parsed
    /// a dozen times per apply and the `snapshots:` section is the bulk of a large lock.
    snapshots: YamlEntries<IgnoredAny>,
}

/// The `snapshots:` section alone, typed for the requirement edges — parsed only when a duplicate
/// copy needs its requirer named ([`parse_pnpm_graph_dependents`]), so the per-entry cost of the
/// untagged [`Tolerant`] buffering is paid once per apply at most rather than on every read of the
/// shared document.
#[derive(Default, Deserialize)]
#[serde(default)]
struct PnpmSnapshotsDocument {
    snapshots: YamlEntries<Tolerant<PnpmSnapshot>>,
}

/// The lock's `lockfileVersion` scalar, normalized to its textual spelling. pnpm writes it as a
/// quoted string from v6 on (`'6.0'`, `'9.0'`) but as a bare YAML number in v5 (`5.4`), so the
/// visitor accepts both scalar shapes rather than fixing one type.
struct LockfileVersion(String);

impl LockfileVersion {
    /// The version's leading major (`'9.0'` → 9, `5.4` → 5), or `None` when the scalar does not
    /// start with an integer.
    fn major(&self) -> Option<u64> {
        let text = self.0.trim();
        let end = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        text.get(..end)?.parse().ok()
    }
}

impl<'de> Deserialize<'de> for LockfileVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct VersionVisitor;
        impl Visitor<'_> for VersionVisitor {
            type Value = LockfileVersion;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a lockfileVersion scalar")
            }

            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(LockfileVersion(value.to_string()))
            }

            fn visit_f64<E: serde::de::Error>(
                self,
                value: f64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(LockfileVersion(value.to_string()))
            }

            fn visit_u64<E: serde::de::Error>(
                self,
                value: u64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(LockfileVersion(value.to_string()))
            }

            fn visit_i64<E: serde::de::Error>(
                self,
                value: i64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(LockfileVersion(value.to_string()))
            }
        }
        deserializer.deserialize_any(VersionVisitor)
    }
}

impl PnpmLockDocument {
    /// Every `packages:` key followed by every `snapshots:` key, each section in document order —
    /// the two places a peer-suffixed package identity can appear, depending on the lock format
    /// (see `pnpm_peer_suffixed_names` in the peers module).
    pub(crate) fn package_and_snapshot_keys(&self) -> impl Iterator<Item = &str> {
        self.packages.keys().chain(self.snapshots.keys())
    }
}

/// A YAML mapping read in document order with duplicate keys tolerated: entries surface exactly
/// as the file lists them, and each occurrence of a duplicated key (which pnpm never writes)
/// contributes its own entry instead of erroring out or collapsing.
struct YamlEntries<V>(Vec<(String, V)>);

impl<V> Default for YamlEntries<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<V> YamlEntries<V> {
    fn entries(&self) -> impl Iterator<Item = (&str, &V)> {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }

    fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(key, _)| key.as_str())
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for YamlEntries<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct EntriesVisitor<V>(std::marker::PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for EntriesVisitor<V> {
            type Value = YamlEntries<V>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a mapping")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut access: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = access.next_entry()? {
                    entries.push(entry);
                }
                Ok(YamlEntries(entries))
            }

            // A section header with nothing under it (`packages:` at end of file) is a null
            // value, not a mapping, and reads as an empty section.
            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(YamlEntries(Vec::new()))
            }
        }
        deserializer.deserialize_map(EntriesVisitor(std::marker::PhantomData))
    }
}

/// A lock value kept fail-open per entry: a shape the typed model does not recognize (an old lock
/// format's scalar dependency entry, a hand-edited file) degrades to `Other` — leaving its key
/// readable and every sibling entry intact — instead of failing the whole document.
#[derive(Deserialize)]
#[serde(untagged)]
enum Tolerant<T> {
    Known(T),
    Other(IgnoredAny),
}

impl<T> Tolerant<T> {
    fn known(&self) -> Option<&T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Other(_) => None,
        }
    }
}

/// One `importers:` member: its direct-dependency groups (see [`DIRECT_GROUPS`]).
#[derive(Default, Deserialize)]
#[serde(default)]
struct PnpmImporter {
    dependencies: YamlEntries<Tolerant<PnpmImporterEntry>>,
    #[serde(rename = "devDependencies")]
    dev_dependencies: YamlEntries<Tolerant<PnpmImporterEntry>>,
    #[serde(rename = "optionalDependencies")]
    optional_dependencies: YamlEntries<Tolerant<PnpmImporterEntry>>,
    #[serde(rename = "peerDependencies")]
    peer_dependencies: YamlEntries<Tolerant<PnpmImporterEntry>>,
}

impl PnpmImporter {
    /// The four groups in [`DIRECT_GROUPS`] order.
    /// Group order is observable only through [`parse_pnpm_importer_specifiers`]'s
    /// first-group-wins deduplication, which this fixed order keeps deterministic.
    fn groups(&self) -> [&YamlEntries<Tolerant<PnpmImporterEntry>>; 4] {
        [
            &self.dependencies,
            &self.dev_dependencies,
            &self.optional_dependencies,
            &self.peer_dependencies,
        ]
    }
}

/// One importer dependency entry: the declared range and the resolved version, each `None` when
/// the lock omits that field.
#[derive(Deserialize)]
struct PnpmImporterEntry {
    specifier: Option<String>,
    version: Option<String>,
}

/// One `snapshots:` entry (lockfileVersion 9): the resolved package's own requirement edges, each
/// value the required package's resolved version — peer suffix included — or, under an aliased
/// key, the real package's `name@version`.
#[derive(Default, Deserialize)]
#[serde(default)]
struct PnpmSnapshot {
    dependencies: YamlEntries<Tolerant<String>>,
    #[serde(rename = "optionalDependencies")]
    optional_dependencies: YamlEntries<Tolerant<String>>,
}

/// One `packages:` entry: its resolution record and declared peer ranges.
///
/// `peerDependenciesMeta` needs no field of its own — the typed model scopes each mapping, so
/// meta children can never be misread as `peerDependencies:` entries.
#[derive(Default, Deserialize)]
#[serde(default)]
struct PnpmPackage {
    resolution: Option<PnpmResolution>,
    #[serde(rename = "peerDependencies")]
    peer_dependencies: YamlEntries<Tolerant<String>>,
}

/// A package's `resolution:` record: the injected-directory shape (`{directory: …, type:
/// directory}`) for local-package recovery, and the exact key set for origin evidence.
#[derive(Default, Deserialize)]
#[serde(default)]
struct PnpmResolution {
    integrity: Option<String>,
    directory: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Every key the model does not name — `tarball` (a registry entry served from a tarball URL
    /// pnpm could not derive from the *default* registry, or a plain URL dependency), `repo` and
    /// `commit` (git), `path`, and anything a future format adds.
    /// Any of them marks a resolution that did not come from the configured registry by name.
    /// Only the keys' presence matters; the values ride along as opaque JSON because flattening
    /// needs a sized value type.
    #[serde(flatten)]
    other: BTreeMap<String, serde_json::Value>,
}

impl PnpmResolution {
    /// Whether this is the shape pnpm writes for an artifact fetched from the configured registry
    /// by package name: the integrity hash and nothing else.
    /// pnpm names no registry per entry; the manager's effective `registry` setting for the
    /// package's scope says which one served it.
    fn is_registry(&self) -> bool {
        self.integrity.is_some()
            && self.directory.is_none()
            && self.kind.is_none()
            && self.other.is_empty()
    }
}

/// Parses `pnpm-lock.yaml` once into the typed document, failing closed on a document cooldown
/// cannot faithfully read: unparsable YAML, a structural-guard breach (nesting depth, alias
/// amplification), or an unsupported pre-v9 `lockfileVersion` — whose `packages:` keys are shaped
/// differently and would otherwise be misread into garbage names like `/lodash`. An empty or
/// whitespace-only document is a legitimately empty lock and reads as the empty document, and a
/// fixture that omits `lockfileVersion` entirely is accepted as v9.
///
/// This is the parse behind [`Pnpm::parse`] (the resolved-package list): like npm's
/// `package-lock.json` parse, a corrupted lock must surface an error, never report zero
/// dependencies as if the project were healthy.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] naming the YAML failure or the unsupported
/// `lockfileVersion` (and the supported one).
pub(crate) fn parse_pnpm_document_strict(content: &str) -> Result<PnpmLockDocument> {
    let doc: PnpmLockDocument = parse_pnpm_yaml(content)?;
    // Only a *parsable* major below 9 is rejected: an absent field tolerates hand-crafted
    // fixtures, an unparsable one falls through to whatever the structure yields (every real
    // pre-9 pnpm lock carries a numeric version), and a future major is left to prove itself
    // rather than pre-emptively rejected.
    if let Some(version) = &doc.lockfile_version
        && let Some(major) = version.major()
        && major < 9
    {
        return Err(CoreError::LockUnreadable(format!(
            "pnpm-lock.yaml: unsupported lockfileVersion {found}; cooldown supports the v9 lock \
             format (lockfileVersion 9, written by pnpm 9+) — re-run `pnpm install` with a \
             current pnpm to migrate the lock",
            found = version.0
        )));
    }
    Ok(doc)
}

/// Parses `pnpm-lock.yaml` into any typed view of it under the one set of parser options every
/// reader shares: duplicate keys kept, the size-proportional budget lifted, the structural guards
/// kept.
/// An empty or whitespace-only document is a legitimately empty lock and reads as the default.
fn parse_pnpm_yaml<T: serde::de::DeserializeOwned + Default>(content: &str) -> Result<T> {
    if content.trim().is_empty() {
        return Ok(T::default());
    }
    let mut options = serde_saphyr::Options::default();
    // pnpm never writes duplicate keys, but a hand-edited lock may carry them; `LastWins` passes
    // duplicate pairs through to the map visitor, so [`YamlEntries`] keeps each of them (the
    // default policy errors out instead).
    options.duplicate_keys = serde_saphyr::options::DuplicateKeyPolicy::LastWins;
    // The default budget caps events/nodes/scalar bytes at totals a large monorepo lock can
    // legitimately exceed, and a breach would surface as a spurious parse failure.
    // Lift the size-proportional caps — the input is a project-local file — while keeping the
    // structural guards (nesting depth, alias amplification).
    if let Some(budget) = options.budget.as_mut() {
        budget.max_events = usize::MAX;
        budget.max_nodes = usize::MAX;
        budget.max_total_scalar_bytes = usize::MAX;
    }
    serde_saphyr::from_str_with_options(content, options)
        .map_err(|error| CoreError::LockUnreadable(format!("pnpm-lock.yaml: {error}")))
}

/// [`parse_pnpm_document_strict`] with the failure collapsed to `None`, for the auxiliary readers
/// (member attribution, peer requirements, local-package consumers, exact pins).
///
/// Those readers stay fail-open by design: their trait signatures are infallible enrichment of a
/// report — a lock the strict parse rejects yields no attribution rather than a second error on
/// top of the one [`Pnpm::parse`] already surfaces for the same content.
pub(crate) fn parse_pnpm_document(content: &str) -> Option<PnpmLockDocument> {
    parse_pnpm_document_strict(content).ok()
}

/// Parses the `packages:` section of `pnpm-lock.yaml` (v9, enforced by
/// [`parse_pnpm_document_strict`]) into every resolved package's peer requirements: each package
/// entry's `peerDependencies:` map (name → range).
///
/// A `(peer@x)` disambiguation suffix on the entry key is stripped, so one package resolved under
/// several peer contexts reports its requirement once per resolved copy — duplicates are harmless
/// to the gate.
/// Optional peers are reported too (see [`PeerRequirement`]).
fn parse_pnpm_peer_requirements(content: &str) -> Vec<PeerRequirement> {
    let Some(doc) = parse_pnpm_document(content) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, package) in doc.packages.entries() {
        let Some(package) = package.known() else {
            continue;
        };
        // Balanced trailing-group strip, not a truncate at the first `(`: an injected
        // key's `file:` path may itself contain parenthesized directory segments.
        let Some(dependent) = split_name_version(strip_pnpm_peer_suffixes(key)) else {
            continue;
        };
        for (name, range) in package.peer_dependencies.entries() {
            let Some(range) = range.known() else {
                continue;
            };
            if !range.is_empty() {
                out.push(PeerRequirement {
                    dependent: dependent.name.clone(),
                    dependent_version: dependent.version.clone(),
                    package: name.to_string(),
                    range: range.clone(),
                });
            }
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
    /// Where the lock records this entry as coming from — the positive origin evidence advisory
    /// identity requires.
    pub origin: LockOrigin,
}

/// What a lock records about where one resolved entry came from.
///
/// Advisory identity needs *positive* origin evidence: an OSV ecosystem names the packages of one
/// public registry, and a same-named package from another registry is a different package.
/// Each lock format proves origin as far as it can, and the adapter grants identity only from
/// proof.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockOrigin {
    /// No per-entry record: a format that keeps none (bun), or an entry without one (a link or
    /// bundled entry, an injected workspace copy, a git or tarball resolution).
    /// Nothing is proven, so no identity is granted.
    Unrecorded,
    /// The URL the artifact was fetched from — npm's and yarn classic's `resolved`, which name the
    /// registry per entry (the integrity hash pins the artifact to what that registry served).
    Url(String),
    /// A registry entry whose registry the lock does not name: pnpm's `resolution: {integrity: …}`
    /// shape, written only for an artifact fetched from the *configured* registry by package name
    /// (anything else carries a `tarball`, `repo`, `directory`, or URL field).
    /// Which registry is the manager's effective `registry` setting for the package's scope, so the
    /// identity is granted provisionally and the feed-time confirmation
    /// ([`EffectiveRegistryQuery::Proves`]) supplies the other half of the proof.
    ConfiguredRegistry,
}

/// How a manager's effective-configuration query bears on advisory identity when the feed runs (see
/// [`confirm_advisory_identities`](cooldown_core::ToolRead::confirm_advisory_identities)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveRegistryQuery {
    /// No reliably parseable query exists — yarn classic merges `.yarnrc` files up the directory
    /// tree — so every identity is withheld at feed time.
    Unavailable,
    /// The lock names each entry's registry itself ([`LockOrigin::Url`]); the query can only *veto*
    /// an identity whose package the effective routing sends elsewhere (npm).
    Vetoes,
    /// The lock only says "the configured registry" ([`LockOrigin::ConfiguredRegistry`]); the query
    /// must *state* the effective `registry` and it must be the public one, and no scope override
    /// may reroute the package.
    /// Failing that — a failed query, an unstated registry, an unreadable value — withholds every
    /// identity, never grants one (pnpm).
    Proves,
}

impl NameVersion {
    /// Builds the pair from its already-split halves, with no recorded origin.
    pub(crate) fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            origin: LockOrigin::Unrecorded,
        }
    }
}

/// Splits a `name@version` (or scoped `@scope/name@version`) specifier into its parts. The version
/// is taken after the last `@`, so the leading `@` of a scope is preserved in the name.
pub(crate) fn split_name_version(spec: &str) -> Option<NameVersion> {
    let at = spec.rfind('@').filter(|&i| i > 0)?;
    let (name, version) = spec.split_at(at);
    Some(NameVersion::new(name, &version[1..]))
}

/// Undoes the quoting of one YAML scalar.
///
/// Test-only: production code decodes scalars through the YAML parser itself, and the
/// [`decode_pnpm_importer_path`] test keeps this helper alive to pin the quoting rules importer
/// keys must follow.
#[cfg(test)]
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
///
/// Test-only, like [`unquote_yaml_scalar`]: its test pins the decoding the YAML parser must apply
/// to importer keys (`''` unescapes inside single quotes only).
#[cfg(test)]
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
    const EFFECTIVE_REGISTRY: EffectiveRegistryQuery = EffectiveRegistryQuery::Vetoes;

    fn parse(content: &str) -> Result<Vec<NameVersion>> {
        parse_npm(content)
    }

    fn member_sources_excluding(content: &str, excluded: &HashSet<String>) -> MemberIndex {
        parse_npm_member_sources(content, excluded)
            .map(|by_name| {
                MemberIndex::name_only(by_name)
                    .with_exact_names(parse_npm_exact_pins(content, excluded))
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
    const SUPPORTS_TRANSITIVE_ADVANCE: bool = true;
    const EFFECTIVE_REGISTRY: EffectiveRegistryQuery = EffectiveRegistryQuery::Proves;

    fn parse(content: &str) -> Result<Vec<NameVersion>> {
        parse_pnpm(content)
    }

    fn member_sources_excluding(content: &str, excluded: &HashSet<String>) -> MemberIndex {
        MemberIndex::version_exact(parse_pnpm_importer_members(content, excluded))
            .with_exact_versions(parse_pnpm_exact_pins(content, excluded))
            .with_declared_specifiers(parse_pnpm_importer_specifiers(content, excluded))
            .with_importer_entries(parse_pnpm_importer_entries(content, excluded))
    }

    fn peer_requirements(content: &str) -> Vec<PeerRequirement> {
        parse_pnpm_peer_requirements(content)
    }

    fn catalog_managed_names(content: &str, excluded: &HashSet<String>) -> HashSet<String> {
        parse_pnpm_catalog_only_names(content, excluded)
    }

    fn member_paths(content: &str) -> HashSet<String> {
        pnpm_importer_paths(content)
    }

    fn local_package_consumers(content: &str) -> HashMap<String, Vec<String>> {
        parse_pnpm_local_package_consumers(content)
    }

    fn graph_dependents(content: &str) -> HashMap<(String, String), Vec<String>> {
        parse_pnpm_graph_dependents(content)
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
                out.push(NameVersion {
                    name: name.to_string(),
                    version: version.to_string(),
                    origin: lock_entry_origin(val),
                });
            }
        }
    } else if let Some(deps) = doc.get("dependencies").and_then(|v| v.as_object()) {
        for (name, val) in deps {
            if let Some(version) = val.get("version").and_then(|v| v.as_str()) {
                out.push(NameVersion {
                    name: name.clone(),
                    version: version.to_string(),
                    origin: lock_entry_origin(val),
                });
            }
        }
    }
    Ok(out)
}

/// The `resolved` URL of one `package-lock.json` entry — npm's record of where the tarball was
/// actually fetched from. Absent for link/bundled entries (and `false` in v1 locks installed
/// with `--no-save`), which therefore carry no origin evidence.
fn lock_entry_origin(val: &serde_json::Value) -> LockOrigin {
    val.get("resolved")
        .and_then(|v| v.as_str())
        .map_or(LockOrigin::Unrecorded, |url| {
            LockOrigin::Url(url.to_string())
        })
}

/// Parses `pnpm-lock.yaml` (v9, enforced by [`parse_pnpm_document_strict`]): the top-level
/// `packages:` section keys every resolved package by its `name@version(...peers)` identity, and
/// each entry's `resolution:` record says whether the configured registry served it
/// ([`LockOrigin::ConfiguredRegistry`]) or something else did.
/// An entry whose shape the model does not recognize proves nothing and reads as unrecorded.
fn parse_pnpm(content: &str) -> Result<Vec<NameVersion>> {
    let doc = parse_pnpm_document_strict(content)?;
    Ok(doc
        .packages
        .entries()
        .filter_map(|(key, package)| {
            // Drop the `(peer@x)` suffixes pnpm appends to disambiguate peer resolutions — as a
            // balanced trailing-group strip, since an injected key's `file:` path may itself
            // contain parenthesized directory segments.
            let mut entry = split_name_version(strip_pnpm_peer_suffixes(key))?;
            let registry = package
                .known()
                .and_then(|package| package.resolution.as_ref())
                .is_some_and(PnpmResolution::is_registry);
            if registry {
                entry.origin = LockOrigin::ConfiguredRegistry;
            }
            Some(entry)
        })
        .collect())
}

/// The dependency-group keys a manifest/importer uses to declare a direct dependency.
const DIRECT_GROUPS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

/// Walks the `importers:` section of a pnpm lockfile and calls `visit` once per direct-dependency
/// entry with `(importer_path, dep_name, specifier, version)` — importers and entries in file
/// order, the groups of one importer in [`DIRECT_GROUPS`] order.
///
/// `specifier`/`version` are the entry's scalar values (`None` when the entry lacks that field).
/// Entry-level delivery is order-agnostic within the entry, so consumers need no
/// specifier-before-version assumption.
/// The four importer parsers share this so the document traversal lives once.
fn walk_pnpm_importer_entries(
    content: &str,
    visit: impl FnMut(&str, &str, Option<&str>, Option<&str>),
) {
    if let Some(doc) = parse_pnpm_document(content) {
        walk_pnpm_document_importers(&doc, visit);
    }
}

/// [`walk_pnpm_importer_entries`] with the importers at `excluded` paths skipped entirely, so a
/// consumer building workspace evidence never sees a declaration the run's exclude policy dropped.
fn walk_pnpm_importer_entries_excluding(
    content: &str,
    excluded: &HashSet<String>,
    mut visit: impl FnMut(&str, &str, Option<&str>, Option<&str>),
) {
    walk_pnpm_importer_entries(content, |member, name, specifier, version| {
        if !excluded.contains(member) {
            visit(member, name, specifier, version);
        }
    });
}

/// [`walk_pnpm_importer_entries`] over an already-parsed document, for callers that read further
/// sections out of the same parse.
fn walk_pnpm_document_importers(
    doc: &PnpmLockDocument,
    mut visit: impl FnMut(&str, &str, Option<&str>, Option<&str>),
) {
    for (path, importer) in doc.importers.entries() {
        let Some(importer) = importer.known() else {
            continue;
        };
        // A crafted or stale importer key could name a path outside the workspace, and
        // every consumer of these paths joins them onto the project root — reject
        // non-workspace-relative keys once, here at the parse boundary.
        if !is_workspace_relative(path) {
            continue;
        }
        for group in importer.groups() {
            for (name, entry) in group.entries() {
                if name.is_empty() {
                    continue;
                }
                let (specifier, version) = match entry.known() {
                    Some(entry) => (entry.specifier.as_deref(), entry.version.as_deref()),
                    // An entry shape the model does not recognize (a pre-v9 scalar) still names
                    // a dependency; it is delivered without specifier/version, which every
                    // consumer skips.
                    None => (None, None),
                };
                visit(path, name, specifier, version);
            }
        }
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
    let Some(doc) = parse_pnpm_document(content) else {
        return HashMap::new();
    };
    let importers = importer_paths_of(&doc);
    let directories = injected_directories_of(&doc);
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    walk_pnpm_document_importers(&doc, |member, name, _specifier, version| {
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
    parse_pnpm_document(content)
        .map(|doc| importer_paths_of(&doc))
        .unwrap_or_default()
}

/// [`pnpm_importer_paths`] over an already-parsed document.
/// Keys count whatever their value's shape — an importer with no dependencies (`path: {}`) is a
/// member all the same.
fn importer_paths_of(doc: &PnpmLockDocument) -> HashSet<String> {
    doc.importers
        .keys()
        .filter(|path| is_workspace_relative(path))
        .map(str::to_string)
        .collect()
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
fn injected_directories_of(doc: &PnpmLockDocument) -> InjectedDirectories {
    let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (key, package) in doc.packages.entries() {
        let Some(resolution) = package
            .known()
            .and_then(|package| package.resolution.as_ref())
        else {
            continue;
        };
        if resolution.kind.as_deref() != Some("directory") {
            continue;
        }
        let Some(directory) = resolution.directory.as_deref() else {
            continue;
        };
        // The first `@file:` is the name/reference boundary: a scope's `@` sits at the
        // very start of the name, and a name can never contain `:`.
        let Some((name, reference)) = key.split_once("@file:") else {
            continue;
        };
        if !name.is_empty() && is_workspace_relative(directory) {
            map.entry(name.to_string())
                .or_default()
                .push((reference.to_string(), directory.to_string()));
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

/// Maps each resolved `(name, version)` dependency to the workspace member importers that declare
/// it, read from `pnpm-lock.yaml`'s `importers:` section.
/// The resolved `version:` line under each dependency gives the exact version (its `(peer)` suffix
/// stripped to match the `packages:` keys); internal `link:`/`file:`/`workspace:` versions are
/// skipped — they are not registry packages.
/// Importer paths (the workspace root is `.`) name the source packages; those in `excluded` are
/// left out.
fn parse_pnpm_importer_members(
    content: &str,
    excluded: &HashSet<String>,
) -> HashMap<(String, String), Vec<String>> {
    let mut map: HashMap<(String, String), Vec<String>> = HashMap::new();
    walk_pnpm_importer_entries_excluding(content, excluded, |member, name, _specifier, version| {
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

/// Every requirement edge in the lock's `snapshots:` section, keyed by the required
/// `(name, version)` and listing the dependents as `name@version`, sorted and deduplicated — the
/// index that names which package's own requirement pulled a second copy into the graph.
/// `link:`/`file:`/`workspace:` values and URL resolutions bind no registry version and are left
/// out; a value under an aliased key (`real@1.2.3`) is credited to the real package.
fn parse_pnpm_graph_dependents(content: &str) -> HashMap<(String, String), Vec<String>> {
    // A pre-v9 lock has no `snapshots:` section and reads as no edges, like the auxiliary readers.
    let Ok(doc) = parse_pnpm_yaml::<PnpmSnapshotsDocument>(content) else {
        return HashMap::new();
    };
    let mut dependents: HashMap<(String, String), BTreeSet<String>> = HashMap::new();
    for (key, snapshot) in doc.snapshots.entries() {
        let Some(snapshot) = snapshot.known() else {
            continue;
        };
        let Some(dependent) = split_name_version(strip_pnpm_peer_suffixes(key)) else {
            continue;
        };
        let dependent = format!("{}@{}", dependent.name, dependent.version);
        for group in [&snapshot.dependencies, &snapshot.optional_dependencies] {
            for (name, value) in group.entries() {
                let Some(value) = value.known() else {
                    continue;
                };
                if let Some(required) = snapshot_edge_target(name, value) {
                    dependents
                        .entry(required)
                        .or_default()
                        .insert(dependent.clone());
                }
            }
        }
    }
    dependents
        .into_iter()
        .map(|(required, names)| (required, names.into_iter().collect()))
        .collect()
}

/// The `(name, version)` one snapshot edge `name: value` binds, or `None` for a value that is no
/// registry version.
/// An alias value carries the real package (`real@1.2.3`, told from a URL by its digit-led
/// version); anything else is the named package at the value's version, peer suffix stripped.
fn snapshot_edge_target(name: &str, value: &str) -> Option<(String, String)> {
    if value.starts_with("link:") || value.starts_with("file:") || value.starts_with("workspace:") {
        return None;
    }
    let value = strip_pnpm_peer_suffixes(value);
    if value.is_empty() {
        return None;
    }
    if let Some(real) = split_name_version(value)
        .filter(|real| real.version.starts_with(|c: char| c.is_ascii_digit()))
    {
        return Some((real.name, real.version));
    }
    (!is_url_resolution(value)).then(|| (name.to_string(), value.to_string()))
}

/// Whether an importer's resolved `version:` is a URL rather than a registry version: pnpm records
/// a git or tarball dependency as the fetch location (`https://codeload.github.com/…`,
/// `github.com/user/repo/<sha>`), which a semver version never contains.
fn is_url_resolution(version: &str) -> bool {
    version.contains(':') || version.contains('/')
}

/// Each importer's (those not in `excluded`) registry-resolved direct entries by name, as
/// `pnpm-lock.yaml` records them: the declared `specifier:` and the resolved `version:` with its
/// peer suffix stripped.
/// `link:`/`file:`/`workspace:` versions are layout facts, not versions, and are left out like
/// everywhere in the index.
/// An importer listing one name in two dependency groups keeps the first group's record, so the
/// map never depends on hash order.
fn parse_pnpm_importer_entries(
    content: &str,
    excluded: &HashSet<String>,
) -> HashMap<String, BTreeMap<String, ImporterEntry>> {
    let mut entries: HashMap<String, BTreeMap<String, ImporterEntry>> = HashMap::new();
    walk_pnpm_importer_entries_excluding(content, excluded, |member, name, specifier, version| {
        let Some(value) = version else {
            return;
        };
        if value.starts_with("link:")
            || value.starts_with("file:")
            || value.starts_with("workspace:")
        {
            return;
        }
        let version = value.split('(').next().unwrap_or(value);
        if version.is_empty() {
            return;
        }
        entries
            .entry(member.to_string())
            .or_default()
            .entry(name.to_string())
            .or_insert_with(|| ImporterEntry {
                specifier: specifier.map(str::to_string),
                version: version.to_string(),
            });
    });
    entries
}

/// Maps each dependency name to the set of distinct range *specifiers* its workspace-member
/// importers (those not in `excluded`) declare, read from `pnpm-lock.yaml`'s `importers:` section
/// (each dependency records a `specifier:` line — the declared range).
/// Whether disagreeing specifiers (`~7.3.0` vs `^7.0.0`, `"<4"` vs `^4`) hold a name out of the
/// joint update depends on the target ([`MemberIndex::splits_for`]).
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
fn parse_pnpm_importer_specifiers(
    content: &str,
    excluded: &HashSet<String>,
) -> HashMap<String, HashSet<String>> {
    let mut map: HashMap<String, HashSet<String>> = HashMap::new();
    let mut recorded: HashSet<(String, String)> = HashSet::new();
    walk_pnpm_importer_entries_excluding(content, excluded, |member, name, specifier, _version| {
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

/// The dependency names every declaring importer manages through a pnpm catalog (`catalog:` /
/// `catalog:<name>` specifier), read from the lock's `importers:` section (the lock copies each
/// manifest's verbatim specifier).
///
/// A name any importer outside `excluded` also declares with a non-catalog specifier is left out:
/// the joint update can still land that importer's copy, so the candidate keeps the normal resolve
/// path.
/// Only the registry-candidate protocol needs this: a `workspace:`-declared dependency resolves to
/// a `link:`/`file:` version, which the resolved-package readers already skip, so it never becomes
/// an upgrade candidate in the first place.
fn parse_pnpm_catalog_only_names(content: &str, excluded: &HashSet<String>) -> HashSet<String> {
    let mut catalog: HashSet<String> = HashSet::new();
    let mut otherwise: HashSet<String> = HashSet::new();
    walk_pnpm_importer_entries_excluding(
        content,
        excluded,
        |_member, name, specifier, _version| {
            let Some(specifier) = specifier else {
                return;
            };
            if specifier.starts_with("catalog:") {
                catalog.insert(name.to_string());
            } else {
                otherwise.insert(name.to_string());
            }
        },
    );
    catalog.retain(|name| !otherwise.contains(name));
    catalog
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
/// `package-lock.json`'s `packages` map.
/// Member entries — the root `""` and any key not under `node_modules/` — list their direct deps as
/// ranges, not resolved versions, so attribution is by name (applied to every resolved version of
/// that name).
/// Members are keyed by their workspace path (the root as `.`), matching pnpm's importer paths;
/// those in `excluded` are left out.
fn parse_npm_member_sources(
    content: &str,
    excluded: &HashSet<String>,
) -> Option<HashMap<String, Vec<String>>> {
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
        if !is_workspace_relative(member) || excluded.contains(member) {
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

/// The `(name, version)` pairs every declaring importer (outside `excluded`) pins exactly in
/// `pnpm-lock.yaml`.
/// The importer records both the `specifier:` (the declared range) and the resolved `version:`; a
/// `(name, version)` is exact-pinned only when *every* importer that declares it used an exact
/// specifier (otherwise some importer's range could still move it).
fn parse_pnpm_exact_pins(content: &str, excluded: &HashSet<String>) -> HashSet<(String, String)> {
    let mut total: HashMap<(String, String), usize> = HashMap::new();
    let mut exact: HashMap<(String, String), usize> = HashMap::new();
    walk_pnpm_importer_entries_excluding(content, excluded, |_member, name, specifier, version| {
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

/// The dependency names every declaring member (outside `excluded`) pins exactly in
/// `package-lock.json`.
/// npm records a range (not a resolved version) per member, so this is name-keyed: a name is pinned
/// only when every member entry that declares it used an exact specifier.
fn parse_npm_exact_pins(content: &str, excluded: &HashSet<String>) -> HashSet<String> {
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
        let member = if key.is_empty() { "." } else { key.as_str() };
        if excluded.contains(member) {
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
    // The entry's fields arrive line by line (`version` before `resolved` in yarn's own output,
    // but the order is not load-bearing here): the names flush on `version`, and a later
    // `resolved` back-fills the rows just flushed — they are the tail of `out`.
    let mut flushed_at = 0;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("  version ") {
            let version = rest.trim().trim_matches('"');
            flushed_at = out.len();
            for name in pending.drain(..) {
                out.push(NameVersion::new(name, version));
            }
        } else if let Some(rest) = line.strip_prefix("  resolved ") {
            // `resolved "https://registry.yarnpkg.com/…/lodash-4.17.21.tgz#<hash>"` — the
            // fragment is yarn's checksum, not part of the origin URL.
            let url = rest.trim().trim_matches('"');
            let url = url.split('#').next().unwrap_or(url);
            for entry in out.get_mut(flushed_at..).unwrap_or_default() {
                entry.origin = LockOrigin::Url(url.to_string());
            }
        } else if !line.starts_with([' ', '#']) && line.trim_end().ends_with(':') {
            flushed_at = out.len();
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
                version: "4.17.15".into(),
                origin: LockOrigin::Unrecorded,
            })
        );
        assert_eq!(
            split_name_version("@babel/core@7.1.0"),
            Some(NameVersion {
                name: "@babel/core".into(),
                version: "7.1.0".into(),
                origin: LockOrigin::Unrecorded,
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

    /// The per-entry `resolved` URL is the advisory-identity origin evidence, so both npm lock
    /// generations must surface it verbatim — and an entry without one must stay `None`.
    #[test]
    fn npm_entries_carry_their_resolved_urls() {
        let lock = indoc! {r#"
            {
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root", "version": "0.1.0" },
                    "node_modules/lodash": { "version": "4.17.15", "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.15.tgz" },
                    "node_modules/private-api": { "version": "1.0.0", "resolved": "https://npm.corp.example/private-api/-/private-api-1.0.0.tgz" },
                    "node_modules/unrecorded": { "version": "2.0.0" }
                }
            }"#};
        let entries = parse_npm(lock).expect("parse");
        let origin_of = |name: &str| {
            entries
                .iter()
                .find(|entry| entry.name == name)
                .expect("entry")
                .origin
                .clone()
        };
        assert_eq!(
            origin_of("lodash"),
            LockOrigin::Url("https://registry.npmjs.org/lodash/-/lodash-4.17.15.tgz".into())
        );
        assert_eq!(
            origin_of("private-api"),
            LockOrigin::Url("https://npm.corp.example/private-api/-/private-api-1.0.0.tgz".into())
        );
        assert_eq!(origin_of("unrecorded"), LockOrigin::Unrecorded);

        let v1 = indoc! {r#"
            {
                "lockfileVersion": 1,
                "dependencies": {
                    "lodash": { "version": "4.17.15", "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.15.tgz" }
                }
            }"#};
        let entries = parse_npm(v1).expect("parse v1");
        assert_eq!(
            entries[0].origin,
            LockOrigin::Url("https://registry.npmjs.org/lodash/-/lodash-4.17.15.tgz".into())
        );
    }

    /// yarn classic's `resolved` line carries the origin URL with the checksum as a fragment;
    /// the URL (fragment stripped) back-fills every name the entry's ranges collapsed to.
    #[test]
    fn yarn_entries_carry_their_resolved_urls() {
        let lock = indoc! {r#"
            # yarn lockfile v1

            lodash@^4.17.0, lodash@~4.17.20:
              version "4.17.21"
              resolved "https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz#679591c"
              integrity sha512-x

            "@corp/api@^1.0.0":
              version "1.0.0"
              resolved "https://npm.corp.example/@corp/api/-/api-1.0.0.tgz#abc"

            unrecorded@^2.0.0:
              version "2.0.0"
        "#};
        let entries = parse_yarn(lock);
        let origin_of = |name: &str| {
            entries
                .iter()
                .find(|entry| entry.name == name)
                .expect("entry")
                .origin
                .clone()
        };
        assert_eq!(
            origin_of("lodash"),
            LockOrigin::Url("https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz".into()),
            "the checksum fragment is not part of the origin"
        );
        assert_eq!(
            origin_of("@corp/api"),
            LockOrigin::Url("https://npm.corp.example/@corp/api/-/api-1.0.0.tgz".into())
        );
        assert_eq!(origin_of("unrecorded"), LockOrigin::Unrecorded);
    }

    fn registry_entry(name: &str, version: &str) -> NameVersion {
        NameVersion {
            name: name.into(),
            version: version.into(),
            origin: LockOrigin::ConfiguredRegistry,
        }
    }

    #[test]
    fn pnpm_packages_section() {
        let lock = "lockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      lodash:\n        specifier: 4.17.15\n        version: 4.17.15\n\npackages:\n\n  lodash@4.17.15:\n    resolution: {integrity: sha512-x}\n\n  '@babel/core@7.1.0':\n    resolution: {integrity: sha512-y}\n\n  chalk@4.0.0(supports-color@7.2.0):\n    resolution: {integrity: sha512-z}\n";
        assert_eq!(
            sorted(parse_pnpm(lock).unwrap()),
            sorted(vec![
                registry_entry("lodash", "4.17.15"),
                registry_entry("@babel/core", "7.1.0"),
                registry_entry("chalk", "4.0.0"),
            ])
        );
    }

    /// pnpm names no registry per entry, but the *shape* of `resolution:` says whether the
    /// configured registry served the artifact by name — only its integrity hash — or something
    /// else did: a tarball URL (a custom registry, or a plain URL dependency), a git repo and
    /// commit, an injected directory.
    /// Only the former is origin evidence; every other shape, an unmodeled key included, reads as
    /// unrecorded.
    #[test]
    fn pnpm_origin_is_the_configured_registry_only_for_an_integrity_only_resolution() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            packages:

              lodash@4.17.21:
                resolution: {integrity: sha512-a}

              private-api@1.0.0:
                resolution: {integrity: sha512-b, tarball: https://npm.corp.example/private-api/-/private-api-1.0.0.tgz}

              pinned-git@1.2.0:
                resolution: {commit: abc123, repo: https://github.com/user/pinned-git.git, type: git}

              local-shim@file:packages/shim:
                resolution: {directory: packages/shim, type: directory}

              future@2.0.0:
                resolution: {integrity: sha512-c, mirror: https://mirror.example/}

              bare@3.0.0:
                engines: {node: '>=18'}
        "};
        let entries = parse_pnpm(lock).expect("parse");
        let origin_of = |name: &str| {
            entries
                .iter()
                .find(|entry| entry.name == name)
                .expect("entry")
                .origin
                .clone()
        };
        assert_eq!(origin_of("lodash"), LockOrigin::ConfiguredRegistry);
        assert_eq!(
            origin_of("private-api"),
            LockOrigin::Unrecorded,
            "a tarball URL names another source"
        );
        assert_eq!(origin_of("pinned-git"), LockOrigin::Unrecorded);
        assert_eq!(origin_of("local-shim"), LockOrigin::Unrecorded);
        assert_eq!(
            origin_of("future"),
            LockOrigin::Unrecorded,
            "an unmodeled resolution key is not proof of a registry fetch"
        );
        assert_eq!(
            origin_of("bare"),
            LockOrigin::Unrecorded,
            "no resolution record, no evidence"
        );
    }

    /// A v6 lock (`lockfileVersion: '6.0'`, pnpm 8) keys `packages:` as `/name@version`; parsing
    /// it as v9 would keep the leading slash and send `/lodash` to the registry. It must be
    /// rejected with an error naming the found version and the supported one.
    #[test]
    fn pnpm_v6_lock_is_rejected_with_a_clear_error() {
        let lock = indoc! {"
            lockfileVersion: '6.0'

            dependencies:
              lodash:
                specifier: ^4.17.0
                version: 4.17.21

            packages:

              /lodash@4.17.21:
                resolution: {integrity: sha512-x}
        "};
        let error = parse_pnpm(lock).unwrap_err().to_string();
        assert!(error.contains("6.0"), "names the found version: {error}");
        assert!(
            error.contains("lockfileVersion 9"),
            "names the supported version: {error}"
        );
        assert!(
            error.contains("pnpm-lock.yaml"),
            "names the offending file: {error}"
        );
    }

    /// A v5 lock writes `lockfileVersion` as a bare YAML number (`5.4`), not a quoted string; the
    /// guard must reject that spelling too.
    #[test]
    fn pnpm_v5_lock_is_rejected_with_a_clear_error() {
        let lock = indoc! {"
            lockfileVersion: 5.4

            dependencies:
              lodash: 4.17.21

            packages:

              /lodash/4.17.21:
                resolution: {integrity: sha512-x}
        "};
        let error = parse_pnpm(lock).unwrap_err().to_string();
        assert!(error.contains("5.4"), "names the found version: {error}");
        assert!(
            error.contains("lockfileVersion 9"),
            "names the supported version: {error}"
        );
    }

    /// A malformed lock must fail closed like npm's: reporting zero dependencies for an
    /// unparsable document would make `outdated`/`dependencies` look healthy on a corrupted
    /// project.
    #[test]
    fn malformed_pnpm_lock_is_an_error_not_an_empty_graph() {
        let lock = indoc! {"
            lockfileVersion: '9.0'
            packages:
              lodash@4.17.21: [unclosed
        "};
        let error = parse_pnpm(lock).unwrap_err();
        assert!(
            matches!(error, CoreError::LockUnreadable(_)),
            "typed lock error, got: {error:?}"
        );
    }

    /// Legitimately empty locks keep working: an empty document and a v9 header with no sections
    /// both read as zero dependencies, not as errors.
    #[test]
    fn empty_pnpm_locks_parse_to_no_dependencies() {
        assert_eq!(parse_pnpm("").unwrap(), Vec::new());
        assert_eq!(parse_pnpm("\n\n").unwrap(), Vec::new());
        assert_eq!(parse_pnpm("lockfileVersion: '9.0'\n").unwrap(), Vec::new());
    }

    /// The fail-open auxiliary readers collapse a pre-v9 lock to their empty result instead of
    /// misreading its `/name@version` keys — the strict parse's error already surfaces through
    /// `Pnpm::parse` for the same content.
    #[test]
    fn auxiliary_readers_yield_nothing_for_a_pre_v9_lock() {
        let lock = indoc! {"
            lockfileVersion: '6.0'

            packages:

              /lodash@4.17.21:
                resolution: {integrity: sha512-x}
                peerDependencies:
                  react: ^18
        "};
        assert!(parse_pnpm_peer_requirements(lock).is_empty());
        assert!(parse_pnpm_importer_members(lock, &HashSet::new()).is_empty());
    }

    /// Only names *every* declaring importer manages through the catalog count as
    /// catalog-managed; a name one importer declares with a plain range keeps the normal resolve
    /// path, since that importer's copy can still land.
    #[test]
    fn catalog_managed_names_require_every_declaring_importer_to_use_the_catalog() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  react:
                    specifier: 'catalog:'
                    version: 18.3.1
                  chalk:
                    specifier: 'catalog:legacy'
                    version: 4.1.2

              apps/web:
                dependencies:
                  chalk:
                    specifier: ^4.0.0
                    version: 4.1.2

            packages:

              react@18.3.1:
                resolution: {integrity: sha512-x}
              chalk@4.1.2:
                resolution: {integrity: sha512-y}
        "};
        let names = parse_pnpm_catalog_only_names(lock, &HashSet::new());
        assert!(names.contains("react"), "catalog-only name detected");
        assert!(
            !names.contains("chalk"),
            "a plain-range declaration keeps the name off the catalog hold"
        );
        // With `apps/web` excluded its plain range is not the run's to land, so the root's
        // catalog-only `chalk` gets its truthful hold instead of an eternal resolver conflict.
        let excluded = HashSet::from(["apps/web".to_string()]);
        assert!(parse_pnpm_catalog_only_names(lock, &excluded).contains("chalk"));
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
                NameVersion {
                    name: "lodash".into(),
                    version: "4.17.15".into(),
                    origin: LockOrigin::Url("https://x".into()),
                },
                NameVersion {
                    name: "@babel/core".into(),
                    version: "7.1.0".into(),
                    origin: LockOrigin::Url("https://y".into()),
                },
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
        let index = MemberIndex::version_exact(parse_pnpm_importer_members(lock, &HashSet::new()));
        assert_eq!(
            index.members_for("vite", "6.0.0"),
            vec!["apps/a", "packages/x"]
        );
        assert_eq!(index.members_for("vite", "7.0.0"), vec!["apps/b"]);
        // The internal workspace link is not a registry package, so it is never attributed.
        assert!(index.members_for("@airtype/api", "0.0.0").is_empty());
    }

    #[test]
    fn a_split_ignores_transitive_duplicates() {
        // `bar` is a genuine workspace split: apps/b declares `^2.0.0`, apps/c `^3.0.0`, and no
        // single target satisfies both — neither line may be collapsed.
        // `foo` is declared at a SINGLE version by importers but ALSO appears as a transitive copy
        // at 2.0.0 in `packages:` — it must NOT split, so it stays exact-pinned and keeps its
        // per-package window and any out-of-range widen.
        // Counting the whole resolved graph (the old behavior) would wrongly float `foo`.
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
        let index = Pnpm::member_sources(lock);
        assert!(
            index.splits_for("bar", "3.1.0"),
            "bar is declared at ^2.0.0 and ^3.0.0 across importers — a genuine split for a v3 target"
        );
        assert!(
            !index.splits_for("foo", "1.4.0"),
            "foo is declared at one version by importers; its transitive 2.0.0 copy must not split it"
        );
    }

    /// A git or tarball resolution in one importer is not a version line: the name keeps its one
    /// registry line, so a target every plain range admits still lands, and the line stays visible
    /// to the resolver-introduced-split guard.
    /// The dependents index reads the v9 `snapshots:` edges: a peer-suffixed key and value are
    /// reduced to `name@version`, an aliased edge is credited to the real package, and
    /// `link:` and URL values bind nothing.
    #[test]
    fn graph_dependents_read_the_snapshot_edges() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  vite-plugin-solid:
                    specifier: ^2.11.0
                    version: 2.11.14(solid-js@1.9.15)

            packages:

              solid-js@1.9.15:
                resolution: {integrity: sha512-a}

              vite-plugin-solid@2.11.14:
                resolution: {integrity: sha512-b}

            snapshots:

              solid-js@1.9.15:
                dependencies:
                  seroval: 1.3.2

              vite-plugin-solid@2.11.14(solid-js@1.9.15):
                dependencies:
                  solid-js: 1.9.15
                  my-lodash: lodash@4.17.21
                  local: link:../local
                  patched: https://codeload.github.com/x/y/tar.gz/abc
                optionalDependencies:
                  fsevents: 2.3.3
        "};
        let dependents = Pnpm::graph_dependents(lock);
        let of = |name: &str, version: &str| {
            dependents
                .get(&(name.to_string(), version.to_string()))
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(of("solid-js", "1.9.15"), vec!["vite-plugin-solid@2.11.14"]);
        assert_eq!(of("seroval", "1.3.2"), vec!["solid-js@1.9.15"]);
        assert_eq!(of("lodash", "4.17.21"), vec!["vite-plugin-solid@2.11.14"]);
        assert_eq!(of("fsevents", "2.3.3"), vec!["vite-plugin-solid@2.11.14"]);
        assert!(of("my-lodash", "lodash@4.17.21").is_empty());
        assert!(
            !dependents
                .keys()
                .any(|(name, _)| name == "local" || name == "patched"),
            "layout facts and URL resolutions bind no version: {dependents:?}"
        );
    }

    #[test]
    fn a_url_resolved_importer_entry_is_not_a_version_line() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              pkgs/a:
                dependencies:
                  foo:
                    specifier: ^1.2.0
                    version: 1.2.3

              pkgs/b:
                dependencies:
                  foo:
                    specifier: github:x/foo#abc
                    version: https://codeload.github.com/x/foo/tar.gz/abc

            packages:

              foo@1.2.3:
                resolution: {integrity: sha512-a}
              foo@https://codeload.github.com/x/foo/tar.gz/abc:
                resolution: {tarball: https://codeload.github.com/x/foo/tar.gz/abc}
        "};
        let index = Pnpm::member_sources(lock);
        assert_eq!(index.resolved_versions_of("foo"), vec!["1.2.3"]);
        assert!(!index.splits_for("foo", "1.2.4"));
        // The URL-resolved importer still has its attribution; only the line count ignores it.
        assert_eq!(
            index.resolved_version("pkgs/b", "foo"),
            Some("https://codeload.github.com/x/foo/tar.gz/abc")
        );
    }

    /// The acceptance matrix for target-aware splits: disagreeing ranges or several resolved lines
    /// split a name only when some declared range excludes (or cannot be judged against) the target
    /// being pinned.
    #[test]
    fn a_split_is_judged_against_the_target() {
        let lock = indoc! {"
            importers:

              pkgs/a:
                dependencies:
                  lodash:
                    specifier: ^4.17.20
                    version: 4.17.21
                  mongoose:
                    specifier: ^9.8.0
                    version: 9.8.0
                  semver:
                    specifier: ~7.3.0
                    version: 7.3.8
                  tailwindcss:
                    specifier: ^3.4.19
                    version: 3.4.19
                  react:
                    specifier: ^18.0.0 || ^19.0.0
                    version: 18.3.1
                  pinned:
                    specifier: 2.11.13
                    version: 2.11.13

              pkgs/b:
                dependencies:
                  lodash:
                    specifier: ^4.17.21
                    version: 4.17.21
                  mongoose:
                    specifier: ^9.8.0
                    version: 9.9.1
                  semver:
                    specifier: ^7.0.0
                    version: 7.3.8
                  tailwindcss:
                    specifier: ^4.3.3
                    version: 4.3.3
                  react:
                    specifier: ^18.0.0
                    version: 18.3.1
                  pinned:
                    specifier: ^2.11.12
                    version: 2.11.13

            packages:

              lodash@4.17.21:
                resolution: {integrity: sha512-a}
              mongoose@9.8.0:
                resolution: {integrity: sha512-b}
              mongoose@9.9.1:
                resolution: {integrity: sha512-c}
              semver@7.3.8:
                resolution: {integrity: sha512-d}
              tailwindcss@3.4.19:
                resolution: {integrity: sha512-e}
              tailwindcss@4.3.3:
                resolution: {integrity: sha512-f}
              react@18.3.1:
                resolution: {integrity: sha512-g}
              pinned@2.11.13:
                resolution: {integrity: sha512-h}
        "};
        let index = Pnpm::member_sources(lock);
        assert!(
            !index.splits_for("lodash", "4.17.22"),
            "two ranges that both admit the target are no split: the pin lands and no manifest changes"
        );
        assert!(
            !index.splits_for("mongoose", "9.9.3"),
            "one range resolved at two versions with the target in range lands"
        );
        assert!(
            index.splits_for("semver", "7.4.0"),
            "~7.3.0 excludes 7.4.0, so the tilde member would be dragged off its range"
        );
        assert!(
            !index.splits_for("semver", "7.3.8"),
            "the same ranges both admit 7.3.8"
        );
        assert!(
            index.splits_for("tailwindcss", "4.3.3"),
            "^3.4.19 excludes the v4 target"
        );
        assert!(
            index.splits_for("react", "18.3.2"),
            "a `||` union the range parser cannot represent keeps the split (fail closed)"
        );
        assert!(
            index.splits_for("pinned", "2.11.14"),
            "a bare version is an exact npm specifier; it excludes every other target"
        );
    }

    /// An `npm:` alias declares the REAL package under an assumed name: the importer records the
    /// alias with a `real@version` resolution. Both names are importer-declared for the
    /// transitive-advance guard — an override on the real name would drag the alias-declared copy
    /// exactly like a same-name declaration. A git/URL resolution also embeds `@` but must not
    /// mint a phantom declared name.
    #[test]
    fn declared_names_include_the_real_package_behind_an_alias() {
        let lock = indoc! {"
            importers:

              .:
                dependencies:
                  my-lodash:
                    specifier: npm:lodash@^4.17.0
                    version: lodash@4.17.21
                  pinned-git:
                    specifier: github:user/pinned-git
                    version: https://codeload.github.com/user/pinned-git/tar.gz/abc123

            packages:

              lodash@4.17.21:
                resolution: {integrity: sha512-a}
        "};
        let declared = Pnpm::member_sources(lock).declared_names();
        assert!(
            declared.contains("my-lodash"),
            "the alias itself is declared"
        );
        assert!(
            declared.contains("lodash"),
            "the real package behind the alias is declared too: {declared:?}"
        );
        assert!(
            !declared.iter().any(|name| name.contains("codeload")),
            "a git resolution must not mint a phantom name: {declared:?}"
        );
    }

    #[test]
    fn a_specifier_split_at_one_resolved_version_is_held_only_off_the_narrower_range() {
        // `semver` is declared with DIFFERENT ranges (`~7.3.0` and `^7.0.0`) by two importers that
        // the lock resolves to the SAME `7.3.8`.
        // Exact-pinning a target outside `~7.3.0` would drag that member off its own range, so such
        // a target splits the name even though `by_version` sees a single resolved version; a
        // target both ranges admit does not.
        // `chalk` is the control: both importers declare the SAME `^5.0.0` at the same resolved
        // version.
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
        assert!(
            index.splits_for("semver", "7.4.0"),
            "semver is declared at ~7.3.0 and ^7.0.0 — a specifier split for a target the tilde excludes"
        );
        assert!(
            !index.splits_for("semver", "7.3.9"),
            "both ranges admit 7.3.9, so the pin lands everywhere with no manifest change"
        );
        assert!(
            !index.splits_for("chalk", "5.6.0"),
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
        let index = Pnpm::member_sources(lock);
        assert!(
            !index.splits_for("react", "18.3.1"),
            "react's catalog: reference is not a range; with one real specifier it must not split"
        );
        assert!(
            !index.splits_for("next", "14.3.0"),
            "next is declared by a single importer (deps + peer); one importer cannot split itself"
        );
    }

    /// An importer the run excludes contributes nothing to the split evidence: with `legacy` left
    /// out, `mongoose` is declared once and resolved once, so the included importer's update is no
    /// longer vetoed by a copy cooldown was told to ignore — and with nothing excluded the second
    /// line still counts.
    #[test]
    fn member_sources_excluding_drops_the_excluded_importers_declarations() {
        let lock = indoc! {"
            importers:

              app:
                dependencies:
                  mongoose:
                    specifier: ^9.8.0
                    version: 9.9.1
                  tailwindcss:
                    specifier: ^4.3.3
                    version: 4.3.3

              legacy:
                dependencies:
                  mongoose:
                    specifier: ^9.8.0
                    version: 9.8.0
                  tailwindcss:
                    specifier: ^3.4.19
                    version: 3.4.19

            packages:

              mongoose@9.8.0:
                resolution: {integrity: sha512-a}
              mongoose@9.9.1:
                resolution: {integrity: sha512-b}
              tailwindcss@3.4.19:
                resolution: {integrity: sha512-c}
              tailwindcss@4.3.3:
                resolution: {integrity: sha512-d}
        "};
        let excluded = HashSet::from(["legacy".to_string()]);
        let index = Pnpm::member_sources_excluding(lock, &excluded);
        assert_eq!(index.resolved_versions_of("mongoose"), ["9.9.1"]);
        assert_eq!(index.declared_specifiers_of("tailwindcss"), ["^4.3.3"]);
        assert!(index.members_for("mongoose", "9.8.0").is_empty());
        assert!(
            !index.splits_for("tailwindcss", "4.3.5"),
            "the excluded importer's v3 line no longer holds the included one"
        );
        assert!(
            Pnpm::member_sources(lock).splits_for("tailwindcss", "4.3.5"),
            "with nothing excluded the v3 line still splits the name"
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
        let index = MemberIndex::version_exact(parse_pnpm_importer_members(lock, &HashSet::new()));

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
        let index = MemberIndex::name_only(
            parse_npm_member_sources(lock, &HashSet::new()).expect("v3 lock has members"),
        );
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
        assert!(parse_npm_member_sources(lock, &HashSet::new()).is_none());
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
        let pins = parse_pnpm_exact_pins(lock, &HashSet::new());
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
        let pins = parse_pnpm_exact_pins(lock, &HashSet::new());

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
