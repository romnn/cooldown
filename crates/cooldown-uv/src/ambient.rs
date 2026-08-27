//! Ambient uv configuration that can route resolution away from PyPI — a *veto* on advisory
//! identity, complementing the per-package index URLs the lock records.
//!
//! `uv.lock` says which index served each locked package, but adopting a shortened window means
//! a *future* resolve, and that resolve honors configuration the lock predates: a project or
//! ancestor `uv.toml` (which overrides `[tool.uv]` in `pyproject.toml`), a `[tool.uv]` table in
//! the project's or an ancestor's `pyproject.toml`, the user and system `uv.toml`, and `UV_*`
//! index environment variables. Any of them declaring an index source withdraws the identity; a
//! config surface that cannot be read or parsed counts as routing — unseen content must not
//! pass as clean. Configuration can only veto here, never grant. (The adapter additionally
//! checks the manifest it actually loaded — see the identity site in `tool.rs` — which keeps
//! its typed-error path; the walk here makes the ambient answer self-sufficient.)

use camino::{Utf8Path, Utf8PathBuf};

/// Environment variables that redirect uv's index resolution; any of them being set at all is a
/// veto — even one naming PyPI signals index routing this module does not model further.
const ENV_INDEX_VARS: [&str; 6] = [
    "UV_DEFAULT_INDEX",
    "UV_INDEX",
    "UV_INDEX_URL",
    "UV_EXTRA_INDEX_URL",
    "UV_FIND_LINKS",
    "UV_NO_INDEX",
];

/// The keys — in a `uv.toml` document or a `[tool.uv]` table — that route resolution somewhere
/// other than the default index.
pub(crate) fn declares_index_keys(table: &toml::Value) -> bool {
    [
        "index",
        "index-url",
        "extra-index-url",
        "find-links",
        "no-index",
    ]
    .iter()
    .any(|key| table.get(key).is_some())
}

/// Whether ambient configuration reroutes this project's resolution: `UV_*` index variables,
/// index keys in the project, ancestor, user, or system `uv.toml`, or a `[tool.uv]` table with
/// index keys in the project's or an ancestor's `pyproject.toml`.
pub(crate) fn reroutes(root: &Utf8Path) -> bool {
    if ENV_INDEX_VARS
        .iter()
        .any(|key| std::env::var_os(key).is_some())
    {
        return true;
    }
    // uv discovers configuration — `uv.toml` *or* a `pyproject.toml` `[tool.uv]` table — in the
    // project directory or the nearest parent, and the search skips a `pyproject.toml` without
    // a `[tool.uv]` table, so an ancestor manifest can govern this project even when the
    // project's own has no such table. Checking every ancestor (not just the nearest applicable
    // file) can only decline more, which is the safe direction.
    if root
        .ancestors()
        .any(|dir| pyproject_declares_index(&dir.join("pyproject.toml")))
    {
        return true;
    }
    let mut files: Vec<Utf8PathBuf> = root.ancestors().map(|dir| dir.join("uv.toml")).collect();
    if let Some(explicit) = std::env::var_os("UV_CONFIG_FILE") {
        let Ok(path) = explicit.into_string() else {
            return true; // an uninspectable config override cannot pass as clean
        };
        files.push(Utf8PathBuf::from(path));
    }
    match user_config_dir() {
        Ok(Some(dir)) => files.push(dir.join("uv").join("uv.toml")),
        Ok(None) => {}
        Err(NonUtf8Path) => return true,
    }
    for dir in system_config_dirs() {
        files.push(dir.join("uv").join("uv.toml"));
    }
    files.iter().any(|path| config_declares_index(path))
}

/// A config location whose path exists but is not UTF-8: it cannot be inspected, so the caller
/// must treat it as routing.
struct NonUtf8Path;

/// `$XDG_CONFIG_HOME`, else `%APPDATA%` on Windows, else `~/.config` — where uv looks for its
/// user-level `uv.toml`.
fn user_config_dir() -> Result<Option<Utf8PathBuf>, NonUtf8Path> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let path = xdg.into_string().map_err(|_| NonUtf8Path)?;
        return Ok(Some(Utf8PathBuf::from(path)));
    }
    if cfg!(windows)
        && let Some(appdata) = std::env::var_os("APPDATA")
    {
        let path = appdata.into_string().map_err(|_| NonUtf8Path)?;
        return Ok(Some(Utf8PathBuf::from(path)));
    }
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        return Ok(None);
    };
    let home = home.into_string().map_err(|_| NonUtf8Path)?;
    Ok(Some(Utf8PathBuf::from(home).join(".config")))
}

