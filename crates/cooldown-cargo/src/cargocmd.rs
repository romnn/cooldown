//! Thin wrappers around the project's own `cargo` binary (resolution/apply engine only).

use camino::Utf8Path;
use cooldown_adapter_util::resolve_program;
use cooldown_core::{
    CoreError, LockStatus, LockVerifyReport, MemberRef, ToolTermination, VerifyReport,
    failure_detail,
};
use std::collections::{HashMap, HashSet};
use tokio::process::Command;

pub(crate) use crate::lockfile::CRATES_IO_SOURCE;
pub use crate::lockfile::LockPackageId;
pub(crate) use crate::lockfile::PackageKey;

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
    /// The attributed constraint edges per resolved `(crate, version)` node: for each node, every
    /// active non-root requirer whose requirement contributes a floor on it and every requirer
    /// whose exact `=` pin caps it, with the requirer's own resolved name and version. The
    /// collapsed [`graph_floors`](Self::graph_floors)/[`graph_ceilings`](Self::graph_ceilings)
    /// stay authoritative for the gate; this attribution exists so `fix` planning can discount
    /// holds contributed by requirers that are themselves too-fresh violations (a circular hold)
    /// and name the compliant requirer behind a genuine one.
    pub hold_edges: HashMap<(String, String), Vec<cooldown_core::GraphHoldEdge>>,
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
    /// Every active version requirement each resolved package imposes on a concrete resolved edge.
    /// Keyed by [`LockPackageId`] so source-distinct packages never share requirements.
    pub declared_requirements: HashMap<LockPackageId, Vec<DeclaredRequirement>>,
    /// Each resolved package's declared `rust-version` (MSRV); absent when the package declares
    /// none (or it is unparsable, which cargo's MSRV-aware resolver likewise treats as
    /// compatible).
    pub rust_versions: HashMap<LockPackageId, RustVersion>,
    /// The lowest `rust-version` any workspace member declares, used as cooldown's conservative
    /// compatibility preference.
    /// Cargo uses heuristics for mixed-MSRV workspaces and may select a dependency above or below
    /// an individual member's needs.
    pub workspace_rust_version: Option<RustVersion>,
}

/// A declared `rust-version` (MSRV) as a release triple.
/// The derived ordering compares `major`, then `minor`, then `patch` — semver precedence, since the
/// manifest field forbids prereleases and build metadata.
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
/// [`RustVersion`], with missing components zeroed.
/// Returns `None` for anything else because the field forbids ranges and prereleases, so an
/// unparsable value is treated as undeclared.
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
    rust_versions: HashMap<LockPackageId, RustVersion>,
    workspace_rust_version: Option<RustVersion>,
}

