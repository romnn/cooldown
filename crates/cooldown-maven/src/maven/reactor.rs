//! A Maven reactor read the way Maven builds each member's model, without running Maven.
//!
//! The reactor is the root `pom.xml` and every module its `<modules>` reach, which together are
//! one cooldown project and the POMs it owns.
//! Each member's direct dependencies are its own and the ones it inherits from local parents, and
//! each version is traced back to the element that spells it: a literal `<version>`, a property
//! in `<properties>`, or a `<dependencyManagement>` entry.
//! Only an element that the reactor owns and that nothing else reads is editable, so an edit moves
//! exactly the dependency cooldown plans.
//! Whatever the reader cannot know without Maven, such as a remote parent's management, an active
//! profile, or the release a version range selects, is reported rather than guessed, and Maven
//! itself verifies every edit before it is published.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Range;

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, MemberRef, Result};

use super::pom::{self, Declared, ParentRef, Pom, Text};
use crate::registry::CENTRAL_URL;
use crate::tree::slash_path;

/// The id of the repository Maven's super POM declares, the one source cooldown judges releases
/// by.
const CENTRAL_ID: &str = "central";

/// How deeply one property may refer to another before cooldown stops following.
const INTERPOLATION_DEPTH: usize = 16;

/// The POMs of one reactor and the local parents they inherit from.
#[derive(Debug)]
pub(crate) struct Reactor {
    root: Utf8PathBuf,
    /// Every POM read: the members first, the root among them first, then local parents outside
    /// the reactor.
    poms: Vec<Loaded>,
    /// The `-D` properties of Maven's command line, which outrank every POM's.
    user_properties: BTreeMap<String, String>,
    /// How often each property is defined across every POM read.
    definitions: BTreeMap<String, usize>,
    /// How often each property is referenced across every POM read.
    references: BTreeMap<String, usize>,
}

/// One POM the reactor read.
#[derive(Debug)]
struct Loaded {
    path: Utf8PathBuf,
    /// The path below the reactor root, for a member's POM.
    owned: Option<Utf8PathBuf>,
    text: String,
    pom: Pom,
    parent: Parent,
}

/// Where a POM's `<parent>` comes from.
#[derive(Debug, Clone, Copy)]
enum Parent {
    None,
    /// A POM read from disk, at this position of [`Reactor::poms`].
    Local(usize),
    /// A POM Maven fetches from a repository, which cooldown does not read.
    Remote,
}

/// One direct dependency of one member.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) member: MemberRef,
    /// The `group:artifact` coordinate.
    pub(crate) name: String,
    pub(crate) version: Resolution,
}

/// What cooldown knows about the version of an [`Entry`].
#[derive(Debug, Clone)]
pub(crate) enum Resolution {
    /// The version Maven selects, and where it is spelled.
    Known { current: String, source: Source },
    /// A version range, from which Maven selects the newest release it finds.
    Floating(String),
    /// A version cooldown cannot read without Maven, for the reason given.
    Unread(String),
}

/// Where a known version is spelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// Text that cooldown may replace: these bytes of the owned POM at this root-relative path.
    Editable {
        pom: Utf8PathBuf,
        span: Range<usize>,
    },
    /// Text cooldown leaves alone, for the reason given.
    Fixed(String),
}

/// The `group:artifact:type:classifier` identity Maven keys dependencies and their management by.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    group: String,
    artifact: String,
    kind: String,
    classifier: String,
}

/// What a `${name}` expression resolves to for one member, in Maven's order of precedence.
enum Lookup<'a> {
    /// A `project.*` value of the model itself.
    Builtin(String),
    /// A `-D` property of Maven's command line.
    User(String),
    /// A `<properties>` entry of this POM.
    Model(&'a Loaded, &'a Text),
}

/// Where an edit may write a version's text, or why it may not.
type Editable = std::result::Result<(Utf8PathBuf, Range<usize>), String>;

impl Reactor {
    /// Reads the reactor whose root `pom.xml` lies in `root`, taking each POM below `root` from
    /// `prepared` when it holds one and from disk otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] when a module lies outside `root`, which one cooldown project
    /// cannot own, and [`CoreError::LockUnreadable`] when a POM cannot be read or parsed.
    pub(crate) fn read(root: &Utf8Path, prepared: &BTreeMap<Utf8PathBuf, String>) -> Result<Self> {
        let mut poms: Vec<Loaded> = Vec::new();

        // Members, breadth first from the root
        let mut queue = VecDeque::from([Utf8PathBuf::from("pom.xml")]);
        while let Some(rel) = queue.pop_front() {
            if poms
                .iter()
                .any(|loaded| loaded.owned.as_ref() == Some(&rel))
            {
                continue;
            }
            let text = match prepared.get(&rel) {
                Some(text) => text.clone(),
                None => read_pom(&root.join(&rel))?,
            };
            let loaded = Loaded::parse(root.join(&rel), Some(rel.clone()), text)?;
            let dir = rel.parent().unwrap_or(Utf8Path::new(""));
            for module in &loaded.pom.modules {
                queue.push_back(module_pom(root, &rel, dir, module)?);
            }
            poms.push(loaded);
        }

        // Parents, including those of the parents read along the way
        let mut index = 0;
        while let Some((path, parent)) = poms
            .get(index)
            .map(|loaded| (loaded.path.clone(), loaded.pom.parent.clone()))
        {
            let parent = local_parent(&mut poms, &path, parent)?;
            if let Some(loaded) = poms.get_mut(index) {
                loaded.parent = parent;
            }
            index += 1;
        }

        let mut definitions = BTreeMap::new();
        let mut references = BTreeMap::new();
        for loaded in &poms {
            for (name, count) in &loaded.pom.definitions {
                *definitions.entry(name.clone()).or_default() += count;
            }
            for (name, count) in &loaded.pom.references {
                *references.entry(name.clone()).or_default() += count;
            }
        }
        Ok(Reactor {
            root: root.to_owned(),
            poms,
            user_properties: user_properties(root),
            definitions,
            references,
        })
    }

    /// The root-relative paths of the POMs the reactor owns, the root first.
    pub(crate) fn owned_paths(&self) -> impl Iterator<Item = &Utf8Path> {
        self.poms
            .iter()
            .filter_map(|loaded| loaded.owned.as_deref())
    }

    /// The local parents outside the reactor, which Maven reads but cooldown never edits.
    pub(crate) fn external_paths(&self) -> impl Iterator<Item = &Utf8Path> {
        self.poms
            .iter()
            .filter(|loaded| loaded.owned.is_none())
            .map(|loaded| loaded.path.as_path())
    }

