//! The project's own uv constraints: `[tool.uv] constraint-dependencies` and
//! `override-dependencies` in the lock root's `pyproject.toml`.
//!
//! uv honors these only in the workspace root's `pyproject.toml`; a member's `[tool.uv]` table and
//! a `uv.toml` do not reach `uv lock`.
//! A constraint narrows every requirement on its package, and an override replaces them, the
//! project's own included, so either can hold a candidate that no upstream package caps.
//!
//! Only declarations without an environment marker count as evidence of a bound.
//! A marker-gated declaration may be inactive across the whole supported Python range, so
//! treating it as in force could suppress a valid widen or blame a hold on a declaration that
//! never applied.
//! Any override, marker-gated or not, still replaces the requirements on its package: uv swaps
//! them out by name before it evaluates the override's marker, so outside that marker the
//! package carries no requirement at all.

use crate::native::normalize_name;
use crate::requirement;
use crate::version;
use camino::Utf8Path;
use cooldown_core::Result;
use cooldown_toml_util::read_toml_file;
use pep440_rs::{Operator, Version, VersionSpecifiers};
use std::collections::BTreeSet;
use std::str::FromStr;

/// Which `[tool.uv]` list declares a [`LocalConstraint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalField {
    /// `constraint-dependencies`: narrows every requirement on the package.
    Constraint,
    /// `override-dependencies`: replaces every requirement on the package.
    Override,
}

impl LocalField {
    /// The `[tool.uv]` key the declaration lives under.
    pub(crate) fn key(self) -> &'static str {
        match self {
            LocalField::Constraint => "constraint-dependencies",
            LocalField::Override => "override-dependencies",
        }
    }
}

/// One marker-free version constraint or override the project declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalConstraint {
    /// The list that declares it.
    pub field: LocalField,
    /// The PEP 503-normalized package name.
    pub name: String,
    /// The requirement as the author wrote it (`onnxruntime==1.31.0`).
    pub requirement: String,
    /// The parsed version specifier.
    specifiers: VersionSpecifiers,
}

impl LocalConstraint {
    /// Whether the declaration is a lone exact pin (`==X` or `===X`), which holds every other
    /// version alike.
    pub(crate) fn is_exact(&self) -> bool {
        matches!(
            &*self.specifiers,
            [specifier] if matches!(specifier.operator(), Operator::Equal | Operator::ExactEqual)
        )
    }

    fn admits(&self, version: &Version) -> bool {
        self.specifiers.contains(version)
    }
}

/// The constraints and overrides of one uv project, with the manifest that declares them.
#[derive(Debug, Clone, Default)]
pub(crate) struct LocalConstraints {
    /// The declaring manifest's file name, as a row names it (`pyproject.toml`).
    pub file: String,
    /// The marker-free declarations: overrides first, then constraints, each in declaration
    /// order.
    declarations: Vec<LocalConstraint>,
    /// The normalized name of every package an override replaces, marker-gated or not.
    overridden: BTreeSet<String>,
}

impl LocalConstraints {
    /// Reads the declarations of `manifest`; an absent manifest or `[tool.uv]` table declares
    /// none.
    ///
    /// An entry that is not a string, names no package, carries a marker, or has no PEP 440
    /// version specifier (a bare name or a URL) bounds no version and is skipped.
    pub(crate) fn read(manifest: &Utf8Path) -> Result<Self> {
        let file = manifest.file_name().unwrap_or("pyproject.toml").to_string();
        let Some(value) = read_toml_file::<toml::Value>(manifest, "pyproject.toml")? else {
            return Ok(Self {
                file,
                ..Self::default()
            });
        };
        let uv = value.get("tool").and_then(|tool| tool.get("uv"));
        let lines = |field: LocalField| {
            uv.and_then(|uv| uv.get(field.key()))
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(toml::Value::as_str)
        };
        let declarations = [LocalField::Override, LocalField::Constraint]
            .into_iter()
            .flat_map(|field| lines(field).filter_map(move |line| declaration(field, line)))
            .collect();
        let overridden = lines(LocalField::Override)
            .filter_map(requirement::parse)
            .map(|parsed| normalize_name(parsed.name))
            .collect();
        Ok(Self {
            file,
            declarations,
            overridden,
        })
    }

    /// Whether an override replaces every declared requirement on `name`, so none of them
    /// constrains it.
    pub(crate) fn overrides(&self, name: &str) -> bool {
        self.overridden.contains(&normalize_name(name))
    }

    /// The first declaration on `name` that excludes `target`, or `None` when none does or
    /// `target` is not a PEP 440 version.
    pub(crate) fn excluding(&self, name: &str, target: &str) -> Option<&LocalConstraint> {
        let target = version::parse(target)?;
        let name = normalize_name(name);
        self.declarations
            .iter()
            .find(|declared| declared.name == name && !declared.admits(&target))
    }

