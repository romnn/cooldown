//! Lockfile parsers for the npm-compatible package managers. Each manager resolves from the same
//! registry but records the resolved graph in its own format; the [`NodeLock`] trait abstracts the
//! per-manager differences (lockfile name, driver binary, parse, and lock refresh args) so a single
//! generic adapter can serve all of them.
//!
//! Every parser returns the flat list of resolved [`NameVersion`] pairs the lock pins.
//! Where the lock records importer/member declarations (npm v2/v3, pnpm, bun's text lock), the
//! adapter uses that same data for both direct/transitive classification and source attribution.
//! A Yarn Classic lock records none, so its members' declarations come from the workspace
//! manifests ([`WorkspaceManifests`]) and are looked up through the lock's selector keys
//! ([`YarnBindings`]).
//! Formats without either fall back to the root manifest's declared dependency names.

mod yarn;

pub use crate::manifest::workspaces::WorkspaceManifests;
pub use yarn::YarnBindings;

use crate::version::{CeilingHold, RangeMatch, is_exact_npm_specifier, range_match};
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

    /// Whether the peer gate can tell which copy a member's peer contract binds to.
    ///
    /// The gate reads that copy from the install tree ([`NodeLock::install_paths`]) when the lock
    /// records one, and otherwise from the member's own declaration, which is pnpm's isolated
    /// layout.
    /// A manager that hoists without recording its tree, or resolves peers by version rather than
    /// through the member's declaration, proves neither, so its peer evidence stays unavailable
    /// rather than borrowing pnpm's rule: yarn classic and bun.
    const PEER_CONTEXTS_MODELED: bool = true;

    /// Whether the lock records no workspace member, so this manager's member evidence comes from
    /// the workspace manifests the root `package.json` names ([`WorkspaceManifests`]): yarn
    /// classic.
    ///
    /// The adapter then reads those manifests, captures every member's in the mutation baseline,
    /// and hands them to [`member_paths_in`](NodeLock::member_paths_in),
    /// [`member_sources_in`](NodeLock::member_sources_in), and
    /// [`member_attribution_in`](NodeLock::member_attribution_in); the other managers' hooks
    /// ignore them.
    const READS_WORKSPACE_MANIFESTS: bool = false;

    /// Parses the lockfile body into the flat list of resolved [`NameVersion`] pairs.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] if the lockfile cannot be parsed.
    fn parse(content: &str) -> Result<Vec<NameVersion>>;

    /// The workspace member package(s) that declare each dependency, for attributing a dependency
    /// to its source package(s) in reports.
    /// Empty for a `bun.lock` whose members [`Bun`] cannot attribute, and for yarn classic, whose
    /// lock records no member, so their `members` column stays blank; yarn classic's members come
    /// from [`member_sources_in`](NodeLock::member_sources_in) instead.
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
    /// members from here rather than from what they declare. Default: empty (yarn classic records
    /// no member data in its lock; see [`member_paths_in`](NodeLock::member_paths_in)).
    #[must_use]
    fn member_paths(_content: &str) -> HashSet<String> {
        HashSet::new()
    }

    /// [`member_paths`](NodeLock::member_paths) given the workspace manifests the adapter read
    /// when [`READS_WORKSPACE_MANIFESTS`](NodeLock::READS_WORKSPACE_MANIFESTS) asks for them.
    ///
    /// `None` is a context without them, which attributes no member for yarn classic.
    /// Default: the lock-only [`member_paths`](NodeLock::member_paths).
    #[must_use]
    fn member_paths_in(content: &str, _workspace: Option<&WorkspaceManifests>) -> HashSet<String> {
        Self::member_paths(content)
    }

    /// [`member_sources_excluding`](NodeLock::member_sources_excluding) given the workspace
    /// manifests, as [`member_paths_in`](NodeLock::member_paths_in) takes them.
    ///
    /// Default: the lock-only [`member_sources_excluding`](NodeLock::member_sources_excluding).
    #[must_use]
    fn member_sources_in(
        content: &str,
        _workspace: Option<&WorkspaceManifests>,
        excluded: &HashSet<String>,
    ) -> MemberIndex {
        Self::member_sources_excluding(content, excluded)
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

    /// How the lock attributes a resolved version to each workspace member declaring it, the
    /// evidence the exact-pin guard judges a member's manifest pins against.
    ///
    /// Default: [`MemberAttribution::Unknown`], for a lock that records no per-member resolution
    /// by itself; yarn classic attributes its members only through
    /// [`member_attribution_in`](NodeLock::member_attribution_in).
    #[must_use]
    fn member_attribution(_content: &str) -> MemberAttribution {
        MemberAttribution::Unknown
    }

    /// [`member_attribution`](NodeLock::member_attribution) given the workspace manifests, as
    /// [`member_paths_in`](NodeLock::member_paths_in) takes them.
    ///
    /// The manifests are the selectors the lock was resolved under, so after cooldown edits a
    /// declaration they must carry its replacement.
    /// Default: the lock-only [`member_attribution`](NodeLock::member_attribution).
    #[must_use]
    fn member_attribution_in(
        content: &str,
        _workspace: Option<&WorkspaceManifests>,
    ) -> MemberAttribution {
        Self::member_attribution(content)
    }

    /// The registry releases the lock installs only through npm aliases.
    ///
    /// An alias (`"foo": "npm:other@2.0.0"`) resolves `other@2.0.0` without being a copy of any
    /// `other` declaration.
    /// Such a release is owned by no member, never direct, never a manifest landing's source, and
    /// no evidence that a move of `other` landed.
    /// npm answers from its install tree ([`InstallPaths::alias_only_versions`]) and, for a v1 lock
    /// that records none, from its top-level entries.
    ///
    /// Default: empty, for lock formats that record no alias distinctly from the copy it installs.
    #[must_use]
    fn alias_only_versions(_content: &str) -> AliasOnlyVersions {
        AliasOnlyVersions::default()
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
    /// `minimum_age_excludes` is the complete release-age exclusion list for this resolve.
    /// It replaces the configured list, so it must carry the configured entries along with the
    /// exact exemptions for pins the native gate would otherwise refuse.
    ///
    /// `--no-save`/`--lockfile-only` keep `package.json` and `node_modules` untouched. Returns `None`
    /// for managers without a joint resolve or when there are no exact pins to apply.
    #[must_use]
    fn whole_graph_args(
        _pins: &[(String, String)],
        _filters: &[String],
        _window_minutes: Option<i64>,
        _minimum_age_excludes: &[String],
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

    /// The driver args that refresh the lock pinned to an exact `version`, for the manifest-rewrite
    /// path (so the lock lands on exactly the cooldown-approved target instead of re-resolving the
    /// widened range to its newest member).
    ///
    /// This may save a range as a side effect.
    /// It is only safe inside a preserving transaction that restores cooldown's authorized manifest
    /// bytes. `None` (the default) means the manager has no exact-pin install,
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
    /// declaration.
    ///
    /// A manager with a joint resolve ([`NodeLock::supports_whole_graph_resolve`]) never lands one
    /// candidate at a time, so it does not implement this.
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
/// The manager's only exact pin also *saves* the range (npm's `install <name>@<version>`, which
/// writes into whichever field already declares the package), so it runs bracketed: pin, restore
/// cooldown's authorized manifest bytes, then run a plain relock.
/// The final step makes the restore consistent: the pin also rewrote the lock's own copy of the
/// manifest metadata, and synchronization recopies the restored ranges while keeping the pinned
/// version, which satisfies them.
#[derive(Debug)]
pub struct PreservingPin {
    /// The exact pin, which may edit manifests.
    pub pin: Vec<String>,
    /// The manifest-preserving lock synchronization that follows the restore.
    pub resync: Vec<String>,
}

/// How exact-pinning a workspace-declared name to one target relates to its declared ranges
/// ([`MemberIndex::split_verdict`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitVerdict {
    /// One joint pin lands without rewriting any declared range.
    Joint,
    /// One joint pin lands once each range that excludes the target takes the implicit-ceiling
    /// widen `--major` authorizes.
    CeilingWiden,
    /// The name stays out of the joint pin.
    Held(SplitHold),
}

/// Why a workspace-declared name stays out of the joint pin to one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitHold {
    /// Importers resolve the name at several versions, and these declared ranges do not provably
    /// admit the target.
    SeveralLines {
        /// The resolved versions, ascending.
        versions: Vec<String>,
        /// The declared ranges that do not provably admit the target.
        ranges: Vec<String>,
    },
    /// Several importers declare the name, yet the lock resolves none of them.
    Unresolved,
    /// Importers resolve the name at several versions, and none records a plain range.
    NoRange,
    /// A declaring member's manifest could not be read to confirm that every install declaration
    /// takes the ceiling widen.
    ManifestUnavailable {
        /// The manifest, project-relative and `/`-separated.
        manifest: String,
    },
    /// A declaring member's manifest pins the name exactly, in an install field the lock need not
    /// record, to a version excluding a target one joint pin would otherwise land.
    /// Unlike a [`Declaration`](Self::Declaration) pin, the plan's permission to rewrite pins
    /// releases it for a downgrade.
    ManifestPin {
        /// The pinned version, verbatim.
        range: String,
    },
    /// The name resolves at one version, and this declared range excludes the target in a way
    /// `--major` does not widen.
    Declaration {
        /// The declared range, verbatim.
        range: String,
        /// Why `--major` cannot widen it.
        hold: CeilingHold,
    },
}

impl SplitHold {
    /// The first of `ranges` that excludes `target` in a way `--major` does not widen, in the
    /// given order, or [`None`] when each either admits `target` or takes the ceiling widen.
    #[must_use]
    pub fn first_declaration_hold<'a>(
        ranges: impl IntoIterator<Item = &'a str>,
        target: &str,
    ) -> Option<SplitHold> {
        ranges
            .into_iter()
            .filter(|range| !crate::version::version_in_range(range, target))
            .find_map(|range| {
                crate::version::ceiling_hold(range, target).map(|hold| SplitHold::Declaration {
                    range: range.to_string(),
                    hold,
                })
            })
    }
}

/// Maps a resolved dependency to the workspace member packages that declare it.
///
/// pnpm records the resolved version per importer, so its entries are keyed exactly by
/// `(name, version)`.
/// npm records only version ranges per member, so it attributes each declaration to the one copy
/// the member's own install-tree lookup binds, as a declared copy, and keeps every declaration by
/// name as well.
/// A member owning a copy declares it directly whatever its source, but only one bound to a
/// registry release may be moved by editing that declaration ([`Self::editable_members_for`]).
/// Bun keys a registry binding by version like pnpm, and a declaration whose binding the lock
/// leaves undetermined by the one copy its package key records, as a declared copy.
/// Yarn classic attributes each declaration to the one copy its member's selector names, as a
/// declared copy, like npm.
#[derive(Debug, Default)]
pub struct MemberIndex {
    by_version: HashMap<(String, String), Vec<String>>,
    /// The members declaring each name, whatever copy they bind (npm): the inventory of
    /// declarations, which owns no copy by itself.
    by_name: HashMap<String, Vec<String>>,
    /// The members each copy is attributed to without an importer record of the version they
    /// resolve: they count as declaring and owning that copy, but no member resolves to it here,
    /// so the copy gives no split evidence.
    /// Bun records a peer declaration's copy here, since the lock leaves which copy the peer binds
    /// uncertain.
    /// npm records here the copy each member's own install-tree lookup binds: its per-package
    /// apply lands one candidate at a time and judges every landing on the tree itself, so the
    /// joint resolve's per-importer versions would only manufacture split evidence for it.
    /// Yarn classic records here the copy each member's own selector identifies, the lock having
    /// no importer record at all.
    declared_copies: HashMap<(String, String), Vec<String>>,
    /// The owners in `declared_copies` whose binding no edit of their declaration is proven to
    /// move, each with why: npm's git sources and local or remote tarballs, and yarn classic's
    /// local paths and selectors the lock or workspace may answer otherwise.
    /// They declare the copy directly, but no edit of their declaration can land a registry
    /// release in its place, so they neither pin nor bound it.
    fixed_copies: HashMap<(String, String), Vec<FixedOwner>>,
    /// `(name, version)` pairs every registry declaration binding them pins exactly (pnpm, npm,
    /// Bun, and yarn classic).
    exact_version: HashSet<(String, String)>,
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

/// One owner of a copy whose binding no edit of its declaration is proven to move
/// ([`MemberIndex::fixed_owners`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FixedOwner {
    /// The owning member's path (`.` for the root).
    pub member: String,
    /// Why no edit of its declaration is proven to move the copy.
    pub reason: FixedBinding,
}

/// Why a member owning a copy cannot move it by editing its declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FixedBinding {
    /// The member binds the copy through a source of its own: git, or a local or remote tarball.
    OwnSource,
    /// The member binds the copy through a local path, a `file:` or `link:` specifier (yarn
    /// classic).
    LocalPath,
    /// The root `resolutions` field can rewrite the member's selector (yarn classic).
    Resolutions,
    /// A workspace package carries the dependency's name, which Yarn links in place of a registry
    /// release whenever its version satisfies a range (yarn classic).
    WorkspaceName,
    /// The lock records the copy from a `resolved` URL that is no registry tarball of it (yarn
    /// classic).
    UnrecognizedOrigin,
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

    fn with_exact_versions(mut self, exact: HashSet<(String, String)>) -> Self {
        self.exact_version = exact;
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
        self.exact_version
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
            .chain(self.declared_copies.values().flatten())
            .cloned()
            .collect()
    }

    /// Whether exact-pinning `name` to `target` across every declaring importer must be held —
    /// the genuine workspace split the whole-graph resolve keeps out of the joint update, NOT a
    /// transitive duplicate.
    ///
    /// See [`Self::split_verdict`] for the rule.
    #[must_use]
    pub fn splits_for(&self, name: &str, target: &str) -> bool {
        matches!(self.split_verdict(name, target), SplitVerdict::Held(_))
    }

    /// How exact-pinning `name` to `target` across every declaring importer relates to their
    /// declared ranges.
    ///
    /// Importers that resolve the name to different versions (one member on `chalk@4.1.2`, another
    /// on `chalk@5.3.0`) or declare it under different range specifiers (`~7.3.0` vs `^7.0.0`) are
    /// only a split *relative to the target*: when every distinct declared range provably admits
    /// it, pinning the whole name there is exactly what a plain `pnpm update` does and no manifest
    /// needs rewriting, so two importers on `^4.17.20` and `^4.17.21` both take `4.17.22`.
    ///
    /// Differently spelled ranges on one resolved line that exclude the target only through a
    /// plain caret or tilde ceiling crossed by a major ([`CeilingHold`]) are no split either: each
    /// excluding range takes the same implicit-ceiling widen `--major` performs for a lone
    /// declaration ([`SplitVerdict::CeilingWiden`]).
    /// Their mere disagreement holds nothing.
    ///
    /// Everything else that excludes the target, or cannot be judged at all, is held
    /// ([`SplitVerdict::Held`]): two resolved lines, an exact pin, a range that is not a plain caret
    /// or tilde, or a ceiling the target does not cross by a major.
    /// Node-semver forms the Rust parser cannot represent (`||` unions, hyphen ranges, dist tags)
    /// fail [`version_in_range`](crate::version::version_in_range) and so keep the split: the
    /// question is answered only on proof, never by default, since the alternative writes an
    /// out-of-range lock entry while `--no-save` leaves the manifest untouched.
    /// A name whose only declarations carry protocol specifiers (`catalog:`, `npm:` aliases)
    /// records no range at all, so several resolved versions of it stay split.
    ///
    /// Derived from per-importer declarations only, so a direct dependency that merely shares a
    /// name with a transitive copy resolved at another version is single-declared and never splits.
    /// Only importer records (pnpm, Bun) carry per-importer data; npm's index keeps the copies its
    /// members bind apart from them and never splits.
    /// The lock records one specifier per importer, so a [`SplitVerdict::CeilingWiden`] still needs
    /// every install declaration in the manifests confirmed before the widen may run.
    #[must_use]
    pub fn split_verdict(&self, name: &str, target: &str) -> SplitVerdict {
        let versions = self.resolved_versions_of(name);
        let specifiers = self.declared_specifiers_of(name);
        if versions.len() <= 1 && specifiers.len() <= 1 {
            return SplitVerdict::Joint;
        }
        if specifiers.is_empty() {
            return SplitVerdict::Held(SplitHold::NoRange);
        }
        let excluding: Vec<&str> = specifiers
            .into_iter()
            .filter(|specifier| !crate::version::version_in_range(specifier, target))
            .collect();
        if excluding.is_empty() {
            return SplitVerdict::Joint;
        }
        match versions.as_slice() {
            [] => SplitVerdict::Held(SplitHold::Unresolved),
            [_] => match SplitHold::first_declaration_hold(excluding, target) {
                Some(hold) => SplitVerdict::Held(hold),
                None => SplitVerdict::CeilingWiden,
            },
            _ => SplitVerdict::Held(SplitHold::SeveralLines {
                versions: versions.into_iter().map(str::to_string).collect(),
                ranges: excluding.into_iter().map(str::to_string).collect(),
            }),
        }
    }

    /// The version `member` (an importer path) resolves `name` to, if this index carries per-importer
    /// version data (pnpm) and the member declares the name.
    /// `None` for npm's index, whose install-tree owners record no per-importer version, and for a
    /// member that does not declare the name.
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
    /// declaration), as opposed to a declared copy, which no importer record resolves.
    /// The peer-feasibility gate needs the distinction where no install tree can prove the binding:
    /// a declared copy of a name resolved at several versions may stand beside a nested transitive
    /// copy, which must not gate.
    #[must_use]
    pub fn version_attributed(&self, name: &str, version: &str) -> bool {
        self.by_version
            .contains_key(&(name.to_string(), version.to_string()))
    }

    /// Every distinct resolved version of `name` across the workspace's importer declarations,
    /// ascending — the divergent lines a multi-version hold keeps apart.
    /// Empty for npm's index, which records no per-importer version.
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
            .chain(self.declared_copies.keys().map(|(name, _)| name.clone()))
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
    /// version but the ranges themselves disagree (`~7.3.0` vs `^7.0.0`).
    /// Empty for npm's index, which records no per-importer specifiers.
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
        self.by_name.contains_key(name)
            || self
                .by_version
                .keys()
                .chain(self.declared_copies.keys())
                .any(|(entry, _)| entry == name)
    }

    /// The member packages owning `name` at `version`, sorted and deduplicated.
    ///
    /// Empty when the lock carries no per-member attribution for this dependency.
    ///
    /// A declaration of the name owns no copy by itself, so a copy no member binds, such as one
    /// nested under an unrelated dependency, has no members even where some member declares the
    /// name ([`declares`](Self::declares)).
    #[must_use]
    pub fn members_for(&self, name: &str, version: &str) -> Vec<String> {
        let key = (name.to_string(), version.to_string());
        let mut members: Vec<String> = self
            .by_version
            .get(&key)
            .into_iter()
            .flatten()
            .chain(self.declared_copies.get(&key).into_iter().flatten())
            .cloned()
            .collect();
        members.sort();
        members.dedup();
        members
    }

    /// The members among [`members_for`](Self::members_for) whose own binding of `name` at
    /// `version` is a registry release, sorted: the declarations an edit may move that copy
    /// through, and the ones its pin and declared bound come from.
    ///
    /// Outside npm and yarn classic every owner qualifies, since those indexes attribute registry
    /// bindings only.
    #[must_use]
    pub fn editable_members_for(&self, name: &str, version: &str) -> Vec<String> {
        let non_registry = self.non_registry_owners(name, version);
        self.members_for(name, version)
            .into_iter()
            .filter(|member| !non_registry.contains(member))
            .collect()
    }

    /// The members owning `name` at `version` through a binding no edit of their declaration is
    /// proven to move ([`fixed_owners`](Self::fixed_owners)), sorted; empty outside npm and yarn
    /// classic.
    ///
    /// Such a member declares the copy directly, yet no registry release can replace it through
    /// that declaration.
    #[must_use]
    pub fn non_registry_owners(&self, name: &str, version: &str) -> Vec<String> {
        let mut members: Vec<String> = self
            .fixed_owners(name, version)
            .into_iter()
            .map(|FixedOwner { member, .. }| member)
            .collect();
        members.dedup();
        members
    }

    /// The owners of `name` at `version` whose binding no edit of their declaration is proven to
    /// move, each with why, sorted by member.
    #[must_use]
    pub fn fixed_owners(&self, name: &str, version: &str) -> Vec<FixedOwner> {
        let mut owners = self
            .fixed_copies
            .get(&(name.to_string(), version.to_string()))
            .cloned()
            .unwrap_or_default();
        owners.sort();
        owners.dedup();
        owners
    }

    /// Every member declaring `name`, whatever copy it binds, sorted and deduplicated.
    ///
    /// npm's inventory lists a member even when its lookup binds no copy the index attributes,
    /// such as a link to a workspace package, so a caller holding the install tree can judge the
    /// physical binding itself.
    #[must_use]
    pub fn declaring_members(&self, name: &str) -> Vec<String> {
        let mut members: Vec<String> = self
            .by_name
            .get(name)
            .into_iter()
            .flatten()
            .chain(
                self.by_version
                    .iter()
                    .chain(&self.declared_copies)
                    .filter(|((entry, _), _)| entry == name)
                    .flat_map(|(_, members)| members),
            )
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
        // An installed package is named by its slot unless it records its own name, as an alias
        // does; a workspace member entry carries a path-shaped key, so its real name comes from
        // the copied manifest's `name` field.
        let Some(name) = npm_entry_name(key, val) else {
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

/// How a lock attributes resolved versions to the workspace members that declare them.
///
/// The name's newest copy says nothing about one member: a transitive duplicate can sit beside a
/// member's own unchanged copy, so only per-member evidence can tell whether that member's
/// manifest declaration still holds.
pub enum MemberAttribution {
    /// Each importer records what it binds each direct dependency to (pnpm).
    Importers(ImporterBindings),
    /// The lock records the physical install tree a member's lookup walks (npm v2/v3).
    InstallTree(InstallPaths),
    /// The lock records each member's declarations and the package keys Bun binds them through
    /// (`bun.lock`).
    PackageKeys(BunBindings),
    /// The lock keys each entry by the selectors resolving to it, and the workspace manifests
    /// name each member's selector (yarn classic).
    Selectors(YarnBindings),
    /// The lock records no per-member resolution (npm v1, an unsupported `bun.lock`, or a yarn
    /// classic lock read without complete workspace manifests).
    Unknown,
}

/// What a lock's [`MemberAttribution`] says one member resolves one name to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberResolution<'a> {
    /// The member resolves the name to this version (pnpm's peer suffix stripped).
    Resolved(&'a str),
    /// The lock attributes resolutions per member and records none for this one.
    Missing,
    /// No answer is known: the lock attributes nothing per member, the member's binding is no
    /// registry version of the name (a local reference, or an alias to another package), or the
    /// lock does not establish which copy the member binds.
    Unknown,
}

impl MemberAttribution {
    /// The version `member` (a workspace member path, `.` for the root) resolves `name` to.
    #[must_use]
    pub fn resolution(&self, member: &str, name: &str) -> MemberResolution<'_> {
        match self {
            MemberAttribution::Importers(bindings) => bindings.resolution(member, name),
            MemberAttribution::InstallTree(paths) => match paths.member_resolution(member, name) {
                InstallLookup::Resolved(instance) => MemberResolution::Resolved(instance.version),
                InstallLookup::Missing => MemberResolution::Missing,
                InstallLookup::Unknown => MemberResolution::Unknown,
            },
            MemberAttribution::PackageKeys(bindings) => bindings.resolution(member, name),
            MemberAttribution::Selectors(bindings) => bindings.resolution(member, name),
            MemberAttribution::Unknown => MemberResolution::Unknown,
        }
    }
}

/// Every importer's raw `version:` binding of each direct dependency it lists, in any group.
///
/// Unlike the registry-only [`MemberIndex`], it keeps local and alias references, so an importer
/// that still lists a dependency is never mistaken for one that dropped it.
#[derive(Debug, Default)]
pub struct ImporterBindings {
    /// `(importer, name)` → the recorded `version:`, `None` for an entry shape the model does not
    /// read; an importer listing one name in two groups keeps the first.
    bindings: HashMap<(String, String), Option<String>>,
}

impl ImporterBindings {
    /// What importer `member` binds `name` to.
    ///
    /// [`MemberResolution::Missing`] means the importer no longer lists the dependency at all.
    /// A `link:`, `file:`, or `workspace:` reference, a URL resolution, an alias to another
    /// package, or an unread entry carries no registry version of `name`, so it is
    /// [`MemberResolution::Unknown`].
    /// An alias naming the dependency itself (`name@1.2.3`, `npm:name@1.2.3`) resolves to its
    /// version.
    #[must_use]
    pub fn resolution(&self, member: &str, name: &str) -> MemberResolution<'_> {
        let Some(binding) = self.bindings.get(&(member.to_string(), name.to_string())) else {
            return MemberResolution::Missing;
        };
        binding
            .as_deref()
            .and_then(|binding| registry_version_of(name, binding))
            .map_or(MemberResolution::Unknown, MemberResolution::Resolved)
    }
}

