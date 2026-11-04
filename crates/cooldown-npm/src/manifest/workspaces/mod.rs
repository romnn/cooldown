//! The members of a Yarn workspace and the snapshot of their manifests that the lock is read
//! through.
//!
//! A Yarn Berry lock records every workspace it installed, so its members are the workspaces the
//! lock lists ([`berry`]), read beside the Yarn configuration that decides how their declarations
//! resolve ([`yarnrc`]).
//!
//! A Classic `yarn.lock` records no workspace member, so the member set comes from the root
//! manifest's `workspaces` field, read as [Yarn 1.22.22's `resolveWorkspaces`][resolve] reads it.
//! Each pattern is globbed for `<pattern>/package.json`, and a match counts only when its manifest
//! carries a `name` and a `version`.
//! A workspace with patterns needs a private root, and two members may not share a name.
//!
//! Discovery never reports a partial member set as complete.
//! A glob form or `workspaces` shape the reader does not model, a symbolic link on the way, an
//! unreadable or malformed manifest, or a configuration Yarn itself refuses leaves the snapshot
//! [unavailable](WorkspaceManifests::is_complete), and every consumer then falls back to the
//! evidence it had without it.
//!
//! [resolve]: https://github.com/yarnpkg/yarn/blob/v1.22.22/src/config.js#L747-L839

mod berry;
mod yarnrc;

pub(crate) use yarnrc::{YarnEnvironment, rc_files, read_settings};

use super::{DeclarationKey, display_rel};
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::MutationBaseline;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Which Yarn's workspace a snapshot reads, which decides how its members are discovered and how
/// its root `resolutions` are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceDialect {
    /// Yarn Classic (v1): the members the root manifest's `workspaces` patterns match, each named
    /// and versioned.
    Classic,
    /// Yarn Berry (v2+): the workspaces the lock records, named or not.
    Berry,
}

/// The manifest fields a declaration of a dependency can sit in, in the order Yarn's manifest
/// normalization ranks duplicate install declarations, the peer contract last.
const DECLARATION_FIELDS: [&str; 4] = [
    "optionalDependencies",
    "dependencies",
    "devDependencies",
    "peerDependencies",
];

/// The parsed manifests of every member of a Yarn workspace: the evidence a Yarn lock's member
/// attribution reads each member's declarations from ([`NodeLock`](crate::lock::NodeLock)'s `*_in`
/// hooks).
///
/// The snapshot is either complete, with every member Yarn would install, or unavailable, in which
/// case it attributes nothing.
#[derive(Debug, Clone)]
pub struct WorkspaceManifests {
    inventory: Inventory,
}

/// What discovery established about the member set.
#[derive(Debug, Clone)]
enum Inventory {
    /// Every member Yarn installs, by `/`-separated project-relative path (`.` for the root).
    Complete {
        members: BTreeMap<String, MemberManifest>,
        resolutions: Resolutions,
        /// Whether Yarn's effective `defaultProtocol` is established as `npm:`.
        /// Yarn Classic has no such setting, so it always is.
        npm_default_protocol: bool,
    },
    /// Discovery could not establish the complete member set, for the given reason.
    Unavailable { reason: String },
}

/// One member's manifest, as far as attribution reads it.
#[derive(Debug, Clone)]
pub(crate) struct MemberManifest {
    /// The package `name`; the root may have none.
    pub(crate) name: Option<String>,
    /// Every declared dependency, with each field declaring it in [`DECLARATION_FIELDS`] order.
    pub(crate) declarations: BTreeMap<String, Vec<FieldSpecifier>>,
}

/// One manifest field's declaration of a dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FieldSpecifier {
    /// The manifest field.
    pub(crate) field: &'static str,
    /// The specifier as written, `None` when the field records no string.
    pub(crate) specifier: Option<String>,
}

/// Which packages the root `resolutions` field can rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolutions {
    /// The package each key targets, the last package of its path (`a/**/@scope/b` →
    /// `@scope/b`).
    Names(BTreeSet<String>),
    /// A key cooldown cannot read, which could target any package.
    Opaque,
}

