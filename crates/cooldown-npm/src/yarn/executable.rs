//! Whether the Yarn that would run in a project reads its lock in the lock's own format.
//!
//! The lock's format and the executable are separate evidence.
//! A Classic lock run by a Berry `yarn` is migrated to the Berry format on the first install, an
//! unfinished migration cooldown must not complete on the user's behalf, and a Berry lock run by
//! a Classic `yarn` is replaced by a lock of its own, so before a resolver runs the adapter
//! checks:
//!
//! - the `packageManager` field of the root `package.json`, which Corepack and Yarn 1.22 enforce;
//! - the version `yarn --version` reports in the project, which is the executable actually
//!   reached, a `yarnPath` redirect or a Corepack shim included.
//!
//! A `yarnPath` setting found in a `.yarnrc.yml` is named in the refusal, since it is the usual
//! reason a project runs another Yarn than the one on `PATH`.

use crate::nodecmd::NodeCmd;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, Result};

/// Refuses to let Yarn run a Classic lock in `root` unless both the declared package manager and
/// the executable `driver` reaches there are Yarn Classic.
///
/// # Errors
///
/// Returns [`CoreError::System`] when `packageManager` names another package manager or a Yarn
/// Berry release, when `yarn --version` fails or names no version, or when it names a Yarn Berry
/// release; returns [`CoreError::ToolSpawn`] when `yarn` cannot be spawned.
pub(super) async fn ensure_classic_executable(driver: &NodeCmd, root: &Utf8Path) -> Result<()> {
    match declared_manager(root)? {
        Some(DeclaredManager::Yarn { spelled, major }) if major >= FIRST_BERRY_MAJOR => {
            return Err(classic_lock_refused(&format!(
                "package.json declares \"packageManager\": \"{spelled}\", a Yarn Berry (v2+) release"
            )));
        }
        Some(DeclaredManager::Other { spelled }) => {
            return Err(CoreError::System(format!(
                "package.json declares \"packageManager\": \"{spelled}\", so this project is not \
                 run by Yarn, and cooldown does not run Yarn on its yarn.lock"
            )));
        }
        Some(DeclaredManager::Yarn { .. }) | None => {}
    }
    let version = executable_version(driver, root).await?;
    if version.major >= FIRST_BERRY_MAJOR {
        let redirect = configured_yarn_path(root).map_or_else(String::new, |configured| {
            format!(
                " (`yarnPath: {yarn_path}` in {rc_file})",
                yarn_path = configured.yarn_path,
                rc_file = configured.rc_file
            )
        });
        return Err(classic_lock_refused(&format!(
            "`yarn` there runs Yarn {version}{redirect}, a Yarn Berry (v2+) release"
        )));
    }
    Ok(())
}

/// The first major version of Yarn Berry; every earlier one is Yarn Classic.
const FIRST_BERRY_MAJOR: u64 = 2;

/// Refuses to let Yarn run a Berry lock in `root` unless both the declared package manager and
/// the executable `driver` reaches there are Yarn Berry, and returns the executable's version.
///
/// Yarn Classic would read the Berry lock as no lock at all and replace it with a lock of its
/// own.
///
/// # Errors
///
/// Returns [`CoreError::System`] when `packageManager` names another package manager or a Yarn
/// Classic release, when `yarn --version` fails or names no version, or when it names a Yarn
/// Classic release; returns [`CoreError::ToolSpawn`] when `yarn` cannot be spawned.
pub(super) async fn ensure_berry_executable(
    driver: &NodeCmd,
    root: &Utf8Path,
) -> Result<semver::Version> {
    match declared_manager(root)? {
        Some(DeclaredManager::Yarn { spelled, major }) if major < FIRST_BERRY_MAJOR => {
            return Err(berry_lock_refused(&format!(
                "package.json declares \"packageManager\": \"{spelled}\", a Yarn Classic (v1) release"
            )));
        }
        Some(DeclaredManager::Other { spelled }) => {
            return Err(CoreError::System(format!(
                "package.json declares \"packageManager\": \"{spelled}\", so this project is not \
                 run by Yarn, and cooldown does not run Yarn on its yarn.lock"
            )));
        }
        Some(DeclaredManager::Yarn { .. }) | None => {}
    }
    let version = executable_version(driver, root).await?;
    if version.major < FIRST_BERRY_MAJOR {
        return Err(berry_lock_refused(&format!(
            "`yarn` there runs Yarn {version}, a Yarn Classic (v1) release"
        )));
    }
    Ok(version)
}

/// The refusal to run a Classic executable on a Berry lock, `executable` saying which one.
fn berry_lock_refused(executable: &str) -> CoreError {
    CoreError::System(format!(
        "yarn.lock is a Yarn Berry (v2+) lockfile, but {executable}, which cannot read it and would \
         replace it; run the project's Yarn Berry release here, for example through Corepack"
    ))
}

/// The refusal to run a Berry executable on a Classic lock, `executable` saying which one.
fn classic_lock_refused(executable: &str) -> CoreError {
    CoreError::System(format!(
        "yarn.lock is a Yarn Classic (v1) lockfile, but {executable}, which would migrate the lock \
         to its own format; cooldown does not migrate a lock: finish the migration with that Yarn \
         (`yarn install`) and commit the Berry lock, or run Yarn 1 here"
    ))
}

