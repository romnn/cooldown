//! Yarn member attribution through the workspace manifests' declarations.
//!
//! [`YarnBindings`] holds what each workspace member's declarations bind, for Yarn Classic, read
//! here, and for Yarn Berry, read by the Berry lock's own attribution.
//!
//! A Classic `yarn.lock` records no workspace member: it keys each resolved entry by every
//! selector (`name@range`) that resolves to it.
//! A member's direct dependency is therefore attributed by the selector Yarn requests for it,
//! read from the member's own manifest ([`WorkspaceManifests`]) and looked up as an exact
//! descriptor key.
//! No satisfying-version scan, range equivalence, or newest copy ever stands in for that key:
//! a copy that merely satisfies the member's range may belong to another selector entirely.
//!
//! The lock is read with a port of [Yarn 1.22.22's lockfile parser][parse], so quoted and
//! escaped keys, grouped selectors, comments, and line endings read exactly as Yarn reads them,
//! and anything that parser rejects attributes nothing.
//!
//! The lock's flat package graph is read through the same grammar ([`graph`]).
//!
//! [parse]: https://github.com/yarnpkg/yarn/blob/v1.22.22/src/lockfile/parse.js

use super::{
    FixedBinding, FixedOwner, MemberIndex, MemberResolution, is_registry_specifier,
    is_yarn_berry_lock,
};
use crate::manifest::workspaces::{FieldSpecifier, WorkspaceManifests};
use crate::version::is_exact_npm_specifier;
use std::collections::{BTreeMap, HashMap, HashSet};

pub(super) mod graph;

/// The deepest indentation level the lock reader descends to.
///
/// Yarn's own lock nests three levels (an entry's `dependencies`), so a deeper document is no lock
/// Yarn wrote, and the bound keeps a crafted one from exhausting the stack.
const MAX_DEPTH: usize = 16;

/// What a Yarn lock binds each workspace member's declared dependencies to.
///
/// A Yarn Berry lock is read through each member's descriptors, joined to the lock's own record of
/// the member (see the Berry reader's attribution).
/// A Yarn Classic lock is read through the selector Yarn requests for each declaration, as follows.
///
/// A member's selector for a name is the one Yarn's manifest normalization keeps: the first
/// declaration in `optionalDependencies`, `dependencies`, `devDependencies` order whose specifier
/// is neither empty nor `*`, else the first declaration's.
/// It names the copy for every install field of that name, so an exact pin hidden in a second
/// field is judged against the copy the first field's range binds.
///
/// The copy is the entry the lock records once under that exact descriptor, at a version of the
/// name itself; a `file:` or `link:` selector is looked up under the root-relative path Yarn
/// rewrites it to.
/// It is a registry copy, which an edit of the declaration moves, only on proof: a registry
/// selector the root `resolutions` cannot rewrite, under a name no workspace package carries, with
/// a `resolved` URL that is absent or a registry tarball of that version.
/// Any other identified copy is still the member's, but fixed in place ([`FixedBinding`]), and a
/// selector whose copy the lock does not identify binds none.
pub struct YarnBindings {
    /// Member path (the root as `.`) → each declared dependency by name.
    members: BTreeMap<String, BTreeMap<String, YarnDependency>>,
}

/// One dependency a member's manifest declares.
pub(super) struct YarnDependency {
    /// The constraint of each install field declaring it, `None` for one that is no string, as
    /// the exact-pin check reads it: Classic's specifier, or Berry's specifier with a same-name
    /// `npm:` reference unwrapped.
    pub(super) install: Vec<Option<String>>,
    pub(super) binding: YarnBinding,
}

