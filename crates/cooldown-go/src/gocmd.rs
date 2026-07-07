//! Thin wrappers around the project's own `go` binary, used as a resolution/apply engine only —
//! never as the source of cooldown policy.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, ToolTermination, VerifyReport, failure_detail};
use std::collections::HashMap;
use tokio::process::Command;

/// A resolved module from `go list -m -json all`.
#[derive(Debug, Clone)]
pub struct GoModule {
    /// The declared module path.
    pub path: String,
    /// The resolved version, or `None` for the main module or a local-path replace.
    pub version: Option<String>,
    /// Whether this is the main module (the one being built).
    pub main: bool,
    /// Whether the module is an indirect (transitive) dependency.
    pub indirect: bool,
    /// The effective path if a `replace` directive applies.
    pub replace_path: Option<String>,
    /// The effective version if a `replace` directive applies; `None` for a local-path replace.
    pub replace_version: Option<String>,
}

impl GoModule {
    /// The module path actually resolved (after `replace`).
    #[must_use]
    pub fn effective_path(&self) -> &str {
        self.replace_path.as_deref().unwrap_or(&self.path)
    }
    /// The version actually resolved (after `replace`). `None` for a local-path replace.
    #[must_use]
    pub fn effective_version(&self) -> Option<&str> {
        if self.replace_path.is_some() {
            self.replace_version.as_deref()
        } else {
            self.version.as_deref()
        }
    }
    /// A local filesystem `replace` (no upstream version to gate).
    #[must_use]
    pub fn is_local_replace(&self) -> bool {
        self.replace_path.is_some() && self.replace_version.is_none()
    }
}

#[derive(serde::Deserialize)]
struct ModuleJson {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Version")]
    version: Option<String>,
    #[serde(rename = "Main")]
    main: Option<bool>,
    #[serde(rename = "Indirect")]
    indirect: Option<bool>,
    #[serde(rename = "Replace")]
    replace: Option<Box<ModuleJson>>,
    /// Present only under `-versions`: the module's available versions, already filtered by Go's
    /// own selection rules (ancient pre-module and `+incompatible` tags omitted for module-aware
    /// modules).
    #[serde(rename = "Versions")]
    versions: Option<Vec<String>>,
}

/// A throwaway copy of `go.mod`/`go.sum` used to run a read under `-mod=mod` without mutating the
/// real lock. It is created in the project directory — so relative `replace` paths still resolve —
/// and removed on drop. The process id and a per-process sequence keep concurrent probes from
/// colliding on the copy.
///
/// `-modfile` points `go` at the copy, so the real `go.mod`/`go.sum` should never be written. As a
/// hard guarantee against any `go` internal that drifts them anyway, the real lock is snapshotted on
/// create and restored byte-for-byte on drop — a version *read* must never leave the lock changed.
struct TempModfile {
    /// The `-modfile` argument value: the copy's file name, relative to the project directory.
    modfile_name: String,
    mod_path: Utf8PathBuf,
    sum_path: Utf8PathBuf,
    real_mod: Utf8PathBuf,
    real_sum: Utf8PathBuf,
    real_mod_snapshot: Option<Vec<u8>>,
    real_sum_snapshot: Option<Vec<u8>>,
}

/// Monotonic suffix making each [`TempModfile`] name unique within the process, so concurrent
/// version probes never share a copy (one's cleanup must not delete another's modfile mid-run).
static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl TempModfile {
    fn create(dir: &Utf8Path) -> Result<Self, CoreError> {
        let sequence = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stem = format!(".cooldown-versions-{}-{sequence}", std::process::id());
        let modfile_name = format!("{stem}.mod");
        let mod_path = dir.join(&modfile_name);
        let sum_path = dir.join(format!("{stem}.sum"));
        let real_mod = dir.join("go.mod");
        let real_sum = dir.join("go.sum");
        let real_mod_snapshot = std::fs::read(&real_mod).ok();
        let real_sum_snapshot = std::fs::read(&real_sum).ok();
        std::fs::copy(&real_mod, &mod_path)
            .map_err(|e| CoreError::LockUnreadable(format!("stage go.mod copy: {e}")))?;
        // go.sum may be absent (a module with no checksummed dependencies); seed the copy when it
        // exists so `go` reuses known sums instead of re-fetching every one.
        if real_sum_snapshot.is_some() {
            std::fs::copy(&real_sum, &sum_path)
                .map_err(|e| CoreError::LockUnreadable(format!("stage go.sum copy: {e}")))?;
        }
        Ok(TempModfile {
            modfile_name,
            mod_path,
            sum_path,
            real_mod,
            real_sum,
            real_mod_snapshot,
            real_sum_snapshot,
        })
    }
}

