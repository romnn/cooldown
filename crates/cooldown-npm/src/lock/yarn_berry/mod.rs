//! Yarn Berry (v2+) `yarn.lock` reading.
//!
//! A Berry lock is a YAML document: a `__metadata` entry naming its schema, then one entry per
//! resolved package, keyed by every descriptor (`name@range`) that resolves to it.
//! The entry's `resolution` is the locator (`name@reference`) those descriptors resolved to, and
//! the locator alone says what the entry is: a registry release, a workspace, or a package from a
//! source of its own.
//! A descriptor only connects a request to that locator, so an npm alias
//! (`foo@npm:bar@^1.0.0`) names a release of `bar`, never one of `foo`.
//!
//! The reading follows [Yarn's own lockfile reader][project]: combined keys split on commas,
//! descriptors and locators split after the package name, scopes kept, and the `npm:` protocol
//! restored on the descriptors of the older schemas that omit it.
//! It is stricter where Yarn's reader is lenient, because a lock read wrongly would report a
//! healthy graph: a descriptor bound to two different locators, an entry missing a field Yarn
//! relies on, a registry entry whose fields disagree, and a schema no released Yarn wrote are each
//! refused as [`CoreError::LockUnreadable`] rather than read as some smaller graph.
//!
//! [project]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/Project.ts

use super::yaml::{YamlEntries, parse_lock_yaml};
use super::{
    AliasOnlyVersions, LockOrigin, NameVersion, NodeLock, RegistryRelease, is_yarn_berry_lock,
};
use cooldown_core::{CoreError, Result, ToolId};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// The lockfile Berry shares with Yarn Classic.
const LOCKFILE: &str = "yarn.lock";

/// The key of the entry that carries the lock's schema rather than a package.
const METADATA_KEY: &str = "__metadata";

/// The `__metadata.version` schemas released Yarn versions write: 4 (Yarn 2 and 3.0), 5 (3.1),
/// 6 (3.2 through 3.8), 8 (4.0 through 4.13), 9 (4.14), and 10 (4.15 on).
///
/// Schema 7 was written only by Yarn 4 release candidates, and a schema above 10 by a Yarn newer
/// than any cooldown was checked against, so neither is read.
const SUPPORTED_SCHEMAS: [u64; 6] = [4, 5, 6, 8, 9, 10];

/// The newest schema whose descriptors may omit the `npm:` protocol, which Yarn's reader restores
/// on every lock up to it (`lockfileVersion <= 6` in Yarn's `Project.ts`).
const LAST_PROTOCOL_LESS_SCHEMA: u64 = 6;

/// The Yarn Berry (v2+) package manager: the YAML `yarn.lock` backed by the npm registry.
///
/// It reads the registry releases the lock resolves; member attribution, lock verification,
/// native `sync`, and mutation are not supported yet, so the `yarn` adapter
/// ([`YarnTool`](crate::YarnTool)) runs no resolver for a Berry project.
/// Berry binds peers through virtual packages its lock does not record, so the peer gate stays out
/// of its locks ([`NodeLock::PEER_CONTEXTS_MODELED`]).
pub(crate) struct YarnBerry;

impl NodeLock for YarnBerry {
    const ID: ToolId = ToolId("yarn");
    const LOCKFILE: &'static str = LOCKFILE;
    const BIN: &'static str = "yarn";
    const PEER_CONTEXTS_MODELED: bool = false;

    fn parse(content: &str) -> Result<Vec<NameVersion>> {
        Ok(BerryLock::read(content)?.registry_releases())
    }

