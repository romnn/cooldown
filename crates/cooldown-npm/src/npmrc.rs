//! npm-config registry routing as a *veto* on advisory identity.
//!
//! The grant side of advisory identity is the lock's own per-entry `resolved` URL — the record
//! of where the artifact was actually fetched from. Configuration can never grant an identity
//! (absence of an override proves nothing: npm also reads global and builtin files this module
//! cannot locate), but it can veto one: a `registry=` or `@scope:registry=` override pointing
//! away from the public registry says future installs of that package resolve elsewhere, so a
//! shortened window would adopt the fix version from a registry that may shadow the public
//! name.
//!
//! Three layers are read, in npm's own precedence (later overrides earlier, per key): the
//! user-level `.npmrc` (`$NPM_CONFIG_USERCONFIG` or `~/.npmrc`), the project `.npmrc`, and
//! `npm_config_*` environment variables. A layer file that exists but cannot be read vetoes
//! everything — unknown routing must not pass as none.

use camino::Utf8Path;
use std::collections::HashMap;

/// The merged registry overrides the npm configuration layers declare.
#[derive(Clone)]
pub(crate) struct RegistryOverrides {
    /// The effective `registry=` points away from the public npm registry.
    global_custom: bool,
    /// Scopes (`@corp`) whose packages resolve from another registry.
    custom_scopes: Vec<String>,
}

/// One layer's overrides: `None`/absent where the layer says nothing, so a higher-precedence
/// layer overrides a lower one per key, like npm's own config cascade.
#[derive(Default)]
struct LayerOverrides {
    global: Option<bool>,
    scopes: HashMap<String, bool>,
}

impl LayerOverrides {
    fn parse(content: &str) -> Self {
        let mut layer = LayerOverrides::default();
        for raw in content.lines() {
            let line = raw.split([';', '#']).next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            layer.assign(key.trim(), value.trim());
        }
        layer
    }

    /// Records one `key=value` assignment; within a layer the last assignment wins, like npm's
    /// ini semantics — a scope routed away and later back to the public registry keeps its
    /// identity.
    fn assign(&mut self, key: &str, value: &str) {
        if key == "registry" {
            self.global = Some(!is_npmjs_registry(value));
        } else if let Some(scope) = key.strip_suffix(":registry")
            && scope.starts_with('@')
        {
            self.scopes
                .insert(scope.to_string(), !is_npmjs_registry(value));
        }
    }

    /// The layer a present-but-unreadable config file yields: unknown routing vetoes everything.
    fn opaque() -> Self {
        LayerOverrides {
            global: Some(true),
            scopes: HashMap::new(),
        }
    }
}

impl RegistryOverrides {
    /// Reads and merges the user `.npmrc`, the project `.npmrc` at `root`, and the
    /// `npm_config_*` environment, lowest precedence first.
    pub(crate) fn read(root: &Utf8Path) -> Self {
        Self::merge([
            user_npmrc_layer(),
            npmrc_file_layer(root.join(".npmrc").as_std_path()),
            env_layer(std::env::vars_os()),
        ])
    }

    fn merge(layers: impl IntoIterator<Item = LayerOverrides>) -> Self {
        let mut global = None;
        let mut scopes: HashMap<String, bool> = HashMap::new();
        for layer in layers {
            if layer.global.is_some() {
                global = layer.global;
            }
            scopes.extend(layer.scopes);
        }
        RegistryOverrides {
            global_custom: global.unwrap_or(false),
            custom_scopes: scopes
                .into_iter()
                .filter(|&(_, custom)| custom)
                .map(|(scope, _)| scope)
                .collect(),
        }
    }

    /// Whether the configuration routes `package` away from the public npm registry — the veto
    /// on an identity the lock's `resolved` URL would otherwise grant.
    ///
    /// Deliberately one-directional: a scope routed back to the public registry *under* a
    /// custom global still vetoes, which can only withhold a shortening, never grant one.
    pub(crate) fn reroutes(&self, package: &str) -> bool {
        self.global_custom
            || self
                .custom_scopes
                .iter()
                .any(|scope| in_scope(package, scope))
    }
}

/// Parses `npm config list --json` — the *effective* configuration npm itself computed, every
/// layer merged (builtin, global, user, project, environment) — into registry overrides. This
/// is the authority the file layers above can only approximate, so it is what advisory
/// identity is confirmed against at feed time. `None` when the document cannot be parsed:
/// unknown routing must not pass as none.
pub(crate) fn overrides_from_effective_config(json: &str) -> Option<RegistryOverrides> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let object = value.as_object()?;
    let mut layer = LayerOverrides::default();
    for (key, value) in object {
        if key != "registry" && !(key.starts_with('@') && key.ends_with(":registry")) {
            continue;
        }
        match value.as_str() {
            Some(value) => layer.assign(key, value),
            // A routing key whose value is not a string is routing this parser cannot read.
            None => layer.assign(key, "\u{fffd}"),
        }
    }
    Some(RegistryOverrides::merge([layer]))
}

