//! npm-config registry routing as a *veto* on advisory identity — and, for a lock that names no
//! registry per entry, as the half of the proof the lock cannot supply.
//!
//! The grant side of advisory identity is the lock's own origin record ([`LockOrigin`]): npm's and
//! yarn's per-entry `resolved` URL says where the artifact was actually fetched from.
//! File configuration can never grant an identity (absence of an override proves nothing: npm also
//! reads global and builtin files this module cannot locate), but it can veto one: a `registry=` or
//! `@scope:registry=` override pointing away from the public registry says future installs of that
//! package resolve elsewhere, so a shortened window would adopt the fix version from a registry
//! that may shadow the public name.
//!
//! pnpm's lock says only that an entry came from *the configured* registry, so for it the manager's
//! effective configuration is asked at feed time to state which one
//! ([`overrides_from_effective_config`] under [`EffectiveRegistryQuery::Proves`]) — an unstated
//! registry withholds rather than passing as the default.
//!
//! The file layers are read in the managers' own precedence (later overrides earlier, per key): the
//! user-level `.npmrc` (`$NPM_CONFIG_USERCONFIG` or `~/.npmrc`), the project `.npmrc`, the
//! manager's YAML settings file when it has one (pnpm's `pnpm-workspace.yaml`, which pnpm reads for
//! the same keys and ranks above the `.npmrc` beside it), and `npm_config_*` environment variables.
//! A layer file that exists but cannot be read vetoes everything — unknown routing must not pass as
//! none.

use crate::lock::{EffectiveRegistryQuery, LockOrigin};
use camino::Utf8Path;
use std::collections::HashMap;

/// The merged registry overrides the npm configuration layers declare.
#[derive(Clone)]
pub(crate) struct RegistryOverrides {
    /// Some layer file exists but could not be read, so its routing is unknown for every package.
    /// Kept apart from `global_custom` because a later layer's `registry=` legitimately overrides
    /// an earlier one's, but nothing can override what was never read.
    unreadable: bool,
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
    /// The layer's file exists but could not be read or parsed.
    unreadable: bool,
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
            unreadable: true,
        }
    }
}

impl RegistryOverrides {
    /// Reads and merges the user `.npmrc`, the project `.npmrc` at `root`, the manager's YAML
    /// settings file at `root` when it has one (`settings_yaml`, pnpm's `pnpm-workspace.yaml`), and
    /// the `npm_config_*` environment, lowest precedence first.
    /// The settings file sits above the project `.npmrc` because that is how pnpm ranks them: a
    /// `registry:` in `pnpm-workspace.yaml` wins over a `registry=` in the `.npmrc` beside it, so a
    /// `.npmrc` naming the public registry must not cancel the file's reroute.
    pub(crate) fn read(root: &Utf8Path, settings_yaml: Option<&str>) -> Self {
        Self::merge([
            user_npmrc_layer(),
            npmrc_file_layer(root.join(".npmrc").as_std_path()),
            settings_yaml
                .map(|file| settings_yaml_file_layer(root.join(file).as_std_path()))
                .unwrap_or_default(),
            env_layer(std::env::vars_os()),
        ])
    }