/// What a member's declared dependency binds to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum YarnBinding {
    /// A registry copy of the dependency itself, at this version, which an edit of the declaration
    /// moves.
    Registry(String),
    /// The copy at this version the member's descriptor names, which no edit of the declaration is
    /// proven to move.
    Fixed {
        version: String,
        reason: FixedBinding,
    },
    /// The descriptor names another package, an npm alias, so no copy of the name is bound.
    Foreign,
    /// The descriptor binds a workspace or a package from a source of its own, which a Berry lock
    /// records as no registry release, so no copy of the name is bound.
    Local,
    /// The member declares the name only as a peer, which Yarn never installs for it.
    PeerOnly,
    /// The lock identifies no copy for the member's selector: a descriptor it records ambiguously,
    /// malformed, or not at all, or a selector Yarn spells from a value that is no string.
    ///
    /// Only Yarn Classic binds this: the declaration owns nothing, and other copies of the name
    /// keep their owners.
    Unresolved,
    /// A Berry declaration whose binding cannot be proven, so no copy of the name may be credited
    /// to any member ([`MemberIndex`]'s unattributed names).
    Unproven(Unproven),
}

/// Why a Yarn Berry declaration's binding cannot be proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Unproven {
    /// A specifier is no string, or the dependency's name is no package name.
    Malformed,
    /// The member's install fields declare the name with different specifiers.
    ConflictingFields,
    /// A `catalog:` reference, which Yarn resolves through the catalogs of its configuration.
    Catalog,
    /// A `patch:` reference, which binds a patched package rather than the release it wraps.
    Patch,
    /// The root `resolutions` field may rewrite the name's descriptor.
    Resolutions,
    /// The lock records the declaration without a protocol and Yarn's `defaultProtocol` is not
    /// established as `npm:`, so the descriptor it resolved cannot be told.
    DefaultProtocol,
    /// The lock records another descriptor for the member's declaration, or none, or records one
    /// the manifest no longer declares.
    Stale,
    /// No entry of the lock lists the member's descriptor.
    Unjoined,
    /// The lock records no workspace at the member's path.
    NotLocked,
    /// The lock records a workspace whose manifest the snapshot holds none of.
    Unlisted,
}

impl Unproven {
    /// Why the binding cannot be proven, as a diagnostic phrases it.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Unproven::Malformed => "its declaration is malformed",
            Unproven::ConflictingFields => "its install fields declare different ranges",
            Unproven::Catalog => "it is a `catalog:` reference",
            Unproven::Patch => "it is a `patch:` reference",
            Unproven::Resolutions => "the root `resolutions` field may rewrite it",
            Unproven::DefaultProtocol => {
                "the lock omits its protocol and Yarn's `defaultProtocol` is not established as `npm:`"
            }
            Unproven::Stale => "yarn.lock records another declaration of it",
            Unproven::Unjoined => "yarn.lock resolves no entry for it",
            Unproven::NotLocked => "yarn.lock records no workspace for the member",
            Unproven::Unlisted => "the member's manifest could not be read",
        }
    }
}

/// The members whose declarations of one name cannot be proven, each with why
/// ([`YarnBindings::unattributed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnattributedName {
    /// The dependency name.
    pub name: String,
    /// Each unproven declaration, sorted by member.
    pub declarations: Vec<UnprovenDeclaration>,
}

/// One member's unproven declaration of a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnprovenDeclaration {
    /// The member's path (`.` for the root).
    pub member: String,
    /// Why its binding cannot be proven.
    pub reason: Unproven,
}

impl YarnBindings {
    /// The bindings of every member's declared dependencies, by member path and name.
    pub(super) fn from_members(
        members: BTreeMap<String, BTreeMap<String, YarnDependency>>,
    ) -> Self {
        YarnBindings { members }
    }

