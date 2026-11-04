//! The Yarn Berry configuration that decides whether a lock's member bindings can be read without
//! running Yarn: whether any plugin is configured, and whether Yarn's `defaultProtocol` is
//! established as `npm:`.
//!
//! Yarn reads its rc file (`.yarnrc.yml`, unless `YARN_RC_FILENAME` names another) in the project
//! root, in every directory above it, and in the user's home directory.
//! A `YARN_*` environment variable overrides the setting it names, its name compared without
//! case ([Configuration.ts][configuration]).
//! cooldown reads the same files and variables, and both home file names Yarn versions read.
//! Without a home directory the home rc file is unknown, so the bindings are unreadable.
//!
//! A plugin may rewrite any descriptor or resolve it through a resolver of its own, so any
//! configured plugin makes the bindings unreadable.
//! Yarn 2 and 3 load a plugin entry's file whatever its `spec` names, so no entry is exempt.
//!
//! [configuration]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/Configuration.ts

use super::display_rel;
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};
use std::ffi::OsString;
use std::path::PathBuf;

/// The rc file name Yarn reads when `YARN_RC_FILENAME` names none.
const DEFAULT_RC_FILENAME: &str = ".yarnrc.yml";

/// The protocol a Berry lock of a protocol-less schema omits.
const NPM_PROTOCOL: &str = "npm:";

/// The process environment Yarn's configuration reads, captured once where the adapter reads a
/// workspace.
#[derive(Debug, Clone, Default)]
pub(crate) struct YarnEnvironment {
    /// The user's home directory, whose rc file Yarn reads too, or `None` when none is
    /// established ([`home_directory`]).
    pub(crate) home: Option<Utf8PathBuf>,
    /// `YARN_RC_FILENAME`: the rc file name Yarn reads in place of `.yarnrc.yml`.
    pub(crate) rc_filename: Option<String>,
    /// Every `YARN_PLUGINS` value: `;`-separated plugin files Yarn loads.
    pub(crate) plugins: Vec<String>,
    /// Every `YARN_DEFAULT_PROTOCOL` value.
    pub(crate) default_protocol: Vec<String>,
}

impl YarnEnvironment {
    /// The environment of this process.
    pub(crate) fn from_process() -> Self {
        let home = home_directory(
            cfg!(windows),
            |name: &str| std::env::var_os(name),
            std::env::home_dir,
        );
        let mut environment = YarnEnvironment {
            home,
            ..YarnEnvironment::default()
        };
        for (key, value) in std::env::vars_os() {
            let value = value.to_string_lossy().into_owned();
            match key.to_string_lossy().to_ascii_lowercase().as_str() {
                "yarn_rc_filename" => {
                    environment.rc_filename.get_or_insert(value);
                }
                "yarn_plugins" => environment.plugins.push(value),
                "yarn_default_protocol" => environment.default_protocol.push(value),
                _ => {}
            }
        }
        environment
    }

    /// An environment that sets nothing, whose home directory is the absent `root/.home`.
    #[cfg(test)]
    pub(crate) fn isolated(root: &Utf8Path) -> Self {
        YarnEnvironment {
            home: Some(root.join(".home")),
            ..YarnEnvironment::default()
        }
    }
}

/// The home directory Node's `os.homedir()` reports, where Yarn reads its home rc file.
///
/// Node reads `USERPROFILE` on Windows and `HOME` elsewhere, and asks the operating system for
/// the user's profile directory only when that variable is unset ([`uv_os_homedir`][homedir]).
/// `std::env::home_dir`, which `account` is in production, follows the same rule.
/// A variable that is set but empty, or a home that is not UTF-8, establishes no home directory.
///
/// [homedir]: https://docs.libuv.org/en/v1.x/misc.html#c.uv_os_homedir
fn home_directory(
    windows: bool,
    variable: impl Fn(&str) -> Option<OsString>,
    account: impl FnOnce() -> Option<PathBuf>,
) -> Option<Utf8PathBuf> {
    let name = if windows { "USERPROFILE" } else { "HOME" };
    let home = match variable(name) {
        Some(home) if home.is_empty() => return None,
        Some(home) => PathBuf::from(home),
        None => account()?,
    };
    Utf8PathBuf::from_path_buf(home).ok()
}

/// What Yarn's configuration around a project establishes about its member bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct YarnConfiguration {
    /// Whether every source of `defaultProtocol` Yarn reads leaves it at, or sets it to, `npm:`.
    pub(super) npm_default_protocol: bool,
}