impl MsrvIndex {
    fn record(&mut self, package: &RawPkg, is_member: bool) {
        let Some(msrv) = package.rust_version.as_deref().and_then(parse_rust_version) else {
            return;
        };
        self.rust_versions.insert(
            LockPackageId::from_metadata(
                &package.name,
                &package.version,
                package.source.as_deref(),
            ),
            msrv,
        );
        // Use the lowest member declaration as cooldown's conservative compatibility threshold;
        // Cargo's mixed-MSRV workspace selection remains heuristic.
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
    /// The concrete package node this declaration resolves to.
    pub resolved: LockPackageId,
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

    /// The attributed floor and ceiling edges constraining the `crate_name`@`version` node, or an
    /// empty slice when no active non-root requirer constrains it. See
    /// [`hold_edges`](Self::hold_edges) for what the attribution is for.
    #[must_use]
    pub fn node_hold_edges(
        &self,
        crate_name: &str,
        version: &str,
    ) -> &[cooldown_core::GraphHoldEdge] {
        self.hold_edges
            .get(&(crate_name.to_string(), version.to_string()))
            .map_or(&[], Vec::as_slice)
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

/// Cargo's authoritative package and target topology for one locked resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StagingMetadata {
    pub(crate) workspace_root: camino::Utf8PathBuf,
    pub(crate) packages: Vec<StagingPackage>,
}

/// One package whose manifest and target paths Cargo may read during a resolve.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct StagingPackage {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) manifest_path: camino::Utf8PathBuf,
    pub(crate) target_paths: Vec<camino::Utf8PathBuf>,
    pub(crate) source: Option<String>,
    pub(crate) workspace_member: bool,
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

/// One manifest requirement awaiting a join to Cargo's active resolve edge — the
/// requirement-candidate rows themselves, and the join half of a [`DeclaredBoundEdge`]. Both the
/// bound and requirement joins resolve their candidates through [`joined_targets`], so the two
/// cannot drift apart on the rename/package-identity rules.
struct DeclaredEdge {
    requirer: String,
    /// The manifest rename (`alias = { package = "…" }`), when the declaration uses one. The
    /// resolve graph names a renamed edge by the rename, so it stays the join key — which also
    /// keeps an *inactive* renamed declaration off the same package's other active edges. A plain
    /// declaration cannot join by edge name at all: the resolve graph then names the edge by the
    /// dependency's **lib target**, which a custom `[lib] name` decouples from the package name,
    /// so it joins through the edge's resolved package identity instead.
    rename: Option<String>,
    package_name: String,
    requirement: String,
}

/// One workspace-member requirement carrying an explicit upper bound, joined to resolve edges the
/// same way as the requirement candidates.
struct DeclaredBoundEdge {
    declaration: DeclaredEdge,
    upper: UpperBound,
}

struct ActiveEdge {
    dependency_name: String,
    package_id: String,
}

type PackageEdges = HashMap<String, Vec<String>>;
type NamedPackageEdges = HashMap<String, Vec<ActiveEdge>>;

/// One declared `=x.y.z` requirement, as a *candidate* ceiling edge: the requirer's package id and
/// the dependency name and exact version it pins.
/// Resolved against the activated graph before it caps anything — a pin behind a disabled feature
/// or non-matching `target` is not a real edge.
struct ExactEdge {
    requirer: String,
    dependency: String,
    version: String,
    /// The verbatim declared requirement, carried for hold-edge attribution (so a held-dependency
    /// warning can quote the pin as written).
    requirement: String,
}

/// One lower-bound requirement, as a *candidate* floor edge: the requirer's package id and the
/// dependency name and floor version its requirement demands.
/// Resolved against the activated graph before it floors anything, for the same activation reasons
/// as [`ExactEdge`].
struct FloorEdge {
    requirer: String,
    dependency: String,
    /// The full declared requirement, kept beside the extracted `floor` so the resolved join can
    /// attach the floor only to nodes the requirement admits.
    requirement: String,
    floor: String,
}

/// Accumulates the per-dependency edge candidates [`build_graph`](Cargo::build_graph) harvests
/// from each package's declarations, before the activated-graph joins distill them into
/// constraints.
#[derive(Default)]
struct EdgeCandidates {
    exact_pins: HashSet<(String, String)>,
    /// Every non-dev `=x.y.z` requirement, as a candidate list — not the final ceiling set.
    exact_edges: Vec<ExactEdge>,
    /// Every non-root, non-dev requirement with a parseable lower bound.
    /// Like `exact_edges`, this is a candidate list resolved against the activated graph later: a
    /// requirement behind a disabled feature or non-matching `target` is not a real edge and
    /// demands no floor.
    /// Root requirements are intentionally excluded because they are direct project constraints
    /// cooldown may rewrite, not structural third-party graph floors.
    floor_edges: Vec<FloorEdge>,
    declared_bound_edges: Vec<DeclaredBoundEdge>,
    declared_requirement_edges: Vec<DeclaredEdge>,
}

impl EdgeCandidates {
    fn record(&mut self, p: &RawPkg, is_root: bool) {
        for dep in &p.dependencies {
            // A dev dependency of a transitive crate is not in the resolved build graph and caps
            // nothing; normal and build dependencies do, once confirmed active below.
            let is_dev = dep.kind.as_deref() == Some("dev");
            if let Some(version) = exact_req_version(&dep.req) {
                // A workspace member's own exact pin is the project's choice: it surfaces as
                // `pinned` (held, but with an adoptable target showing what it could be repinned
                // to).
                if is_root {
                    self.exact_pins.insert((dep.name.clone(), version.clone()));
                }
                if !is_dev {
                    self.exact_edges.push(ExactEdge {
                        requirer: p.id.clone(),
                        dependency: dep.name.clone(),
                        version,
                        requirement: dep.req.clone(),
                    });
                }
            }
            if !is_dev
                && !is_root
                && let Some(floor) = req_floor(&dep.req)
            {
                self.floor_edges.push(FloorEdge {
                    requirer: p.id.clone(),
                    dependency: dep.name.clone(),
                    requirement: dep.req.clone(),
                    floor,
                });
            }
            // A member's dev-dependency bound is as deliberate as its normal one, and dev deps
            // are resolved and upgradeable — the same reasoning that keeps dev pins in
            // `exact_pins` above.
            if is_root && let Some(upper) = explicit_upper_bound(&dep.req) {
                self.declared_bound_edges.push(DeclaredBoundEdge {
                    declaration: DeclaredEdge {
                        requirer: p.id.clone(),
                        rename: dep.rename.clone(),
                        package_name: dep.name.clone(),
                        requirement: dep.req.clone(),
                    },
                    upper,
                });
            }
            // A non-member's dev dependency gets the same treatment as in the exact/floor edges
            // above: it is not in the resolved build graph, yet its requirement would join by
            // *name* onto the crate's one active normal edge and veto edge rewrites the real
            // requirement admits. A member's dev requirement stays, mirroring the dev-pin and
            // dev-bound reasoning above.
            if !is_dev || is_root {
                self.declared_requirement_edges.push(DeclaredEdge {
                    requirer: p.id.clone(),
                    rename: dep.rename.clone(),
                    package_name: dep.name.clone(),
                    requirement: dep.req.clone(),
                });
            }
        }
    }
}

/// The activated `=`-pin ceilings [`resolved_graph_ceilings`] distills from the candidates: which
/// `(name, version)` nodes are capped, by which requirer package ids, and the attributed ceiling
/// edges (requirer name and version) for hold discounting.
struct ResolvedCeilings {
    ceilings: HashSet<(String, String)>,
    requirers: HashMap<(String, String), Vec<String>>,
    edges: HashMap<(String, String), Vec<cooldown_core::GraphHoldEdge>>,
}

/// The activated floors [`resolved_graph_floors`] distills from the candidates: the collapsed
/// per-node maximum floor beside the attributed floor edges behind it.
struct ResolvedFloors {
    floors: HashMap<(String, String), String>,
    edges: HashMap<(String, String), Vec<cooldown_core::GraphHoldEdge>>,
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

/// The manifest renames that can own an active resolve edge, indexed per requirer package id and
/// depended-on package name, normalized like resolve-edge names (hyphens to underscores).
///
/// A plain declaration joins through the edge's resolved package identity (see
/// [`DeclaredEdge::rename`]), which alone would also attach it to a *sibling rename's* node:
/// `foo = ">=0.5, <2"` beside `foo05 = { package = "foo", version = "0.5" }` resolves two `foo`
/// nodes, and the plain range admits both. The index names the edges the renamed declarations
/// own so the plain join can skip them. Keyed per depended-on package because a rename that
/// happens to collide with another package's lib target name owns none of that package's edges.
struct RenameIndex {
    by_requirer: HashMap<String, HashMap<String, HashSet<String>>>,
}

impl RenameIndex {
    /// Indexes the renamed declarations among `declarations`. Built from the
    /// requirement-candidate list because that list is exactly the declarations whose edges can
    /// appear in the resolve graph — every non-dev declaration plus a member's dev declarations
    /// (a non-member's dev edge is never resolved). The bound candidates would not do: they are
    /// the subset with explicit upper bounds and would miss a rename declared without one (a bare
    /// `foo05 = { package = "foo", version = "0.5" }`).
    fn from_declarations<'a>(declarations: impl IntoIterator<Item = &'a DeclaredEdge>) -> Self {
        let mut by_requirer: HashMap<String, HashMap<String, HashSet<String>>> = HashMap::new();
        for declaration in declarations {
            if let Some(rename) = &declaration.rename {
                by_requirer
                    .entry(declaration.requirer.clone())
                    .or_default()
                    .entry(declaration.package_name.clone())
                    .or_default()
                    .insert(rename.replace('-', "_"));
            }
        }
        RenameIndex { by_requirer }
    }

    /// Whether `requirer`'s edge named `edge_name` onto `package_name` belongs to one of its
    /// renamed declarations — the edges a plain declaration's package-identity join must skip.
    fn owns_edge(&self, requirer: &str, package_name: &str, edge_name: &str) -> bool {
        self.by_requirer
            .get(requirer)
            .and_then(|packages| packages.get(package_name))
            .is_some_and(|renames| renames.contains(&edge_name.replace('-', "_")))
    }
}

/// The resolved package nodes `declaration`'s active edges join to — the one join rule shared by
/// the declared-bound and declared-requirement candidates.
///
/// A renamed declaration joins by its rename (hyphen/underscore-normalized, as Cargo spells
/// resolve-edge names), which the resolve edge reliably carries. A plain declaration's edge name
/// is the dependency's lib target name — decoupled from the package name by a custom `[lib]
/// name` — so the package-identity and requirement-admission checks are its whole join, minus the
/// edges a sibling renamed declaration of the same package owns (see [`RenameIndex`]). Reduced
/// metadata fixtures that omit edge names still join their plain declarations, which never match
/// on the edge name.
fn joined_targets<'graph>(
    declaration: &DeclaredEdge,
    active_edges: &NamedPackageEdges,
    packages: &'graph HashMap<String, PkgInfo>,
    renames: &RenameIndex,
) -> Vec<&'graph PkgInfo> {
    let Some(edges) = active_edges.get(&declaration.requirer) else {
        return Vec::new();
    };
    let mut targets = Vec::new();
    for edge in edges {
        match &declaration.rename {
            Some(rename) => {
                if edge.dependency_name.replace('-', "_") != rename.replace('-', "_") {
                    continue;
                }
            }
            None => {
                if renames.owns_edge(
                    &declaration.requirer,
                    &declaration.package_name,
                    &edge.dependency_name,
                ) {
                    continue;
                }
            }
        }
        let Some(target) = packages.get(&edge.package_id) else {
            continue;
        };
        if target.name != declaration.package_name
            || !crate::version::version_in_range(&declaration.requirement, &target.version)
        {
            continue;
        }
        targets.push(target);
    }
    targets
}