/// The registry version of `name` a pnpm importer binding records, peer suffix stripped, or `None`
/// for a local reference, a URL resolution, or an alias to another package.
fn registry_version_of<'a>(name: &str, binding: &'a str) -> Option<&'a str> {
    if ["link:", "file:", "workspace:"]
        .iter()
        .any(|protocol| binding.starts_with(protocol))
    {
        return None;
    }
    let reference = binding.split('(').next().unwrap_or(binding);
    let reference = reference.strip_prefix("npm:").unwrap_or(reference);
    let version = match reference.rfind('@').filter(|&at| at > 0) {
        Some(at) => {
            let (alias, version) = reference.split_at(at);
            if alias != name {
                return None;
            }
            version.get(1..)?
        }
        None => reference,
    };
    (!version.is_empty() && !is_url_resolution(version)).then_some(version)
}

/// Every importer's raw direct-dependency bindings ([`ImporterBindings`]), excluded importers
/// included.
fn parse_pnpm_importer_bindings(content: &str) -> ImporterBindings {
    let mut bindings: HashMap<(String, String), Option<String>> = HashMap::new();
    walk_pnpm_importer_entries(content, |member, name, _specifier, version| {
        bindings
            .entry((member.to_string(), name.to_string()))
            .or_insert_with(|| version.map(str::to_string));
    });
    ImporterBindings { bindings }
}

/// The physical install layout of a hoisted lock: every resolved instance keyed by the directory
/// it is installed at (`node_modules/react`, `apps/a/node_modules/react`, and a workspace
/// member's own directory such as `apps/a`) — the tree npm resolves `require` and peers against.
/// Peer visibility is a question about this layout, not about declaring members: hoisting lets
/// disjoint members' packages meet at the root, and a nested conflict copy shadows the hoisted
/// one, so neither declaration paths nor a graph-wide version can answer "which copy does this
/// dependent actually see".
///
/// An npm alias (`"foo": "npm:other@1.0.0"`) installs `other` at `node_modules/foo` and records
/// `name: "other"` there.
/// That slot stays in the tree, since it shadows any farther copy of `foo` exactly as a genuine
/// copy would, but it binds no version of `foo` ([`InstallLookup::Unknown`]) and is no instance of
/// `foo` or `other` that a change of either rewrites.
pub struct InstallPaths {
    /// Install directory → the package installed there.
    installed: HashMap<String, InstalledPackage>,
    /// Package name → every `(version, directory)` instance of it, alias slots left out.
    instances: HashMap<String, Vec<(String, String)>>,
    /// Every `(name, version)` an alias slot installs.
    alias_targets: HashSet<(String, String)>,
    /// Every `(name, version)` installed as a registry copy at a slot of its own name: no alias,
    /// no link, and no local source.
    registry_copies: HashSet<(String, String)>,
}

/// The registry releases that only npm aliases install, as
/// [`NodeLock::alias_only_versions`] reads them.
#[derive(Debug, Default)]
pub struct AliasOnlyVersions {
    releases: HashSet<RegistryRelease>,
}

/// One registry release, by package name and version.
#[derive(Debug, PartialEq, Eq, Hash)]
struct RegistryRelease {
    name: String,
    version: String,
}

impl AliasOnlyVersions {
    /// Whether `name@version` is installed only through npm aliases, so no declaration of `name`
    /// owns it.
    #[must_use]
    pub fn contains(&self, name: &str, version: &str) -> bool {
        self.releases.contains(&RegistryRelease {
            name: name.to_string(),
            version: version.to_string(),
        })
    }
}

/// The package one install directory holds, each half [`None`] when the lock does not establish
/// it, such as a link whose target entry is absent or records no version.
struct InstalledPackage {
    /// The package's own name: the entry's recorded `name`, else the name its path implies.
    name: Option<String>,
    /// The version installed there.
    version: Option<String>,
    /// Where the package comes from, as the entry records it.
    source: SlotSource,
}

/// Where the package at one `package-lock.json` entry comes from, as the entry records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotSource {
    /// An install slot recording an `http(s)` `resolved`, or none at all.
    /// That is a registry release unless a declaration binding the slot names a source of its own
    /// ([`is_registry_specifier`]): npm records a remote tarball's `resolved` exactly like a
    /// registry tarball's.
    Registry,
    /// A link to a workspace or other local directory, which the lock lists as no resolved
    /// package of its own.
    Link,
    /// A git, local, or other non-registry source ([`is_local_source`]), or a workspace member's
    /// own entry.
    NonRegistry,
}

/// What a lookup on the [`InstallPaths`] finds for one name.
#[derive(Debug, PartialEq)]
pub enum InstallLookup<'a> {
    /// The nearest slot for the name holds a copy of it.
    Resolved(ResolvedInstance<'a>),
    /// No enclosing `node_modules` holds the name.
    Missing,
    /// The nearest slot for the name holds another package, an npm alias, or one the lock does not
    /// identify, so the name is bound to no known version of itself and nothing farther is visible.
    Unknown,
}

impl<'a> InstallLookup<'a> {
    /// The resolved copy, or [`None`] when the lookup proves no copy: missing or aliased alike.
    #[must_use]
    pub fn resolved(self) -> Option<ResolvedInstance<'a>> {
        match self {
            InstallLookup::Resolved(instance) => Some(instance),
            InstallLookup::Missing | InstallLookup::Unknown => None,
        }
    }
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
    /// elsewhere — npm's lock records each member's declarations as ranges only, but the physical
    /// lookup is exact.
    #[must_use]
    pub fn member_resolution(&self, member: &str, name: &str) -> InstallLookup<'_> {
        // The root project's member path is `.`; its install-tree directory is the empty key.
        let dir = if member == "." { "" } else { member };
        self.resolve_from(dir, name)
    }

    /// The version of `name` that `member`'s own lookup binds
    /// ([`member_resolution`](Self::member_resolution)), when its entry records a registry source.
    ///
    /// It is judged at the one slot the lookup stops at: a link, a git source, a local tarball, or
    /// an npm alias there yields `None`, even when a registry copy of the same name and version
    /// sits elsewhere in the tree.
    /// A remote tarball's entry records an `https` URL like a registry release's, so only the
    /// member's declaration tells it apart ([`MemberIndex::non_registry_owners`]).
    #[must_use]
    pub fn member_registry_version(&self, member: &str, name: &str) -> Option<&str> {
        let ResolvedInstance { version, directory } =
            self.member_resolution(member, name).resolved()?;
        (self.slot_source(directory) == Some(SlotSource::Registry)).then_some(version)
    }

    /// Where the package installed at `directory` comes from, `None` for a directory the lock
    /// records no entry at.
    fn slot_source(&self, directory: &str) -> Option<SlotSource> {
        self.installed
            .get(directory)
            .map(|installed| installed.source)
    }

    /// The instance `name` resolves to from `dir` — the nearest enclosing `node_modules/<name>`,
    /// walking from the directory itself up to the workspace root — as a [`ResolvedInstance`].
    /// Intermediate ancestors that are not package directories probe keys no lock ever records
    /// (`…/node_modules/node_modules/x`), which is harmless.
    ///
    /// The nearest slot decides even when an alias occupies it: Node's lookup stops there, so the
    /// result is then [`InstallLookup::Unknown`] rather than a farther copy the alias hides.
    #[must_use]
    pub fn resolve_from(&self, dir: &str, name: &str) -> InstallLookup<'_> {
        let mut ancestor = dir;
        loop {
            let candidate = if ancestor.is_empty() {
                format!("node_modules/{name}")
            } else {
                format!("{ancestor}/node_modules/{name}")
            };
            if let Some((directory, installed)) = self.installed.get_key_value(&candidate) {
                return match (installed.name.as_deref(), installed.version.as_deref()) {
                    (Some(installed_name), Some(version)) if installed_name == name => {
                        InstallLookup::Resolved(ResolvedInstance { version, directory })
                    }
                    _ => InstallLookup::Unknown,
                };
            }
            if ancestor.is_empty() {
                return InstallLookup::Missing;
            }
            ancestor = ancestor.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }

    /// The install directories holding `name` at exactly `version` — every physical copy a
    /// context-less move of that version could rewrite.
    /// An alias slot is a copy of neither its own name nor its target, so it is never listed.
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

    /// Every release installed only through npm aliases: some alias slot installs it, and no
    /// registry copy of it sits at a slot of its own name.
    ///
    /// A workspace or other local source of the same name and version is no such copy, since it
    /// is a different package from the registry release an alias names.
    #[must_use]
    pub fn alias_only_versions(&self) -> AliasOnlyVersions {
        let releases = self
            .alias_targets
            .difference(&self.registry_copies)
            .map(|(name, version)| RegistryRelease {
                name: name.clone(),
                version: version.clone(),
            })
            .collect();
        AliasOnlyVersions { releases }
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
///
/// An install slot whose package is not the name the slot implies, an npm alias, is kept as an
/// occupied slot but is no instance of either name ([`InstallPaths`]).
fn parse_npm_install_paths(content: &str) -> Option<InstallPaths> {
    let doc = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let packages = doc.get("packages")?.as_object()?;
    Some(npm_install_paths(packages))
}

/// The [`InstallPaths`] of a `package-lock.json` `packages` map ([`parse_npm_install_paths`]).
fn npm_install_paths(packages: &serde_json::Map<String, serde_json::Value>) -> InstallPaths {
    let mut installed: HashMap<String, InstalledPackage> = HashMap::new();
    let mut instances: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut alias_targets: HashSet<(String, String)> = HashSet::new();
    let mut registry_copies: HashSet<(String, String)> = HashSet::new();
    for (key, entry) in packages {
        // The root project itself is keyed by the empty string; it is not an installed instance.
        if key.is_empty() {
            continue;
        }
        // A link entry records only its target path; the instance's name and version live there,
        // and the name is read in the target's own context, so a link at one name's slot to a
        // member of another name is an alias like any other.
        let link = entry.get("link").and_then(serde_json::Value::as_bool) == Some(true);
        let source = if link {
            SlotSource::Link
        } else if npm_slot_name(key).is_none() || is_local_source(entry) {
            SlotSource::NonRegistry
        } else {
            SlotSource::Registry
        };
        let target = if link {
            entry
                .get("resolved")
                .and_then(serde_json::Value::as_str)
                .and_then(|target| packages.get_key_value(target))
        } else {
            Some((key, entry))
        };
        let identity = target.and_then(|(target_key, target)| {
            let name = npm_entry_name(target_key, target)?;
            let version = target.get("version").and_then(serde_json::Value::as_str)?;
            Some((name, version))
        });
        // A slot whose package cannot be identified is still occupied: Node's lookup stops there,
        // so it must keep hiding any farther copy rather than vanish from the tree.
        let Some((name, version)) = identity else {
            installed.insert(
                key.clone(),
                InstalledPackage {
                    name: None,
                    version: None,
                    source,
                },
            );
            continue;
        };
        let aliased = npm_slot_name(key).is_some_and(|slot| slot != name);
        installed.insert(
            key.clone(),
            InstalledPackage {
                name: Some(name.to_string()),
                version: Some(version.to_string()),
                source,
            },
        );
        let identity = (name.to_string(), version.to_string());
        if aliased {
            alias_targets.insert(identity);
            continue;
        }
        if source == SlotSource::Registry {
            registry_copies.insert(identity);
        }
        instances
            .entry(name.to_string())
            .or_default()
            .push((version.to_string(), key.clone()));
    }
    InstallPaths {
        installed,
        instances,
        alias_targets,
        registry_copies,
    }
}

/// Whether a `package-lock.json` install entry came from a local or non-registry source, which npm
/// records as a `resolved` that is no HTTP URL (`file:../pkg`, `git+ssh://…`).
/// An entry recording no `resolved` at all is taken as a registry copy.
fn is_local_source(entry: &serde_json::Value) -> bool {
    entry
        .get("resolved")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|resolved| {
            !resolved.starts_with("https://") && !resolved.starts_with("http://")
        })
}

/// The name an install-path key's slot implies, its path tail (`node_modules/@scope/foo` →
/// `@scope/foo`), or [`None`] for a key that is no install path, such as a workspace member's.
fn npm_slot_name(key: &str) -> Option<&str> {
    key.rsplit_once("node_modules/")
        .map(|(_, tail)| tail)
        .filter(|tail| !tail.is_empty())
}

/// The name of the package one `package-lock.json` entry records.
///
/// npm writes an entry's `name` only where it differs from the name the entry's path implies, so
/// the path supplies it otherwise.
/// An install-path entry is named by its slot unless it records its own `name`, as npm does for an
/// alias (`"foo": "npm:other@1.0.0"` installs `other` at `node_modules/foo`).
/// A workspace member entry carries a path-shaped key, so its name is the copied manifest's
/// `name`, else the one its directory implies ([`npm_folder_name`]).
fn npm_entry_name<'a>(key: &'a str, entry: &'a serde_json::Value) -> Option<&'a str> {
    let recorded = entry
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty());
    let implied = match npm_slot_name(key) {
        Some(slot) => Some(slot),
        None if key.contains("node_modules/") => None,
        None => npm_folder_name(key),
    };
    recorded.or(implied)
}