    /// The text of the owned POM at the root-relative `path`.
    pub(crate) fn owned_text(&self, path: &Utf8Path) -> Option<&str> {
        self.poms
            .iter()
            .find(|loaded| loaded.owned.as_deref() == Some(path))
            .map(|loaded| loaded.text.as_str())
    }

    /// Every member whose dependencies cooldown leaves alone because Maven can fetch them from a
    /// repository other than Maven Central, with the reason.
    pub(crate) fn foreign_repositories(&self) -> Vec<(MemberRef, String)> {
        self.members_loaded()
            .filter_map(|member| {
                let model = self.model(member);
                let reason = model.chain.iter().find_map(|loaded| self.foreign(loaded))?;
                Some((member.member_ref(), reason))
            })
            .collect()
    }

    /// Every direct dependency of every member, with what cooldown knows about its version.
    ///
    /// A member that [`foreign_repositories`](Self::foreign_repositories) names has none.
    pub(crate) fn entries(&self) -> Vec<Entry> {
        let siblings: BTreeSet<(String, String)> = self
            .members_loaded()
            .filter_map(|loaded| coordinates(&loaded.pom))
            .collect();
        let mut entries = Vec::new();
        for member in self.members_loaded() {
            let model = self.model(member);
            if model
                .chain
                .iter()
                .any(|loaded| self.foreign(loaded).is_some())
            {
                continue;
            }
            let entry = |name: String, version| Entry {
                member: member.member_ref(),
                name,
                version,
            };
            let managed = model.effective(|pom| &pom.managed);
            for (declaring, declared, key) in model.effective(|pom| &pom.dependencies) {
                let entry_managing = managed.iter().find(|managing| managing.2 == key);
                // Maven takes the scope a declaration omits from its management entry.
                let scope = declared
                    .scope
                    .as_deref()
                    .or_else(|| entry_managing?.1.scope.as_deref())
                    .and_then(|scope| model.interpolate(scope));
                // A `system` dependency is a file on disk and an `import` one only means
                // something under management, so neither is a Maven Central release.
                if matches!(scope.as_deref(), Some("system" | "import")) {
                    continue;
                }
                // A sibling module is built by the reactor itself.
                if siblings.contains(&(key.group.clone(), key.artifact.clone())) {
                    continue;
                }
                // A profile Maven activates can replace the version, or the whole declaration, and
                // whether it does can turn on a file the scratch copy Maven verifies in lacks.
                if let Some(profiling) = model.profiling(&key) {
                    let reason = format!(
                        "a profile of {} declares it too, so Maven may select another version \
                         than the POM's own",
                        self.display_path(&profiling.path)
                    );
                    entries.push(entry(
                        format!("{}:{}", key.group, key.artifact),
                        Resolution::Unread(reason),
                    ));
                    continue;
                }
                let version = match (&declared.version, entry_managing) {
                    (Some(version), _) => model.resolve(declaring, version),
                    (None, Some((managing, managed, _))) => match &managed.version {
                        Some(version) => model.resolve(managing, version),
                        None => Resolution::Unread("the managed entry declares no version".into()),
                    },
                    (None, None) => Resolution::Unread(
                        "the version comes from dependency management outside this project, \
                         such as a remote parent POM or an imported BOM"
                            .into(),
                    ),
                };
                entries.push(entry(format!("{}:{}", key.group, key.artifact), version));
            }
            for (declaring, declared, key) in &managed {
                let imported = declared.scope.as_deref() == Some("import") && key.kind == "pom";
                let Some(version) = declared.version.as_ref().filter(|_| imported) else {
                    continue;
                };
                let version = fixed(
                    model.resolve(declaring, version),
                    "it is an imported BOM, whose version cooldown does not move",
                );
                entries.push(entry(format!("{}:{}", key.group, key.artifact), version));
            }
            if let (Parent::Remote, Some(parent)) = (member.parent, &member.pom.parent)
                && let (Some(group), Some(artifact), Some(version)) =
                    (&parent.group, &parent.artifact, &parent.version)
                && !version.value.contains("${")
            {
                let version = fixed(
                    spec(&version.value, Err(String::new())),
                    "it is the parent POM, whose version cooldown does not move",
                );
                entries.push(entry(format!("{group}:{artifact}"), version));
            }
        }
        entries
    }

    fn members_loaded(&self) -> impl Iterator<Item = &Loaded> {
        self.poms.iter().filter(|loaded| loaded.owned.is_some())
    }

    /// The model of `member`: its POM and its local parents, nearest first.
    fn model<'a>(&'a self, member: &'a Loaded) -> Model<'a> {
        let mut chain = vec![member];
        let mut current = member;
        while let Parent::Local(index) = current.parent
            && let Some(parent) = self.poms.get(index)
        {
            // Maven refuses a cycle of parents, so it never reaches a model to verify against.
            if chain.iter().any(|seen| std::ptr::eq(*seen, parent)) {
                break;
            }
            chain.push(parent);
            current = parent;
        }
        Model {
            reactor: self,
            chain,
        }
    }

    /// Why Maven can fetch the dependencies of `loaded` from somewhere other than Maven Central,
    /// when its `<repositories>` name such a repository.
    fn foreign(&self, loaded: &Loaded) -> Option<String> {
        loaded
            .pom
            .repositories
            .iter()
            .any(|repository| {
                foreign_releases(
                    repository.id.as_deref(),
                    repository.url.as_deref(),
                    repository.releases,
                )
            })
            .then(|| {
                format!(
                    "{} declares a repository other than Maven Central, so no Maven Central \
                     release date tells how old the dependencies Maven fetches through it are",
                    self.display_path(&loaded.path)
                )
            })
    }

    /// `path` as reports spell it: below the root and `/`-separated when it lies there.
    fn display_path(&self, path: &Utf8Path) -> String {
        path.strip_prefix(&self.root)
            .map_or_else(|_| path.to_string(), slash_path)
    }
}

/// One member's model: its POM and its local parents, nearest first.
struct Model<'a> {
    reactor: &'a Reactor,
    chain: Vec<&'a Loaded>,
}

