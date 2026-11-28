//! The pixi manifest a project declares its dependencies in: `pixi.toml`, or the
//! `pyproject.toml` that hosts pixi's tables under `[tool.pixi]` and its PyPI requirements under
//! `[project]` and `[dependency-groups]`.
//!
//! cooldown reads which packages it declares, in any feature or target, and edits only the
//! top-level tables of a scratch copy to force a landing's targets, leaving how features combine
//! to pixi; the project's own manifest is never written.

use super::lock::{Kind, normal};
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, NativePolicyLayer, NativeRule, RawWindow, Result, Selector};
use cooldown_uv::pypi::PYPI;
use cooldown_uv::requirement;
use jiff::Timestamp;
use toml_edit::{DocumentMut, InlineTable, Item, TableLike, Value};

/// The keys of a requirement table that take a package from elsewhere than a package index, or
/// inherit it from the workspace.
const NOT_REGISTRY: [&str; 4] = ["path", "git", "url", "workspace"];

/// The dependency tables of one pixi scope, with the ecosystem each declares packages of.
const TABLES: [(&str, Kind); 4] = [
    ("dependencies", Kind::Conda),
    ("host-dependencies", Kind::Conda),
    ("build-dependencies", Kind::Conda),
    ("pypi-dependencies", Kind::Pypi),
];

/// A pixi manifest, as written at a project root.
#[derive(Clone)]
pub(crate) struct Manifest {
    /// The manifest's file name at the project root.
    pub file: &'static str,
    /// The manifest's text as written.
    pub text: String,
    document: DocumentMut,
    /// The keys from the document's root to pixi's own tables.
    pixi: Vec<String>,
    /// Every requirement the manifest declares, in any scope.
    declarations: Vec<Declaration>,
    /// The `find-links` entries of every `pypi-options` table.
    links: Vec<Link>,
}

/// One requirement a manifest declares on a package.
#[derive(Clone)]
pub(crate) struct Declaration {
    kind: Kind,
    /// The [`normal`] name.
    name: String,
    /// Where a top-level PyPI requirement on a package index sits, which cooldown can narrow, or
    /// `None` for any other.
    pub location: Option<Location>,
}

/// Where a top-level PyPI requirement sits in a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Location {
    /// A key of `[pypi-dependencies]`, by the keys from the document's root down to it.
    Key(Vec<String>),
    /// A PEP 508 requirement of `[project]`'s dependencies, by its index there.
    Item(usize),
}

/// One `find-links` entry.
#[derive(Clone)]
enum Link {
    Path(String),
    Url(String),
}

