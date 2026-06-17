//! Thin wrappers around the project's own `go` binary, used as a resolution/apply engine only —
//! never as the source of cooldown policy.

use camino::Utf8Path;
use cooldown_core::{CoreError, VerifyReport};
use std::collections::HashMap;
use tokio::process::Command;

/// A resolved module from `go list -m -json all`.
#[derive(Debug, Clone)]
pub struct GoModule {
    pub path: String,
    pub version: Option<String>,
    pub main: bool,
    pub indirect: bool,
    /// The effective path/version if a `replace` directive applies.
    pub replace_path: Option<String>,
    pub replace_version: Option<String>,
}

impl GoModule {
    /// The module path actually resolved (after `replace`).
    pub fn effective_path(&self) -> &str {
        self.replace_path.as_deref().unwrap_or(&self.path)
    }
    /// The version actually resolved (after `replace`). `None` for a local-path replace.
    pub fn effective_version(&self) -> Option<&str> {
        if self.replace_path.is_some() {
            self.replace_version.as_deref()
        } else {
            self.version.as_deref()
        }
    }
    /// A local filesystem `replace` (no upstream version to gate).
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
            .map_err(|e| CoreError::Tool {
                tool: self.bin.clone(),
                status: -1,
                stderr: format!("failed to spawn `{} {}`: {e}", self.bin, args.join(" ")),
            })
    }

    /// Run, requiring success; returns stdout.
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

    /// The resolved module graph (`go list -m -json all`). The main module is marked `main: true`.
    pub async fn list_modules(&self, dir: &Utf8Path) -> Result<Vec<GoModule>, CoreError> {
        let stdout = self.run(dir, &["list", "-m", "-json", "all"]).await?;
        let mut out = Vec::new();
        let de = serde_json::Deserializer::from_str(&stdout);
        for v in de.into_iter::<ModuleJson>() {
            let m = v.map_err(|e| CoreError::Parse(format!("go list -m -json: {e}")))?;
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

    /// The MVS floor each module is held at by *other* modules' requirements, from `go mod graph`.
    /// A module whose resolved version equals this floor cannot be lowered by changing direct deps
    /// alone → `check` annotates it `graph_held`.
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
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// `go get <module>@<version>` — updates `go.mod` and re-runs MVS.
    pub async fn get(&self, dir: &Utf8Path, module: &str, version: &str) -> Result<(), CoreError> {
        self.run(dir, &["get", &format!("{module}@{version}")])
            .await
            .map(|_| ())
    }

    /// `go mod tidy` — prune/add indirects and sync `go.sum`.
    pub async fn mod_tidy(&self, dir: &Utf8Path) -> Result<(), CoreError> {
        self.run(dir, &["mod", "tidy"]).await.map(|_| ())
    }

    /// `go build ./...` — the opt-in compile verification (`--build`).
    pub async fn build(&self, dir: &Utf8Path) -> Result<VerifyReport, CoreError> {
        let out = self.output(dir, &["build", "./..."]).await?;
        Ok(VerifyReport {
            ok: out.status.success(),
            detail: if out.status.success() {
                "go build ./... succeeded".to_string()
            } else {
                String::from_utf8_lossy(&out.stderr).into_owned()
            },
        })
    }
}