impl<'a> Model<'a> {
    /// The declarations `section` selects from every POM of the chain, each under the key it
    /// interpolates to, a nearer POM's declaration hiding a farther one's of the same key as
    /// Maven's inheritance does.
    fn effective(
        &self,
        section: impl Fn(&'a Pom) -> &'a [Declared],
    ) -> Vec<(&'a Loaded, &'a Declared, Key)> {
        let mut seen = BTreeSet::new();
        let mut effective = Vec::new();
        for &loaded in &self.chain {
            for declared in section(&loaded.pom) {
                let Some(key) = self.key(declared) else {
                    continue;
                };
                if seen.insert(key.clone()) {
                    effective.push((loaded, declared, key));
                }
            }
        }
        effective
    }

    /// The POM of the chain one of whose profiles declares the artifact `key` names, under any
    /// type or classifier.
    fn profiling(&self, key: &Key) -> Option<&'a Loaded> {
        self.chain.iter().copied().find(|loaded| {
            loaded.pom.profiled.iter().any(|declared| {
                self.key(declared)
                    .is_some_and(|other| other.group == key.group && other.artifact == key.artifact)
            })
        })
    }

    fn key(&self, declared: &Declared) -> Option<Key> {
        let field = |value: &Option<String>, default: &str| match value {
            Some(value) => self.interpolate(value),
            None => Some(default.to_owned()),
        };
        Some(Key {
            group: self.interpolate(declared.group.as_deref()?)?,
            artifact: self.interpolate(declared.artifact.as_deref()?)?,
            kind: field(&declared.kind, "jar")?,
            classifier: field(&declared.classifier, "")?,
        })
    }

    /// What `${name}` means for this member.
    fn lookup(&self, name: &str) -> Option<Lookup<'a>> {
        if let Some(value) = self.builtin(name) {
            return Some(Lookup::Builtin(value));
        }
        if let Some(value) = self.reactor.user_properties.get(name) {
            return Some(Lookup::User(value.clone()));
        }
        self.chain.iter().find_map(|&loaded| {
            let text = loaded.pom.properties.get(name)?;
            Some(Lookup::Model(loaded, text))
        })
    }

    /// The `project.*` value `name` names for this member, as Maven's model reads it, under its
    /// `pom.*` alias too.
    fn builtin(&self, name: &str) -> Option<String> {
        let field = name
            .strip_prefix("project.")
            .or_else(|| name.strip_prefix("pom."))?;
        let pom = &self.chain.first()?.pom;
        let parent = pom.parent.as_ref();
        match field {
            "groupId" => coordinates(pom).map(|(group, _)| group),
            "artifactId" => pom.artifact.clone(),
            "version" => pom
                .version
                .clone()
                .or_else(|| Some(parent?.version.as_ref()?.value.clone())),
            "parent.groupId" => parent?.group.clone(),
            "parent.artifactId" => parent?.artifact.clone(),
            "parent.version" => Some(parent?.version.as_ref()?.value.clone()),
            _ => None,
        }
    }

    /// `text` with every `${name}` replaced, or `None` when one cannot be resolved without Maven.
    fn interpolate(&self, text: &str) -> Option<String> {
        let mut text = text.to_owned();
        for _ in 0..INTERPOLATION_DEPTH {
            let Some(name) = pom::references(&text).next().map(str::to_owned) else {
                return Some(text);
            };
            let value = match self.lookup(&name)? {
                Lookup::Builtin(value) | Lookup::User(value) => value,
                Lookup::Model(_, property) => property.value.clone(),
            };
            text = text.replacen(&format!("${{{name}}}"), &value, 1);
        }
        None
    }

    /// Traces `version`, the version element of a declaration in `declaring`, to the text Maven
    /// selects for this member.
    fn resolve(&self, declaring: &Loaded, version: &Text) -> Resolution {
        let Some(name) = pom::whole_reference(&version.value) else {
            if version.value.contains("${") {
                return match self.interpolate(&version.value) {
                    Some(value) => fixed(
                        spec(&value, Err(String::new())),
                        &format!(
                            "its version `{}` combines several expressions",
                            version.value
                        ),
                    ),
                    None => unread_expression(&version.value),
                };
            }
            return spec(
                &version.value,
                self.editable(declaring, version.span.clone()),
            );
        };
        match self.lookup(name) {
            Some(Lookup::Builtin(value)) => fixed(
                spec(&value, Err(String::new())),
                &format!("its version is Maven's built-in `${{{name}}}`"),
            ),
            Some(Lookup::User(value)) => fixed(
                spec(&value, Err(String::new())),
                &format!(
                    "its version is the property `{name}`, which `-D{name}` sets on Maven's \
                     command line, in `.mvn/maven.config` or `MAVEN_ARGS`"
                ),
            ),
            Some(Lookup::Model(defining, property)) => {
                if property.value.contains("${") {
                    return match self.interpolate(&property.value) {
                        Some(value) => fixed(
                            spec(&value, Err(String::new())),
                            &format!(
                                "its version is the property `{name}`, whose value refers to \
                                 another property"
                            ),
                        ),
                        None => unread_expression(&property.value),
                    };
                }
                let exclusive = self.reactor.definitions.get(name) == Some(&1)
                    && self.reactor.references.get(name) == Some(&1);
                if !exclusive {
                    return fixed(
                        spec(&property.value, Err(String::new())),
                        &format!(
                            "its version is the property `{name}`, which is defined more than \
                             once or used elsewhere too"
                        ),
                    );
                }
                spec(
                    &property.value,
                    self.editable(defining, property.span.clone()),
                )
            }
            None => unread_expression(&version.value),
        }
    }

    /// Where an edit may write the text spelled at `span` in `pom`, or why it may not.
    fn editable(&self, pom: &Loaded, span: Option<Range<usize>>) -> Editable {
        let Some(owned) = &pom.owned else {
            return Err(format!(
                "its version is declared in {}, outside this project",
                self.reactor.display_path(&pom.path)
            ));
        };
        span.map(|span| (owned.clone(), span)).ok_or_else(|| {
            "its version element holds a comment, a CDATA section or a character reference".into()
        })
    }
}

impl Loaded {
    fn parse(path: Utf8PathBuf, owned: Option<Utf8PathBuf>, text: String) -> Result<Self> {
        let pom = pom::parse(&text)
            .map_err(|error| CoreError::LockUnreadable(format!("{path}: {error}")))?;
        Ok(Loaded {
            path,
            owned,
            text,
            pom,
            parent: Parent::None,
        })
    }

    /// The member this POM is, as reports attribute it: named by its artifact and placed at its
    /// directory below the root, `/`-separated, with the root itself at `.`.
    fn member_ref(&self) -> MemberRef {
        let dir = self
            .owned
            .as_deref()
            .and_then(Utf8Path::parent)
            .unwrap_or(Utf8Path::new(""));
        let path = slash_path(dir);
        MemberRef {
            name: self.pom.artifact.clone().unwrap_or_else(|| path.clone()),
            path,
        }
    }
}