/// The package name npm infers from a directory path, as it does for a workspace entry whose
/// `name` it omits: the last component, scoped by its parent when that parent is an `@scope`
/// directory (`packages/@scope/other` → `@scope/other`).
fn npm_folder_name(path: &str) -> Option<&str> {
    let (parent, base) = path.rsplit_once('/').unwrap_or(("", path));
    if base.is_empty() {
        return None;
    }
    let scope = parent.rsplit('/').next().unwrap_or(parent);
    if scope.len() > 1 && scope.starts_with('@') {
        return path.get(path.len() - base.len() - 1 - scope.len()..);
    }
    Some(base)
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
///
/// The lock records no workspace member, so member attribution reads each member's selectors
/// from the workspace manifests ([`YarnBindings`]).
/// Yarn hoists without recording its tree and binds peers by version, so the peer gate stays out
/// of its locks ([`NodeLock::PEER_CONTEXTS_MODELED`]).
pub struct Yarn;
/// The Bun package manager: `bun.lock` (text lockfile) backed by the npm registry.
///
/// Member attribution follows Bun's own loader ([`BunBindings`]) and covers lock versions 1
/// through 3; any other lock attributes nothing per member.
/// Current Bun binds peers by version and hoists without recording its tree, so the peer gate
/// stays out of its locks ([`NodeLock::PEER_CONTEXTS_MODELED`]).
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
        parse_npm_member_index(content, excluded).unwrap_or_default()
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

    fn member_attribution(content: &str) -> MemberAttribution {
        // A v1 lock records no `packages` install tree, so it attributes nothing per member.
        Self::install_paths(content).map_or(MemberAttribution::Unknown, |paths| {
            MemberAttribution::InstallTree(paths)
        })
    }

    fn alias_only_versions(content: &str) -> AliasOnlyVersions {
        Self::install_paths(content).map_or_else(
            || parse_npm_v1_alias_only_versions(content),
            |paths| paths.alias_only_versions(),
        )
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
        // while retaining the exact, compatible resolution. See [`PreservingPin`].
        let mut pin = Self::pinned_relock_args(name, version, before)?;
        for workspace in workspaces {
            pin.push(format!("--workspace={workspace}"));
        }
        let resync = Self::relock_args(before);
        Some(PreservingPin { pin, resync })
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

    fn member_attribution(content: &str) -> MemberAttribution {
        MemberAttribution::Importers(parse_pnpm_importer_bindings(content))
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
        minimum_age_excludes: &[String],
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
        // The exclusions exempt pins the native gate would refuse: with pnpm's default strictness,
        // a refused version needs an approval that `--no-save` cannot record.
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
        push_minimum_age_excludes(&mut args, minimum_age_excludes);
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
        push_minimum_age_excludes(&mut args, minimum_age_excludes);
        Some(args)
    }

    fn lock_consistency_error(content: &str) -> Option<String> {
        pnpm_lock_consistency_error(content)
    }

    fn build_args(_before: Option<&str>) -> Vec<String> {
        vec!["install".into()]
    }
}

/// Appends pnpm's release-age exclusion list as repeated `--config` flags.
///
/// pnpm builds a list only from a repeated `--config.<key>=` flag.
/// A lone flag becomes a plain string, which pnpm reads as one pattern per character, so a single
/// entry is passed twice.
fn push_minimum_age_excludes(args: &mut Vec<String>, excludes: &[String]) {
    let repeat = usize::from(excludes.len() == 1);
    for exclusion in excludes.iter().chain(excludes.iter().take(repeat)) {
        args.push(format!("--config.minimumReleaseAgeExclude={exclusion}"));
    }
}

impl NodeLock for Yarn {
    const ID: ToolId = ToolId("yarn");
    const LOCKFILE: &'static str = "yarn.lock";
    const BIN: &'static str = "yarn";
    const PEER_CONTEXTS_MODELED: bool = false;
    const READS_WORKSPACE_MANIFESTS: bool = true;

    fn parse(content: &str) -> Result<Vec<NameVersion>> {
        // A Berry lock shares the file name but none of the Classic grammar, so the Classic
        // reader would find no entries and `outdated` or `upgrade` would silently see an empty
        // graph.
        if is_yarn_berry_lock(content) {
            return Err(CoreError::LockUnreadable(
                "yarn.lock is a Yarn Berry (v2+) lockfile, which cooldown cannot read; only Yarn \
                 Classic (v1) lockfiles are supported"
                    .into(),
            ));
        }
        Ok(parse_yarn(content))
    }

    fn member_paths_in(_content: &str, workspace: Option<&WorkspaceManifests>) -> HashSet<String> {
        workspace
            .map(WorkspaceManifests::member_paths)
            .unwrap_or_default()
    }

    fn member_sources_in(
        content: &str,
        workspace: Option<&WorkspaceManifests>,
        excluded: &HashSet<String>,
    ) -> MemberIndex {
        workspace
            .and_then(|workspace| YarnBindings::read(content, workspace))
            .map(|bindings| bindings.member_index(excluded))
            .unwrap_or_default()
    }

    fn member_attribution_in(
        content: &str,
        workspace: Option<&WorkspaceManifests>,
    ) -> MemberAttribution {
        workspace
            .and_then(|workspace| YarnBindings::read(content, workspace))
            .map_or(MemberAttribution::Unknown, MemberAttribution::Selectors)
    }

    fn relock_args(_before: Option<&str>) -> Vec<String> {
        vec!["install".into()]
    }

    fn build_args(_before: Option<&str>) -> Vec<String> {
        vec!["install".into()]
    }
}

/// Whether a `yarn.lock` was written by Yarn Berry (v2+), which carries a top-level
/// `__metadata` key that no Classic lock has.
///
/// Berry locks are YAML, so the key may be quoted or followed by a comment, and the file may start
/// with a byte-order mark.
/// A Classic entry for a package named `__metadata` is keyed by its selector
/// (`__metadata@^1.0.0`), so it never matches.
fn is_yarn_berry_lock(content: &str) -> bool {
    let content = content.trim_start_matches('\u{feff}');
    // Every spelling of the key contains its name, which spares a Classic lock the YAML pass.
    if !content.contains("__metadata") {
        return false;
    }
    // The YAML reading covers every mapping spelling (flow maps, spaced colons, tab-led comments).
    // A Classic lock is usually not valid YAML, so the line reading still runs when it fails.
    let parsed =
        parse_pnpm_yaml::<BerryMetadata>(content).is_ok_and(|document| document.metadata.is_some());
    parsed
        || content
            .lines()
            .any(|line| yaml_top_level_key(line) == Some("__metadata"))
}

/// The one top-level key of a YAML document that marks it as a Berry `yarn.lock`.
#[derive(Default, serde::Deserialize)]
struct BerryMetadata {
    #[serde(rename = "__metadata")]
    metadata: Option<serde::de::IgnoredAny>,
}

/// The key a top-level YAML mapping line opens, unquoted, or [`None`] for an indented, comment or
/// non-key line.
fn yaml_top_level_key(line: &str) -> Option<&str> {
    if line.starts_with([' ', '\t', '#']) {
        return None;
    }
    let without_comment = line.split(" #").next().unwrap_or(line).trim_end();
    let key = without_comment.strip_suffix(':')?;
    Some(key.trim_matches(['"', '\'']))
}

impl NodeLock for Bun {
    const ID: ToolId = ToolId("bun");
    const LOCKFILE: &'static str = "bun.lock";
    const BIN: &'static str = "bun";

    const PEER_CONTEXTS_MODELED: bool = false;

    fn parse(content: &str) -> Result<Vec<NameVersion>> {
        parse_bun(content)
    }

    fn member_sources_excluding(content: &str, excluded: &HashSet<String>) -> MemberIndex {
        BunBindings::read(content)
            .map(|bindings| bindings.member_index(excluded))
            .unwrap_or_default()
    }

    fn member_paths(content: &str) -> HashSet<String> {
        BunBindings::read(content)
            .map(|bindings| bindings.members.into_keys().collect())
            .unwrap_or_default()
    }

    fn member_attribution(content: &str) -> MemberAttribution {
        BunBindings::read(content)
            .map_or(MemberAttribution::Unknown, MemberAttribution::PackageKeys)
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
///
/// Only the v1 tree's top-level entries are read, each through [`read_npm_v1_entry`], so an alias
/// is named by its target and a non-registry entry yields no row.
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
            // An alias's entry records its target's identity, which is the package actually
            // resolved there: the slot's name is only the alias.
            if npm_slot_name(key).is_none() {
                continue;
            }
            let Some(name) = npm_entry_name(key, val) else {
                continue;
            };
            if let Some(version) = val.get("version").and_then(|v| v.as_str()) {
                out.push(NameVersion {
                    name: name.to_string(),
                    version: version.to_string(),
                    origin: lock_entry_origin(val),
                });
            }
        }
    } else if let Some(deps) = doc.get("dependencies").and_then(|v| v.as_object()) {
        out.extend(
            deps.iter()
                .filter_map(|(key, entry)| read_npm_v1_entry(key, entry))
                .map(|NpmV1Entry { package, .. }| package),
        );
    }
    Ok(out)
}

/// The registry release one top-level entry of a v1 `dependencies` tree installs.
struct NpmV1Entry {
    /// The release, named by an alias's target, with the entry's `resolved` origin.
    package: NameVersion,
    /// Whether the entry is an npm alias of another package, which installs the release under the
    /// entry's key without being a copy of that key's package or of any declaration of the target.
    aliased: bool,
}

/// Reads one top-level entry of a v1 `dependencies` tree, or `None` when it resolves no registry
/// release.
///
/// v1 records an alias as `"foo": { "version": "npm:other@2.0.0" }`, so a `version` naming the
/// `npm:` protocol is read as its target's identity before any other protocol is rejected.
/// A target that does not split into a name and a registry version yields `None` rather than
/// falling back to the entry's key, which names only the alias.
/// Any other non-registry `version` (`file:`, a git or tarball reference) is skipped.
fn read_npm_v1_entry(key: &str, entry: &serde_json::Value) -> Option<NpmV1Entry> {
    let version = entry.get("version").and_then(serde_json::Value::as_str)?;
    let (name, version) = match version.strip_prefix("npm:") {
        Some(target) => {
            let NameVersion { name, version, .. } = split_name_version(target)?;
            (name, version)
        }
        None => (key.to_string(), version.to_string()),
    };
    if !is_registry_reference(&version) {
        return None;
    }
    let aliased = name != key;
    Some(NpmV1Entry {
        package: NameVersion {
            name,
            version,
            origin: lock_entry_origin(entry),
        },
        aliased,
    })
}

/// The releases a v1 lock installs only through npm aliases ([`NodeLock::alias_only_versions`]).
///
/// It reads the same top-level entries [`parse_npm`] does: a foreign alias target counts unless a
/// registry copy of the same release sits under the target's own key.
/// A copy whose `resolved` names a local source is another package than the registry release an
/// alias names, so it restores nothing.
fn parse_npm_v1_alias_only_versions(content: &str) -> AliasOnlyVersions {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(content) else {
        return AliasOnlyVersions::default();
    };
    let Some(deps) = doc
        .get("dependencies")
        .and_then(serde_json::Value::as_object)
    else {
        return AliasOnlyVersions::default();
    };
    let mut alias_targets = HashSet::new();
    let mut registry_copies = HashSet::new();
    for (key, entry) in deps {
        let Some(NpmV1Entry { package, aliased }) = read_npm_v1_entry(key, entry) else {
            continue;
        };
        let release = RegistryRelease {
            name: package.name,
            version: package.version,
        };
        if aliased {
            alias_targets.insert(release);
        } else if !is_local_source(entry) {
            registry_copies.insert(release);
        }
    }
    alias_targets.retain(|release| !registry_copies.contains(release));
    AliasOnlyVersions {
        releases: alias_targets,
    }
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

/// Validates one pnpm importer entry, reporting only a resolved version its specifier provably
/// excludes.
///
/// The specifier is judged with npm's grammar by [`range_match`]: a bare `6.0.0` and its
/// `v6.0.0`, `=v6.0.0`, and `= v6.0.0` spellings are exact pins, and `||` unions and comparator
/// sets are read branch by branch.
/// Only [`RangeMatch::Excludes`] is reported.
/// [`RangeMatch::Matches`] and [`RangeMatch::Unknown`] produce no error, because a range the
/// matcher cannot read (`^v6`, `>=v6`, `v6.x`, a hyphen range, a dist tag) proves nothing.
/// A false report would send a healthy lock into repair or fail it as stale, so divergences from
/// node-semver may only under-report.
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
    if range_match(specifier, version) != RangeMatch::Excludes {
        return None;
    }
    Some(format!(
        "pnpm-lock.yaml importer {member} dependency {name}: version {version} does not satisfy range {specifier}"
    ))
}

/// The [`MemberIndex`] of `package-lock.json` (v2/v3), with the members at `excluded` paths left
/// out, or `None` for a lock without a `packages` map (v1), which attributes nothing per member.
///
/// A member entry lists its declarations as ranges, never the copy each binds, so one walk derives
/// everything per copy from the install tree: directness, pins, and declared bounds all follow the
/// same owners.
/// A member owns only the copy its own lookup binds ([`InstallPaths::member_resolution`]), so a
/// copy nested under an unrelated dependency stays transitive however many members declare its
/// name.
/// The copy is judged at the one slot the lookup stops at, and a link there owns nothing: it binds
/// a workspace or other local directory, never the resolved package of the same name and version
/// that may sit elsewhere in the tree.
///
/// Owning a copy makes the member declare it directly whatever its source, but only a registry
/// binding may be moved by editing the declaration ([`MemberIndex::editable_members_for`]).
/// A git source or a local tarball is told apart by its entry, while a remote tarball records an
/// `https` URL like a registry release, so the declarations decide: a slot any member binds
/// through a source of its own ([`is_registry_specifier`]) holds no registry release.
/// A remote tarball only a dependency's own requirement names is therefore read as a registry
/// release when a member's registry range binds it.
///
/// A copy is exact-pinned when every registry declaration of a member owning it is exact, in every
/// field that declares it: a range binding another copy neither releases its pin nor gains one.
/// Every declaration enters the name inventory whatever it binds, so a member whose lookup finds no
/// copy, or another package at the slot, still declares the name.
/// The tree and the declarations naming a slot's source are read whole, since excluding a member
/// changes neither the copy another member binds nor where that copy comes from.
fn parse_npm_member_index(content: &str, excluded: &HashSet<String>) -> Option<MemberIndex> {
    let doc = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let packages = doc.get("packages")?.as_object()?;
    let install = npm_install_paths(packages);
    let members = npm_member_entries(packages);
    let non_registry_slots = non_registry_slots(&install, &members);
    let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
    let mut owners: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut non_registry: HashMap<(String, String), Vec<FixedOwner>> = HashMap::new();
    let mut declared: HashMap<(String, String), usize> = HashMap::new();
    let mut exact: HashMap<(String, String), usize> = HashMap::new();
    for NpmMemberEntry { path, declarations } in members
        .iter()
        .filter(|member| !excluded.contains(member.path))
    {
        for (name, specifiers) in declarations {
            by_name
                .entry((*name).to_string())
                .or_default()
                .push((*path).to_string());
            let Some(ResolvedInstance { version, directory }) =
                install.member_resolution(path, name).resolved()
            else {
                continue;
            };
            let registry = match install.slot_source(directory) {
                Some(SlotSource::Registry) => !non_registry_slots.contains(directory),
                Some(SlotSource::NonRegistry) => false,
                Some(SlotSource::Link) | None => continue,
            };
            let copy = ((*name).to_string(), version.to_string());
            owners
                .entry(copy.clone())
                .or_default()
                .push((*path).to_string());
            if !registry {
                non_registry.entry(copy).or_default().push(FixedOwner {
                    member: (*path).to_string(),
                    reason: FixedBinding::OwnSource,
                });
                continue;
            }
            *declared.entry(copy.clone()).or_default() += specifiers.len();
            *exact.entry(copy).or_default() += specifiers
                .iter()
                .filter(|specifier| specifier.is_some_and(is_exact_npm_specifier))
                .count();
        }
    }
    let exact_version = declared
        .into_iter()
        .filter(|(copy, count)| exact.get(copy) == Some(count))
        .map(|(copy, _)| copy)
        .collect();
    Some(MemberIndex {
        by_name,
        declared_copies: owners,
        fixed_copies: non_registry,
        exact_version,
        authoritative: true,
        ..MemberIndex::default()
    })
}

/// One workspace member entry of a `package-lock.json`: the root `""`, keyed `.`, or any other key
/// not under `node_modules/`.
struct NpmMemberEntry<'a> {
    /// The member's workspace path.
    path: &'a str,
    /// Its declarations ([`npm_member_declarations`]).
    declarations: BTreeMap<&'a str, Vec<Option<&'a str>>>,
}

/// Every member entry of a `package-lock.json` `packages` map.
///
/// Member keys are workspace paths that consumers join onto the project root, so a crafted or
/// stale key that could address a manifest outside the workspace is rejected here.
fn npm_member_entries(
    packages: &serde_json::Map<String, serde_json::Value>,
) -> Vec<NpmMemberEntry<'_>> {
    packages
        .iter()
        .filter(|(key, _)| !key.contains("node_modules/"))
        .filter_map(|(key, entry)| {
            let path = if key.is_empty() { "." } else { key.as_str() };
            is_workspace_relative(path).then(|| NpmMemberEntry {
                path,
                declarations: npm_member_declarations(entry),
            })
        })
        .collect()
}

/// The install slots some member binds through a declaration naming a source of its own
/// ([`is_registry_specifier`]), which therefore hold no registry release whatever their entries
/// record.
fn non_registry_slots<'i>(
    install: &'i InstallPaths,
    members: &[NpmMemberEntry<'_>],
) -> HashSet<&'i str> {
    members
        .iter()
        .flat_map(|NpmMemberEntry { path, declarations }| {
            declarations
                .iter()
                .filter(|(_, specifiers)| {
                    specifiers
                        .iter()
                        .flatten()
                        .any(|specifier| !is_registry_specifier(specifier))
                })
                .filter_map(|(name, _)| install.member_resolution(path, name).resolved())
                .map(|instance| instance.directory)
        })
        .collect()
}

/// Whether a manifest specifier asks the registry for a release: a version, a range, a dist-tag,
/// or an npm alias of a registry release.
///
/// A URL, a git reference (`github:user/repo` or the `user/repo` shorthand), a local path or
/// tarball, and a link each name a source of their own.
/// None of those spellings fits the registry forms, which never contain `:` or `/` and never end
/// in a tarball extension.
fn is_registry_specifier(specifier: &str) -> bool {
    if specifier.starts_with("npm:") {
        return true;
    }
    let tarball = [".tgz", ".tar.gz", ".tar"]
        .iter()
        .any(|extension| specifier.ends_with(extension));
    !tarball && !specifier.contains(':') && !specifier.contains('/')
}

/// Every dependency one `package-lock.json` member entry declares, with the specifier of each
/// field declaring it in [`DIRECT_GROUPS`] order, `None` for a specifier that is no string.
fn npm_member_declarations(entry: &serde_json::Value) -> BTreeMap<&str, Vec<Option<&str>>> {
    let mut declarations: BTreeMap<&str, Vec<Option<&str>>> = BTreeMap::new();
    for field in DIRECT_GROUPS {
        let Some(group) = entry.get(field).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, specifier) in group {
            declarations
                .entry(name.as_str())
                .or_default()
                .push(specifier.as_str());
        }
    }
    declarations
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

/// Parses a classic (v1) `yarn.lock`: each entry is one or more comma-separated `name@range`
/// specifiers ending in `:`, followed by an indented `version "x.y.z"` line that resolves them.
///
/// This lenient line reading is the flat graph's projection only.
/// Member attribution reads the lock through Yarn's own grammar instead ([`YarnBindings`]), so a
/// descriptor this reading would misspell never names a member's copy.
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
/// `["name@version", registry, {...}, integrity]`.
///
/// Only registry identities are returned.
///
/// A source in place of a version, such as a git or tarball URL, a `file:`, `link:`, or
/// `workspace:` protocol, or a repository path, names no registry release and is left out.
fn parse_bun(content: &str) -> Result<Vec<NameVersion>> {
    let doc = decode_bun(content)?;
    let mut out = Vec::new();
    if let Some(packages) = doc.get("packages").and_then(|v| v.as_object()) {
        for val in packages.values() {
            // Split with `BunIdentity` rather than `split_name_version`: the latter takes the
            // last `@`, which a git source's own `@` would make the separator.
            if let Some(BunIdentity { name, reference }) =
                bun_identity(Some(val)).and_then(BunIdentity::parse)
                && is_registry_reference(reference)
            {
                out.push(NameVersion::new(name, reference));
            }
        }
    }
    Ok(out)
}

/// Decodes the `bun.lock` body, the one decoding every Bun reader shares.
///
/// Bun writes trailing commas, valid JSONC but not JSON, so the body is normalised before the JSON
/// parser sees it.
fn decode_bun(content: &str) -> Result<serde_json::Value> {
    serde_json::from_str(&strip_trailing_commas(content))
        .map_err(|err| CoreError::Parse(format!("bun.lock: {err}")))
}

/// The `bun.lock` `lockfileVersion`s whose member lookups [`BunBindings`] models.
///
/// From version 1 on, Bun's loader binds a member's direct dependency to the package key
/// `<workspace-name>/<dependency>`, else to the root key `<dependency>`, in Bun 1.3.0 and current
/// Bun alike; version 2 only tightened parsing and version 3 only added scoped overrides.
/// Version 0 resolved member dependencies through the workspace's own package row instead, and a
/// newer version is unproven, so both stay unattributed.
const BUN_KEYED_LOCK_VERSIONS: std::ops::RangeInclusive<u64> = 1..=3;

