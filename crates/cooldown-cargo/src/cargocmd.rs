//! Thin wrappers around the project's own `cargo` binary (resolution/apply engine only).

use camino::Utf8Path;
use cooldown_core::{CoreError, MemberRef, ToolTermination, VerifyReport};
use std::collections::{HashMap, HashSet};
use tokio::process::Command;

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
        self.source.as_deref() == Some("registry+https://github.com/rust-lang/crates.io-index")
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

    /// Resolve a graph node id to its workspace-member `(name, path)`, or `None` for a node that is
    /// not a known package. The shared mapping behind both attribution methods below.
    fn member_of(&self, node: &str) -> Option<MemberRef> {
        self.packages.get(node).map(|info| MemberRef {
            name: info.name.clone(),
            path: info.path.clone(),
        })
    }

    /// Whether every listed workspace member directly resolves `crate_name` at the requested target.
    ///
    /// Cargo can keep several versions of the same crate in one workspace. A lock-level check that
    /// only asks whether `crate_name@target` exists can therefore confuse another member's dependency
    /// for this member's unresolved one.
    #[must_use]
    pub fn direct_members_reach(
        &self,
        members: &[MemberRef],
        crate_name: &str,
        target: &str,
        downgrade: bool,
    ) -> bool {
        if members.is_empty() {
            return false;
        }

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
            let reached = dep_ids
                .iter()
                .filter_map(|id| self.packages.get(id))
                .any(|info| {
                    if info.name != crate_name || !info.is_crates_io() {
                        return false;
                    }
                    // Scope to the target's own compatibility slot, like `reached` does via its
                    // `(name, major)` key. One member can resolve several majors of a crate at once
                    // (a normal `nix = "0.28"` beside a target-gated `nix = "0.31"`); without this a
                    // sibling major that satisfies the bound would mask the slot we are moving.
                    if crate::version::major_key(&info.version) != target_major {
                        return false;
                    }
                    let ordering = crate::version::compare(&info.version, target);
                    if downgrade {
                        ordering.is_le()
                    } else {
                        ordering.is_ge()
                    }
                });
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
}