fn resolved_declared_bounds(
    candidates: Vec<DeclaredBoundEdge>,
    active_edges: &NamedPackageEdges,
    packages: &HashMap<String, PkgInfo>,
    renames: &RenameIndex,
) -> HashMap<(String, String), String> {
    let mut picks: HashMap<(String, String), (UpperBound, String)> = HashMap::new();
    for candidate in candidates {
        for target in joined_targets(&candidate.declaration, active_edges, packages, renames) {
            let key = (target.name.clone(), target.version.clone());
            picks
                .entry(key)
                .and_modify(|current| {
                    if upper_bound_is_stricter(&candidate.upper, &current.0) {
                        current.clone_from(&(
                            candidate.upper.clone(),
                            candidate.declaration.requirement.clone(),
                        ));
                    }
                })
                .or_insert_with(|| {
                    (
                        candidate.upper.clone(),
                        candidate.declaration.requirement.clone(),
                    )
                });
        }
    }
    picks
        .into_iter()
        .map(|(key, (_, requirement))| (key, requirement))
        .collect()
}

fn resolved_declared_requirements(
    candidates: Vec<DeclaredEdge>,
    active_edges: &NamedPackageEdges,
    packages: &HashMap<String, PkgInfo>,
    renames: &RenameIndex,
) -> HashMap<LockPackageId, Vec<DeclaredRequirement>> {
    let mut resolved: HashMap<LockPackageId, Vec<DeclaredRequirement>> = HashMap::new();
    for candidate in candidates {
        let Some(dependent) = packages.get(&candidate.requirer) else {
            continue;
        };
        for target in joined_targets(&candidate, active_edges, packages, renames) {
            let requirement = DeclaredRequirement {
                dependency: target.name.clone(),
                resolved: LockPackageId::from_metadata(
                    &target.name,
                    &target.version,
                    target.source.as_deref(),
                ),
                requirement: candidate.requirement.clone(),
            };
            let requirements = resolved
                .entry(LockPackageId::from_metadata(
                    &dependent.name,
                    &dependent.version,
                    dependent.source.as_deref(),
                ))
                .or_default();
            if !requirements.iter().any(|existing| {
                existing.dependency == requirement.dependency
                    && existing.resolved == requirement.resolved
                    && existing.requirement == requirement.requirement
            }) {
                requirements.push(requirement);
            }
        }
    }
    resolved
}

/// Walks each active requirer edge to the depended node of the candidate's name that the
/// candidate's requirement admits and records the highest lower bound demanded of it, per resolved
/// `(name, version)` node.
fn resolved_graph_floors(
    floor_edges: Vec<FloorEdge>,
    edges: &HashMap<String, Vec<String>>,
    packages: &HashMap<String, PkgInfo>,
) -> ResolvedFloors {
    let mut graph_floors: HashMap<(String, String), String> = HashMap::new();
    let mut attributed: HashMap<(String, String), Vec<cooldown_core::GraphHoldEdge>> =
        HashMap::new();
    for candidate in floor_edges {
        let Some(dep_ids) = edges.get(&candidate.requirer) else {
            continue;
        };
        let Some(requirer_info) = packages.get(&candidate.requirer) else {
            continue;
        };
        for id in dep_ids {
            let Some(info) = packages.get(id) else {
                continue;
            };
            if info.name != candidate.dependency {
                continue;
            }
            // A renamed multi-major dependency resolves several same-name nodes into one
            // requirer's dep ids (`syn 1` beside `syn 2`). Joining by name alone would let the
            // max-wins merge below push the new major's floor onto the old major's node — a floor
            // above that node's own version, which [`req_floor`]'s contract rules out. A floor
            // attaches only to the node its requirement admits.
            if !crate::version::version_in_range(&candidate.requirement, &info.version) {
                continue;
            }
            let key = (info.name.clone(), info.version.clone());
            graph_floors
                .entry(key.clone())
                .and_modify(|current| {
                    if crate::version::compare(&candidate.floor, current).is_gt() {
                        current.clone_from(&candidate.floor);
                    }
                })
                .or_insert_with(|| candidate.floor.clone());
            attributed
                .entry(key)
                .or_default()
                .push(cooldown_core::GraphHoldEdge {
                    requirer: requirer_info.name.clone(),
                    requirer_version: cooldown_core::Version::new(requirer_info.version.clone()),
                    requirement: candidate.requirement.clone(),
                    bound: cooldown_core::Version::new(candidate.floor.clone()),
                    kind: cooldown_core::GraphHoldKind::Floor,
                });
        }
    }
    ResolvedFloors {
        floors: graph_floors,
        edges: attributed,
    }
}

/// The per-node graph constraints [`resolved_graph_constraints`] distills from the candidate
/// edges: the collapsed ceilings and floors [`ResolvedGraph`] gates with, plus the merged
/// attributed hold edges behind them.
struct GraphConstraints {
    graph_ceilings: HashSet<(String, String)>,
    ceiling_requirers: HashMap<(String, String), Vec<String>>,
    graph_floors: HashMap<(String, String), String>,
    hold_edges: HashMap<(String, String), Vec<cooldown_core::GraphHoldEdge>>,
}

/// Joins the exact-pin and floor candidates against the activated graph and merges their
/// attributed edges into one per-node hold-edge index.
///
/// A `=x.y.z` requirement caps a node only when its edge is actually in the resolved graph: an
/// inactive (optional/target-gated) pin is declared but absent from `resolve.nodes`, so it
/// contributes no ceiling — the consumer would otherwise over-hold a freely upgradable crate. A
/// non-root requirement floors a node only at the version its edge actually resolved to, the same
/// active-edge intersection.
fn resolved_graph_constraints(
    exact_edges: Vec<ExactEdge>,
    floor_edges: Vec<FloorEdge>,
    roots: &HashSet<String>,
    edges: &HashMap<String, Vec<String>>,
    packages: &HashMap<String, PkgInfo>,
) -> GraphConstraints {
    let ResolvedCeilings {
        ceilings: graph_ceilings,
        requirers: ceiling_requirers,
        edges: ceiling_hold_edges,
    } = resolved_graph_ceilings(exact_edges, roots, edges, packages);
    let ResolvedFloors {
        floors: graph_floors,
        edges: floor_hold_edges,
    } = resolved_graph_floors(floor_edges, edges, packages);
    let mut hold_edges = floor_hold_edges;
    for (key, mut ceiling_edges) in ceiling_hold_edges {
        hold_edges
            .entry(key)
            .or_default()
            .append(&mut ceiling_edges);
    }
    GraphConstraints {
        graph_ceilings,
        ceiling_requirers,
        graph_floors,
        hold_edges,
    }
}

