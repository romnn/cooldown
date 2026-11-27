//! Maven's own verdict on an edit, from stock plugins at pinned versions run in a scratch copy of
//! the project.
//!
//! The copy holds the whole project, the POMs as the mutation prepared them, together with the
//! local parents outside it and a `.mvn` directory above it, each at its place relative to the
//! others, so Maven activates the same profiles and builds the same projects there as in the
//! project.
//! The user's settings, credentials, and environment pass through, since they decide what Maven
//! can resolve at all.
//! Maven, not cooldown's reading, names the projects it builds, and each run has to report each
//! of them exactly once, or the verdict counts for nothing.

use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::Driver;
use cooldown_core::{CoreError, Result, failure_detail};
use serde::Deserialize;

use super::reactor::{self, slash_path};

/// The goal that collects each project's dependency graph, at a pinned release so the verdict
/// never drifts.
///
/// Collection reads POMs but no other artifact, so a clean reactor whose modules depend on each
/// other resolves without building them first.
const DEPENDENCY_TREE: &str = "org.apache.maven.plugins:maven-dependency-plugin:3.8.1:tree";

/// The goal that writes the reactor's effective models: the projects Maven builds, with the
/// repositories each reads.
const EFFECTIVE_POM: &str = "org.apache.maven.plugins:maven-help-plugin:3.5.1:effective-pom";

/// The goal that writes the settings Maven runs with, mirrors included.
const EFFECTIVE_SETTINGS: &str =
    "org.apache.maven.plugins:maven-help-plugin:3.5.1:effective-settings";

/// The directories a copy leaves out: version control metadata, and Maven's build output.
const UNSTAGED: [&str; 5] = [".git", ".hg", ".svn", ".jj", "target"];

/// A scratch copy of a project, deleted when dropped.
pub(crate) struct Scratch {
    dir: tempfile::TempDir,
    /// Where the project root sits in the copy.
    root: Utf8PathBuf,
    /// What every report of this copy's runs ends in, unique to the copy, so no file the project
    /// holds passes for one.
    suffix: String,
}

/// Which of the two runs over a scratch copy, before and after the edit.
#[derive(Clone, Copy)]
pub(crate) enum Run {
    /// The project as prepared.
    Before,
    /// The edited project, which also reports the settings Maven ran with.
    After,
}

/// What one run of Maven reports.
#[derive(Debug, Clone)]
pub(crate) struct Verdict {
    /// Every project Maven builds, by its POM's path below the project root, `/`-separated.
    pub(crate) modules: BTreeMap<String, Module>,
    /// Whether a mirror stands in for Maven Central, from the [`Run::After`] run.
    pub(crate) central_mirrored: bool,
}

/// One project of the reactor as Maven resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Module {
    /// The project's `group:artifact:version`.
    pub(crate) gav: String,
    /// The version Maven selects for each direct dependency.
    pub(crate) dependencies: BTreeMap<Artifact, String>,
    /// The repositories of the project's effective model.
    pub(crate) repositories: Vec<Repository>,
}

/// The full identity of a dependency, which Maven keeps apart where `group:artifact` alone does
/// not: a `jar` and a `test-jar` of one artifact are two dependencies.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Artifact {
    /// The `group:artifact` coordinate.
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) classifier: String,
}

/// A repository of an effective model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Repository {
    pub(crate) id: Option<String>,
    pub(crate) url: Option<String>,
    /// Whether Maven fetches releases from it.
    pub(crate) releases: bool,
}

/// A node of the dependency plugin's JSON tree.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Node {
    group_id: String,
    artifact_id: String,
    version: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    classifier: String,
    #[serde(default)]
    children: Vec<Node>,
}

