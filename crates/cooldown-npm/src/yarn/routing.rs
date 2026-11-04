//! Which registry Yarn Berry fetches each package from, asked of Yarn when advisories are
//! matched.
//!
//! A Berry lock names no registry for a package fetched by name, so the identity read from the
//! lock is provisional until Yarn's effective configuration names a public registry for the
//! package's scope, as pnpm's is.
//! Yarn routes a package by its scope's `npmRegistryServer` in `npmScopes`, falling back to the
//! global `npmRegistryServer` when the scope sets none, and sends the `jsr` scope to
//! `npm.jsr.io` when `npmScopes` has no entry for it ([npmConfigUtils.ts][config]).
//! `yarn config get` prints both settings as Yarn resolved them, defaults included.
//!
//! The same routing decides where a future version would be fetched, so an identity the
//! routing sends elsewhere is withheld whatever the lock recorded.
//!
//! [config]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/plugin-npm/sources/npmConfigUtils.ts

use crate::nodecmd::{EnvOverride, NodeCmd};
use camino::Utf8Path;
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;

/// The scope Yarn routes to the JSR registry when `npmScopes` has no entry for it.
const JSR_SCOPE: &str = "jsr";

/// The registries Yarn fetches packages from in one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Routing {
    /// The global `npmRegistryServer`.
    default: String,
    /// Each `npmScopes` entry by its scope without `@`, with the registry it sets, if any.
    scopes: BTreeMap<String, Option<String>>,
}

impl Routing {
    /// Reads the JSON `yarn config get npmRegistryServer --json` and
    /// `yarn config get npmScopes --json` printed, or `None` for output of another shape.
    pub(super) fn parse(registry: &str, scopes: &str) -> Option<Self> {
        let default = serde_json::from_str::<String>(registry.trim()).ok()?;
        let Value::Object(entries) = serde_json::from_str::<Value>(scopes.trim()).ok()? else {
            return None;
        };
        let mut routed = BTreeMap::new();
        for (scope, entry) in entries {
            let registry = match entry.get("npmRegistryServer") {
                None | Some(Value::Null) => None,
                Some(Value::String(registry)) => Some(registry.clone()),
                Some(_) => return None,
            };
            routed.insert(scope, registry);
        }
        Some(Routing {
            default,
            scopes: routed,
        })
    }

    /// Whether Yarn fetches the package `name` from anywhere but the public npm registry.
    pub(super) fn reroutes(&self, name: &str) -> bool {
        let scope = name
            .strip_prefix('@')
            .and_then(|scoped| scoped.split_once('/'))
            .map(|(scope, _)| scope);
        let registry = match scope.map(|scope| (scope, self.scopes.get(scope))) {
            Some((_, Some(Some(registry)))) => registry,
            Some((scope, None)) if scope == JSR_SCOPE => return true,
            _ => &self.default,
        };
        !crate::npmrc::is_npmjs_registry(registry)
    }
}

/// Asks the Yarn `driver` runs in `root` for its routing, or `None` when Yarn cannot say.
///
/// The queries only read configuration, and run with Yarn and Corepack off the network and
/// telemetry off, so asking leaves the project and the network untouched.
pub(super) async fn query(driver: &NodeCmd, root: &Utf8Path) -> Option<Routing> {
    let registry = config_get(driver, root, "npmRegistryServer").await?;
    let scopes = config_get(driver, root, "npmScopes").await?;
    Routing::parse(&registry, &scopes)
}