/// Where the `<parent>` of the POM at `path` comes from, reading a local one into `poms` when it
/// is not read yet.
///
/// Maven reads the parent from its relative path, `../pom.xml` unless the POM names another,
/// when the POM there has the coordinates the `<parent>` names, and fetches it otherwise.
fn local_parent(
    poms: &mut Vec<Loaded>,
    path: &Utf8Path,
    parent: Option<ParentRef>,
) -> Result<Parent> {
    let Some(parent) = parent else {
        return Ok(Parent::None);
    };
    let relative = parent.relative_path.as_deref().unwrap_or("../pom.xml");
    let dir = path.parent().unwrap_or(Utf8Path::new(""));
    let Some(mut candidate) = normalize(&dir.join(relative)).filter(|_| !relative.is_empty())
    else {
        return Ok(Parent::Remote);
    };
    if candidate.is_dir() {
        candidate.push("pom.xml");
    }
    if candidate == path || !candidate.is_file() {
        return Ok(Parent::Remote);
    }
    if let Some(found) = poms.iter().position(|loaded| loaded.path == candidate) {
        let local = poms
            .get(found)
            .is_some_and(|loaded| declares(&loaded.pom, &parent));
        return Ok(if local {
            Parent::Local(found)
        } else {
            Parent::Remote
        });
    }
    let text = read_pom(&candidate)?;
    let loaded = Loaded::parse(candidate, None, text)?;
    if !declares(&loaded.pom, &parent) {
        return Ok(Parent::Remote);
    }
    poms.push(loaded);
    Ok(Parent::Local(poms.len() - 1))
}

/// The effective group and artifact of `pom`, the group inherited from its `<parent>` when it
/// declares none.
fn coordinates(pom: &Pom) -> Option<(String, String)> {
    let group = pom
        .group
        .clone()
        .or_else(|| pom.parent.as_ref()?.group.clone())?;
    Some((group, pom.artifact.clone()?))
}

/// Whether `pom` is the POM `parent` names: the coordinates Maven compares before it reads a
/// parent from disk rather than a repository.
fn declares(pom: &Pom, parent: &ParentRef) -> bool {
    let inherited = pom.parent.as_ref();
    let version = pom
        .version
        .clone()
        .or_else(|| Some(inherited?.version.as_ref()?.value.clone()));
    coordinates(pom).map(|(group, _)| group) == parent.group
        && pom.artifact == parent.artifact
        && version == parent.version.as_ref().map(|version| version.value.clone())
}

/// The [`Resolution`] of the version specification `value`, whose text an edit may rewrite at
/// `at`.
///
/// A hard requirement `[v]` pins exactly `v`, and an edit rewrites only the version inside its
/// brackets, while any other range floats.
fn spec(value: &str, at: Editable) -> Resolution {
    let (current, offset) = if value.starts_with(['[', '(']) || value.ends_with([']', ')']) {
        match value
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
        {
            Some(inner) if !inner.contains(',') && !inner.trim().is_empty() => {
                let leading = inner.len() - inner.trim_start().len();
                (inner.trim(), 1 + leading)
            }
            _ => return Resolution::Floating(value.to_owned()),
        }
    } else {
        (value, 0)
    };
    let source = match at {
        Ok((pom, span)) => Source::Editable {
            pom,
            span: span.start + offset..span.start + offset + current.len(),
        },
        Err(reason) => Source::Fixed(reason),
    };
    Resolution::Known {
        current: current.to_owned(),
        source,
    }
}

/// `resolution` with its source fixed for `reason`, wherever the reader found it spelled.
fn fixed(resolution: Resolution, reason: &str) -> Resolution {
    match resolution {
        Resolution::Known { current, .. } => Resolution::Known {
            current,
            source: Source::Fixed(reason.to_owned()),
        },
        other => other,
    }
}

fn unread_expression(expression: &str) -> Resolution {
    Resolution::Unread(format!(
        "the version `{expression}` refers to a property no POM of this project defines"
    ))
}

/// Whether Maven can fetch a release from the repository `id` at `url` that is not Maven Central
/// as Maven's super POM declares it.
///
/// A repository with `releases` off serves snapshots alone, as the Apache and Sonatype parent POMs
/// declare one, so no release cooldown plans can come from it.
pub(crate) fn foreign_releases(id: Option<&str>, url: Option<&str>, releases: bool) -> bool {
    let central =
        id == Some(CENTRAL_ID) && url.map(|url| url.trim_end_matches('/')) == Some(CENTRAL_URL);
    releases && !central
}

/// The root-relative POM of `module`, listed by the POM at `listing` in the directory `dir`.
fn module_pom(
    root: &Utf8Path,
    listing: &Utf8Path,
    dir: &Utf8Path,
    module: &str,
) -> Result<Utf8PathBuf> {
    let outside = || {
        CoreError::Config(format!(
            "{} lists the module {module} outside the reactor root {root}; cooldown owns a \
             reactor only when every module lies below its root",
            slash_path(listing)
        ))
    };
    let target = normalize(&dir.join(module)).ok_or_else(outside)?;
    if target.has_root() || target.as_str().is_empty() {
        return Err(outside());
    }
    let pom = if root.join(&target).is_file() {
        target
    } else {
        target.join("pom.xml")
    };
    if !root.join(&pom).is_file() {
        return Err(CoreError::LockUnreadable(format!(
            "{} lists the module {module}, which has no POM",
            slash_path(listing)
        )));
    }
    Ok(pom)
}

/// The directories below `root` that the reactor rooted there claims as its modules, read from
/// each POM's `<modules>` alone.
///
/// A POM that cannot be read claims nothing, so a broken module stays a project of its own.
pub(crate) fn claimed_dirs(root: &Utf8Path) -> BTreeSet<Utf8PathBuf> {
    let mut claimed = BTreeSet::new();
    let mut queue = VecDeque::from([Utf8PathBuf::from("pom.xml")]);
    let mut seen = BTreeSet::new();
    while let Some(rel) = queue.pop_front() {
        if !seen.insert(rel.clone()) {
            continue;
        }
        let Some(pom) = read_pom(&root.join(&rel))
            .ok()
            .and_then(|text| pom::parse(&text).ok())
        else {
            continue;
        };
        let dir = rel.parent().unwrap_or(Utf8Path::new("")).to_owned();
        for module in &pom.modules {
            if let Ok(module) = module_pom(root, &rel, &dir, module) {
                // Only a module whose POM is the directory's `pom.xml` makes the directory the
                // reactor's, since another POM listed beside it leaves that `pom.xml` its own.
                if module.file_name() == Some("pom.xml")
                    && let Some(module_dir) = module.parent().filter(|dir| !dir.as_str().is_empty())
                {
                    claimed.insert(root.join(module_dir));
                }
                queue.push_back(module);
            }
        }
    }
    claimed
}