impl YarnConfiguration {
    /// Reads the configuration Yarn applies under `root` in `environment`.
    ///
    /// # Errors
    ///
    /// Returns why the bindings cannot be read: a configured plugin, no home directory, or an rc
    /// file Yarn itself would refuse to read.
    pub(super) fn read(root: &Utf8Path, environment: &YarnEnvironment) -> Result<Self, String> {
        if let Some(plugins) = environment
            .plugins
            .iter()
            .find(|plugins| !plugins.is_empty())
        {
            return Err(format!(
                "YARN_PLUGINS loads the Yarn plugins `{plugins}`, which may rewrite how a member's dependencies resolve"
            ));
        }
        let Some(home) = &environment.home else {
            return Err(
                "no home directory is established, so the rc file Yarn reads there is unknown"
                    .to_string(),
            );
        };
        let mut npm_default_protocol = environment
            .default_protocol
            .iter()
            .all(|protocol| protocol == NPM_PROTOCOL);
        for rc_file in rc_files(root, home, environment) {
            let Some(settings) = read_settings(root, &rc_file)? else {
                continue;
            };
            if settings.get("plugins").is_some_and(configures_plugins) {
                return Err(format!(
                    "{} configures Yarn plugins, which may rewrite how a member's dependencies resolve",
                    shown(root, &rc_file)
                ));
            }
            if settings
                .get("defaultProtocol")
                .is_some_and(|protocol| protocol.as_str() != Some(NPM_PROTOCOL))
            {
                npm_default_protocol = false;
            }
        }
        Ok(YarnConfiguration {
            npm_default_protocol,
        })
    }
}

/// Every rc file Yarn reads for a project at `root`: one in `root` and in each directory above
/// it, and those in the `home` directory.
///
/// Yarn 4 reads `.yarnrc.yml` in the home directory whatever `YARN_RC_FILENAME` names, while
/// Yarn 2 and 3 read the file it names there too, so both are read without asking which Yarn
/// runs ([Configuration.ts][configuration]).
///
/// [configuration]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/Configuration.ts
fn rc_files(root: &Utf8Path, home: &Utf8Path, environment: &YarnEnvironment) -> Vec<Utf8PathBuf> {
    let filename = environment
        .rc_filename
        .as_deref()
        .unwrap_or(DEFAULT_RC_FILENAME);
    let mut files: Vec<Utf8PathBuf> = root
        .ancestors()
        .map(|directory| directory.join(filename))
        .collect();
    for home_filename in [DEFAULT_RC_FILENAME, filename] {
        let home_file = home.join(home_filename);
        if !files.contains(&home_file) {
            files.push(home_file);
        }
    }
    files
}

/// The top-level settings of the rc file at `path`, `None` when it does not exist.
///
/// An empty file sets nothing.
/// A file that cannot be read, or holds no YAML mapping, is refused, since Yarn refuses it too.
fn read_settings(root: &Utf8Path, path: &Utf8Path) -> Result<Option<Map<String, Value>>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{}: {err}", shown(root, path))),
    };
    if content.trim().is_empty() {
        return Ok(Some(Map::new()));
    }
    match serde_saphyr::from_str::<Value>(&content) {
        Ok(Value::Object(settings)) => Ok(Some(settings)),
        Ok(Value::Null) => Ok(Some(Map::new())),
        Ok(_) => Err(format!("{} is no YAML mapping", shown(root, path))),
        Err(err) => Err(format!("{}: {err}", shown(root, path))),
    }
}

/// Whether a `plugins` setting names any plugin: every value but an empty list or none does.
fn configures_plugins(plugins: &Value) -> bool {
    match plugins {
        Value::Null => false,
        Value::Array(entries) => !entries.is_empty(),
        Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Object(_) => true,
    }
}

/// `path` as a diagnostic names it: project-relative and `/`-separated inside `root`, and as it is
/// outside.
fn shown(root: &Utf8Path, path: &Utf8Path) -> String {
    path.strip_prefix(root)
        .map_or_else(|_| path.to_string(), display_rel)
}

#[cfg(test)]
mod tests {
    use super::{YarnConfiguration, YarnEnvironment, home_directory};
    use camino::Utf8PathBuf;
    use color_eyre::eyre;
    use std::ffi::OsString;
    use std::path::PathBuf;

    /// A project root nested two directories below a temporary directory, so an rc file can sit
    /// above it.
    struct Nested {
        /// Owns the temporary directory; dropping it deletes the tree, so it stays bound.
        _guard: tempfile::TempDir,
        top: Utf8PathBuf,
        root: Utf8PathBuf,
    }