    /// Reads the bindings of `content` through `workspace`, or `None` when the snapshot is not
    /// complete or the body is no Classic lock Yarn would read.
    pub(crate) fn read(content: &str, workspace: &WorkspaceManifests) -> Option<Self> {
        if !workspace.is_complete() {
            return None;
        }
        let lock = ClassicLock::read(content)?;
        let siblings = workspace.sibling_names();
        let evidence = BindingEvidence {
            lock: &lock,
            workspace,
            siblings: &siblings,
        };
        let members = workspace
            .members()
            .map(|(path, manifest)| {
                let dependencies = manifest
                    .declarations
                    .iter()
                    .map(|(name, fields)| {
                        let install = fields
                            .iter()
                            .filter(|declaration| declaration.field != "peerDependencies")
                            .map(|declaration| declaration.specifier.clone())
                            .collect();
                        let binding = evidence.binding(path, name, fields);
                        (name.clone(), YarnDependency { install, binding })
                    })
                    .collect();
                (path.to_string(), dependencies)
            })
            .collect();
        Some(YarnBindings { members })
    }

    /// What member `member` binds `name` to, as the exact-pin guard and the landing check judge
    /// it.
    ///
    /// [`MemberResolution::Missing`] means the member's manifest declares no such dependency in
    /// any field.
    /// Only a registry copy resolves.
    /// A Berry declaration whose binding cannot be proven is [`MemberResolution::Unproven`].
    /// A member the workspace does not list, a peer-only declaration, an alias, a local or
    /// workspace binding, a copy fixed in place, and a Classic selector the lock does not identify
    /// are [`MemberResolution::Unknown`].
    #[must_use]
    pub fn resolution(&self, member: &str, name: &str) -> MemberResolution<'_> {
        let Some(dependencies) = self.members.get(member) else {
            return MemberResolution::Unknown;
        };
        match dependencies.get(name).map(|dependency| &dependency.binding) {
            None => MemberResolution::Missing,
            Some(YarnBinding::Registry(version)) => MemberResolution::Resolved(version),
            Some(YarnBinding::Unproven(_)) => MemberResolution::Unproven,
            Some(
                YarnBinding::Fixed { .. }
                | YarnBinding::Foreign
                | YarnBinding::Local
                | YarnBinding::PeerOnly
                | YarnBinding::Unresolved,
            ) => MemberResolution::Unknown,
        }
    }

    /// The version of the copy member `member`'s selector for `name` identifies, a registry copy
    /// or one fixed in place, or `None` when it identifies none.
    #[must_use]
    pub fn bound_version(&self, member: &str, name: &str) -> Option<&str> {
        match &self.members.get(member)?.get(name)?.binding {
            YarnBinding::Registry(version) | YarnBinding::Fixed { version, .. } => Some(version),
            YarnBinding::Foreign
            | YarnBinding::Local
            | YarnBinding::PeerOnly
            | YarnBinding::Unresolved
            | YarnBinding::Unproven(_) => None,
        }
    }

    /// Every name some member declares with a binding that cannot be proven, sorted, each with
    /// those members and why.
    ///
    /// Only a Berry lock yields any.
    #[must_use]
    pub fn unattributed(&self) -> Vec<UnattributedName> {
        let mut names: BTreeMap<&str, Vec<UnprovenDeclaration>> = BTreeMap::new();
        for (member, dependencies) in &self.members {
            for (name, dependency) in dependencies {
                if let YarnBinding::Unproven(reason) = dependency.binding {
                    names.entry(name).or_default().push(UnprovenDeclaration {
                        member: member.clone(),
                        reason,
                    });
                }
            }
        }
        names
            .into_iter()
            .map(|(name, declarations)| UnattributedName {
                name: name.to_string(),
                declarations,
            })
            .collect()
    }

    /// The [`MemberIndex`] of every member's declarations, the members at `excluded` paths left
    /// out.
    ///
    /// Each member owns the one copy its selector identifies, as a declared copy, like npm's
    /// per-member ownership; no importer record or split evidence is manufactured.
    /// A copy fixed in place keeps its member as a direct owner that may not edit it
    /// ([`MemberIndex::fixed_owners`]), so it neither pins nor bounds the copy.
    /// A copy is exact-pinned when every install declaration of every registry owner is exact.
    /// An alias, a local or workspace binding, a peer-only declaration, and an unidentified copy
    /// own nothing, though the declaration still enters the name inventory, so no other copy of
    /// the name is credited to it.
    ///
    /// A name any included member declares with an unproven binding is unattributed: no copy of it
    /// is credited to any member, its proven owners included, since the unproven declaration may
    /// bind any of them.
    pub(crate) fn member_index(&self, excluded: &HashSet<String>) -> MemberIndex {
        let included = || {
            self.members
                .iter()
                .filter(|(path, _)| !excluded.contains(*path))
        };
        let mut unattributed: HashMap<String, Vec<String>> = HashMap::new();
        for (path, dependencies) in included() {
            for (name, dependency) in dependencies {
                if matches!(dependency.binding, YarnBinding::Unproven(_)) {
                    unattributed
                        .entry(name.clone())
                        .or_default()
                        .push(path.clone());
                }
            }
        }
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        let mut declared_copies: HashMap<(String, String), Vec<String>> = HashMap::new();
        let mut fixed_copies: HashMap<(String, String), Vec<FixedOwner>> = HashMap::new();
        let mut declared: HashMap<(String, String), usize> = HashMap::new();
        let mut exact: HashMap<(String, String), usize> = HashMap::new();
        for (path, dependencies) in included() {
            for (name, YarnDependency { install, binding }) in dependencies {
                by_name.entry(name.clone()).or_default().push(path.clone());
                if unattributed.contains_key(name) {
                    continue;
                }
                let version = match binding {
                    YarnBinding::Registry(version) => version,
                    YarnBinding::Fixed { version, reason } => {
                        let copy = (name.clone(), version.clone());
                        declared_copies
                            .entry(copy.clone())
                            .or_default()
                            .push(path.clone());
                        fixed_copies.entry(copy).or_default().push(FixedOwner {
                            member: path.clone(),
                            reason: *reason,
                        });
                        continue;
                    }
                    YarnBinding::Foreign
                    | YarnBinding::Local
                    | YarnBinding::PeerOnly
                    | YarnBinding::Unresolved
                    | YarnBinding::Unproven(_) => {
                        continue;
                    }
                };
                let copy = (name.clone(), version.clone());
                declared_copies
                    .entry(copy.clone())
                    .or_default()
                    .push(path.clone());
                *declared.entry(copy.clone()).or_default() += install.len();
                *exact.entry(copy).or_default() += install
                    .iter()
                    .filter(|specifier| specifier.as_deref().is_some_and(is_exact_npm_specifier))
                    .count();
            }
        }
        let exact_version = declared
            .into_iter()
            .filter(|(copy, count)| exact.get(copy) == Some(count))
            .map(|(copy, _)| copy)
            .collect();
        MemberIndex {
            by_name,
            declared_copies,
            fixed_copies,
            exact_version,
            unattributed,
            authoritative: true,
            ..MemberIndex::default()
        }
    }
}

