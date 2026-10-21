//! cooldown's resolution windows in uv's own spelling.
//!
//! A project resolves against a default cutoff (`exclude-newer`) plus per-package cutoffs
//! (`exclude-newer-package`).
//! The arguments of every uv command cooldown runs and the table `sync` writes share one rendering
//! ([`PackageCutoff`]), so for the same policy a lock cooldown produces records the same resolver
//! options a plain `uv lock` under the synced config does.
//! uv compares a recorded span structurally (`"5 days"` matches `"P5D"` but not `"120 hours"`), so
//! the rendering must be identical on both paths, not merely equivalent.

use crate::native::normalize_name;
use cooldown_core::{Project, WindowSpec, window_exclude_newer};
use std::collections::BTreeMap;

/// One package's resolution cutoff, as uv's `exclude-newer-package` spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageCutoff {
    /// The package resolves against this cutoff: a relative span (`"3 days"`) or an RFC 3339
    /// instant for a freeze, rendered by [`window_exclude_newer`].
    Cutoff(String),
    /// The package resolves without any cutoff (uv's `false`): a `0d`, `latest`, or `allow`
    /// package must not stay under the project's default window.
    Disabled,
}

impl PackageCutoff {
    /// The cutoff for an effective window.
    #[must_use]
    pub fn from_window(window: &WindowSpec) -> Self {
        window_exclude_newer(window).map_or(PackageCutoff::Disabled, PackageCutoff::Cutoff)
    }

    /// The value of a `NAME=VALUE` `--exclude-newer-package` argument.
    #[must_use]
    pub fn as_arg(&self) -> &str {
        match self {
            PackageCutoff::Cutoff(cutoff) => cutoff,
            PackageCutoff::Disabled => "false",
        }
    }

    /// The value of the package's entry in an `exclude-newer-package` table.
    #[must_use]
    pub fn to_toml(&self) -> toml_edit::Value {
        match self {
            PackageCutoff::Cutoff(cutoff) => cutoff.as_str().into(),
            PackageCutoff::Disabled => false.into(),
        }
    }
}

/// The per-package cutoffs uv can carry for `windows`, keyed by package name.
///
/// Only a name already in PEP 503 normal form is kept.
/// `uv.lock` records normalized names, so that is the only spelling of a `package` selector the
/// cooldown gate ever matches; carrying `Foo_Bar` as `foo-bar` would exempt a package the gate
/// itself does not.
/// The rule also drops names that are not Python package names at all, such as a global
/// `[package."@scope/pkg"]` meant for npm.
#[must_use]
pub fn package_cutoffs(windows: &BTreeMap<String, WindowSpec>) -> BTreeMap<String, PackageCutoff> {
    windows
        .iter()
        .filter(|(name, _)| is_normalized_name(name))
        .map(|(name, window)| (name.clone(), PackageCutoff::from_window(window)))
        .collect()
}

/// Whether `name` is a Python package name in PEP 503 normal form (`onnxruntime`, `types-pyyaml`).
fn is_normalized_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && normalize_name(name) == name
}

/// The resolution cutoffs one uv invocation pins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cutoffs {
    /// The default cutoff (`--exclude-newer`); `None` passes none.
    default: Option<String>,
    /// The per-package cutoffs (`--exclude-newer-package`), keyed by normalized package name.
    packages: BTreeMap<String, PackageCutoff>,
}

impl Cutoffs {
    /// The cutoffs of a project whose policy cooldown has resolved.
    #[must_use]
    pub fn for_project(project: &Project) -> Self {
        Cutoffs {
            default: project.exclude_newer.clone(),
            packages: package_cutoffs(&project.exclude_newer_package),
        }
    }

    /// Cutoffs with only a default and no per-package entries.
    #[must_use]
    pub fn uniform(default: Option<&str>) -> Self {
        Cutoffs {
            default: default.map(str::to_string),
            packages: BTreeMap::new(),
        }
    }

    /// The default cutoff, as `--exclude-newer` and `UV_EXCLUDE_NEWER` carry it.
    #[must_use]
    pub fn default_cutoff(&self) -> Option<&str> {
        self.default.as_deref()
    }

    /// The cutoff that governs `package`: its own entry, else the default.
    ///
    /// `None` means the package resolves without a cutoff.
    #[must_use]
    pub fn for_package(&self, package: &str) -> Option<&str> {
        match self.packages.get(package) {
            Some(PackageCutoff::Cutoff(cutoff)) => Some(cutoff),
            Some(PackageCutoff::Disabled) => None,
            None => self.default_cutoff(),
        }
    }