    fn member_paths(content: &str) -> HashSet<String> {
        BerryLock::read(content)
            .map(|lock| lock.workspace_paths().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn alias_only_versions(content: &str) -> AliasOnlyVersions {
        BerryLock::read(content)
            .map(|lock| lock.alias_only_versions())
            .unwrap_or_default()
    }

    // The `yarn` adapter refuses every Berry mutation before a resolver could run, so neither
    // command is ever spawned; they name the plain install only because the trait requires one.
    fn relock_args(_before: Option<&str>) -> Vec<String> {
        vec!["install".into()]
    }

    fn build_args(_before: Option<&str>) -> Vec<String> {
        vec!["install".into()]
    }
}

/// A decoded Berry `yarn.lock`: every package entry with the descriptors that resolve to it.
#[derive(Debug)]
pub(crate) struct BerryLock {
    /// Each package entry once, in document order.
    packages: Vec<BerryPackage>,
}

/// One package entry: the locator it resolves to, what that locator is, and the descriptors bound
/// to it.
#[derive(Debug)]
struct BerryPackage {
    /// The descriptors the entry's key lists, normalized as Yarn's reader normalizes them.
    descriptors: Vec<Descriptor>,
    /// The entry's `resolution`.
    locator: Locator,
    /// What the locator's reference says about where the package comes from.
    source: BerrySource,
}

/// Where a locator's package comes from, as its reference says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BerrySource {
    /// A release of the locator's own package from the npm registry, at this version.
    ///
    /// The archive may still come from another URL (an `__archiveUrl` parameter), and a checksum
    /// or a hard link proves nothing about which registry served it, so the release carries no
    /// origin evidence.
    Registry { version: String },
    /// A workspace of the project, at this project-relative, `/`-separated path (`.` for the root).
    Workspace { path: String },
    /// A source of its own: a patch, link, portal, file, git, exec, or tarball URL reference, or any
    /// protocol another plugin adds, none of which names a registry release by itself.
    Unregistered,
}

impl BerryLock {
    /// Decodes a Berry lock body.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockUnreadable`] for a Classic lock, unparsable YAML, a missing or
    /// unsupported schema, an entry Yarn would not read or whose fields disagree, and a descriptor
    /// the lock binds to two different locators.
    pub(crate) fn read(content: &str) -> Result<Self> {
        if !is_yarn_berry_lock(content) {
            return Err(unreadable(
                "the lock is a Yarn Classic (v1) lockfile, which the Yarn Berry reader cannot read",
            ));
        }
        // Yarn's YAML parser drops a leading byte-order mark.
        let content = content.strip_prefix('\u{feff}').unwrap_or(content);
        let RawLock { entries } = parse_lock_yaml(content, LOCKFILE)?;
        let schema = schema_of(&entries)?;
        let mut packages: Vec<BerryPackage> = Vec::new();
        let mut recorded: HashMap<Locator, RecordedEntry<'_>> = HashMap::new();
        let mut bindings: HashMap<Descriptor, Locator> = HashMap::new();
        for (key, entry) in entries.entries() {
            if key == METADATA_KEY {
                continue;
            }
            let package = decode_package(key, entry, schema)?;
            for descriptor in &package.descriptors {
                match bindings.get(descriptor) {
                    Some(bound) if *bound != package.locator => {
                        return Err(unreadable(&format!(
                            "{descriptor} resolves to both {bound} and {locator}",
                            locator = package.locator
                        )));
                    }
                    Some(_) => {}
                    None => {
                        bindings.insert(descriptor.clone(), package.locator.clone());
                    }
                }
            }
            if let Some(RecordedEntry { index, fields }) = recorded.get(&package.locator) {
                if *fields != entry {
                    return Err(unreadable(&format!(
                        "{locator} is recorded twice with different fields",
                        locator = package.locator
                    )));
                }
                // Yarn's writer lists every descriptor of a locator in one key, so a second entry
                // with the same fields only adds the descriptors it names.
                if let Some(existing) = packages.get_mut(*index) {
                    for descriptor in package.descriptors {
                        if !existing.descriptors.contains(&descriptor) {
                            existing.descriptors.push(descriptor);
                        }
                    }
                }
            } else {
                recorded.insert(
                    package.locator.clone(),
                    RecordedEntry {
                        index: packages.len(),
                        fields: entry,
                    },
                );
                packages.push(package);
            }
        }
        Ok(BerryLock { packages })
    }