fn resolved_graph_ceilings(
    candidates: Vec<ExactEdge>,
    roots: &HashSet<String>,
    edges: &HashMap<String, Vec<String>>,
    packages: &HashMap<String, PkgInfo>,
) -> ResolvedCeilings {
    let mut ceilings = HashSet::new();
    let mut requirers: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut attributed: HashMap<(String, String), Vec<cooldown_core::GraphHoldEdge>> =
        HashMap::new();
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
        if let Some(requirer_info) = packages.get(&candidate.requirer) {
            requirers
                .entry(key.clone())
                .or_default()
                .push(requirer_info.name.clone());
            // A workspace member's own exact pin surfaces as `pinned`, not as a ceiling (the
            // adapter masks `graph_ceiling` for pinned nodes), so attributing it here would let
            // effective-hold recomputation resurrect a cap the collapsed view deliberately
            // withholds. Attribution mirrors the floors: third-party requirers only.
            if !roots.contains(&candidate.requirer) {
                // An active exact pin's ceiling is the pinned node's own version (the adapter
                // invariant `graph_ceiling == current` documented on the core model).
                let bound = cooldown_core::Version::new(key.1.clone());
                attributed
                    .entry(key)
                    .or_default()
                    .push(cooldown_core::GraphHoldEdge {
                        requirer: requirer_info.name.clone(),
                        requirer_version: cooldown_core::Version::new(
                            requirer_info.version.clone(),
                        ),
                        requirement: candidate.requirement,
                        bound,
                        kind: cooldown_core::GraphHoldKind::Ceiling,
                    });
            }
        }
    }
    ResolvedCeilings {
        ceilings,
        requirers,
        edges: attributed,
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
    #[serde(default)]
    targets: Vec<RawTarget>,
}

#[derive(serde::Deserialize)]
struct RawTarget {
    src_path: String,
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

    /// A wrapper driving an explicit binary, so hermetic tests can substitute a scripted fake for
    /// the real `cargo` without touching the process environment. Unix-gated with its only
    /// consumers (the script-driven widen tests), which need a shell to run the fake.
    #[cfg(all(test, unix))]
    pub(crate) fn with_bin(bin: impl Into<String>) -> Self {
        Cargo { bin: bin.into() }
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
            // `ok` covers the spawn only; a launched command that exits non-zero still logs
            // `ok=true` here and its own `cargo command failed` line below.
            ok = result.is_ok(),
            "cargo finished"
        );
        if let Ok(out) = &result
            && !out.status.success()
        {
            tracing::debug!(
                bin = self.bin,
                args = ?args,
                status = %out.status,
                detail = %failure_detail(out),
                "cargo command failed"
            );
        }
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

    async fn run_locked_metadata(&self, dir: &Utf8Path) -> Result<String, CoreError> {
        let out = self
            .output(
                dir,
                &[
                    "metadata",
                    "--all-features",
                    "--locked",
                    "--format-version",
                    "1",
                ],
            )
            .await?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stale_lock_diagnostic(&stderr) {
            return Err(CoreError::StaleLock(format!(
                "Cargo.lock is stale in {dir}; run `cargo update` or `cargo generate-lockfile`"
            )));
        }
        Err(CoreError::Tool {
            tool: self.bin.clone(),
            termination: ToolTermination::from_exit_status(out.status),
            stderr: failure_detail(&out),
        })
    }

    /// Resolves the lock-generation graph for `dir` via `cargo metadata --all-features`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `cargo` cannot be spawned,
    /// [`CoreError::Tool`] if it exits non-zero, and [`CoreError::LockUnreadable`] if its JSON
    /// output cannot be parsed.
    pub async fn metadata(&self, dir: &Utf8Path) -> Result<ResolvedGraph, CoreError> {
        let stdout = self
            .run(
                dir,
                &["metadata", "--all-features", "--format-version", "1"],
            )
            .await?;
        Self::parse_graph(&stdout)
    }

    /// Reads the lock-generation graph without allowing Cargo to update `Cargo.lock`.
    ///
    /// The command may access the registry: `cargo metadata` reads every package's manifest, so
    /// it must be free to download the crates a fresh checkout has not cached.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::StaleLock`] when the lock must be updated, or the corresponding Cargo
    /// tool error for any other failure.
    pub async fn metadata_locked(&self, dir: &Utf8Path) -> Result<ResolvedGraph, CoreError> {
        let stdout = self.run_locked_metadata(dir).await?;
        Self::parse_graph(&stdout)
    }