/// What one member's binding is judged against: the lock and the workspace around the member.
struct BindingEvidence<'a> {
    lock: &'a ClassicLock,
    workspace: &'a WorkspaceManifests,
    /// The package names of the members other than the root.
    siblings: &'a HashSet<&'a str>,
}

impl BindingEvidence<'_> {
    /// What member `member` declaring `name` in `fields` binds it to.
    fn binding(&self, member: &str, name: &str, fields: &[FieldSpecifier]) -> YarnBinding {
        let install: Vec<&FieldSpecifier> = fields
            .iter()
            .filter(|declaration| declaration.field != "peerDependencies")
            .collect();
        if install.is_empty() {
            return YarnBinding::PeerOnly;
        }
        let Some(selector) = effective_selector(&install) else {
            return YarnBinding::Unresolved;
        };
        if selector.starts_with("npm:") {
            return YarnBinding::Foreign;
        }
        // Yarn looks a dependency up under its bare name before the full selector, the spelling a
        // `--save` writes, so such a key may serve this declaration instead.
        if self.lock.descriptors.contains_key(name) {
            return YarnBinding::Unresolved;
        }
        let local = local_path_selector(member, selector);
        let descriptor = format!("{name}@{}", local.as_deref().unwrap_or(selector));
        let Some(Some(LockedEntry {
            version,
            resolved,
            name: recorded,
        })) = self.lock.descriptors.get(&descriptor)
        else {
            return YarnBinding::Unresolved;
        };
        // A field holding no string proves nothing the entry should say, so it leaves the copy
        // unidentified before any reason could credit the member with it.
        let Field::Text(version) = version else {
            return YarnBinding::Unresolved;
        };
        if crate::version::parse(version).is_none() {
            return YarnBinding::Unresolved;
        }
        let resolved = match resolved {
            Field::Absent => None,
            Field::Text(url) => Some(url.as_str()),
            Field::Malformed => return YarnBinding::Unresolved,
        };
        match recorded {
            Field::Absent => {}
            Field::Text(recorded) if recorded == name => {}
            Field::Text(_) => return YarnBinding::Foreign,
            Field::Malformed => return YarnBinding::Unresolved,
        }
        // The first reason that keeps an edit of the declaration from moving the copy wins.
        // A resolution rewrites the selector Yarn resolves, and a workspace package of the name
        // may be linked instead of the copy, so neither is proven to follow a widened range.
        let reason = if local.is_some() {
            Some(FixedBinding::LocalPath)
        } else if !is_registry_specifier(selector) {
            Some(FixedBinding::OwnSource)
        } else if self.workspace.resolution_may_rewrite(name) {
            Some(FixedBinding::Resolutions)
        } else if self.siblings.contains(name) {
            Some(FixedBinding::WorkspaceName)
        } else {
            match resolved {
                None => None,
                Some(url) if is_registry_tarball(name, version, url) => None,
                Some(_) => Some(FixedBinding::UnrecognizedOrigin),
            }
        };
        match reason {
            Some(reason) => YarnBinding::Fixed {
                version: version.clone(),
                reason,
            },
            None => YarnBinding::Registry(version.clone()),
        }
    }
}