/// What a `bun.lock` binds each workspace member's direct dependencies to, read the way Bun's own
/// loader binds them.
///
/// The root resolves a dependency through the package key of the dependency's name.
/// Another member resolves it through `<workspace-name>/<dependency>` before that root key, where
/// the workspace name is the member's package `name`, never its directory, and scoped names stay
/// whole (`@acme/ui/@types/node`).
/// The selected entry's first array element names the package that key holds.
/// Package keys are Bun's own lookup paths, not npm install paths, so nothing else is read from
/// them.
///
/// Only a lock of version 1 through 3 (`BUN_KEYED_LOCK_VERSIONS`) with a root workspace is read.
/// A member is unsupported when the lock does not prove its key prefix: its name's root key must
/// hold the member itself, `name@workspace:<path>`, and no other member may carry its name.
/// When a root dependency of the same name holds that key, current Bun's loader reads the member's
/// keys under `./<name>`, while Bun 1.3.0's reads them under `<name>`, where that root
/// dependency's own nested copies live, so the lock alone cannot say which applies.
pub struct BunBindings {
    /// Member path (the root as `.`) → what the lock records for it; a member path that could
    /// address a manifest outside the workspace is dropped.
    members: BTreeMap<String, BunMember>,
}

/// One workspace member of a [`BunBindings`].
enum BunMember {
    /// The member's dependencies resolve through package keys the reader models, by name.
    Keyed(BTreeMap<String, BunDependency>),
    /// The lock does not prove which keys the member's dependencies resolve through.
    Unsupported,
}

/// One dependency a member's lock entry declares, in every field that declares it.
struct BunDependency {
    /// The declarations in [`DIRECT_GROUPS`] order.
    declarations: Vec<BunDeclaration>,
    binding: BunBinding,
}

/// One field's declaration of a dependency, as the member's lock entry copies it.
struct BunDeclaration {
    field: &'static str,
    /// The verbatim specifier, `None` when the entry records no string.
    specifier: Option<String>,
}

impl BunDeclaration {
    /// Whether the declaration pins one exact version.
    fn is_exact(&self) -> bool {
        self.specifier
            .as_deref()
            .is_some_and(is_exact_npm_specifier)
    }
}

/// Whether `copy` provably satisfies at least one of `declarations`, peer ranges included.
///
/// A range the matcher cannot judge proves nothing, so it never makes a copy the member's.
fn satisfies_a_declaration(declarations: &[BunDeclaration], copy: &str) -> bool {
    declarations.iter().any(|declaration| {
        declaration
            .specifier
            .as_deref()
            .is_some_and(|range| crate::version::version_in_range(range, copy))
    })
}

/// What a member's declared dependency binds to.
enum BunBinding {
    /// A registry copy of the dependency itself, at this version.
    Registry(String),
    /// No key holds the dependency, which Bun leaves unresolved only for an optional one.
    Unresolved,
    /// The key holds something other than a registry copy of the name: an alias to another
    /// package, a workspace or other local package, or a git or tarball source.
    Foreign,
    /// The member declares the name itself, but the lock does not establish which copy it binds:
    /// a peer, which current Bun binds by version rather than by key, or an entry no loader would
    /// accept.
    Uncertain {
        /// The registry copy of the name the member's package key holds, when it holds one.
        recorded: Option<String>,
    },
}

impl BunBindings {
    /// Reads the bindings from a `bun.lock` body, or `None` when the body does not decode or is no
    /// lock [`BunBindings`] can read.
    fn read(content: &str) -> Option<Self> {
        let doc = decode_bun(content).ok()?;
        let version = doc.get("lockfileVersion")?.as_u64()?;
        if !BUN_KEYED_LOCK_VERSIONS.contains(&version) {
            return None;
        }
        let workspaces = doc.get("workspaces")?.as_object()?;
        // Bun always records the root, so a lock without it is no complete record of what any
        // member declares.
        workspaces.get("")?;
        let no_packages = serde_json::Map::new();
        let packages = match doc.get("packages") {
            Some(packages) => packages.as_object()?,
            None => &no_packages,
        };
        // Two members of one name share every key, so neither member's keys are its own.
        let mut named: HashMap<&str, usize> = HashMap::new();
        for (key, workspace) in workspaces {
            if let Some(name) = workspace.get("name").and_then(serde_json::Value::as_str)
                && !key.is_empty()
            {
                *named.entry(name).or_default() += 1;
            }
        }
        let shared: HashSet<&str> = named
            .into_iter()
            .filter_map(|(name, count)| (count > 1).then_some(name))
            .collect();
        let members = workspaces
            .iter()
            .filter_map(|(key, workspace)| {
                let path = if key.is_empty() { "." } else { key.as_str() };
                // Member paths are joined onto the project root, so a crafted or stale key that
                // could address a manifest outside the workspace is rejected here.
                is_workspace_relative(path).then(|| {
                    (
                        path.to_string(),
                        bun_member(key, workspace, packages, &shared),
                    )
                })
            })
            .collect();
        Some(BunBindings { members })
    }

    /// What member `member` binds `name` to.
    ///
    /// [`MemberResolution::Missing`] means the member's lock entry declares no such dependency, or
    /// declares it only as an optional dependency no key holds.
    /// A member the lock does not list, an unsupported member, a peer declaration (which Bun binds
    /// by version rather than by key), and every binding that is no registry copy of `name` are
    /// [`MemberResolution::Unknown`]; [`Self::is_uncertain`] tells the cases apart.
    #[must_use]
    pub fn resolution(&self, member: &str, name: &str) -> MemberResolution<'_> {
        let Some(BunMember::Keyed(dependencies)) = self.members.get(member) else {
            return MemberResolution::Unknown;
        };
        match dependencies.get(name).map(|dependency| &dependency.binding) {
            None | Some(BunBinding::Unresolved) => MemberResolution::Missing,
            Some(BunBinding::Registry(version)) => MemberResolution::Resolved(version),
            Some(BunBinding::Foreign | BunBinding::Uncertain { .. }) => MemberResolution::Unknown,
        }
    }

    /// Whether member `member` declares `name` itself while the lock leaves which copy it binds
    /// undetermined, such as a name it also declares as a peer.
    ///
    /// Such a member's [`MemberResolution::Unknown`] is the only one that some copy of `name`
    /// could answer; every other unknown binding is proven to be no registry copy of the name, or
    /// belongs to a member the lock does not attribute.
    #[must_use]
    pub fn is_uncertain(&self, member: &str, name: &str) -> bool {
        let Some(BunMember::Keyed(dependencies)) = self.members.get(member) else {
            return false;
        };
        dependencies
            .get(name)
            .is_some_and(|dependency| matches!(dependency.binding, BunBinding::Uncertain { .. }))
    }

    /// The [`MemberIndex`] of every member's declarations, the members at `excluded` paths left
    /// out.
    ///
    /// A registry binding is attributed by its exact version.
    /// An uncertain binding is attributed to the one copy its package key records, as a declared
    /// copy no member resolves to, so the declaration still makes the dependency direct: what the
    /// lock leaves uncertain is which copy the member finally binds, never that it declares the
    /// name.
    /// A foreign, local, or unresolved binding owns no registry copy of the name at all.
    ///
    /// A lock with an unsupported member yields the empty, non-authoritative index: an index
    /// without that member's declarations would classify its dependencies as transitive.
    fn member_index(&self, excluded: &HashSet<String>) -> MemberIndex {
        if self
            .members
            .values()
            .any(|member| matches!(member, BunMember::Unsupported))
        {
            return MemberIndex::default();
        }
        let mut by_version: HashMap<(String, String), Vec<String>> = HashMap::new();
        let mut declared_copies: HashMap<(String, String), Vec<String>> = HashMap::new();
        let mut declared: HashMap<(String, String), usize> = HashMap::new();
        let mut exact: HashMap<(String, String), usize> = HashMap::new();
        let mut loose: HashSet<String> = HashSet::new();
        let mut specifiers: HashMap<String, HashSet<String>> = HashMap::new();
        let mut entries: HashMap<String, BTreeMap<String, ImporterEntry>> = HashMap::new();
        for (path, member) in &self.members {
            let BunMember::Keyed(dependencies) = member else {
                continue;
            };
            if excluded.contains(path) {
                continue;
            }
            for (name, dependency) in dependencies {
                let BunDependency {
                    declarations,
                    binding,
                } = dependency;
                // The first field's specifier speaks for the member, as pnpm's one specifier per
                // importer does, so a lone member never splits itself.
                let specifier = declarations
                    .first()
                    .and_then(|declaration| declaration.specifier.clone());
                // A semver range never contains `:`, so the test drops every protocol and alias.
                if let Some(range) = specifier
                    .as_deref()
                    .filter(|range| !range.is_empty() && !range.contains(':'))
                {
                    specifiers
                        .entry(name.clone())
                        .or_default()
                        .insert(range.to_string());
                }
                let version = match binding {
                    BunBinding::Registry(version) => version,
                    // The member still declares the name itself, so the copy its key records keeps
                    // the dependency direct and its declared bound applies.
                    // That key is the evidence a registry binding rests on too; only which copy a
                    // peer finally binds is uncertain, so the member owns that one copy and no
                    // other, and only when it provably satisfies one of the member's own
                    // declarations.
                    BunBinding::Uncertain { recorded } => {
                        if let Some(copy) = recorded
                            .as_deref()
                            .filter(|copy| satisfies_a_declaration(declarations, copy))
                        {
                            declared_copies
                                .entry((name.clone(), copy.to_string()))
                                .or_default()
                                .push(path.clone());
                        }
                        // A range whose copy is unknown could hold any copy of the name, so none
                        // is pinned by its exact declarations alone.
                        if !declarations.iter().all(BunDeclaration::is_exact) {
                            loose.insert(name.clone());
                        }
                        continue;
                    }
                    // A foreign or local binding is proven to be no registry copy of the name, and
                    // an unresolved one is proven absent, so the member owns none of the name's
                    // registry copies; its manifest still shows the declaration.
                    BunBinding::Foreign | BunBinding::Unresolved => continue,
                };
                let key = (name.clone(), version.clone());
                by_version
                    .entry(key.clone())
                    .or_default()
                    .push(path.clone());
                for declaration in declarations {
                    *declared.entry(key.clone()).or_default() += 1;
                    if declaration.is_exact() {
                        *exact.entry(key.clone()).or_default() += 1;
                    }
                }
                entries.entry(path.clone()).or_default().insert(
                    name.clone(),
                    ImporterEntry {
                        specifier,
                        version: version.clone(),
                    },
                );
            }
        }
        // A copy is exact-pinned only when every declaration binding it is exact, since any
        // range among them could still move it.
        let exact_version = declared
            .into_iter()
            .filter(|(key, count)| exact.get(key) == Some(count))
            .filter(|((name, _), _)| !loose.contains(name))
            .map(|(key, _)| key)
            .collect();
        MemberIndex {
            by_version,
            declared_copies,
            exact_version,
            declared_specifiers: specifiers,
            importer_entries: entries,
            authoritative: true,
            ..MemberIndex::default()
        }
    }
}

/// Reads one `workspaces` entry of a `bun.lock`, keyed `key` (the root's is empty), against the
/// lock's `packages`, where `shared` lists the names several members carry.
fn bun_member(
    key: &str,
    workspace: &serde_json::Value,
    packages: &serde_json::Map<String, serde_json::Value>,
    shared: &HashSet<&str>,
) -> BunMember {
    let Some(workspace) = workspace.as_object() else {
        return BunMember::Unsupported;
    };
    // The root's dependencies resolve through bare keys; another member's go through its name.
    let prefix = if key.is_empty() {
        None
    } else {
        let Some(name) = workspace
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty() && !shared.contains(name))
        else {
            return BunMember::Unsupported;
        };
        let linked = format!("{name}@workspace:{key}");
        if bun_identity(packages.get(name)) != Some(linked.as_str()) {
            return BunMember::Unsupported;
        }
        Some(name)
    };
    let mut declared: BTreeMap<String, Vec<BunDeclaration>> = BTreeMap::new();
    for field in DIRECT_GROUPS {
        let Some(section) = workspace.get(field) else {
            continue;
        };
        let Some(section) = section.as_object() else {
            return BunMember::Unsupported;
        };
        for (name, specifier) in section {
            if name.is_empty() {
                continue;
            }
            declared
                .entry(name.clone())
                .or_default()
                .push(BunDeclaration {
                    field,
                    specifier: specifier.as_str().map(str::to_string),
                });
        }
    }
    let dependencies = declared
        .into_iter()
        .map(|(name, declarations)| {
            let binding = bun_binding(prefix, &name, &declarations, packages);
            let dependency = BunDependency {
                declarations,
                binding,
            };
            (name, dependency)
        })
        .collect();
    BunMember::Keyed(dependencies)
}

/// What a member whose keys start with `prefix` (its workspace name, `None` for the root) binds
/// its declared dependency `name` to.
fn bun_binding(
    prefix: Option<&str>,
    name: &str,
    declarations: &[BunDeclaration],
    packages: &serde_json::Map<String, serde_json::Value>,
) -> BunBinding {
    // A declaration without a string specifier is no entry Bun would load.
    if declarations
        .iter()
        .any(|declaration| declaration.specifier.is_none())
    {
        return BunBinding::Uncertain { recorded: None };
    }
    let nested = prefix.map(|workspace| format!("{workspace}/{name}"));
    let entry = nested
        .as_deref()
        .and_then(|nested| packages.get(nested))
        .or_else(|| packages.get(name));
    let Some(entry) = entry else {
        // Bun refuses to load a lock that leaves a required dependency unresolved, so only an
        // optional one is genuinely absent.
        let optional = declarations
            .iter()
            .all(|declaration| declaration.field == "optionalDependencies");
        return if optional {
            BunBinding::Unresolved
        } else {
            BunBinding::Uncertain { recorded: None }
        };
    };
    let Some(BunIdentity {
        name: resolved,
        reference,
    }) = bun_identity(Some(entry)).and_then(BunIdentity::parse)
    else {
        return BunBinding::Uncertain { recorded: None };
    };
    if resolved != name || !is_registry_reference(reference) {
        return BunBinding::Foreign;
    }
    // Current Bun binds a peer by a version scan over every copy of the name, so the copy the key
    // holds need not be the one the member sees.
    if declarations
        .iter()
        .any(|declaration| declaration.field == "peerDependencies")
    {
        return BunBinding::Uncertain {
            recorded: Some(reference.to_string()),
        };
    }
    BunBinding::Registry(reference.to_string())
}

/// The identity a `packages` entry holds: its value's first array element, `name@reference`.
fn bun_identity(entry: Option<&serde_json::Value>) -> Option<&str> {
    entry?.as_array()?.first()?.as_str()
}

/// The two halves of a package identity, `name@reference`.
struct BunIdentity<'a> {
    /// The package the key holds, a scope's leading `@` kept.
    name: &'a str,
    /// A registry version, or a protocol reference (`workspace:`, `file:`, `link:`, `root:`, a git
    /// or tarball source).
    reference: &'a str,
}

impl<'a> BunIdentity<'a> {
    /// Splits `identity` at the first `@` after a scope's leading one, since a git reference can
    /// itself contain an `@` (`pkg@git+ssh://git@github.com/…`).
    fn parse(identity: &'a str) -> Option<Self> {
        let at = identity.get(1..)?.find('@')? + 1;
        let (name, reference) = identity.split_at_checked(at)?;
        let reference = reference.strip_prefix('@')?;
        Some(BunIdentity { name, reference })
    }
}

/// Whether an identity's `reference` is a registry version rather than a protocol reference.
fn is_registry_reference(reference: &str) -> bool {
    !reference.is_empty() && !is_url_resolution(reference)
}

/// The registry version of `name` a package identity records, or `None` for another package or a
/// protocol reference.
#[cfg(test)]
fn bun_registry_version<'a>(name: &str, identity: &'a str) -> Option<&'a str> {
    let BunIdentity {
        name: resolved,
        reference,
    } = BunIdentity::parse(identity)?;
    (resolved == name && is_registry_reference(reference)).then_some(reference)
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
pub(crate) mod yarn_fixtures;

/// Real `bun.lock` files the Bun readers' tests share.
///
/// Each was written by `bun install --lockfile-only` (Bun 1.4.4) in a scratch workspace whose
/// manifests the `workspaces` entries copy verbatim; only the integrity hashes are elided.
#[cfg(test)]
pub(crate) mod bun_fixtures {
    use indoc::indoc;