impl WorkspaceManifests {
    /// Reads the snapshot of a `dialect` workspace from disk under `root`, and Yarn's
    /// configuration from the process environment.
    ///
    /// Anything that keeps discovery from establishing every member, an unreadable manifest
    /// included, makes the snapshot unavailable rather than failing the read.
    pub(crate) fn read(root: &Utf8Path, dialect: WorkspaceDialect) -> Self {
        Self::read_in(root, dialect, &YarnEnvironment::from_process())
    }

    /// [`read`](Self::read) under the process environment `environment`.
    pub(crate) fn read_in(
        root: &Utf8Path,
        dialect: WorkspaceDialect,
        environment: &YarnEnvironment,
    ) -> Self {
        match discover_live(root, dialect, environment) {
            Ok(LiveDiscovery {
                root_document,
                directories,
                npm_default_protocol,
                ..
            }) => Self::assemble(
                Assembly {
                    root_document: &root_document,
                    directories: &directories,
                    dialect,
                    npm_default_protocol,
                },
                |rel| read_text(root, rel),
            ),
            Err(reason) => Self::unavailable(reason),
        }
    }

    /// Builds the snapshot of a `dialect` workspace entirely from what `baseline` captured: the
    /// member directories and configuration its discovery record names ([`BaselineDiscovery`]),
    /// and the manifest bytes read beside them.
    ///
    /// Nothing is read from the project, so a directory a trial creates or removes after the
    /// capture changes neither the members nor their declarations, and a configuration change
    /// after it changes no binding.
    /// A baseline without the record, one recording that discovery was unavailable, or one missing
    /// a member's manifest makes the snapshot unavailable.
    pub(crate) fn from_baseline(baseline: &MutationBaseline, dialect: WorkspaceDialect) -> Self {
        let (directories, npm_default_protocol) =
            match (dialect, DiscoveryRecord::read(baseline, dialect)) {
                (WorkspaceDialect::Classic, Ok(DiscoveryRecord::Members(directories))) => {
                    (directories, true)
                }
                (
                    WorkspaceDialect::Berry,
                    Ok(DiscoveryRecord::BerryMembers {
                        directories,
                        npm_default_protocol,
                    }),
                ) => (directories, npm_default_protocol),
                (_, Ok(DiscoveryRecord::Unavailable(reason)) | Err(reason)) => {
                    return Self::unavailable(reason);
                }
                (_, Ok(DiscoveryRecord::Members(_) | DiscoveryRecord::BerryMembers { .. })) => {
                    return Self::unavailable(
                        "the workspace discovery record is of another Yarn's workspace",
                    );
                }
            };
        let directories: Vec<Utf8PathBuf> = directories
            .iter()
            .map(|directory| directory.split('/').collect())
            .collect();
        let captured = |rel: &Utf8Path| -> Result<Option<String>, String> {
            let Some(file) = baseline.files().iter().find(|file| file.path() == rel) else {
                return Err(format!("{} was not captured", display_rel(rel)));
            };
            file.contents()
                .map(|bytes| {
                    String::from_utf8(bytes.to_vec())
                        .map_err(|err| format!("{}: {err}", display_rel(rel)))
                })
                .transpose()
        };
        let root_manifest = match captured(Utf8Path::new("package.json")) {
            Ok(Some(content)) => content,
            Ok(None) => return Self::unavailable("package.json does not exist"),
            Err(reason) => return Self::unavailable(reason),
        };
        match serde_json::from_str::<Value>(&root_manifest) {
            Ok(root_document) => Self::assemble(
                Assembly {
                    root_document: &root_document,
                    directories: &directories,
                    dialect,
                    npm_default_protocol,
                },
                captured,
            ),
            Err(err) => Self::unavailable(format!("package.json: {err}")),
        }
    }

