//! Thin wrappers around the project's own `cargo` binary (resolution/apply engine only).

use camino::Utf8Path;
use cooldown_adapter_util::resolve_program;
use cooldown_core::{CoreError, MemberRef, ToolTermination, VerifyReport, failure_detail};
use std::collections::{HashMap, HashSet};
use tokio::process::Command;

/// The `source` string a `Cargo.lock` entry and `cargo metadata` carry for crates.io packages.
pub(crate) const CRATES_IO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// The crates.io index in Cargo package-ID-spec form (`<url>#<name>@<version>`) — the same
/// source as [`CRATES_IO_SOURCE`] without the `registry+` kind prefix.
const CRATES_IO_SPEC_SOURCE: &str = "https://github.com/rust-lang/crates.io-index";

/// The resolved dependency graph, distilled from `cargo metadata`.
pub struct ResolvedGraph {
    /// package id → (name, version, source).
    pub packages: HashMap<String, PkgInfo>,
    /// workspace members / roots (their edges are what `upgrade` can change).
    pub roots: HashSet<String>,
    /// node id → its resolved dependency package ids.
    pub edges: HashMap<String, Vec<String>>,
    /// `(crate, version)` pairs a workspace member pins exactly (`serde = "=1.0.197"`). A single
    /// `=` requirement forces that resolved version, so it is held: it cannot move without editing a
    /// `Cargo.toml`.
    pub exact_pins: HashSet<(String, String)>,
    /// `(crate, version)` nodes some *requirer* caps with an exact `=x.y.z` requirement — the
    /// upgrade-direction mirror of the graph floor. Cargo coexists multiple versions of one crate, so
    /// the cap is per resolved node, not per name: only the node whose version equals the pin is held
    /// (the ceiling is that node's own version). Restricted to pins whose edge is actually in the
    /// resolved graph: dev-dependencies and inactive (optional/target-gated) edges are excluded, as
    /// they cap nothing. Workspace-member pins are surfaced via [`exact_pins`](Self::exact_pins)
    /// instead, so the consumer ignores a ceiling on a pinned node.
    pub graph_ceilings: HashSet<(String, String)>,
    /// For each capped `(crate, version)` node, the *requirer* crate names whose active `=x.y.z`
    /// edge imposes that cap — the blame source when a candidate is held below its target by a shared
    /// single-major pin. A node may have several requirers all pinning the same version; the consumer
    /// names one. Keyed by the same `(name, version)` as [`graph_ceilings`](Self::graph_ceilings).
    pub ceiling_requirers: HashMap<(String, String), Vec<String>>,
    /// The graph floor per resolved `(crate, version)` node: the highest lower bound any active
    /// non-root requirer's version requirement imposes on it. Cargo picks the *newest* version
    /// satisfying every requirer's range, so a resolved node can sit far above the floor the ranges
    /// actually demand — e.g. a `quote` every crate requires as `^1.0` resolves to the latest `1.0.x`
    /// even though `1.0.0` satisfies them all. The floor records that demanded minimum so a too-fresh
    /// node a re-resolve floats up can be matured *down* to the newest version still at or above it.
    /// Workspace-member requirements are project-owned constraints that cooldown can rewrite for
    /// direct deps, so they are tracked as `pinned`/members instead of immutable graph floors.
    pub graph_floors: HashMap<(String, String), String>,
    /// The most restrictive explicit upper-bound requirement declared by workspace members for
    /// each active resolved node.
    pub declared_bounds: HashMap<(String, String), String>,
    /// Every version requirement each resolved package declares, by dependency name — the
    /// requirements a lock edge of that package must satisfy. Dev-dependencies of
    /// non-workspace packages are excluded (they are never resolved into the lock, so they
    /// constrain no edge); workspace members' dev deps are resolved and included. Keyed by
    /// [`PackageKey`] because name and version are all a `Cargo.lock` entry carries; a
    /// same-name-same-version collision across sources merges its requirements, which only ever
    /// *over*-constrains an edge check (the safe direction).
    pub declared_requirements: HashMap<PackageKey, Vec<DeclaredRequirement>>,
    /// Each resolved package's declared `rust-version` (MSRV); absent when the package declares
    /// none (or it is unparsable, which cargo's MSRV-aware resolver likewise treats as
    /// compatible).
    pub rust_versions: HashMap<PackageKey, RustVersion>,
    /// The lowest `rust-version` any workspace member declares — the workspace MSRV cargo's
    /// MSRV-aware resolver honors when picking versions — or `None` when no member declares one.
    pub workspace_rust_version: Option<RustVersion>,
}

/// A resolved package's `(name, version)` identity, as both `cargo metadata` and `Cargo.lock`
/// spell it. Not guaranteed unique — the same name and version can resolve from two sources at
/// once; maps keyed by it merge such collisions, so each consumer must be safe under that merge.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageKey {
    /// The crate's package name.
    pub name: String,
    /// The resolved version.
    pub version: String,
}

impl PackageKey {
    /// Builds the identity from anything string-like, cloning borrowed inputs.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        PackageKey {
            name: name.into(),
            version: version.into(),
        }
    }
}

/// A declared `rust-version` (MSRV) as a release triple. The derived ordering compares
/// `major`, then `minor`, then `patch` — semver precedence, since the manifest field forbids
/// prereleases and build metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RustVersion {
    /// The major component (`1` in `1.70.3`).
    pub major: u64,
    /// The minor component; zero when the manifest omits it.
    pub minor: u64,
    /// The patch component; zero when the manifest omits it.
    pub patch: u64,
}

impl RustVersion {
    /// Builds the triple from its components.
    #[must_use]
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        RustVersion {
            major,
            minor,
            patch,
        }
    }
}

/// Parses a manifest `rust-version` value (`"1.70"`, `"1.70.0"`) into a comparable
/// [`RustVersion`], with missing components zeroed. `None` for anything else — the field forbids
/// ranges and prereleases, so an unparsable value is treated as undeclared.
fn parse_rust_version(value: &str) -> Option<RustVersion> {
    let mut components = value.trim().splitn(3, '.');
    let major = components.next()?.parse().ok()?;
    let minor = components.next().map_or(Some(0), |c| c.parse().ok())?;
    let patch = components.next().map_or(Some(0), |c| c.parse().ok())?;
    Some(RustVersion {
        major,
        minor,
        patch,
    })
}

/// Accumulates the MSRV data [`build_graph`](Cargo::build_graph) collects per package: each
/// declared `rust-version` keyed by package identity, and the workspace minimum across members.
#[derive(Default)]
struct MsrvIndex {
    rust_versions: HashMap<PackageKey, RustVersion>,
    workspace_rust_version: Option<RustVersion>,
}

impl MsrvIndex {
    fn record(&mut self, package: &RawPkg, is_member: bool) {
        let Some(msrv) = package.rust_version.as_deref().and_then(parse_rust_version) else {
            return;
        };
        self.rust_versions
            .insert(PackageKey::new(&*package.name, &*package.version), msrv);
        // The workspace MSRV is the lowest member declaration — the strictest bound the MSRV-aware
        // resolver honors for any member's dependency choice.
        if is_member {
            self.workspace_rust_version = Some(
                self.workspace_rust_version
                    .map_or(msrv, |current| current.min(msrv)),
            );
        }
    }
}

/// One version requirement a resolved package declares, as the edge-policy module consumes it.
#[derive(Debug, Clone)]
pub struct DeclaredRequirement {
    /// The depended-on crate's package name (not a manifest rename).
    pub dependency: String,
    /// The verbatim semver requirement string, e.g. `^1.0` or `>=0.7.0, <2.0.0`.
    pub requirement: String,
}

/// A single resolved package from `cargo metadata`.
pub struct PkgInfo {
    /// The crate name (e.g. `serde`).
    pub name: String,
    /// The exact resolved version (e.g. `1.0.197`).
    pub version: String,
    /// The source registry/path URL, or [`None`] for path/workspace members.
    pub source: Option<String>,
    /// The crate's directory relative to the workspace root (`.` for a crate at the root); used to
    /// attribute a dependency to its source member by path.
    pub path: String,
}

impl PkgInfo {
    /// Returns `true` when this package was resolved from the crates.io registry.
    ///
    /// Only crates.io packages have publish times in the sparse index, so this
    /// gates which dependencies the cooldown policy can evaluate.
    #[must_use]
    pub fn is_crates_io(&self) -> bool {
        self.source.as_deref() == Some(CRATES_IO_SOURCE)
    }
}