impl Manifest {
    /// The manifest of the pixi project at `root`, or `None` when it has none: a `pixi.toml`, or
    /// else a `pyproject.toml` with a `[tool.pixi]` table.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Parse`] when the manifest is not valid TOML, and the I/O error of a
    /// manifest that cannot be read.
    pub(crate) fn read(root: &Utf8Path) -> Result<Option<Self>> {
        for file in ["pixi.toml", "pyproject.toml"] {
            let path = root.join(file);
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err.into()),
            };
            let manifest = Manifest::parse(file, text)
                .map_err(|err| CoreError::Parse(format!("{path}: {err}")))?;
            if manifest.is_some() {
                return Ok(manifest);
            }
        }
        Ok(None)
    }

    /// The manifest `text` of `file`, or `None` when it holds no pixi tables.
    ///
    /// # Errors
    ///
    /// Returns the parse error of `text` that is not valid TOML.
    pub(crate) fn parse(
        file: &'static str,
        text: String,
    ) -> std::result::Result<Option<Self>, toml_edit::TomlError> {
        let document: DocumentMut = text.parse()?;
        let pixi: Vec<String> = if file == "pixi.toml" {
            Vec::new()
        } else {
            vec!["tool".to_owned(), "pixi".to_owned()]
        };
        if table(&document, &pixi).is_none() {
            return Ok(None);
        }
        let mut manifest = Manifest {
            file,
            text,
            document,
            pixi,
            declarations: Vec::new(),
            links: Vec::new(),
        };
        manifest.walk();
        Ok(Some(manifest))
    }

    /// Finds every requirement and `find-links` entry of the manifest.
    fn walk(&mut self) {
        let document = &self.document;
        let root = keys(&self.pixi, &[]);
        let mut scopes = vec![root.clone()];
        for (feature, _) in entries(item(document, &keys(&root, &["feature"]))) {
            scopes.push(keys(&root, &["feature", feature]));
        }
        for scope in scopes.clone() {
            for (target, _) in entries(item(document, &keys(&scope, &["target"]))) {
                scopes.push(keys(&scope, &["target", target]));
            }
        }
        // An inline environment's own requirements count for attribution alone.
        let mut declaring = scopes.clone();
        for (environment, _) in entries(item(document, &keys(&root, &["environments"]))) {
            declaring.push(keys(&root, &["environments", environment]));
        }
        for scope in &declaring {
            for (key, kind) in TABLES {
                let at = keys(scope, &[key]);
                for (name, declared) in entries(item(document, &at)) {
                    let narrowable = *scope == root && kind == Kind::Pypi && registry(declared);
                    self.declarations.push(Declaration {
                        kind,
                        name: normal(kind, name),
                        location: narrowable.then(|| Location::Key(keys(&at, &[name]))),
                    });
                }
            }
        }
        // `[project]` is the older spelling of `[workspace]`.
        let workspace = [keys(&root, &["workspace"]), keys(&root, &["project"])];
        for scope in workspace.into_iter().chain(scopes) {
            let links = item(document, &keys(&scope, &["pypi-options", "find-links"]));
            // An inline array of tables and an array of tables spell the same entries.
            let tables: Vec<&dyn TableLike> = match links {
                Some(Item::ArrayOfTables(tables)) => {
                    tables.iter().map(|table| table as &dyn TableLike).collect()
                }
                links => links
                    .and_then(Item::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_inline_table)
                    .map(|table| table as &dyn TableLike)
                    .collect(),
            };
            for link in tables {
                let path = link.get("path").and_then(Item::as_str);
                let url = link.get("url").and_then(Item::as_str);
                self.links
                    .extend(path.map(|path| Link::Path(path.to_owned())));
                self.links.extend(url.map(|url| Link::Url(url.to_owned())));
            }
        }
        if self.file == "pyproject.toml" {
            self.requirements(&keys(&[], &["project", "dependencies"]), true);
            for parent in [
                &["project", "optional-dependencies"][..],
                &["dependency-groups"],
            ] {
                let parent = keys(&[], parent);
                let groups: Vec<String> = entries(item(&self.document, &parent))
                    .map(|(group, _)| group.to_owned())
                    .collect();
                for group in groups {
                    self.requirements(&keys(&parent, &[&group]), false);
                }
            }
        }
    }

    /// Adds the PEP 508 requirements of the array at `list`, of `[project]`'s own dependencies
    /// when `top_level`.
    fn requirements(&mut self, list: &[String], top_level: bool) {
        let lines = item(&self.document, list)
            .and_then(Item::as_array)
            .into_iter();
        for (index, line) in lines.flatten().enumerate() {
            let Some(parsed) = line.as_str().and_then(requirement::parse) else {
                continue;
            };
            let narrowable = top_level && !parsed.specifier.starts_with('@');
            self.declarations.push(Declaration {
                kind: Kind::Pypi,
                name: normal(Kind::Pypi, parsed.name),
                location: narrowable.then_some(Location::Item(index)),
            });
        }
    }

    /// Every requirement the manifest declares on the `kind` package `name`, in any scope.
    pub(crate) fn declarations(&self, kind: Kind, name: &str) -> Vec<&Declaration> {
        let name = normal(kind, name);
        self.declarations
            .iter()
            .filter(|declaration| declaration.kind == kind && declaration.name == name)
            .collect()
    }

    /// The `find-links` directories pixi reads, as the manifest names them relative to the
    /// project root.
    ///
    /// # Errors
    ///
    /// Returns why a copy of the project cannot hold an entry: an absolute path or a `file:` URL.
    pub(crate) fn find_links(&self) -> std::result::Result<Vec<&str>, String> {
        let mut paths = Vec::new();
        for link in &self.links {
            match link {
                Link::Path(path) if !Utf8Path::new(path).is_absolute() => paths.push(path.as_str()),
                Link::Url(url) if !url.starts_with("file:") => {}
                Link::Path(local) | Link::Url(local) => {
                    return Err(format!(
                        "{} names the find-links directory {local}, which cooldown cannot copy \
                         beside the project",
                        self.file
                    ));
                }
            }
        }
        Ok(paths)
    }

    /// Why the top-level constraint written on the conda package `name` cannot be narrowed to `to`
    /// safely, or `None` when it can or none is written.
    pub(crate) fn constraint_refusal(&self, name: &str, to: &str) -> Option<String> {
        let mut written = item(&self.document, &keys(&self.pixi, &["constraints", name]))?.clone();
        narrow_item(&mut written, to, narrowed_conda)
            .is_none()
            .then(|| {
                format!(
                    "{} constrains {name} in a form cooldown cannot narrow to {to}",
                    self.file
                )
            })
    }

    /// Adds a top-level constraint that holds the conda package `name` at exactly `to`, narrowing
    /// one written there already while keeping its build and channel.
    ///
    /// # Errors
    ///
    /// Returns why the constraint cannot be narrowed safely: it is no version requirement, or one
    /// with alternatives a narrowing would not bind.
    pub(crate) fn constrain(&mut self, name: &str, to: &str) -> std::result::Result<(), String> {
        let refusal = || {
            format!(
                "{} constrains {name} in a form cooldown cannot narrow to {to}",
                self.file
            )
        };
        let constraints = table_mut(&mut self.document, &self.pixi)
            .and_then(|pixi| subtable(pixi, "constraints"))
            .ok_or_else(refusal)?;
        let constraint = constraints
            .entry(name)
            .or_insert(Item::Value(Value::from("*")));
        narrow_item(constraint, to, narrowed_conda).ok_or_else(refusal)
    }

    /// Narrows the top-level PyPI requirement at `location` to exactly `to`, keeping its extras,
    /// markers and index.
    ///
    /// # Errors
    ///
    /// Returns why the requirement cannot be narrowed: it is not a requirement on a package
    /// index.
    pub(crate) fn narrow(
        &mut self,
        location: &Location,
        to: &str,
    ) -> std::result::Result<(), String> {
        let refusal = || format!("cooldown cannot narrow a requirement of {}", self.file);
        match location {
            Location::Key(keys) => item_mut(&mut self.document, keys)
                .and_then(|item| narrow_item(item, to, |spec, to| Some(narrowed(spec, to))))
                .ok_or_else(refusal),
            Location::Item(index) => {
                let list = keys(&[], &["project", "dependencies"]);
                let array = item_mut(&mut self.document, &list)
                    .and_then(Item::as_array_mut)
                    .ok_or_else(refusal)?;
                let parsed = array
                    .get(*index)
                    .and_then(Value::as_str)
                    .and_then(requirement::parse)
                    .filter(|parsed| !parsed.specifier.starts_with('@'))
                    .ok_or_else(refusal)?;
                let extras = parsed
                    .extras
                    .map(|extras| format!("[{extras}]"))
                    .unwrap_or_default();
                let marker = parsed
                    .marker
                    .map(|marker| format!(" ; {marker}"))
                    .unwrap_or_default();
                let spec = narrowed(parsed.specifier, to);
                let line = format!("{}{extras}{spec}{marker}", parsed.name);
                array.replace(*index, line);
                Ok(())
            }
        }
    }

    /// Makes pixi skip every release uploaded after `cutoff`, apart from what `exemptions` let
    /// through, by package name.
    pub(crate) fn exclude_newer(&mut self, cutoff: Timestamp, exemptions: InlineTable) {
        let Some(pixi) = table_mut(&mut self.document, &self.pixi) else {
            return;
        };
        // `[project]` is the older spelling of `[workspace]`.
        let key = if pixi.contains_key("project") && !pixi.contains_key("workspace") {
            "project"
        } else {
            "workspace"
        };
        let Some(workspace) = subtable(pixi, key) else {
            return;
        };
        let mut setting = InlineTable::from_iter([("cutoff", Value::from(cutoff.to_string()))]);
        if !exemptions.is_empty() {
            setting.insert("exemptions", Value::InlineTable(exemptions));
        }
        workspace.insert("exclude-newer", Item::Value(Value::InlineTable(setting)));
    }

    /// pixi's own cutoffs as cooldown windows: the workspace's `exclude-newer` for every package,
    /// and its `pypi-exclude-newer` for PyPI packages.
    ///
    /// A cutoff cooldown cannot read is left out with a warning, so a spelling pixi accepts and
    /// cooldown does not never stops cooldown from judging the project.
    pub(crate) fn native_policy(&self) -> Option<NativePolicyLayer> {
        let pixi = table(&self.document, &self.pixi)?;
        let workspace = pixi.get("workspace").or_else(|| pixi.get("project"))?;
        let mut rules = Vec::new();
        for (key, selector) in [
            ("exclude-newer", Selector::Default),
            ("pypi-exclude-newer", Selector::Registry(PYPI.to_owned())),
        ] {
            let Some(setting) = workspace.get(key) else {
                continue;
            };
            // A table without a cutoff of its own falls back to the workspace's.
            let Some(cutoff) = setting
                .as_str()
                .or_else(|| setting.get("cutoff").and_then(Item::as_str))
            else {
                continue;
            };
            if let Some(window) = native_window(cutoff) {
                rules.push(NativeRule { selector, window });
            } else {
                tracing::warn!(
                    manifest = self.file,
                    key,
                    cutoff,
                    "ignoring a pixi cutoff that is neither an instant, a date nor a duration"
                );
            }
        }
        (!rules.is_empty()).then_some(NativePolicyLayer { rules })
    }

    /// The manifest as edited.
    pub(crate) fn render(&self) -> String {
        self.document.to_string()
    }
}