    /// The `--exclude-newer` and `--exclude-newer-package` arguments pinning these cutoffs.
    ///
    /// Every per-package entry is passed explicitly, so it overrides an entry for the same package
    /// that uv would otherwise inherit from its own config files; uv merges the command-line entries
    /// into the configured table key by key.
    #[must_use]
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(default) = &self.default {
            args.push("--exclude-newer".to_string());
            args.push(default.clone());
        }
        for (package, cutoff) in &self.packages {
            args.push("--exclude-newer-package".to_string());
            args.push(format!("{package}={}", cutoff.as_arg()));
        }
        args
    }
}

#[cfg(test)]
mod tests {
    use super::{Cutoffs, PackageCutoff, package_cutoffs};
    use cooldown_core::WindowSpec;
    use jiff::SignedDuration;
    use std::collections::BTreeMap;

    fn days(count: i64) -> WindowSpec {
        WindowSpec::MinAge(SignedDuration::from_hours(24 * count))
    }

    /// Each window kind maps to the uv value that keeps the recorded lock options stable: a
    /// relative span for an age, the instant for a freeze, and `false` for no window at all.
    #[test]
    fn windows_map_to_uv_package_cutoffs() -> Result<(), jiff::Error> {
        let freeze: jiff::Timestamp = "2026-10-01T00:00:00Z".parse()?;
        let windows = BTreeMap::from([
            ("onnxruntime".to_string(), days(0)),
            ("ruff".to_string(), WindowSpec::Latest),
            ("numpy".to_string(), days(30)),
            (
                "torch".to_string(),
                WindowSpec::MinAge(SignedDuration::from_hours(36)),
            ),
            ("pinned".to_string(), WindowSpec::Freeze(freeze)),
        ]);

        assert_eq!(
            package_cutoffs(&windows),
            BTreeMap::from([
                ("numpy".to_string(), PackageCutoff::Cutoff("30 days".into())),
                ("onnxruntime".to_string(), PackageCutoff::Disabled),
                (
                    "pinned".to_string(),
                    PackageCutoff::Cutoff("2026-10-01T00:00:00Z".into())
                ),
                ("ruff".to_string(), PackageCutoff::Disabled),
                (
                    "torch".to_string(),
                    PackageCutoff::Cutoff("36 hours".into())
                ),
            ])
        );
        Ok(())
    }

    /// Only names the gate can match against `uv.lock` are carried to uv.
    #[test]
    fn package_cutoffs_keep_only_normalized_python_names() {
        let windows: BTreeMap<String, WindowSpec> = [
            "types-pyyaml",
            "zope-interface2",
            "Foo_Bar",
            "foo_bar",
            "foo--bar",
            "-foo",
            "@scope/pkg",
            "github.com/acme/widget",
            "serde_json_merge",
        ]
        .into_iter()
        .map(|name| (name.to_string(), WindowSpec::Latest))
        .collect();

        let names: Vec<String> = package_cutoffs(&windows).into_keys().collect();
        assert_eq!(names, ["types-pyyaml", "zope-interface2"]);
    }

    /// Every uv command pins the default and each package explicitly, in a stable order.
    #[test]
    fn args_pass_the_default_and_every_package_entry() {
        let cutoffs = Cutoffs {
            default: Some("14 days".to_string()),
            packages: BTreeMap::from([
                ("onnxruntime".to_string(), PackageCutoff::Disabled),
                ("numpy".to_string(), PackageCutoff::Cutoff("30 days".into())),
            ]),
        };

        assert_eq!(
            cutoffs.args(),
            [
                "--exclude-newer",
                "14 days",
                "--exclude-newer-package",
                "numpy=30 days",
                "--exclude-newer-package",
                "onnxruntime=false",
            ]
        );
        assert!(Cutoffs::uniform(None).args().is_empty());
    }

    /// A package's own entry wins over the default, and a disabled entry means no cutoff.
    #[test]
    fn for_package_prefers_the_package_entry() {
        let cutoffs = Cutoffs {
            default: Some("14 days".to_string()),
            packages: BTreeMap::from([
                ("onnxruntime".to_string(), PackageCutoff::Disabled),
                ("numpy".to_string(), PackageCutoff::Cutoff("30 days".into())),
            ]),
        };

        assert_eq!(cutoffs.for_package("numpy"), Some("30 days"));
        assert_eq!(cutoffs.for_package("onnxruntime"), None);
        assert_eq!(cutoffs.for_package("httpx"), Some("14 days"));
    }
}