/// The advisory identity one lock entry earns: its `resolved` URL must name the public npm
/// registry (positive origin evidence — no URL, no identity), and no configured override may
/// route the package elsewhere.
pub(crate) fn advisory_identity(
    name: &str,
    resolved: Option<&str>,
    overrides: &RegistryOverrides,
) -> Option<String> {
    resolved
        .filter(|url| resolved_from_npmjs(url))
        .filter(|_| !overrides.reroutes(name))
        .map(|_| name.to_string())
}

/// Whether a lock entry's `resolved` URL names the public npm registry — the positive origin
/// evidence advisory identity requires. The integrity hash pins the artifact's content to what
/// that registry served at lock time, so even a later mirror fetch delivers the same bytes.
fn resolved_from_npmjs(url: &str) -> bool {
    let url = url.to_ascii_lowercase();
    [
        "https://registry.npmjs.org/",
        "https://registry.yarnpkg.com/",
    ]
    .iter()
    .any(|registry| url.starts_with(registry))
}

fn npmrc_file_layer(path: &std::path::Path) -> LayerOverrides {
    match std::fs::read_to_string(path) {
        Ok(content) => LayerOverrides::parse(&content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LayerOverrides::default(),
        Err(_) => LayerOverrides::opaque(),
    }
}

/// The user-level `.npmrc`: `$NPM_CONFIG_USERCONFIG` when set (unreadable or non-UTF-8 →
/// opaque), else `~/.npmrc`, else nothing.
fn user_npmrc_layer() -> LayerOverrides {
    for key in ["NPM_CONFIG_USERCONFIG", "npm_config_userconfig"] {
        if let Some(path) = std::env::var_os(key) {
            return match path.into_string() {
                Ok(path) => npmrc_file_layer(std::path::Path::new(&path)),
                Err(_) => LayerOverrides::opaque(),
            };
        }
    }
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        return LayerOverrides::default();
    };
    npmrc_file_layer(&std::path::PathBuf::from(home).join(".npmrc"))
}

