//! Thin wrappers around the project's own `uv` binary (resolution/apply engine only).

use camino::Utf8Path;
use cooldown_core::{CoreError, VerifyReport};
use tokio::process::Command;

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

    /// Whether `uv.lock` is current relative to `pyproject.toml` (`uv lock --check`: 0 clean,
    /// non-zero stale).
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

    /// `uv lock --upgrade-package <name>==<version>` — re-resolve, pinning `name` to the target.
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

    /// `uv sync` — the opt-in install/build verification.
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