async fn config_get(driver: &NodeCmd, root: &Utf8Path, setting: &str) -> Option<String> {
    let overrides = [
        ("YARN_ENABLE_NETWORK", "0"),
        ("COREPACK_ENABLE_NETWORK", "0"),
        ("YARN_ENABLE_TELEMETRY", "0"),
    ]
    .map(|(name, value)| EnvOverride {
        name,
        value: OsString::from(value),
    });
    let args = ["config", "get", setting, "--json"].map(ToString::to_string);
    let output = driver
        .output_overriding(root, &args, &overrides)
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Real `yarn config get` output, as Yarn 4.18.1 (and, for [`NO_SCOPES`], Yarn 2.4.3 and 3.1.1)
/// printed it in throwaway projects.
#[cfg(test)]
pub(super) mod captures {
    /// `yarn config get npmRegistryServer --json` without configuration.
    pub(in crate::yarn) const PUBLIC_REGISTRY: &str = "\"https://registry.yarnpkg.com\"\n";

    /// `yarn config get npmScopes --json` for a `corp` scope routed to a private registry, its
    /// token redacted as Yarn prints it.
    pub(in crate::yarn) const PRIVATE_SCOPE: &str = r#"{"corp":{"npmAlwaysAuth":false,"npmAuthIdent":null,"npmAuthToken":"********","npmAuditRegistry":null,"npmPublishRegistry":null,"npmRegistryServer":"https://npm.corp.example","npmMinimalAgeGate":1440}}"#;

    /// `yarn config get npmRegistryServer --json` with a mirror configured.
    pub(in crate::yarn) const MIRROR_REGISTRY: &str = "\"https://mirror.example/npm\"\n";

    /// `yarn config get npmScopes --json` beside that mirror, for a `jsr` entry that sets only
    /// `npmAlwaysAuth`: Yarn resolves the entry's registry to its own default.
    pub(in crate::yarn) const JSR_ENTRY: &str = r#"{"jsr":{"npmAlwaysAuth":true,"npmAuthIdent":null,"npmAuthToken":null,"npmAuditRegistry":null,"npmPublishRegistry":null,"npmRegistryServer":"https://registry.yarnpkg.com","npmMinimalAgeGate":1440}}"#;

    /// `yarn config get npmScopes --json` without scopes, under Yarn 2.4.3 and 3.1.1.
    pub(in crate::yarn) const NO_SCOPES: &str = "{}\n";
}

#[cfg(test)]
mod tests {
    use super::Routing;
    use super::captures::{JSR_ENTRY, MIRROR_REGISTRY, NO_SCOPES, PRIVATE_SCOPE, PUBLIC_REGISTRY};

    fn routing(registry: &str, scopes: &str) -> Routing {
        Routing::parse(registry, scopes).expect("captured output parses")
    }

    /// A public default keeps unscoped packages, and scopes without an entry; a scope routed to
    /// a private registry is withheld, and so is `jsr` without an entry, which Yarn sends to the
    /// JSR registry.
    #[test]
    fn the_scopes_registry_decides_and_jsr_reroutes_implicitly() {
        let routing = routing(PUBLIC_REGISTRY, PRIVATE_SCOPE);

        assert!(!routing.reroutes("left-pad"));
        assert!(!routing.reroutes("@types/node"));
        assert!(routing.reroutes("@corp/internal"));
        assert!(routing.reroutes("@jsr/std__path"));

        assert!(!self::routing(PUBLIC_REGISTRY, NO_SCOPES).reroutes("left-pad"));
    }

    /// A mirror reroutes every package that falls back to it, while a scope entry naming the
    /// public registry, as Yarn fills in a `jsr` entry's own default, keeps that scope.
    #[test]
    fn a_mirror_reroutes_what_falls_back_to_it() {
        let routing = routing(MIRROR_REGISTRY, JSR_ENTRY);

        assert!(routing.reroutes("left-pad"));
        assert!(routing.reroutes("@types/node"));
        assert!(!routing.reroutes("@jsr/std__path"));
    }

    /// A scope whose registry is null falls back to the default registry.
    ///
    /// The scope output is synthetic: Yarn 4.18.1 fills in every scope's registry, and older
    /// releases may print null.
    #[test]
    fn a_null_scope_registry_falls_back_to_the_default() {
        let nulled = r#"{"corp":{"npmRegistryServer":null},"jsr":{"npmRegistryServer":null}}"#;

        assert!(!routing(PUBLIC_REGISTRY, nulled).reroutes("@corp/internal"));
        assert!(!routing(PUBLIC_REGISTRY, nulled).reroutes("@jsr/std__path"));
        assert!(routing(MIRROR_REGISTRY, nulled).reroutes("@corp/internal"));
    }

    /// Output of any other shape establishes no routing, so every identity is withheld.
    #[test]
    fn output_of_another_shape_establishes_no_routing() {
        assert_eq!(Routing::parse("", NO_SCOPES), None);
        assert_eq!(Routing::parse("Usage Error: nope", NO_SCOPES), None);
        assert_eq!(Routing::parse(PUBLIC_REGISTRY, "[]"), None);
        assert_eq!(
            Routing::parse(PUBLIC_REGISTRY, r#"{"corp":{"npmRegistryServer":1}}"#),
            None
        );
    }
}