/// The system config directories uv consults (`$XDG_CONFIG_DIRS`, plus `/etc/uv` via the `/etc`
/// fallback). A non-UTF-8 entry is skipped rather than vetoing: it could never have been
/// written by this module's audience, and the `/etc` fallback still gets checked.
fn system_config_dirs() -> Vec<Utf8PathBuf> {
    let mut dirs: Vec<Utf8PathBuf> = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_DIRS")
        && let Ok(paths) = xdg.into_string()
    {
        dirs.extend(
            paths
                .split(':')
                .filter(|dir| !dir.is_empty())
                .map(Utf8PathBuf::from),
        );
    }
    if cfg!(unix) {
        dirs.push(Utf8PathBuf::from("/etc"));
    }
    if cfg!(windows)
        && let Some(programdata) = std::env::var_os("PROGRAMDATA")
        && let Ok(path) = programdata.into_string()
    {
        dirs.push(Utf8PathBuf::from(path));
    }
    dirs
}

fn config_declares_index(path: &Utf8Path) -> bool {
    match cooldown_toml_util::read_toml_file::<toml::Value>(path, "uv.toml") {
        Ok(None) => false,
        Ok(Some(value)) => declares_index_keys(&value),
        // Present but unreadable or unparsable: the routing it may declare is unseen.
        Err(_) => true,
    }
}

/// Whether a `pyproject.toml` at `path` routes resolution via index keys in its `[tool.uv]`
/// table. A missing file is clean; one that exists but cannot be read or parsed counts as
/// routing, since the table it may hold is unseen.
fn pyproject_declares_index(path: &Utf8Path) -> bool {
    match cooldown_toml_util::read_toml_file::<toml::Value>(path, "pyproject.toml") {
        Ok(None) => false,
        Ok(Some(value)) => value
            .get("tool")
            .and_then(|tool| tool.get("uv"))
            .is_some_and(declares_index_keys),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toml_value(content: &str) -> toml::Value {
        toml::from_str(content).expect("toml")
    }

    #[test]
    fn index_keys_are_recognized_in_every_spelling() {
        assert!(declares_index_keys(&toml_value(
            "[[index]]\nurl = \"https://pypi.corp.example/simple\"\n"
        )));
        assert!(declares_index_keys(&toml_value(
            "index-url = \"https://pypi.corp.example/simple\"\n"
        )));
        assert!(declares_index_keys(&toml_value(
            "extra-index-url = [\"https://pypi.corp.example/simple\"]\n"
        )));
        assert!(declares_index_keys(&toml_value("no-index = true\n")));
        assert!(declares_index_keys(&toml_value(
            "find-links = [\"./wheels\"]\n"
        )));
    }

    /// Non-index configuration — the common `exclude-newer` cooldown, cache settings — must not
    /// veto: the check is per key, never "a config file exists".
    #[test]
    fn non_index_configuration_is_clean() {
        assert!(!declares_index_keys(&toml_value(
            "exclude-newer = \"7 days\"\n"
        )));
        assert!(!declares_index_keys(&toml_value("")));
    }

    #[test]
    fn a_missing_config_file_is_clean_and_a_broken_one_is_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        assert!(!config_declares_index(&root.join("uv.toml")));

        std::fs::write(root.join("uv.toml"), "not = [valid").expect("write");
        assert!(config_declares_index(&root.join("uv.toml")));
    }

    /// Only a `[tool.uv]` table with index keys routes; other tool tables, a `[tool.uv]` table
    /// of non-index settings, and a missing manifest are all clean — while a manifest that
    /// cannot be parsed is not.
    #[test]
    fn a_pyproject_routes_only_through_tool_uv_index_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let manifest = root.join("pyproject.toml");
        assert!(!pyproject_declares_index(&manifest));

        std::fs::write(
            &manifest,
            "[tool.uv]\nindex-url = \"https://pypi.corp.example/simple\"\n",
        )
        .expect("write");
        assert!(pyproject_declares_index(&manifest));

        std::fs::write(
            &manifest,
            "[tool.uv]\nexclude-newer = \"7 days\"\n\n[tool.ruff]\nline-length = 100\n",
        )
        .expect("write");
        assert!(!pyproject_declares_index(&manifest));

        std::fs::write(&manifest, "not = [valid").expect("write");
        assert!(pyproject_declares_index(&manifest));
    }
}
