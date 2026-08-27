//! Ambient pip configuration that can route installation away from PyPI — the surfaces a bare
//! requirements parse cannot see: `-r`/`-c` include chains, `PIP_*` routing environment
//! variables, and pip configuration files in every location pip consults.
//!
//! A requirements pin's PyPI origin is only provable when all of these are visibly clean, since
//! pip defines index options as install-wide: one `--index-url` in an included file reroutes
//! every pin. A surface that cannot be read counts as routing, never as clean — and none of
//! this can *grant* an identity, only withhold one the requirements file would otherwise claim.

use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::PathBuf;

/// Environment variables that reroute pip's installation; any of them being set at all is a
/// veto — even one naming PyPI signals index routing this module does not model further.
/// `PIP_REQUIREMENT` and `PIP_CONSTRAINT` are the environment spellings of `-r`/`-c`: they
/// splice another requirements file into every pip invocation (including cooldown's own install
/// verification), and that file can carry install-wide index options — so their mere presence
/// is routing, without following the file they name.
const ENV_ROUTING_VARS: [&str; 6] = [
    "PIP_INDEX_URL",
    "PIP_EXTRA_INDEX_URL",
    "PIP_NO_INDEX",
    "PIP_FIND_LINKS",
    "PIP_REQUIREMENT",
    "PIP_CONSTRAINT",
];

/// Whether anything ambient routes this requirements project away from PyPI: a directive in the
/// requirements tree (includes followed), a `PIP_*` routing variable, or an index option in any
/// pip configuration file.
pub(crate) fn reroutes(lockfile: &Utf8Path) -> bool {
    requirements_tree_routes_custom(lockfile)
        || env_routes(&|key| std::env::var_os(key))
        || config_files(&|key| std::env::var_os(key))
            .iter()
            .any(|path| config_file_routes(path))
}

fn env_routes(get: &dyn Fn(&str) -> Option<OsString>) -> bool {
    ENV_ROUTING_VARS.iter().any(|key| get(key).is_some())
}

/// Whether the requirements file at `path` — or any file it includes via `-r`/`-c`,
/// transitively — declares a custom index/location directive. An unreadable file (including a
/// missing include) counts as declaring one.
pub(crate) fn requirements_tree_routes_custom(path: &Utf8Path) -> bool {
    let mut visited = HashSet::new();
    tree_routes(path, &mut visited)
}

fn tree_routes(path: &Utf8Path, visited: &mut HashSet<Utf8PathBuf>) -> bool {
    // Includes are cycle-checked on the canonical path, so `a -r ./a` terminates instead of
    // growing `dir/./a` forever; a file that cannot be canonicalized cannot be read either.
    let canonical = path.canonicalize_utf8().unwrap_or_else(|_| path.to_owned());
    if !visited.insert(canonical) {
        return false; // a repeated include adds nothing new
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return true; // unseen content must not pass as clean
    };
    for raw in content.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if crate::lock::line_routes_custom(line) {
            return true;
        }
        if let Some(target) = crate::lock::line_include_target(line) {
            // pip resolves an include relative to the file containing it.
            let target = match path.parent() {
                Some(dir) => dir.join(target),
                None => Utf8PathBuf::from(target),
            };
            if tree_routes(&target, visited) {
                return true;
            }
        }
    }
    false
}

/// Whether the poetry manifest declares any `[[tool.poetry.source]]`: poetry then resolves
/// (some of) the graph from explicit sources, and whether the lock's per-package source records
/// cover a replaced *default* source is poetry-version-dependent — so every pin's origin claim
/// is withdrawn. An unreadable or unparsable manifest withdraws them too.
pub(crate) fn poetry_declares_sources(manifest: &Utf8Path) -> bool {
    let content = match std::fs::read_to_string(manifest) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    let Ok(value) = toml::from_str::<toml::Value>(&content) else {
        return true;
    };
    value
        .get("tool")
        .and_then(|tool| tool.get("poetry"))
        .is_some_and(|poetry| poetry.get("source").is_some())
}

