//! A thin wrapper around a package manager's own binary (npm/pnpm/yarn/bun), used only as the
//! resolution/apply engine. The cooldown verdict itself is computed in [`cooldown_core`]; these
//! drivers merely re-pin a dependency and (optionally) install the resolved graph.

use camino::Utf8Path;
use cooldown_adapter_util::{resolve_program, supervised_output};
use cooldown_core::{
    CoreError, LockStatus, LockVerifyReport, Result, ToolTermination, VerifyReport, failure_detail,
};
use std::ffi::OsString;
use tokio::process::Command;

/// A handle to one package manager binary. The binary defaults to its canonical name on `PATH`
/// (e.g. `pnpm`) and is overridable via `COOLDOWN_<BIN>` (e.g. `COOLDOWN_PNPM`), which the test
/// suite and reproducible builds use to pin an exact executable.
#[derive(Clone)]
pub struct NodeCmd {
    bin: String,
    /// The variables every invocation sets on top of the inherited environment.
    env: Vec<EnvVar>,
}

/// One environment variable a [`NodeCmd`] sets for its driver.
#[derive(Clone)]
struct EnvVar {
    name: &'static str,
    value: &'static str,
}

/// One environment variable a single run sets on top of the driver's own
/// ([`NodeCmd::output_overriding`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnvOverride {
    pub(crate) name: &'static str,
    pub(crate) value: OsString,
}

/// The names among `inherited` that equal one of `names` without regard to case, the exact
/// spellings included.
fn case_variants(inherited: impl IntoIterator<Item = OsString>, names: &[&str]) -> Vec<OsString> {
    inherited
        .into_iter()
        .filter(|variable| {
            variable.to_str().is_some_and(|variable| {
                names.iter().any(|name| variable.eq_ignore_ascii_case(name))
            })
        })
        .collect()
}

impl NodeCmd {
    /// The executable selected for this driver, including any explicit override.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.bin
    }

    /// Creates a handle to `bin`, honouring the `COOLDOWN_<BIN>` override.
    #[must_use]
    pub fn new(bin: &str) -> Self {
        let env_key = format!("COOLDOWN_{}", bin.to_uppercase());
        NodeCmd {
            bin: std::env::var(&env_key).unwrap_or_else(|_| bin.to_string()),
            env: Vec::new(),
        }
    }

    /// Sets `name` to `value` for every invocation of the driver, whatever the inherited
    /// environment says.
    ///
    /// A later call for the same `name` wins.
    #[must_use]
    pub(crate) fn with_env(mut self, name: &'static str, value: &'static str) -> Self {
        self.env.push(EnvVar { name, value });
        self
    }

    /// A wrapper driving an explicit binary, so hermetic tests can substitute a scripted fake for
    /// the real package manager without touching the process environment (the `COOLDOWN_<BIN>`
    /// override is process-global, which parallel tests cannot share). Unix-gated with its only
    /// consumers, which need a shell to run the fake.
    #[cfg(all(test, unix))]
    pub(crate) fn with_bin(bin: impl Into<String>) -> Self {
        NodeCmd {
            bin: bin.into(),
            env: Vec::new(),
        }
    }

    async fn output(&self, dir: &Utf8Path, args: &[String]) -> Result<std::process::Output> {
        supervised_output(
            Command::new(resolve_program(&self.bin))
                .args(args)
                .envs(self.env.iter().map(|EnvVar { name, value }| (name, value)))
                .current_dir(dir.as_std_path()),
            &self.bin,
            || format!("{} {}", self.bin, args.join(" ")),
        )
        .await
    }

    /// Runs the driver with `overrides` set on top of the driver's own variables, and returns its
    /// output whatever its exit status.
    ///
    /// Every inherited variable whose name equals one the run sets, compared without case, is
    /// removed first.
    /// Yarn reads `YARN_*` settings case-insensitively, so a `yarn_enable_network` the caller's
    /// shell exports would otherwise compete with the `YARN_ENABLE_NETWORK` set here.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if the binary cannot be spawned.
    pub(crate) async fn output_overriding(
        &self,
        dir: &Utf8Path,
        args: &[String],
        overrides: &[EnvOverride],
    ) -> Result<std::process::Output> {
        let names: Vec<&str> = self
            .env
            .iter()
            .map(|EnvVar { name, .. }| *name)
            .chain(overrides.iter().map(|EnvOverride { name, .. }| *name))
            .collect();
        let mut command = Command::new(resolve_program(&self.bin));
        for variant in case_variants(std::env::vars_os().map(|(name, _)| name), &names) {
            command.env_remove(variant);
        }
        command
            .args(args)
            .envs(self.env.iter().map(|EnvVar { name, value }| (name, value)))
            .envs(
                overrides
                    .iter()
                    .map(|EnvOverride { name, value }| (name, value)),
            )
            .current_dir(dir.as_std_path());
        supervised_output(&mut command, &self.bin, || {
            format!("{} {}", self.bin, args.join(" "))
        })
        .await
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

    /// Runs the driver as a lock-currency proof, telling the manager's refusal of a stale lock
    /// apart from every other failure.
    ///
    /// A failed run is [`LockStatus::Stale`] only when `is_stale` recognizes its failure detail
    /// as the manager's own stale-lock refusal, and the run exited on its own rather than through
    /// a signal or an interruption ([`ToolTermination::is_abnormal`]).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if the binary cannot be spawned, or [`CoreError::Tool`]
    /// for any other failure, such as a registry, authentication, or configuration error, which
    /// `--allow-stale-lock` never waives.
    pub async fn lock_report(
        &self,
        dir: &Utf8Path,
        args: &[String],
        lockfile: &str,
        is_stale: fn(&str) -> bool,
    ) -> Result<LockVerifyReport> {
        let out = self.output(dir, args).await?;
        if out.status.success() {
            return Ok(LockVerifyReport {
                status: LockStatus::Current,
                detail: format!("{lockfile} is current"),
            });
        }
        let termination = ToolTermination::from_exit_status(out.status);
        let detail = failure_detail(&out);
        // Only the termination is checked, not the detail's broken-environment phrases: a stale
        // refusal quotes the manifest's dependency names, which can spell one of them.
        if termination.is_abnormal() || !is_stale(&detail) {
            return Err(CoreError::Tool {
                tool: self.bin.clone(),
                termination,
                stderr: detail,
            });
        }
        // The stale detail leads with the lockfile and its project, ahead of the manager's own
        // failure text: the diagnostic's path field is not printed on the TTY, and a repository
        // with several locks needs to know which one the manager rejected.
        Ok(LockVerifyReport {
            status: LockStatus::Stale,
            detail: format!("{lockfile} is stale in {dir}: {detail}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::case_variants;
    use std::ffi::OsString;

    /// Every spelling of a set variable is scrubbed, whatever its case, and no other variable
    /// is.
    #[test]
    fn case_variants_match_names_without_case() {
        let inherited = [
            "YARN_ENABLE_NETWORK",
            "yarn_enable_network",
            "Yarn_Enable_Network",
            "YARN_ENABLE_NETWORK_X",
            "git_allow_protocol",
            "PATH",
        ]
        .map(OsString::from);

        let scrubbed = case_variants(inherited, &["YARN_ENABLE_NETWORK", "GIT_ALLOW_PROTOCOL"]);

        assert_eq!(
            scrubbed,
            [
                "YARN_ENABLE_NETWORK",
                "yarn_enable_network",
                "Yarn_Enable_Network",
                "git_allow_protocol"
            ]
            .map(OsString::from)
        );
    }
}