    fn nested() -> eyre::Result<Nested> {
        let guard = tempfile::tempdir()?;
        let top = Utf8PathBuf::from_path_buf(guard.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let root = top.join("work").join("project");
        std::fs::create_dir_all(&root)?;
        Ok(Nested {
            _guard: guard,
            top,
            root,
        })
    }

    /// Any plugin entry of any shape, in the project, above it, or in the home directory, and any
    /// `YARN_PLUGINS` value make the bindings unreadable; an empty list does not.
    #[test]
    fn every_configured_plugin_makes_the_bindings_unreadable() -> eyre::Result<()> {
        let Nested { _guard, top, root } = nested()?;
        let home = top.join("home");
        std::fs::create_dir_all(&home)?;
        let isolated = YarnEnvironment {
            home: Some(home.clone()),
            ..YarnEnvironment::default()
        };

        // No rc file, and an empty plugin list
        assert!(YarnConfiguration::read(&root, &isolated).is_ok());
        std::fs::write(root.join(".yarnrc.yml"), "plugins: []\n")?;
        assert!(YarnConfiguration::read(&root, &isolated).is_ok());

        // An official plugin's spec exempts nothing, since Yarn 2 and 3 load its file.
        for plugins in [
            "plugins:\n  - path: .yarn/plugins/x.cjs\n    spec: \"@yarnpkg/plugin-workspace-tools\"\n",
            "plugins:\n  - .yarn/plugins/x.cjs\n",
            "plugins: .yarn/plugins/x.cjs\n",
        ] {
            std::fs::write(root.join(".yarnrc.yml"), plugins)?;
            let reason = YarnConfiguration::read(&root, &isolated)
                .err()
                .ok_or_else(|| eyre::eyre!("{plugins} configures a plugin"))?;
            assert!(
                reason.starts_with(".yarnrc.yml configures Yarn plugins"),
                "{reason}"
            );
        }
        std::fs::remove_file(root.join(".yarnrc.yml"))?;

        // Above the project, and in the home directory
        for directory in [top.join("work"), home.clone()] {
            let rc_file = directory.join(".yarnrc.yml");
            std::fs::write(&rc_file, "plugins:\n  - .yarn/plugins/x.cjs\n")?;
            let reason = YarnConfiguration::read(&root, &isolated)
                .err()
                .ok_or_else(|| eyre::eyre!("{rc_file} configures a plugin"))?;
            assert!(reason.starts_with(rc_file.as_str()), "{reason}");
            std::fs::remove_file(rc_file)?;
        }

        // The environment
        let loading = YarnEnvironment {
            plugins: vec!["/x.cjs".to_string()],
            ..isolated.clone()
        };
        assert!(YarnConfiguration::read(&root, &loading).is_err());
        let empty = YarnEnvironment {
            plugins: vec![String::new()],
            ..isolated
        };
        assert!(YarnConfiguration::read(&root, &empty).is_ok());
        Ok(())
    }

    /// `YARN_RC_FILENAME` renames the rc file Yarn reads in the project and above it, but Yarn 4
    /// still reads `.yarnrc.yml` in the home directory, and Yarn 2 and 3 the renamed file there.
    #[test]
    fn the_home_rc_file_is_read_under_either_name() -> eyre::Result<()> {
        let Nested { _guard, top, root } = nested()?;
        let home = top.join("home");
        std::fs::create_dir_all(&home)?;
        let renamed = YarnEnvironment {
            home: Some(home.clone()),
            rc_filename: Some(".custom.yml".to_string()),
            ..YarnEnvironment::default()
        };
        for home_filename in [".yarnrc.yml", ".custom.yml"] {
            let rc_file = home.join(home_filename);
            std::fs::write(&rc_file, "plugins:\n  - .yarn/plugins/x.cjs\n")?;
            let reason = YarnConfiguration::read(&root, &renamed)
                .err()
                .ok_or_else(|| eyre::eyre!("{rc_file} configures a plugin"))?;
            assert!(reason.starts_with(rc_file.as_str()), "{reason}");
            std::fs::remove_file(rc_file)?;
        }

        // The project's own `.yarnrc.yml` is the one the rename replaces.
        std::fs::write(
            root.join(".yarnrc.yml"),
            "plugins:\n  - .yarn/plugins/x.cjs\n",
        )?;
        assert!(YarnConfiguration::read(&root, &renamed).is_ok());
        Ok(())
    }

    /// The home directory is the one Node reports: `USERPROFILE` on Windows and `HOME` elsewhere,
    /// the operating system's record of the user only when that variable is unset, and none for
    /// an empty variable.
    #[test]
    fn the_home_directory_follows_the_platforms_rule() {
        let both = |name: &str| match name {
            "HOME" => Some(OsString::from("/posix-home")),
            "USERPROFILE" => Some(OsString::from("/windows-profile")),
            _ => None,
        };
        let only_home = |name: &str| (name == "HOME").then(|| OsString::from("/posix-home"));
        let empty = |_: &str| Some(OsString::new());
        let account = || Some(PathBuf::from("/account"));
        let shown = |home: Option<Utf8PathBuf>| home.map(String::from);

        // The platform's variable wins over the other platform's.
        assert_eq!(
            shown(home_directory(true, both, account)).as_deref(),
            Some("/windows-profile")
        );
        assert_eq!(
            shown(home_directory(false, both, account)).as_deref(),
            Some("/posix-home")
        );

        // Windows ignores `HOME` and asks for the user's profile directory.
        assert_eq!(
            shown(home_directory(true, only_home, account)).as_deref(),
            Some("/account")
        );

        // An empty variable, or no record of the user, establishes no home.
        assert_eq!(home_directory(false, empty, account), None);
        assert_eq!(home_directory(false, |_: &str| None, || None), None);
    }

    /// Without a home directory the rc file Yarn reads there is unknown, so the bindings are
    /// unreadable.
    #[test]
    fn no_home_directory_makes_the_bindings_unreadable() -> eyre::Result<()> {
        let Nested { _guard, root, .. } = nested()?;
        let homeless = YarnEnvironment::default();
        let reason = YarnConfiguration::read(&root, &homeless)
            .err()
            .ok_or_else(|| eyre::eyre!("no home directory is established"))?;
        assert!(
            reason.starts_with("no home directory is established"),
            "{reason}"
        );
        assert!(YarnConfiguration::read(&root, &YarnEnvironment::isolated(&root)).is_ok());
        Ok(())
    }

    /// `defaultProtocol` is established as `npm:` only where every source Yarn reads leaves it
    /// unset or sets it to `npm:`, the rc file `YARN_RC_FILENAME` names included.
    #[test]
    fn the_default_protocol_is_npm_only_where_every_source_agrees() -> eyre::Result<()> {
        let Nested { _guard, top, root } = nested()?;
        let isolated = YarnEnvironment::isolated(&root);
        let npm = |environment: &YarnEnvironment| -> eyre::Result<bool> {
            YarnConfiguration::read(&root, environment)
                .map(|configuration| configuration.npm_default_protocol)
                .map_err(|reason| eyre::eyre!(reason))
        };

        // Unset, or set to `npm:`
        assert!(npm(&isolated)?);
        std::fs::write(root.join(".yarnrc.yml"), "defaultProtocol: \"npm:\"\n")?;
        assert!(npm(&isolated)?);

        // Another protocol, in the project or above it
        std::fs::write(root.join(".yarnrc.yml"), "defaultProtocol: \"file:\"\n")?;
        assert!(!npm(&isolated)?);
        std::fs::remove_file(root.join(".yarnrc.yml"))?;
        std::fs::write(top.join(".yarnrc.yml"), "defaultProtocol: \"file:\"\n")?;
        assert!(!npm(&isolated)?);
        std::fs::remove_file(top.join(".yarnrc.yml"))?;

        // The rc file `YARN_RC_FILENAME` names, and not `.yarnrc.yml` beside it
        std::fs::write(root.join(".custom.yml"), "defaultProtocol: \"file:\"\n")?;
        let renamed = YarnEnvironment {
            rc_filename: Some(".custom.yml".to_string()),
            ..isolated.clone()
        };
        assert!(!npm(&renamed)?);
        assert!(npm(&isolated)?);

        // The environment
        let overridden = YarnEnvironment {
            default_protocol: vec!["file:".to_string()],
            ..isolated.clone()
        };
        assert!(!npm(&overridden)?);
        let explicit = YarnEnvironment {
            default_protocol: vec!["npm:".to_string()],
            ..isolated
        };
        assert!(npm(&explicit)?);
        Ok(())
    }

    /// An rc file Yarn would refuse to read leaves the bindings unreadable.
    #[test]
    fn an_unreadable_rc_file_makes_the_bindings_unreadable() -> eyre::Result<()> {
        let Nested { _guard, root, .. } = nested()?;
        let isolated = YarnEnvironment::isolated(&root);
        std::fs::write(root.join(".yarnrc.yml"), "plugins: [\n")?;
        assert!(YarnConfiguration::read(&root, &isolated).is_err());
        std::fs::write(root.join(".yarnrc.yml"), "- a list\n")?;
        assert!(YarnConfiguration::read(&root, &isolated).is_err());
        std::fs::write(root.join(".yarnrc.yml"), "")?;
        assert!(YarnConfiguration::read(&root, &isolated).is_ok());
        Ok(())
    }
}