    /// The first declaration on `name` that admits every one of its `locked` versions and
    /// excludes `target`: a bound in force on the resolved graph that the target lies outside.
    /// `None` when nothing of `name` is locked, since a declaration cannot hold a package the
    /// resolve dropped.
    pub(crate) fn capping(
        &self,
        name: &str,
        locked: &[Version],
        target: &Version,
    ) -> Option<&LocalConstraint> {
        if locked.is_empty() {
            return None;
        }
        let name = normalize_name(name);
        self.declarations.iter().find(|declared| {
            declared.name == name
                && !declared.admits(target)
                && locked.iter().all(|version| declared.admits(version))
        })
    }
}

/// Parses one declaration of `field`, or `None` when it bounds no version unconditionally.
fn declaration(field: LocalField, line: &str) -> Option<LocalConstraint> {
    let parsed = requirement::parse(line)?;
    if parsed.marker.is_some() || !parsed.has_version_specifier() {
        return None;
    }
    // PEP 508 also allows the specifier in parentheses (`certifi (<2024.12)`), which the PEP 440
    // parser does not accept.
    let specifier = parsed
        .specifier
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
        .unwrap_or(parsed.specifier);
    let specifiers = VersionSpecifiers::from_str(specifier).ok()?;
    Some(LocalConstraint {
        field,
        name: normalize_name(parsed.name),
        requirement: line.trim().to_string(),
        specifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::{LocalConstraints, LocalField};
    use crate::version;
    use camino::Utf8PathBuf;
    use indoc::indoc;

    fn read(contents: &str) -> LocalConstraints {
        let directory = tempfile::tempdir().expect("tempdir");
        let manifest =
            Utf8PathBuf::from_path_buf(directory.path().join("pyproject.toml")).expect("utf8 path");
        std::fs::write(&manifest, contents).expect("write manifest");
        LocalConstraints::read(&manifest).expect("a valid manifest")
    }

    #[test]
    fn reads_marker_free_constraints_and_overrides() {
        let local = read(indoc! {r#"
            [project]
            name = "demo"
            dependencies = ["onnxruntime>=1.20"]

            [tool.uv]
            constraint-dependencies = [
                "onnxruntime==1.31.0",
                "Protobuf < 7",
                "numpy (>=2,<2.3)",
                "certifi==2024.8.30; python_version < '3.11'",
                "requests",
                "pkg @ https://example.com/pkg.whl",
            ]
            override-dependencies = ["idna>=3.7", "urllib3<2; python_version < '3.11'"]
        "#});
        assert_eq!(local.file, "pyproject.toml");

        // Exact pins and ranges are both read, names normalized and the written form kept.
        let pin = local
            .excluding("onnxruntime", "1.32.0")
            .expect("the pin excludes a newer release");
        assert_eq!(pin.field, LocalField::Constraint);
        assert_eq!(pin.requirement, "onnxruntime==1.31.0");
        assert!(pin.is_exact());
        let range = local
            .excluding("protobuf", "7.0.0")
            .expect("the range excludes the next major");
        assert_eq!(range.requirement, "Protobuf < 7");
        assert!(!range.is_exact());
        assert!(local.excluding("protobuf", "6.33.6").is_none());
        // PEP 508's parenthesized specifier bounds the same way.
        assert!(local.excluding("numpy", "2.3.0").is_some());
        assert!(local.excluding("numpy", "2.2.6").is_none());

        // A marker-gated pin, a bare name, and a URL bound no version unconditionally.
        assert!(local.excluding("certifi", "2024.12.14").is_none());
        assert!(local.excluding("requests", "9.0.0").is_none());
        assert!(local.excluding("pkg", "9.0.0").is_none());

        // Only an override replaces declared requirements, and a marker-gated one does too,
        // though its marker keeps it from bounding anything.
        assert!(local.overrides("idna"));
        assert!(local.overrides("urllib3"));
        assert!(local.excluding("urllib3", "2.3.0").is_none());
        assert!(!local.overrides("onnxruntime"));
    }

    #[test]
    fn a_declaration_caps_only_when_it_admits_every_locked_version() {
        let local = read(indoc! {r#"
            [tool.uv]
            constraint-dependencies = ["numpy<2.3"]
        "#});
        let target = version::parse("2.3.1").expect("a version");
        let older = version::parse("2.2.6").expect("a version");
        let newer = version::parse("2.3.0").expect("a version");
        assert!(
            local
                .capping("numpy", std::slice::from_ref(&older), &target)
                .is_some()
        );
        // A fork whose other node already sits past the bound is not held by it.
        assert!(local.capping("numpy", &[older, newer], &target).is_none());
        // A package the resolve dropped is not held by anything.
        assert!(local.capping("numpy", &[], &target).is_none());
    }

    #[test]
    fn a_manifest_without_uv_settings_declares_nothing() {
        let local = read(indoc! {r#"
            [project]
            name = "demo"
        "#});
        assert!(local.excluding("anything", "1.0.0").is_none());
        assert!(!local.overrides("anything"));
    }
}