impl ResolvedGraph {
    /// Is `id` an edge target of any root node (a direct dep)?
    #[must_use]
    pub fn is_direct(&self, id: &str) -> bool {
        self.roots
            .iter()
            .filter_map(|r| self.edges.get(r))
            .any(|deps| deps.iter().any(|d| d == id))
    }
    /// Is `crate_name` at `version` exact-pinned (`=x.y.z`) by a workspace member?
    #[must_use]
    pub fn is_exact_pinned(&self, crate_name: &str, version: &str) -> bool {
        self.exact_pins
            .contains(&(crate_name.to_string(), version.to_string()))
    }

    /// Is the `crate_name`@`version` node capped by some requirer's exact `=x.y.z` requirement (its
    /// graph ceiling)? Keyed per node because Cargo coexists multiple versions of a crate.
    #[must_use]
    pub fn is_graph_capped(&self, crate_name: &str, version: &str) -> bool {
        self.graph_ceilings
            .contains(&(crate_name.to_string(), version.to_string()))
    }

    /// The requirer crate whose active `=x.y.z` edge caps any node of `held` below `target` — the
    /// crate to blame when a candidate is held back by a shared single-major exact pin. Scans the
    /// capped nodes of `held`, keeps those pinned below `target`, and returns the (sorted, stable)
    /// first requirer name. `None` when no requirer caps `held` below the target.
    #[must_use]
    pub fn exact_requirer_of(&self, held: &str, target: &str) -> Option<String> {
        let mut requirers: Vec<&str> = self
            .ceiling_requirers
            .iter()
            .filter(|((name, version), _)| {
                name == held && crate::version::compare(target, version).is_gt()
            })
            .flat_map(|(_, requirers)| requirers.iter().map(String::as_str))
            .collect();
        requirers.sort_unstable();
        requirers.into_iter().next().map(str::to_string)
    }

    /// The graph floor for the `crate_name`@`version` node — the highest lower bound its active
    /// non-root requirers' ranges demand — or `None` when no such requirer imposes a parseable one.
    /// Keyed per node because Cargo coexists multiple versions of a crate.
    #[must_use]
    pub fn graph_floor(&self, crate_name: &str, version: &str) -> Option<&str> {
        self.graph_floors
            .get(&(crate_name.to_string(), version.to_string()))
            .map(String::as_str)
    }

    /// The workspace requirement that explicitly upper-bounds `crate_name` at `version`.
    #[must_use]
    pub fn declared_bound(&self, crate_name: &str, version: &str) -> Option<&str> {
        self.declared_bounds
            .get(&(crate_name.to_string(), version.to_string()))
            .map(String::as_str)
    }

    /// Resolve a graph node id to its workspace-member `(name, path)`, or `None` for a node that is
    /// not a known package. The shared mapping behind both attribution methods below.
    fn member_of(&self, node: &str) -> Option<MemberRef> {
        self.packages.get(node).map(|info| MemberRef {
            name: info.name.clone(),
            path: info.path.clone(),
        })
    }

    /// Whether every listed workspace member directly resolves `crate_name` at the exact requested
    /// target, having actually left the `from` line behind.
    ///
    /// Cargo can keep several versions of the same crate in one workspace. A lock-level check that
    /// only asks whether `crate_name@target` exists can therefore confuse another member's
    /// dependency for this member's unresolved one. And a member can itself hold the target through
    /// a *different* manifest entry: with `[dependencies] toml = "1"` beside `[build-dependencies]
    /// toml = "0.5"`, an edge into the target major exists before the planned `0.5.x` move is even
    /// attempted, so a cross-major move additionally requires the member to have no remaining edge
    /// into `from`'s major — otherwise the untouched old line masquerades as an applied move.
    #[must_use]
    pub fn direct_members_reach(
        &self,
        members: &[MemberRef],
        crate_name: &str,
        from: &str,
        target: &str,
    ) -> bool {
        if members.is_empty() {
            return false;
        }

        let from_major = crate::version::major_key(from);
        let target_major = crate::version::major_key(target);
        for member in members {
            let Some(root) = self.roots.iter().find(|root| {
                self.packages
                    .get(*root)
                    .is_some_and(|info| info.name == member.name && info.path == member.path)
            }) else {
                return false;
            };
            let Some(dep_ids) = self.edges.get(root) else {
                return false;
            };
            let mut reached = false;
            for info in dep_ids.iter().filter_map(|id| self.packages.get(id)) {
                if info.name != crate_name || !info.is_crates_io() {
                    continue;
                }
                let major = crate::version::major_key(&info.version);
                // A cross-major move must vacate the old slot: any surviving member edge into
                // `from`'s major means the planned line never moved, no matter what other entries
                // already resolve in the target major.
                if from_major != target_major && major == from_major {
                    return false;
                }
                // Scope to the target's own compatibility slot, like `reached` does via its
                // `(name, major)` key. One member can resolve several majors of a crate at once
                // (a normal `nix = "0.28"` beside a target-gated `nix = "0.31"`); without this a
                // sibling major that satisfies the bound would mask the slot we are moving.
                if major != target_major {
                    continue;
                }
                reached |= info.version == target;
            }
            if !reached {
                return false;
            }
        }

        true
    }

    /// The workspace member crates that directly depend on `id` — the source packages a dependency
    /// is attributed to in reports. Sorted by name and deduplicated for stable output.
    #[must_use]
    pub fn direct_members(&self, id: &str) -> Vec<MemberRef> {
        let mut members: Vec<MemberRef> = self
            .roots
            .iter()
            .filter(|root| {
                self.edges
                    .get(*root)
                    .is_some_and(|deps| deps.iter().any(|dep| dep == id))
            })
            .filter_map(|root| self.member_of(root))
            .collect();
        members.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
        members.dedup();
        members
    }

    /// The workspace members that reach `id` through the graph — directly or transitively — so a
    /// *transitive* dependency can be attributed to the members that pull it in ("via …"). Uses the
    /// shared, tool-agnostic reverse-reachability helper over this graph's edges.
    #[must_use]
    pub fn reaching_members(&self, id: &str) -> Vec<MemberRef> {
        let edges = self
            .edges
            .iter()
            .flat_map(|(from, tos)| tos.iter().map(move |to| (from.as_str(), to.as_str())));
        let roots: HashSet<&str> = self.roots.iter().map(String::as_str).collect();
        cooldown_adapter_util::reaching_members(edges, &roots, id, |node| self.member_of(node))
    }
}

#[derive(serde::Deserialize)]
struct RawMeta {
    packages: Vec<RawPkg>,
    workspace_members: Vec<String>,
    #[serde(default)]
    workspace_root: String,
    resolve: Option<RawResolve>,
}

/// Extracts the version from an exact `=x.y.z` Cargo requirement. Cargo uses a single `=`; the
/// default bare `"1.2.3"` is `^1.2.3`, a range, not a pin.
fn exact_req_version(req: &str) -> Option<String> {
    let req = req.trim();
    req.strip_prefix('=')
        .filter(|version| !version.starts_with('='))
        .map(str::trim)
        .filter(|version| semver::Version::parse(version).is_ok())
        .map(str::to_string)
}

#[derive(Clone)]
struct UpperBound {
    version: semver::Version,
    inclusive: bool,
}

struct DeclaredBoundEdge {
    requirer: String,
    dependency_name: String,
    package_name: String,
    requirement: String,
    upper: UpperBound,
}

struct ActiveEdge {
    dependency_name: String,
    package_id: String,
}

type PackageEdges = HashMap<String, Vec<String>>;
type NamedPackageEdges = HashMap<String, Vec<ActiveEdge>>;

/// One declared `=x.y.z` requirement, as a *candidate* ceiling edge: the requirer's package id and
/// the dependency name and exact version it pins. Resolved against the activated graph before it
/// caps anything — a pin behind a disabled feature or non-matching `target` is not a real edge.
struct ExactEdge {
    requirer: String,
    dependency: String,
    version: String,
}

/// One lower-bound requirement, as a *candidate* floor edge: the requirer's package id and the
/// dependency name and floor version its requirement demands. Resolved against the activated
/// graph before it floors anything, for the same activation reasons as [`ExactEdge`].
struct FloorEdge {
    requirer: String,
    dependency: String,
    floor: String,
}

