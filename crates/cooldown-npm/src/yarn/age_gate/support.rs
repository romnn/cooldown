//! Which Yarn reads the age gate `sync` writes, and which of its settings that Yarn knows.
//!
//! Yarn 4.10.0 introduced `npmMinimalAgeGate` and `npmPreapprovedPackages`, and Yarn 4.17.0 the
//! per-scope `npmScopes.<scope>.npmMinimalAgeGate` ([index.ts][index]).
//! An earlier Yarn refuses a project rc file that sets a setting it does not know, so writing one
//! would break every `yarn` command in the project rather than gate it.
//! Two Yarns decide: the release the root manifest's `packageManager` declares, which Corepack
//! runs for everyone, and the one `yarn --version` reaches in the project on this machine, through
//! any `yarnPath` or Corepack shim.
//!
//! [index]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/plugin-npm/sources/index.ts

use crate::nodecmd::NodeCmd;
use camino::Utf8Path;
use cooldown_core::{CoreError, Result};
use semver::Version;

/// The first Yarn that reads `npmMinimalAgeGate` and `npmPreapprovedPackages`.
const FIRST_GATED: Version = Version::new(4, 10, 0);

/// The first Yarn that reads a gate per scope, and gives every configured scope one of its own.
const FIRST_SCOPED: Version = Version::new(4, 17, 0);

/// What the Yarns that read the project's rc file make of the gate settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct GateSupport {
    /// Whether one of them gates a configured scope by that scope's own `npmMinimalAgeGate`,
    /// Yarn's one-day default when the entry sets none, instead of the global gate.
    pub(super) scope_gates_apply: bool,
    /// Whether every one of them reads a scope's `npmMinimalAgeGate`, so writing one breaks none.
    pub(super) scope_gates_writable: bool,
}

/// What the Yarns that read the rc file in `root` support, or `None` when the project's Yarn
/// predates the gate, so it has no native setting to sync.
///
/// # Errors
///
/// Returns [`CoreError::System`] when `packageManager` names another package manager or an
/// unreadable Yarn version, when `yarn --version` fails or names no version, and when the
/// executable predates the gate while the declared Yarn has it; returns
/// [`CoreError::ToolSpawn`] when `yarn` cannot be spawned.
pub(super) async fn gate_support(driver: &NodeCmd, root: &Utf8Path) -> Result<Option<GateSupport>> {
    let declared = declared_yarn(root)?;
    if declared
        .as_ref()
        .is_some_and(|declared| *declared < FIRST_GATED)
    {
        return Ok(None);
    }
    let executable = executable_yarn(driver, root).await?;
    if executable < FIRST_GATED {
        return match declared {
            None => Ok(None),
            Some(declared) => Err(CoreError::System(format!(
                "package.json declares Yarn {declared}, but `yarn` here runs Yarn {executable}, which \
                 predates npmMinimalAgeGate (Yarn {FIRST_GATED}) and refuses a .yarnrc.yml that sets \
                 it, so sync wrote nothing: run the declared Yarn here, for example through Corepack"
            ))),
        };
    }
    Ok(Some(support_between(declared.as_ref(), &executable)))
}

/// What the `declared` and the `executable` Yarn, both with the gate, make of a scope's gate.
///
/// A prerelease, such as the `4.17.0-git.…` a build from source reports, may or may not have its
/// release's settings.
/// It is assumed to apply scope gates, so a looser scope is still reported, and not to read them,
/// so none is written that it could refuse.
fn support_between(declared: Option<&Version>, executable: &Version) -> GateSupport {
    let versions = [declared, Some(executable)];
    let release = |version: &Version| Version::new(version.major, version.minor, version.patch);
    GateSupport {
        scope_gates_apply: versions
            .iter()
            .flatten()
            .any(|version| release(version) >= FIRST_SCOPED),
        scope_gates_writable: versions
            .iter()
            .flatten()
            .all(|version| **version >= FIRST_SCOPED),
    }
}

/// Whether the root manifest in `root` declares a Yarn that predates the gate, which leaves
/// nothing to sync whichever Yarn runs here.
pub(in crate::yarn) fn declares_ungated_yarn(root: &Utf8Path) -> bool {
    matches!(declared_yarn(root), Ok(Some(declared)) if declared < FIRST_GATED)
}