/// The `npm_config_*` environment as a layer: npm reads its config keys case-insensitively
/// from the environment, so `NPM_CONFIG_REGISTRY` and `npm_config_@corp:registry` both route.
/// A non-UTF-8 value is unknown routing and counts as custom.
fn env_layer(
    vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> LayerOverrides {
    let mut layer = LayerOverrides::default();
    for (key, value) in vars {
        let key = key.to_string_lossy().to_ascii_lowercase();
        let Some(key) = key.strip_prefix("npm_config_") else {
            continue;
        };
        if key != "registry" && !(key.starts_with('@') && key.ends_with(":registry")) {
            continue;
        }
        let value = value
            .into_string()
            .unwrap_or_else(|_| String::from("\u{fffd}"));
        layer.assign(key, &value);
    }
    layer
}

/// Whether `package` (`@scope/name`) belongs to `scope` (`@scope`).
fn in_scope(package: &str, scope: &str) -> bool {
    package
        .strip_prefix(scope)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// The public npm registry; `registry.yarnpkg.com` is its long-standing alias.
fn is_npmjs_registry(url: &str) -> bool {
    let url = url.trim_end_matches('/');
    url.eq_ignore_ascii_case("https://registry.npmjs.org")
        || url.eq_ignore_ascii_case("https://registry.yarnpkg.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(content: &str) -> RegistryOverrides {
        RegistryOverrides::merge([LayerOverrides::parse(content)])
    }

    #[test]
    fn no_npmrc_declares_no_overrides() {
        let overrides = overrides("");
        assert!(!overrides.reroutes("lodash"));
        assert!(!overrides.reroutes("@corp/api"));
    }

    #[test]
    fn a_global_registry_override_reroutes_every_package() {
        let overrides = overrides("registry=https://npm.corp.example/\n");
        assert!(overrides.reroutes("lodash"));
        assert!(overrides.reroutes("@corp/api"));
    }

    #[test]
    fn an_explicit_public_registry_does_not_veto() {
        let overrides = overrides("registry=https://registry.npmjs.org/\n");
        assert!(!overrides.reroutes("lodash"));
    }

    #[test]
    fn a_scope_override_vetoes_exactly_that_scope() {
        let overrides = overrides("@corp:registry=https://npm.corp.example\n; comment\n");
        assert!(overrides.reroutes("@corp/api"));
        assert!(!overrides.reroutes("@corporate/api"), "prefix, not scope");
        assert!(!overrides.reroutes("lodash"));
    }

    #[test]
    fn the_last_assignment_wins() {
        let overrides = overrides(
            "@corp:registry=https://npm.corp.example\n@corp:registry=https://registry.npmjs.org\n",
        );
        assert!(!overrides.reroutes("@corp/api"));
    }

    /// The project layer overrides the user layer per key, like npm's cascade; keys the project
    /// does not set keep the user layer's routing.
    #[test]
    fn a_higher_layer_overrides_per_key() {
        let user = LayerOverrides::parse(
            "registry=https://npm.corp.example\n@tools:registry=https://npm.corp.example\n",
        );
        let project = LayerOverrides::parse("registry=https://registry.npmjs.org\n");
        let merged = RegistryOverrides::merge([user, project]);
        assert!(!merged.reroutes("lodash"), "the project routes back");
        assert!(merged.reroutes("@tools/cli"), "the user scope veto stands");
    }

    #[test]
    fn the_environment_layer_routes_like_a_file() {
        let vars = [
            ("NPM_CONFIG_REGISTRY", "https://npm.corp.example"),
            ("HOME", "/home/user"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()));
        let merged = RegistryOverrides::merge([env_layer(vars)]);
        assert!(merged.reroutes("lodash"));

        let scoped = [("npm_config_@corp:registry", "https://npm.corp.example")]
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()));
        let merged = RegistryOverrides::merge([env_layer(scoped)]);
        assert!(merged.reroutes("@corp/api"));
        assert!(!merged.reroutes("lodash"));
    }

    #[test]
    fn an_unreadable_layer_vetoes_everything() {
        let merged = RegistryOverrides::merge([LayerOverrides::opaque()]);
        assert!(merged.reroutes("lodash"));
    }

    /// The grant chain end to end: a public `resolved` URL grants, everything else — a private
    /// URL, a missing record, a configured reroute — withholds.
    #[test]
    fn identity_needs_a_resolved_url_and_no_reroute() {
        let clean = overrides("");
        assert_eq!(
            advisory_identity(
                "lodash",
                Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz"),
                &clean
            ),
            Some("lodash".to_string())
        );
        assert_eq!(
            advisory_identity(
                "lodash",
                Some("https://npm.corp.example/lodash.tgz"),
                &clean
            ),
            None
        );
        assert_eq!(
            advisory_identity("lodash", None, &clean),
            None,
            "no per-entry record, no identity — config absence grants nothing"
        );
        let vetoed = overrides("@corp:registry=https://npm.corp.example\n");
        assert_eq!(
            advisory_identity(
                "@corp/api",
                Some("https://registry.npmjs.org/@corp/api/-/api-1.0.0.tgz"),
                &vetoed
            ),
            None,
            "a configured reroute vetoes what the lock granted"
        );
    }

    /// `npm config list --json` is the authority the file layers only approximate: routing
    /// keys are honored whatever layer set them, unreadable routing counts as routing, and a
    /// whole document that fails to parse confirms nothing.
    #[test]
    fn effective_config_json_is_the_confirmation_authority() {
        let merged = overrides_from_effective_config(
            r#"{"registry":"https://registry.npmjs.org/","@corp:registry":"https://npm.corp.example/","cache":"/tmp/x"}"#,
        )
        .expect("parsed");
        assert!(!merged.reroutes("lodash"));
        assert!(merged.reroutes("@corp/api"));

        let corp = overrides_from_effective_config(r#"{"registry":"https://npm.corp.example/"}"#)
            .expect("parsed");
        assert!(corp.reroutes("lodash"));

        assert!(
            overrides_from_effective_config("npm WARN not json").is_none(),
            "an unparsable document confirms nothing"
        );
        let odd = overrides_from_effective_config(r#"{"registry":42}"#).expect("parsed");
        assert!(
            odd.reroutes("lodash"),
            "a routing value that is not a string cannot be read, so it vetoes"
        );
    }

    #[test]
    fn resolved_urls_prove_only_the_public_registry() {
        assert!(resolved_from_npmjs(
            "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz"
        ));
        assert!(resolved_from_npmjs(
            "https://registry.yarnpkg.com/@babel/core/-/core-7.1.0.tgz"
        ));
        assert!(!resolved_from_npmjs(
            "https://npm.corp.example/lodash/-/lodash-4.17.21.tgz"
        ));
        assert!(
            !resolved_from_npmjs("https://registry.npmjs.org.evil.example/lodash.tgz"),
            "the host boundary is part of the prefix"
        );
        assert!(!resolved_from_npmjs("git+https://github.com/x/y.git"));
    }
}