    /// Reads the locked package and target topology without allowing Cargo to rewrite the source.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::StaleLock`] when the source lock is stale and a lock-read error when
    /// Cargo emits malformed or non-UTF-8 filesystem paths.
    pub(crate) async fn staging_metadata(
        &self,
        dir: &Utf8Path,
    ) -> Result<StagingMetadata, CoreError> {
        let stdout = self.run_locked_metadata(dir).await?;
        let raw: RawMeta = serde_json::from_str(&stdout)
            .map_err(|error| CoreError::LockUnreadable(format!("cargo metadata: {error}")))?;
        let workspace_root = camino::Utf8PathBuf::from(raw.workspace_root);
        if !workspace_root.is_absolute() {
            return Err(CoreError::LockUnreadable(
                "cargo metadata returned a relative or empty workspace root".to_string(),
            ));
        }
        let members: HashSet<_> = raw.workspace_members.into_iter().collect();
        let mut packages = Vec::with_capacity(raw.packages.len());
        for package in raw.packages {
            let manifest_path = camino::Utf8PathBuf::from(package.manifest_path);
            if !manifest_path.is_absolute() {
                return Err(CoreError::LockUnreadable(
                    "cargo metadata returned a relative or empty package manifest path".to_string(),
                ));
            }
            let mut target_paths = package
                .targets
                .into_iter()
                .map(|target| camino::Utf8PathBuf::from(target.src_path))
                .collect::<Vec<_>>();
            if target_paths.iter().any(|path| !path.is_absolute()) {
                return Err(CoreError::LockUnreadable(format!(
                    "cargo metadata returned a relative target path for {manifest_path}"
                )));
            }
            target_paths.sort();
            let workspace_member = members.contains(&package.id);
            packages.push(StagingPackage {
                id: package.id,
                name: package.name,
                version: package.version,
                manifest_path,
                target_paths,
                source: package.source,
                workspace_member,
            });
        }
        packages.sort();
        Ok(StagingMetadata {
            workspace_root,
            packages,
        })
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
        let mut candidates = EdgeCandidates::default();
        let mut msrv = MsrvIndex::default();
        for p in raw.packages {
            msrv.record(&p, roots.contains(&p.id));
            candidates.record(&p, roots.contains(&p.id));
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
        let GraphConstraints {
            graph_ceilings,
            ceiling_requirers,
            graph_floors,
            hold_edges,
        } = resolved_graph_constraints(
            candidates.exact_edges,
            candidates.floor_edges,
            &roots,
            &edges,
            &packages,
        );
        // Indexed from the requirement candidates rather than the bound candidates: only the
        // former list every edge-owning declaration (see [`RenameIndex::from_declarations`]).
        let renames = RenameIndex::from_declarations(&candidates.declared_requirement_edges);
        let declared_bounds = resolved_declared_bounds(
            candidates.declared_bound_edges,
            &active_edges,
            &packages,
            &renames,
        );
        let declared_requirements = resolved_declared_requirements(
            candidates.declared_requirement_edges,
            &active_edges,
            &packages,
            &renames,
        );
        ResolvedGraph {
            packages,
            roots,
            edges,
            exact_pins: candidates.exact_pins,
            graph_ceilings,
            ceiling_requirers,
            hold_edges,
            graph_floors,
            declared_bounds,
            declared_requirements,
            rust_versions: msrv.rust_versions,
            workspace_rust_version: msrv.workspace_rust_version,
        }
    }

    fn parse_graph(stdout: &str) -> Result<ResolvedGraph, CoreError> {
        let raw: RawMeta = serde_json::from_str(stdout)
            .map_err(|error| CoreError::LockUnreadable(format!("cargo metadata: {error}")))?;
        Ok(Self::build_graph(raw))
    }

    /// Verifies `Cargo.lock` and returns the authoritative resolved graph when it is current.
    ///
    /// Runs the same `cargo metadata --all-features --locked` as [`Self::metadata_locked`]; a
    /// stale lock exits 101 with cargo's `--locked` message and yields `Ok(None)`.
    /// The probe deliberately does not pass `--offline`: `cargo metadata` reads every package's
    /// manifest, so on a checkout whose crates are not cached it would fail to download them,
    /// and offline cargo also narrows the resolver to cached versions, which turns a stale lock
    /// into a spurious resolver conflict instead of the `--locked` message.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `cargo` cannot be spawned, or [`CoreError::Tool`] if it
    /// fails for a reason other than a stale lock.
    pub async fn verify_locked(&self, dir: &Utf8Path) -> Result<Option<ResolvedGraph>, CoreError> {
        match self.run_locked_metadata(dir).await {
            Ok(stdout) => Self::parse_graph(&stdout).map(Some),
            Err(CoreError::StaleLock(_)) => Ok(None),
            Err(error) => Err(error),
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

    /// Brings `Cargo.lock` current with the manifests via `cargo update --workspace`, the minimal
    /// refresh (the `--lock` step before a read-only command evaluates the lock).
    ///
    /// `--workspace` re-resolves only the workspace members' own entries: a missing lock is
    /// generated, a requirement the lock no longer satisfies is re-resolved, and a new dependency
    /// is added — while every locked version the manifests still admit stays exactly where it is.
    /// A plain `cargo update` (or `generate-lockfile`) would instead float the whole graph to the
    /// newest versions, and a refresh that itself drags in the too-fresh releases the gate then
    /// flags would defeat the point of the flag.
    ///
    /// A refresh cargo rejects is a tool failure, not a stale lock: the lock's currency was never
    /// established, so the read-only command must fail closed even under `--allow-stale-lock`,
    /// exactly as it does when the currency probe itself cannot run.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if the `cargo` process cannot be spawned, or
    /// [`CoreError::Tool`] with cargo's own detail if it exits non-zero.
    pub async fn refresh_lock(&self, dir: &Utf8Path) -> Result<LockVerifyReport, CoreError> {
        self.run(dir, &["update", "--workspace"]).await?;
        Ok(LockVerifyReport {
            status: LockStatus::Current,
            detail: "Cargo.lock refreshed (cargo update --workspace)".into(),
        })
    }
}

fn stale_lock_diagnostic(stderr: &str) -> bool {
    stderr.contains("needs to be updated but --locked was passed")
        || (stderr.contains("cannot update the lock file")
            && stderr.contains("because --locked was passed to prevent this"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use indoc::{formatdoc, indoc};

    #[tokio::test]
    async fn locked_metadata_rejects_a_stale_lock_without_rewriting_it() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::write(
            root.join("Cargo.toml"),
            indoc! {r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2024"
            "#},
        )?;
        std::fs::write(root.join("src/lib.rs"), "pub fn app() {}\n")?;
        let generated = std::process::Command::new(Cargo::new().bin)
            .args(["generate-lockfile", "--offline"])
            .current_dir(root)
            .output()?;
        if !generated.status.success() {
            return Err(eyre::eyre!(
                "cargo generate-lockfile failed: {}",
                String::from_utf8_lossy(&generated.stderr)
            ));
        }
        let before = std::fs::read(root.join("Cargo.lock"))?;
        std::fs::write(
            root.join("Cargo.toml"),
            indoc! {r#"
                [package]
                name = "app"
                version = "0.2.0"
                edition = "2024"
            "#},
        )?;

        let cargo = Cargo::new();
        let error = cargo.metadata_locked(root).await.err();
        std::assert_matches!(error, Some(CoreError::StaleLock(_)));
        let staging_error = cargo.staging_metadata(root).await.err();
        std::assert_matches!(staging_error, Some(CoreError::StaleLock(_)));
        assert_eq!(std::fs::read(root.join("Cargo.lock"))?, before);
        Ok(())
    }

    /// The `--lock` refresh generates a missing lock and brings a stale one current, while a
    /// refresh cargo rejects is a tool failure carrying cargo's detail, never a stale-lock report.
    /// A dependency-free workspace keeps the real `cargo` off the network.
    #[tokio::test]
    async fn refresh_lock_generates_a_missing_lock_and_brings_a_stale_one_current()
    -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        std::fs::create_dir_all(root.join("src"))?;
        let manifest = |version: &str| {
            formatdoc! {r#"
                [package]
                name = "app"
                version = "{version}"
                edition = "2024"
            "#}
        };
        std::fs::write(root.join("Cargo.toml"), manifest("0.1.0"))?;
        std::fs::write(root.join("src/lib.rs"), "pub fn app() {}\n")?;
        let cargo = Cargo::new();

        let generated = cargo.refresh_lock(root).await?;
        assert_eq!(
            generated.status,
            LockStatus::Current,
            "{}",
            generated.detail
        );
        assert!(
            root.join("Cargo.lock").is_file(),
            "a missing lock is generated"
        );
        assert!(cargo.verify_locked(root).await?.is_some());

        std::fs::write(root.join("Cargo.toml"), manifest("0.2.0"))?;
        std::assert_matches!(
            cargo.metadata_locked(root).await.err(),
            Some(CoreError::StaleLock(_)),
            "the version bump staled the lock"
        );
        let refreshed = cargo.refresh_lock(root).await?;
        assert_eq!(
            refreshed.status,
            LockStatus::Current,
            "{}",
            refreshed.detail
        );
        assert!(
            cargo.verify_locked(root).await?.is_some(),
            "the refreshed lock is current again"
        );

        // A manifest cargo cannot resolve (a path dependency that does not exist) is a tool
        // failure carrying cargo's own detail, never a stale-lock report `--allow-stale-lock`
        // could wave through.
        std::fs::write(
            root.join("Cargo.toml"),
            formatdoc! {r#"
                {}
                [dependencies]
                missing = {{ path = "missing" }}
            "#, manifest("0.2.0")},
        )?;
        let rejected = cargo
            .refresh_lock(root)
            .await
            .expect_err("cargo rejects the manifest");
        std::assert_matches!(
            &rejected,
            CoreError::Tool { stderr, .. } if stderr.contains("missing")
        );
        Ok(())
    }

    #[test]
    fn stale_lock_classification_matches_only_cargos_update_diagnostic() {
        assert!(stale_lock_diagnostic(
            "the lock file /tmp/Cargo.lock needs to be updated but --locked was passed to prevent this"
        ));
        assert!(stale_lock_diagnostic(
            "cannot update the lock file /tmp/Cargo.lock because --locked was passed to prevent this"
        ));
        assert!(!stale_lock_diagnostic(
            "failed to read lock file while --locked was passed: permission denied"
        ));
        assert!(!stale_lock_diagnostic(
            "failed to parse lock file: invalid TOML"
        ));
    }

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
        // keyed by full lock identity for the canonicalize candidate filter.
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
            graph
                .rust_versions
                .get(&LockPackageId::new("uuid", "1.24.0", None::<String>)),
            Some(&RustVersion::new(1, 63, 0))
        );
        assert!(
            !graph
                .rust_versions
                .contains_key(&LockPackageId::new("uuid", "0.8.2", None::<String>)),
            "a package without a declared rust-version contributes nothing"
        );
    }

    #[test]
    fn build_graph_keeps_source_distinct_requirements_separate() {
        let graph = Cargo::build_graph_from_json(indoc! {r#"
            {
                "packages": [
                    {"id": "registry-twin", "name": "twin", "version": "1.0.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "rust_version": "1.70",
                     "dependencies": [{"name": "dep", "req": "^1"}]},
                    {"id": "git-twin", "name": "twin", "version": "1.0.0",
                     "source": "git+https://example.com/twin#abcdef",
                     "rust_version": "1.80",
                     "dependencies": [{"name": "dep", "req": "^2"}]},
                    {"id": "dep-one", "name": "dep", "version": "1.5.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []},
                    {"id": "dep-two", "name": "dep", "version": "2.5.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []}
                ],
                "workspace_members": [],
                "workspace_root": "",
                "resolve": {"nodes": [
                    {"id": "registry-twin", "deps": [{"name": "dep", "pkg": "dep-one"}]},
                    {"id": "git-twin", "deps": [{"name": "dep", "pkg": "dep-two"}]}
                ]}
            }
        "#});
        let registry = LockPackageId::new("twin", "1.0.0", Some(CRATES_IO_SOURCE));
        let git = LockPackageId::new("twin", "1.0.0", Some("git+https://example.com/twin#abcdef"));

        assert_eq!(graph.declared_requirements[&registry][0].requirement, "^1");
        assert_eq!(graph.declared_requirements[&git][0].requirement, "^2");
        assert_eq!(graph.rust_versions[&registry], RustVersion::new(1, 70, 0));
        assert_eq!(graph.rust_versions[&git], RustVersion::new(1, 80, 0));
    }

    #[test]
    fn edge_requirements_exclude_inactive_renamed_declarations() {
        let graph = Cargo::build_graph_from_json(indoc! {r#"
            {
                "packages": [
                    {"id": "consumer", "name": "consumer", "version": "1.0.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": [
                         {"name": "dep", "req": ">=1, <3"},
                         {"name": "dep", "rename": "dep-narrow", "req": "^1.0"}
                     ]},
                    {"id": "dep", "name": "dep", "version": "2.0.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []}
                ],
                "workspace_members": [],
                "workspace_root": "",
                "resolve": {"nodes": [
                    {"id": "consumer", "deps": [{"name": "dep", "pkg": "dep"}]}
                ]}
            }
        "#});
        let consumer = LockPackageId::new("consumer", "1.0.0", Some(CRATES_IO_SOURCE));
        let requirements = &graph.declared_requirements[&consumer];

        assert_eq!(requirements.len(), 1);
        assert_eq!(requirements[0].requirement, ">=1, <3");
        assert_eq!(requirements[0].resolved.version, "2.0.0");
    }

    #[test]
    fn edge_requirements_join_a_custom_lib_target_name_through_package_identity() {
        // `resolve.nodes[].deps[].name` is the dependency's *lib target* name: a package with
        // `[lib] name = "weird_lib"` never matches its own package name, so a name-based join
        // silently degraded every edge onto it to an Unaddressable "metadata did not identify"
        // row. Plain declarations join through the edge's package id instead; the renamed
        // declaration beside it still joins through its rename.
        let graph = Cargo::build_graph_from_json(indoc! {r#"
            {
                "packages": [
                    {"id": "consumer", "name": "consumer", "version": "1.0.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": [
                         {"name": "odd-crate", "req": "^1"},
                         {"name": "dep", "rename": "dep-one", "req": "^1"}
                     ]},
                    {"id": "odd", "name": "odd-crate", "version": "1.2.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []},
                    {"id": "dep-v1", "name": "dep", "version": "1.5.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []}
                ],
                "workspace_members": [],
                "workspace_root": "",
                "resolve": {"nodes": [
                    {"id": "consumer", "deps": [
                        {"name": "weird_lib", "pkg": "odd"},
                        {"name": "dep_one", "pkg": "dep-v1"}
                    ]},
                    {"id": "odd", "deps": []},
                    {"id": "dep-v1", "deps": []}
                ]}
            }
        "#});
        let consumer = LockPackageId::new("consumer", "1.0.0", Some(CRATES_IO_SOURCE));
        let requirements = &graph.declared_requirements[&consumer];

        assert_eq!(requirements.len(), 2);
        assert!(
            requirements.iter().any(|requirement| {
                requirement.dependency == "odd-crate"
                    && requirement.resolved.version == "1.2.0"
                    && requirement.requirement == "^1"
            }),
            "a custom lib target name must not break the requirement join"
        );
        assert!(
            requirements.iter().any(|requirement| {
                requirement.dependency == "dep" && requirement.resolved.version == "1.5.0"
            }),
            "a manifest rename still joins through its rename"
        );
    }

    #[test]
    fn edge_requirements_attach_a_plain_declaration_beside_a_rename_to_its_own_node() {
        // `foo = ">=0.5, <2"` beside `foo05 = { package = "foo", version = "0.5" }` resolves two
        // `foo` nodes, and the plain range admits both. The package-identity join alone would
        // attach the plain requirement to the rename's 0.5 node too; the rename's edge belongs to
        // the renamed declaration, so each requirement stays on its own node.
        let graph = Cargo::build_graph_from_json(indoc! {r#"
            {
                "packages": [
                    {"id": "consumer", "name": "consumer", "version": "1.0.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": [
                         {"name": "foo", "req": ">=0.5, <2"},
                         {"name": "foo", "rename": "foo05", "req": "^0.5"}
                     ]},
                    {"id": "foo-v1", "name": "foo", "version": "1.9.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []},
                    {"id": "foo-v05", "name": "foo", "version": "0.5.3",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []}
                ],
                "workspace_members": [],
                "workspace_root": "",
                "resolve": {"nodes": [
                    {"id": "consumer", "deps": [
                        {"name": "foo", "pkg": "foo-v1"},
                        {"name": "foo05", "pkg": "foo-v05"}
                    ]},
                    {"id": "foo-v1", "deps": []},
                    {"id": "foo-v05", "deps": []}
                ]}
            }
        "#});
        let consumer = LockPackageId::new("consumer", "1.0.0", Some(CRATES_IO_SOURCE));
        let requirements = &graph.declared_requirements[&consumer];

        assert_eq!(requirements.len(), 2);
        assert!(
            requirements.iter().any(|requirement| {
                requirement.requirement == ">=0.5, <2" && requirement.resolved.version == "1.9.0"
            }),
            "the plain requirement joins only its own node"
        );
        assert!(
            requirements.iter().any(|requirement| {
                requirement.requirement == "^0.5" && requirement.resolved.version == "0.5.3"
            }),
            "the rename's requirement joins only the rename's node"
        );
    }

    #[test]
    fn edge_requirements_exclude_a_non_member_dev_dependency() {
        // diesel (a non-member) declares normal `uuid >=0.7, <2.0` beside dev `uuid ^0.8`. The
        // dev dep of a transitive crate is not in the resolved build graph, yet its requirement
        // would join by name onto diesel's one active uuid edge and make
        // `RequirementIndex::admits` veto a `0.8.2 → 1.24.0` restoration the normal range allows.
        // A member's own dev requirement stays indexed (dev deps of members are resolved).
        let graph = Cargo::build_graph_from_json(indoc! {r#"
            {
                "packages": [
                    {"id": "root", "name": "app", "version": "0.1.0",
                     "dependencies": [
                        {"name": "diesel", "req": "^2"},
                        {"name": "criterion", "req": "^0.5", "kind": "dev"}
                     ]},
                    {"id": "diesel", "name": "diesel", "version": "2.3.11",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": [
                        {"name": "uuid", "req": ">=0.7.0, <2.0.0"},
                        {"name": "uuid", "req": "^0.8", "kind": "dev"}
                     ]},
                    {"id": "uuid", "name": "uuid", "version": "0.8.2",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []},
                    {"id": "criterion", "name": "criterion", "version": "0.5.1",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []}
                ],
                "workspace_members": ["root"],
                "workspace_root": "",
                "resolve": {"nodes": [
                    {"id": "root", "deps": [
                        {"name": "diesel", "pkg": "diesel"},
                        {"name": "criterion", "pkg": "criterion"}
                    ]},
                    {"id": "diesel", "deps": [{"name": "uuid", "pkg": "uuid"}]},
                    {"id": "uuid", "deps": []},
                    {"id": "criterion", "deps": []}
                ]}
            }
        "#});
        let diesel = LockPackageId::new("diesel", "2.3.11", Some(CRATES_IO_SOURCE));
        let requirements = &graph.declared_requirements[&diesel];

        assert_eq!(requirements.len(), 1);
        assert_eq!(requirements[0].requirement, ">=0.7.0, <2.0.0");

        let root = LockPackageId::new("app", "0.1.0", None::<String>);
        let member_requirements = &graph.declared_requirements[&root];
        assert!(
            member_requirements
                .iter()
                .any(|requirement| requirement.dependency == "criterion"
                    && requirement.requirement == "^0.5"),
            "a workspace member's dev requirement stays indexed"
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
        let candidates: Vec<DeclaredBoundEdge> = [
            ("root-a", "serde", ">=1, <3"),
            ("root-b", "serde", ">=1, <2"),
            ("inactive-root", "serde", "<1.5"),
        ]
        .into_iter()
        .map(|(root, name, requirement)| DeclaredBoundEdge {
            declaration: DeclaredEdge {
                requirer: root.to_string(),
                rename: None,
                package_name: name.to_string(),
                requirement: requirement.to_string(),
            },
            upper: explicit_upper_bound(requirement).expect("upper bound"),
        })
        .collect();
        let renames = RenameIndex::from_declarations(
            candidates.iter().map(|candidate| &candidate.declaration),
        );

        let bounds = resolved_declared_bounds(candidates, &active_edges, &packages, &renames);

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
        let candidates: Vec<DeclaredBoundEdge> = [("foo-v1", ">=1, <2"), ("foo-v2", ">=2, <3")]
            .into_iter()
            .map(|(rename, requirement)| DeclaredBoundEdge {
                declaration: DeclaredEdge {
                    requirer: "root".to_string(),
                    rename: Some(rename.to_string()),
                    package_name: "foo".to_string(),
                    requirement: requirement.to_string(),
                },
                upper: explicit_upper_bound(requirement).expect("upper bound"),
            })
            .collect();
        let renames = RenameIndex::from_declarations(
            candidates.iter().map(|candidate| &candidate.declaration),
        );

        let bounds = resolved_declared_bounds(candidates, &active_edges, &packages, &renames);

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
    fn declared_bounds_join_a_custom_lib_target_name_through_package_identity() {
        // The bounds-level mirror of the requirements test above: a member's deliberate `<1.5`
        // cap on a crate shipping `[lib] name = "weird_lib"` never matched the resolve edge's
        // lib-target name, so `declared_bound` yielded `None`, `honor_declared_bounds` could not
        // veto targets past the cap, and the tentative-widen loop would rewrite the member's own
        // deliberate upper bound.
        let graph = Cargo::build_graph_from_json(indoc! {r#"
            {
                "packages": [
                    {"id": "root", "name": "app", "version": "0.1.0",
                     "dependencies": [{"name": "odd-crate", "req": ">=1, <1.5"}]},
                    {"id": "odd", "name": "odd-crate", "version": "1.2.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []}
                ],
                "workspace_members": ["root"],
                "workspace_root": "",
                "resolve": {"nodes": [
                    {"id": "root", "deps": [{"name": "weird_lib", "pkg": "odd"}]},
                    {"id": "odd", "deps": []}
                ]}
            }
        "#});
        assert_eq!(
            graph.declared_bound("odd-crate", "1.2.0"),
            Some(">=1, <1.5"),
            "a custom lib target name must not break the bound join"
        );
    }

    #[test]
    fn declared_bounds_exclude_edges_owned_by_a_sibling_renamed_declaration() {
        // The bounds-level plain-beside-rename case, with the rename declared as a bare caret:
        // `^0.5` writes no explicit upper bound, so its node must end up with *no* declared
        // bound — the plain `<2` must not leak across the package-identity join onto the
        // rename's node.
        let graph = Cargo::build_graph_from_json(indoc! {r#"
            {
                "packages": [
                    {"id": "root", "name": "app", "version": "0.1.0",
                     "dependencies": [
                         {"name": "foo", "req": ">=0.5, <2"},
                         {"name": "foo", "rename": "foo05", "req": "^0.5"}
                     ]},
                    {"id": "foo-v1", "name": "foo", "version": "1.9.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []},
                    {"id": "foo-v05", "name": "foo", "version": "0.5.3",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []}
                ],
                "workspace_members": ["root"],
                "workspace_root": "",
                "resolve": {"nodes": [
                    {"id": "root", "deps": [
                        {"name": "foo", "pkg": "foo-v1"},
                        {"name": "foo05", "pkg": "foo-v05"}
                    ]},
                    {"id": "foo-v1", "deps": []},
                    {"id": "foo-v05", "deps": []}
                ]}
            }
        "#});
        assert_eq!(graph.declared_bound("foo", "1.9.0"), Some(">=0.5, <2"));
        assert_eq!(
            graph.declared_bound("foo", "0.5.3"),
            None,
            "the plain bound must not attach to the rename's node"
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
    fn hold_edges_attribute_floors_and_third_party_ceilings_but_not_member_pins() {
        // syn floors quote (a third-party floor edge) and pins serde exactly (a third-party
        // ceiling edge); the workspace root pins itoa exactly, which surfaces as `pinned`, not as
        // an attributed ceiling — attribution must mirror what the collapsed view exposes, or
        // effective-hold recomputation could resurrect a cap the adapter deliberately masks.
        let json = r#"{
            "packages": [
                {"id": "root", "name": "root", "version": "0.1.0",
                 "dependencies": [{"name": "syn", "req": "^2.0"}, {"name": "itoa", "req": "=1.0.11"}]},
                {"id": "syn", "name": "syn", "version": "2.0.50",
                 "dependencies": [{"name": "quote", "req": "^1.0"}, {"name": "serde", "req": "=1.0.200"}]},
                {"id": "quote", "name": "quote", "version": "1.0.46", "dependencies": []},
                {"id": "serde", "name": "serde", "version": "1.0.200", "dependencies": []},
                {"id": "itoa", "name": "itoa", "version": "1.0.11", "dependencies": []}
            ],
            "workspace_members": ["root"],
            "workspace_root": "",
            "resolve": {"nodes": [
                {"id": "root", "deps": [{"pkg": "syn"}, {"pkg": "itoa"}]},
                {"id": "syn", "deps": [{"pkg": "quote"}, {"pkg": "serde"}]},
                {"id": "quote", "deps": []},
                {"id": "serde", "deps": []},
                {"id": "itoa", "deps": []}
            ]}
        }"#;
        let graph = Cargo::build_graph_from_json(json);

        let quote_edges = graph.node_hold_edges("quote", "1.0.46");
        assert_eq!(quote_edges.len(), 1);
        assert_eq!(quote_edges[0].requirer, "syn");
        assert_eq!(quote_edges[0].requirer_version.as_str(), "2.0.50");
        assert_eq!(quote_edges[0].bound.as_str(), "1.0.0");
        assert_eq!(quote_edges[0].kind, cooldown_core::GraphHoldKind::Floor);

        // An exact pin bounds from both sides, so it contributes a floor edge (its lower bound)
        // beside the ceiling edge — mirroring how the collapsed floors already include `=` pins.
        let serde_edges = graph.node_hold_edges("serde", "1.0.200");
        assert_eq!(serde_edges.len(), 2);
        assert!(serde_edges.iter().all(|edge| {
            edge.requirer == "syn"
                && edge.requirer_version.as_str() == "2.0.50"
                && edge.bound.as_str() == "1.0.200"
        }));
        assert!(
            serde_edges
                .iter()
                .any(|edge| edge.kind == cooldown_core::GraphHoldKind::Ceiling)
        );
        assert!(
            serde_edges
                .iter()
                .any(|edge| edge.kind == cooldown_core::GraphHoldKind::Floor)
        );

        // The root's exact pin still caps the collapsed view (masked behind `pinned` by the
        // dependency builder) but contributes no attributed edge.
        assert!(graph.is_graph_capped("itoa", "1.0.11"));
        assert!(graph.node_hold_edges("itoa", "1.0.11").is_empty());
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
    fn graph_floor_stays_on_the_node_each_requirement_admits() {
        // `renamer` depends on both syn majors via rename, so its resolved dep ids carry two
        // same-name `syn` nodes. The `^2` floor (2.0.0) must not attach to the 1.x node — that
        // would be a floor above the node's own version and would misclassify the 1.x line as
        // irreducible; each requirement floors only the node it admits.
        let json = r#"{
            "packages": [
                {"id": "root", "name": "root", "version": "0.1.0",
                 "dependencies": [{"name": "renamer", "req": "^1.0"}]},
                {"id": "renamer", "name": "renamer", "version": "1.0.0",
                 "dependencies": [
                    {"name": "syn", "req": "^1"},
                    {"name": "syn", "rename": "syn2", "req": "^2"}
                 ]},
                {"id": "syn-1", "name": "syn", "version": "1.0.100", "dependencies": []},
                {"id": "syn-2", "name": "syn", "version": "2.0.50", "dependencies": []}
            ],
            "workspace_members": ["root"],
            "workspace_root": "",
            "resolve": {"nodes": [
                {"id": "root", "deps": [{"pkg": "renamer"}]},
                {"id": "renamer", "deps": [{"pkg": "syn-1"}, {"pkg": "syn-2"}]},
                {"id": "syn-1", "deps": []},
                {"id": "syn-2", "deps": []}
            ]}
        }"#;
        let graph = Cargo::build_graph_from_json(json);
        assert_eq!(graph.graph_floor("syn", "1.0.100"), Some("1.0.0"));
        assert_eq!(graph.graph_floor("syn", "2.0.50"), Some("2.0.0"));
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
            hold_edges: HashMap::new(),
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
            hold_edges: HashMap::new(),
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
            hold_edges: HashMap::new(),
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
            hold_edges: HashMap::new(),
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
