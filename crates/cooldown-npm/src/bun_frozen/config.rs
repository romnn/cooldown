//! The configuration that could disable Bun's install cache for the probe.
//!
//! Bun 1.4.5 writes `node_modules/.cache` into the project when a frozen dry run of a stale lock
//! runs without its cache, whatever `--cache-dir` or `BUN_INSTALL_CACHE_DIR` say.
//! The probe therefore refuses to run when any file Bun reads switches the cache off:
//!
//! - a bunfig, the global `.bunfig.toml` or the project's `bunfig.toml`, through
//!   `[install] cache = false` or `[install.cache] disable = true`;
//! - an npmrc, the project's `.npmrc` or the user's, through a `cache` value such as `false`.
//!
//! Only those settings are read; a cache setting the probe cannot establish as enabled, or a file
//! that cannot be read, leaves the cache unproven too.
//! Bun picks the user npmrc through `NPM_CONFIG_USERCONFIG` after loading the project's dotenv
//! files, so a dotenv file that may set it leaves the cache unproven as well.
//! The global bunfig lookup and `BUN_OPTIONS` read the process environment, which the probe
//! controls.

use camino::{Utf8Path, Utf8PathBuf};

/// The dotenv files Bun 1.4.5 loads from the project for an install, for any `NODE_ENV`.
const DOTENV_FILES: [&str; 8] = [
    ".env.development.local",
    ".env.production.local",
    ".env.test.local",
    ".env.local",
    ".env.development",
    ".env.production",
    ".env.test",
    ".env",
];

/// The process environment that locates Bun's user-level configuration.
#[derive(Debug, Default)]
pub(super) struct Environment {
    /// `XDG_CONFIG_HOME`, only when it is an absolute path.
    pub(super) xdg_config_home: Option<Utf8PathBuf>,
    /// The home directory: `HOME` on Unix, `USERPROFILE` on Windows.
    pub(super) home: Option<Utf8PathBuf>,
    /// The user npmrc files `NPM_CONFIG_USERCONFIG` and `npm_config_userconfig` name, relative
    /// ones against the project root.
    pub(super) userconfig: Vec<Utf8PathBuf>,
}

impl Environment {
    pub(super) fn from_process() -> Self {
        let variable = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .map(Utf8PathBuf::from)
        };
        let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        Environment {
            xdg_config_home: variable("XDG_CONFIG_HOME").filter(|path| path.is_absolute()),
            home: variable(home),
            userconfig: ["NPM_CONFIG_USERCONFIG", "npm_config_userconfig"]
                .into_iter()
                .filter_map(variable)
                .collect(),
        }
    }

    /// The global bunfig: `.bunfig.toml` in `XDG_CONFIG_HOME` when it is set, else in the home
    /// directory, whether or not the file exists.
    fn global_bunfig(&self) -> Option<Utf8PathBuf> {
        Some(
            self.xdg_config_home
                .as_ref()
                .or(self.home.as_ref())?
                .join(".bunfig.toml"),
        )
    }

    /// The user npmrc files: the configured ones, else `.npmrc` in `XDG_CONFIG_HOME` when it
    /// exists there, else in the home directory.
    fn user_npmrcs(&self, root: &Utf8Path) -> Vec<Utf8PathBuf> {
        if !self.userconfig.is_empty() {
            return self.userconfig.iter().map(|path| root.join(path)).collect();
        }
        let xdg = self
            .xdg_config_home
            .as_ref()
            .map(|directory| directory.join(".npmrc"))
            .filter(|path| path.exists());
        xdg.or_else(|| self.home.as_ref().map(|home| home.join(".npmrc")))
            .into_iter()
            .collect()
    }
}