    /// Every registry release the lock resolves, named by the package the locator resolved, an
    /// npm alias's target included.
    ///
    /// No entry records origin evidence ([`LockOrigin::Unrecorded`]).
    pub(crate) fn registry_releases(&self) -> Vec<NameVersion> {
        self.packages
            .iter()
            .filter_map(|package| match &package.source {
                BerrySource::Registry { version } => Some(NameVersion {
                    name: package.locator.ident.full_name(),
                    version: version.clone(),
                    origin: LockOrigin::Unrecorded,
                }),
                BerrySource::Workspace { .. } | BerrySource::Unregistered => None,
            })
            .collect()
    }

    /// The registry releases that only npm aliases resolve to: no descriptor of the release's own
    /// package binds them.
    pub(crate) fn alias_only_versions(&self) -> AliasOnlyVersions {
        let releases = self
            .packages
            .iter()
            .filter_map(|package| {
                let BerrySource::Registry { version } = &package.source else {
                    return None;
                };
                let aliased_only = package
                    .descriptors
                    .iter()
                    .all(|descriptor| descriptor.ident != package.locator.ident);
                aliased_only.then(|| RegistryRelease {
                    name: package.locator.ident.full_name(),
                    version: version.clone(),
                })
            })
            .collect();
        AliasOnlyVersions { releases }
    }

    /// The path of every workspace the lock records, the root as `.`.
    pub(crate) fn workspace_paths(&self) -> impl Iterator<Item = &str> {
        self.packages
            .iter()
            .filter_map(|package| match &package.source {
                BerrySource::Workspace { path } => Some(path.as_str()),
                BerrySource::Registry { .. } | BerrySource::Unregistered => None,
            })
    }
}

/// Where [`BerryLock::read`] keeps the first entry for a locator, so a second entry for it is
/// judged by its fields.
struct RecordedEntry<'a> {
    /// The package's position in [`BerryLock::packages`].
    index: usize,
    fields: &'a RawEntry,
}

/// The schema `__metadata.version` names.
fn schema_of(entries: &YamlEntries<RawEntry>) -> Result<u64> {
    let mut metadata = entries
        .entries()
        .filter(|(key, _)| *key == METADATA_KEY)
        .map(|(_, entry)| entry);
    let Some(first) = metadata.next() else {
        return Err(unreadable("the lock records no `__metadata` entry"));
    };
    if metadata.next().is_some() {
        return Err(unreadable("the lock records `__metadata` twice"));
    }
    let Some(version) = first.version.as_deref() else {
        return Err(unreadable("`__metadata` records no `version`"));
    };
    // Yarn reads every scalar as a string and compares the version numerically.
    let schema = version
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|schema| SUPPORTED_SCHEMAS.contains(schema));
    schema.ok_or_else(|| {
        unreadable(&format!(
            "unsupported Yarn Berry lockfile version {version}; cooldown reads the versions \
             released Yarn 2 through 4.18 write (4 to 6 and 8 to 10) — re-run `yarn install` \
             with a released Yarn to rewrite the lock"
        ))
    })
}