impl Scratch {
    /// Copies the [`sources`] of the project rooted at `root` and its local parents `external`
    /// into a scratch directory, with the owned POMs `owned`, by their root-relative paths, in
    /// place of the project's own.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] when a file to copy shares no ancestor with `root`, and a
    /// filesystem error when the copy cannot be written.
    pub(crate) fn stage<'a>(
        root: &Utf8Path,
        owned: impl Iterator<Item = (&'a Utf8Path, &'a str)>,
        external: &[&Utf8Path],
    ) -> Result<Self> {
        let sources = sources(root, external)?;
        // The copy keeps the layout below the deepest directory holding everything Maven reads.
        let mut ancestor = root.to_owned();
        for source in &sources {
            while !source.starts_with(&ancestor) {
                if !ancestor.pop() {
                    return Err(CoreError::Config(format!(
                        "{source} shares no directory with {root}, so cooldown cannot verify the \
                         project in a copy"
                    )));
                }
            }
        }
        let dir = tempfile::tempdir()?;
        let base = utf8(dir.path())?;
        let place = |path: &Utf8Path| -> Result<Utf8PathBuf> {
            let rel = path
                .strip_prefix(&ancestor)
                .map_err(|_| CoreError::System(format!("{path} lies outside {ancestor}")))?;
            Ok(base.join(rel))
        };
        // An owned POM is written once, as prepared, since a copy keeps a read-only POM's
        // permissions and could not be written over.
        let owned: BTreeMap<Utf8PathBuf, &str> =
            owned.map(|(rel, text)| (root.join(rel), text)).collect();
        for source in sources.iter().filter(|source| !owned.contains_key(*source)) {
            let staged = place(source)?;
            if let Some(parent) = staged.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(source, staged)?;
        }
        for (path, text) in &owned {
            write_file(&place(path)?, text.as_bytes())?;
        }
        let scratch_root = place(root)?;
        let suffix = format!(
            "cooldown{}",
            base.file_name()
                .unwrap_or_default()
                .replace(|character: char| !character.is_ascii_alphanumeric(), "")
        );
        Ok(Scratch {
            dir,
            root: scratch_root,
            suffix,
        })
    }

    /// Replaces the owned POM at the root-relative `rel` with `text`.
    pub(crate) fn write(&self, rel: &Utf8Path, text: &str) -> Result<()> {
        write_file(&self.root.join(rel), text.as_bytes())
    }

    /// Runs `run` over this copy with `mvn`, in the copy's project root, and binds each project
    /// Maven reports to the POM it built it from.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::System`] when Maven fails [`Run::Before`] or cannot report each of
    /// its projects exactly once there, since the project as it stands is then what Maven cannot
    /// resolve; for [`Run::After`], the driver's failure, or [`CoreError::UnacceptableResolve`]
    /// when the edit leaves Maven's report incomplete.
    /// Returns [`CoreError::LockUnreadable`] when a report cannot be read.
    pub(crate) async fn verdict(&self, run: Run, mvn: &Driver) -> Result<Verdict> {
        let base = utf8(self.dir.path())?;
        let (name, mut goals) = match run {
            Run::Before => ("before", vec![EFFECTIVE_POM]),
            Run::After => ("after", vec![EFFECTIVE_POM, EFFECTIVE_SETTINGS]),
        };
        goals.push(DEPENDENCY_TREE);
        let tree_suffix = format!(".{}-{name}.json", self.suffix);
        let mut args: Vec<String> = ["-B", "-ntp", "-q"].map(str::to_owned).to_vec();
        args.extend(goals.into_iter().map(str::to_owned));
        // Maven expands each expression per goal and per project, so each help goal writes a
        // report of its own, and each project's tree lands beside the POM Maven built it from.
        args.extend([
            format!(
                "-Doutput={}",
                base.join(format!("{name}-${{mojoExecution.goal}}.xml"))
            ),
            "-DoutputType=json".to_owned(),
            format!("-DoutputFile=${{project.file}}{tree_suffix}"),
        ]);
        let output = mvn.output_with_env(&self.root, &args, &[]).await?;
        if !output.status.success() {
            return Err(match run {
                Run::Before => CoreError::System(format!(
                    "Maven cannot resolve the project as it stands: {}",
                    failure_detail(&output)
                )),
                Run::After => mvn.failure(&output),
            });
        }
        let report = |goal: &str| -> Result<String> {
            std::fs::read_to_string(base.join(format!("{name}-{goal}.xml"))).map_err(|error| {
                CoreError::LockUnreadable(format!("Maven's {goal} report: {error}"))
            })
        };
        let roster = effective_models(&report("effective-pom")?)?;
        let mut trees = Vec::new();
        for path in files_below(&base)? {
            let Some(pom) = path.as_str().strip_suffix(&tree_suffix) else {
                continue;
            };
            let Ok(rel) = Utf8Path::new(pom).strip_prefix(&self.root) else {
                trees.push((None, read_tree(&path)?));
                continue;
            };
            trees.push((Some(slash_path(rel)), read_tree(&path)?));
        }
        let modules = bind(roster, trees).map_err(|detail| match run {
            Run::Before => CoreError::System(detail),
            Run::After => CoreError::UnacceptableResolve(format!("after the edit, {detail}")),
        })?;
        Ok(Verdict {
            modules,
            central_mirrored: match run {
                Run::Before => false,
                Run::After => mirrors_central(&report("effective-settings")?)?,
            },
        })
    }
}