    /// The snapshot of the workspace `assembly` describes, reading each member's manifest through
    /// `manifest_at`.
    fn assemble(
        Assembly {
            root_document,
            directories,
            dialect,
            npm_default_protocol,
        }: Assembly<'_>,
        manifest_at: impl Fn(&Utf8Path) -> Result<Option<String>, String>,
    ) -> Self {
        let mut candidates = Vec::with_capacity(directories.len());
        for directory in directories {
            let rel = directory.join("package.json");
            match manifest_at(&rel) {
                Ok(Some(content)) => candidates.push(Candidate {
                    directory: directory.clone(),
                    content,
                }),
                Ok(None) => {
                    return Self::unavailable(format!("{} disappeared", display_rel(&rel)));
                }
                Err(reason) => return Self::unavailable(reason),
            }
        }
        match build(root_document, candidates, dialect, npm_default_protocol) {
            Ok(inventory) => WorkspaceManifests { inventory },
            Err(reason) => Self::unavailable(reason),
        }
    }

    fn unavailable(reason: impl Into<String>) -> Self {
        WorkspaceManifests {
            inventory: Inventory::Unavailable {
                reason: reason.into(),
            },
        }
    }

    /// Whether discovery established every member Yarn installs.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self.inventory, Inventory::Complete { .. })
    }

    /// Why discovery could not establish every member, or [`None`] for a complete snapshot.
    #[must_use]
    pub fn unavailable_reason(&self) -> Option<&str> {
        match &self.inventory {
            Inventory::Complete { .. } => None,
            Inventory::Unavailable { reason } => Some(reason),
        }
    }

    /// Every member's path and manifest, empty for an unavailable snapshot.
    pub(crate) fn members(&self) -> impl Iterator<Item = (&str, &MemberManifest)> {
        let members = match &self.inventory {
            Inventory::Complete { members, .. } => Some(members),
            Inventory::Unavailable { .. } => None,
        };
        members
            .into_iter()
            .flatten()
            .map(|(path, manifest)| (path.as_str(), manifest))
    }

    /// Every member path, empty for an unavailable snapshot.
    pub(crate) fn member_paths(&self) -> HashSet<String> {
        self.members().map(|(path, _)| path.to_string()).collect()
    }

    /// The package names of the members other than the root: the names Yarn may link to a
    /// workspace package instead of a registry release.
    pub(crate) fn sibling_names(&self) -> HashSet<&str> {
        self.members()
            .filter(|(path, _)| *path != ".")
            .filter_map(|(_, manifest)| manifest.name.as_deref())
            .collect()
    }

    /// Whether Yarn's effective `defaultProtocol` is established as `npm:`, the protocol a Berry
    /// lock of a protocol-less schema omits; `false` for an unavailable snapshot.
    pub(crate) fn npm_default_protocol(&self) -> bool {
        match &self.inventory {
            Inventory::Complete {
                npm_default_protocol,
                ..
            } => *npm_default_protocol,
            Inventory::Unavailable { .. } => false,
        }
    }

    /// Whether the root `resolutions` field could rewrite a selector or descriptor of `name`.
    ///
    /// Yarn Classic keeps every selector a resolution rewrites under one global descriptor, and a
    /// Yarn Berry resolution reaches every descriptor of its target name, its parent qualifier
    /// aside, so a resolution reaching the name anywhere in the graph taints every member's
    /// declaration of it.
    pub(crate) fn resolution_may_rewrite(&self, name: &str) -> bool {
        match &self.inventory {
            Inventory::Complete { resolutions, .. } => match resolutions {
                Resolutions::Names(names) => names.contains(name),
                Resolutions::Opaque => true,
            },
            Inventory::Unavailable { .. } => true,
        }
    }

    /// This snapshot with each `(declaration, specifier)` of `replacements` written in place of the
    /// declaration's specifier, the manifests cooldown's own edits leave.
    ///
    /// A replacement for a declaration the snapshot does not hold changes nothing.
    pub(crate) fn with_replacements<'a>(
        &self,
        replacements: impl IntoIterator<Item = (&'a DeclarationKey, &'a str)>,
    ) -> Self {
        let mut replaced = self.clone();
        if let Inventory::Complete { members, .. } = &mut replaced.inventory {
            for (key, specifier) in replacements {
                let declaration = members
                    .get_mut(&key.member)
                    .and_then(|manifest| manifest.declarations.get_mut(&key.dependency))
                    .and_then(|fields| fields.iter_mut().find(|field| field.field == key.field));
                if let Some(declaration) = declaration {
                    declaration.specifier = Some(specifier.to_string());
                }
            }
        }
        replaced
    }
}

