//! One `pom.xml` read the way Maven's model reader reads it, keeping where each version sits in the
//! file so that an edit can replace exactly its text and leave every other byte alone.
//!
//! Only the project's own elements count: a declaration inside a comment, a profile, or a build
//! plugin is no project dependency, so the reader never mistakes one for a dependency.

use std::collections::BTreeMap;
use std::ops::Range;

use roxmltree::Node;

/// The namespace every POM element lives in, followed by the model version, as in
/// `http://maven.apache.org/POM/4.0.0`.
const POM_NAMESPACE: &str = "http://maven.apache.org/POM/";

/// An element's text as Maven reads it, and the bytes that spell it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Text {
    /// The trimmed text, comments left out.
    pub(crate) value: String,
    /// Where `value` sits in the file, when one plain text node spells it.
    ///
    /// `None` when a comment, a CDATA section or a character reference shares the element, since
    /// writing a new value over such an element would have to rewrite its markup too.
    pub(crate) span: Option<Range<usize>>,
}

/// A `<dependency>` element of `dependencies` or `dependencyManagement`, before interpolation.
#[derive(Debug, Clone, Default)]
pub(crate) struct Declared {
    pub(crate) group: Option<String>,
    pub(crate) artifact: Option<String>,
    /// The `<type>`, which Maven defaults to `jar`.
    pub(crate) kind: Option<String>,
    pub(crate) classifier: Option<String>,
    pub(crate) scope: Option<String>,
    pub(crate) version: Option<Text>,
}

/// The `<parent>` element.
#[derive(Debug, Clone)]
pub(crate) struct ParentRef {
    pub(crate) group: Option<String>,
    pub(crate) artifact: Option<String>,
    pub(crate) version: Option<Text>,
    /// The `<relativePath>`: `None` when absent, which Maven reads as `../pom.xml`, and an empty
    /// string when the POM turns the local lookup off.
    pub(crate) relative_path: Option<String>,
}

/// A `<repository>` of the project's `<repositories>`.
#[derive(Debug, Clone)]
pub(crate) struct Repository {
    pub(crate) id: Option<String>,
    pub(crate) url: Option<String>,
    /// Whether Maven fetches releases from it, which a repository for snapshots alone turns off.
    pub(crate) releases: bool,
}

/// What cooldown reads from one POM.
#[derive(Debug, Clone, Default)]
pub(crate) struct Pom {
    pub(crate) group: Option<String>,
    pub(crate) artifact: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) parent: Option<ParentRef>,
    /// The project's `<properties>`, the last definition of a repeated name winning as it does in
    /// Maven.
    pub(crate) properties: BTreeMap<String, Text>,
    /// How often each property is defined, in the project's `<properties>` and in every profile's,
    /// since an active profile's definition outranks the project's.
    pub(crate) definitions: BTreeMap<String, usize>,
    pub(crate) modules: Vec<String>,
    pub(crate) dependencies: Vec<Declared>,
    /// The `<dependencyManagement>` entries.
    pub(crate) managed: Vec<Declared>,
    /// The dependencies and management entries of every profile, which an active profile adds to
    /// the project's or puts in their place.
    pub(crate) profiled: Vec<Declared>,
    /// The project's repositories and every profile's, since cooldown cannot tell which profile
    /// Maven activates.
    pub(crate) repositories: Vec<Repository>,
    /// How often each `${name}` occurs in the file outside comments, anywhere Maven interpolates.
    pub(crate) references: BTreeMap<String, usize>,
}

/// Parses `text` as a POM.
///
/// # Errors
///
/// Returns the XML parser's message when `text` is not well-formed XML, and a message of its own
/// when its root element is not a `<project>`.
pub(crate) fn parse(text: &str) -> Result<Pom, String> {
    let document = roxmltree::Document::parse(text).map_err(|error| error.to_string())?;
    let project = document.root_element();
    if !is_pom(project, "project") {
        return Err("the root element is not a Maven <project>".to_owned());
    }
    let mut pom = Pom {
        group: value(project, "groupId"),
        artifact: value(project, "artifactId"),
        version: value(project, "version"),
        parent: child(project, "parent").map(|parent| ParentRef {
            group: value(parent, "groupId"),
            artifact: value(parent, "artifactId"),
            version: child(parent, "version").map(|version| text_of(version, text)),
            // An empty `<relativePath/>` turns the local lookup off, which an absent one does not.
            relative_path: child(parent, "relativePath")
                .map(|path| text_content(path).trim().to_owned()),
        }),
        modules: children(child(project, "modules"), "module")
            .filter_map(|module| text_value(module))
            .collect(),
        dependencies: dependencies(child(project, "dependencies"), text),
        managed: dependencies(
            child(project, "dependencyManagement")
                .and_then(|managed| child(managed, "dependencies")),
            text,
        ),
        repositories: repositories(project),
        ..Pom::default()
    };
    for property in elements(child(project, "properties")) {
        let name = property.tag_name().name().to_owned();
        *pom.definitions.entry(name.clone()).or_default() += 1;
        pom.properties.insert(name, text_of(property, text));
    }
    for profile in children(child(project, "profiles"), "profile") {
        for property in elements(child(profile, "properties")) {
            *pom.definitions
                .entry(property.tag_name().name().to_owned())
                .or_default() += 1;
        }
        pom.profiled
            .extend(dependencies(child(profile, "dependencies"), text));
        pom.profiled.extend(dependencies(
            child(profile, "dependencyManagement")
                .and_then(|managed| child(managed, "dependencies")),
            text,
        ));
        pom.repositories.extend(repositories(profile));
    }
    let mut interpolated: Vec<&str> = Vec::new();
    for node in project.descendants() {
        if node.is_text() {
            interpolated.extend(node.text());
        } else if node.is_element() {
            interpolated.extend(node.attributes().map(|attribute| attribute.value()));
        }
    }
    for piece in interpolated {
        for name in references(piece) {
            *pom.references.entry(name.to_owned()).or_default() += 1;
        }
    }
    Ok(pom)
}