/// The activated `=`-pin ceilings [`resolved_graph_ceilings`] distills from the candidates: which
/// `(name, version)` nodes are capped, and by which requirer package ids.
struct ResolvedCeilings {
    ceilings: HashSet<(String, String)>,
    requirers: HashMap<(String, String), Vec<String>>,
}

/// The resolved dependency edges per package id, in the two projections the graph builder needs.
struct ResolvedEdges {
    /// Each package id's resolved dependency package ids.
    by_package: PackageEdges,
    /// The same edges with the resolve graph's dependency names attached.
    named: NamedPackageEdges,
}

fn resolved_edges(resolve: Option<RawResolve>) -> ResolvedEdges {
    let mut package_edges = HashMap::new();
    let mut named_edges = HashMap::new();
    let Some(resolve) = resolve else {
        return ResolvedEdges {
            by_package: package_edges,
            named: named_edges,
        };
    };
    for node in resolve.nodes {
        let mut package_ids = Vec::with_capacity(node.deps.len());
        let mut named = Vec::with_capacity(node.deps.len());
        for dependency in node.deps {
            package_ids.push(dependency.pkg.clone());
            named.push(ActiveEdge {
                dependency_name: dependency.name,
                package_id: dependency.pkg,
            });
        }
        package_edges.insert(node.id.clone(), package_ids);
        named_edges.insert(node.id, named);
    }
    ResolvedEdges {
        by_package: package_edges,
        named: named_edges,
    }
}

fn explicit_upper_bound(req: &str) -> Option<UpperBound> {
    let parsed = semver::VersionReq::parse(req).ok()?;
    parsed
        .comparators
        .iter()
        .filter_map(|comparator| {
            let inclusive = match comparator.op {
                semver::Op::Less => false,
                semver::Op::LessEq => true,
                _ => return None,
            };
            let mut version = semver::Version::new(
                comparator.major,
                comparator.minor.unwrap_or(0),
                comparator.patch.unwrap_or(0),
            );
            version.pre = comparator.pre.clone();
            Some(UpperBound { version, inclusive })
        })
        .min_by(|a, b| {
            a.version
                .cmp(&b.version)
                .then_with(|| a.inclusive.cmp(&b.inclusive))
        })
}

fn upper_bound_is_stricter(candidate: &UpperBound, current: &UpperBound) -> bool {
    candidate.version < current.version
        || (candidate.version == current.version && !candidate.inclusive && current.inclusive)
}

fn resolved_declared_bounds(
    candidates: Vec<DeclaredBoundEdge>,
    active_edges: &HashMap<String, Vec<ActiveEdge>>,
    packages: &HashMap<String, PkgInfo>,
) -> HashMap<(String, String), String> {
    let mut picks: HashMap<(String, String), (UpperBound, String)> = HashMap::new();
    for candidate in candidates {
        let Some(edges) = active_edges.get(&candidate.requirer) else {
            continue;
        };
        for edge in edges {
            // Cargo normalizes hyphens to underscores in dependency names used by the resolve graph.
            // An empty name supports reduced metadata fixtures that omit this disambiguation.
            let names_match = edge.dependency_name.is_empty()
                || edge.dependency_name.replace('-', "_")
                    == candidate.dependency_name.replace('-', "_");
            if !names_match {
                continue;
            }
            let Some(info) = packages.get(&edge.package_id) else {
                continue;
            };
            if info.name != candidate.package_name
                || !crate::version::version_in_range(&candidate.requirement, &info.version)
            {
                continue;
            }
            let key = (info.name.clone(), info.version.clone());
            picks
                .entry(key)
                .and_modify(|current| {
                    if upper_bound_is_stricter(&candidate.upper, &current.0) {
                        current
                            .clone_from(&(candidate.upper.clone(), candidate.requirement.clone()));
                    }
                })
                .or_insert_with(|| (candidate.upper.clone(), candidate.requirement.clone()));
        }
    }
    picks
        .into_iter()
        .map(|(key, (_, requirement))| (key, requirement))
        .collect()
}

/// Walks each active requirer edge to the depended node of the candidate's name and records the
/// highest lower bound demanded of it, per resolved `(name, version)` node.
fn resolved_graph_floors(
    floor_edges: Vec<FloorEdge>,
    edges: &HashMap<String, Vec<String>>,
    packages: &HashMap<String, PkgInfo>,
) -> HashMap<(String, String), String> {
    let mut graph_floors: HashMap<(String, String), String> = HashMap::new();
    for candidate in floor_edges {
        let Some(dep_ids) = edges.get(&candidate.requirer) else {
            continue;
        };
        for id in dep_ids {
            let Some(info) = packages.get(id) else {
                continue;
            };
            if info.name != candidate.dependency {
                continue;
            }
            let key = (info.name.clone(), info.version.clone());
            graph_floors
                .entry(key)
                .and_modify(|current| {
                    if crate::version::compare(&candidate.floor, current).is_gt() {
                        current.clone_from(&candidate.floor);
                    }
                })
                .or_insert_with(|| candidate.floor.clone());
        }
    }
    graph_floors
}

fn resolved_graph_ceilings(
    candidates: Vec<ExactEdge>,
    edges: &HashMap<String, Vec<String>>,
    packages: &HashMap<String, PkgInfo>,
) -> ResolvedCeilings {
    let mut ceilings = HashSet::new();
    let mut requirers: HashMap<(String, String), Vec<String>> = HashMap::new();
    for candidate in candidates {
        let active = edges.get(&candidate.requirer).is_some_and(|dep_ids| {
            dep_ids.iter().any(|id| {
                packages.get(id).is_some_and(|info| {
                    info.name == candidate.dependency && info.version == candidate.version
                })
            })
        });
        if !active {
            continue;
        }
        let key = (candidate.dependency, candidate.version);
        ceilings.insert(key.clone());
        if let Some(requirer_name) = packages
            .get(&candidate.requirer)
            .map(|info| info.name.clone())
        {
            requirers.entry(key).or_default().push(requirer_name);
        }
    }
    ResolvedCeilings {
        ceilings,
        requirers,
    }
}

/// The lowest concrete version a Cargo requirement admits — the floor its lower-bound comparators
/// demand — as a `major.minor.patch` string, or `None` when the requirement names no floor we can
/// safely assert. `^1.0`/`~1.2`/`>=1.2.3`/`=1.2.3`/`1.*` all floor at the stated version with missing
/// components zeroed (`^1.0` → `1.0.0`); within a multi-comparator range the tightest (highest) lower
/// bound wins. These contribute nothing: an upper bound (`<`/`<=`); a strict `>` (whose real floor is
/// the *next* release, which the requirement alone does not name); and a prerelease-qualified bound
/// (its true floor sits below its stable base, and a too-high floor could exceed the version a node
/// actually resolved to). Omitting an unnamable bound only makes a node look *more* reducible, which
/// the apply-time resolve re-checks, so erring low is the safe direction.
fn req_floor(req: &str) -> Option<String> {
    let parsed = semver::VersionReq::parse(req).ok()?;
    let mut best: Option<(u64, u64, u64)> = None;
    for comparator in &parsed.comparators {
        // `>` excludes the stated version (its real floor is the next release, unnamable from the
        // requirement alone) and a prerelease bound floors below its stable base — neither yields a
        // floor we can assert without risking floor > resolved version, so skip them.
        let imposes_lower_bound = matches!(
            comparator.op,
            semver::Op::Exact
                | semver::Op::GreaterEq
                | semver::Op::Tilde
                | semver::Op::Caret
                | semver::Op::Wildcard
        );
        if !imposes_lower_bound || !comparator.pre.is_empty() {
            continue;
        }
        let candidate = (
            comparator.major,
            comparator.minor.unwrap_or(0),
            comparator.patch.unwrap_or(0),
        );
        best = Some(best.map_or(candidate, |current| current.max(candidate)));
    }
    best.map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
}

