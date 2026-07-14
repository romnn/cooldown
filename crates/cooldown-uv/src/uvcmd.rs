//! Thin wrappers around the project's own `uv` binary (resolution/apply engine only).

use camino::Utf8Path;
use cooldown_adapter_util::resolve_program;
use cooldown_core::{CoreError, ToolTermination, VerifyReport, failure_detail};
use tokio::process::Command;

/// The `--exclude-newer <window>` argument pair for a resolution cutoff, or empty when `None`. The
/// window is cooldown's resolved value — a relative span (`"14 days"`) or an absolute instant.
///
/// A relative span keeps the *persisted* value stable across runs (no per-run churn in the lock), but
/// it still resolves to `now - window` each run, so once a dependency matures past the window
/// `uv lock --check` reports the lock needs updating — the intended signal to re-lock, not a
/// guarantee `--check`'s verdict never changes.
///
/// uv resolves against a cutoff from (in precedence order) `--exclude-newer`, the `UV_EXCLUDE_NEWER`
/// env var, then config files. Passing cooldown's window as the CLI flag — the highest-precedence
/// source — makes the lock honor cooldown's policy regardless of the developer's environment or
/// `~/.config/uv/uv.toml`, which may set a shorter (weaker) window.
fn exclude_newer_args(cutoff: Option<&str>) -> Vec<String> {
    match cutoff {
        Some(cutoff) => vec!["--exclude-newer".to_string(), cutoff.to_string()],
        None => Vec::new(),
    }
}

/// A handle to the project's own `uv` binary, used only as the resolution/apply engine.
///
/// The binary defaults to `uv` on `PATH` and is overridable via the `COOLDOWN_UV`
/// environment variable. Every method shells out in a given project directory;
/// the cooldown verdict itself is computed in [`cooldown_core`], not here.
#[derive(Clone)]
pub struct Uv {
    bin: String,
}

impl Default for Uv {
    fn default() -> Self {
        Uv {
            bin: std::env::var("COOLDOWN_UV").unwrap_or_else(|_| "uv".to_string()),
        }
    }
}

impl Uv {
    /// Creates a handle to the `uv` binary, honouring the `COOLDOWN_UV` override.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawns `uv` with `args`, pinning the resolution cutoff. `cutoff` is a *required* argument — not
    /// a default — so no uv invocation can be added without consciously deciding its window. The
    /// single chokepoint every uv command flows through, so the type system guarantees cooldown's
    /// window is never silently forgotten. `Some` sets `UV_EXCLUDE_NEWER` to cooldown's cutoff
    /// (overriding the inherited env, `~/.config/uv`, and any pyproject value, since env beats config).
    /// `None` (a `Latest`/opt-out window) *clears* the inherited `UV_EXCLUDE_NEWER` rather than letting
    /// the developer's ambient value silently apply a cutoff the policy disclaimed — uv then falls
    /// back to its config (the repo's own `uv.toml`), never the developer's shell.
    async fn output(
        &self,
        dir: &Utf8Path,
        args: &[&str],
        cutoff: Option<&str>,
    ) -> Result<std::process::Output, CoreError> {
        let mut command = Command::new(resolve_program(&self.bin));
        command.args(args).current_dir(dir.as_std_path());
        match cutoff {
            Some(cutoff) => command.env("UV_EXCLUDE_NEWER", cutoff),
            None => command.env_remove("UV_EXCLUDE_NEWER"),
        };
        command.output().await.map_err(|e| CoreError::ToolSpawn {
            tool: self.bin.clone(),
            detail: format!("`{} {}`: {e}", self.bin, args.join(" ")),
        })
    }

    async fn run(
        &self,
        dir: &Utf8Path,
        args: &[&str],
        cutoff: Option<&str>,
    ) -> Result<(), CoreError> {
        let out = self.output(dir, args, cutoff).await?;
        if out.status.success() {
            Ok(())
        } else {
            Err(CoreError::Tool {
                tool: self.bin.clone(),
                termination: ToolTermination::from_exit_status(out.status),
                stderr: failure_detail(&out),
            })
        }
    }