/// The selector Yarn's manifest normalization keeps for one name's install declarations, given in
/// `optionalDependencies`, `dependencies`, `devDependencies` order: the first specifier that is
/// neither empty nor `*`, else the first one.
///
/// `None` when a declaration is no string, whose selector Yarn spells from a value it never
/// validates.
fn effective_selector<'a>(install: &[&'a FieldSpecifier]) -> Option<&'a str> {
    let specifiers: Vec<&str> = install
        .iter()
        .map(|declaration| declaration.specifier.as_deref())
        .collect::<Option<_>>()?;
    specifiers
        .iter()
        .copied()
        .find(|specifier| !specifier.is_empty() && *specifier != "*")
        .or_else(|| specifiers.first().copied())
}

/// The spelling Yarn records a `file:` or `link:` selector of the member at `member` under, or
/// `None` for any other selector.
///
/// Yarn's manifest normalization re-bases the path from the member's directory onto the root and
/// keeps a `./` the author wrote ([`resolveRelative`][relative]), so `file:../../local` in
/// `packages/a` is recorded as `file:local`.
/// An absolute path has no root-relative spelling cooldown can derive, so it is `None` too and
/// its descriptor stays unidentified.
///
/// [relative]: https://github.com/yarnpkg/yarn/blob/v1.22.22/src/util/normalize-manifest/resolve-relative.js
fn local_path_selector(member: &str, selector: &str) -> Option<String> {
    let (prefix, path) = ["file:", "link:"]
        .iter()
        .find_map(|prefix| selector.strip_prefix(prefix).map(|path| (*prefix, path)))?;
    if path.starts_with('/') {
        return None;
    }
    let base = if member == "." { "" } else { member };
    let mut segments: Vec<&str> = Vec::new();
    let mut parents = 0;
    for segment in base.split('/').chain(path.split('/')) {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    parents += 1;
                }
            }
            _ => segments.push(segment),
        }
    }
    let mut relative: Vec<&str> = std::iter::repeat_n("..", parents).collect();
    relative.extend(segments);
    let relative = if relative.is_empty() {
        ".".to_string()
    } else {
        relative.join("/")
    };
    let keeps_dot = path == "." || path.starts_with("./");
    let relative = if relative != "." && keeps_dot && !relative.starts_with("../") {
        format!("./{relative}")
    } else {
        relative
    };
    Some(format!("{prefix}{relative}"))
}