/// The crate's directory relative to the workspace root (`.` for a crate at the root). Cargo reports
/// absolute manifest paths; relativizing keeps member paths short and workspace-portable.
fn member_path(manifest_path: &str, workspace_root: &str) -> String {
    if manifest_path.is_empty() || workspace_root.is_empty() {
        return ".".to_string();
    }
    let dir = Utf8Path::new(manifest_path)
        .parent()
        .unwrap_or_else(|| Utf8Path::new(""));
    let root = Utf8Path::new(workspace_root);
    match dir.strip_prefix(root) {
        Ok(rel) if !rel.as_str().is_empty() => rel.to_string(),
        _ => ".".to_string(),
    }
}
#[derive(serde::Deserialize)]
struct RawPkg {
    id: String,
    name: String,
    version: String,
    #[serde(default)]
    source: Option<String>,
    /// Absolute path to the crate's `Cargo.toml`; relativized to the workspace root for the member
    /// path. Defaults to empty when absent (older cargo), yielding a `.` path.
    #[serde(default)]
    manifest_path: String,
    /// The crate's declared dependencies (with their version requirements), used to detect exact
    /// `=x.y.z` pins on workspace-member crates.
    #[serde(default)]
    dependencies: Vec<RawDep>,
    /// The crate's declared `rust-version` (MSRV), absent when it declares none.
    #[serde(default)]
    rust_version: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawDep {
    name: String,
    /// The dependency's manifest key when it renames `name`.
    #[serde(default)]
    rename: Option<String>,
    /// The semver requirement string, e.g. `^1.0.197` (default caret) or `=1.0.197` (exact pin).
    req: String,
    /// The dependency kind: absent/`null` for a normal dep, `"dev"`, or `"build"`. A transitive
    /// crate's dev-dependencies are not resolved into the build graph, so a dev `=` pin caps nothing
    /// and is excluded from the ceiling; normal and build dependencies are.
    #[serde(default)]
    kind: Option<String>,
}
#[derive(serde::Deserialize)]
struct RawResolve {
    #[serde(default)]
    nodes: Vec<RawNode>,
}
#[derive(serde::Deserialize)]
struct RawNode {
    id: String,
    #[serde(default)]
    deps: Vec<RawNodeDep>,
}
#[derive(serde::Deserialize)]
struct RawNodeDep {
    /// The dependency name used by this edge, including a manifest rename.
    #[serde(default)]
    name: String,
    pkg: String,
}

/// A thin wrapper around the `cargo` executable used for resolution and apply.
///
/// The binary defaults to `cargo` but can be overridden via the `COOLDOWN_CARGO`
/// environment variable (resolved once in [`Cargo::default`]).
#[derive(Clone)]
pub struct Cargo {
    bin: String,
}

impl Default for Cargo {
    fn default() -> Self {
        Cargo {
            bin: std::env::var("COOLDOWN_CARGO").unwrap_or_else(|_| "cargo".to_string()),
        }
    }
}

impl Cargo {
    /// Creates a `Cargo` wrapper, honoring the `COOLDOWN_CARGO` binary override.
    ///
    /// Equivalent to [`Cargo::default`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    async fn output(
        &self,
        dir: &Utf8Path,
        args: &[&str],
    ) -> Result<std::process::Output, CoreError> {
        tracing::debug!(bin = self.bin, args = ?args, dir = %dir, "spawn cargo");
        let started = std::time::Instant::now();
        let result = Command::new(resolve_program(&self.bin))
            .args(args)
            .current_dir(dir.as_std_path())
            .output()
            .await
            .map_err(|e| CoreError::ToolSpawn {
                tool: self.bin.clone(),
                detail: format!("`{} {}`: {e}", self.bin, args.join(" ")),
            });
        tracing::debug!(
            bin = self.bin,
            args = ?args,
            elapsed_ms = started.elapsed().as_millis(),
            ok = result.is_ok(),
            "cargo finished"
        );
        result
    }