/// Whether a dependency table's entry is a version requirement on a package index.
fn registry(item: &Item) -> bool {
    item.as_table_like()
        .is_none_or(|fields| !NOT_REGISTRY.iter().any(|key| fields.contains_key(key)))
}

/// Narrows the requirement `item` holds, a version string or a table of a registry source with a
/// `version`, to exactly `to` with `narrow`, or returns `None`, leaving it as it was, when it
/// holds neither, inherits it from the workspace, or `narrow` refuses.
fn narrow_item(item: &mut Item, to: &str, narrow: fn(&str, &str) -> Option<String>) -> Option<()> {
    if let Some(spec) = item.as_str() {
        let mut value = Value::from(narrow(spec, to)?);
        if let Some(old) = item.as_value() {
            *value.decor_mut() = old.decor().clone();
        }
        *item = Item::Value(value);
        return Some(());
    }
    if !registry(item) {
        return None;
    }
    let fields = item.as_table_like_mut()?;
    let spec = fields.get("version").and_then(Item::as_str).unwrap_or("*");
    let spec = narrow(spec, to)?;
    fields.insert("version", Item::Value(Value::from(spec)));
    Some(())
}

/// `spec` narrowed to exactly `to`; pixi's `*` admits every version.
fn narrowed(spec: &str, to: &str) -> String {
    let spec = spec.trim();
    if spec.is_empty() || spec == "*" {
        format!("=={to}")
    } else {
        format!("{spec},=={to}")
    }
}