fn config_file_routes(path: &std::path::Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(content) => config_declares_index(&content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Whether one pip ini document sets an index option. Keys are matched in dash and underscore
/// spelling under any section: pip scopes them per command (`[global]`, `[install]`), and a
/// per-section parse would only re-derive "it routes somewhere". Both `key = value` and
/// `key: value` assignments count — pip reads its files with `RawConfigParser`, which accepts
/// either delimiter.
fn config_declares_index(content: &str) -> bool {
    content.lines().any(|raw| {
        let line = raw.trim();
        if line.starts_with(['#', ';', '[']) {
            return false;
        }
        let Some((key, _)) = line.split_once(['=', ':']) else {
            return false;
        };
        let key = key.trim().to_ascii_lowercase().replace('-', "_");
        matches!(
            key.as_str(),
            "index_url" | "extra_index_url" | "no_index" | "find_links"
        )
    })
}

/// Whether one `pip config list` output — the merged *effective* configuration of the selected
/// pip executable, environment included (its `:env:` entries) — is visibly free of routing
/// options. This is the confirmation authority the static file walk below cannot be: pip's
/// site configuration lives at the interpreter's `sys.prefix`, which shims (mise, pyenv) place
/// beyond any enumerable location, so only pip itself can say what its future invocation
/// reads. Each line is `section.key=<value repr>`; a routing key anywhere — an index option,
/// or a spliced `requirement`/`constraint` file — or a line that does not parse makes the
/// output unclean, since unseen or unrecognized routing must not pass as none.
pub(crate) fn effective_config_is_clean(output: &str) -> bool {
    output.lines().all(|raw| {
        let line = raw.trim();
        if line.is_empty() {
            return true;
        }
        let Some((qualified, _)) = line.split_once('=') else {
            return false;
        };
        let Some((_, key)) = qualified.rsplit_once('.') else {
            return false;
        };
        let key = key.trim().to_ascii_lowercase().replace('-', "_");
        !matches!(
            key.as_str(),
            "index_url"
                | "extra_index_url"
                | "no_index"
                | "find_links"
                | "requirement"
                | "constraint"
        )
    })
}

/// The locations pip's precedence documentation names for configuration files:
/// `PIP_CONFIG_FILE`, the virtualenv's site file, the user files, and the global files. This
/// walk is an early, spawn-free veto only, never the proof: it cannot locate the site config
/// of an arbitrary interpreter prefix behind a shim (mise, pyenv), which is why a feed-time
/// grant is confirmed against pip's own `config list` ([`effective_config_is_clean`]).
/// Checking a location the running platform would skip only risks an extra decline.
fn config_files(get: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let ini = if cfg!(windows) { "pip.ini" } else { "pip.conf" };
    let mut files = Vec::new();
    if let Some(explicit) = get("PIP_CONFIG_FILE") {
        files.push(PathBuf::from(explicit));
    }
    if let Some(venv) = get("VIRTUAL_ENV") {
        files.push(PathBuf::from(venv).join(ini));
    }
    // pip's "site" config lives at `sys.prefix`; in a conda environment that is the env prefix
    // even though `VIRTUAL_ENV` is unset.
    if let Some(conda) = get("CONDA_PREFIX") {
        files.push(PathBuf::from(conda).join(ini));
    }
    if let Some(xdg) = get("XDG_CONFIG_HOME") {
        files.push(PathBuf::from(xdg).join("pip").join(ini));
    }
    if cfg!(windows)
        && let Some(appdata) = get("APPDATA")
    {
        files.push(PathBuf::from(appdata).join("pip").join(ini));
    }
    if let Some(home) = get("HOME").or_else(|| get("USERPROFILE")) {
        let home = PathBuf::from(home);
        files.push(home.join(".config").join("pip").join(ini));
        files.push(home.join(".pip").join(ini)); // legacy location
        if cfg!(target_os = "macos") {
            files.push(
                home.join("Library")
                    .join("Application Support")
                    .join("pip")
                    .join(ini),
            );
        }
        if cfg!(windows) {
            files.push(home.join("pip").join(ini)); // legacy location
        }
    }
    if cfg!(unix) {
        files.push(PathBuf::from("/etc/pip.conf"));
        let dirs = get("XDG_CONFIG_DIRS")
            .and_then(|value| value.into_string().ok())
            .unwrap_or_else(|| "/etc/xdg".to_string());
        files.extend(
            dirs.split(':')
                .filter(|dir| !dir.is_empty())
                .map(|dir| PathBuf::from(dir).join("pip").join("pip.conf")),
        );
    }
    if cfg!(target_os = "macos") {
        files.push(PathBuf::from("/Library/Application Support/pip/pip.conf"));
    }
    if cfg!(windows)
        && let Some(programdata) = get("PROGRAMDATA")
    {
        files.push(PathBuf::from(programdata).join("pip").join(ini));
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        (dir, root)
    }

    #[test]
    fn a_clean_include_chain_stays_provable() {
        let (_guard, root) = tmp_root();
        std::fs::write(
            root.join("requirements.txt"),
            "Django==5.0.0\n-r extra.txt\n",
        )
        .expect("write");
        std::fs::write(root.join("extra.txt"), "ms==1.0.0  # comment\n").expect("write");
        assert!(!requirements_tree_routes_custom(
            &root.join("requirements.txt")
        ));
    }

    /// pip defines index options as install-wide, so a directive buried in an included file
    /// reroutes every pin of the tree — and an include that cannot be read must count the same.
    #[test]
    fn an_included_directive_or_missing_include_routes_the_whole_tree() {
        let (_guard, root) = tmp_root();
        std::fs::write(
            root.join("requirements.txt"),
            "Django==5.0.0\n-r extra.txt\n",
        )
        .expect("write");
        std::fs::write(
            root.join("extra.txt"),
            "--index-url https://pypi.corp.example/simple\n",
        )
        .expect("write");
        assert!(requirements_tree_routes_custom(
            &root.join("requirements.txt")
        ));

        std::fs::write(
            root.join("requirements.txt"),
            "Django==5.0.0\n--requirement=absent.txt\n",
        )
        .expect("write");
        assert!(
            requirements_tree_routes_custom(&root.join("requirements.txt")),
            "unseen content must not pass as clean"
        );
    }

    #[test]
    fn an_include_cycle_terminates_clean() {
        let (_guard, root) = tmp_root();
        std::fs::write(root.join("a.txt"), "-r b.txt\nDjango==5.0.0\n").expect("write");
        std::fs::write(root.join("b.txt"), "-r ./a.txt\n").expect("write");
        assert!(!requirements_tree_routes_custom(&root.join("a.txt")));
    }

    #[test]
    fn no_index_alone_routes_custom() {
        let (_guard, root) = tmp_root();
        std::fs::write(root.join("requirements.txt"), "--no-index\nDjango==5.0.0\n")
            .expect("write");
        assert!(requirements_tree_routes_custom(
            &root.join("requirements.txt")
        ));
    }

    /// The confirmation authority: pip's own `config list` merges every file the selected
    /// interpreter reads plus the environment; a routing key anywhere — or a line the parser
    /// cannot read — is unclean, while non-routing settings pass.
    #[test]
    fn effective_config_output_is_the_confirmation_authority() {
        assert!(effective_config_is_clean(""));
        assert!(effective_config_is_clean(
            "global.timeout='60'\ninstall.progress-bar='off'\n"
        ));
        assert!(!effective_config_is_clean(
            "global.index-url='https://pypi.corp.example/simple'\n"
        ));
        assert!(!effective_config_is_clean(
            ":env:.index-url='https://pypi.corp.example/simple'\n"
        ));
        assert!(!effective_config_is_clean("install.no-index='true'\n"));
        assert!(!effective_config_is_clean(
            ":env:.requirement='/tmp/corporate-routing.txt'\n"
        ));
        assert!(
            !effective_config_is_clean("garbage output\n"),
            "unparsable output must not pass as clean"
        );
    }

    /// `PIP_REQUIREMENT`/`PIP_CONSTRAINT` splice an unseen file into every install, so they
    /// veto exactly like the index variables — presence alone, contents never inspected.
    #[test]
    fn requirement_and_constraint_env_vars_route() {
        let only = |name: &'static str| {
            move |key: &str| (key == name).then(|| OsString::from("/tmp/corporate-routing.txt"))
        };
        assert!(env_routes(&only("PIP_REQUIREMENT")));
        assert!(env_routes(&only("PIP_CONSTRAINT")));
        assert!(env_routes(&only("PIP_INDEX_URL")));
        assert!(!env_routes(&|_| None));
        assert!(!env_routes(&only("PIP_TIMEOUT")));
    }

    #[test]
    fn config_index_keys_route_in_both_spellings() {
        assert!(config_declares_index(
            "[global]\nindex-url = https://pypi.corp.example/simple\n"
        ));
        assert!(config_declares_index("[install]\nno_index = true\n"));
        assert!(config_declares_index("find-links = /wheels\n"));
        assert!(
            config_declares_index("[global]\nindex-url: https://pypi.corp.example/simple\n"),
            "RawConfigParser accepts colon assignments too"
        );
        assert!(!config_declares_index(
            "[global]\ntimeout = 60\n; index-url comment\n# find-links comment\n"
        ));
    }

    #[test]
    fn a_poetry_manifest_with_sources_withdraws_origin_claims() {
        let (_guard, root) = tmp_root();
        let manifest = root.join("pyproject.toml");
        std::fs::write(
            &manifest,
            "[tool.poetry]\nname = \"demo\"\n\n[[tool.poetry.source]]\nname = \"corp\"\nurl = \"https://pypi.corp.example/simple\"\npriority = \"primary\"\n",
        )
        .expect("write");
        assert!(poetry_declares_sources(&manifest));

        std::fs::write(&manifest, "[tool.poetry]\nname = \"demo\"\n").expect("write");
        assert!(!poetry_declares_sources(&manifest));

        assert!(
            !poetry_declares_sources(&root.join("absent.toml")),
            "no manifest, no declared sources"
        );
    }
}