    /// Reports whether `uv.lock` is current relative to `pyproject.toml`.
    ///
    /// Runs `uv lock --check`: exit 0 means clean (`true`), and the known
    /// "stale lock" exit means `false`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `uv` cannot be spawned, or [`CoreError::Tool`] if it
    /// exits non-zero for a reason *other* than a stale lock (so a genuine failure is never
    /// silently reported as "stale").
    pub async fn verify_check(
        &self,
        dir: &Utf8Path,
        cutoff: Option<&str>,
    ) -> Result<bool, CoreError> {
        let mut args = vec!["lock".to_string(), "--check".to_string()];
        args.extend(exclude_newer_args(cutoff));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.output(dir, &arg_refs, cutoff).await?;
        if out.status.success() {
            return Ok(true);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("needs to be updated")
            || stderr.contains("--check")
            || stderr.contains("--locked")
        {
            Ok(false)
        } else {
            Err(CoreError::Tool {
                tool: self.bin.clone(),
                termination: ToolTermination::from_exit_status(out.status),
                stderr: failure_detail(&out),
            })
        }
    }

    /// Re-resolves the **whole** dependency graph once under cooldown's window, letting uv settle every
    /// conflict itself.
    ///
    /// With `upgrade = true` it runs `uv lock --upgrade --exclude-newer <cutoff>`: uv re-resolves the
    /// entire graph to the maximal versions admissible under the cutoff, resolving every mutual
    /// exclusion (e.g. raising `huggingface-hub` forces `typer` down) on its own. The result is the
    /// unique maximal-within-window lock, so a second identical invocation is a fixed point — no
    /// per-package pins, no oscillation, and no package outside an explicit candidate set is left to
    /// drift silently, because *every* package is re-resolved under the same cutoff.
    ///
    /// With `upgrade = false` it runs `uv lock --exclude-newer <cutoff>` (no `--upgrade`): a minimal
    /// re-lock that lowers only the packages whose locked version is now *newer* than the cutoff (a
    /// too-fresh pin is invalid under `--exclude-newer`, so uv must mature it down) while leaving every
    /// already-compliant package untouched. This is the `fix` / reconcile form — roll the too-fresh
    /// deps back to the window without otherwise churning the graph.
    ///
    /// `ceilings` caps specific packages at `<= version` via uv's own `--upgrade-package <name><=<v>`.
    /// It is empty in the uniform-window case (one global cutoff governs the whole graph); cooldown
    /// adds a ceiling only for a package whose verdict is stricter than the global window — a longer
    /// per-package window, a floor, or an exempt freeze — so that per-package policy is enforced
    /// without pinning the rest of the graph. The global `cutoff` still applies to every other package.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if `uv` cannot be spawned, or [`CoreError::Tool`] if it
    /// exits non-zero (e.g. the declared requirements are unsatisfiable under the cutoff).
    pub async fn lock_resolve(
        &self,
        dir: &Utf8Path,
        upgrade: bool,
        ceilings: &[(String, String)],
        cutoff: Option<&str>,
    ) -> Result<(), CoreError> {
        let mut args = vec!["lock".to_string()];
        if upgrade {
            args.push("--upgrade".to_string());
        }
        for (name, version) in ceilings {
            args.push("--upgrade-package".to_string());
            args.push(format!("{name}<={version}"));
        }
        args.extend(exclude_newer_args(cutoff));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(dir, &arg_refs, cutoff).await
    }

    /// Runs `uv sync`, the opt-in install/build verification step.
    ///
    /// The exit status is folded into the returned [`VerifyReport`]: a failed
    /// sync is a *report* with `ok: false`, not an error, so the build outcome
    /// can be surfaced to the user.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] only if `uv` cannot be spawned at all; a non-zero
    /// `uv sync` exit is reported via the [`VerifyReport`] instead.
    pub async fn sync(
        &self,
        dir: &Utf8Path,
        cutoff: Option<&str>,
    ) -> Result<VerifyReport, CoreError> {
        let mut args = vec!["sync".to_string()];
        args.extend(exclude_newer_args(cutoff));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.output(dir, &arg_refs, cutoff).await?;
        Ok(VerifyReport {
            ok: out.status.success(),
            detail: if out.status.success() {
                "uv sync succeeded".into()
            } else {
                failure_detail(&out)
            },
        })
    }
}