/// What [`WorkspaceManifests::assemble`] builds a snapshot from.
struct Assembly<'a> {
    root_document: &'a Value,
    /// The project-relative member directories, the root's excluded.
    directories: &'a [Utf8PathBuf],
    dialect: WorkspaceDialect,
    npm_default_protocol: bool,
}

/// The name a [`MutationBaseline`] records a Yarn Classic workspace's discovery under.
const DISCOVERY_RECORD: &str = "cooldown-npm/yarn-workspaces";

/// The name a [`MutationBaseline`] records a Yarn Berry workspace's discovery under.
const BERRY_DISCOVERY_RECORD: &str = "cooldown-npm/yarn-berry-workspaces";

/// Workspace discovery as a mutation baseline captures it: the manifests to read and the outcome
/// to record beside them, so every trial rebuilds the members from the capture alone
/// ([`WorkspaceManifests::from_baseline`]).
pub(crate) struct BaselineDiscovery {
    /// The project-relative manifest of every matched directory, the root's excluded.
    ///
    /// A match Yarn Classic would skip, one without a name or a version, is listed as well, since
    /// only its manifest can tell.
    pub(crate) manifests: Vec<Utf8PathBuf>,
    outcome: Result<LiveDiscovery, String>,
    dialect: WorkspaceDialect,
}

impl BaselineDiscovery {
    /// Discovers the members of a `dialect` workspace under `root` for a baseline about to be
    /// captured, reading Yarn's configuration from the process environment.
    pub(crate) fn run(root: &Utf8Path, dialect: WorkspaceDialect) -> Self {
        Self::run_in(root, dialect, &YarnEnvironment::from_process())
    }

    /// [`run`](Self::run) under the process environment `environment`.
    pub(crate) fn run_in(
        root: &Utf8Path,
        dialect: WorkspaceDialect,
        environment: &YarnEnvironment,
    ) -> Self {
        let outcome = discover_live(root, dialect, environment);
        let manifests = outcome
            .as_ref()
            .map(|LiveDiscovery { directories, .. }| {
                directories
                    .iter()
                    .map(|directory| directory.join("package.json"))
                    .collect()
            })
            .unwrap_or_default();
        BaselineDiscovery {
            manifests,
            outcome,
            dialect,
        }
    }

    /// Records the outcome in `baseline`, which captured the root manifest and [`Self::manifests`].
    ///
    /// A root manifest whose captured bytes differ from the ones discovery read changed during the
    /// capture, so its patterns may name other members, and the discovery is recorded unavailable.
    pub(crate) fn record(self, baseline: &mut MutationBaseline) {
        let captured_root = baseline
            .files()
            .iter()
            .find(|file| file.path() == Utf8Path::new("package.json"))
            .and_then(|file| file.contents());
        let record = match self.outcome {
            Ok(LiveDiscovery {
                root_manifest,
                directories,
                npm_default_protocol,
                ..
            }) if captured_root == Some(root_manifest.as_bytes()) => {
                let directories = directories
                    .iter()
                    .map(|directory| display_rel(directory))
                    .collect();
                match self.dialect {
                    WorkspaceDialect::Classic => DiscoveryRecord::Members(directories),
                    WorkspaceDialect::Berry => DiscoveryRecord::BerryMembers {
                        directories,
                        npm_default_protocol,
                    },
                }
            }
            Ok(_) => DiscoveryRecord::Unavailable(
                "package.json changed while the mutation baseline was captured".to_string(),
            ),
            Err(reason) => DiscoveryRecord::Unavailable(reason),
        };
        // A record that cannot be written is left out, which reads as unavailable discovery.
        if let Ok(fact) = serde_json::to_string(&record) {
            baseline.record(DiscoveryRecord::name(self.dialect), fact);
        }
    }
}

/// The outcome of workspace discovery a [`MutationBaseline`] records.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum DiscoveryRecord {
    /// Every directory the root's patterns matched, `/`-separated and project-relative.
    Members(Vec<String>),
    /// Every directory of a workspace a Yarn Berry lock records, the root's excluded, and whether
    /// Yarn's `defaultProtocol` was established as `npm:` at the capture.
    BerryMembers {
        directories: Vec<String>,
        npm_default_protocol: bool,
    },
    /// Why discovery could not establish every member.
    Unavailable(String),
}