/// Whether `resolved` is a registry tarball of `name` at `version`: an `http(s)` URL naming its
/// host, whose path, its query and fragment aside, ends in the registry's
/// `<name>/-/<basename>-<version>.tgz` layout, a scope's `/` written as itself or `%2f`.
///
/// The layout is the npm registry API's, which the public registry and the usual mirrors serve
/// alike, so it identifies the release where an `http(s)` URL alone proves nothing: a remote
/// tarball, a git archive, or a forked build is served under paths of its own.
/// Only the parsed path counts, so a layout spelled in a query or a fragment, such as a download
/// endpoint's `?file=` argument, proves nothing, while Yarn's `#` checksum fragment and a mirror's
/// `?token=` leave a registry path intact.
fn is_registry_tarball(name: &str, version: &str, resolved: &str) -> bool {
    let Some(authority) = resolved
        .strip_prefix("https://")
        .or_else(|| resolved.strip_prefix("http://"))
    else {
        return false;
    };
    // URL parsing skips any further slashes after an `http(s)` scheme and would read the first
    // path segment as the host, so the host must follow the scheme's own `//` directly.
    if authority.starts_with(['/', '\\']) {
        return false;
    }
    let Ok(url) = url::Url::parse(resolved) else {
        return false;
    };
    if url.host_str().is_none_or(str::is_empty) {
        return false;
    }
    let basename = name.rsplit('/').next().unwrap_or(name);
    let Some(package) = url
        .path()
        .strip_suffix(&format!("/-/{basename}-{version}.tgz"))
    else {
        return false;
    };
    [
        name.to_string(),
        name.replacen('/', "%2f", 1),
        name.replacen('/', "%2F", 1),
    ]
    .iter()
    .any(|spelled| package.ends_with(&format!("/{spelled}")))
}

/// The descriptor keys of a Classic lock and the entry each resolves to.
struct ClassicLock {
    /// Descriptor → its entry, `None` for a descriptor recorded with differing entries or with a
    /// plain value in place of an entry, which no single entry answers.
    descriptors: HashMap<String, Option<LockedEntry>>,
}

/// The fields of one lock entry that attribution reads.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LockedEntry {
    /// The resolved `version`.
    version: Field,
    /// The `resolved` URL.
    resolved: Field,
    /// The package `name`, which Yarn records only where it differs from the descriptor's.
    name: Field,
}

/// One string field of a lock entry, a value Yarn never writes kept apart from an absent one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    Absent,
    Text(String),
    /// A number, a boolean, or a nested object, which proves nothing the field should say.
    Malformed,
}

impl ClassicLock {
    /// Reads the descriptors of a Classic lock body, or `None` where Yarn's own reader would not
    /// read it as one ([`read_items`]).
    fn read(content: &str) -> Option<Self> {
        let items = read_items(content)?;
        let mut descriptors: HashMap<String, Option<LockedEntry>> = HashMap::new();
        for Item { keys, value } in items {
            let entry = match value {
                Value::Object(fields) => Some(locked_entry(&fields)),
                Value::Text(_) | Value::Other => None,
            };
            for key in keys {
                descriptors
                    .entry(key)
                    .and_modify(|recorded| {
                        if *recorded != entry {
                            *recorded = None;
                        }
                    })
                    .or_insert_with(|| entry.clone());
            }
        }
        Some(ClassicLock { descriptors })
    }
}