impl Artifact {
    /// The artifact as Maven spells it, `group:artifact:type[:classifier]`.
    pub(crate) fn display(&self) -> String {
        if self.classifier.is_empty() {
            format!("{}:{}", self.name, self.kind)
        } else {
            format!("{}:{}:{}", self.name, self.kind, self.classifier)
        }
    }
}

/// One project's tree as the dependency plugin wrote it: its `group:artifact:version` and the
/// version it selects for each direct dependency.
struct Tree {
    gav: String,
    dependencies: BTreeMap<Artifact, String>,
}

/// Binds each project of Maven's `roster`, its effective models by `group:artifact:version`, to
/// the one tree reporting it, by the path of the POM the tree lies beside, `None` for one outside
/// the project.
///
/// # Errors
///
/// Returns why the reports do not cover the roster exactly once each: a project without a tree,
/// one with several, or a tree of a project Maven did not name or outside the project.
fn bind(
    mut roster: BTreeMap<String, Vec<Repository>>,
    trees: Vec<(Option<String>, Tree)>,
) -> std::result::Result<BTreeMap<String, Module>, String> {
    let mut modules = BTreeMap::new();
    for (pom, tree) in trees {
        let Some(repositories) = roster.remove(&tree.gav) else {
            return Err(format!(
                "Maven reports {} more than once or beside its effective models",
                tree.gav
            ));
        };
        let Some(pom) = pom else {
            return Err(format!("Maven reports {} outside the project", tree.gav));
        };
        let module = Module {
            gav: tree.gav,
            dependencies: tree.dependencies,
            repositories,
        };
        modules.insert(pom, module);
    }
    if let Some(gav) = roster.keys().next() {
        return Err(format!(
            "Maven builds {gav} but reports no dependencies for it"
        ));
    }
    Ok(modules)
}

fn read_tree(path: &Utf8Path) -> Result<Tree> {
    let text = std::fs::read_to_string(path)?;
    let root: Node = serde_json::from_str(&text)
        .map_err(|error| CoreError::LockUnreadable(format!("Maven's dependency tree: {error}")))?;
    Ok(Tree {
        gav: format!("{}:{}:{}", root.group_id, root.artifact_id, root.version),
        dependencies: root
            .children
            .into_iter()
            .map(|node| {
                let artifact = Artifact {
                    name: format!("{}:{}", node.group_id, node.artifact_id),
                    kind: node.kind,
                    classifier: node.classifier,
                };
                (artifact, node.version)
            })
            .collect(),
    })
}