/// What the root manifest's `packageManager` field names.
#[derive(Debug, PartialEq, Eq)]
enum DeclaredManager {
    /// A Yarn release, as spelled, and its major version.
    Yarn { spelled: String, major: u64 },
    /// Another package manager, as spelled.
    Other { spelled: String },
}

/// The package manager the root manifest at `root` declares, or `None` when it declares none or
/// there is no root manifest.
///
/// # Errors
///
/// Returns [`CoreError::System`] when the manifest cannot be read or parsed, or names a Yarn
/// release whose version cannot be read, since then the executable it asks for is unknown.
fn declared_manager(root: &Utf8Path) -> Result<Option<DeclaredManager>> {
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
    Ok(Some(parse_package_manager(spelled).ok_or_else(|| {
        CoreError::System(format!(
            "package.json declares \"packageManager\": \"{spelled}\", whose Yarn version cooldown \
             cannot read"
        ))
    })?))
}

/// Reads a `packageManager` value (`yarn@4.18.1`, `yarn@1.22.22+sha512.…`), or `None` for a Yarn
/// value whose major version is unreadable.
fn parse_package_manager(spelled: &str) -> Option<DeclaredManager> {
    let (name, version) = spelled
        .split_once('@')
        .map_or((spelled, None), |(name, version)| (name, Some(version)));
    if name != "yarn" {
        return Some(DeclaredManager::Other {
            spelled: spelled.to_string(),
        });
    }
    // Corepack's integrity suffix follows a `+`, after the version itself.
    let version = version?;
    let version = version
        .split_once('+')
        .map_or(version, |(version, _)| version);
    let major = version.split('.').next()?.parse().ok()?;
    Some(DeclaredManager::Yarn {
        spelled: spelled.to_string(),
        major,
    })
}

/// The version `yarn --version` reports in `root`.
///
/// # Errors
///
/// Returns [`CoreError::ToolSpawn`] when `yarn` cannot be spawned, and [`CoreError::System`] when
/// it fails or prints no version.
async fn executable_version(driver: &NodeCmd, root: &Utf8Path) -> Result<semver::Version> {
    let stdout = driver
        .stdout(root, &["--version".to_string()])
        .await
        .map_err(|err| match err {
            CoreError::Tool { .. } => CoreError::System(format!(
                "cannot tell which Yarn would run yarn.lock, because `yarn --version` failed: {err}"
            )),
            other => other,
        })?;
    stdout
        .lines()
        .find_map(|line| crate::version::parse(line.trim()))
        .ok_or_else(|| {
            CoreError::System(format!(
                "cannot tell which Yarn would run yarn.lock, because `yarn --version` printed no \
                 version: {stdout}",
                stdout = stdout.trim()
            ))
        })
}

/// A `yarnPath` setting and the `.yarnrc.yml` that sets it.
struct ConfiguredYarnPath {
    /// The setting as written.
    yarn_path: String,
    /// The file, relative to the project root and `/`-separated (`.yarnrc.yml`, `../.yarnrc.yml`).
    rc_file: String,
}

/// The `yarnPath` the nearest `.yarnrc.yml` at or above `root` sets, which Yarn Berry and Yarn 1.22
/// both follow, or `None` when none sets one or a file cannot be read.
fn configured_yarn_path(root: &Utf8Path) -> Option<ConfiguredYarnPath> {
    /// The one setting read from a `.yarnrc.yml`.
    #[derive(Default, serde::Deserialize)]
    struct YarnRc {
        #[serde(rename = "yarnPath")]
        yarn_path: Option<String>,
    }

    let mut directory: Option<Utf8PathBuf> = Some(root.to_path_buf());
    let mut depth = 0;
    while let Some(current) = directory {
        if let Ok(content) = std::fs::read_to_string(current.join(".yarnrc.yml"))
            && let Ok(YarnRc {
                yarn_path: Some(yarn_path),
            }) = serde_saphyr::from_str::<YarnRc>(&content)
        {
            return Some(ConfiguredYarnPath {
                yarn_path,
                rc_file: format!("{}.yarnrc.yml", "../".repeat(depth)),
            });
        }
        directory = current.parent().map(Utf8Path::to_path_buf);
        depth += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{DeclaredManager, parse_package_manager};

    /// A `packageManager` value names its manager before the `@`, and a Yarn value's major
    /// version decides between Classic and Berry whatever integrity suffix follows it.
    #[test]
    fn package_manager_values_name_their_manager_and_yarn_major() {
        assert_eq!(
            parse_package_manager("yarn@4.18.1"),
            Some(DeclaredManager::Yarn {
                spelled: "yarn@4.18.1".into(),
                major: 4,
            })
        );
        assert_eq!(
            parse_package_manager("yarn@1.22.22+sha512.abc"),
            Some(DeclaredManager::Yarn {
                spelled: "yarn@1.22.22+sha512.abc".into(),
                major: 1,
            })
        );
        assert_eq!(
            parse_package_manager("pnpm@9.0.0"),
            Some(DeclaredManager::Other {
                spelled: "pnpm@9.0.0".into(),
            })
        );
        // A Yarn value without a readable major version names no executable cooldown can judge.
        assert_eq!(parse_package_manager("yarn"), None);
        assert_eq!(parse_package_manager("yarn@latest"), None);
    }
}