/// The conda match spec `spec` narrowed to exactly `to`, keeping a build it selects after the
/// version, or `None` for one with alternatives, which a `,` would bind only the last of.
fn narrowed_conda(spec: &str, to: &str) -> Option<String> {
    if spec.contains('|') {
        return None;
    }
    Some(match spec.trim().split_once(char::is_whitespace) {
        Some((version, build)) => format!("{} {}", narrowed(version, to), build.trim()),
        None => narrowed(spec, to),
    })
}

/// pixi's reading of a cutoff: an instant, a date standing for the start of the next day in UTC,
/// or a span back from now in humantime's units, a month being 30.44 days.
fn native_window(cutoff: &str) -> Option<RawWindow> {
    if let Ok(instant) = cutoff.parse::<Timestamp>() {
        return Some(RawWindow::AbsoluteDate(instant));
    }
    if let Ok(date) = cutoff.parse::<jiff::civil::Date>() {
        let next = date
            .tomorrow()
            .ok()?
            .to_zoned(jiff::tz::TimeZone::UTC)
            .ok()?;
        return Some(RawWindow::AbsoluteDate(next.timestamp()));
    }
    let span = humantime::parse_duration(cutoff).ok()?;
    Some(RawWindow::RelativeDuration(span.try_into().ok()?))
}

/// `path` with each `..` taking off the component before it, as pixi joins a relative path to
/// the project root.
pub(crate) fn lexical(path: &Utf8Path) -> Utf8PathBuf {
    let mut normal = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::ParentDir => {
                normal.pop();
            }
            Utf8Component::CurDir => {}
            other => normal.push(other.as_str()),
        }
    }
    normal
}

/// `base` followed by `more`.
fn keys(base: &[String], more: &[&str]) -> Vec<String> {
    base.iter()
        .cloned()
        .chain(more.iter().map(|key| (*key).to_owned()))
        .collect()
}