    fn merge(layers: impl IntoIterator<Item = LayerOverrides>) -> Self {
        let mut global = None;
        let mut scopes: HashMap<String, bool> = HashMap::new();
        let mut unreadable = false;
        for layer in layers {
            if layer.global.is_some() {
                global = layer.global;
            }
            scopes.extend(layer.scopes);
            unreadable |= layer.unreadable;
        }
        RegistryOverrides {
            unreadable,
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
    /// A layer that could not be read vetoes every package whatever the readable layers say.
    pub(crate) fn reroutes(&self, package: &str) -> bool {
        self.unreadable
            || self.global_custom
            || self
                .custom_scopes
                .iter()
                .any(|scope| in_scope(package, scope))
    }
}

/// Parses `<bin> config list --json` — the *effective* configuration the manager itself computed,
/// every layer merged (builtin, global, user, project, environment) — into registry overrides.
/// This is the authority the file layers above can only approximate, so it is what advisory
/// identity is confirmed against at feed time.
/// `None` when the document cannot be parsed, or when the manager offers no such query: unknown
/// routing must not pass as none.
///
/// Under [`EffectiveRegistryQuery::Proves`] the configuration is half the proof rather than a veto,
/// so the effective `registry` must be *stated*: a document that leaves it out (or carries an
/// unreadable value) reroutes everything, where a vetoing manager's absent key merely means "no
/// override".
pub(crate) fn overrides_from_effective_config(
    json: &str,
    query: EffectiveRegistryQuery,
) -> Option<RegistryOverrides> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let object = value.as_object()?;
    let mut layer = LayerOverrides::default();
    for (key, value) in object {
        if !is_routing_key(key) {
            continue;
        }
        match value.as_str() {
            Some(value) => layer.assign(key, value),
            // A routing key whose value is not a string is routing this parser cannot read.
            None => layer.assign(key, UNREADABLE_ROUTING),
        }
    }
    match query {
        EffectiveRegistryQuery::Unavailable => return None,
        EffectiveRegistryQuery::Vetoes => {}
        EffectiveRegistryQuery::Proves => {
            if layer.global.is_none() {
                layer.global = Some(true);
            }
        }
    }
    Some(RegistryOverrides::merge([layer]))
}

/// A registry-routing configuration key: the default `registry` or a scoped `@scope:registry`.
fn is_routing_key(key: &str) -> bool {
    key == "registry" || (key.starts_with('@') && key.ends_with(":registry"))
}

/// The stand-in value for a routing key this module cannot read; it never equals the public
/// registry, so it counts as custom.
const UNREADABLE_ROUTING: &str = "\u{fffd}";

/// The advisory identity one lock entry earns from its recorded origin and the configured routing:
/// a `resolved` URL must name the public npm registry, a configured-registry entry (pnpm) is
/// granted provisionally for the feed-time query to confirm, an unrecorded origin grants nothing —
/// and in every case no configured override may route the package elsewhere.
pub(crate) fn advisory_identity(
    name: &str,
    origin: &LockOrigin,
    overrides: &RegistryOverrides,
) -> Option<String> {
    let recorded = match origin {
        LockOrigin::Unrecorded => false,
        LockOrigin::Url(url) => resolved_from_npmjs(url),
        LockOrigin::ConfiguredRegistry => true,
    };
    (recorded && !overrides.reroutes(name)).then(|| name.to_string())
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

/// The manager's YAML settings file (pnpm's `pnpm-workspace.yaml`, which pnpm reads for the same
/// routing keys as `.npmrc`) as a layer: absent means nothing, present-but-unreadable or unparsable
/// vetoes everything, like an `.npmrc` in the same state.
fn settings_yaml_file_layer(path: &std::path::Path) -> LayerOverrides {
    match std::fs::read_to_string(path) {
        Ok(content) => settings_yaml_layer(&content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LayerOverrides::default(),
        Err(_) => LayerOverrides::opaque(),
    }
}

/// Parses the routing keys out of a YAML settings document: a top-level `registry` or
/// `"@scope:registry"` mapping entry.
/// A document that is not a mapping of readable scalars where it matters is unknown routing and
/// vetoes everything.
fn settings_yaml_layer(content: &str) -> LayerOverrides {
    if content.trim().is_empty() {
        return LayerOverrides::default();
    }
    let Ok(document) = serde_saphyr::from_str::<HashMap<String, serde_json::Value>>(content) else {
        return LayerOverrides::opaque();
    };
    let mut layer = LayerOverrides::default();
    for (key, value) in &document {
        if !is_routing_key(key) {
            continue;
        }
        match value.as_str() {
            Some(value) => layer.assign(key, value),
            None => layer.assign(key, UNREADABLE_ROUTING),
        }
    }
    layer
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
            .unwrap_or_else(|_| String::from(UNREADABLE_ROUTING));
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

    fn url(url: &str) -> LockOrigin {
        LockOrigin::Url(url.to_string())
    }

    /// The grant chain end to end: a public `resolved` URL grants, everything else — a private
    /// URL, a missing record, a configured reroute — withholds.
    #[test]
    fn identity_needs_a_resolved_url_and_no_reroute() {
        let clean = overrides("");
        assert_eq!(
            advisory_identity(
                "lodash",
                &url("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz"),
                &clean
            ),
            Some("lodash".to_string())
        );
        assert_eq!(
            advisory_identity(
                "lodash",
                &url("https://npm.corp.example/lodash.tgz"),
                &clean
            ),
            None
        );
        assert_eq!(
            advisory_identity("lodash", &LockOrigin::Unrecorded, &clean),
            None,
            "no per-entry record, no identity — config absence grants nothing"
        );
        let vetoed = overrides("@corp:registry=https://npm.corp.example\n");
        assert_eq!(
            advisory_identity(
                "@corp/api",
                &url("https://registry.npmjs.org/@corp/api/-/api-1.0.0.tgz"),
                &vetoed
            ),
            None,
            "a configured reroute vetoes what the lock granted"
        );
    }

    /// A configured-registry origin (pnpm) is granted for the feed-time query to confirm, and the
    /// file layers veto it exactly like a URL-backed one.
    #[test]
    fn a_configured_registry_origin_is_granted_until_a_layer_reroutes_it() {
        let clean = overrides("");
        assert_eq!(
            advisory_identity("lodash", &LockOrigin::ConfiguredRegistry, &clean),
            Some("lodash".to_string())
        );
        let corp = overrides("registry=https://npm.corp.example/\n");
        assert_eq!(
            advisory_identity("lodash", &LockOrigin::ConfiguredRegistry, &corp),
            None,
            "a custom default registry is where every configured-registry entry came from"
        );
        let scoped = overrides("@corp:registry=https://npm.corp.example\n");
        assert_eq!(
            advisory_identity("@corp/api", &LockOrigin::ConfiguredRegistry, &scoped),
            None
        );
        assert_eq!(
            advisory_identity("lodash", &LockOrigin::ConfiguredRegistry, &scoped),
            Some("lodash".to_string()),
            "a scope override withholds only its scope"
        );
    }

    /// pnpm reads its routing keys from `pnpm-workspace.yaml` too, so that file is a layer: a
    /// readable document contributes its `registry`/`@scope:registry` entries, an empty one
    /// nothing, and one that does not parse (or hides a value the reader cannot take as a string)
    /// vetoes everything.
    /// An unreadable layer is a veto nothing above it can lift: a later `registry=` naming the
    /// public registry overrides an earlier readable layer's routing, but not routing that was
    /// never read.
    #[test]
    fn an_unreadable_layer_vetoes_through_a_later_public_registry() {
        let overrides = RegistryOverrides::merge([
            LayerOverrides::opaque(),
            LayerOverrides::parse("registry=https://registry.npmjs.org/\n"),
        ]);
        assert!(overrides.reroutes("lodash"));
        assert!(overrides.reroutes("@corp/api"));
    }

    #[test]
    fn the_settings_yaml_routes_like_an_npmrc() {
        let scoped = RegistryOverrides::merge([settings_yaml_layer(indoc::indoc! {"
            packages:
              - packages/*
            minimumReleaseAge: 20160
            '@corp:registry': https://npm.corp.example/
        "})]);
        assert!(scoped.reroutes("@corp/api"));
        assert!(!scoped.reroutes("lodash"));

        let public = RegistryOverrides::merge([settings_yaml_layer(
            "registry: https://registry.npmjs.org/\n",
        )]);
        assert!(!public.reroutes("lodash"));
        // pnpm ranks the settings file above the project `.npmrc`, so the file's reroute stands
        // when the `.npmrc` beside it names the public registry.
        let rerouted = RegistryOverrides::merge([
            LayerOverrides::parse("registry=https://registry.npmjs.org/\n"),
            settings_yaml_layer("registry: https://npm.corp.example/\n"),
        ]);
        assert!(rerouted.reroutes("lodash"));

        assert!(!RegistryOverrides::merge([settings_yaml_layer("")]).reroutes("lodash"));
        assert!(
            RegistryOverrides::merge([settings_yaml_layer("registry: [not, a, url]\n")])
                .reroutes("lodash"),
            "a routing value that is not a string cannot be read, so it vetoes"
        );
        assert!(
            RegistryOverrides::merge([settings_yaml_layer("- just\n- a list\n")])
                .reroutes("lodash"),
            "a document that is not a mapping is unknown routing"
        );
    }

    /// `npm config list --json` is the authority the file layers only approximate: routing
    /// keys are honored whatever layer set them, unreadable routing counts as routing, and a
    /// whole document that fails to parse confirms nothing.
    #[test]
    fn effective_config_json_is_the_confirmation_authority() {
        let vetoes = EffectiveRegistryQuery::Vetoes;
        let merged = overrides_from_effective_config(
            r#"{"registry":"https://registry.npmjs.org/","@corp:registry":"https://npm.corp.example/","cache":"/tmp/x"}"#,
            vetoes,
        )
        .expect("parsed");
        assert!(!merged.reroutes("lodash"));
        assert!(merged.reroutes("@corp/api"));

        let corp =
            overrides_from_effective_config(r#"{"registry":"https://npm.corp.example/"}"#, vetoes)
                .expect("parsed");
        assert!(corp.reroutes("lodash"));

        assert!(
            overrides_from_effective_config("npm WARN not json", vetoes).is_none(),
            "an unparsable document confirms nothing"
        );
        let odd = overrides_from_effective_config(r#"{"registry":42}"#, vetoes).expect("parsed");
        assert!(
            odd.reroutes("lodash"),
            "a routing value that is not a string cannot be read, so it vetoes"
        );
        assert!(
            overrides_from_effective_config(
                r#"{"registry":"https://registry.npmjs.org/"}"#,
                EffectiveRegistryQuery::Unavailable
            )
            .is_none(),
            "a manager without a query confirms nothing whatever the document says"
        );
    }

    /// Under a proving query (pnpm) the effective `registry` is the missing half of the proof, so
    /// it must be stated and public: an unstated registry withholds everything where a vetoing
    /// manager's absent key would mean "no override"; a stated public one grants, minus rerouted
    /// scopes.
    #[test]
    fn a_proving_query_must_state_the_public_registry() {
        let proves = EffectiveRegistryQuery::Proves;
        let stated = overrides_from_effective_config(
            r#"{"registry":"https://registry.npmjs.org/","@corp:registry":"https://npm.corp.example/"}"#,
            proves,
        )
        .expect("parsed");
        assert!(!stated.reroutes("lodash"));
        assert!(
            stated.reroutes("@corp/api"),
            "the scope override still withholds its scope"
        );

        let unstated =
            overrides_from_effective_config(r#"{"user-agent":"pnpm/10"}"#, proves).expect("parsed");
        assert!(
            unstated.reroutes("lodash"),
            "no stated registry, no proof — every identity is withheld"
        );
        assert!(
            !overrides_from_effective_config(
                r#"{"user-agent":"pnpm/10"}"#,
                EffectiveRegistryQuery::Vetoes
            )
            .expect("parsed")
            .reroutes("lodash"),
            "the same document merely fails to veto for a manager whose lock names the registry"
        );
        let custom =
            overrides_from_effective_config(r#"{"registry":"https://npm.corp.example/"}"#, proves)
                .expect("parsed");
        assert!(custom.reroutes("lodash"));
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