/// The top-level items of a Classic lock body, or `None` where Yarn's own reader would not read it
/// as one: a Berry lock, a merge conflict, a newer lockfile version, or any grammar error.
///
/// Like Yarn, a leading byte-order mark is dropped and `\r\n` line endings read as `\n`.
fn read_items(content: &str) -> Option<Vec<Item>> {
    if is_yarn_berry_lock(content) {
        return None;
    }
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let content = content.replace("\r\n", "\n");
    if content.contains("<<<<<<<") && content.contains("=======") && content.contains(">>>>>>>") {
        return None;
    }
    let tokens = tokenize(&content)?;
    Parser {
        tokens,
        position: 0,
    }
    .parse_object(0)
}

/// The attribution fields of one entry's object, a repeated field's last value winning as in
/// Yarn's reader.
fn locked_entry(fields: &[Item]) -> LockedEntry {
    let mut entry = LockedEntry {
        version: Field::Absent,
        resolved: Field::Absent,
        name: Field::Absent,
    };
    for Item { keys, value } in fields {
        let text = match value {
            Value::Text(text) => Field::Text(text.clone()),
            Value::Object(_) | Value::Other => Field::Malformed,
        };
        for key in keys {
            match key.as_str() {
                "version" => entry.version.clone_from(&text),
                "resolved" => entry.resolved.clone_from(&text),
                "name" => entry.name.clone_from(&text),
                _ => {}
            }
        }
    }
    entry
}

/// One token of Yarn's lockfile grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Newline,
    /// Leading spaces after a newline, in units of two.
    Indent(usize),
    /// A quoted string, decoded, or an unquoted run.
    Text(String),
    Number,
    Boolean,
    Colon,
    Comma,
    Eof,
}

/// The token past the end of the stream, which the parser reads as the end.
static END: Token = Token::Eof;

/// Splits `input` into Yarn's lockfile tokens, comments dropped, or `None` where Yarn's tokenizer
/// yields an invalid token, rejects an odd indentation, or meets a lockfile version above 1.
///
/// The cases run in Yarn's order: a newline (one or two characters swallowed, as Yarn's `chop`
/// does), a comment, a space, a quoted string, digits, `true`, `false`, `:`, `,`, and an unquoted
/// run starting with a letter, `/`, `.`, or `-` that ends at `:`, a space, a newline, or `,`.
fn tokenize(input: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut rest = input;
    let mut after_newline = false;
    while let Some(first) = rest.chars().next() {
        let second = rest.chars().nth(1);
        let consumed = match first {
            '\n' | '\r' => {
                tokens.push(Token::Newline);
                1 + usize::from(second == Some('\n'))
            }
            '#' => {
                let comment = rest
                    .find('\n')
                    .map_or(rest, |end| rest.get(..end).unwrap_or(rest));
                // Yarn refuses a lockfile version above its own, which is 1.
                let version = comment
                    .get(1..)
                    .map(str::trim)
                    .and_then(|comment| comment.strip_prefix("yarn lockfile v"))
                    .filter(|version| {
                        !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit())
                    });
                if version
                    .is_some_and(|version| !matches!(version.trim_start_matches('0'), "" | "1"))
                {
                    return None;
                }
                comment.len()
            }
            ' ' if after_newline => {
                let spaces = rest.len() - rest.trim_start_matches(' ').len();
                if spaces % 2 == 1 {
                    return None;
                }
                tokens.push(Token::Indent(spaces / 2));
                spaces
            }
            ' ' => 1,
            '"' => {
                let length = quoted_length(rest);
                let quoted = rest.get(..length)?;
                tokens.push(Token::Text(serde_json::from_str(quoted).ok()?));
                length
            }
            '0'..='9' => {
                tokens.push(Token::Number);
                rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len()
            }
            _ if rest.starts_with("true") => {
                tokens.push(Token::Boolean);
                4
            }
            _ if rest.starts_with("false") => {
                tokens.push(Token::Boolean);
                5
            }
            ':' => {
                tokens.push(Token::Colon);
                1
            }
            ',' => {
                tokens.push(Token::Comma);
                1
            }
            _ if first.is_ascii_alphabetic() || matches!(first, '/' | '.' | '-') => {
                let length = rest.find([':', ' ', '\n', '\r', ',']).unwrap_or(rest.len());
                tokens.push(Token::Text(rest.get(..length)?.to_string()));
                length
            }
            _ => return None,
        };
        after_newline = first == '\n' || (first == '\r' && second == Some('\n'));
        rest = rest.get(consumed..)?;
    }
    tokens.push(Token::Eof);
    Some(tokens)
}