    async fn run(&self, dir: &Utf8Path, args: &[&str]) -> Result<String, CoreError> {
        let out = self.output(dir, args).await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(CoreError::Tool {
                tool: self.bin.clone(),
                termination: ToolTermination::from_exit_status(out.status),
                stderr: failure_detail(&out),
            })
        }
    }

    /// Resolves the dependency graph for `dir` via `cargo metadata --format-version 1`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `cargo` cannot be spawned,
    /// [`CoreError::Tool`] if it exits non-zero, and [`CoreError::LockUnreadable`] if its JSON
    /// output cannot be parsed.
    pub async fn metadata(&self, dir: &Utf8Path) -> Result<ResolvedGraph, CoreError> {
        let stdout = self
            .run(dir, &["metadata", "--format-version", "1"])
            .await?;
        let raw: RawMeta = serde_json::from_str(&stdout)
            .map_err(|e| CoreError::LockUnreadable(format!("cargo metadata: {e}")))?;
        Ok(Self::build_graph(raw))
    }

    /// Builds a [`ResolvedGraph`] from raw `cargo metadata` JSON, for tests that exercise the graph
    /// logic (exact-pin ceilings, requirer blame) without spawning `cargo`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn build_graph_from_json(json: &str) -> ResolvedGraph {
        let raw: RawMeta = serde_json::from_str(json).expect("parse metadata");
        Self::build_graph(raw)
    }

    /// Builds the [`ResolvedGraph`] from parsed `cargo metadata`. Split from [`Self::metadata`] so the
    /// graph logic — exact pins, the active-edge ceiling intersection, reverse edges — is unit-testable
    /// without spawning cargo.
    fn build_graph(raw: RawMeta) -> ResolvedGraph {
        let workspace_root = raw.workspace_root.clone();
        let roots: HashSet<String> = raw.workspace_members.iter().cloned().collect();
        let mut packages = HashMap::new();
        let mut exact_pins = HashSet::new();
        // Every non-dev `=x.y.z` requirement, as a candidate list — not the final ceiling set.
        let mut exact_edges: Vec<ExactEdge> = Vec::new();
        // Every non-root, non-dev requirement with a parseable lower bound. Like `exact_edges`, a
        // candidate list resolved against the activated graph below: a requirement behind a
        // disabled feature or non-matching `target` is not a real edge and demands no floor. Root
        // requirements are intentionally excluded: they are direct project constraints cooldown
        // may rewrite, not structural third-party graph floors.
        let mut floor_edges: Vec<FloorEdge> = Vec::new();
        let mut declared_bound_edges: Vec<DeclaredBoundEdge> = Vec::new();
        let mut declared_requirements: HashMap<PackageKey, Vec<DeclaredRequirement>> =
            HashMap::new();
        let mut msrv = MsrvIndex::default();
        for p in raw.packages {
            msrv.record(&p, roots.contains(&p.id));
            for dep in &p.dependencies {
                // A dev dependency of a transitive crate is not in the resolved build graph and caps
                // nothing; normal and build dependencies do, once confirmed active below.
                let is_dev = dep.kind.as_deref() == Some("dev");
                if let Some(version) = exact_req_version(&dep.req) {
                    // A workspace member's own exact pin is the project's choice: it surfaces as
                    // `pinned` (held, but with an adoptable target showing what it could be repinned to).
                    if roots.contains(&p.id) {
                        exact_pins.insert((dep.name.clone(), version.clone()));
                    }
                    if !is_dev {
                        exact_edges.push(ExactEdge {
                            requirer: p.id.clone(),
                            dependency: dep.name.clone(),
                            version,
                        });
                    }
                }
                if !is_dev
                    && !roots.contains(&p.id)
                    && let Some(floor) = req_floor(&dep.req)
                {
                    floor_edges.push(FloorEdge {
                        requirer: p.id.clone(),
                        dependency: dep.name.clone(),
                        floor,
                    });
                }
                // A member's dev-dependency bound is as deliberate as its normal one, and dev deps
                // are resolved and upgradeable — the same reasoning that keeps dev pins in
                // `exact_pins` above.
                if roots.contains(&p.id)
                    && let Some(upper) = explicit_upper_bound(&dep.req)
                {
                    declared_bound_edges.push(DeclaredBoundEdge {
                        requirer: p.id.clone(),
                        dependency_name: dep.rename.clone().unwrap_or_else(|| dep.name.clone()),
                        package_name: dep.name.clone(),
                        requirement: dep.req.clone(),
                        upper,
                    });
                }
                // A lock edge of this package must satisfy this requirement, so record it for the
                // edge-policy check — except a non-workspace package's dev-dependency, which is
                // never resolved into the lock and so constrains no edge.
                if !is_dev || roots.contains(&p.id) {
                    declared_requirements
                        .entry(PackageKey::new(&*p.name, &*p.version))
                        .or_default()
                        .push(DeclaredRequirement {
                            dependency: dep.name.clone(),
                            requirement: dep.req.clone(),
                        });
                }
            }
            packages.insert(
                p.id.clone(),
                PkgInfo {
                    name: p.name,
                    version: p.version,
                    source: p.source,
                    path: member_path(&p.manifest_path, &workspace_root),
                },
            );
        }
        let ResolvedEdges {
            by_package: edges,
            named: active_edges,
        } = resolved_edges(raw.resolve);
        // A `=x.y.z` requirement caps a node only when its edge is actually in the resolved graph:
        // keep an exact pin only if the requirer resolves an edge to a node of that name and version.
        // An inactive (optional/target-gated) edge is declared but absent from `resolve.nodes`, so it
        // contributes no ceiling — the consumer would otherwise over-hold a freely upgradable crate.
        let ResolvedCeilings {
            ceilings: graph_ceilings,
            requirers: ceiling_requirers,
        } = resolved_graph_ceilings(exact_edges, &edges, &packages);
        // A non-root requirement floors a node only at the version its edge actually resolved to;
        // an inactive (optional/target-gated) edge is absent from `resolve.nodes`, so it
        // contributes no floor — mirroring the ceiling's active-edge intersection above.
        let graph_floors = resolved_graph_floors(floor_edges, &edges, &packages);
        let declared_bounds =
            resolved_declared_bounds(declared_bound_edges, &active_edges, &packages);
        ResolvedGraph {
            packages,
            roots,
            edges,
            exact_pins,
            graph_ceilings,
            ceiling_requirers,
            graph_floors,
            declared_bounds,
            declared_requirements,
            rust_versions: msrv.rust_versions,
            workspace_rust_version: msrv.workspace_rust_version,
        }
    }

    /// Returns whether `Cargo.lock` is current relative to `Cargo.toml`.
    ///
    /// Runs `cargo metadata --locked --offline`; a stale lock exits 101 and yields `Ok(false)`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `cargo` cannot be spawned, or [`CoreError::Tool`] if it
    /// fails for a reason other than a stale lock (e.g. a missing offline index).
    pub async fn verify_locked(&self, dir: &Utf8Path) -> Result<bool, CoreError> {
        let out = self
            .output(
                dir,
                &["metadata", "--locked", "--offline", "--format-version", "1"],
            )
            .await?;
        if out.status.success() {
            return Ok(true);
        }
        // `--locked` on a stale lock exits 101 with a clear message. A different failure (e.g.
        // missing offline index) is reported as a tool error.
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("--locked") || stderr.contains("lock file") {
            Ok(false)
        } else {
            Err(CoreError::Tool {
                tool: self.bin.clone(),
                termination: ToolTermination::from_exit_status(out.status),
                stderr: failure_detail(&out),
            })
        }
    }

    /// Pins the crates.io `name@from` node to exactly `to` via `cargo update -p <spec>
    /// --precise <to>`, with the spec source-qualified as [`CRATES_IO_SPEC_SOURCE`].
    ///
    /// The qualified spec cannot address a same-name package from another registry, and
    /// `@<from>` disambiguates a crate resolved at multiple versions. One spec per invocation,
    /// never several: cargo accepts multiple `-p` specs alongside `--precise` but silently
    /// applies the pin to only the first spec and exits 0 (observed on cargo 1.96), so batching
    /// crates that share a target version into one call loses every pin but the first.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `cargo` cannot be spawned, or [`CoreError::Tool`] if the
    /// update is rejected (e.g. a `=`-pin or resolver conflict blocks the precise move, or `from`
    /// no longer names a locked node). A rejection is the caller's signal that the candidate stays
    /// where the resolver placed it.
    pub(crate) async fn update_precise_crates_io(
        &self,
        dir: &Utf8Path,
        name: &str,
        from: &str,
        to: &str,
    ) -> Result<(), CoreError> {
        let spec = format!("{CRATES_IO_SPEC_SOURCE}#{name}@{from}");
        self.run(dir, &["update", "-p", &spec, "--precise", to])
            .await
            .map(|_| ())
    }

    /// Runs `cargo build` as the opt-in compile verification, reporting success in the [`VerifyReport`].
    ///
    /// A failed build is **not** an error: it is surfaced as `VerifyReport { ok: false, .. }` with
    /// the compiler's failure detail in `detail`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] only if the `cargo` process itself cannot be spawned.
    pub async fn build(&self, dir: &Utf8Path) -> Result<VerifyReport, CoreError> {
        let out = self.output(dir, &["build"]).await?;
        Ok(VerifyReport {
            ok: out.status.success(),
            detail: if out.status.success() {
                "cargo build succeeded".into()
            } else {
                failure_detail(&out)
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_path_relativizes_workspace_members() {
        assert_eq!(
            member_path("/repo/crates/app/Cargo.toml", "/repo"),
            "crates/app"
        );
        assert_eq!(member_path("/repo/Cargo.toml", "/repo"), ".");
    }

    #[test]
    fn member_path_defaults_to_root_when_metadata_is_missing() {
        assert_eq!(member_path("", "/repo"), ".");
        assert_eq!(member_path("/repo/crates/app/Cargo.toml", ""), ".");
    }

    #[test]
    fn parse_rust_version_pads_missing_components_and_rejects_junk() {
        assert_eq!(parse_rust_version("1.70"), Some(RustVersion::new(1, 70, 0)));
        assert_eq!(
            parse_rust_version("1.70.3"),
            Some(RustVersion::new(1, 70, 3))
        );
        assert_eq!(
            parse_rust_version(" 1.70 "),
            Some(RustVersion::new(1, 70, 0))
        );
        assert_eq!(parse_rust_version("1"), Some(RustVersion::new(1, 0, 0)));
        assert_eq!(parse_rust_version("^1.70"), None);
        assert_eq!(parse_rust_version("edition2021"), None);
    }

    #[test]
    fn build_graph_collects_msrv_data_for_the_edge_policy() {
        // The workspace MSRV is the lowest member declaration; per-package `rust-version`s are
        // keyed by (name, version) for the canonicalize candidate filter.
        let json = r#"{
            "packages": [
                {"id": "root-a", "name": "app-a", "version": "0.1.0", "rust_version": "1.75",
                 "dependencies": []},
                {"id": "root-b", "name": "app-b", "version": "0.1.0", "rust_version": "1.70.1",
                 "dependencies": []},
                {"id": "uuid", "name": "uuid", "version": "1.24.0", "rust_version": "1.63",
                 "dependencies": []},
                {"id": "old", "name": "uuid", "version": "0.8.2", "dependencies": []}
            ],
            "workspace_members": ["root-a", "root-b"],
            "workspace_root": "",
            "resolve": {"nodes": []}
        }"#;
        let graph = Cargo::build_graph_from_json(json);
        assert_eq!(
            graph.workspace_rust_version,
            Some(RustVersion::new(1, 70, 1))
        );
        assert_eq!(
            graph.rust_versions.get(&PackageKey::new("uuid", "1.24.0")),
            Some(&RustVersion::new(1, 63, 0))
        );
        assert!(
            !graph
                .rust_versions
                .contains_key(&PackageKey::new("uuid", "0.8.2")),
            "a package without a declared rust-version contributes nothing"
        );
    }

    #[test]
    fn exact_req_version_accepts_only_single_equals_pins() {
        assert_eq!(exact_req_version("=1.0.197").as_deref(), Some("1.0.197"));
        assert_eq!(exact_req_version(" = 1.0.197 ").as_deref(), Some("1.0.197"));
        assert_eq!(exact_req_version("^1.0.197"), None);
        assert_eq!(exact_req_version("1.0.197"), None);
        assert_eq!(exact_req_version("==1.0.197"), None);
        assert_eq!(exact_req_version("=1"), None);
        assert_eq!(exact_req_version("=1.0.197, <2.0.0"), None);
    }

    #[test]
    fn explicit_upper_bound_requires_a_written_less_than_comparator() {
        assert!(explicit_upper_bound(">=1, <2").is_some());
        assert!(explicit_upper_bound("^1").is_none());
        assert!(explicit_upper_bound("=1.2.3").is_none());
    }

    #[test]
    fn declared_bounds_choose_the_strictest_active_workspace_requirement() {
        let packages = HashMap::from([(
            "serde-id".to_string(),
            PkgInfo {
                name: "serde".to_string(),
                version: "1.0.228".to_string(),
                source: Some(CRATES_IO_SOURCE.to_string()),
                path: String::new(),
            },
        )]);
        let active_edges = HashMap::from([
            (
                "root-a".to_string(),
                vec![ActiveEdge {
                    dependency_name: "serde".to_string(),
                    package_id: "serde-id".to_string(),
                }],
            ),
            (
                "root-b".to_string(),
                vec![ActiveEdge {
                    dependency_name: "serde".to_string(),
                    package_id: "serde-id".to_string(),
                }],
            ),
        ]);
        let candidates = [
            ("root-a", "serde", ">=1, <3"),
            ("root-b", "serde", ">=1, <2"),
            ("inactive-root", "serde", "<1.5"),
        ]
        .into_iter()
        .map(|(root, name, requirement)| DeclaredBoundEdge {
            requirer: root.to_string(),
            dependency_name: name.to_string(),
            package_name: name.to_string(),
            requirement: requirement.to_string(),
            upper: explicit_upper_bound(requirement).expect("upper bound"),
        })
        .collect();

        let bounds = resolved_declared_bounds(candidates, &active_edges, &packages);

        assert_eq!(
            bounds
                .get(&("serde".to_string(), "1.0.228".to_string()))
                .map(String::as_str),
            Some(">=1, <2")
        );
    }

    #[test]
    fn declared_bounds_follow_renamed_edges_to_their_resolved_major() {
        let packages = HashMap::from([
            (
                "foo-v1".to_string(),
                PkgInfo {
                    name: "foo".to_string(),
                    version: "1.9.0".to_string(),
                    source: Some(CRATES_IO_SOURCE.to_string()),
                    path: String::new(),
                },
            ),
            (
                "foo-v2".to_string(),
                PkgInfo {
                    name: "foo".to_string(),
                    version: "2.4.0".to_string(),
                    source: Some(CRATES_IO_SOURCE.to_string()),
                    path: String::new(),
                },
            ),
        ]);
        let active_edges = HashMap::from([(
            "root".to_string(),
            vec![
                ActiveEdge {
                    dependency_name: "foo-v1".to_string(),
                    package_id: "foo-v1".to_string(),
                },
                ActiveEdge {
                    dependency_name: "foo-v2".to_string(),
                    package_id: "foo-v2".to_string(),
                },
            ],
        )]);
        let candidates = [("foo-v1", ">=1, <2"), ("foo-v2", ">=2, <3")]
            .into_iter()
            .map(|(dependency_name, requirement)| DeclaredBoundEdge {
                requirer: "root".to_string(),
                dependency_name: dependency_name.to_string(),
                package_name: "foo".to_string(),
                requirement: requirement.to_string(),
                upper: explicit_upper_bound(requirement).expect("upper bound"),
            })
            .collect();

        let bounds = resolved_declared_bounds(candidates, &active_edges, &packages);

        assert_eq!(
            bounds
                .get(&("foo".to_string(), "1.9.0".to_string()))
                .map(String::as_str),
            Some(">=1, <2")
        );
        assert_eq!(
            bounds
                .get(&("foo".to_string(), "2.4.0".to_string()))
                .map(String::as_str),
            Some(">=2, <3")
        );
    }

    #[test]
    fn declared_bounds_include_a_workspace_member_dev_dependency() {
        // A member's dev-dependency bound is as deliberate as a normal one (and dev deps are
        // resolved and upgradeable), so `criterion = ">=0.5, <0.6"` must hold like the normal
        // `serde` bound — the same treatment `exact_pins` gives a dev `=` pin.
        let json = r#"{
            "packages": [
                {"id": "root", "name": "root", "version": "0.1.0",
                 "dependencies": [
                    {"name": "serde", "req": ">=1, <2"},
                    {"name": "criterion", "req": ">=0.5, <0.6", "kind": "dev"}
                 ]},
                {"id": "serde", "name": "serde", "version": "1.0.228", "dependencies": []},
                {"id": "criterion", "name": "criterion", "version": "0.5.1", "dependencies": []}
            ],
            "workspace_members": ["root"],
            "workspace_root": "",
            "resolve": {"nodes": [
                {"id": "root", "deps": [{"pkg": "serde"}, {"pkg": "criterion"}]},
                {"id": "serde", "deps": []},
                {"id": "criterion", "deps": []}
            ]}
        }"#;
        let graph = Cargo::build_graph_from_json(json);
        assert_eq!(graph.declared_bound("serde", "1.0.228"), Some(">=1, <2"));
        assert_eq!(
            graph.declared_bound("criterion", "0.5.1"),
            Some(">=0.5, <0.6")
        );
    }

    #[test]
    fn req_floor_extracts_the_lower_bound_per_operator() {
        // Caret/tilde/exact/`>=`/wildcard all floor at the stated version, missing components zeroed.
        assert_eq!(req_floor("^1.0").as_deref(), Some("1.0.0"));
        assert_eq!(req_floor("^1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(req_floor("1").as_deref(), Some("1.0.0")); // bare == caret
        assert_eq!(req_floor("~1.2").as_deref(), Some("1.2.0"));
        assert_eq!(req_floor(">=1.5.0").as_deref(), Some("1.5.0"));
        assert_eq!(req_floor("=1.0.197").as_deref(), Some("1.0.197"));
        assert_eq!(req_floor("1.*").as_deref(), Some("1.0.0"));
        // A multi-comparator range takes the tightest (highest) lower bound; an upper bound alone
        // imposes none.
        assert_eq!(req_floor(">=1.2.0, <2.0.0").as_deref(), Some("1.2.0"));
        assert_eq!(req_floor("<2.0.0"), None);
        assert_eq!(req_floor("not a req"), None);
        // A strict `>` excludes the stated version (its real floor is the next, unnamable release), and
        // a prerelease bound floors below its stable base — both name no safe floor.
        assert_eq!(req_floor(">1.2.3"), None);
        assert_eq!(req_floor(">=1.2.3-rc1"), None);
        assert_eq!(req_floor("^1.2.3-beta.1"), None);
        // A `>` paired with an inclusive lower bound still honors the inclusive one.
        assert_eq!(req_floor(">1.0.0, >=1.5.0").as_deref(), Some("1.5.0"));
    }

    #[test]
    fn graph_floor_records_the_demanded_minimum_below_the_resolved_version() {
        // `quote` resolves to the latest 1.0.46, but every requirer only asks `^1.0` — so the floor is
        // 1.0.0 and the node is freely reducible down to any matured 1.0.x.
        let json = r#"{
            "packages": [
                {"id": "root", "name": "root", "version": "0.1.0",
                 "dependencies": [{"name": "syn", "req": "^2.0"}]},
                {"id": "syn", "name": "syn", "version": "2.0.50",
                 "dependencies": [{"name": "quote", "req": "^1.0"}]},
                {"id": "quote", "name": "quote", "version": "1.0.46", "dependencies": []}
            ],
            "workspace_members": ["root"],
            "workspace_root": "",
            "resolve": {"nodes": [
                {"id": "root", "deps": [{"pkg": "syn"}]},
                {"id": "syn", "deps": [{"pkg": "quote"}]},
                {"id": "quote", "deps": []}
            ]}
        }"#;
        let graph = Cargo::build_graph_from_json(json);
        assert_eq!(graph.graph_floor("quote", "1.0.46"), Some("1.0.0"));
        // The workspace root's own `syn` requirement is project-owned and editable, not a structural
        // graph floor.
        assert_eq!(graph.graph_floor("syn", "2.0.50"), None);
        // A node no edge floors has none.
        assert_eq!(graph.graph_floor("quote", "9.9.9"), None);
    }

    #[test]
    fn graph_floor_ignores_workspace_member_requirements() {
        // Root lower bounds and exact pins are project-owned constraints: direct deps can be
        // rewritten by cooldown, so they must not become immutable graph floors that make
        // `fix --downgrade-pinned` impossible.
        let json = r#"{
            "packages": [
                {"id": "root", "name": "root", "version": "0.1.0",
                 "dependencies": [
                    {"name": "serde", "req": "=1.0.228"},
                    {"name": "syn", "req": "^2.0"}
                 ]},
                {"id": "serde", "name": "serde", "version": "1.0.228", "dependencies": []},
                {"id": "syn", "name": "syn", "version": "2.0.50", "dependencies": []}
            ],
            "workspace_members": ["root"],
            "workspace_root": "",
            "resolve": {"nodes": [
                {"id": "root", "deps": [{"pkg": "serde"}, {"pkg": "syn"}]},
                {"id": "serde", "deps": []},
                {"id": "syn", "deps": []}
            ]}
        }"#;
        let graph = Cargo::build_graph_from_json(json);
        assert_eq!(graph.graph_floor("serde", "1.0.228"), None);
        assert_eq!(graph.graph_floor("syn", "2.0.50"), None);
        assert!(graph.is_exact_pinned("serde", "1.0.228"));
    }

    #[test]
    fn graph_floor_takes_the_tightest_requirer() {
        // Two requirers floor `quote`: `^1.0` and `^1.0.40`. The graph must hold the highest (1.0.40).
        let json = r#"{
            "packages": [
                {"id": "root", "name": "root", "version": "0.1.0",
                 "dependencies": [{"name": "syn", "req": "^2.0"}, {"name": "newer", "req": "^1.0"}]},
                {"id": "syn", "name": "syn", "version": "2.0.50",
                 "dependencies": [{"name": "quote", "req": "^1.0"}]},
                {"id": "newer", "name": "newer", "version": "1.0.0",
                 "dependencies": [{"name": "quote", "req": "^1.0.40"}]},
                {"id": "quote", "name": "quote", "version": "1.0.46", "dependencies": []}
            ],
            "workspace_members": ["root"],
            "workspace_root": "",
            "resolve": {"nodes": [
                {"id": "root", "deps": [{"pkg": "syn"}, {"pkg": "newer"}]},
                {"id": "syn", "deps": [{"pkg": "quote"}]},
                {"id": "newer", "deps": [{"pkg": "quote"}]},
                {"id": "quote", "deps": []}
            ]}
        }"#;
        let graph = Cargo::build_graph_from_json(json);
        assert_eq!(graph.graph_floor("quote", "1.0.46"), Some("1.0.40"));
    }

    #[test]
    fn graph_floor_ignores_inactive_requirer_edges() {
        // `ghost` declares `quote ^1.5` but resolves no edge to it (an inactive optional/target dep),
        // so it must not raise the floor; only the active `^1.0` from `syn` counts.
        let json = r#"{
            "packages": [
                {"id": "root", "name": "root", "version": "0.1.0",
                 "dependencies": [{"name": "syn", "req": "^2.0"}, {"name": "ghost", "req": "^1.0"}]},
                {"id": "syn", "name": "syn", "version": "2.0.50",
                 "dependencies": [{"name": "quote", "req": "^1.0"}]},
                {"id": "ghost", "name": "ghost", "version": "0.1.0",
                 "dependencies": [{"name": "quote", "req": "^1.5"}]},
                {"id": "quote", "name": "quote", "version": "1.0.46", "dependencies": []}
            ],
            "workspace_members": ["root"],
            "workspace_root": "",
            "resolve": {"nodes": [
                {"id": "root", "deps": [{"pkg": "syn"}, {"pkg": "ghost"}]},
                {"id": "syn", "deps": [{"pkg": "quote"}]},
                {"id": "ghost", "deps": []},
                {"id": "quote", "deps": []}
            ]}
        }"#;
        let graph = Cargo::build_graph_from_json(json);
        assert_eq!(graph.graph_floor("quote", "1.0.46"), Some("1.0.0"));
    }

    #[test]
    fn exact_pin_is_version_specific() {
        let graph = ResolvedGraph {
            packages: HashMap::new(),
            roots: HashSet::new(),
            edges: HashMap::new(),
            exact_pins: HashSet::from([("serde".to_string(), "1.0.197".to_string())]),
            graph_ceilings: HashSet::new(),
            ceiling_requirers: HashMap::new(),
            graph_floors: HashMap::new(),
            declared_bounds: HashMap::new(),
            declared_requirements: HashMap::new(),
            rust_versions: HashMap::new(),
            workspace_rust_version: None,
        };

        assert!(graph.is_exact_pinned("serde", "1.0.197"));
        assert!(!graph.is_exact_pinned("serde", "0.9.0"));
    }

    #[test]
    fn graph_cap_is_version_specific() {
        // serde_derive is capped at 1.0.228 by some requirer's `=1.0.228`; a coexisting 1.0.300 node
        // pulled by a caret requirer is not capped — the ceiling is keyed per (name, version) node.
        let graph = ResolvedGraph {
            packages: HashMap::new(),
            roots: HashSet::new(),
            edges: HashMap::new(),
            exact_pins: HashSet::new(),
            graph_ceilings: HashSet::from([("serde_derive".to_string(), "1.0.228".to_string())]),
            ceiling_requirers: HashMap::new(),
            graph_floors: HashMap::new(),
            declared_bounds: HashMap::new(),
            declared_requirements: HashMap::new(),
            rust_versions: HashMap::new(),
            workspace_rust_version: None,
        };

        assert!(graph.is_graph_capped("serde_derive", "1.0.228"));
        assert!(!graph.is_graph_capped("serde_derive", "1.0.300"));
        assert!(!graph.is_graph_capped("serde", "1.0.228"));
    }

    #[test]
    fn graph_ceiling_ignores_inactive_pin_edges() {
        // `live` pins `dep =1.0.0` and resolves an edge to it → a real ceiling. `ghost` declares
        // `other =2.0.0` but its edge is absent from `resolve.nodes` (an inactive optional/target
        // dep); `other` resolves to 2.0.0 only via `open`'s caret range, so it is NOT capped.
        let json = r#"{
            "packages": [
                {"id": "live", "name": "live", "version": "1.0.0",
                 "dependencies": [{"name": "dep", "req": "=1.0.0"}]},
                {"id": "ghost", "name": "ghost", "version": "0.1.0",
                 "dependencies": [{"name": "other", "req": "=2.0.0", "kind": null}]},
                {"id": "open", "name": "open", "version": "1.0.0",
                 "dependencies": [{"name": "other", "req": "^2.0"}]},
                {"id": "dep", "name": "dep", "version": "1.0.0"},
                {"id": "other", "name": "other", "version": "2.0.0"}
            ],
            "workspace_members": ["root"],
            "workspace_root": "",
            "resolve": {"nodes": [
                {"id": "live", "deps": [{"pkg": "dep"}]},
                {"id": "open", "deps": [{"pkg": "other"}]},
                {"id": "ghost", "deps": []},
                {"id": "dep", "deps": []},
                {"id": "other", "deps": []}
            ]}
        }"#;
        let raw: RawMeta = serde_json::from_str(json).expect("parse metadata");
        let graph = Cargo::build_graph(raw);
        assert!(graph.is_graph_capped("dep", "1.0.0")); // active `=` edge → real ceiling
        assert!(!graph.is_graph_capped("other", "2.0.0")); // pinned only by an inactive edge
    }

    #[test]
    fn direct_members_returns_roots_that_declare_dependency() {
        let graph = ResolvedGraph {
            packages: HashMap::from([
                (
                    "root-a".to_string(),
                    PkgInfo {
                        name: "app-a".to_string(),
                        version: "0.1.0".to_string(),
                        source: None,
                        path: "apps/a".to_string(),
                    },
                ),
                (
                    "root-b".to_string(),
                    PkgInfo {
                        name: "app-b".to_string(),
                        version: "0.1.0".to_string(),
                        source: None,
                        path: "apps/b".to_string(),
                    },
                ),
                (
                    "dep".to_string(),
                    PkgInfo {
                        name: "serde".to_string(),
                        version: "1.0.0".to_string(),
                        source: Some(
                            "registry+https://github.com/rust-lang/crates.io-index".to_string(),
                        ),
                        path: ".".to_string(),
                    },
                ),
            ]),
            roots: HashSet::from(["root-a".to_string(), "root-b".to_string()]),
            edges: HashMap::from([
                ("root-a".to_string(), vec!["dep".to_string()]),
                ("root-b".to_string(), Vec::new()),
            ]),
            exact_pins: HashSet::new(),
            graph_ceilings: HashSet::new(),
            ceiling_requirers: HashMap::new(),
            graph_floors: HashMap::new(),
            declared_bounds: HashMap::new(),
            declared_requirements: HashMap::new(),
            rust_versions: HashMap::new(),
            workspace_rust_version: None,
        };

        assert_eq!(
            graph
                .direct_members("dep")
                .iter()
                .map(|member| (member.name.as_str(), member.path.as_str()))
                .collect::<Vec<_>>(),
            vec![("app-a", "apps/a")]
        );
    }

    #[test]
    fn direct_members_reach_checks_the_declaring_member_not_any_matching_crate() {
        let graph = Cargo::build_graph_from_json(
            r#"{
                "packages": [
                    {"id": "root-a", "name": "app-a", "version": "0.1.0",
                     "manifest_path": "/repo/apps/a/Cargo.toml"},
                    {"id": "root-b", "name": "app-b", "version": "0.1.0",
                     "manifest_path": "/repo/apps/b/Cargo.toml"},
                    {"id": "nix-old", "name": "nix", "version": "0.28.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index"},
                    {"id": "nix-new", "name": "nix", "version": "0.31.3",
                     "source": "registry+https://github.com/rust-lang/crates.io-index"}
                ],
                "workspace_members": ["root-a", "root-b"],
                "workspace_root": "/repo",
                "resolve": {"nodes": [
                    {"id": "root-a", "deps": [{"pkg": "nix-old"}]},
                    {"id": "root-b", "deps": [{"pkg": "nix-new"}]},
                    {"id": "nix-old", "deps": []},
                    {"id": "nix-new", "deps": []}
                ]}
            }"#,
        );

        assert!(!graph.direct_members_reach(
            &[MemberRef {
                name: "app-a".to_string(),
                path: "apps/a".to_string(),
            }],
            "nix",
            "0.28.0",
            "0.31.3",
        ));
        assert!(graph.direct_members_reach(
            &[MemberRef {
                name: "app-b".to_string(),
                path: "apps/b".to_string(),
            }],
            "nix",
            "0.28.0",
            "0.31.3",
        ));
        assert!(
            !graph.direct_members_reach(
                &[MemberRef {
                    name: "app-b".to_string(),
                    path: "apps/b".to_string(),
                }],
                "nix",
                "0.28.0",
                "0.31.2",
            ),
            "a Cargo precise target must not verify through an overshoot"
        );
    }

    #[test]
    fn direct_members_reach_ignores_a_sibling_major_under_the_same_member() {
        // `app` resolves two majors of `foo` at once (e.g. a normal `foo = "1"` beside a
        // target-gated `foo = "2"`). A bump of the 1.x slot to 1.5.0 has not landed; `app` still
        // holds foo 1.4.0, so the coexisting 2.1.0 edge must not be read as "reached".
        let graph = Cargo::build_graph_from_json(
            r#"{
                "packages": [
                    {"id": "root", "name": "app", "version": "0.1.0",
                     "manifest_path": "/repo/apps/app/Cargo.toml"},
                    {"id": "foo-1", "name": "foo", "version": "1.4.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index"},
                    {"id": "foo-2", "name": "foo", "version": "2.1.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index"}
                ],
                "workspace_members": ["root"],
                "workspace_root": "/repo",
                "resolve": {"nodes": [
                    {"id": "root", "deps": [{"pkg": "foo-1"}, {"pkg": "foo-2"}]},
                    {"id": "foo-1", "deps": []},
                    {"id": "foo-2", "deps": []}
                ]}
            }"#,
        );

        assert!(!graph.direct_members_reach(
            &[MemberRef {
                name: "app".to_string(),
                path: "apps/app".to_string(),
            }],
            "foo",
            "1.4.0",
            "1.5.0",
        ));
    }

    #[test]
    fn direct_members_reach_requires_a_cross_major_move_to_vacate_the_old_slot() {
        // `app` declares `foo` twice (e.g. `[dependencies] foo = "1"` beside `[dev-dependencies]
        // foo = "0.4"`), so an edge into the target major exists before the planned 0.4.8 -> 1.0.11
        // move is attempted. As long as the member still edges the 0.4 line, the move has not
        // happened — the pre-existing 1.x edge must not read as "reached".
        let graph = Cargo::build_graph_from_json(
            r#"{
                "packages": [
                    {"id": "root", "name": "app", "version": "0.1.0",
                     "manifest_path": "/repo/apps/app/Cargo.toml"},
                    {"id": "foo-old", "name": "foo", "version": "0.4.8",
                     "source": "registry+https://github.com/rust-lang/crates.io-index"},
                    {"id": "foo-new", "name": "foo", "version": "1.0.11",
                     "source": "registry+https://github.com/rust-lang/crates.io-index"}
                ],
                "workspace_members": ["root"],
                "workspace_root": "/repo",
                "resolve": {"nodes": [
                    {"id": "root", "deps": [{"pkg": "foo-old"}, {"pkg": "foo-new"}]},
                    {"id": "foo-old", "deps": []},
                    {"id": "foo-new", "deps": []}
                ]}
            }"#,
        );
        let member = [MemberRef {
            name: "app".to_string(),
            path: "apps/app".to_string(),
        }];

        assert!(
            !graph.direct_members_reach(&member, "foo", "0.4.8", "1.0.11"),
            "the surviving 0.4 edge means the planned line never moved"
        );

        // Once the old slot is vacated (both entries resolve in the target major), it is reached.
        let moved = Cargo::build_graph_from_json(
            r#"{
                "packages": [
                    {"id": "root", "name": "app", "version": "0.1.0",
                     "manifest_path": "/repo/apps/app/Cargo.toml"},
                    {"id": "foo-new", "name": "foo", "version": "1.0.11",
                     "source": "registry+https://github.com/rust-lang/crates.io-index"}
                ],
                "workspace_members": ["root"],
                "workspace_root": "/repo",
                "resolve": {"nodes": [
                    {"id": "root", "deps": [{"pkg": "foo-new"}]},
                    {"id": "foo-new", "deps": []}
                ]}
            }"#,
        );
        assert!(moved.direct_members_reach(&member, "foo", "0.4.8", "1.0.11"));
    }

    #[test]
    fn direct_members_reach_ignores_non_registry_same_name() {
        let graph = Cargo::build_graph_from_json(
            r#"{
                "packages": [
                    {"id": "root", "name": "app", "version": "0.1.0",
                     "manifest_path": "/repo/apps/app/Cargo.toml",
                     "dependencies": [{"name": "foo", "req": "^1.0"}]},
                    {"id": "foo-path", "name": "foo", "version": "1.5.0",
                     "manifest_path": "/repo/vendor/foo/Cargo.toml",
                     "dependencies": []}
                ],
                "workspace_members": ["root"],
                "workspace_root": "/repo",
                "resolve": {"nodes": [
                    {"id": "root", "deps": [{"pkg": "foo-path"}]},
                    {"id": "foo-path", "deps": []}
                ]}
            }"#,
        );

        assert!(!graph.direct_members_reach(
            &[MemberRef {
                name: "app".to_string(),
                path: "apps/app".to_string(),
            }],
            "foo",
            "1.4.0",
            "1.5.0",
        ));
    }

    #[test]
    fn reaching_members_attributes_a_transitive_dep_to_its_requirers() {
        // root-a → dep → trans : `trans` is transitive, reached only through `dep`, so it is
        // attributed to app-a (rendered "via app-a").
        let pkg = |name: &str, path: &str| PkgInfo {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
            path: path.to_string(),
        };
        let graph = ResolvedGraph {
            packages: HashMap::from([
                ("root-a".to_string(), pkg("app-a", "apps/a")),
                ("root-b".to_string(), pkg("app-b", "apps/b")),
                ("dep".to_string(), pkg("serde", ".")),
                ("trans".to_string(), pkg("syn", ".")),
            ]),
            roots: HashSet::from(["root-a".to_string(), "root-b".to_string()]),
            edges: HashMap::from([
                ("root-a".to_string(), vec!["dep".to_string()]),
                ("root-b".to_string(), Vec::new()),
                ("dep".to_string(), vec!["trans".to_string()]),
                ("trans".to_string(), Vec::new()),
            ]),
            exact_pins: HashSet::new(),
            graph_ceilings: HashSet::new(),
            ceiling_requirers: HashMap::new(),
            graph_floors: HashMap::new(),
            declared_bounds: HashMap::new(),
            declared_requirements: HashMap::new(),
            rust_versions: HashMap::new(),
            workspace_rust_version: None,
        };

        let names = |members: Vec<MemberRef>| {
            members
                .iter()
                .map(|member| member.name.clone())
                .collect::<Vec<_>>()
        };
        // Transitive: only app-a reaches `trans`.
        assert_eq!(names(graph.reaching_members("trans")), vec!["app-a"]);
        // Direct deps are reached too — reaching is a superset of direct.
        assert_eq!(names(graph.reaching_members("dep")), vec!["app-a"]);
    }
}