    /// A root and three members, two of them pinning `chalk` at their own versions.
    ///
    /// `pkg-a` and `@acme/scoped` keep nested copies under their workspace names, while `pkg-b`
    /// falls back to the root's copy, aliases `is-number` as `isnum`, and links `pkg-a`.
    /// The root key `@types/node` is `@acme/scoped`'s hoisted copy, which the root itself does not
    /// declare.
    pub(crate) const WORKSPACES: &str = indoc! {r#"
        {
          "lockfileVersion": 2,
          "configVersion": 1,
          "workspaces": {
            "": {
              "name": "root",
              "dependencies": {
                "chalk": "4.1.2",
                "is-number": "^7.0.0",
              },
            },
            "packages/a": {
              "name": "pkg-a",
              "dependencies": {
                "@types/node": "20.0.0",
                "chalk": "4.1.0",
              },
            },
            "packages/b": {
              "name": "pkg-b",
              "dependencies": {
                "chalk": "4.1.2",
                "isnum": "npm:is-number@6.0.0",
                "pkg-a": "workspace:*",
              },
            },
            "packages/scoped": {
              "name": "@acme/scoped",
              "devDependencies": {
                "@types/node": "20.1.0",
                "chalk": "5.3.0",
              },
            },
          },
          "packages": {
            "@acme/scoped": ["@acme/scoped@workspace:packages/scoped"],

            "@types/node": ["@types/node@20.1.0", "", {}, "sha512-x"],

            "ansi-styles": ["ansi-styles@4.3.0", "", { "dependencies": { "color-convert": "^2.0.1" } }, "sha512-x"],

            "chalk": ["chalk@4.1.2", "", { "dependencies": { "ansi-styles": "^4.1.0", "supports-color": "^7.1.0" } }, "sha512-x"],

            "color-convert": ["color-convert@2.0.1", "", { "dependencies": { "color-name": "~1.1.4" } }, "sha512-x"],

            "color-name": ["color-name@1.1.4", "", {}, "sha512-x"],

            "has-flag": ["has-flag@4.0.0", "", {}, "sha512-x"],

            "is-number": ["is-number@7.0.0", "", {}, "sha512-x"],

            "isnum": ["is-number@6.0.0", "", {}, "sha512-x"],

            "pkg-a": ["pkg-a@workspace:packages/a"],

            "pkg-b": ["pkg-b@workspace:packages/b"],

            "supports-color": ["supports-color@7.2.0", "", { "dependencies": { "has-flag": "^4.0.0" } }, "sha512-x"],

            "@acme/scoped/chalk": ["chalk@5.3.0", "", {}, "sha512-x"],

            "pkg-a/@types/node": ["@types/node@20.0.0", "", {}, "sha512-x"],

            "pkg-a/chalk": ["chalk@4.1.0", "", { "dependencies": { "ansi-styles": "^4.1.0", "supports-color": "^7.1.0" } }, "sha512-x"],
          }
        }
    "#};

    /// A library member declaring `chalk` both as an exact dev pin and as a peer range, beside a
    /// member declaring it only as an optional dependency.
    pub(crate) const PEER_AND_OPTIONAL: &str = indoc! {r#"
        {
          "lockfileVersion": 2,
          "configVersion": 1,
          "workspaces": {
            "": {
              "name": "root",
              "dependencies": {
                "chalk": "4.1.2",
              },
            },
            "packages/a": {
              "name": "pkg-a",
              "devDependencies": {
                "chalk": "4.1.0",
              },
              "peerDependencies": {
                "chalk": "^4.0.0",
              },
            },
            "packages/b": {
              "name": "pkg-b",
              "optionalDependencies": {
                "chalk": "4.1.1",
              },
            },
          },
          "packages": {
            "ansi-styles": ["ansi-styles@4.3.0", "", { "dependencies": { "color-convert": "^2.0.1" } }, "sha512-x"],

            "chalk": ["chalk@4.1.2", "", { "dependencies": { "ansi-styles": "^4.1.0", "supports-color": "^7.1.0" } }, "sha512-x"],

            "color-convert": ["color-convert@2.0.1", "", { "dependencies": { "color-name": "~1.1.4" } }, "sha512-x"],

            "color-name": ["color-name@1.1.4", "", {}, "sha512-x"],

            "has-flag": ["has-flag@4.0.0", "", {}, "sha512-x"],

            "pkg-a": ["pkg-a@workspace:packages/a"],

            "pkg-b": ["pkg-b@workspace:packages/b"],

            "supports-color": ["supports-color@7.2.0", "", { "dependencies": { "has-flag": "^4.0.0" } }, "sha512-x"],

            "pkg-a/chalk": ["chalk@4.1.0", "", { "dependencies": { "ansi-styles": "^4.1.0", "supports-color": "^7.1.0" } }, "sha512-x"],

            "pkg-b/chalk": ["chalk@4.1.1", "", { "dependencies": { "ansi-styles": "^4.1.0", "supports-color": "^7.1.0" } }, "sha512-x"],
          }
        }
    "#};

    /// A root and a library member that each declare `chalk` both as a dev dependency and as a peer
    /// range with an explicit upper bound, the member's dev pin kept at its nested key.
    pub(crate) const ROOT_PEER_AND_DEV: &str = indoc! {r#"
        {
          "lockfileVersion": 2,
          "configVersion": 1,
          "workspaces": {
            "": {
              "name": "root",
              "devDependencies": {
                "chalk": ">=4.1.0 <4.2.0",
              },
              "peerDependencies": {
                "chalk": "^4.0.0",
              },
            },
            "packages/a": {
              "name": "pkg-a",
              "devDependencies": {
                "chalk": "4.1.0",
              },
              "peerDependencies": {
                "chalk": ">=4.0.0 <5.0.0",
              },
            },
          },
          "packages": {
            "ansi-styles": ["ansi-styles@4.3.0", "", { "dependencies": { "color-convert": "^2.0.1" } }, "sha512-x"],

            "chalk": ["chalk@4.1.2", "", { "dependencies": { "ansi-styles": "^4.1.0", "supports-color": "^7.1.0" } }, "sha512-x"],

            "color-convert": ["color-convert@2.0.1", "", { "dependencies": { "color-name": "~1.1.4" } }, "sha512-x"],

            "color-name": ["color-name@1.1.4", "", {}, "sha512-x"],

            "has-flag": ["has-flag@4.0.0", "", {}, "sha512-x"],

            "pkg-a": ["pkg-a@workspace:packages/a"],

            "supports-color": ["supports-color@7.2.0", "", { "dependencies": { "has-flag": "^4.0.0" } }, "sha512-x"],

            "pkg-a/chalk": ["chalk@4.1.0", "", { "dependencies": { "ansi-styles": "^4.1.0", "supports-color": "^7.1.0" } }, "sha512-x"],
          }
        }
    "#};

    /// A member aliasing `chalk` to `is-number@6.0.0`, whose alias holds the root key `chalk`,
    /// beside the root's `log-symbols`, whose own registry `chalk@4.1.2` Bun nests under it.
    pub(crate) const ALIAS_BESIDE_TRANSITIVE: &str = indoc! {r#"
        {
          "lockfileVersion": 2,
          "configVersion": 1,
          "workspaces": {
            "": {
              "name": "root",
              "dependencies": {
                "log-symbols": "4.1.0",
              },
            },
            "packages/b": {
              "name": "pkg-b",
              "dependencies": {
                "chalk": "npm:is-number@6.0.0",
              },
            },
          },
          "packages": {
            "ansi-styles": ["ansi-styles@4.3.0", "", { "dependencies": { "color-convert": "^2.0.1" } }, "sha512-x"],

            "chalk": ["is-number@6.0.0", "", {}, "sha512-x"],

            "color-convert": ["color-convert@2.0.1", "", { "dependencies": { "color-name": "~1.1.4" } }, "sha512-x"],

            "color-name": ["color-name@1.1.4", "", {}, "sha512-x"],

            "has-flag": ["has-flag@4.0.0", "", {}, "sha512-x"],

            "is-unicode-supported": ["is-unicode-supported@0.1.0", "", {}, "sha512-x"],

            "log-symbols": ["log-symbols@4.1.0", "", { "dependencies": { "chalk": "^4.1.0", "is-unicode-supported": "^0.1.0" } }, "sha512-x"],

            "pkg-b": ["pkg-b@workspace:packages/b"],

            "supports-color": ["supports-color@7.2.0", "", { "dependencies": { "has-flag": "^4.0.0" } }, "sha512-x"],

            "log-symbols/chalk": ["chalk@4.1.2", "", { "dependencies": { "ansi-styles": "^4.1.0", "supports-color": "^7.1.0" } }, "sha512-x"],
          }
        }
    "#};

    /// A member declaring `chalk` as a bounded dev range and as a peer, whose copy `chalk@4.1.0`
    /// holds the root key, beside the root's `log-symbols`, whose `chalk@5.6.2` Bun nests under it.
    pub(crate) const PEER_BESIDE_TRANSITIVE: &str = indoc! {r#"
        {
          "lockfileVersion": 2,
          "configVersion": 1,
          "workspaces": {
            "": {
              "name": "root",
              "dependencies": {
                "log-symbols": "6.0.0",
              },
            },
            "packages/b": {
              "name": "pkg-b",
              "devDependencies": {
                "chalk": ">=4.1.0 <4.1.1",
              },
              "peerDependencies": {
                "chalk": "^4.0.0",
              },
            },
          },
          "packages": {
            "ansi-styles": ["ansi-styles@4.3.0", "", { "dependencies": { "color-convert": "^2.0.1" } }, "sha512-x"],

            "chalk": ["chalk@4.1.0", "", { "dependencies": { "ansi-styles": "^4.1.0", "supports-color": "^7.1.0" } }, "sha512-x"],

            "color-convert": ["color-convert@2.0.1", "", { "dependencies": { "color-name": "~1.1.4" } }, "sha512-x"],

            "color-name": ["color-name@1.1.4", "", {}, "sha512-x"],

            "has-flag": ["has-flag@4.0.0", "", {}, "sha512-x"],

            "is-unicode-supported": ["is-unicode-supported@1.3.0", "", {}, "sha512-x"],

            "log-symbols": ["log-symbols@6.0.0", "", { "dependencies": { "chalk": "^5.3.0", "is-unicode-supported": "^1.3.0" } }, "sha512-x"],

            "pkg-b": ["pkg-b@workspace:packages/b"],

            "supports-color": ["supports-color@7.2.0", "", { "dependencies": { "has-flag": "^4.0.0" } }, "sha512-x"],

            "log-symbols/chalk": ["chalk@5.6.2", "", {}, "sha512-x"],
          }
        }
    "#};

    /// A member named `is-number` beside the root's registry `is-number@7.0.0`, which holds the
    /// member's name key, so current Bun files the member's `chalk` under `./is-number/chalk`.
    pub(crate) const COLLIDING_MEMBER: &str = indoc! {r#"
        {
          "lockfileVersion": 2,
          "configVersion": 1,
          "workspaces": {
            "": {
              "name": "root",
              "dependencies": {
                "chalk": "4.1.2",
                "is-number": "7.0.0",
              },
            },
            "packages/a": {
              "name": "is-number",
              "version": "1.0.0",
              "dependencies": {
                "chalk": "4.1.0",
              },
            },
            "packages/b": {
              "name": "pkg-b",
              "dependencies": {
                "chalk": "4.1.2",
              },
            },
          },
          "packages": {
            "ansi-styles": ["ansi-styles@4.3.0", "", { "dependencies": { "color-convert": "^2.0.1" } }, "sha512-x"],

            "chalk": ["chalk@4.1.2", "", { "dependencies": { "ansi-styles": "^4.1.0", "supports-color": "^7.1.0" } }, "sha512-x"],

            "color-convert": ["color-convert@2.0.1", "", { "dependencies": { "color-name": "~1.1.4" } }, "sha512-x"],

            "color-name": ["color-name@1.1.4", "", {}, "sha512-x"],

            "has-flag": ["has-flag@4.0.0", "", {}, "sha512-x"],

            "is-number": ["is-number@7.0.0", "", {}, "sha512-x"],

            "pkg-b": ["pkg-b@workspace:packages/b"],

            "supports-color": ["supports-color@7.2.0", "", { "dependencies": { "has-flag": "^4.0.0" } }, "sha512-x"],

            "./is-number/chalk": ["chalk@4.1.0", "", { "dependencies": { "ansi-styles": "^4.1.0", "supports-color": "^7.1.0" } }, "sha512-x"],
          }
        }
    "#};
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::{formatdoc, indoc};

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
        assert_eq!(parse_pnpm_peer_requirements(lock), []);
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

    /// The importer check reads specifiers with npm's grammar and reports only a proven exclusion.
    ///
    /// Every row is `(specifier, version, reported)`.
    /// A range the matcher cannot read proves nothing, so it must never be reported: a false
    /// report sends a healthy lock into repair or fails it as stale.
    #[test]
    fn pnpm_importer_entries_report_only_proven_exclusions() {
        let cases: &[(Option<&str>, Option<&str>, bool)] = &[
            // Exact pins in every node-semver spelling, resolved off the pin
            (Some("6.0.0"), Some("6.1.0"), true),
            (Some("=6.0.0"), Some("6.1.0"), true),
            (Some("v6.0.0"), Some("6.1.0"), true),
            (Some("=v6.0.0"), Some("6.1.0"), true),
            (Some("= v6.0.0"), Some("6.1.0"), true),
            (Some(" 6.0.0 "), Some("6.1.0(react@18.2.0)"), true),
            // The same pins resolved at the pinned version
            (Some("6.0.0"), Some("6.0.0"), false),
            (Some("v6.0.0"), Some("6.0.0(react@18.2.0)"), false),
            (Some("=v6.0.0"), Some("6.0.0+build.7"), false),
            (Some("= v6.0.0"), Some("6.0.0+build.7(react@18.2.0)"), false),
            (Some("6.0.0+build.7"), Some("6.0.0"), false),
            // Ranges npm reads that the matcher cannot, whatever the version
            (Some("^v6.0.0"), Some("7.0.0"), false),
            (Some(">=v6"), Some("5.0.0"), false),
            (Some("v6.x"), Some("7.0.0"), false),
            (Some("6.0.0 - 6.0.5"), Some("6.1.0"), false),
            (Some("latest"), Some("6.1.0"), false),
            (Some("next"), Some("6.1.0-rc.1"), false),
            // Understood ranges, partials, and comparator sets
            (Some("^6"), Some("7.3.5"), true),
            (Some("^6"), Some("6.4.3"), false),
            (Some("6.0"), Some("6.1.0"), true),
            (Some("6.0"), Some("6.0.9"), false),
            (Some(">=6.0.0 <6.1.0"), Some("6.1.0"), true),
            (Some(">=6.0.0 <6.1.0"), Some("6.0.4"), false),
            // Understood unions
            (Some("^5.0.0 || ^6.0.0"), Some("6.1.0"), false),
            (Some("^4.0.0 || ^5.0.0"), Some("6.1.0"), true),
            (Some("5.0.0 || 6.0.0"), Some("6.1.0"), true),
            // A prerelease opted in by a comparator of its tuple, beside a partial
            (Some("1.x >=1.2.3-beta.1"), Some("1.2.3-beta.2"), false),
            (Some("1.2 >=1.2.3-beta.1"), Some("1.2.3-beta.2"), false),
            (
                Some("1.x >=1.2.3-beta.1 || ^2"),
                Some("1.2.3-beta.2"),
                false,
            ),
            (Some("1.x"), Some("1.2.3-beta.2"), true),
            (Some("1.x"), Some("2.0.0"), true),
            // A `>=0.0.0` floor, written or implied, bounds nothing in node-semver
            (Some("^0.0 >=0.0.0-alpha"), Some("0.0.0-beta"), false),
            (Some("0.x >=0.0.0-alpha"), Some("0.0.0-beta"), false),
            (Some(">=0.0.0 >=0.0.0-alpha"), Some("0.0.0-beta"), false),
            (Some("^0.0.0 >=0.0.0-alpha"), Some("0.0.0-beta"), false),
            (Some("~0.0.0 >=0.0.0-alpha"), Some("0.0.0-beta"), false),
            (Some("^0.0.0 >=0.0.0-alpha"), Some("0.0.1"), true),
            (Some("^0.0"), Some("0.0.0-beta"), true),
            // A union with an unreadable branch and no matching branch proves nothing
            (Some("^5.0.0 || 6.0.0 - 6.0.5"), Some("6.1.0"), false),
            (Some("^5.0.0 || ^v6.0.0"), Some("7.0.0"), false),
            // Protocols, aliases, and local or non-registry resolutions
            (Some("npm:other@6.0.0"), Some("other@6.1.0"), false),
            (Some("catalog:"), Some("6.1.0"), false),
            (Some("catalog:react18"), Some("6.1.0"), false),
            (Some("workspace:*"), Some("link:../pkg"), false),
            (Some("workspace:^1.0.0"), Some("2.0.0"), false),
            (Some("link:../pkg"), Some("link:../pkg"), false),
            (Some("file:../pkg"), Some("file:../pkg"), false),
            (Some("github:user/repo"), Some("6.1.0"), false),
            (Some("6.0.0"), Some("link:../pkg"), false),
            (Some("6.0.0"), Some("file:../pkg(react@18.2.0)"), false),
            (Some("6.0.0"), Some("workspace:../pkg"), false),
            (
                Some("6.0.0"),
                Some("https://codeload.github.com/user/repo/tar.gz/abc123"),
                false,
            ),
            (Some("6.0.0"), Some("other@6.1.0"), false),
            // Missing, empty, or unreadable values
            (None, Some("6.1.0"), false),
            (Some("6.0.0"), None, false),
            (None, None, false),
            (Some(""), Some("6.1.0"), false),
            (Some("   "), Some("6.1.0"), false),
            (Some("6.0.0"), Some(""), false),
            (Some("6.0.0"), Some("not-a-version"), false),
        ];
        for &(specifier, version, reported) in cases {
            let error = pnpm_importer_entry_error("apps/web", "dep", specifier, version);
            assert_eq!(
                error.is_some(),
                reported,
                "specifier {specifier:?} at version {version:?}: {error:?}"
            );
        }
    }

    /// A reported exact-pin mismatch names the importer, the dependency, the resolved version
    /// without its peer suffix, and the specifier as the manifest spelled it.
    #[test]
    fn pnpm_lock_consistency_names_a_floated_exact_pin() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/web:
                dependencies:
                  vite:
                    specifier: = v6.0.0
                    version: 6.1.0(@types/node@22.19.20)
        "};

        let error = pnpm_lock_consistency_error(lock).expect("a floated exact pin");

        assert_eq!(
            error,
            "pnpm-lock.yaml importer apps/web dependency vite: version 6.1.0 does not satisfy \
             range = v6.0.0"
        );
    }

    /// An importer entry whose shape the model does not recognize (a pre-v9 scalar) and a lock
    /// that does not parse carry no specifier to judge, so neither is reported.
    #[test]
    fn pnpm_lock_consistency_skips_unreadable_entries() {
        let scalar = indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/web:
                dependencies:
                  vite: 6.1.0
        "};
        assert_eq!(pnpm_lock_consistency_error(scalar), None);
        assert_eq!(pnpm_lock_consistency_error("importers: [unclosed"), None);
    }

    /// A Berry `yarn.lock` is refused as unreadable instead of parsing as an empty graph that
    /// `outdated` and `upgrade` would silently accept.
    #[test]
    fn a_yarn_berry_lock_is_unreadable_not_empty() {
        let berry = indoc! {r#"
            # This file is generated by running "yarn install" inside your project.
            # Manual changes might be lost - proceed with caution!

            __metadata:
              version: 8
              cacheKey: 10c0

            "lodash@npm:^4.17.21":
              version: 4.17.21
              resolution: "lodash@npm:4.17.21"
              checksum: 10c0/d8cbea072bb08655bb4c989da418994b073a608dffa608b09ac04b43a791b12aeae7cd7ad919aa4c925f33b48490b5cfe6c1f71d827956071dae2e7bb3a6b74
              languageName: node
              linkType: hard
        "#};
        let error = <Yarn as NodeLock>::parse(berry).expect_err("a Berry lock is not read");
        assert!(
            matches!(&error, CoreError::LockUnreadable(message) if message.contains("Yarn Berry")),
            "typed unreadable-lock error naming Berry, got: {error:?}"
        );

        let classic = indoc! {r#"
            # THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
            # yarn lockfile v1


            lodash@^4.17.21:
              version "4.17.21"
              resolved "https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz#abc"
        "#};
        let entries = <Yarn as NodeLock>::parse(classic).expect("a Classic lock is read");
        assert_eq!(entries.len(), 1, "{entries:?}");

        // YAML lets Berry's metadata key be quoted, commented or preceded by a byte-order mark.
        for lock in [
            "\"__metadata\":\n  version: 8\n",
            "'__metadata':\n  version: 8\n",
            "__metadata: # metadata\n  version: 8\n",
            "\u{feff}__metadata:\n  version: 8\n",
            "__metadata:\r\n  version: 8\r\n",
            "__metadata: { version: 8 }\n",
            "__metadata :\n  version: 8\n",
            "__metadata:\t# metadata\n  version: 8\n",
        ] {
            assert!(
                <Yarn as NodeLock>::parse(lock).is_err(),
                "{lock:?} carries a Berry metadata key"
            );
        }

        // A Classic entry for a package named `__metadata` is keyed by its selector.
        let classic_metadata_package = indoc! {r#"
            # yarn lockfile v1


            __metadata@^1.0.0:
              version "1.0.0"
              resolved "https://registry.yarnpkg.com/__metadata/-/__metadata-1.0.0.tgz#abc"
        "#};
        let entries = <Yarn as NodeLock>::parse(classic_metadata_package)
            .expect("a Classic package named __metadata is read");
        assert_eq!(entries.len(), 1, "{entries:?}");
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

    /// A mixed `bun.lock` yields exactly its registry packages: a git identity's inner `@`
    /// must not move the name's split, and every protocol or repository source is left out.
    #[test]
    fn bun_text_lock_keeps_only_registry_identities() -> Result<()> {
        let lock = indoc! {r#"
            {
                "lockfileVersion": 1,
                "packages": {
                    "lodash": ["lodash@4.17.21", "", {}, "sha512-a"],
                    "chalk": ["chalk@5.0.0+build.1", "", {}, "sha512-b"],
                    "@babel/core": ["@babel/core@7.1.0", "", {}, "sha512-c"],
                    "pkg": ["pkg@git+ssh://git@github.com/x/pkg.git#abc123", {}],
                    "@scope/pkg": ["@scope/pkg@git+ssh://git@github.com/x/pkg.git#abc123", {}],
                    "y": ["y@github:x/y#sha", {}],
                    "local": ["local@file:../local", {}],
                    "pkg-a": ["pkg-a@workspace:packages/a"],
                    "tarball": ["tarball@https://example.com/tarball.tgz", {}],
                    "pkg-c": ["pkg-c@x/y#sha", {}],
                },
            }"#};
        assert_eq!(
            sorted(parse_bun(lock)?),
            sorted(vec![
                NameVersion::new("lodash", "4.17.21"),
                NameVersion::new("chalk", "5.0.0+build.1"),
                NameVersion::new("@babel/core", "7.1.0"),
            ])
        );
        Ok(())
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
        assert_eq!(
            index.members_for("@airtype/api", "0.0.0"),
            [] as [String; 0]
        );
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
        assert_eq!(of("my-lodash", "lodash@4.17.21"), [] as [String; 0]);
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

    /// Workspace declarations exercising every way ranges can disagree about one target.
    const SPLIT_MATRIX_LOCK: &str = indoc! {"
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

    /// The acceptance matrix for target-aware splits: disagreeing ranges or several resolved lines
    /// split a name only when some declared range excludes (or cannot be judged against) the target
    /// being pinned.
    #[test]
    fn a_split_is_judged_against_the_target() {
        let lock = SPLIT_MATRIX_LOCK;
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

    /// Each held row of the acceptance matrix names the condition that holds it.
    #[test]
    fn a_held_split_names_its_condition() {
        let index = Pnpm::member_sources(SPLIT_MATRIX_LOCK);
        let declaration = |range: &str, hold| {
            SplitVerdict::Held(SplitHold::Declaration {
                range: range.to_string(),
                hold,
            })
        };
        assert_eq!(
            index.split_verdict("semver", "7.4.0"),
            declaration("~7.3.0", CeilingHold::WithinLine)
        );
        assert_eq!(
            index.split_verdict("tailwindcss", "4.3.3"),
            SplitVerdict::Held(SplitHold::SeveralLines {
                versions: vec!["3.4.19".to_string(), "4.3.3".to_string()],
                ranges: vec!["^3.4.19".to_string()],
            })
        );
        assert_eq!(
            index.split_verdict("react", "18.3.2"),
            declaration("^18.0.0 || ^19.0.0", CeilingHold::NotPlain)
        );
        assert_eq!(
            index.split_verdict("pinned", "2.11.14"),
            declaration("2.11.13", CeilingHold::ExactPin)
        );
    }

    /// Differently spelled ranges on one resolved line that exclude a cross-major target only
    /// through a plain caret or tilde ceiling take the `--major` widen instead of a split hold,
    /// while every other way of excluding it still holds.
    #[test]
    fn a_cross_major_target_past_plain_ceilings_is_no_split() {
        let lock = indoc! {"
            importers:

              pkgs/a:
                dependencies:
                  caret:
                    specifier: ^20.11.1
                    version: 20.14.5
                  tilde:
                    specifier: ~20.11.1
                    version: 20.11.4
                  zero:
                    specifier: ^0.3.1
                    version: 0.3.2
                  exact:
                    specifier: 20.14.5
                    version: 20.14.5
                  bounded:
                    specifier: ^20.11.1
                    version: 20.14.5
                  floor:
                    specifier: ^21.0.0
                    version: 21.1.0
                  next:
                    specifier: ~20.0.0-next.1
                    version: 20.0.0
                  unresolved:
                    specifier: ^20.11.1

              pkgs/b:
                dependencies:
                  caret:
                    specifier: ^20.9.0
                    version: 20.14.5
                  tilde:
                    specifier: ^20.9.0
                    version: 20.11.4
                  zero:
                    specifier: ^0.3.0
                    version: 0.3.2
                  exact:
                    specifier: ^20.9.0
                    version: 20.14.5
                  bounded:
                    specifier: '>=20 <21'
                    version: 20.14.5
                  floor:
                    specifier: ^21.1.0
                    version: 21.1.0
                  next:
                    specifier: ^20.0.0
                    version: 20.0.0
                  unresolved:
                    specifier: ^20.9.0
        "};
        let index = Pnpm::member_sources(lock);
        let declaration = |range: &str, hold| {
            SplitVerdict::Held(SplitHold::Declaration {
                range: range.to_string(),
                hold,
            })
        };

        // The implicit ceilings widen.
        assert_eq!(
            index.split_verdict("caret", "21.0.4"),
            SplitVerdict::CeilingWiden
        );
        assert_eq!(
            index.split_verdict("tilde", "21.0.4"),
            SplitVerdict::CeilingWiden
        );
        assert_eq!(
            index.split_verdict("zero", "0.4.0"),
            SplitVerdict::CeilingWiden,
            "a 0.x minor is the major line"
        );
        assert_eq!(
            index.split_verdict("next", "21.0.0"),
            SplitVerdict::CeilingWiden,
            "a prerelease base is still a plain tilde"
        );
        assert!(!index.splits_for("caret", "21.0.4"));

        // An in-line target both ranges admit needs no widen at all.
        assert_eq!(index.split_verdict("caret", "20.14.6"), SplitVerdict::Joint);

        // Everything else holds, naming why.
        assert_eq!(
            index.split_verdict("exact", "21.0.4"),
            declaration("20.14.5", CeilingHold::ExactPin)
        );
        assert_eq!(
            index.split_verdict("bounded", "21.0.4"),
            declaration(">=20 <21", CeilingHold::NotPlain)
        );
        assert_eq!(
            index.split_verdict("floor", "20.14.6"),
            declaration("^21.0.0", CeilingHold::BelowFloor),
            "a rollback below a caret floor is no ceiling widen"
        );
        assert_eq!(
            index.split_verdict("unresolved", "21.0.4"),
            SplitVerdict::Held(SplitHold::Unresolved),
            "the widen needs exactly one resolved line"
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
        assert_eq!(index.members_for("mongoose", "9.8.0"), [] as [String; 0]);
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

    /// A `package-lock.json` of `lockfile_version` whose `packages` map holds `packages`.
    /// A v2 lock also carries the legacy `dependencies` tree, here contradicting every copy the
    /// map records, so a reader that consulted it would attribute the wrong ones.
    fn npm_tree_lock(lockfile_version: u8, packages: &str) -> String {
        let legacy = if lockfile_version == 2 {
            indoc! {r#"
                "dependencies": {
                  "other": { "version": "9.0.0" },
                  "unrelated": { "version": "9.0.0" }
                },
            "#}
        } else {
            ""
        };
        formatdoc! {r#"
            {{
              "name": "root",
              "lockfileVersion": {lockfile_version},
              {legacy}
              "packages": {{
                {packages}
              }}
            }}
        "#}
    }

    /// The members owning `other@version` in `index`.
    fn owners_of(index: &MemberIndex, version: &str) -> Vec<String> {
        index.members_for("other", version)
    }

    /// A member owns only the copy its own lookup binds: the root's `^1.0.0` owns its hoisted
    /// `1.0.0`, while the `2.0.0` an unrelated dependency nests has no owner, though the root
    /// declares the name, and a declaration with no reachable copy owns nothing.
    /// The root is keyed as `.`, a member by its workspace path.
    #[test]
    fn npm_attributes_only_the_copy_a_members_lookup_binds() {
        for lockfile_version in [2, 3] {
            let lock = npm_tree_lock(
                lockfile_version,
                indoc! {r#"
                    "": {
                      "name": "root",
                      "dependencies": { "other": "^1.0.0", "unrelated": "^1.0.0" },
                      "devDependencies": { "turbo": "^2.0.0" }
                    },
                    "packages/api": { "name": "api", "dependencies": { "unrelated": "^1.0.0" } },
                    "node_modules/other": { "version": "1.0.0" },
                    "node_modules/unrelated": {
                      "version": "1.0.0",
                      "dependencies": { "other": "^2.0.0" }
                    },
                    "node_modules/unrelated/node_modules/other": { "version": "2.0.0" }
                "#},
            );
            let index = Npm::member_sources(&lock);

            assert!(index.is_authoritative(), "v{lockfile_version}");
            assert_eq!(owners_of(&index, "1.0.0"), ["."], "v{lockfile_version}");
            assert_eq!(
                owners_of(&index, "2.0.0"),
                [] as [String; 0],
                "v{lockfile_version}: the nested duplicate is transitive"
            );
            assert_eq!(
                index.members_for("unrelated", "1.0.0"),
                [".", "packages/api"],
                "v{lockfile_version}: a member without a node_modules binds the hoisted copy"
            );
            assert_eq!(
                index.members_for("turbo", "2.9.16"),
                [] as [String; 0],
                "v{lockfile_version}: a declaration with no reachable copy owns none"
            );
            // The declarations themselves stay in the inventory.
            assert!(index.declares("turbo") && index.declares("other"));
            assert!(index.declared_names().contains("turbo"));
            assert_eq!(
                index.all_paths(),
                HashSet::from([".".to_string(), "packages/api".to_string()])
            );
            // npm's owners are no importer records, so they give no split evidence.
            assert_eq!(index.resolved_versions_of("other"), [] as [&str; 0]);
            assert_eq!(index.resolved_version(".", "other"), None);
            assert_eq!(index.split_verdict("other", "2.1.0"), SplitVerdict::Joint);
        }
    }

    /// Members binding different copies each own their own: a hoisted one, one nested in the
    /// member's own `node_modules`, and one a parent member hoists for a member nested inside it,
    /// while a copy nested under a dependency belongs to none of them.
    #[test]
    fn npm_members_on_different_copies_each_own_theirs() {
        for lockfile_version in [2, 3] {
            let lock = npm_tree_lock(
                lockfile_version,
                indoc! {r#"
                    "": { "name": "root", "workspaces": ["apps/a", "apps/b", "apps/b/plugin", "apps/c"] },
                    "apps/a": { "name": "a", "version": "0.1.0", "dependencies": { "other": "^1.0.0" } },
                    "apps/b": { "name": "b", "version": "0.1.0", "dependencies": { "other": "^2.0.0" } },
                    "apps/b/plugin": { "name": "plugin", "version": "0.1.0", "dependencies": { "other": "^2.0.0" } },
                    "apps/c": { "name": "c", "version": "0.1.0", "dependencies": { "other": "^1.0.0" } },
                    "node_modules/other": { "version": "1.0.0" },
                    "apps/b/node_modules/other": { "version": "2.0.0" },
                    "apps/a/node_modules/x": { "version": "1.0.0" },
                    "apps/a/node_modules/x/node_modules/other": { "version": "3.0.0" }
                "#},
            );
            let index = Npm::member_sources(&lock);

            assert_eq!(
                owners_of(&index, "1.0.0"),
                ["apps/a", "apps/c"],
                "v{lockfile_version}"
            );
            assert_eq!(
                owners_of(&index, "2.0.0"),
                ["apps/b", "apps/b/plugin"],
                "v{lockfile_version}"
            );
            assert_eq!(
                owners_of(&index, "3.0.0"),
                [] as [String; 0],
                "v{lockfile_version}"
            );
        }
    }

    /// An alias grants no ownership of its target's release, even beside a genuine nested copy of
    /// that release, and an alias at a member's own slot hides the farther genuine copy from that
    /// member, which then owns no copy of the name at all.
    #[test]
    fn npm_aliases_own_nothing_beside_genuine_copies() {
        for lockfile_version in [2, 3] {
            let lock = npm_tree_lock(
                lockfile_version,
                indoc! {r#"
                    "": {
                      "name": "root",
                      "workspaces": ["apps/a"],
                      "dependencies": { "alias": "npm:other@2.0.0", "other": "^1.0.0", "foo": "^1.0.0" }
                    },
                    "apps/a": { "name": "a", "version": "0.1.0", "dependencies": { "foo": "npm:other@1.0.0" } },
                    "node_modules/alias": { "name": "other", "version": "2.0.0" },
                    "node_modules/other": { "version": "1.0.0" },
                    "node_modules/x": { "version": "1.0.0" },
                    "node_modules/x/node_modules/other": { "version": "2.0.0" },
                    "node_modules/foo": { "version": "1.0.0" },
                    "apps/a/node_modules/foo": { "name": "other", "version": "1.0.0" }
                "#},
            );
            let index = Npm::member_sources(&lock);

            assert_eq!(owners_of(&index, "1.0.0"), ["."], "v{lockfile_version}");
            assert_eq!(
                owners_of(&index, "2.0.0"),
                [] as [String; 0],
                "v{lockfile_version}: neither the alias nor the nested copy is the root's"
            );
            assert_eq!(
                index.members_for("alias", "2.0.0"),
                [] as [String; 0],
                "v{lockfile_version}"
            );
            assert_eq!(
                index.members_for("foo", "1.0.0"),
                ["."],
                "v{lockfile_version}: the alias at apps/a's slot hides the hoisted foo"
            );
            assert!(index.declares("foo"), "v{lockfile_version}");
        }
    }

    /// A git source or a local or remote tarball at the member's own slot makes the member declare
    /// that copy directly, but binds no registry release, so the member neither pins nor bounds
    /// it and no edit of its declaration may move it.
    /// A remote tarball records an `https` URL like a registry release, so the declaration naming
    /// the URL is what marks its slot, for every member binding it.
    /// A link to a workspace package owns nothing, so a registry copy of the same name and version
    /// nested elsewhere stays transitive, while the inventory still lists the link's declarer.
    #[test]
    fn npm_non_registry_sources_are_direct_but_not_editable() {
        for lockfile_version in [2, 3] {
            let lock = npm_tree_lock(
                lockfile_version,
                indoc! {r#"
                    "": {
                      "name": "root",
                      "workspaces": ["packages/lib", "apps/a"],
                      "dependencies": {
                        "lib": "^2.0.0",
                        "vendored": "file:vendor/vendored-1.0.0.tgz",
                        "repo": "github:user/repo#v3.0.0",
                        "remote": "https://example.org/remote-1.0.0.tgz",
                        "x": "1.0.0"
                      }
                    },
                    "packages/lib": { "name": "lib", "version": "2.0.0" },
                    "apps/a": { "name": "a", "version": "0.1.0", "dependencies": { "remote": "^1.0.0" } },
                    "node_modules/lib": { "resolved": "packages/lib", "link": true },
                    "node_modules/vendored": { "version": "1.0.0", "resolved": "file:vendor/vendored-1.0.0.tgz" },
                    "node_modules/repo": { "version": "3.0.0", "resolved": "git+ssh://git@github.com/user/repo.git#0123abc" },
                    "node_modules/remote": { "version": "1.0.0", "resolved": "https://example.org/remote-1.0.0.tgz" },
                    "node_modules/x": { "version": "1.0.0", "resolved": "https://registry.npmjs.org/x/-/x-1.0.0.tgz" },
                    "node_modules/x/node_modules/lib": { "version": "2.0.0", "resolved": "https://registry.npmjs.org/lib/-/lib-2.0.0.tgz" }
                "#},
            );
            let index = Npm::member_sources(&lock);

            // Sources of their own: direct owners, none of them editable or pinning
            for (name, version, owners) in [
                ("vendored", "1.0.0", vec!["."]),
                ("repo", "3.0.0", vec!["."]),
                ("remote", "1.0.0", vec![".", "apps/a"]),
            ] {
                assert_eq!(
                    index.members_for(name, version),
                    owners,
                    "v{lockfile_version} {name}"
                );
                assert_eq!(
                    index.non_registry_owners(name, version),
                    owners,
                    "v{lockfile_version} {name}"
                );
                assert_eq!(
                    index.editable_members_for(name, version),
                    [] as [String; 0],
                    "v{lockfile_version} {name}"
                );
                assert!(
                    !index.is_exact_pinned(name, version),
                    "v{lockfile_version} {name}"
                );
            }
            // A workspace link owns no row, yet its declarer stays in the inventory
            assert_eq!(
                index.members_for("lib", "2.0.0"),
                [] as [String; 0],
                "v{lockfile_version}"
            );
            assert_eq!(index.declaring_members("lib"), ["."], "v{lockfile_version}");
            // A registry release is direct and editable
            assert_eq!(
                index.members_for("x", "1.0.0"),
                ["."],
                "v{lockfile_version}"
            );
            assert_eq!(
                index.editable_members_for("x", "1.0.0"),
                ["."],
                "v{lockfile_version}"
            );
            assert!(index.is_exact_pinned("x", "1.0.0"), "v{lockfile_version}");
        }
    }

    /// A specifier that asks the registry for a release is told from one naming a source of its
    /// own by its spelling alone.
    #[test]
    fn registry_specifiers_are_told_from_sources_of_their_own() {
        for registry in [
            "1.0.0",
            "^1.0.0",
            "~1.2.3",
            ">=1.0.0 <2.0.0",
            "1.x",
            "*",
            "",
            "latest",
            "next",
            "1.0.0 - 2.0.0",
            "^1 || ^2",
            "npm:other@^1.0.0",
            "npm:@scope/other@1.0.0",
        ] {
            assert!(is_registry_specifier(registry), "{registry:?}");
        }
        for source in [
            "https://example.org/plugin-1.0.0.tgz",
            "http://example.org/plugin.tgz",
            "git+ssh://git@github.com/user/repo.git#0123abc",
            "git://github.com/user/repo.git",
            "github:user/repo",
            "gitlab:user/repo#v1.0.0",
            "user/repo",
            "user/repo#semver:^1.0.0",
            "file:../plugin",
            "file:vendor/plugin-1.0.0.tgz",
            "../plugin",
            "./vendor/plugin-1.0.0.tgz",
            "plugin-1.0.0.tgz",
            "plugin.tar.gz",
            "link:../plugin",
            "workspace:^1.0.0",
        ] {
            assert!(!is_registry_specifier(source), "{source:?}");
        }
    }

    /// A declaration whose lookup finds no slot, optional or required, or an unidentified one owns
    /// no copy, even when another dependency nests a copy of the name.
    #[test]
    fn npm_missing_and_unknown_slots_own_nothing() {
        for lockfile_version in [2, 3] {
            let lock = npm_tree_lock(
                lockfile_version,
                indoc! {r#"
                    "": {
                      "name": "root",
                      "dependencies": { "gone": "^1.0.0", "odd": "^1.0.0", "x": "^1.0.0" },
                      "optionalDependencies": { "fsevents": "^2.0.0" }
                    },
                    "node_modules/odd": { "resolved": "packages/absent", "link": true },
                    "node_modules/x": { "version": "1.0.0" },
                    "node_modules/x/node_modules/gone": { "version": "1.0.0" },
                    "node_modules/x/node_modules/odd": { "version": "1.0.0" },
                    "node_modules/x/node_modules/fsevents": { "version": "2.3.0", "optional": true }
                "#},
            );
            let index = Npm::member_sources(&lock);

            for (name, version) in [("gone", "1.0.0"), ("odd", "1.0.0"), ("fsevents", "2.3.0")] {
                assert_eq!(
                    index.members_for(name, version),
                    [] as [String; 0],
                    "v{lockfile_version} {name}"
                );
                assert!(!index.is_exact_pinned(name, version));
                assert!(index.declares(name), "v{lockfile_version} {name}");
            }
        }
    }

    /// A copy is pinned when every declaration of a member owning it is exact: an exact pin on one
    /// copy holds beside a range on another, never pins an unrelated nested copy, and is released
    /// by a range on the same copy, in another member or another field of its own.
    #[test]
    fn npm_pins_are_judged_per_copy() {
        for lockfile_version in [2, 3] {
            let lock =
                |packages: &str| Npm::member_sources(&npm_tree_lock(lockfile_version, packages));

            let split = lock(indoc! {r#"
                "": { "name": "root", "workspaces": ["apps/a", "apps/b"] },
                "apps/a": { "name": "a", "dependencies": { "other": "1.0.0" } },
                "apps/b": { "name": "b", "dependencies": { "other": "^2.0.0" } },
                "node_modules/other": { "version": "1.0.0" },
                "apps/b/node_modules/other": { "version": "2.0.0" }
            "#});
            assert!(
                split.is_exact_pinned("other", "1.0.0"),
                "v{lockfile_version}"
            );
            assert!(
                !split.is_exact_pinned("other", "2.0.0"),
                "v{lockfile_version}"
            );

            let nested = lock(indoc! {r#"
                "": { "name": "root", "dependencies": { "other": "1.0.0", "x": "^1.0.0" } },
                "node_modules/other": { "version": "1.0.0" },
                "node_modules/x": { "version": "1.0.0" },
                "node_modules/x/node_modules/other": { "version": "2.0.0" }
            "#});
            assert!(
                nested.is_exact_pinned("other", "1.0.0"),
                "v{lockfile_version}"
            );
            assert!(
                !nested.is_exact_pinned("other", "2.0.0"),
                "v{lockfile_version}"
            );

            let shared = lock(indoc! {r#"
                "": { "name": "root", "workspaces": ["apps/a", "apps/c"] },
                "apps/a": { "name": "a", "dependencies": { "other": "1.0.0" } },
                "apps/c": { "name": "c", "dependencies": { "other": "^1.0.0" } },
                "node_modules/other": { "version": "1.0.0" }
            "#});
            assert!(
                !shared.is_exact_pinned("other", "1.0.0"),
                "v{lockfile_version}"
            );

            let both_exact = lock(indoc! {r#"
                "": { "name": "root", "workspaces": ["apps/a", "apps/c"] },
                "apps/a": { "name": "a", "dependencies": { "other": "1.0.0" } },
                "apps/c": { "name": "c", "dependencies": { "other": "=v1.0.0" } },
                "node_modules/other": { "version": "1.0.0" }
            "#});
            assert!(
                both_exact.is_exact_pinned("other", "1.0.0"),
                "v{lockfile_version}"
            );

            let fields = lock(indoc! {r#"
                "": {
                  "name": "root",
                  "dependencies": { "other": "1.0.0" },
                  "devDependencies": { "other": "^1.0.0" }
                },
                "node_modules/other": { "version": "1.0.0" }
            "#});
            assert!(
                !fields.is_exact_pinned("other", "1.0.0"),
                "v{lockfile_version}"
            );
        }
    }

    /// Excluding a member removes its declarations from ownership and pins alike, while the tree
    /// is still read whole: an excluded member's range no longer releases a sibling's pin, and
    /// excluding the member a copy is hoisted beside leaves it owned by the member that binds it.
    #[test]
    fn npm_exclusions_recompute_owners_and_pins() {
        for lockfile_version in [2, 3] {
            let lock = npm_tree_lock(
                lockfile_version,
                indoc! {r#"
                    "": { "name": "root", "workspaces": ["apps/a", "apps/c"], "dependencies": { "other": "^1.0.0" } },
                    "apps/a": { "name": "a", "dependencies": { "other": "1.0.0" } },
                    "apps/c": { "name": "c", "dependencies": { "other": "^1.0.0" } },
                    "node_modules/other": { "version": "1.0.0" }
                "#},
            );
            let excluding = |paths: &[&str]| {
                let excluded: HashSet<String> = paths.iter().map(ToString::to_string).collect();
                Npm::member_sources_excluding(&lock, &excluded)
            };

            let whole = excluding(&[]);
            assert_eq!(owners_of(&whole, "1.0.0"), [".", "apps/a", "apps/c"]);
            assert!(!whole.is_exact_pinned("other", "1.0.0"));

            let pinned = excluding(&[".", "apps/c"]);
            assert_eq!(owners_of(&pinned, "1.0.0"), ["apps/a"]);
            assert!(
                pinned.is_exact_pinned("other", "1.0.0"),
                "v{lockfile_version}"
            );

            let without_root = excluding(&["."]);
            assert_eq!(owners_of(&without_root, "1.0.0"), ["apps/a", "apps/c"]);
            assert!(!without_root.is_exact_pinned("other", "1.0.0"));
        }
    }

    /// A peer-only declaration owns the copy its lookup binds, like an install declaration, and
    /// joins every other field's declaration of the name in the pin judgment; one npm left
    /// uninstalled owns nothing but is still declared.
    #[test]
    fn npm_peer_declarations_own_the_copy_they_bind() {
        for lockfile_version in [2, 3] {
            let lock = npm_tree_lock(
                lockfile_version,
                indoc! {r#"
                    "": { "name": "root", "workspaces": ["apps/a", "apps/b"] },
                    "apps/a": { "name": "a", "peerDependencies": { "react": "^18.0.0", "absent": "^1.0.0" } },
                    "apps/b": {
                      "name": "b",
                      "devDependencies": { "react": "^18.0.0" },
                      "peerDependencies": { "react": "18.2.0" }
                    },
                    "node_modules/react": { "version": "18.2.0", "peer": true },
                    "node_modules/x": { "version": "1.0.0" },
                    "node_modules/x/node_modules/react": { "version": "17.0.2" }
                "#},
            );
            let index = Npm::member_sources(&lock);

            assert_eq!(
                index.members_for("react", "18.2.0"),
                ["apps/a", "apps/b"],
                "v{lockfile_version}"
            );
            assert_eq!(
                index.members_for("react", "17.0.2"),
                [] as [String; 0],
                "v{lockfile_version}"
            );
            assert!(
                !index.is_exact_pinned("react", "18.2.0"),
                "v{lockfile_version}"
            );
            assert!(index.declares("absent"), "v{lockfile_version}");
            assert_eq!(index.members_for("absent", "1.0.0"), [] as [String; 0]);
        }
    }

    /// The owned copy is the one the member's own lookup stops at, never one farther away or
    /// elsewhere in the tree: a member's link hides the hoisted registry copy from it, and its own
    /// nested registry copy outranks the hoisted one.
    #[test]
    fn npm_ownership_is_judged_at_the_nearest_slot() {
        let lock = npm_tree_lock(
            3,
            indoc! {r#"
                "": {
                  "name": "root",
                  "workspaces": ["apps/a", "packages/lib"],
                  "dependencies": { "lib": "^2.0.0", "other": "^1.0.0" }
                },
                "apps/a": { "name": "a", "dependencies": { "lib": "^2.0.0", "other": "^2.0.0" } },
                "packages/lib": { "name": "lib", "version": "2.0.0" },
                "node_modules/lib": { "version": "2.0.0" },
                "apps/a/node_modules/lib": { "resolved": "packages/lib", "link": true },
                "node_modules/other": { "version": "1.0.0" },
                "apps/a/node_modules/other": { "version": "2.0.0", "resolved": "http://registry.example/other-2.0.0.tgz" }
            "#},
        );
        let index = Npm::member_sources(&lock);

        assert_eq!(index.editable_members_for("lib", "2.0.0"), ["."]);
        assert_eq!(index.declaring_members("lib"), [".", "apps/a"]);
        assert_eq!(index.editable_members_for("other", "1.0.0"), ["."]);
        assert_eq!(index.editable_members_for("other", "2.0.0"), ["apps/a"]);
    }

    #[test]
    fn npm_member_sources_are_absent_for_v1_lock() {
        // A v1 lock has no `packages` map, so direct-ness falls back to the root manifest.
        let lock =
            r#"{ "lockfileVersion": 1, "dependencies": { "lodash": { "version": "4.17.15" } } }"#;
        assert!(parse_npm_member_index(lock, &HashSet::new()).is_none());
        assert!(!Npm::member_sources(lock).is_authoritative());
        assert_eq!(
            Npm::member_sources(lock).members_for("lodash", "4.17.15"),
            [] as [String; 0]
        );
    }

    #[test]
    fn member_index_is_empty_by_default() {
        // Yarn, an unsupported bun.lock, and the unparsable case: no attribution, so the column
        // stays blank.
        let index = MemberIndex::default();
        assert_eq!(index.members_for("anything", "1.0.0"), [] as [String; 0]);
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
        // node-semver's prefixed and spaced spellings are the same exact pin.
        assert!(is_exact_npm_specifier("v2.11.0"));
        assert!(is_exact_npm_specifier("=v2.11.0"));
        assert!(is_exact_npm_specifier("= v2.11.0"));
        assert!(!is_exact_npm_specifier("^v2.11.0"));
        assert!(!is_exact_npm_specifier("v2.x"));
    }

    /// A prefixed or spaced pin counts as exact in both lock formats' pin detection, so a
    /// dependency every member pins that way is pinned as a whole.
    #[test]
    fn prefixed_lock_specifiers_are_exact_pins() {
        for pin in ["v2.11.0", "=v2.11.0", "= v2.11.0"] {
            let pnpm = formatdoc! {"
                importers:

                  apps/a:
                    dependencies:
                      pinned:
                        specifier: {pin}
                        version: 2.11.0

                packages:

                  pinned@2.11.0:
                    resolution: {{integrity: sha512-x}}
            "};
            assert!(
                parse_pnpm_exact_pins(&pnpm, &HashSet::new())
                    .contains(&("pinned".to_string(), "2.11.0".to_string())),
                "{pin}"
            );
            let npm = serde_json::json!({
                "lockfileVersion": 3,
                "packages": {
                    "": { "dependencies": { "pinned": pin, "loose": "^1.0.0" } },
                    "node_modules/pinned": { "version": "2.11.0" },
                    "node_modules/loose": { "version": "1.0.0" }
                }
            })
            .to_string();
            let index = Npm::member_sources(&npm);
            assert!(index.is_exact_pinned("pinned", "2.11.0"), "{pin}");
            assert!(!index.is_exact_pinned("loose", "1.0.0"), "{pin}");
        }
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
            InstallLookup::Resolved(ResolvedInstance {
                version: "2.0.0",
                directory: "node_modules/host"
            }),
            "a hoisted instance resolves the hoisted peer"
        );
        assert_eq!(
            paths.resolve_from("node_modules/shadowed", "host"),
            InstallLookup::Resolved(ResolvedInstance {
                version: "1.0.0",
                directory: "node_modules/shadowed/node_modules/host"
            }),
            "a nested copy shadows the hoisted one"
        );
        assert_eq!(
            paths.member_resolution("apps/a", "plugin"),
            InstallLookup::Resolved(ResolvedInstance {
                version: "1.0.0",
                directory: "node_modules/plugin"
            }),
            "a workspace member resolves hoisted packages from its own directory"
        );
        assert_eq!(
            paths.resolve_from("node_modules/lib", "host"),
            InstallLookup::Resolved(ResolvedInstance {
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
            InstallLookup::Missing,
            "an uninstalled peer binds nowhere"
        );

        assert!(
            crate::lock::Npm::install_paths(r#"{"lockfileVersion": 1}"#).is_none(),
            "a v1 lock records no layout — the caller falls back to member overlap"
        );
    }

    /// An npm lock in which the root and member `apps/a` declare `foo`, with `foo`'s hoisted slot
    /// recording `name` when given and a member-nested slot beside it when `nested` is given.
    fn npm_alias_lock(lockfile_version: u8, name: Option<&str>, nested: Option<&str>) -> String {
        let name = name.map_or(String::new(), |name| format!(r#""name": "{name}", "#));
        let nested = nested.map_or(String::new(), |name| {
            format!(r#""apps/a/node_modules/foo": {{ "name": "{name}", "version": "3.0.0" }},"#)
        });
        formatdoc! {r#"
            {{
              "lockfileVersion": {lockfile_version},
              "packages": {{
                "": {{ "name": "root", "dependencies": {{ "foo": "npm:other@2.0.0" }} }},
                "apps/a": {{ "name": "a", "version": "0.1.0", "dependencies": {{ "foo": "1.0.0" }} }},
                {nested}
                "node_modules/foo": {{
                  {name}"version": "2.0.0",
                  "resolved": "https://registry.npmjs.org/other/-/other-2.0.0.tgz",
                  "peerDependencies": {{ "host": "^1.0.0" }}
                }}
              }}
            }}
        "#}
    }

    /// An npm alias slot records its target's `name`: the slot stays occupied, so a lookup of the
    /// alias name stops there unattributed instead of reaching a farther copy, and it is no
    /// instance of either name, while an entry with no `name`, or its own slot's, is the genuine
    /// copy.
    #[test]
    fn npm_install_paths_keep_an_alias_slot_occupied_but_unattributed() {
        for lockfile_version in [2, 3] {
            for target in ["other", "@scope/other"] {
                let paths =
                    Npm::install_paths(&npm_alias_lock(lockfile_version, Some(target), None))
                        .expect("the lock records the layout");
                assert_eq!(
                    paths.member_resolution(".", "foo"),
                    InstallLookup::Unknown,
                    "v{lockfile_version} {target}"
                );
                assert_eq!(
                    paths.member_resolution("apps/a", "foo"),
                    InstallLookup::Unknown
                );
                assert_eq!(
                    paths.member_resolution(".", target),
                    InstallLookup::Missing,
                    "the target is installed under the alias's name only"
                );
                assert_eq!(paths.instance_dirs("foo", "2.0.0"), [] as [&str; 0]);
                assert_eq!(paths.instance_dirs(target, "2.0.0"), [] as [&str; 0]);
            }
            for name in [None, Some("foo")] {
                let paths = Npm::install_paths(&npm_alias_lock(lockfile_version, name, None))
                    .expect("the lock records the layout");
                assert_eq!(
                    paths.member_resolution("apps/a", "foo"),
                    InstallLookup::Resolved(ResolvedInstance {
                        version: "2.0.0",
                        directory: "node_modules/foo"
                    }),
                    "v{lockfile_version} {name:?}"
                );
                assert_eq!(paths.instance_dirs("foo", "2.0.0"), ["node_modules/foo"]);
            }
        }

        // A member-nested alias shadows the genuine hoisted copy for that member only.
        let paths = Npm::install_paths(&npm_alias_lock(3, None, Some("other")))
            .expect("the lock records the layout");
        assert_eq!(
            paths.member_resolution("apps/a", "foo"),
            InstallLookup::Unknown
        );
        assert_eq!(
            paths.member_resolution(".", "foo"),
            InstallLookup::Resolved(ResolvedInstance {
                version: "2.0.0",
                directory: "node_modules/foo"
            })
        );
        assert_eq!(paths.instance_dirs("foo", "2.0.0"), ["node_modules/foo"]);
        assert_eq!(paths.instance_dirs("foo", "3.0.0"), [] as [&str; 0]);
        assert_eq!(paths.instance_dirs("other", "3.0.0"), [] as [&str; 0]);
    }

    /// A link is identified in its target's context: npm omits a workspace entry's `name` when it
    /// equals the directory's last component, so a link at `foo`'s slot to an unnamed
    /// `packages/other` is an alias, while a link to an unnamed `packages/foo` is the genuine copy.
    /// A link whose target records no version, or no entry at all, still occupies its slot, so it
    /// keeps hiding a genuine ancestor copy.
    #[test]
    fn npm_link_slots_are_identified_by_their_target() {
        let lock = |target: &str, target_entry: &str| {
            formatdoc! {r#"
                {{
                  "lockfileVersion": 3,
                  "packages": {{
                    "": {{ "name": "root", "workspaces": ["apps/a", "{target}"] }},
                    "apps/a": {{ "name": "a", "version": "0.1.0" }},
                    {target_entry}
                    "apps/a/node_modules/foo": {{ "resolved": "{target}", "link": true }},
                    "node_modules/foo": {{ "version": "1.0.0" }}
                  }}
                }}
            "#}
        };
        let genuine = lock("packages/foo", r#""packages/foo": { "version": "2.0.0" },"#);
        let paths = Npm::install_paths(&genuine).expect("the lock records the layout");
        assert_eq!(
            paths.member_resolution("apps/a", "foo"),
            InstallLookup::Resolved(ResolvedInstance {
                version: "2.0.0",
                directory: "apps/a/node_modules/foo"
            })
        );
        let mut dirs = paths.instance_dirs("foo", "2.0.0");
        dirs.sort_unstable();
        assert_eq!(dirs, ["apps/a/node_modules/foo", "packages/foo"]);

        for (target, target_entry) in [
            (
                "packages/other",
                r#""packages/other": { "version": "2.0.0" },"#,
            ),
            (
                "packages/other",
                r#""packages/other": { "name": "@scope/other", "version": "2.0.0" },"#,
            ),
            ("packages/foo", r#""packages/foo": { "name": "foo" },"#),
            ("packages/foo", ""),
        ] {
            let paths = Npm::install_paths(&lock(target, target_entry))
                .expect("the lock records the layout");
            assert_eq!(
                paths.member_resolution("apps/a", "foo"),
                InstallLookup::Unknown,
                "{target_entry:?}: the link still hides the root copy"
            );
            assert_eq!(
                paths.member_resolution(".", "foo"),
                InstallLookup::Resolved(ResolvedInstance {
                    version: "1.0.0",
                    directory: "node_modules/foo"
                })
            );
            assert_eq!(paths.instance_dirs("foo", "2.0.0"), [] as [&str; 0]);
        }
    }

    /// npm infers an unnamed workspace entry's name from its directory, scoped by an `@scope`
    /// parent, so an unnamed `packages/@scope/other` is `@scope/other`: its link at that name's
    /// slot is the genuine copy, a link to it at `other`'s slot is an alias, and its peer contract
    /// is recorded under the scoped name.
    #[test]
    fn an_unnamed_scoped_workspace_is_named_by_its_scope_and_folder() {
        assert_eq!(
            npm_folder_name("packages/@scope/other"),
            Some("@scope/other")
        );
        assert_eq!(npm_folder_name("@scope/other"), Some("@scope/other"));
        assert_eq!(npm_folder_name("packages/other"), Some("other"));
        assert_eq!(npm_folder_name("other"), Some("other"));
        assert_eq!(npm_folder_name("packages/@/other"), Some("other"));
        assert_eq!(npm_folder_name("packages/"), None);

        let lock = indoc! {r#"
            {
              "lockfileVersion": 3,
              "packages": {
                "": { "name": "root", "workspaces": ["packages/@scope/other"] },
                "packages/@scope/other": {
                  "version": "2.0.0",
                  "peerDependencies": { "host": "^1.0.0" }
                },
                "node_modules/@scope/other": { "resolved": "packages/@scope/other", "link": true },
                "node_modules/other": { "resolved": "packages/@scope/other", "link": true }
              }
            }
        "#};
        let paths = Npm::install_paths(lock).expect("the lock records the layout");
        assert_eq!(
            paths.member_resolution(".", "@scope/other"),
            InstallLookup::Resolved(ResolvedInstance {
                version: "2.0.0",
                directory: "node_modules/@scope/other"
            })
        );
        assert_eq!(
            paths.member_resolution(".", "other"),
            InstallLookup::Unknown
        );
        assert_eq!(paths.instance_dirs("other", "2.0.0"), [] as [&str; 0]);
        let mut dirs = paths.instance_dirs("@scope/other", "2.0.0");
        dirs.sort_unstable();
        assert_eq!(dirs, ["node_modules/@scope/other", "packages/@scope/other"]);
        assert_eq!(
            Npm::peer_requirements(lock),
            [PeerRequirement {
                dependent: "@scope/other".to_string(),
                dependent_version: "2.0.0".to_string(),
                package: "host".to_string(),
                range: "^1.0.0".to_string(),
            }]
        );
    }

    /// The flat package list and the recorded peer contracts name an alias's installed copy by its
    /// target, the package actually resolved there, with the entry's origin kept: the alias name
    /// never acquires the target's version.
    #[test]
    fn npm_packages_name_an_alias_by_its_target() -> Result<()> {
        for target in ["other", "@scope/other"] {
            let lock = npm_alias_lock(3, Some(target), None);
            let parsed = Npm::parse(&lock)?;
            assert_eq!(
                parsed,
                [NameVersion {
                    name: target.to_string(),
                    version: "2.0.0".to_string(),
                    origin: LockOrigin::Url(
                        "https://registry.npmjs.org/other/-/other-2.0.0.tgz".to_string()
                    ),
                }]
            );
            assert_eq!(
                Npm::peer_requirements(&lock),
                [PeerRequirement {
                    dependent: target.to_string(),
                    dependent_version: "2.0.0".to_string(),
                    package: "host".to_string(),
                    range: "^1.0.0".to_string(),
                }]
            );
        }
        // An entry without a `name` is still named by its slot.
        assert_eq!(
            Npm::parse(&npm_alias_lock(3, None, None))?
                .into_iter()
                .map(|NameVersion { name, .. }| name)
                .collect::<Vec<_>>(),
            ["foo"]
        );
        Ok(())
    }

    /// An alias specifier is no exact pin, and an exact pin of the alias name confers no pinned
    /// status on the alias target.
    #[test]
    fn an_npm_alias_confers_no_pin() {
        let lock = npm_alias_lock(3, Some("other"), None);
        assert!(!is_exact_npm_specifier("npm:other@2.0.0"));
        let index = Npm::member_sources(&lock);
        assert!(!index.is_exact_pinned("other", "2.0.0"));
        assert!(
            !index.is_exact_pinned("foo", "2.0.0"),
            "the root's alias is no pin"
        );
        assert_eq!(index.members_for("other", "2.0.0"), [] as [String; 0]);
    }

    /// A v1 lock records an alias as `"foo": { "version": "npm:other@2.0.0" }`, so the flat list
    /// names the entry by its target, scoped or not, with the entry's own `resolved` origin.
    /// A plain entry keeps its key.
    #[test]
    fn npm_v1_names_an_alias_by_its_target() -> Result<()> {
        let lock = indoc! {r#"
            {
              "lockfileVersion": 1,
              "dependencies": {
                "foo": {
                  "version": "npm:other@2.0.0",
                  "resolved": "https://registry.npmjs.org/other/-/other-2.0.0.tgz"
                },
                "bar": {
                  "version": "npm:@scope/other@3.0.0",
                  "resolved": "https://npm.corp.example/@scope/other/-/other-3.0.0.tgz"
                },
                "unrecorded-alias": { "version": "npm:target@1.2.0" },
                "unsaved-alias": { "version": "npm:target@1.3.0", "resolved": false },
                "lodash": {
                  "version": "4.17.15",
                  "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.15.tgz"
                },
                "unrecorded": { "version": "1.0.0" }
              }
            }
        "#};
        assert_eq!(
            sorted(Npm::parse(lock)?),
            sorted(vec![
                NameVersion {
                    name: "other".to_string(),
                    version: "2.0.0".to_string(),
                    origin: LockOrigin::Url(
                        "https://registry.npmjs.org/other/-/other-2.0.0.tgz".to_string()
                    ),
                },
                NameVersion {
                    name: "@scope/other".to_string(),
                    version: "3.0.0".to_string(),
                    origin: LockOrigin::Url(
                        "https://npm.corp.example/@scope/other/-/other-3.0.0.tgz".to_string()
                    ),
                },
                NameVersion::new("target", "1.2.0"),
                NameVersion::new("target", "1.3.0"),
                NameVersion {
                    name: "lodash".to_string(),
                    version: "4.17.15".to_string(),
                    origin: LockOrigin::Url(
                        "https://registry.npmjs.org/lodash/-/lodash-4.17.15.tgz".to_string()
                    ),
                },
                NameVersion::new("unrecorded", "1.0.0"),
            ])
        );
        Ok(())
    }

    /// A v1 entry that resolves no registry release yields no row, and a malformed alias is never
    /// read as a version of the alias name instead.
    #[test]
    fn npm_v1_skips_non_registry_and_malformed_versions() -> Result<()> {
        let lock = indoc! {r#"
            {
              "lockfileVersion": 1,
              "dependencies": {
                "local": { "version": "file:packages/local" },
                "tarball": { "version": "file:vendor/tarball-1.0.0.tgz" },
                "repo": { "version": "git+https://github.com/user/repo.git#0123abc" },
                "shorthand": { "version": "github:user/repo#0123abc" },
                "remote": { "version": "https://example.com/remote-1.0.0.tgz" },
                "empty": { "version": "" },
                "missing": { "resolved": "https://registry.npmjs.org/missing/-/missing-1.0.0.tgz" },
                "bare-alias": { "version": "npm:target" },
                "scope-only-alias": { "version": "npm:@scope/target" },
                "versionless-alias": { "version": "npm:target@" },
                "git-alias": { "version": "npm:target@git+https://github.com/user/target.git" },
                "kept": { "version": "1.0.0" }
              }
            }
        "#};
        assert_eq!(Npm::parse(lock)?, [NameVersion::new("kept", "1.0.0")]);
        Ok(())
    }

    /// A v1 lock records no install tree, so the alias evidence comes from its top-level entries:
    /// a foreign alias target counts until a genuine registry copy of the same release sits under
    /// the target's own key, which a local source of that name never is.
    #[test]
    fn npm_v1_alias_only_versions() {
        let alias_only = |entries: &str| {
            let lock = formatdoc! {r#"
                {{ "lockfileVersion": 1, "dependencies": {{ {entries} }} }}
            "#};
            Npm::alias_only_versions(&lock)
        };
        let alias = r#""foo": { "version": "npm:other@2.0.0" }"#;

        // Alias alone
        let versions = alias_only(alias);
        assert!(versions.contains("other", "2.0.0"));
        assert!(!versions.contains("foo", "2.0.0"));
        assert!(!versions.contains("other", "1.0.0"));

        // A scoped target
        let versions = alias_only(r#""foo": { "version": "npm:@scope/other@2.0.0" }"#);
        assert!(versions.contains("@scope/other", "2.0.0"));

        // Genuine registry copies of the same release, recorded or not
        for copy in [
            r#""other": { "version": "2.0.0" }"#,
            r#""other": { "version": "2.0.0", "resolved": false }"#,
            r#""other": { "version": "2.0.0", "resolved": "https://registry.npmjs.org/o.tgz" }"#,
        ] {
            assert!(
                !alias_only(&format!("{alias}, {copy}")).contains("other", "2.0.0"),
                "{copy}"
            );
        }

        // Local sources of the alias target's name
        for copy in [
            r#""other": { "version": "file:packages/other" }"#,
            r#""other": { "version": "2.0.0", "resolved": "file:vendor/other-2.0.0.tgz" }"#,
        ] {
            assert!(
                alias_only(&format!("{alias}, {copy}")).contains("other", "2.0.0"),
                "{copy}"
            );
        }

        // Same-name alias and an unrelated release of the target
        assert!(!alias_only(r#""foo": { "version": "npm:foo@2.0.0" }"#).contains("foo", "2.0.0"));
        assert!(
            alias_only(&format!(r#"{alias}, "other": {{ "version": "1.0.0" }}"#))
                .contains("other", "2.0.0")
        );

        // The install tree answers for a v2/v3 lock
        let versions = Npm::alias_only_versions(&npm_alias_lock(3, Some("other"), None));
        assert!(versions.contains("other", "2.0.0"));
        assert!(!versions.contains("foo", "2.0.0"));
    }

    /// Normalizing v1 identities manufactures no install tree: an alias and a plain binding alike
    /// keep the conservative v1 readers, which attribute no members, pins, or peers.
    #[test]
    fn npm_v1_aliases_keep_the_conservative_readers() {
        let lock = indoc! {r#"
            {
              "lockfileVersion": 1,
              "dependencies": {
                "foo": { "version": "npm:other@2.0.0" },
                "other": { "version": "1.0.0" }
              }
            }
        "#};
        let index = Npm::member_sources(lock);
        assert!(!index.is_exact_pinned("other", "1.0.0"));
        assert!(!index.is_exact_pinned("other", "2.0.0"));
        assert_eq!(index.members_for("other", "2.0.0"), [] as [String; 0]);
        assert_eq!(Npm::peer_requirements(lock), [] as [PeerRequirement; 0]);
        assert!(Npm::install_paths(lock).is_none());
        assert!(matches!(
            Npm::member_attribution(lock),
            MemberAttribution::Unknown
        ));
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

    /// The expected member list of a copy no member binds.
    const NO_MEMBERS: [String; 0] = [];

    /// The member paths a Bun lock yields, sorted.
    fn bun_member_paths(lock: &str) -> Vec<String> {
        let mut paths: Vec<String> = Bun::member_paths(lock).into_iter().collect();
        paths.sort();
        paths
    }

    /// Every member binds its own copy: the root through its dependency's key, another member
    /// through `<workspace-name>/<dependency>` first and the root key after it, scoped names
    /// whole on both sides.
    #[test]
    fn bun_attributes_each_member_its_own_copy() {
        use bun_fixtures::WORKSPACES;
        let attribution = Bun::member_attribution(WORKSPACES);

        // Two members pinning one name at different versions each resolve their own copy.
        assert_eq!(
            attribution.resolution(".", "chalk"),
            MemberResolution::Resolved("4.1.2")
        );
        assert_eq!(
            attribution.resolution("packages/a", "chalk"),
            MemberResolution::Resolved("4.1.0")
        );
        assert_eq!(
            attribution.resolution("packages/scoped", "chalk"),
            MemberResolution::Resolved("5.3.0")
        );

        // Without a nested key, the member falls back to the root's copy.
        assert_eq!(
            attribution.resolution("packages/b", "chalk"),
            MemberResolution::Resolved("4.1.2")
        );
        assert_eq!(
            attribution.resolution("packages/scoped", "@types/node"),
            MemberResolution::Resolved("20.1.0")
        );

        // A scoped dependency nests under the workspace name, not its directory.
        assert_eq!(
            attribution.resolution("packages/a", "@types/node"),
            MemberResolution::Resolved("20.0.0")
        );

        // A root key the member does not declare binds nothing for it.
        assert_eq!(
            attribution.resolution(".", "@types/node"),
            MemberResolution::Missing
        );

        // A foreign alias and a linked workspace are no registry copy of their name.
        assert_eq!(
            attribution.resolution("packages/b", "isnum"),
            MemberResolution::Unknown
        );
        assert_eq!(
            attribution.resolution("packages/b", "pkg-a"),
            MemberResolution::Unknown
        );

        // The index attributes each copy to its declaring members only.
        let index = Bun::member_sources(WORKSPACES);
        assert!(index.is_authoritative());
        assert_eq!(index.members_for("chalk", "4.1.2"), [".", "packages/b"]);
        assert_eq!(index.members_for("chalk", "4.1.0"), ["packages/a"]);
        assert_eq!(index.members_for("chalk", "5.3.0"), ["packages/scoped"]);
        assert_eq!(index.members_for("@types/node", "20.0.0"), ["packages/a"]);
        assert_eq!(
            index.members_for("@types/node", "20.1.0"),
            ["packages/scoped"]
        );
        assert_eq!(index.members_for("is-number", "6.0.0"), NO_MEMBERS);
        assert_eq!(index.resolved_version("packages/b", "chalk"), Some("4.1.2"));
        assert_eq!(
            index.resolved_versions_of("chalk"),
            ["4.1.0", "4.1.2", "5.3.0"]
        );
        assert_eq!(
            bun_member_paths(WORKSPACES),
            [".", "packages/a", "packages/b", "packages/scoped"]
        );
    }

    /// Each member's exact registry declarations pin their own copy, its first field's specifier
    /// is its recorded entry, and an excluded member contributes nothing.
    #[test]
    fn bun_index_records_exact_pins_and_entries_per_member() {
        use bun_fixtures::WORKSPACES;
        let index = Bun::member_sources(WORKSPACES);
        assert!(index.is_exact_pinned("chalk", "4.1.0"));
        assert!(index.is_exact_pinned("chalk", "4.1.2"));
        assert!(!index.is_exact_pinned("is-number", "7.0.0"));
        assert_eq!(
            index.entries_of("packages/scoped").get("chalk"),
            Some(&&ImporterEntry {
                specifier: Some("5.3.0".into()),
                version: "5.3.0".into(),
            })
        );
        assert_eq!(
            index.declared_specifiers_of("chalk"),
            ["4.1.0", "4.1.2", "5.3.0"]
        );
        assert!(!index.entries_of("packages/b").contains_key("isnum"));

        let excluded = HashSet::from(["packages/a".to_string()]);
        let index = Bun::member_sources_excluding(WORKSPACES, &excluded);
        assert_eq!(index.members_for("chalk", "4.1.0"), NO_MEMBERS);
        assert_eq!(index.declared_specifiers_of("chalk"), ["4.1.2", "5.3.0"]);
    }

    /// A peer declaration is bound by Bun's version scan rather than the member's key, so even an
    /// exact dev pin beside it is unknown, while an optional dependency resolves through its key.
    /// The unknown binding keeps its declaration: the member owns the one copy its key records,
    /// though it resolves to none, and no copy is pinned for it.
    #[test]
    fn bun_peer_declarations_are_unknown_and_optional_ones_resolve() {
        use bun_fixtures::PEER_AND_OPTIONAL;
        let attribution = Bun::member_attribution(PEER_AND_OPTIONAL);
        assert_eq!(
            attribution.resolution("packages/a", "chalk"),
            MemberResolution::Unknown
        );
        assert_eq!(
            attribution.resolution("packages/b", "chalk"),
            MemberResolution::Resolved("4.1.1")
        );
        let index = Bun::member_sources(PEER_AND_OPTIONAL);
        assert!(index.is_authoritative());
        assert!(index.declares("chalk"));
        assert_eq!(index.members_for("chalk", "4.1.0"), ["packages/a"]);
        assert_eq!(index.members_for("chalk", "4.1.1"), ["packages/b"]);
        assert_eq!(index.members_for("chalk", "4.1.2"), ["."]);
        assert_eq!(index.resolved_version("packages/a", "chalk"), None);
        assert!(!index.is_exact_pinned("chalk", "4.1.0"));
        assert_eq!(
            index.declared_specifiers_of("chalk"),
            ["4.1.0", "4.1.1", "4.1.2"]
        );
    }

    /// A root that declares a name both as a dev dependency and as a peer keeps it declared through
    /// the copy its key records, so the dependency stays direct even though the lock leaves which
    /// copy it binds unknown.
    #[test]
    fn bun_root_peer_and_dev_declaration_stays_declared() {
        use bun_fixtures::ROOT_PEER_AND_DEV;
        let attribution = Bun::member_attribution(ROOT_PEER_AND_DEV);
        for member in [".", "packages/a"] {
            assert_eq!(
                attribution.resolution(member, "chalk"),
                MemberResolution::Unknown,
                "{member}"
            );
        }
        let index = Bun::member_sources(ROOT_PEER_AND_DEV);
        assert!(index.is_authoritative());
        assert_eq!(index.members_for("chalk", "4.1.2"), ["."]);
        assert_eq!(index.members_for("chalk", "4.1.0"), ["packages/a"]);
        assert_eq!(index.resolved_version(".", "chalk"), None);
        assert!(!index.is_exact_pinned("chalk", "4.1.0"));
    }

    /// A member whose binding is uncertain owns only the copy its key records, and only when that
    /// copy satisfies one of its own declarations, never another line of the name.
    #[test]
    fn bun_uncertain_member_owns_only_its_recorded_copy() {
        use bun_fixtures::PEER_BESIDE_TRANSITIVE;
        let index = Bun::member_sources(PEER_BESIDE_TRANSITIVE);
        assert_eq!(index.members_for("chalk", "4.1.0"), ["packages/b"]);
        assert_eq!(index.members_for("chalk", "5.6.2"), NO_MEMBERS);

        // A recorded copy outside every declaration proves no ownership.
        let outside = PEER_BESIDE_TRANSITIVE.replace(r#"">=4.1.0 <4.1.1""#, r#""4.1.1""#);
        let outside = outside.replace(r#""^4.0.0""#, r#""4.1.1""#);
        assert_eq!(
            Bun::member_sources(&outside).members_for("chalk", "4.1.0"),
            NO_MEMBERS
        );
    }

    /// A foreign alias and a workspace link are proven to bind no registry copy of their name, so
    /// they own none, unlike a declaration whose copy is merely undetermined.
    #[test]
    fn bun_foreign_and_local_declarations_own_no_registry_copy() {
        use bun_fixtures::{PEER_AND_OPTIONAL, WORKSPACES};
        let bindings = BunBindings::read(WORKSPACES);
        for name in ["isnum", "pkg-a"] {
            assert!(
                bindings
                    .as_ref()
                    .is_some_and(|bindings| !bindings.is_uncertain("packages/b", name)),
                "{name}"
            );
        }
        let index = Bun::member_sources(WORKSPACES);
        assert!(!index.declares("isnum"));
        assert!(!index.declares("pkg-a"));
        assert_eq!(index.members_for("isnum", "6.0.0"), NO_MEMBERS);

        // A peer declaration of the name itself is the uncertain case.
        assert!(
            BunBindings::read(PEER_AND_OPTIONAL)
                .is_some_and(|bindings| bindings.is_uncertain("packages/a", "chalk"))
        );
    }

    /// A member whose name the root's registry dependency holds files its keys under `./<name>`
    /// in current Bun and under `<name>` in Bun 1.3.0, so its bindings are unknown, its siblings'
    /// stay readable, and the index is not authoritative.
    #[test]
    fn bun_member_colliding_with_a_root_dependency_is_unknown() {
        use bun_fixtures::COLLIDING_MEMBER;
        let attribution = Bun::member_attribution(COLLIDING_MEMBER);
        assert_eq!(
            attribution.resolution("packages/a", "chalk"),
            MemberResolution::Unknown
        );
        assert_eq!(
            attribution.resolution("packages/b", "chalk"),
            MemberResolution::Resolved("4.1.2")
        );
        assert_eq!(
            attribution.resolution(".", "is-number"),
            MemberResolution::Resolved("7.0.0")
        );
        let index = Bun::member_sources(COLLIDING_MEMBER);
        assert!(!index.is_authoritative());
        assert!(index.all_paths().is_empty());
        assert_eq!(
            bun_member_paths(COLLIDING_MEMBER),
            [".", "packages/a", "packages/b"]
        );
    }

    /// A lock outside the supported schema attributes nothing: no member, no index, no binding.
    #[test]
    fn bun_unsupported_schemas_attribute_nothing() {
        let lock = |version: &str, root: &str| {
            formatdoc! {r#"
                {{
                  "lockfileVersion": {version},
                  "workspaces": {{
                    "{root}": {{ "dependencies": {{ "chalk": "4.1.0" }} }},
                  }},
                  "packages": {{ "chalk": ["chalk@4.1.0", "", {{}}, "sha512-x"] }},
                }}
            "#}
        };
        // Supported baseline
        assert_eq!(
            Bun::member_attribution(&lock("1", "")).resolution(".", "chalk"),
            MemberResolution::Resolved("4.1.0")
        );

        // Version 0, an unreleased version, a non-numeric version, no root, and no JSON at all
        for lock in [
            lock("0", ""),
            lock("4", ""),
            lock(r#""1""#, ""),
            lock("1", "packages/a"),
            "{ not json".to_string(),
        ] {
            assert!(
                matches!(Bun::member_attribution(&lock), MemberAttribution::Unknown),
                "{lock}"
            );
            assert!(!Bun::member_sources(&lock).is_authoritative(), "{lock}");
            assert!(Bun::member_paths(&lock).is_empty(), "{lock}");
        }
    }

    /// Every binding the lock does not establish is unknown, never missing: only a dependency the
    /// member's entry does not declare, or an optional one no key holds, is genuinely absent.
    #[test]
    fn bun_uncertain_bindings_are_unknown_not_missing() {
        let lock = indoc! {r#"
            {
              "lockfileVersion": 1,
              "workspaces": {
                "": {
                  "dependencies": {
                    "absent": "1.0.0",
                    "garbled": "1.0.0",
                    "git": "github:acme/git",
                    "unread": 1,
                  },
                  "optionalDependencies": { "fsevents": "2.3.3" },
                },
                "../outside": { "name": "outside", "dependencies": { "chalk": "4.1.0" } },
                "packages/moved": { "name": "moved", "dependencies": { "chalk": "4.1.0" } },
                "packages/twin-a": { "name": "twin", "dependencies": { "chalk": "4.1.0" } },
                "packages/twin-b": { "name": "twin", "dependencies": { "chalk": "4.1.0" } },
                "packages/broken": { "name": "broken", "dependencies": ["chalk"] },
              },
              "packages": {
                "chalk": ["chalk@4.1.0", "", {}, "sha512-x"],
                "garbled": "garbled@1.0.0",
                "git": ["git@git+ssh://git@github.com/acme/git.git#abc", {}, "abc"],
                "moved": ["moved@workspace:packages/elsewhere"],
                "twin": ["twin@workspace:packages/twin-a"],
                "broken": ["broken@workspace:packages/broken"],
              },
            }
        "#};
        let attribution = Bun::member_attribution(lock);
        let resolution = |member: &str, name: &str| attribution.resolution(member, name);

        // Genuine absence
        assert_eq!(resolution(".", "chalk"), MemberResolution::Missing);
        assert_eq!(resolution(".", "fsevents"), MemberResolution::Missing);

        // A required dependency no key holds, a malformed entry, a git source, an unread
        // specifier
        for name in ["absent", "garbled", "git", "unread"] {
            assert_eq!(resolution(".", name), MemberResolution::Unknown, "{name}");
        }

        // An unsafe member path, a name whose key holds another path, a name two members share,
        // and a malformed entry
        for member in [
            "../outside",
            "packages/moved",
            "packages/twin-a",
            "packages/twin-b",
            "packages/broken",
        ] {
            assert_eq!(
                resolution(member, "chalk"),
                MemberResolution::Unknown,
                "{member}"
            );
        }
        assert_eq!(
            bun_member_paths(lock),
            [
                ".",
                "packages/broken",
                "packages/moved",
                "packages/twin-a",
                "packages/twin-b"
            ]
        );
    }

    /// A package identity splits at the first `@` after a scope, so neither a scoped name nor a
    /// git reference containing `@` misreads as a version.
    #[test]
    fn bun_registry_versions_split_at_the_name() {
        assert_eq!(
            bun_registry_version("@types/node", "@types/node@20.0.0"),
            Some("20.0.0")
        );
        assert_eq!(
            bun_registry_version("chalk", "chalk@5.0.0+build.1"),
            Some("5.0.0+build.1")
        );
        assert_eq!(bun_registry_version("isnum", "is-number@6.0.0"), None);
        assert_eq!(
            bun_registry_version("git", "git@git+ssh://git@github.com/acme/git.git#abc"),
            None
        );
        assert_eq!(
            bun_registry_version("pkg-a", "pkg-a@workspace:packages/a"),
            None
        );
        assert_eq!(bun_registry_version("root", "root@root:"), None);
        assert_eq!(bun_registry_version("chalk", "chalk"), None);
    }
}