/// Decodes the package entry `entry` keyed by `key` under lock schema `schema`.
fn decode_package(key: &str, entry: &RawEntry, schema: u64) -> Result<BerryPackage> {
    let malformed = |detail: &str| unreadable(&format!("entry \"{key}\" {detail}"));
    // Every field Yarn's reader dereferences unconditionally must be present.
    let Some(resolution) = entry.resolution.as_deref() else {
        return Err(malformed("records no `resolution`"));
    };
    let Some(locator) = Locator::parse(resolution) else {
        return Err(malformed(&format!(
            "resolves to `{resolution}`, which is no locator"
        )));
    };
    let Some(link_type) = entry.link_type.as_deref().and_then(LinkType::parse) else {
        return Err(malformed("records no `linkType` of `hard` or `soft`"));
    };
    if let Some(checksum) = entry.checksum.as_deref()
        && !is_checksum(checksum)
    {
        return Err(malformed(&format!("records the checksum `{checksum}`")));
    }
    for (name, range) in entry.dependencies.iter().flat_map(YamlEntries::entries) {
        if Ident::parse(name).is_none() || range.is_empty() {
            return Err(malformed(&format!(
                "records the dependency `{name}: {range}`"
            )));
        }
    }
    let descriptors = combined_key_descriptors(key)
        .map(|spelled| {
            let descriptor = Descriptor::parse(spelled)
                .ok_or_else(|| malformed(&format!("lists `{spelled}`, which is no descriptor")))?;
            Ok(if schema <= LAST_PROTOCOL_LESS_SCHEMA {
                descriptor.with_protocol_restored()
            } else {
                descriptor
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let source = match locator.source() {
        Some(BerrySource::Registry { version }) => {
            // A registry release is read only when its fields agree with the locator, since each
            // of them is Yarn's own record of the same npm package.
            if entry.version.as_deref() != Some(version.as_str()) {
                return Err(malformed(&format!(
                    "resolves to {locator} but records the version {recorded}",
                    recorded = entry.version.as_deref().unwrap_or("(none)")
                )));
            }
            if link_type != LinkType::Hard {
                return Err(malformed("resolves to an npm release but links it softly"));
            }
            let language = entry.language_name.as_deref().unwrap_or(DEFAULT_LANGUAGE);
            if language != DEFAULT_LANGUAGE {
                return Err(malformed(&format!(
                    "resolves to an npm release but records the language `{language}`"
                )));
            }
            BerrySource::Registry { version }
        }
        Some(source) => source,
        None => {
            return Err(malformed(&format!(
                "resolves to {locator}, which is malformed"
            )));
        }
    };
    Ok(BerryPackage {
        descriptors,
        locator,
        source,
    })
}

/// The language Yarn assigns an entry that records none (its `defaultLanguageName` setting), which
/// is the one every npm package has.
const DEFAULT_LANGUAGE: &str = "node";

/// The descriptors a combined entry key lists, split as Yarn's reader splits it (`/ *, */`): at
/// each comma, with the spaces beside the comma and no others removed.
fn combined_key_descriptors(key: &str) -> impl Iterator<Item = &str> {
    let last = key.matches(',').count();
    key.split(',').enumerate().map(move |(index, part)| {
        let part = if index > 0 {
            part.trim_start_matches(' ')
        } else {
            part
        };
        if index < last {
            part.trim_end_matches(' ')
        } else {
            part
        }
    })
}

/// How Yarn links a package: copied into place or pointing at a location of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkType {
    Hard,
    Soft,
}

impl LinkType {
    /// Reads a `linkType` field, which Yarn upper-cases before comparing.
    fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("hard") {
            Some(LinkType::Hard)
        } else if value.eq_ignore_ascii_case("soft") {
            Some(LinkType::Soft)
        } else {
            None
        }
    }
}

/// Whether `checksum` has the shape Yarn writes: a hex digest, optionally behind the cache key it
/// was computed under (`10c0/<hex>`).
fn is_checksum(checksum: &str) -> bool {
    let digest = match checksum.split_once('/') {
        Some((cache_key, digest)) if !cache_key.is_empty() => digest,
        Some(_) => return false,
        None => checksum,
    };
    !digest.is_empty() && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// A package's identity: its scope, if any, and its name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Ident {
    /// The scope without its `@`, `None` for an unscoped package.
    scope: Option<String>,
    name: String,
}

impl Ident {
    /// Parses a bare identity (`@scope/name` or `name`), as Yarn's `tryParseIdent` does.
    fn parse(text: &str) -> Option<Self> {
        let Identified { ident, rest } = Ident::split(text)?;
        rest.is_none().then_some(ident)
    }

    /// Splits `text` after the package name, the way Yarn's strict descriptor and locator
    /// patterns do (`^(?:@([^/]+?)\/)?([^@/]+?)(?:@(.+))$`).
    ///
    /// The name ends at the first `@` after the scope, so everything behind it — an alias's
    /// target name, a patch's inner descriptor, a URL — stays in the range or reference.
    fn split(text: &str) -> Option<Identified<'_>> {
        let (scope, unscoped) = match text.strip_prefix('@') {
            Some(scoped) => {
                let (scope, unscoped) = scoped.split_once('/')?;
                if scope.is_empty() {
                    return None;
                }
                (Some(scope.to_string()), unscoped)
            }
            None => (None, text),
        };
        let (name, rest) = match unscoped.split_once('@') {
            Some((name, rest)) => (name, Some(rest)),
            None => (unscoped, None),
        };
        if name.is_empty() || name.contains('/') {
            return None;
        }
        Some(Identified {
            ident: Ident {
                scope,
                name: name.to_string(),
            },
            rest,
        })
    }

    /// The package name as npm spells it.
    fn full_name(&self) -> String {
        match &self.scope {
            Some(scope) => format!("@{scope}/{name}", name = self.name),
            None => self.name.clone(),
        }
    }
}

/// A string split after its package name by [`Ident::split`].
struct Identified<'a> {
    ident: Ident,
    /// Everything after the `@` that ends the name, `None` when no `@` follows it.
    rest: Option<&'a str>,
}