#[derive(serde::Deserialize)]
struct RawDep {
    name: String,
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
        let result = Command::new(&self.bin)
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
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
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
        // `(requirer id, dep name, exact pinned version)` for every non-dev `=x.y.z` requirement.
        // Resolved against the activated graph below: a declared pin behind a disabled feature or a
        // non-matching `target` cfg is not a real edge and must not cap, so this is a candidate list,
        // not the final ceiling set.
        let mut exact_edges: Vec<(String, String, String)> = Vec::new();
        // `(requirer id, dep name, floor)` for every non-root, non-dev requirement with a parseable
        // lower bound. Like `exact_edges`, a candidate list resolved against the activated graph
        // below: a requirement behind a disabled feature or non-matching `target` is not a real edge
        // and demands no floor. Root requirements are intentionally excluded: they are direct project
        // constraints cooldown may rewrite, not structural third-party graph floors.
        let mut floor_edges: Vec<(String, String, String)> = Vec::new();
        for p in raw.packages {
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
                        exact_edges.push((p.id.clone(), dep.name.clone(), version));
                    }
                }
                if !is_dev
                    && !roots.contains(&p.id)
                    && let Some(floor) = req_floor(&dep.req)
                {
                    floor_edges.push((p.id.clone(), dep.name.clone(), floor));
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
        let mut edges: HashMap<String, Vec<String>> = HashMap::new();
        if let Some(resolve) = raw.resolve {
            for node in resolve.nodes {
                edges.insert(node.id, node.deps.into_iter().map(|d| d.pkg).collect());
            }
        }
        // A `=x.y.z` requirement caps a node only when its edge is actually in the resolved graph:
        // keep an exact pin only if the requirer resolves an edge to a node of that name and version.
        // An inactive (optional/target-gated) edge is declared but absent from `resolve.nodes`, so it
        // contributes no ceiling — the consumer would otherwise over-hold a freely upgradable crate.
        let mut graph_ceilings = HashSet::new();
        let mut ceiling_requirers: HashMap<(String, String), Vec<String>> = HashMap::new();
        for (requirer, name, version) in exact_edges {
            let active = edges.get(&requirer).is_some_and(|dep_ids| {
                dep_ids.iter().any(|id| {
                    packages
                        .get(id)
                        .is_some_and(|info| info.name == name && info.version == version)
                })
            });
            if active {
                let key = (name.clone(), version.clone());
                graph_ceilings.insert(key.clone());
                if let Some(requirer_name) = packages.get(&requirer).map(|info| info.name.clone()) {
                    ceiling_requirers
                        .entry(key)
                        .or_default()
                        .push(requirer_name);
                }
            }
        }
        // A non-root requirement floors a node only at the version its edge actually resolved to: walk
        // each active requirer edge to the depended node of that name and record the highest lower
        // bound demanded of it. An inactive (optional/target-gated) edge is absent from
        // `resolve.nodes`, so it contributes no floor — mirroring the ceiling's active-edge
        // intersection above.
        let mut graph_floors: HashMap<(String, String), String> = HashMap::new();
        for (requirer, name, floor) in floor_edges {
            let Some(dep_ids) = edges.get(&requirer) else {
                continue;
            };
            for id in dep_ids {
                let Some(info) = packages.get(id) else {
                    continue;
                };
                if info.name != name {
                    continue;
                }
                let key = (info.name.clone(), info.version.clone());
                graph_floors
                    .entry(key)
                    .and_modify(|current| {
                        if crate::version::compare(&floor, current).is_gt() {
                            current.clone_from(&floor);
                        }
                    })
                    .or_insert_with(|| floor.clone());
            }
        }
        ResolvedGraph {
            packages,
            roots,
            edges,
            exact_pins,
            graph_ceilings,
            ceiling_requirers,
            graph_floors,
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
                stderr: stderr.into_owned(),
            })
        }
    }

    /// Pins every `(name, from)` in `specs` to the single shared version `to` in one whole-graph
    /// re-resolve via `cargo update -p A@<from> -p B@<from> … --precise <to>`. `--precise` accepts a
    /// single version but multiple `[SPEC]`s, so crates that share a target version are batched into
    /// one re-resolve; the caller groups distinct targets and calls this once per group. Each
    /// `@<from>` disambiguates a crate name that resolves to multiple versions in the graph.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `cargo` cannot be spawned, or [`CoreError::Tool`] if the
    /// update is rejected (e.g. a `=`-pin or resolver conflict blocks the precise move). A rejection
    /// is the caller's signal that the candidates stay where the resolver placed them.
    pub async fn update_precise_many(
        &self,
        dir: &Utf8Path,
        specs: &[(String, String)],
        to: &str,
    ) -> Result<(), CoreError> {
        if specs.is_empty() {
            return Ok(());
        }
        let mut args: Vec<String> = vec!["update".to_string()];
        for (name, from) in specs {
            args.push("-p".to_string());
            args.push(format!("{name}@{from}"));
        }
        args.push("--precise".to_string());
        args.push(to.to_string());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(dir, &arg_refs).await.map(|_| ())
    }

    /// Runs `cargo build` as the opt-in compile verification, reporting success in the [`VerifyReport`].
    ///
    /// A failed build is **not** an error: it is surfaced as `VerifyReport { ok: false, .. }` with
    /// the compiler's stderr in `detail`.
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
                String::from_utf8_lossy(&out.stderr).into_owned()
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
            "0.31.3",
            false,
        ));
        assert!(graph.direct_members_reach(
            &[MemberRef {
                name: "app-b".to_string(),
                path: "apps/b".to_string(),
            }],
            "nix",
            "0.31.3",
            false,
        ));
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
            "1.5.0",
            false,
        ));
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
            "1.5.0",
            false,
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