impl DiscoveryRecord {
    /// The name a `dialect` workspace's record is kept under.
    fn name(dialect: WorkspaceDialect) -> &'static str {
        match dialect {
            WorkspaceDialect::Classic => DISCOVERY_RECORD,
            WorkspaceDialect::Berry => BERRY_DISCOVERY_RECORD,
        }
    }

    /// The record of a `dialect` workspace `baseline` carries, or why it carries none that can be
    /// read.
    fn read(baseline: &MutationBaseline, dialect: WorkspaceDialect) -> Result<Self, String> {
        let fact = baseline
            .recorded(Self::name(dialect))
            .ok_or_else(|| "the mutation baseline recorded no workspace discovery".to_string())?;
        serde_json::from_str(fact).map_err(|err| format!("the workspace discovery record: {err}"))
    }
}

/// What discovery found on disk under a root: the root manifest it read, the member directories,
/// and whether Yarn's `defaultProtocol` is established as `npm:`.
struct LiveDiscovery {
    root_manifest: String,
    root_document: Value,
    directories: Vec<Utf8PathBuf>,
    npm_default_protocol: bool,
}

/// Discovers the members of a `dialect` workspace under `root` from the files on disk.
fn discover_live(
    root: &Utf8Path,
    dialect: WorkspaceDialect,
    environment: &YarnEnvironment,
) -> Result<LiveDiscovery, String> {
    let root_manifest = read_text(root, Utf8Path::new("package.json"))?
        .ok_or_else(|| "package.json does not exist".to_string())?;
    let root_document: Value =
        serde_json::from_str(&root_manifest).map_err(|err| format!("package.json: {err}"))?;
    let (directories, npm_default_protocol) = match dialect {
        WorkspaceDialect::Classic => (discover(root, &root_document)?, true),
        WorkspaceDialect::Berry => {
            let configuration = yarnrc::YarnConfiguration::read(root, environment)?;
            (
                berry::locked_directories(root)?,
                configuration.npm_default_protocol,
            )
        }
    };
    Ok(LiveDiscovery {
        root_manifest,
        root_document,
        directories,
        npm_default_protocol,
    })
}

/// The text of the project-relative file `rel` under `root`, `None` when it does not exist.
fn read_text(root: &Utf8Path, rel: &Utf8Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(root.join(rel)) {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{}: {err}", display_rel(rel))),
    }
}

/// One directory a workspace pattern matched, with its manifest's text.
struct Candidate {
    /// The project-relative directory.
    directory: Utf8PathBuf,
    content: String,
}

/// The complete inventory of the `dialect` workspace whose root manifest is `root_document` and
/// whose members are `candidates`, or why Yarn would not install it as such.
fn build(
    root_document: &Value,
    candidates: Vec<Candidate>,
    dialect: WorkspaceDialect,
    npm_default_protocol: bool,
) -> Result<Inventory, String> {
    let mut members = BTreeMap::new();
    members.insert(
        ".".to_string(),
        member_manifest(root_document).map_err(|field| format!("package.json: {field}"))?,
    );
    let mut names: HashSet<String> = HashSet::new();
    for Candidate { directory, content } in candidates {
        let rel = display_rel(&directory.join("package.json"));
        let document: Value =
            serde_json::from_str(&content).map_err(|err| format!("{rel}: {err}"))?;
        let name = truthy_string(&document, "name").map_err(|field| format!("{rel}: {field}"))?;
        let version =
            truthy_string(&document, "version").map_err(|field| format!("{rel}: {field}"))?;
        // Yarn Classic skips a match without a name or a version, with a warning, so it installs
        // none of its declarations.
        // A Yarn Berry workspace needs neither: Yarn names an unnamed one after its directory.
        if dialect == WorkspaceDialect::Classic && (name.is_none() || version.is_none()) {
            continue;
        }
        if let Some(name) = name
            && !names.insert(name.to_string())
        {
            return Err(format!("two workspace members are named {name}"));
        }
        let manifest = member_manifest(&document).map_err(|field| format!("{rel}: {field}"))?;
        members.insert(display_rel(&directory), manifest);
    }
    let resolutions = match dialect {
        WorkspaceDialect::Classic => resolutions(root_document),
        WorkspaceDialect::Berry => berry::resolutions(root_document),
    };
    Ok(Inventory::Complete {
        members,
        resolutions,
        npm_default_protocol,
    })
}