/// Every key and item of the table `item` holds, or none when it holds no table.
fn entries(item: Option<&Item>) -> impl Iterator<Item = (&str, &Item)> {
    item.and_then(Item::as_table_like)
        .into_iter()
        .flat_map(TableLike::iter)
}

fn item<'a>(document: &'a DocumentMut, keys: &[String]) -> Option<&'a Item> {
    keys.iter()
        .try_fold(document.as_item(), |item, key| item.get(key.as_str()))
}

fn item_mut<'a>(document: &'a mut DocumentMut, keys: &[String]) -> Option<&'a mut Item> {
    keys.iter().try_fold(document.as_item_mut(), |item, key| {
        item.get_mut(key.as_str())
    })
}

fn table<'a>(document: &'a DocumentMut, keys: &[String]) -> Option<&'a dyn TableLike> {
    item(document, keys)?.as_table_like()
}

fn table_mut<'a>(document: &'a mut DocumentMut, keys: &[String]) -> Option<&'a mut dyn TableLike> {
    item_mut(document, keys)?.as_table_like_mut()
}

/// The table under `key`, created empty when missing, or `None` when `key` holds something else.
fn subtable<'a>(table: &'a mut dyn TableLike, key: &str) -> Option<&'a mut dyn TableLike> {
    table
        .entry(key)
        .or_insert(Item::Value(Value::InlineTable(InlineTable::new())))
        .as_table_like_mut()
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    fn manifest(file: &'static str, text: &str) -> Manifest {
        match Manifest::parse(file, text.to_owned()) {
            Ok(Some(manifest)) => manifest,
            other => panic!("{file} is no pixi manifest: {:?}", other.err()),
        }
    }

    /// The location of each top-level requirement on the PyPI package `name`.
    fn top_level(manifest: &Manifest, name: &str) -> Vec<Location> {
        manifest
            .declarations(Kind::Pypi, name)
            .into_iter()
            .filter_map(|declaration| declaration.location.clone())
            .collect()
    }

    /// A top-level PyPI requirement is narrowed to the target, keeping its extras, index and
    /// markers, whether `[pypi-dependencies]` or `[project]` declares it, while one of a feature is
    /// a declaration cooldown does not narrow.
    #[test]
    fn a_top_level_pypi_requirement_is_narrowed() {
        let mut pixi = manifest(
            "pixi.toml",
            indoc! {r#"
                [pypi-dependencies]
                urllib3 = { version = ">=2", extras = ["socks"], index = "https://example.org/simple" }

                [feature.web.pypi-dependencies]
                Urllib3 = ">=2,<3"
            "#},
        );
        assert_eq!(pixi.declarations(Kind::Pypi, "urllib3").len(), 2);
        for location in top_level(&pixi, "urllib3") {
            assert_eq!(pixi.narrow(&location, "2.2.0"), Ok(()));
        }
        let pinned = pixi.render();
        assert!(
            pinned.contains(r#"version = ">=2,==2.2.0""#)
                && pinned.contains(r#"extras = ["socks"]"#)
                && pinned.contains(r#"index = "https://example.org/simple""#)
                && pinned.contains(r#"Urllib3 = ">=2,<3""#),
            "{pinned}"
        );

        let mut pyproject = manifest(
            "pyproject.toml",
            indoc! {r#"
                [project]
                dependencies = ["urllib3[socks]>=2 ; python_version >= '3.8'"]

                [project.optional-dependencies]
                web = ["idna>=3"]

                [tool.pixi.workspace]
                channels = ["conda-forge"]
            "#},
        );
        assert_eq!(top_level(&pyproject, "idna"), []);
        for location in top_level(&pyproject, "urllib3") {
            assert_eq!(pyproject.narrow(&location, "2.2.0"), Ok(()));
        }
        assert!(
            pyproject
                .render()
                .contains("urllib3[socks]>=2,==2.2.0 ; python_version >= '3.8'"),
            "{}",
            pyproject.render()
        );
    }

    /// Every scope's requirements count as declarations, an inline environment's among them, but
    /// only a top-level one on a package index is located for narrowing.
    #[test]
    fn declarations_cover_every_scope_and_locate_the_top_level() {
        let pixi = manifest(
            "pixi.toml",
            indoc! {r#"
                [pypi-dependencies]
                local = { path = "./local" }
                idna = ">=3"

                [feature.regex.target.linux-64.dependencies]
                pcre2 = ">=10.43,<11"

                [environments.web.pypi-dependencies]
                urllib3 = ">=2"

                [environments.web.dependencies]
                zstd = "*"
            "#},
        );
        for (kind, name) in [
            (Kind::Conda, "pcre2"),
            (Kind::Conda, "zstd"),
            (Kind::Pypi, "urllib3"),
            (Kind::Pypi, "local"),
        ] {
            assert_eq!(pixi.declarations(kind, name).len(), 1, "{name}");
            assert_eq!(top_level(&pixi, name), [], "{name}");
        }
        assert_eq!(top_level(&pixi, "idna").len(), 1);
        assert!(pixi.declarations(Kind::Pypi, "pcre2").is_empty());
    }

    /// A conda target is pinned by one top-level constraint, one written there already is narrowed
    /// keeping its build and channel, and one with alternatives or inherited from the workspace is
    /// refused.
    #[test]
    fn a_top_level_constraint_pins_a_conda_target() {
        let mut pixi = manifest(
            "pixi.toml",
            indoc! {r#"
                [constraints]
                zstd = { version = "1.5.*", build = "*_1", channel = "conda-forge" }
                bzip2 = "1.0.6|1.0.8"
                libzlib = { workspace = true }
            "#},
        );
        assert_eq!(pixi.constraint_refusal("pcre2", "10.42"), None);
        assert_eq!(pixi.constraint_refusal("zstd", "1.5.6"), None);
        for name in ["bzip2", "libzlib"] {
            assert!(pixi.constraint_refusal(name, "1.0.8").is_some(), "{name}");
        }
        assert_eq!(pixi.constrain("pcre2", "10.42"), Ok(()));
        assert_eq!(pixi.constrain("zstd", "1.5.6"), Ok(()));
        assert!(pixi.constrain("libzlib", "1.3").is_err());

        let constrained = pixi.render();
        assert!(
            constrained.contains(r#"pcre2 = "==10.42""#)
                && constrained.contains(
                    r#"zstd = { version = "1.5.*,==1.5.6", build = "*_1", channel = "conda-forge" }"#
                ),
            "{constrained}"
        );
    }

    /// The `find-links` paths of every `pypi-options` table, in either array shape and under the
    /// older `[project]` too, are what pixi reads beside the manifest, while an absolute path or a
    /// `file:` URL is one a copy cannot hold.
    #[test]
    fn find_links_are_relative_paths_or_refused() {
        let pixi = manifest(
            "pixi.toml",
            indoc! {r#"
                [workspace.pypi-options]
                find-links = [{ path = "../wheels" }, { url = "https://example.org/links" }]

                [[project.pypi-options.find-links]]
                path = "./older"

                [[feature.web.pypi-options.find-links]]
                path = "./more"
            "#},
        );
        assert_eq!(
            pixi.find_links(),
            Ok(vec!["../wheels", "./older", "./more"])
        );

        for link in [
            "[pypi-options]\nfind-links = [{ path = \"/opt/wheels\" }]",
            "[[pypi-options.find-links]]\nurl = \"file:///opt/wheels\"",
        ] {
            assert!(manifest("pixi.toml", link).find_links().is_err(), "{link}");
        }
    }

    /// pixi's own cutoffs become windows: a date means the start of the next day, as pixi reads
    /// it, a span takes humantime's units, subseconds included, `pypi-exclude-newer` governs the
    /// PyPI registry alone, and a cutoff cooldown cannot read is left out.
    #[test]
    fn the_workspace_cutoffs_are_native_windows() {
        let windows = |setting: &str| -> Vec<String> {
            manifest("pixi.toml", &format!("[workspace]\n{setting}\n"))
                .native_policy()
                .map(|layer| {
                    layer
                        .rules
                        .iter()
                        .map(|rule| format!("{:?} {:?}", rule.selector, rule.window))
                        .collect()
                })
                .unwrap_or_default()
        };

        assert_eq!(
            windows(indoc! {r#"
                exclude-newer = "2024-01-01"
                pypi-exclude-newer = { cutoff = "1month 1.5s" }
            "#}),
            [
                "Default AbsoluteDate(2024-01-02T00:00:00Z)".to_owned(),
                format!(
                    "Registry(\"pypi\") RelativeDuration({:?})",
                    jiff::SignedDuration::new(2_630_017, 500_000_000)
                ),
            ]
        );
        assert_eq!(windows(r#"exclude-newer = "P7D""#), Vec::<String>::new());
    }
}