/// The text after a package name in a strict descriptor or locator: non-empty, without a line
/// break (which the pattern's `.` does not match), and never Yarn's reserved `unknown`.
fn strict_tail(rest: Option<&str>) -> Option<&str> {
    rest.filter(|rest| {
        !rest.is_empty()
            && !rest.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
            && *rest != "unknown"
    })
}

/// A dependency request: a package and the range it asks for (`chalk@npm:^4.1.0`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Descriptor {
    ident: Ident,
    range: String,
}

impl Descriptor {
    /// Parses a descriptor with its range required, or `None` for one Yarn would not read.
    fn parse(text: &str) -> Option<Self> {
        let Identified { ident, rest } = Ident::split(text)?;
        let range = strict_tail(rest)?.to_string();
        Some(Descriptor { ident, range })
    }

    /// The descriptor as Yarn's reader stores it from a lock of a schema up to
    /// [`LAST_PROTOCOL_LESS_SCHEMA`].
    ///
    /// Yarn 2 and 3 wrote some descriptors without the `npm:` protocol (`resolve@^1.22.0`), which
    /// Yarn's `normalizeDependency` restores on a semver range or a tag.
    /// Their builtin-patch descriptors also wrap an inner range without it, and Yarn rewrites
    /// those with `range.replace(/^patch:[^@]+@(?!npm(:|%3A))/, "$1npm%3A")`.
    /// That replacement's group never matches, so it drops the `patch:<name>@` prefix instead of
    /// inserting the protocol after it; cooldown reproduces it, so a lookup answers what Yarn's
    /// own descriptor index answers.
    fn with_protocol_restored(self) -> Self {
        let range = if is_semver_range(&self.range) || is_tag(&self.range) {
            format!("npm:{range}", range = self.range)
        } else {
            self.range
        };
        let range = match legacy_patch_inner(&range) {
            Some(inner) => format!("npm%3A{inner}"),
            None => range,
        };
        Descriptor {
            ident: self.ident,
            range,
        }
    }
}

impl std::fmt::Display for Descriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.ident.full_name(), self.range)
    }
}

/// The text after `patch:<name>@` in a builtin-patch range whose inner range lacks the `npm:`
/// protocol (spelled `npm:` or `npm%3A`), or `None` for any other range.
///
/// `<name>` is one or more characters other than `@`, so a scoped inner descriptor never matches,
/// exactly as in Yarn's pattern.
fn legacy_patch_inner(range: &str) -> Option<&str> {
    let after_protocol = range.strip_prefix("patch:")?;
    let (name, inner) = after_protocol.split_once('@')?;
    if name.is_empty() || inner.starts_with("npm:") || inner.starts_with("npm%3A") {
        return None;
    }
    Some(inner)
}