impl Drop for TempModfile {
    fn drop(&mut self) {
        // Best-effort cleanup; a leftover dotfile is harmless and overwritten on the next run.
        let _ = std::fs::remove_file(&self.mod_path);
        let _ = std::fs::remove_file(&self.sum_path);
        restore_unchanged(&self.real_mod, self.real_mod_snapshot.as_deref());
        restore_unchanged(&self.real_sum, self.real_sum_snapshot.as_deref());
    }
}

/// Restore `path` to its pre-read `snapshot`, only when it actually drifted (so an untouched lock
/// keeps its mtime). `None` means the file did not exist before, so any copy that appeared is
/// removed. Best-effort: a read must not fail because cleanup hit a transient I/O error.
fn restore_unchanged(path: &Utf8Path, snapshot: Option<&[u8]>) {
    match snapshot {
        Some(original) => {
            if std::fs::read(path).ok().as_deref() != Some(original) {
                let _ = std::fs::write(path, original);
            }
        }
        None => {
            if path.exists() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// The `go` driver, bound to a binary (default `go`, overridable via `COOLDOWN_GO`).
#[derive(Clone)]
pub struct Go {
    bin: String,
}

impl Default for Go {
    fn default() -> Self {
        Go {
            bin: std::env::var("COOLDOWN_GO").unwrap_or_else(|_| "go".to_string()),
        }
    }
}

impl Go {
    /// Creates a driver bound to the `go` binary, honoring the `COOLDOWN_GO` override.
    ///
    /// Equivalent to [`Go::default`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    async fn output(
        &self,
        dir: &Utf8Path,
        args: &[&str],
    ) -> Result<std::process::Output, CoreError> {
        tracing::debug!(bin = self.bin, args = ?args, dir = %dir, "spawn go");
        let started = std::time::Instant::now();
        let result = Command::new(&self.bin)
            .args(args)
            // Neutralize an ambient GOFLAGS for cooldown's own invocations. Repos commonly set
            // `GOFLAGS=-mod=mod` (via .env, a dotenv loaded by their task runner, or `go env -w`),
            // which would turn cooldown's *read* commands (`go list -m`, `go mod graph`) into
            // lock-mutating ones — they would rewrite go.sum just to report what is outdated.
            // cooldown drives `-mod` explicitly where it needs it (the `-modfile` version probe runs
            // `-mod=mod` against a throwaway copy), so clearing the inherited value keeps reads
            // read-only and leaves the project's go.mod/go.sum untouched.
            .env_remove("GOFLAGS")
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
            "go finished"
        );
        result
    }

    /// Run, requiring success; returns stdout.
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

    /// The resolved module graph (`go list -m -json all`). The main module is marked `main: true`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if the `go` command fails to spawn, [`CoreError::Tool`] if
    /// it exits non-zero, or [`CoreError::LockUnreadable`] if its JSON output cannot be parsed.
    pub async fn list_modules(&self, dir: &Utf8Path) -> Result<Vec<GoModule>, CoreError> {
        let stdout = self.run(dir, &["list", "-m", "-json", "all"]).await?;
        let mut out = Vec::new();
        let de = serde_json::Deserializer::from_str(&stdout);
        for v in de.into_iter::<ModuleJson>() {
            let m = v.map_err(|e| CoreError::LockUnreadable(format!("go list -m -json: {e}")))?;
            let (replace_path, replace_version) = match &m.replace {
                Some(r) => (Some(r.path.clone()), r.version.clone()),
                None => (None, None),
            };
            out.push(GoModule {
                path: m.path,
                version: m.version,
                main: m.main.unwrap_or(false),
                indirect: m.indirect.unwrap_or(false),
                replace_path,
                replace_version,
            });
        }
        Ok(out)
    }

    /// Go's authoritative available-version list per module (`go list -m -versions`), as a map
    /// from module path to the versions Go itself would consider.
    ///
    /// This is the single source of truth for *which versions exist* — unlike the raw GOPROXY
    /// `@v/list`, it applies Go's own selection rules. A module-aware module (one that adopted a
    /// `go.mod`) never lists its ancient pre-module tags: `k8s.io/client-go` reports only its
    /// `v0.x` line, never `v1.5.2` or `v11.0.0+incompatible`, while `github.com/docker/cli` (no
    /// `go.mod`) correctly keeps its `+incompatible` line. Sourcing candidates from here is what
    /// makes cooldown "only suggest what Go would".
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `go` fails to spawn, [`CoreError::Tool`] if it exits
    /// non-zero, or [`CoreError::LockUnreadable`] if its JSON output cannot be parsed.
    pub async fn list_versions(
        &self,
        dir: &Utf8Path,
    ) -> Result<HashMap<String, Vec<String>>, CoreError> {
        // Target the actual dependency module paths, never `all`: `go list -m -versions all` also
        // tries to resolve versions for the local main module (and any local-path replacements),
        // which have no published versions, so the whole command fails. Resolve the dependency
        // paths first, then ask Go for just their versions.
        let modules = self.list_modules(dir).await?;
        let mut paths: Vec<String> = modules
            .iter()
            .filter(|module| !module.main && !module.is_local_replace())
            .map(|module| module.effective_path().to_string())
            .collect();
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            return Ok(HashMap::new());
        }

        // Determining which versions Go would select means fetching each module's go.mod, which can
        // require go.sum entries the lock does not yet carry — so it needs `-mod=mod`, which writes
        // those (benign, additive) entries. To keep this a pure read, point `go` at a throwaway copy
        // of the lock via `-modfile` (its checksum file is the sibling `.sum`); the real go.mod and
        // go.sum are never touched. The copy lives in `dir` so relative `replace` paths still
        // resolve, and a Drop guard removes it on every exit path.
        let temp = TempModfile::create(dir)?;
        let modfile_arg = format!("-modfile={}", temp.modfile_name);
        let mut args = vec!["list", "-m", "-versions", "-json", "-mod=mod", &modfile_arg];
        args.extend(paths.iter().map(String::as_str));
        let stdout = self.run(dir, &args).await?;
        drop(temp);

        let mut out = HashMap::new();
        let de = serde_json::Deserializer::from_str(&stdout);
        for value in de.into_iter::<ModuleJson>() {
            let module = value.map_err(|e| {
                CoreError::LockUnreadable(format!("go list -m -versions -json: {e}"))
            })?;
            if let Some(versions) = module.versions {
                out.insert(module.path, versions);
            }
        }
        Ok(out)
    }

    /// The MVS floor each module is held at by *other* modules' requirements, from `go mod graph`.
    /// A module whose resolved version equals this floor cannot be lowered by changing direct deps
    /// alone → `check` annotates it `graph_held`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `go mod graph` fails to spawn, or [`CoreError::Tool`]
    /// if it exits non-zero.
    pub async fn mod_graph_floors(
        &self,
        dir: &Utf8Path,
        main_path: &str,
    ) -> Result<HashMap<String, String>, CoreError> {
        let stdout = self.run(dir, &["mod", "graph"]).await?;
        let mut floors: HashMap<String, String> = HashMap::new();
        for line in stdout.lines() {
            let mut parts = line.split_whitespace();
            let (Some(src), Some(tgt)) = (parts.next(), parts.next()) else {
                continue;
            };
            let src_path = src.split('@').next().unwrap_or(src);
            if src_path == main_path {
                continue; // the main module's direct requirement is what `upgrade` can change
            }
            let Some((tgt_path, tgt_ver)) = tgt.split_once('@') else {
                continue;
            };
            floors
                .entry(tgt_path.to_string())
                .and_modify(|cur| {
                    if crate::semver::compare(tgt_ver, cur) == std::cmp::Ordering::Greater {
                        *cur = tgt_ver.to_string();
                    }
                })
                .or_insert_with(|| tgt_ver.to_string());
        }
        Ok(floors)
    }

    /// Whether `go.mod`/`go.sum` are current relative to source (`go mod tidy -diff`, Go ≥ 1.23).
    /// `Ok(true)` = clean; `Ok(false)` = stale; `Err` = the probe itself failed.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `go mod tidy -diff` fails to spawn, or
    /// [`CoreError::Tool`] if it fails for a reason other than reporting a diff (e.g. the flag is
    /// unsupported on an older Go).
    pub async fn mod_tidy_is_clean(&self, dir: &Utf8Path) -> Result<bool, CoreError> {
        let out = self.output(dir, &["mod", "tidy", "-diff"]).await?;
        if out.status.success() {
            return Ok(true);
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return Ok(false); // a diff was printed → stale
        }
        Err(CoreError::Tool {
            tool: self.bin.clone(),
            termination: ToolTermination::from_exit_status(out.status),
            stderr: failure_detail(&out),
        })
    }

    /// `go get <module>@<version>` — updates `go.mod` and re-runs MVS.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `go get` fails to spawn, or [`CoreError::Tool`] if it
    /// exits non-zero (e.g. the resolver rejects the requested version).
    pub async fn get(&self, dir: &Utf8Path, module: &str, version: &str) -> Result<(), CoreError> {
        self.get_many(dir, &[(module.to_string(), version.to_string())])
            .await
    }

    /// `go get module1@v1 module2@v2 …` — the batched whole-graph form: every `module@version`
    /// target is passed to one invocation so Go runs a single minimal-version-selection (MVS) pass
    /// that settles all candidates jointly, rather than a sequence of per-module re-resolves.
    ///
    /// Each argument raises that module's `require` to an MVS *floor* (`max(all requirements)`), so
    /// the joint result is deterministic and convergent — there is no `<` upper bound for two
    /// candidates to push each other below forever. A no-op call (`targets` empty) is skipped.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `go get` fails to spawn, or [`CoreError::Tool`] if it
    /// exits non-zero (e.g. the joint resolve/compile rejects the requested set).
    pub async fn get_many(
        &self,
        dir: &Utf8Path,
        targets: &[(String, String)],
    ) -> Result<(), CoreError> {
        if targets.is_empty() {
            return Ok(());
        }
        let specs: Vec<String> = targets
            .iter()
            .map(|(module, version)| format!("{module}@{version}"))
            .collect();
        let mut args = vec!["get"];
        args.extend(specs.iter().map(String::as_str));
        self.run(dir, &args).await.map(|_| ())
    }

    /// `go mod tidy` — prune/add indirects and sync `go.sum`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `go mod tidy` fails to spawn, or [`CoreError::Tool`] if
    /// it exits non-zero.
    pub async fn mod_tidy(&self, dir: &Utf8Path) -> Result<(), CoreError> {
        self.run(dir, &["mod", "tidy"]).await.map(|_| ())
    }

    /// `go build ./...` — the opt-in compile verification (`--build`).
    ///
    /// A build failure is reported in the returned [`VerifyReport`] (with `ok: false`),
    /// not as an `Err`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] only if the `go` binary cannot be spawned at all.
    pub async fn build(&self, dir: &Utf8Path) -> Result<VerifyReport, CoreError> {
        let out = self.output(dir, &["build", "./..."]).await?;
        Ok(VerifyReport {
            ok: out.status.success(),
            detail: if out.status.success() {
                "go build ./... succeeded".to_string()
            } else {
                failure_detail(&out)
            },
        })
    }
}