/// The byte length of the quoted string `rest` starts with, through its closing quote, or all of
/// `rest` when none closes it.
///
/// A quote is escaped when the character before it is a backslash that is not itself preceded by
/// one, Yarn's own two-character test.
fn quoted_length(rest: &str) -> usize {
    let bytes = rest.as_bytes();
    let mut index = 1;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'"' {
            let escaped = bytes.get(index - 1) == Some(&b'\\')
                && index.checked_sub(2).and_then(|before| bytes.get(before)) != Some(&b'\\');
            if !escaped {
                return index + 1;
            }
        }
        index += 1;
    }
    bytes.len()
}

/// One key line of a lock object: every key it lists and their shared value.
struct Item {
    keys: Vec<String>,
    value: Value,
}

/// The value of an [`Item`].
enum Value {
    /// A string.
    Text(String),
    /// A number or a boolean.
    Other,
    /// A nested object.
    Object(Vec<Item>),
}

/// A port of Yarn's recursive-descent lockfile parser over the tokens of [`tokenize`].
struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn current(&self) -> &Token {
        self.tokens.get(self.position).unwrap_or(&END)
    }

    fn advance(&mut self) {
        self.position += 1;
    }

    /// Parses the object at indentation `indent`, or `None` on a grammar error Yarn would raise.
    fn parse_object(&mut self, indent: usize) -> Option<Vec<Item>> {
        if indent > MAX_DEPTH {
            return None;
        }
        let mut items = Vec::new();
        loop {
            match self.current() {
                Token::Newline => {
                    self.advance();
                    // At the top level any line continues the object; below it, only a line
                    // indented to this level does.
                    if indent == 0 {
                        continue;
                    }
                    match self.current() {
                        Token::Indent(level) if *level == indent => self.advance(),
                        _ => break,
                    }
                }
                Token::Indent(level) => {
                    if *level != indent {
                        break;
                    }
                    self.advance();
                }
                Token::Eof => break,
                Token::Text(key) => {
                    let mut keys = vec![key.clone()];
                    self.advance();
                    while *self.current() == Token::Comma {
                        self.advance();
                        let Token::Text(key) = self.current() else {
                            return None;
                        };
                        keys.push(key.clone());
                        self.advance();
                    }
                    if keys.iter().any(String::is_empty) {
                        return None;
                    }
                    let colon = *self.current() == Token::Colon;
                    if colon {
                        self.advance();
                    }
                    let value = match self.current() {
                        Token::Text(text) => Value::Text(text.clone()),
                        Token::Number | Token::Boolean => Value::Other,
                        _ if colon => {
                            let object = self.parse_object(indent + 1)?;
                            items.push(Item {
                                keys,
                                value: Value::Object(object),
                            });
                            if indent > 0 && !matches!(self.current(), Token::Indent(_)) {
                                break;
                            }
                            continue;
                        }
                        _ => return None,
                    };
                    items.push(Item { keys, value });
                    self.advance();
                }
                Token::Number | Token::Boolean | Token::Colon | Token::Comma => return None,
            }
        }
        Some(items)
    }
}

#[cfg(test)]
mod tests;
