//! A thin wrapper around a package manager's own binary, used by adapters only as the
//! resolution/apply engine. The cooldown verdict itself is computed in [`cooldown_core`]; a driver
//! merely re-pins a dependency and (optionally) installs/locks the resolved graph.

use camino::Utf8Path;
use cooldown_core::{CoreError, Result, ToolTermination, VerifyReport, failure_detail};
use tokio::process::Command;

/// A handle to one package manager binary (e.g. `bundle`, `mix`, `mvn`). The binary defaults to
/// its given name on `PATH` and is overridable via `COOLDOWN_<BIN>` (e.g. `COOLDOWN_BUNDLE`), with
/// any `-` in the name normalised to `_`; reproducible builds and the test suite use the override
/// to pin an exact executable.
#[derive(Clone)]
pub struct Driver {
    bin: String,
}

impl Driver {
    /// Creates a handle to `bin`, honouring the `COOLDOWN_<BIN>` override.
    #[must_use]
    pub fn new(bin: &str) -> Self {
        let env_key = format!("COOLDOWN_{}", bin.to_uppercase().replace('-', "_"));
        Driver {
            bin: std::env::var(&env_key).unwrap_or_else(|_| bin.to_string()),
        }
    }

    /// The resolved binary name (after any `COOLDOWN_<BIN>` override).
    #[must_use]
    pub fn program(&self) -> &str {
        &self.bin
    }

    async fn output(&self, dir: &Utf8Path, args: &[String]) -> Result<std::process::Output> {
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

    /// Runs the driver, mapping a non-zero exit to a [`CoreError::Tool`].
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if the binary cannot be spawned, or [`CoreError::Tool`] if
    /// it exits non-zero (e.g. an unsatisfiable pin — a resolver conflict).
    pub async fn run(&self, dir: &Utf8Path, args: &[String]) -> Result<()> {
        let out = self.output(dir, args).await?;
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

    /// Runs the driver as a build/verify step, folding the exit status into a [`VerifyReport`] so a
    /// failed install/lock is a reported outcome rather than a hard error.
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
}
