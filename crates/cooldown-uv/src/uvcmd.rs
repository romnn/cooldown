//! Thin wrappers around the project's own `uv` binary (resolution/apply engine only).

use crate::cutoff::Cutoffs;
use camino::Utf8Path;
use cooldown_adapter_util::{resolve_program, supervised_output};
use cooldown_core::{CoreError, ToolTermination, VerifyReport, failure_detail};
use tokio::process::Command;

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

    /// Spawns `uv` with `args`, pinning the resolution cutoffs. `cutoffs` is a *required* argument —
    /// not a default — so no uv invocation can be added without consciously deciding its window. The
    /// single chokepoint every uv command flows through, so the type system guarantees cooldown's
    /// window is never silently forgotten.
    ///
    /// The cutoffs are appended as `--exclude-newer` / `--exclude-newer-package` arguments, the
    /// highest-precedence source, so the lock honors cooldown's policy regardless of the developer's
    /// environment or `~/.config/uv/uv.toml`, which may set a shorter (weaker) window or no
    /// exemption for a package the policy exempts.
    /// A relative span keeps the *persisted* value stable across runs (no per-run churn in the lock),
    /// but it still resolves to `now - window` each run, so once a dependency matures past the window
    /// `uv lock --check` reports the lock needs updating — the intended signal to re-lock, not a
    /// guarantee `--check`'s verdict never changes.
    ///
    /// A default cutoff also sets `UV_EXCLUDE_NEWER` (overriding the inherited env, `~/.config/uv`,
    /// and any pyproject value, since env beats config).
    /// No default (a `Latest`/opt-out window) *clears* the inherited `UV_EXCLUDE_NEWER` rather than
    /// letting the developer's ambient value silently apply a cutoff the policy disclaimed — uv then
    /// falls back to its config (the repo's own `uv.toml`), never the developer's shell.
    async fn output(
        &self,
        dir: &Utf8Path,
        args: &[&str],
        cutoffs: &Cutoffs,
    ) -> Result<std::process::Output, CoreError> {
        let cutoff_args = cutoffs.args();
        let mut command = Command::new(resolve_program(&self.bin));
        command
            .args(args)
            .args(&cutoff_args)
            .current_dir(dir.as_std_path());
        match cutoffs.default_cutoff() {
            Some(cutoff) => command.env("UV_EXCLUDE_NEWER", cutoff),
            None => command.env_remove("UV_EXCLUDE_NEWER"),
        };
        supervised_output(&mut command, &self.bin, || {
            std::iter::once(self.bin.as_str())
                .chain(args.iter().copied())
                .chain(cutoff_args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .await
    }

    async fn run(&self, dir: &Utf8Path, args: &[&str], cutoffs: &Cutoffs) -> Result<(), CoreError> {
        let out = self.output(dir, args, cutoffs).await?;
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
    pub async fn verify_check(&self, dir: &Utf8Path, cutoffs: &Cutoffs) -> Result<bool, CoreError> {
        let out = self.output(dir, &["lock", "--check"], cutoffs).await?;
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
    /// without pinning the rest of the graph. The default cutoff still applies to every other
    /// package, and each per-package cutoff in `cutoffs` to its own package.
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
        cutoffs: &Cutoffs,
    ) -> Result<(), CoreError> {
        let mut args = vec!["lock".to_string()];
        if upgrade {
            args.push("--upgrade".to_string());
        }
        for (name, version) in ceilings {
            args.push("--upgrade-package".to_string());
            args.push(format!("{name}<={version}"));
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(dir, &arg_refs, cutoffs).await
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
    pub async fn sync(&self, dir: &Utf8Path, cutoffs: &Cutoffs) -> Result<VerifyReport, CoreError> {
        let out = self.output(dir, &["sync"], cutoffs).await?;
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