/// The string `field` of `document` when it is a non-empty string, `None` when it is absent or
/// empty, which Yarn treats alike.
///
/// Any other value is the error, since Yarn refuses such a manifest.
fn truthy_string<'a>(document: &'a Value, field: &str) -> Result<Option<&'a str>, String> {
    match document.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str()).filter(|value| !value.is_empty())),
        Some(_) => Err(format!("`{field}` is not a string")),
    }
}

/// The declarations of one manifest document, or the field that keeps Yarn from reading them.
fn member_manifest(document: &Value) -> Result<MemberManifest, String> {
    let name = truthy_string(document, "name")?.map(str::to_string);
    let mut declarations: BTreeMap<String, Vec<FieldSpecifier>> = BTreeMap::new();
    for field in DECLARATION_FIELDS {
        let section = match document.get(field) {
            None | Some(Value::Null) => continue,
            Some(Value::Object(section)) => section,
            Some(_) => return Err(format!("`{field}` is not an object")),
        };
        for (dependency, specifier) in section {
            declarations
                .entry(dependency.clone())
                .or_default()
                .push(FieldSpecifier {
                    field,
                    specifier: specifier.as_str().map(str::to_string),
                });
        }
    }
    Ok(MemberManifest { name, declarations })
}

/// The packages the root manifest's `resolutions` can rewrite.
///
/// A key is read as Yarn's resolution map reads it: its packages are its `/`-separated
/// segments, a scope joined to the name after it, and it targets the last one.
/// A key Yarn rejects as no package path (a trailing `/`, an empty segment, or a trailing `*`),
/// and a field that is no object, are opaque, which taints every name rather than none.
fn resolutions(root_document: &Value) -> Resolutions {
    let entries = match root_document.get("resolutions") {
        None | Some(Value::Null) => return Resolutions::Names(BTreeSet::new()),
        Some(Value::Object(entries)) => entries,
        Some(_) => return Resolutions::Opaque,
    };
    let mut names = BTreeSet::new();
    for key in entries.keys() {
        let Some(name) = resolution_target(key) else {
            return Resolutions::Opaque;
        };
        names.insert(name);
    }
    Resolutions::Names(names)
}

/// The package a `resolutions` key targets, or `None` for a key Yarn rejects.
///
/// A segment starting with `@` and more is a scope, which takes the segment after it as its name,
/// as Yarn's `(@[^/]+/)?([^/]+)` package-path reading does.
fn resolution_target(key: &str) -> Option<String> {
    if key.is_empty() || key.ends_with('/') || key.ends_with('*') || key.contains("//") {
        return None;
    }
    let mut target = None;
    let mut segments = key.split('/');
    while let Some(segment) = segments.next() {
        let scoped = segment.len() > 1 && segment.starts_with('@');
        let name = if scoped { segments.next() } else { None };
        target = Some(match name {
            Some(name) => format!("{segment}/{name}"),
            None => segment.to_string(),
        });
    }
    target
}

/// One `/`-separated segment of a workspace pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// A directory name matched verbatim.
    Literal(String),
    /// A name pattern of `*` (any run of characters) and `?` (one character).
    Wildcard(String),
    /// `**`: zero or more directories.
    Globstar,
}

/// The project-relative directories the root manifest's `workspaces` patterns match, sorted and
/// deduplicated, the root excluded; empty for a project without workspaces.
fn discover(root: &Utf8Path, root_document: &Value) -> Result<Vec<Utf8PathBuf>, String> {
    let patterns = workspace_patterns(root_document)?;
    if patterns.is_empty() {
        return Ok(Vec::new());
    }
    // Yarn refuses to install a workspace whose root may be published.
    if root_document.get("private") != Some(&Value::Bool(true)) {
        return Err("a workspace root must be private".to_string());
    }
    let mut directories = BTreeSet::new();
    for pattern in patterns {
        let segments = parse_pattern(pattern)?;
        expand(root, Utf8Path::new(""), &segments, &mut directories)?;
    }
    if directories.contains(Utf8Path::new("")) {
        return Err("a workspace pattern matches the root itself".to_string());
    }
    Ok(directories.into_iter().collect())
}