/// The names of the `${name}` expressions in `text`, in order.
pub(crate) fn references(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        let start = rest.find("${")?;
        let after = rest.get(start + 2..)?;
        let end = after.find('}')?;
        let name = after.get(..end)?;
        rest = after.get(end + 1..)?;
        Some(name)
    })
}

/// The name `text` refers to when it is exactly one `${name}` expression.
pub(crate) fn whole_reference(text: &str) -> Option<&str> {
    let name = text.strip_prefix("${")?.strip_suffix('}')?;
    (!name.is_empty() && !name.contains(['$', '{', '}'])).then_some(name)
}

/// Whether the `<repository>` element `repository` serves releases, which it does unless its
/// `<releases>` say `<enabled>false</enabled>`.
///
/// Matching by local name alone lets the same reading serve Maven's own effective POM.
pub(crate) fn releases_enabled(repository: Node<'_, '_>) -> bool {
    fn named<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
        node.children()
            .find(|child| child.is_element() && child.tag_name().name() == name)
    }
    named(repository, "releases")
        .and_then(|releases| named(releases, "enabled"))
        .is_none_or(|enabled| text_content(enabled).trim() != "false")
}

/// Whether `node` is the POM element `name`.
///
/// Maven's own documents use the POM namespace, and hand-written ones often none at all, so both
/// count, while an element of another namespace is an extension Maven does not read as a field.
fn is_pom(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element()
        && node.tag_name().name() == name
        && node
            .tag_name()
            .namespace()
            .is_none_or(|namespace| namespace.starts_with(POM_NAMESPACE))
}

fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    node.children().find(|candidate| is_pom(*candidate, name))
}

fn children<'a, 'input: 'a>(
    node: Option<Node<'a, 'input>>,
    name: &'a str,
) -> impl Iterator<Item = Node<'a, 'input>> + 'a {
    node.into_iter()
        .flat_map(|node| node.children())
        .filter(move |candidate| is_pom(*candidate, name))
}

/// Every POM element directly below `node`, such as each property of `<properties>`.
fn elements<'a, 'input: 'a>(
    node: Option<Node<'a, 'input>>,
) -> impl Iterator<Item = Node<'a, 'input>> + 'a {
    node.into_iter()
        .flat_map(|node| node.children())
        .filter(|candidate| {
            candidate.is_element()
                && candidate
                    .tag_name()
                    .namespace()
                    .is_none_or(|namespace| namespace.starts_with(POM_NAMESPACE))
        })
}

fn value(node: Node<'_, '_>, name: &str) -> Option<String> {
    child(node, name).and_then(text_value)
}