/// Why the configuration Bun reads for the project at `root` may disable its install cache, if it
/// may.
pub(super) fn cache_disabled(root: &Utf8Path, environment: &Environment) -> Option<String> {
    let bunfigs = environment
        .global_bunfig()
        .into_iter()
        .chain([root.join("bunfig.toml")]);
    for path in bunfigs {
        let reason = match read_optional(&path) {
            Ok(None) => continue,
            Ok(Some(text)) => match toml::from_str::<toml::Table>(&text) {
                Ok(document) => bunfig_cache_veto(&document),
                Err(_) => Some("cannot be read as TOML"),
            },
            Err(_) => Some("cannot be read"),
        };
        if let Some(reason) = reason {
            return Some(format!("{path} {reason}"));
        }
    }
    let npmrcs = std::iter::once(root.join(".npmrc")).chain(environment.user_npmrcs(root));
    for path in npmrcs {
        match read_optional(&path) {
            Ok(Some(text)) if npmrc_may_disable_cache(&text) => {
                return Some(format!(
                    "{path} sets a `cache` value that may disable Bun's cache"
                ));
            }
            Ok(_) => {}
            Err(_) => return Some(format!("{path} cannot be read")),
        }
    }
    dotenv_veto(root)
}

/// The file at `path`, or `None` when nothing is there.
fn read_optional(path: &Utf8Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Why the bunfig `document` disables Bun's install cache or leaves it undetermined, if it does.
///
/// `[install] cache` is a directory, a switch, or a table whose `disable` switch turns the cache
/// off.
fn bunfig_cache_veto(document: &toml::Table) -> Option<&'static str> {
    let install = document.get("install")?;
    let Some(install) = install.as_table() else {
        return Some("sets `install` to something other than a table");
    };
    match install.get("cache") {
        None | Some(toml::Value::String(_) | toml::Value::Boolean(true)) => None,
        Some(toml::Value::Boolean(false)) => Some("sets `install.cache = false`"),
        Some(toml::Value::Table(cache)) => match cache.get("disable") {
            None | Some(toml::Value::Boolean(false)) => None,
            Some(toml::Value::Boolean(true)) => Some("sets `install.cache.disable = true`"),
            Some(_) => Some("sets `install.cache.disable` to something other than a switch"),
        },
        Some(_) => Some("sets `install.cache` to something other than a directory or a switch"),
    }
}

/// Whether an npmrc's `text` may disable Bun's cache: it has a `cache` key, or a key that could
/// expand to one, whose value is not established as enabling the cache.
///
/// Bun's ini parser ends a value at an unescaped `;` or `#`, drops surrounding quotes, and reads
/// `false` as the switch that disables the cache, while another word names a cache directory.
/// An empty value, `null`, or `undefined` is no directory either.
/// A `${...}` expansion or a backslash escape can spell any of those, so it establishes nothing.
fn npmrc_may_disable_cache(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        if line.starts_with([';', '#']) {
            return false;
        }
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        let key = unquote(key.trim());
        let key = key.strip_suffix("[]").unwrap_or(key);
        if key != "cache" && !key.contains(['$', '\\']) {
            return false;
        }
        let value = value.split([';', '#']).next().unwrap_or_default();
        let value = unquote(value.trim());
        value.is_empty()
            || value.contains(['$', '\\'])
            || matches!(value, "false" | "null" | "undefined")
    })
}

/// Why a dotenv file Bun loads in `root` may choose the user npmrc, if one may.
///
/// Any mention of `NPM_CONFIG_USERCONFIG`, in any case, counts, which covers every assignment
/// form Bun's dotenv parser accepts.
fn dotenv_veto(root: &Utf8Path) -> Option<String> {
    DOTENV_FILES.into_iter().find_map(|file| {
        let text = match read_optional(&root.join(file)) {
            Ok(Some(text)) => text,
            Ok(None) => return None,
            Err(_) => return Some(format!("{file} cannot be read")),
        };
        text.to_ascii_lowercase()
            .contains("npm_config_userconfig")
            .then(|| {
                format!("{file} may set `NPM_CONFIG_USERCONFIG`, which chooses the user npmrc")
            })
    })
}

/// `text` without one pair of surrounding double or single quotes.
fn unquote(text: &str) -> &str {
    ["\"", "'"]
        .into_iter()
        .find_map(|quote| text.strip_prefix(quote)?.strip_suffix(quote))
        .unwrap_or(text)
}