/// Whether `range` is an npm dist-tag as Yarn's `TAG_REGEXP` (`/^(?!v)[a-z0-9._-]+$/i`) reads it.
fn is_tag(range: &str) -> bool {
    !range.is_empty()
        && !range.starts_with(['v', 'V'])
        && range
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Whether node-semver's strict `Range` constructor accepts `range`, which is the test Yarn's
/// `semverUtils.validRange` applies after refusing any range containing `:`.
///
/// Each `||` branch is empty, a hyphen range of two X-ranges, or whitespace-separated
/// comparators, each an X-range behind at most one operator, with node-semver's spaces between an
/// operator and its version allowed.
fn is_semver_range(range: &str) -> bool {
    !range.contains(':') && range.split("||").all(is_semver_branch)
}

/// Whether one `||` branch of a range is valid, as [`is_semver_range`] describes.
fn is_semver_branch(branch: &str) -> bool {
    let tokens: Vec<&str> = branch.split_whitespace().collect();
    if let [low, "-", high] = tokens.as_slice() {
        return is_x_range(low) && is_x_range(high);
    }
    let mut tokens = tokens.into_iter();
    while let Some(token) = tokens.next() {
        let mut version = strip_operator(token);
        // An operator written apart from its version (`>= 1.2.3`) applies to the next token.
        if version.is_empty() {
            let Some(next) = tokens.next() else {
                return false;
            };
            version = next;
        }
        if !is_x_range(version) {
            return false;
        }
    }
    true
}

/// The version of a comparator, behind the operator it may start with.
fn strip_operator(token: &str) -> &str {
    ["~>", ">=", "<=", "~", "^", ">", "<", "="]
        .iter()
        .find_map(|operator| token.strip_prefix(operator))
        .unwrap_or(token)
}

/// Whether `version` is node-semver's strict X-range (`XRANGEPLAIN`): any `v` and `=` prefix, then
/// one to three components, each a number without leading zeros or a wildcard (`x`, `X`, `*`),
/// with a prerelease and build only after the third.
fn is_x_range(version: &str) -> bool {
    let version = version.trim_start_matches(['v', '=']);
    let (core, build) = match version.split_once('+') {
        Some((core, build)) => (core, Some(build)),
        None => (version, None),
    };
    let (main, prerelease) = match core.split_once('-') {
        Some((main, prerelease)) => (main, Some(prerelease)),
        None => (core, None),
    };
    let components: Vec<&str> = main.split('.').collect();
    if components.len() > 3 || ((prerelease.is_some() || build.is_some()) && components.len() != 3)
    {
        return false;
    }
    let wildcard_or_number = |component: &&str| {
        matches!(*component, "x" | "X" | "*") || is_numeric_identifier(component)
    };
    if !components.iter().all(wildcard_or_number) {
        return false;
    }
    let prerelease_valid = prerelease.is_none_or(|prerelease| {
        prerelease.split('.').all(|identifier| {
            is_numeric_identifier(identifier) || is_alphanumeric_identifier(identifier)
        })
    });
    let build_valid = build.is_none_or(|build| {
        build.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    });
    prerelease_valid && build_valid
}

/// Whether `identifier` is node-semver's `NUMERICIDENTIFIER` (`0|[1-9]\d*`).
fn is_numeric_identifier(identifier: &str) -> bool {
    identifier == "0"
        || (!identifier.starts_with('0')
            && !identifier.is_empty()
            && identifier.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Whether `identifier` is node-semver's `NONNUMERICIDENTIFIER` (`\d*[a-zA-Z-][a-zA-Z0-9-]*`).
fn is_alphanumeric_identifier(identifier: &str) -> bool {
    identifier
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && identifier
            .bytes()
            .any(|byte| byte.is_ascii_alphabetic() || byte == b'-')
}

/// A resolved package: a package and the reference it resolved to (`chalk@npm:4.1.2`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Locator {
    ident: Ident,
    reference: String,
}

impl Locator {
    /// Parses a locator with its reference required, or `None` for one Yarn would not read.
    fn parse(text: &str) -> Option<Self> {
        let Identified { ident, rest } = Ident::split(text)?;
        let reference = strict_tail(rest)?.to_string();
        Some(Locator { ident, reference })
    }

    /// What the reference says about the package's source, or `None` for a registry or workspace
    /// reference Yarn could not resolve.
    ///
    /// An `npm:` reference is a registry release when its selector is a valid version, which is
    /// the test Yarn's npm resolver applies before claiming the locator; a workspace reference must
    /// name a path inside the project.
    fn source(&self) -> Option<BerrySource> {
        // Yarn writes a workspace's path verbatim after the protocol and its workspace resolver
        // reads the whole suffix back the same way, so a `#`, `::`, or `%` in a directory name is
        // part of the path, never a range's selector, binding, or escape.
        if let Some(path) = self.reference.strip_prefix(WORKSPACE_PROTOCOL) {
            return is_workspace_path(path).then(|| BerrySource::Workspace {
                path: path.to_string(),
            });
        }
        let ReferenceParts {
            protocol,
            source,
            selector,
        } = ReferenceParts::split(&self.reference);
        match protocol {
            Some("npm:") => {
                if selector.is_some() {
                    return None;
                }
                let version = percent_decode(source)?;
                crate::version::parse(&version)?;
                Some(BerrySource::Registry { version })
            }
            _ => Some(BerrySource::Unregistered),
        }
    }
}

impl std::fmt::Display for Locator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.ident.full_name(), self.reference)
    }
}