/// The `.mvn` directory Maven's launcher reads for a build started in `root`: the nearest one in
/// `root` or above it.
pub(crate) fn maven_config_dir(root: &Utf8Path) -> Option<Utf8PathBuf> {
    root.ancestors()
        .map(|dir| dir.join(".mvn"))
        .find(|dir| dir.is_dir())
}

/// The arguments Maven's launcher adds to every build started in `root`: each line of
/// `.mvn/maven.config` as one argument, skipping blank lines and those starting with `#` as Maven
/// does, and then `MAVEN_ARGS` split at whitespace as the launcher script splits it.
fn maven_arguments(root: &Utf8Path) -> Vec<String> {
    let mut arguments = Vec::new();
    if let Some(config) = maven_config_dir(root)
        .and_then(|dir| std::fs::read_to_string(dir.join("maven.config")).ok())
    {
        let lines = config
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'));
        arguments.extend(lines.map(str::to_owned));
    }
    if let Ok(args) = std::env::var("MAVEN_ARGS") {
        arguments.extend(args.split_whitespace().map(str::to_owned));
    }
    arguments
}

/// The `-D` properties a build started in `root` gets on Maven's command line, from
/// [`maven_arguments`].
fn user_properties(root: &Utf8Path) -> BTreeMap<String, String> {
    defines(&maven_arguments(root))
}

/// The `-Dname=value` and `--define name=value` arguments of `arguments`, a value left out
/// reading as `true`.
fn defines(arguments: &[String]) -> BTreeMap<String, String> {
    let mut properties = BTreeMap::new();
    let mut tokens = arguments.iter().map(String::as_str);
    while let Some(token) = tokens.next() {
        let define = match token {
            "-D" | "--define" => tokens.next(),
            _ => token
                .strip_prefix("--define=")
                .or_else(|| token.strip_prefix("-D").filter(|rest| !rest.is_empty())),
        };
        if let Some(define) = define {
            let (name, value) = define.split_once('=').unwrap_or((define, "true"));
            properties.insert(name.to_owned(), value.to_owned());
        }
    }
    properties
}

/// The option of [`maven_arguments`] that narrows or redirects the reactor Maven builds for
/// `root`, if one does.
///
/// cooldown verifies an edit against every project of the reactor, so a build limited to some of
/// them, or started from another POM, cannot vouch for it.
pub(crate) fn narrowing_option(root: &Utf8Path) -> Option<String> {
    const NARROWING: [&str; 8] = [
        "-N",
        "--non-recursive",
        "-pl",
        "--projects",
        "-rf",
        "--resume-from",
        "-f",
        "--file",
    ];
    maven_arguments(root).into_iter().find_map(|argument| {
        let option = argument
            .split_once('=')
            .map_or(argument.as_str(), |(option, _)| option);
        if NARROWING.contains(&option) {
            return Some(option.to_owned());
        }
        // `-pl`, `-rf` and `-f` also take their value attached, as in `-plapp`, while `-fae`,
        // `-ff` and `-fn` are flags of their own.
        let attached = ["-pl", "-rf", "-f"]
            .into_iter()
            .find(|short| argument.starts_with(short))?;
        (!["-fae", "-ff", "-fn"].contains(&argument.as_str())).then(|| attached.to_owned())
    })
}

fn read_pom(path: &Utf8Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|error| CoreError::LockUnreadable(format!("{path}: {error}")))
}

