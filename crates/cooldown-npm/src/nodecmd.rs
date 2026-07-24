//! A thin wrapper around a package manager's own binary (npm/pnpm/yarn/bun), used only as the
//! resolution/apply engine. The cooldown verdict itself is computed in [`cooldown_core`]; these
//! drivers merely re-pin a dependency and (optionally) install the resolved graph.

use camino::Utf8Path;
use cooldown_adapter_util::resolve_program;
use cooldown_core::{
    CoreError, LockStatus, LockVerifyReport, Result, ToolTermination, VerifyReport, failure_detail,
};
use tokio::process::Command;

/// A handle to one package manager binary. The binary defaults to its canonical name on `PATH`
/// (e.g. `pnpm`) and is overridable via `COOLDOWN_<BIN>` (e.g. `COOLDOWN_PNPM`), which the test
/// suite and reproducible builds use to pin an exact executable.
#[derive(Clone)]
pub struct NodeCmd {
    bin: String,
}

impl NodeCmd {
    /// Creates a handle to `bin`, honouring the `COOLDOWN_<BIN>` override.
    #[must_use]
    pub fn new(bin: &str) -> Self {
        let env_key = format!("COOLDOWN_{}", bin.to_uppercase());
        NodeCmd {
            bin: std::env::var(&env_key).unwrap_or_else(|_| bin.to_string()),
        }
    }

    async fn output(&self, dir: &Utf8Path, args: &[String]) -> Result<std::process::Output> {
        Command::new(resolve_program(&self.bin))
            .args(args)
            .current_dir(dir.as_std_path())
            .output()
            .await
            .map_err(|e| CoreError::ToolSpawn {
                tool: self.bin.clone(),
                detail: format!("`{} {}`: {e}", self.bin, args.join(" ")),
            })
    }

    async fn checked_output(
        &self,
        dir: &Utf8Path,
        args: &[String],
    ) -> Result<std::process::Output> {
        let output = self.output(dir, args).await?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(CoreError::Tool {
                tool: self.bin.clone(),
                termination: ToolTermination::from_exit_status(output.status),
                stderr: failure_detail(&output),
            })
        }
    }

    /// Runs the driver, mapping a non-zero exit to a [`CoreError::Tool`].
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if the binary cannot be spawned, or [`CoreError::Tool`] if
    /// it exits non-zero (e.g. an unsatisfiable pin — a resolver conflict).
    pub async fn run(&self, dir: &Utf8Path, args: &[String]) -> Result<()> {
        self.checked_output(dir, args).await.map(|_| ())
    }

    /// Runs the driver and returns its standard output.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if the binary cannot be spawned, or [`CoreError::Tool`] if
    /// it exits non-zero.
    pub(crate) async fn stdout(&self, dir: &Utf8Path, args: &[String]) -> Result<String> {
        let output = self.checked_output(dir, args).await?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs the driver as a build/verify step, folding the exit status into a [`VerifyReport`] so a
    /// failed install is a reported outcome rather than a hard error.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] only if the binary cannot be spawned at all.
    pub async fn verify(
        &self,
        dir: &Utf8Path,
        args: &[String],
        ok_detail: &str,
    ) -> Result<VerifyReport> {
        let out = self.output(dir, args).await?;
        Ok(VerifyReport {
            ok: out.status.success(),
            detail: if out.status.success() {
                ok_detail.to_string()
            } else {
                failure_detail(&out)
            },
        })
    }

    /// Runs the driver as a lock-currency proof/refresh, folding resolver failure into a
    /// [`LockVerifyReport`] so callers can report a stale lock without treating it as a subprocess
    /// infrastructure error.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] only if the binary cannot be spawned at all.
    pub async fn lock_report(
        &self,
        dir: &Utf8Path,
        args: &[String],
        ok_detail: &str,
    ) -> Result<LockVerifyReport> {
        let out = self.output(dir, args).await?;
        Ok(LockVerifyReport {
            status: if out.status.success() {
                LockStatus::Current
            } else {
                LockStatus::Stale
            },
            detail: if out.status.success() {
                ok_detail.to_string()
            } else {
                failure_detail(&out)
            },
        })
    }
}