/// The patterns of the root manifest's `workspaces` field: the array itself, or the `packages`
/// array of the object form, whose `nohoist` changes no membership.
fn workspace_patterns(root_document: &Value) -> Result<Vec<&str>, String> {
    let patterns = match root_document.get("workspaces") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(patterns)) => patterns,
        Some(Value::Object(object)) => match object.get("packages") {
            None | Some(Value::Null) => return Ok(Vec::new()),
            Some(Value::Array(patterns)) => patterns,
            Some(_) => return Err("`workspaces.packages` is not an array".to_string()),
        },
        Some(_) => return Err("`workspaces` is neither an array nor an object".to_string()),
    };
    patterns
        .iter()
        .map(|pattern| {
            pattern
                .as_str()
                .ok_or_else(|| "a workspace pattern is not a string".to_string())
        })
        .collect()
}

/// The segments of one workspace pattern.
///
/// Yarn globs `<pattern>/package.json` with one trailing `/` dropped, so a leading `./` and a
/// trailing `/` are spelling only.
/// The supported forms are literal names, `*` and `?` within a name, and `**` as a whole segment.
/// Brace and extended globs, character classes, negation, escapes, absolute or parent paths, and a
/// pattern naming `node_modules` are refused, so no form the reader does not model is ever read as
/// matching less than Yarn's glob would.
fn parse_pattern(pattern: &str) -> Result<Vec<Segment>, String> {
    let unsupported = || format!("unsupported workspace pattern `{pattern}`");
    let mut rest = pattern.strip_suffix('/').unwrap_or(pattern);
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped;
    }
    if rest.is_empty()
        || rest.starts_with('/')
        || rest.contains(['[', ']', '{', '}', '(', ')', '!', '\\'])
    {
        return Err(unsupported());
    }
    rest.split('/')
        .map(|segment| match segment {
            "" | "." | ".." | "node_modules" => Err(unsupported()),
            "**" => Ok(Segment::Globstar),
            _ if segment.contains("**") => Err(unsupported()),
            _ if segment.contains(['*', '?']) => Ok(Segment::Wildcard(segment.to_string())),
            _ => Ok(Segment::Literal(segment.to_string())),
        })
        .collect()
}

/// Adds to `out` every directory below `directory` (project-relative under `root`) that the
/// remaining `segments` match and that holds a `package.json`.
///
/// A wildcard never matches a name with a leading `.`, as Yarn's glob runs without `dot`.
/// `**` skips `node_modules` when it ends the pattern, where Yarn's ignore list drops every match
/// below one, and refuses to cross one otherwise, where that list no longer covers the matches.
/// A symbolic link to a directory the walk would enter or match, or a linked manifest, refuses
/// the walk, since Yarn's glob follows some links and not others.
fn expand(
    root: &Utf8Path,
    directory: &Utf8Path,
    segments: &[Segment],
    out: &mut BTreeSet<Utf8PathBuf>,
) -> Result<(), String> {
    let Some((segment, rest)) = segments.split_first() else {
        if holds_manifest(root, directory)? {
            out.insert(directory.to_owned());
        }
        return Ok(());
    };
    match segment {
        Segment::Literal(name) => {
            let child = directory.join(name);
            if entry_kind(root, &child)?.is_directory(root, &child)? {
                expand(root, &child, rest, out)?;
            }
        }
        Segment::Wildcard(pattern) => {
            for (name, kind) in children(root, directory)? {
                let child = directory.join(&name);
                if (!name.starts_with('.') || pattern.starts_with('.'))
                    && wildcard_matches(pattern, &name)
                    && kind.is_directory(root, &child)?
                {
                    expand(root, &child, rest, out)?;
                }
            }
        }
        Segment::Globstar => {
            expand(root, directory, rest, out)?;
            for (name, kind) in children(root, directory)? {
                let child = directory.join(&name);
                if name.starts_with('.') || !kind.is_directory(root, &child)? {
                    continue;
                }
                if name == "node_modules" {
                    if rest.is_empty() {
                        continue;
                    }
                    return Err(format!(
                        "a workspace pattern's `**` would cross {}",
                        display_rel(&child)
                    ));
                }
                expand(root, &child, segments, out)?;
            }
        }
    }
    Ok(())
}

