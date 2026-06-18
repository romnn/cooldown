//! Thin wrappers around the project's own `uv` binary (resolution/apply engine only).

use camino::Utf8Path;
use cooldown_core::{CoreError, VerifyReport};
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

    async fn run(&self, dir: &Utf8Path, args: &[&str]) -> Result<(), CoreError> {
        let out = self.output(dir, args).await?;
        if out.status.success() {
            Ok(())
        } else {
            Err(CoreError::Tool {
                tool: self.bin.clone(),
                status: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
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
    /// Returns [`CoreError::Tool`] if `uv` cannot be spawned, or if it exits
    /// non-zero for a reason *other* than a stale lock (so a genuine failure is
    /// never silently reported as "stale").
    pub async fn verify_check(&self, dir: &Utf8Path) -> Result<bool, CoreError> {
        let out = self.output(dir, &["lock", "--check"]).await?;
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
                status: out.status.code().unwrap_or(-1),
                stderr: stderr.into_owned(),
            })
        }
    }

    /// Re-resolves the lock, pinning `name` to `version`.
    ///
    /// Runs `uv lock --upgrade-package <name>==<version>`, which lets uv adjust
    /// the rest of the graph to keep it consistent.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Tool`] if `uv` cannot be spawned or exits non-zero
    /// (e.g. the pin is unsatisfiable — a resolver conflict).
    pub async fn upgrade_to(
        &self,
        dir: &Utf8Path,
        name: &str,
        version: &str,
    ) -> Result<(), CoreError> {
        self.run(
            dir,
            &["lock", "--upgrade-package", &format!("{name}=={version}")],
        )
        .await
    }

    /// Runs `uv sync`, the opt-in install/build verification step.
    ///
    /// The exit status is folded into the returned [`VerifyReport`]: a failed
    /// sync is a *report* with `ok: false`, not an error, so the build outcome
    /// can be surfaced to the user.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Tool`] only if `uv` cannot be spawned at all; a
    /// non-zero `uv sync` exit is reported via the [`VerifyReport`] instead.
    pub async fn sync(&self, dir: &Utf8Path) -> Result<VerifyReport, CoreError> {
        let out = self.output(dir, &["sync"]).await?;
        Ok(VerifyReport {
            ok: out.status.success(),
            detail: if out.status.success() {
                "uv sync succeeded".into()
            } else {
                String::from_utf8_lossy(&out.stderr).into_owned()
            },
        })
    }
}