/// The trimmed text of `node`, or `None` when it is empty, which Maven reads as absent.
fn text_value(node: Node<'_, '_>) -> Option<String> {
    let text = text_content(node);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The text Maven's reader takes from `node`: every text child, comments skipped.
fn text_content(node: Node<'_, '_>) -> String {
    node.children()
        .filter(Node::is_text)
        .filter_map(|child| child.text())
        .collect()
}

/// The [`Text`] of the element `node` in the document `source`.
fn text_of(node: Node<'_, '_>, source: &str) -> Text {
    let content = text_content(node);
    let value = content.trim().to_owned();
    let mut nodes = node.children();
    let span = match (nodes.next(), nodes.next()) {
        (Some(only), None) if only.is_text() => {
            let range = only.range();
            // A character reference or a CDATA section decodes to other text than it spells, so
            // only a node whose bytes are its text can be rewritten in place.
            source
                .get(range.clone())
                .filter(|raw| *raw == content)
                .map(|raw| {
                    let start = range.start + (raw.len() - raw.trim_start().len());
                    start..start + value.len()
                })
        }
        _ => None,
    };
    Text { value, span }
}

/// The `<repositories>` of `node`, a project or a profile.
fn repositories(node: Node<'_, '_>) -> Vec<Repository> {
    children(child(node, "repositories"), "repository")
        .map(|repository| Repository {
            id: value(repository, "id"),
            url: value(repository, "url"),
            releases: releases_enabled(repository),
        })
        .collect()
}

fn dependencies(node: Option<Node<'_, '_>>, source: &str) -> Vec<Declared> {
    children(node, "dependency")
        .map(|dependency| Declared {
            group: value(dependency, "groupId"),
            artifact: value(dependency, "artifactId"),
            kind: value(dependency, "type"),
            classifier: value(dependency, "classifier"),
            scope: value(dependency, "scope"),
            version: child(dependency, "version").map(|version| text_of(version, source)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    /// Only the project's own dependencies and management entries are read: a commented-out
    /// dependency, a profile's, and a build plugin's are not, and each version keeps the exact
    /// bytes that spell it.
    #[test]
    fn reads_only_the_projects_own_declarations() -> Result<(), String> {
        let text = indoc! {r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <properties>
                <text.version> 1.10.0 </text.version>
              </properties>
              <dependencies>
                <!--
                <dependency>
                  <groupId>org.slf4j</groupId>
                  <artifactId>slf4j-api</artifactId>
                  <version>1.7.30</version>
                </dependency>
                -->
                <dependency>
                  <groupId>org.apache.commons</groupId>
                  <artifactId>commons-text</artifactId>
                  <version>${text.version}</version>
                </dependency>
              </dependencies>
              <dependencyManagement>
                <dependencies>
                  <dependency>
                    <groupId>org.apache.commons</groupId>
                    <artifactId>commons-lang3</artifactId>
                    <version>3.12.0</version>
                    <type>test-jar</type>
                  </dependency>
                </dependencies>
              </dependencyManagement>
              <profiles>
                <profile>
                  <properties><text.version>1.11.0</text.version></properties>
                  <dependencies>
                    <dependency>
                      <groupId>com.google.guava</groupId>
                      <artifactId>guava</artifactId>
                      <version>33.0.0-jre</version>
                    </dependency>
                  </dependencies>
                </profile>
              </profiles>
              <build>
                <plugins>
                  <plugin>
                    <dependencies>
                      <dependency>
                        <groupId>org.ow2.asm</groupId>
                        <artifactId>asm</artifactId>
                        <version>9.6</version>
                      </dependency>
                    </dependencies>
                  </plugin>
                </plugins>
              </build>
            </project>
        "#};

        let pom = parse(text)?;

        // One dependency and one management entry; the commented, profile and plugin ones are
        // invisible.
        let artifacts: Vec<_> = pom
            .dependencies
            .iter()
            .map(|d| d.artifact.clone())
            .collect();
        assert_eq!(artifacts, [Some("commons-text".to_owned())]);
        let managed = &pom.managed[0];
        assert_eq!(managed.kind.as_deref(), Some("test-jar"));
        let version = managed.version.clone().ok_or("a managed version")?;
        assert_eq!(text.get(version.span.ok_or("a span")?), Some("3.12.0"));

        // The property's span covers its trimmed value, and the profile's redefinition counts.
        let property = &pom.properties["text.version"];
        assert_eq!(property.value, "1.10.0");
        assert_eq!(
            text.get(property.span.clone().ok_or("a span")?),
            Some("1.10.0")
        );
        assert_eq!(pom.definitions["text.version"], 2);
        // The reference inside the comment is not one Maven interpolates.
        assert_eq!(pom.references["text.version"], 1);
        Ok(())
    }

    /// A version spelled with a comment or a character reference is read as Maven reads it, but
    /// offers no span to rewrite.
    #[test]
    fn a_version_with_markup_has_no_span() -> Result<(), String> {
        let pom = parse(indoc! {r"
            <project>
              <dependencies>
                <dependency><artifactId>a</artifactId><version>1.0<!-- x -->.1</version></dependency>
                <dependency><artifactId>b</artifactId><version>1&#46;2</version></dependency>
                <dependency><artifactId>c</artifactId><version>1.3</version></dependency>
              </dependencies>
            </project>
        "})?;

        let versions: Vec<_> = pom
            .dependencies
            .iter()
            .filter_map(|dependency| dependency.version.clone())
            .map(|version| (version.value, version.span.is_some()))
            .collect();
        assert_eq!(
            versions,
            [
                ("1.0.1".to_owned(), false),
                ("1.2".to_owned(), false),
                ("1.3".to_owned(), true),
            ]
        );
        Ok(())
    }

    /// An element of another namespace is no POM field, while the POM's own namespace and no
    /// namespace both are.
    #[test]
    fn elements_of_another_namespace_are_not_read() -> Result<(), String> {
        let pom = parse(indoc! {r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0" xmlns:x="urn:other">
              <dependencies>
                <x:dependency><artifactId>other</artifactId></x:dependency>
                <dependency><artifactId>own</artifactId></dependency>
              </dependencies>
            </project>
        "#})?;

        assert_eq!(pom.dependencies.len(), 1);
        assert_eq!(pom.dependencies[0].artifact.as_deref(), Some("own"));
        assert!(parse("<settings/>").is_err());
        Ok(())
    }

    #[test]
    fn references_and_whole_references() {
        assert_eq!(references("${a}.${b}-x").collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(whole_reference("${a.b}"), Some("a.b"));
        assert_eq!(whole_reference("${a}.${b}"), None);
        assert_eq!(whole_reference("1.0"), None);
    }
}