/// `path` with `.` and `..` resolved without touching the disk, or `None` when a `..` climbs
/// above its start.
fn normalize(path: &Utf8Path) -> Option<Utf8PathBuf> {
    let mut normalized = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                if !matches!(
                    normalized.components().next_back(),
                    Some(Utf8Component::Normal(_))
                ) {
                    return None;
                }
                normalized.pop();
            }
            other => normalized.push(other.as_str()),
        }
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::{formatdoc, indoc};

    /// A directory holding `files`, by their `/`-separated relative paths.
    fn tree(files: &[(&str, &str)]) -> Result<(tempfile::TempDir, Utf8PathBuf)> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
        for (path, text) in files {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, text)?;
        }
        Ok((dir, root))
    }

    /// A POM for `demo:{artifact}` with `body` inside `<project>`.
    fn pom(artifact: &str, body: &str) -> String {
        formatdoc! {r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>demo</groupId>
              <artifactId>{artifact}</artifactId>
              <version>0.1.0</version>
            {body}</project>
        "#}
    }

    /// A module POM for `demo:{artifact}` inheriting from `demo:parent` one directory up.
    fn module(artifact: &str, body: &str) -> String {
        formatdoc! {r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <parent>
                <groupId>demo</groupId>
                <artifactId>parent</artifactId>
                <version>0.1.0</version>
              </parent>
              <artifactId>{artifact}</artifactId>
            {body}</project>
        "#}
    }

    /// A `<dependency>` on `group:artifact` at `version`, with `extra` elements.
    fn dependency(coordinate: &str, version: &str, extra: &str) -> String {
        let (group, artifact) = coordinate.split_once(':').unwrap_or_default();
        format!(
            "<dependency><groupId>{group}</groupId><artifactId>{artifact}</artifactId>\
             <version>{version}</version>{extra}</dependency>\n"
        )
    }

    fn read(root: &Utf8Path) -> Result<Reactor> {
        Reactor::read(root, &BTreeMap::new())
    }

    /// The entries of `reactor` for `name`, as `(member path, resolution)`.
    fn entries_of(reactor: &Reactor, name: &str) -> Vec<(String, Resolution)> {
        reactor
            .entries()
            .into_iter()
            .filter(|entry| entry.name == name)
            .map(|entry| (entry.member.path, entry.version))
            .collect()
    }

    /// The text the editable source of `resolution` spans in its POM.
    fn edited_text<'a>(reactor: &'a Reactor, resolution: &Resolution) -> Option<&'a str> {
        let Resolution::Known {
            source: Source::Editable { pom, span },
            ..
        } = resolution
        else {
            return None;
        };
        reactor.owned_text(pom)?.get(span.clone())
    }

    fn fixed_reason(resolution: &Resolution) -> Option<&str> {
        match resolution {
            Resolution::Known {
                source: Source::Fixed(reason),
                ..
            } => Some(reason),
            _ => None,
        }
    }

    /// A property defined once and used once is the element an edit rewrites, while one that
    /// another expression reads too stays fixed, its version still reported.
    #[test]
    fn only_an_exclusive_property_is_editable() -> Result<()> {
        let body = formatdoc! {"
            <properties>
              <text.version>1.10.0</text.version>
              <lang.version>3.12.0</lang.version>
              <label>lang ${{lang.version}}</label>
            </properties>
            <dependencies>
            {text}{lang}</dependencies>
        ", text = dependency("org.apache.commons:commons-text", "${text.version}", ""),
        lang = dependency("org.apache.commons:commons-lang3", "${lang.version}", "")};
        let (_dir, root) = tree(&[("pom.xml", &pom("demo", &body))])?;

        let reactor = read(&root)?;

        let text = entries_of(&reactor, "org.apache.commons:commons-text");
        assert_eq!(edited_text(&reactor, &text[0].1), Some("1.10.0"));
        let lang = entries_of(&reactor, "org.apache.commons:commons-lang3");
        assert!(matches!(&lang[0].1, Resolution::Known { current, .. } if current == "3.12.0"));
        assert!(fixed_reason(&lang[0].1).is_some_and(|reason| reason.contains("`lang.version`")));
        Ok(())
    }

    /// A module inherits its parent's dependencies, attributed to both, and takes a version-less
    /// dependency's version from the parent's management entry, which is the element to edit; a
    /// module's own redeclaration of an inherited dependency hides the parent's.
    #[test]
    fn inherited_and_managed_versions_trace_to_their_element() -> Result<()> {
        let root = pom(
            "parent",
            &formatdoc! {"
                <packaging>pom</packaging>
                <modules><module>app</module><module>lib</module></modules>
                <dependencyManagement><dependencies>
                {managed}</dependencies></dependencyManagement>
                <dependencies>
                {inherited}</dependencies>
            ", managed = dependency("org.apache.commons:commons-text", "1.10.0", ""),
            inherited = dependency("org.apache.commons:commons-lang3", "3.12.0", "")},
        );
        let app = module(
            "app",
            "<dependencies><dependency><groupId>org.apache.commons</groupId>\
             <artifactId>commons-text</artifactId></dependency></dependencies>\n",
        );
        let lib = module(
            "lib",
            &format!(
                "<dependencies>{}</dependencies>\n",
                dependency("org.apache.commons:commons-lang3", "3.17.0", "")
            ),
        );
        let (_dir, root_dir) = tree(&[
            ("pom.xml", &root),
            ("app/pom.xml", &app),
            ("lib/pom.xml", &lib),
        ])?;

        let reactor = read(&root_dir)?;

        // The managed version lands in the root's management entry, and is only app's.
        let text = entries_of(&reactor, "org.apache.commons:commons-text");
        assert_eq!(text.len(), 1);
        assert_eq!(text[0].0, "app");
        let Resolution::Known {
            source: Source::Editable { pom, .. },
            ..
        } = &text[0].1
        else {
            panic!("the managed version is editable: {text:?}");
        };
        assert_eq!(pom, Utf8Path::new("pom.xml"));
        assert_eq!(edited_text(&reactor, &text[0].1), Some("1.10.0"));

        // The root and app share the root's 3.12.0, while lib's own 3.17.0 hides it.
        let lang: Vec<(String, String)> = entries_of(&reactor, "org.apache.commons:commons-lang3")
            .into_iter()
            .filter_map(|(member, resolution)| match resolution {
                Resolution::Known { current, .. } => Some((member, current)),
                _ => None,
            })
            .collect();
        assert_eq!(
            lang,
            [
                (".".to_owned(), "3.12.0".to_owned()),
                ("app".to_owned(), "3.12.0".to_owned()),
                ("lib".to_owned(), "3.17.0".to_owned()),
            ]
        );
        assert_eq!(
            reactor.owned_paths().map(slash_path).collect::<Vec<_>>(),
            ["pom.xml", "app/pom.xml", "lib/pom.xml"]
        );
        Ok(())
    }

    /// A range floats, while a hard requirement `[v]` pins `v` and edits only the version inside
    /// its brackets.
    #[test]
    fn ranges_float_and_a_hard_requirement_pins() -> Result<()> {
        let body = format!(
            "<dependencies>\n{}{}</dependencies>\n",
            dependency("org.apache.commons:commons-text", "[1.10.0,2.0)", ""),
            dependency("org.apache.commons:commons-lang3", "[ 3.12.0 ]", ""),
        );
        let (_dir, root) = tree(&[("pom.xml", &pom("demo", &body))])?;

        let reactor = read(&root)?;

        let text = entries_of(&reactor, "org.apache.commons:commons-text");
        assert!(matches!(&text[0].1, Resolution::Floating(range) if range == "[1.10.0,2.0)"));
        let lang = entries_of(&reactor, "org.apache.commons:commons-lang3");
        assert!(matches!(&lang[0].1, Resolution::Known { current, .. } if current == "3.12.0"));
        assert_eq!(edited_text(&reactor, &lang[0].1), Some("3.12.0"));
        Ok(())
    }

    /// A `-D` property in `.mvn/maven.config` outranks the POM's, so the dependency reads at the
    /// overriding value and stays fixed.
    #[test]
    fn a_command_line_property_outranks_the_pom() -> Result<()> {
        let body = format!(
            "<properties><text.version>1.10.0</text.version></properties>\n\
             <dependencies>{}</dependencies>\n",
            dependency("org.apache.commons:commons-text", "${text.version}", "")
        );
        let (_dir, root) = tree(&[
            ("pom.xml", &pom("demo", &body)),
            (".mvn/maven.config", "-B\n-Dtext.version=1.11.0\n"),
        ])?;

        let reactor = read(&root)?;

        let text = entries_of(&reactor, "org.apache.commons:commons-text");
        assert!(matches!(&text[0].1, Resolution::Known { current, .. } if current == "1.11.0"));
        assert!(fixed_reason(&text[0].1).is_some_and(|reason| reason.contains("-Dtext.version")));
        Ok(())
    }

    /// An imported BOM and a remote parent are reported at their versions but fixed, a version
    /// only remote management supplies is unread, and a sibling module is no dependency.
    #[test]
    fn boms_remote_parents_and_siblings() -> Result<()> {
        let text = indoc! {r"
            <project>
              <parent>
                <groupId>org.sonatype.oss</groupId>
                <artifactId>oss-parent</artifactId>
                <version>7</version>
              </parent>
              <groupId>demo</groupId>
              <artifactId>demo</artifactId>
              <version>0.1.0</version>
              <modules><module>lib</module></modules>
              <dependencyManagement><dependencies>
                <dependency>
                  <groupId>org.junit</groupId><artifactId>junit-bom</artifactId>
                  <version>5.10.0</version><type>pom</type><scope>import</scope>
                </dependency>
              </dependencies></dependencyManagement>
              <dependencies>
                <dependency>
                  <groupId>org.junit.jupiter</groupId><artifactId>junit-jupiter-api</artifactId>
                </dependency>
                <dependency>
                  <groupId>${project.groupId}</groupId><artifactId>lib</artifactId>
                  <version>${project.version}</version>
                </dependency>
              </dependencies>
            </project>
        "};
        let lib = pom("lib", "");
        let (_dir, root) = tree(&[("pom.xml", text), ("lib/pom.xml", &lib)])?;

        let reactor = read(&root)?;

        let names: Vec<String> = reactor
            .entries()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            names,
            [
                "org.junit.jupiter:junit-jupiter-api",
                "org.junit:junit-bom",
                "org.sonatype.oss:oss-parent",
            ]
        );
        let bom = entries_of(&reactor, "org.junit:junit-bom");
        assert!(fixed_reason(&bom[0].1).is_some_and(|reason| reason.contains("BOM")));
        let parent = entries_of(&reactor, "org.sonatype.oss:oss-parent");
        assert!(fixed_reason(&parent[0].1).is_some_and(|reason| reason.contains("parent POM")));
        let api = entries_of(&reactor, "org.junit.jupiter:junit-jupiter-api");
        assert!(matches!(&api[0].1, Resolution::Unread(_)));
        Ok(())
    }

    /// A POM chain that declares a repository other than Maven Central leaves its member's
    /// dependencies unread, while a redeclared Maven Central or a repository for snapshots alone
    /// does not.
    #[test]
    fn a_foreign_repository_leaves_a_member_unread() -> Result<()> {
        let repository = |id: &str, url: &str| {
            format!(
                "<repositories><repository><id>{id}</id><url>{url}</url></repository></repositories>\n\
                 <dependencies>{}</dependencies>\n",
                dependency("org.apache.commons:commons-text", "1.10.0", "")
            )
        };
        let (_central, central) = tree(&[(
            "pom.xml",
            &pom(
                "demo",
                &repository("central", "https://repo.maven.apache.org/maven2/"),
            ),
        )])?;
        let (_corp, corp) = tree(&[(
            "pom.xml",
            &pom(
                "demo",
                &repository("corp", "https://repo.example.com/maven2"),
            ),
        )])?;

        let snapshots = formatdoc! {"
            <repositories><repository>
              <id>snapshots</id><url>https://repo.example.com/snapshots</url>
              <releases><enabled>false</enabled></releases>
            </repository></repositories>
            <dependencies>{text}</dependencies>
        ", text = dependency("org.apache.commons:commons-text", "1.10.0", "")};
        let (_snapshots, snapshots) = tree(&[("pom.xml", &pom("demo", &snapshots))])?;

        assert_eq!(read(&central)?.entries().len(), 1);
        assert_eq!(read(&snapshots)?.entries().len(), 1);
        let corp = read(&corp)?;
        assert!(corp.entries().is_empty());
        let foreign = corp.foreign_repositories();
        assert_eq!(foreign.len(), 1);
        assert!(!foreign[0].1.contains("example.com"), "{foreign:?}");
        Ok(())
    }

    /// What a profile declares may be active in the project but not in the copy Maven verifies in,
    /// so a dependency a profile declares too is unread, and a repository a profile declares
    /// leaves its member unread.
    #[test]
    fn profiles_leave_what_they_could_change_unread() -> Result<()> {
        let profile = |content: &str| {
            pom(
                "demo",
                &formatdoc! {"
                    <dependencies>{text}</dependencies>
                    <profiles><profile>
                      <activation><file><exists>src/main/java</exists></file></activation>
                      {content}
                    </profile></profiles>
                ", text = dependency("org.apache.commons:commons-text", "1.10.0", "")},
            )
        };
        let declared = format!(
            "<dependencyManagement><dependencies>{}</dependencies></dependencyManagement>",
            dependency("org.apache.commons:commons-text", "1.11.0", "")
        );
        let repository = "<repositories><repository><id>corp</id><url>https://repo.example.com/m2</url>\
             </repository></repositories>";
        let (_declared, declared) = tree(&[("pom.xml", &profile(&declared))])?;
        let (_repository, repository) = tree(&[("pom.xml", &profile(repository))])?;

        let declared = read(&declared)?;
        let text = entries_of(&declared, "org.apache.commons:commons-text");
        assert!(
            matches!(&text[0].1, Resolution::Unread(reason) if reason.contains("profile")),
            "{text:?}"
        );
        let repository = read(&repository)?;
        assert!(repository.entries().is_empty());
        assert_eq!(repository.foreign_repositories().len(), 1);
        Ok(())
    }

    /// A local parent outside the reactor is read, so its properties and dependencies count, but
    /// never owned, so nothing it spells is editable.
    #[test]
    fn a_local_parent_outside_the_reactor_is_read_but_not_owned() -> Result<()> {
        let parent = pom(
            "parent",
            &formatdoc! {"
                <packaging>pom</packaging>
                <properties><text.version>1.10.0</text.version></properties>
                <dependencies>{lang}</dependencies>
            ", lang = dependency("org.apache.commons:commons-lang3", "3.12.0", "")},
        );
        let app = module(
            "app",
            &format!(
                "<dependencies>{}</dependencies>\n",
                dependency("org.apache.commons:commons-text", "${text.version}", "")
            ),
        );
        let (_dir, base) = tree(&[("pom.xml", &parent), ("app/pom.xml", &app)])?;

        let reactor = read(&base.join("app"))?;

        assert_eq!(reactor.external_paths().count(), 1);
        for name in [
            "org.apache.commons:commons-text",
            "org.apache.commons:commons-lang3",
        ] {
            let entries = entries_of(&reactor, name);
            assert!(
                fixed_reason(&entries[0].1).is_some_and(|reason| reason.contains("outside")),
                "{entries:?}"
            );
        }
        Ok(())
    }

    /// A module outside the reactor root cannot be owned by one project, so reading refuses it;
    /// ownership claims only the modules below the root.
    #[test]
    fn a_module_outside_the_root_is_refused() -> Result<()> {
        let root = pom(
            "parent",
            "<packaging>pom</packaging><modules><module>../sibling</module><module>app</module></modules>\n",
        );
        let (_dir, base) = tree(&[
            ("root/pom.xml", &root),
            ("root/app/pom.xml", &module("app", "")),
            ("sibling/pom.xml", &pom("sibling", "")),
        ])?;

        assert!(matches!(
            read(&base.join("root")),
            Err(CoreError::Config(_))
        ));
        assert_eq!(
            claimed_dirs(&base.join("root")),
            BTreeSet::from([base.join("root/app")])
        );
        Ok(())
    }

    /// A module listed by its POM file claims no directory, so the `pom.xml` beside it stays a
    /// project of its own.
    #[test]
    fn a_module_listed_by_its_file_claims_no_directory() -> Result<()> {
        let root = pom(
            "parent",
            "<packaging>pom</packaging>\n\
             <modules><module>app</module><module>nested/extra.xml</module></modules>\n",
        );
        let (_dir, root_dir) = tree(&[
            ("pom.xml", &root),
            ("app/pom.xml", &module("app", "")),
            ("nested/extra.xml", &module("nested-extra", "")),
            ("nested/pom.xml", &pom("nested", "")),
        ])?;

        assert_eq!(
            claimed_dirs(&root_dir),
            BTreeSet::from([root_dir.join("app")])
        );
        Ok(())
    }

    /// A dependency whose management entry gives it the `system` scope is a file on disk, not a
    /// Maven Central release, so it is no entry.
    #[test]
    fn a_managed_system_scope_is_no_dependency() -> Result<()> {
        let body = format!(
            "<dependencyManagement><dependencies>{}</dependencies></dependencyManagement>\n\
             <dependencies><dependency><groupId>com.sun</groupId><artifactId>tools</artifactId>\
             </dependency></dependencies>\n",
            dependency(
                "com.sun:tools",
                "1.8",
                "<scope>system</scope><systemPath>/opt/tools.jar</systemPath>"
            )
        );
        let (_dir, root) = tree(&[("pom.xml", &pom("demo", &body))])?;

        assert!(read(&root)?.entries().is_empty());
        Ok(())
    }

    /// An empty `<relativePath/>` turns the local lookup off, so the POM above is no parent even
    /// when its coordinates match, and the parent is reported as a remote one.
    #[test]
    fn an_empty_relative_path_makes_the_parent_remote() -> Result<()> {
        let parent = pom(
            "parent",
            &format!(
                "<packaging>pom</packaging><dependencies>{}</dependencies>\n",
                dependency("org.apache.commons:commons-lang3", "3.12.0", "")
            ),
        );
        let app = module("app", "").replace("</parent>", "<relativePath/></parent>");
        let (_dir, base) = tree(&[("pom.xml", &parent), ("app/pom.xml", &app)])?;

        let reactor = read(&base.join("app"))?;

        assert_eq!(reactor.external_paths().count(), 0);
        let names: Vec<String> = reactor
            .entries()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, ["demo:parent"]);
        Ok(())
    }

    /// A line of `.mvn/maven.config` that starts with `#` is a comment Maven skips, so the `-D`
    /// in it overrides nothing, while the other lines are one stream of arguments, so a `-D` on
    /// one line defines the property the next line names.
    #[test]
    fn maven_config_is_one_stream_without_its_comments() -> Result<()> {
        let body = format!(
            "<properties><text.version>1.10.0</text.version></properties>\n\
             <dependencies>{}</dependencies>\n",
            dependency("org.apache.commons:commons-text", "${text.version}", "")
        );
        let pom = pom("demo", &body);
        let read_with = |config: &str| -> Result<Vec<(String, Resolution)>> {
            let (_dir, root) = tree(&[("pom.xml", &pom), (".mvn/maven.config", config)])?;
            Ok(entries_of(&read(&root)?, "org.apache.commons:commons-text"))
        };

        let commented = read_with("# -Dtext.version=1.11.0\n-B\n")?;
        let split = read_with("-B\n-D\ntext.version=1.11.0\n")?;

        assert!(
            matches!(&commented[0].1, Resolution::Known { current, .. } if current == "1.10.0"),
            "{commented:?}"
        );
        assert!(
            matches!(&split[0].1, Resolution::Known { current, .. } if current == "1.11.0"),
            "{split:?}"
        );
        Ok(())
    }

    /// The launcher options that build only part of the reactor, or another POM's, are found in
    /// every spelling, and no other option is taken for one, not even text inside a
    /// `maven.config` line, which Maven reads as one argument.
    #[test]
    fn narrowing_options_in_every_spelling() -> Result<()> {
        let narrowing = |config: &str| -> Result<Option<String>> {
            let (_dir, root) = tree(&[(".mvn/maven.config", config)])?;
            Ok(narrowing_option(&root))
        };

        for (config, option) in [
            ("-N", "-N"),
            ("--non-recursive", "--non-recursive"),
            ("-pl app", "-pl"),
            ("-plapp", "-pl"),
            ("--projects=app", "--projects"),
            ("-rf :app", "-rf"),
            ("--resume-from\n:app", "--resume-from"),
            ("-f other.xml", "-f"),
            ("-fother.xml", "-f"),
            ("--file=other.xml", "--file"),
            ("# a comment\n-B\n-pl\napp", "-pl"),
        ] {
            assert_eq!(narrowing(config)?.as_deref(), Some(option), "{config}");
        }
        for config in [
            "-B\n-fae\n-ff\n-fn\n-Pextra\n-am\n-amd\n-Dpl=app",
            "# -N\n-o",
            "-Ddescription=build with -pl app",
        ] {
            assert_eq!(narrowing(config)?, None, "{config}");
        }
        Ok(())
    }

    #[test]
    fn defines_reads_every_spelling() {
        let arguments: Vec<String> = "-Da=1 -D b=2 --define c=3 --define=d=4 -De -Pprofile x"
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let properties = defines(&arguments);
        let expected: BTreeMap<String, String> = [
            ("a", "1"),
            ("b", "2"),
            ("c", "3"),
            ("d", "4"),
            ("e", "true"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
        assert_eq!(properties, expected);
    }
}