/// The Yarn release the root manifest in `root` declares as its `packageManager`, or `None` when
/// it declares none.
fn declared_yarn(root: &Utf8Path) -> Result<Option<Version>> {
    let path = root.join("package.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(CoreError::System(format!("{path}: {err}"))),
    };
    let manifest: serde_json::Value = serde_json::from_str(&content)
        .map_err(|err| CoreError::System(format!("{path}: {err}")))?;
    let Some(spelled) = manifest
        .get("packageManager")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    parse_package_manager(spelled).map(Some)
}

/// The Yarn release a `packageManager` value names (`yarn@4.18.1`, `yarn@4.10.3+sha512.…`).
fn parse_package_manager(spelled: &str) -> Result<Version> {
    let (name, version) = spelled.split_once('@').unwrap_or((spelled, ""));
    if name != "yarn" {
        return Err(CoreError::System(format!(
            "package.json declares \"packageManager\": \"{spelled}\", so this project is not run by \
             Yarn, and sync does not write Yarn's age gate for it"
        )));
    }
    // Corepack's integrity suffix follows a `+`, after the version itself.
    let version = version
        .split_once('+')
        .map_or(version, |(version, _)| version);
    Version::parse(version).map_err(|_| {
        CoreError::System(format!(
            "package.json declares \"packageManager\": \"{spelled}\", whose Yarn version cooldown \
             cannot read, so it cannot tell whether that Yarn reads npmMinimalAgeGate"
        ))
    })
}

/// The version `yarn --version` reports in `root`.
async fn executable_yarn(driver: &NodeCmd, root: &Utf8Path) -> Result<Version> {
    let stdout = driver
        .stdout(root, &["--version".to_string()])
        .await
        .map_err(|err| match err {
            CoreError::Tool { .. } => CoreError::System(format!(
                "cannot tell whether the Yarn here reads npmMinimalAgeGate, because `yarn \
                 --version` failed: {err}"
            )),
            other => other,
        })?;
    stdout
        .lines()
        .find_map(|line| crate::version::parse(line.trim()))
        .ok_or_else(|| {
            CoreError::System(format!(
                "cannot tell whether the Yarn here reads npmMinimalAgeGate, because `yarn \
                 --version` printed no version: {stdout}",
                stdout = stdout.trim()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::{GateSupport, parse_package_manager, support_between};
    use cooldown_core::CoreError;
    use semver::Version;

    /// Scope gates apply when either Yarn may gate scopes, and are written only when both read
    /// them, a prerelease of 4.17.0 counting as one that may apply them but not as one that reads
    /// them.
    #[test]
    fn scope_gates_follow_both_yarns() -> color_eyre::eyre::Result<()> {
        let support = |declared: Option<&str>, executable: &str| -> color_eyre::eyre::Result<_> {
            let declared = declared.map(Version::parse).transpose()?;
            Ok(support_between(
                declared.as_ref(),
                &Version::parse(executable)?,
            ))
        };
        let both = GateSupport {
            scope_gates_apply: true,
            scope_gates_writable: true,
        };
        let apply_only = GateSupport {
            scope_gates_apply: true,
            scope_gates_writable: false,
        };
        let neither = GateSupport {
            scope_gates_apply: false,
            scope_gates_writable: false,
        };
        assert_eq!(support(None, "4.18.1")?, both);
        assert_eq!(support(Some("4.17.0"), "4.18.1")?, both);
        assert_eq!(support(None, "4.16.0")?, neither);
        assert_eq!(support(Some("4.16.0"), "4.18.1")?, apply_only);
        assert_eq!(support(Some("4.18.1"), "4.12.0")?, apply_only);
        assert_eq!(support(None, "4.17.0-git.20251020.hash-abc")?, apply_only);
        assert_eq!(support(None, "4.16.9-rc.1")?, neither);
        Ok(())
    }

    /// A Yarn `packageManager` value names its full version whatever integrity suffix follows,
    /// and any other value is refused.
    #[test]
    fn package_manager_values_name_their_yarn_version() -> color_eyre::eyre::Result<()> {
        assert_eq!(parse_package_manager("yarn@4.18.1")?.to_string(), "4.18.1");
        assert_eq!(
            parse_package_manager("yarn@4.10.3+sha512.abc")?.to_string(),
            "4.10.3"
        );
        for spelled in ["pnpm@9.0.0", "yarn", "yarn@latest", "yarn@4"] {
            assert!(
                matches!(parse_package_manager(spelled), Err(CoreError::System(_))),
                "{spelled}"
            );
        }
        Ok(())
    }
}
