//! Thin wrappers around the project's own `cargo` binary (resolution/apply engine only).

use camino::Utf8Path;
use cooldown_core::{CoreError, VerifyReport};
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
}

/// A single resolved package from `cargo metadata`.
pub struct PkgInfo {
    /// The crate name (e.g. `serde`).
    pub name: String,
    /// The exact resolved version (e.g. `1.0.197`).
    pub version: String,
    /// The source registry/path URL, or [`None`] for path/workspace members.
    pub source: Option<String>,
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
    /// Is `id` required by a non-root node (held by the graph)?
    #[must_use]
    pub fn is_graph_held(&self, id: &str) -> bool {
        self.edges
            .iter()
            .filter(|(node, _)| !self.roots.contains(*node))
            .any(|(_, deps)| deps.iter().any(|d| d == id))
    }
}

#[derive(serde::Deserialize)]
struct RawMeta {
    packages: Vec<RawPkg>,
    workspace_members: Vec<String>,
    resolve: Option<RawResolve>,
}
#[derive(serde::Deserialize)]
struct RawPkg {
    id: String,
    name: String,
    version: String,
    #[serde(default)]
    source: Option<String>,
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
        Command::new(&self.bin)
            .args(args)
            .current_dir(dir.as_std_path())
            .output()
            .await
            .map_err(|e| CoreError::ToolSpawn {
                tool: self.bin.clone(),
                detail: format!("`{} {}`: {e}", self.bin, args.join(" ")),
            })
    }

    async fn run(&self, dir: &Utf8Path, args: &[&str]) -> Result<String, CoreError> {
        let out = self.output(dir, args).await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(CoreError::Tool {
                tool: self.bin.clone(),
                status: out.status.code().unwrap_or(-1),
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
        let mut packages = HashMap::new();
        for p in raw.packages {
            packages.insert(
                p.id.clone(),
                PkgInfo {
                    name: p.name,
                    version: p.version,
                    source: p.source,
                },
            );
        }
        let mut edges = HashMap::new();
        if let Some(resolve) = raw.resolve {
            for node in resolve.nodes {
                edges.insert(node.id, node.deps.into_iter().map(|d| d.pkg).collect());
            }
        }
        Ok(ResolvedGraph {
            packages,
            roots: raw.workspace_members.into_iter().collect(),
            edges,
        })
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
                status: out.status.code().unwrap_or(-1),
                stderr: stderr.into_owned(),
            })
        }
    }

    /// Pins `name` from `from` to `to` via `cargo update -p <name>@<from> --precise <to>`.
    ///
    /// The `@<from>` disambiguates when a crate name resolves to multiple versions in the graph.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `cargo` cannot be spawned, or [`CoreError::Tool`] if
    /// the update is rejected (e.g. a `=`-pin or resolver conflict that blocks `--precise`).
    pub async fn update_precise(
        &self,
        dir: &Utf8Path,
        name: &str,
        from: &str,
        to: &str,
    ) -> Result<(), CoreError> {
        let spec = format!("{name}@{from}");
        self.run(dir, &["update", "-p", &spec, "--precise", to])
            .await
            .map(|_| ())
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