/// A reference split as Yarn's `parseRange` splits it
/// (`^([^#:]*:)?((?:(?!::)[^#])*)(?:#((?:(?!::).)*))?(?:::(.*))?$`), its parameters dropped.
#[derive(Debug, PartialEq, Eq)]
struct ReferenceParts<'a> {
    /// The protocol with its colon (`npm:`), when one leads the reference before any `#`.
    protocol: Option<&'a str>,
    /// The part before any `#` or `::`, still percent-encoded.
    source: &'a str,
    /// The part after a `#`, when there is one.
    selector: Option<&'a str>,
}

impl<'a> ReferenceParts<'a> {
    fn split(reference: &'a str) -> Self {
        let (protocol, rest) = match reference.find([':', '#']) {
            Some(colon) if reference.get(colon..=colon) == Some(":") => {
                let (protocol, rest) = reference.split_at(colon + 1);
                (Some(protocol), rest)
            }
            _ => (None, reference),
        };
        // The query string after `::` holds only bindings (`__archiveUrl`, a parent locator),
        // which name no other package.
        let body = rest.split_once("::").map_or(rest, |(body, _)| body);
        let (source, selector) = match body.split_once('#') {
            Some((source, selector)) => (source, Some(selector)),
            None => (body, None),
        };
        ReferenceParts {
            protocol,
            source,
            selector,
        }
    }
}

/// Decodes `%XX` escapes as JavaScript's `decodeURIComponent` does, or `None` for a malformed
/// escape or a result that is no UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let hex = text
                .get(index + 1..index + 3)
                .filter(|hex| hex.bytes().all(|digit| digit.is_ascii_hexdigit()))?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// The protocol of a workspace locator, whose reference is the workspace's path.
const WORKSPACE_PROTOCOL: &str = "workspace:";

/// Whether `path` is a workspace path Yarn writes: `.` for the root, or a relative, `/`-separated
/// path below it with no empty, `.`, or `..` segment.
///
/// A leading drive letter or a backslash, which would name a path outside the project on Windows,
/// is refused too; any other character, a `#`, `%`, or `:` included, is part of a directory name.
fn is_workspace_path(path: &str) -> bool {
    let drive_letter = path.split_once(':').is_some_and(|(drive, _)| {
        drive.len() == 1 && drive.bytes().all(|byte| byte.is_ascii_alphabetic())
    });
    path == "."
        || (!drive_letter
            && !path.contains('\\')
            && path
                .split('/')
                .all(|segment| !matches!(segment, "" | "." | "..")))
}

/// The lock document: every top-level entry in document order, duplicates kept.
#[derive(Default, Deserialize)]
#[serde(transparent)]
struct RawLock {
    entries: YamlEntries<RawEntry>,
}

/// One top-level entry's fields: `__metadata`'s schema, or a package's record.
///
/// Every scalar reads as its text, as Yarn's YAML schema reads it, so `version: 8` and
/// `version: "8"` are the same field.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
    version: Option<String>,
    resolution: Option<String>,
    language_name: Option<String>,
    link_type: Option<String>,
    checksum: Option<String>,
    conditions: Option<String>,
    dependencies: Option<YamlEntries<String>>,
}

/// A [`CoreError::LockUnreadable`] naming the lockfile.
fn unreadable(detail: &str) -> CoreError {
    CoreError::LockUnreadable(format!("{LOCKFILE}: {detail}"))
}

#[cfg(test)]
mod tests;