/// The repositories of every model in Maven's effective POM, by its `group:artifact:version`.
fn effective_models(text: &str) -> Result<BTreeMap<String, Vec<Repository>>> {
    let document = roxmltree::Document::parse(text)
        .map_err(|error| CoreError::LockUnreadable(format!("Maven's effective POM: {error}")))?;
    let root = document.root_element();
    let projects: Vec<roxmltree::Node<'_, '_>> = if root.has_tag_name("projects") {
        named(root, "project").collect()
    } else {
        vec![root]
    };
    let mut models = BTreeMap::new();
    for project in projects {
        let field = |node: roxmltree::Node<'_, '_>, name: &str| {
            named(node, name)
                .next()
                .and_then(|child| child.text())
                .map(|text| text.trim().to_owned())
        };
        let parent = named(project, "parent").next();
        let inherited = |name: &str| field(project, name).or_else(|| field(parent?, name));
        let (Some(group), Some(artifact), Some(version)) = (
            inherited("groupId"),
            field(project, "artifactId"),
            inherited("version"),
        ) else {
            continue;
        };
        let declared = named(project, "repositories")
            .flat_map(|list| named(list, "repository"))
            .map(|repository| Repository {
                id: field(repository, "id"),
                url: field(repository, "url"),
                releases: super::pom::releases_enabled(repository),
            })
            .collect();
        models.insert(format!("{group}:{artifact}:{version}"), declared);
    }
    Ok(models)
}

/// Whether the effective settings route requests for Maven Central to a mirror.
///
/// A blocked mirror serves nothing, so it never stands in for Maven Central.
fn mirrors_central(text: &str) -> Result<bool> {
    let document = roxmltree::Document::parse(text).map_err(|error| {
        CoreError::LockUnreadable(format!("Maven's effective settings: {error}"))
    })?;
    Ok(named(document.root_element(), "mirrors")
        .flat_map(|mirrors| named(mirrors, "mirror"))
        .any(|mirror| {
            let text = |name: &'static str| {
                named(mirror, name)
                    .next()
                    .and_then(|node| node.text())
                    .map(str::trim)
            };
            text("blocked") != Some("true") && text("mirrorOf").is_some_and(matches_central)
        }))
}

/// Whether Maven applies a mirror whose `<mirrorOf>` is `pattern` to Maven Central, an external
/// `https` repository whose id is `central`.
///
/// Maven matches the comma-separated list in order: `!id` excludes a repository outright, `id` and
/// `*` match, `external:*` matches every repository off the local host, and `external:http:*` only
/// those that use plain `http`.
fn matches_central(pattern: &str) -> bool {
    let mut matched = false;
    for entry in pattern.split(',').map(str::trim) {
        match entry {
            "!central" => return false,
            "central" => return true,
            "*" | "external:*" => matched = true,
            _ => {}
        }
    }
    matched
}

/// The child elements of `node` named `name`, whatever their namespace.
fn named<'a, 'input: 'a>(
    node: roxmltree::Node<'a, 'input>,
    name: &'a str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'input>> + 'a {
    node.children()
        .filter(move |child| child.is_element() && child.tag_name().name() == name)
}

fn write_file(path: &Utf8Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}

fn utf8(path: &std::path::Path) -> Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(path.to_path_buf())
        .map_err(|path| CoreError::PathEncoding(format!("{} is not valid UTF-8", path.display())))
}

/// Every file a Maven build started in `root` can read from the project's side, by absolute
/// path: the project's own files, its local parents `external` outside it, and a `.mvn`
/// directory the launcher finds above it.
///
/// Both the copy an apply verifies in and the copy a preview runs in hold exactly these, so
/// Maven activates the same profiles and builds the same projects in each as in the project.
pub(crate) fn sources(root: &Utf8Path, external: &[&Utf8Path]) -> Result<Vec<Utf8PathBuf>> {
    let mut sources = files_below(root)?;
    sources.extend(external.iter().map(|path| (*path).to_owned()));
    if let Some(config) = reactor::maven_config_dir(root).filter(|config| !config.starts_with(root))
    {
        sources.extend(files_below(&config)?);
    }
    Ok(sources)
}