/// What one directory entry is, without following a symbolic link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Missing,
    Directory,
    File,
    Link,
}

impl EntryKind {
    /// Whether the entry at the project-relative `rel` under `root` is a directory the walk may
    /// enter, or the error that refuses a symbolic link to one.
    ///
    /// A link to anything else, or a dangling one, is no directory and matches nothing.
    fn is_directory(self, root: &Utf8Path, rel: &Utf8Path) -> Result<bool, String> {
        match self {
            EntryKind::Directory => Ok(true),
            EntryKind::Link
                if std::fs::metadata(root.join(rel)).is_ok_and(|target| target.is_dir()) =>
            {
                Err(format!(
                    "{} is a symbolic link to a directory, which workspace discovery does not follow",
                    display_rel(rel)
                ))
            }
            EntryKind::Missing | EntryKind::File | EntryKind::Link => Ok(false),
        }
    }
}

/// The kind of the project-relative entry `rel` under `root`.
fn entry_kind(root: &Utf8Path, rel: &Utf8Path) -> Result<EntryKind, String> {
    match std::fs::symlink_metadata(root.join(rel)) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(EntryKind::Link),
        Ok(metadata) if metadata.is_dir() => Ok(EntryKind::Directory),
        Ok(_) => Ok(EntryKind::File),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(EntryKind::Missing),
        Err(err) => Err(format!("{}: {err}", display_rel(rel))),
    }
}

/// Whether the project-relative `directory` holds a `package.json` file.
fn holds_manifest(root: &Utf8Path, directory: &Utf8Path) -> Result<bool, String> {
    let manifest = directory.join("package.json");
    match entry_kind(root, &manifest)? {
        EntryKind::File => Ok(true),
        EntryKind::Missing => Ok(false),
        EntryKind::Directory => Err(format!("{} is a directory", display_rel(&manifest))),
        EntryKind::Link => Err(format!(
            "{} is a symbolic link, which workspace discovery does not follow",
            display_rel(&manifest)
        )),
    }
}

/// Every entry of the project-relative `directory` with its kind, sorted by name; empty when the
/// directory does not exist.
fn children(root: &Utf8Path, directory: &Utf8Path) -> Result<Vec<(String, EntryKind)>, String> {
    let entries = match std::fs::read_dir(root.join(directory)) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("{}: {err}", display_rel(directory))),
    };
    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("{}: {err}", display_rel(directory)))?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            return Err(format!(
                "{} holds an entry whose name is not UTF-8",
                display_rel(directory)
            ));
        };
        let kind = entry_kind(root, &directory.join(&name))?;
        children.push((name, kind));
    }
    children.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok(children)
}

/// Whether `name` matches `pattern`, where `*` matches any run of characters and `?` exactly one.
fn wildcard_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    // The classic two-pointer match: `star` remembers the last `*` and how much of the name it
    // has absorbed, so a mismatch backtracks by letting that `*` absorb one more character.
    let mut pattern_index = 0;
    let mut name_index = 0;
    let mut star: Option<(usize, usize)> = None;
    while name_index < name.len() {
        match pattern.get(pattern_index) {
            Some('*') => {
                star = Some((pattern_index, name_index));
                pattern_index += 1;
            }
            Some(&expected)
                if expected == '?' || name.get(name_index).is_some_and(|&c| c == expected) =>
            {
                pattern_index += 1;
                name_index += 1;
            }
            _ => match star {
                Some((star_index, absorbed)) => {
                    pattern_index = star_index + 1;
                    name_index = absorbed + 1;
                    star = Some((star_index, absorbed + 1));
                }
                None => return false,
            },
        }
    }
    pattern
        .get(pattern_index..)
        .is_some_and(|rest| rest.iter().all(|&c| c == '*'))
}

#[cfg(test)]
mod tests;