/// Every file below `dir`, a symbolic link to one included, leaving out [`UNSTAGED`]
/// directories.
///
/// # Errors
///
/// Returns [`CoreError::System`] for a symbolic link to a directory, which could loop, or lead
/// Maven's reports out of the copy into the project, and a filesystem error when a directory
/// cannot be read.
fn files_below(dir: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(current) = pending.pop() {
        for entry in current.read_dir_utf8()? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_file() {
                files.push(entry.into_path());
                continue;
            }
            if UNSTAGED.contains(&entry.file_name()) {
                continue;
            }
            let path = entry.into_path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_symlink() {
                match std::fs::metadata(&path) {
                    Ok(target) if target.is_dir() => {
                        let rel = path.strip_prefix(dir).unwrap_or(Utf8Path::new(""));
                        return Err(CoreError::System(format!(
                            "{} is a symbolic link to a directory, which cooldown does not \
                             follow into the copy it verifies the project in",
                            slash_path(rel)
                        )));
                    }
                    Ok(target) if target.is_file() => files.push(path),
                    // A link to nothing is no file to Maven either.
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        return Err(error.into());
                    }
                    _ => {}
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    /// A mirror stands in for Maven Central when its pattern names it or matches every external
    /// repository, unless the pattern excludes it, matches only plain `http`, or the mirror is
    /// blocked.
    #[test]
    fn mirror_patterns_that_cover_central() -> Result<()> {
        for pattern in ["*", "central", "external:*", "corp,central", "*,!snapshots"] {
            assert!(matches_central(pattern), "{pattern}");
        }
        for pattern in [
            "external:http:*",
            "corp",
            "*,!central",
            "!central,*",
            "external:*,!central",
        ] {
            assert!(!matches_central(pattern), "{pattern}");
        }

        let settings = |mirrors: &str| {
            format!(
                r#"<settings xmlns="http://maven.apache.org/SETTINGS/1.2.0"><mirrors>{mirrors}</mirrors></settings>"#
            )
        };
        assert!(mirrors_central(&settings(
            "<mirror><mirrorOf>external:*</mirrorOf><id>corp</id></mirror>"
        ))?);
        assert!(!mirrors_central(&settings(indoc! {"
            <mirror><mirrorOf>external:http:*</mirrorOf><blocked>true</blocked></mirror>
            <mirror><mirrorOf>*</mirrorOf><blocked>true</blocked></mirror>
        "}))?);
        Ok(())
    }

    /// One model or several: each model lands under its `group:artifact:version`, the group and
    /// version inherited from its parent where the model names none, with its repositories.
    #[test]
    fn effective_models_by_coordinates() -> Result<()> {
        let single = indoc! {r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <groupId>demo</groupId>
              <artifactId>demo</artifactId>
              <version>0.1.0</version>
              <repositories>
                <repository><id>central</id><url>https://repo.maven.apache.org/maven2</url></repository>
              </repositories>
            </project>
        "#};
        let several = indoc! {"
            <projects>
              <project><groupId>demo</groupId><artifactId>parent</artifactId><version>1</version></project>
              <project>
                <parent><groupId>demo</groupId><version>1</version></parent>
                <artifactId>app</artifactId>
                <repositories><repository><id>corp</id></repository></repositories>
              </project>
            </projects>
        "};

        let single = effective_models(single)?;
        let several = effective_models(several)?;

        assert_eq!(
            single["demo:demo:0.1.0"],
            [Repository {
                id: Some("central".into()),
                url: Some("https://repo.maven.apache.org/maven2".into()),
                releases: true,
            }]
        );
        assert_eq!(several["demo:parent:1"], []);
        assert_eq!(several["demo:app:1"][0].id.as_deref(), Some("corp"));
        Ok(())
    }

    /// A project at `project` below a fresh directory, holding a `pom.xml` and an empty
    /// `markers/on`.
    fn project() -> Result<(tempfile::TempDir, Utf8PathBuf)> {
        let dir = tempfile::tempdir()?;
        let root = utf8(dir.path())?.join("project");
        std::fs::create_dir_all(root.join("markers"))?;
        std::fs::write(root.join("pom.xml"), "<project/>")?;
        std::fs::write(root.join("markers/on"), "")?;
        Ok((dir, root))
    }

    /// A symbolic link to a file reaches the copy as the file it leads to, so a marker that
    /// activates a profile activates it there too.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_file_is_copied_as_the_file() -> Result<()> {
        let (_dir, root) = project()?;
        std::os::unix::fs::symlink("markers/on", root.join("extra.enabled"))?;

        let scratch = Scratch::stage(&root, std::iter::empty(), &[])?;

        assert!(std::fs::symlink_metadata(scratch.root.join("extra.enabled"))?.is_file());
        Ok(())
    }

    /// A symbolic link to a directory refuses the copy, naming the link but not where the
    /// project lies.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_directory_refuses_the_copy() -> Result<()> {
        let (_dir, root) = project()?;
        std::os::unix::fs::symlink("markers", root.join("linked"))?;

        let refused = sources(&root, &[]);

        assert!(
            matches!(&refused, Err(CoreError::System(detail))
                if detail.starts_with("linked ") && !detail.contains(root.as_str())),
            "{refused:?}"
        );
        Ok(())
    }

    /// An owned POM reaches the copy as prepared even where the project's own file is read-only.
    #[test]
    fn a_read_only_owned_pom_is_staged_as_prepared() -> Result<()> {
        let (_dir, root) = project()?;
        let pom = root.join("pom.xml");
        let mut permissions = std::fs::metadata(&pom)?.permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&pom, permissions)?;
        let prepared = "<project><!-- prepared --></project>";

        let scratch = Scratch::stage(
            &root,
            [(Utf8Path::new("pom.xml"), prepared)].into_iter(),
            &[],
        )?;

        assert_eq!(
            std::fs::read_to_string(scratch.root.join("pom.xml"))?,
            prepared
        );
        Ok(())
    }

    /// A tree reporting `gav`.
    fn tree(gav: &str) -> Tree {
        Tree {
            gav: gav.to_owned(),
            dependencies: BTreeMap::new(),
        }
    }

    /// Each project Maven names binds to exactly one tree, by its POM, while a project without a
    /// tree, a second tree of one project, a tree of a project Maven did not name, and one outside
    /// the project leave the report incomplete.
    #[test]
    fn bind_needs_exactly_one_tree_per_project() {
        let roster = || -> BTreeMap<String, Vec<Repository>> {
            ["demo:parent:1", "demo:app:1"]
                .into_iter()
                .map(|gav| (gav.to_owned(), Vec::new()))
                .collect()
        };
        let at = |pom: &str, gav: &str| (Some(pom.to_owned()), tree(gav));

        let bound = bind(
            roster(),
            vec![
                at("pom.xml", "demo:parent:1"),
                at("app/extra.xml", "demo:app:1"),
            ],
        );
        assert_eq!(
            bound.map(|modules| modules.into_keys().collect::<Vec<_>>()),
            Ok(vec!["app/extra.xml".to_owned(), "pom.xml".to_owned()])
        );

        for trees in [
            vec![at("pom.xml", "demo:parent:1")],
            vec![
                at("pom.xml", "demo:parent:1"),
                at("app/pom.xml", "demo:app:1"),
                at("app/extra.xml", "demo:app:1"),
            ],
            vec![
                at("pom.xml", "demo:parent:1"),
                at("app/pom.xml", "demo:app:1"),
                at("extra/pom.xml", "demo:extra:1"),
            ],
            vec![at("pom.xml", "demo:parent:1"), (None, tree("demo:app:1"))],
        ] {
            assert!(bind(roster(), trees).is_err());
        }
    }
}
