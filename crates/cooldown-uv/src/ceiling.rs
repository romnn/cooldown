//! Why a candidate the whole-graph resolve left below its target is held: the project's own uv
//! constraint or override that excludes the target, or else the requirement that caps it and the
//! package declaring it, attributed from the lock first and from PyPI's per-release metadata
//! second.
//!
//! uv settles every conflict itself and says nothing about a candidate it kept back, and a
//! modern `uv.lock` records declared requirements (`requires-dist`) only for workspace sources,
//! so for a registry requirer the specifier that caps the candidate exists only in that
//! requirer's own release metadata.
//! Attribution is best-effort and read-only: it never changes what the resolve adopted, only
//! what the held row says.
//! A hold nothing attributes stays unattributed: a package whose resolved edge merely reaches the
//! candidate has not been shown to cap it, so it is never named.

use crate::constraints::{LocalConstraint, LocalConstraints, LocalField};
use crate::lock::{Package, UvLock};
use crate::requirement;
use crate::version;
use async_trait::async_trait;
use cooldown_adapter_util::pep503_normalize;
use cooldown_core::{RawRelease, Result, SkipReason};
use jiff::Timestamp;
use pep440_rs::{Version, VersionSpecifiers};
use std::collections::BTreeSet;
use std::str::FromStr;

/// How many newer releases of a capping package are read to find the one that lifts the cap.
/// Each is one cached registry read; a package with a long tail of newer releases stops here
/// rather than fetch them all.
const LIFT_PROBES: usize = 6;

/// What the registry knows about a package's releases and their declared requirements: the
/// half of attribution the lock cannot answer.
#[async_trait]
pub(crate) trait DeclaredRequirements: Sync {
    /// The PEP 508 `requires-dist` lines of `name` at `version`; `None` when unknown.
    async fn requires_dist(&self, name: &str, version: &str) -> Result<Option<Vec<String>>>;

    /// Every release of `name` with its publish instant, in no particular order.
    async fn releases(&self, name: &str) -> Result<Vec<RawRelease>>;
}

#[async_trait]
impl DeclaredRequirements for crate::pypi::PyPi {
    async fn requires_dist(&self, name: &str, version: &str) -> Result<Option<Vec<String>>> {
        crate::pypi::PyPi::requires_dist(self, name, version).await
    }

    async fn releases(&self, name: &str) -> Result<Vec<RawRelease>> {
        let package = cooldown_core::PackageId::new(
            crate::tool::UV_ID,
            name.to_string(),
            Some(crate::pypi::PYPI.to_string()),
        );
        cooldown_core::PackageRegistry::releases(self, &package).await
    }
}

/// What holds a candidate below its target: a package's requirement, or the project's own
/// constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ceiling {
    /// The package named on the row as the blocker: the package the held candidate conflicts
    /// with, or the held candidate itself when its own project constraint holds it.
    pub blocker: String,
    /// The blocker's locked version, when the lock records one.
    pub blocker_version: Option<String>,
    /// How the blocker holds the candidate.
    pub cause: Cause,
}

/// Which side of the conflict declares the capping requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cause {
    /// The blocker requires the held package under a specifier that excludes the target.
    Requires(Declared),
    /// The held package, at `requirer_version`, requires the blocker under a specifier the
    /// blocker's locked version violates, so the candidate cannot land unless the blocker moves.
    RequiredBy {
        requirer_version: String,
        declared: Declared,
    },
    /// The project's own `[tool.uv]` constraint or override on the held package excludes the
    /// target.
    Local(LocalCap),
}

/// A project declaration in `[tool.uv]` that excludes a held candidate's target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalCap {
    /// The list that declares it.
    pub field: LocalField,
    /// The declaration as the author wrote it (`onnxruntime==1.31.0`).
    pub requirement: String,
    /// Whether it is a lone exact pin.
    pub exact: bool,
    /// The manifest that declares it (`pyproject.toml`).
    pub file: String,
    /// The target it excludes.
    pub target: String,
}

/// A declared requirement as its author wrote it, split from its marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Declared {
    /// The requirement without its marker (`transformers<5.9.0,>=4.42.0`).
    pub requirement: String,
    /// The environment marker gating it, verbatim.
    pub marker: Option<String>,
}

/// The first newer release of a capping package whose requirement admits the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lift {
    pub version: String,
    pub published_at: Option<Timestamp>,
}

/// The ceiling holding `held` below `target` in `lock`, or `None` when nothing attributes it.
///
/// Attribution is tried in order of how much it can say, so the row carries the most it can:
///
/// 0. The project's own constraint or override on `held` excludes `target` while admitting every
///    locked version of `held`.
/// 1. A requirer's `requires-dist` in the lock (a workspace source) caps `held` below `target`.
/// 2. A registry requirer's release metadata caps `held` below `target`.
/// 3. The target release's own metadata requires a sibling under a specifier the sibling's
///    locked version violates.
///
/// A cap is a requirement the locked version satisfies and the target does not; a requirement
/// the lock already violates is inactive (a marker uv resolved away) and blames nothing.
/// An override replaces every requirement on its package, so a requirer's requirement on an
/// overridden package caps nothing, at step 1 and 2 for `held` and at step 3 for a sibling.
/// Registry reads are best-effort: a failed or unknown read falls through to the next step.
pub(crate) async fn attribute(
    lock: &UvLock,
    held: &str,
    target: &str,
    local: &LocalConstraints,
    registry: &dyn DeclaredRequirements,
) -> Option<Ceiling> {
    let target_version = version::parse(target)?;
    let locked: Vec<Version> = lock
        .packages
        .iter()
        .filter(|package| package.name == held)
        .filter_map(|package| package.version.as_deref().and_then(version::parse))
        .collect();
    if let Some(declared) = local.capping(held, &locked, &target_version) {
        return Some(local_ceiling(held, target, declared, &local.file));
    }
    if !local.overrides(held) {
        let current = locked.first();
        if let Some(ceiling) = requirer_capping_in_lock(lock, held, current, &target_version) {
            return Some(ceiling);
        }
        if let Some(ceiling) =
            requirer_capping_in_metadata(lock, held, current, &target_version, registry).await
        {
            return Some(ceiling);
        }
    }
    sibling_violated_by_target(lock, held, target, local, registry).await
}

/// The ceiling of the project's own declaration `declared` on `held`, excluding `target`.
fn local_ceiling(held: &str, target: &str, declared: &LocalConstraint, file: &str) -> Ceiling {
    Ceiling {
        blocker: held.to_string(),
        blocker_version: None,
        cause: Cause::Local(LocalCap {
            field: declared.field,
            requirement: declared.requirement.clone(),
            exact: declared.is_exact(),
            file: file.to_string(),
            target: target.to_string(),
        }),
    }
}

/// The release of the blocker of `ceiling` that lifts its cap on `held`, when the cap is the
/// blocker's own requirement and one of its next [`LIFT_PROBES`] newer stable releases either
/// admits `target` or no longer requires `held`.
/// `None` when the cap is not the blocker's to lift, when no probed release lifts it, or when
/// the registry cannot say.
pub(crate) async fn lift(
    lock: &UvLock,
    ceiling: &Ceiling,
    held: &str,
    target: &str,
    registry: &dyn DeclaredRequirements,
) -> Option<Lift> {
    if !matches!(ceiling.cause, Cause::Requires(_)) {
        return None;
    }
    let target_version = version::parse(target)?;
    let blocker = lock
        .packages
        .iter()
        .find(|package| package.name == ceiling.blocker)?;
    let locked = version::parse(ceiling.blocker_version.as_deref()?)?;
    let mut newer: Vec<RawRelease> = registry
        .releases(&blocker.name)
        .await
        .ok()?
        .into_iter()
        .filter(|release| !release.yanked && !version::is_prerelease(release.version.as_str()))
        .filter(|release| version::parse(release.version.as_str()).is_some_and(|v| v > locked))
        .collect();
    newer.sort_by(|a, b| version::compare(a.version.as_str(), b.version.as_str()));
    for release in newer.into_iter().take(LIFT_PROBES) {
        // An unknown requirement list says nothing about this release either way.
        let Ok(Some(lines)) = registry
            .requires_dist(&blocker.name, release.version.as_str())
            .await
        else {
            continue;
        };
        let active = |marker: Option<&str>| requirement_is_active(marker, blocker, held);
        if capping_requirement(&lines, held, None, &target_version, active).is_none() {
            return Some(Lift {
                version: release.version.as_str().to_string(),
                published_at: release.published_at,
            });
        }
    }
    None
}

impl Ceiling {
    /// The skip reason of the held row.
    ///
    /// The project's own exact pin holds like any exact manifest pin.
    /// Its own range holds a forward move as an explicit upper bound and a downgrade as a floor
    /// the graph requires.
    /// A requirement of another package is a graph ceiling.
    pub(crate) fn skip_reason(&self, downgrade: bool) -> SkipReason {
        match &self.cause {
            Cause::Local(cap) if cap.exact => SkipReason::PinHeld,
            Cause::Local(_) if downgrade => SkipReason::GraphHeld,
            Cause::Local(_) => SkipReason::DeclaredBoundHeld,
            Cause::Requires(_) | Cause::RequiredBy { .. } => SkipReason::GraphCeilingHeld,
        }
    }

    /// The held row's detail: the blocker with its version, the requirement that caps the
    /// candidate with its marker, and the release that lifts the cap when known.
    /// For the project's own declaration it names the declaration, its `[tool.uv]` list, and
    /// the manifest.
    ///
    /// `cutoff` is the project's resolution window as uv was given it, so a lifting release the
    /// window excluded is dated with when it matures; `now` anchors that estimate.
    pub(crate) fn detail(
        &self,
        held: &str,
        lift: Option<&Lift>,
        cutoff: Option<&str>,
        now: Timestamp,
    ) -> String {
        let blocker = match &self.blocker_version {
            Some(version) => format!("{} {version}", self.blocker),
            None => self.blocker.clone(),
        };
        let mut detail = match &self.cause {
            Cause::Requires(declared) => {
                format!(
                    "held: conflicts with {blocker}, which requires {}",
                    declared.describe()
                )
            }
            Cause::RequiredBy {
                requirer_version,
                declared,
            } => format!(
                "held: conflicts with {blocker}: {held} {requirer_version} requires {}",
                declared.describe()
            ),
            Cause::Local(cap) => {
                return format!(
                    "held: {} [tool.uv] {} declares {}, which excludes {}",
                    cap.file,
                    cap.field.key(),
                    cap.requirement,
                    cap.target
                );
            }
        };
        if let Some(lift) = lift {
            use std::fmt::Write as _;
            let _ = write!(
                detail,
                "; the cap lifts in {} {}",
                self.blocker, lift.version
            );
            if let Some(note) = window_note(lift.published_at, cutoff, now) {
                let _ = write!(detail, " ({note})");
            }
        }
        detail
    }
}

impl Declared {
    /// The requirement followed by its marker, when it has one, as `<requirement> on <marker>`.
    fn describe(&self) -> String {
        match &self.marker {
            Some(marker) => format!("{} on {marker}", self.requirement),
            None => self.requirement.clone(),
        }
    }
}

/// Where a lifting release stands against the window uv resolved under: its publish date, and
/// when it matures if the window excluded it (a relative window) or that it falls after the
/// freeze (an absolute one).
/// `None` without a publish date.
fn window_note(
    published: Option<Timestamp>,
    cutoff: Option<&str>,
    now: Timestamp,
) -> Option<String> {
    let published = published?;
    let date = published.strftime("%Y-%m-%d");
    let position = cutoff.and_then(|cutoff| {
        if let Ok(window) = cooldown_core::duration::parse_duration(cutoff) {
            let inside = now.checked_sub(window).ok()? < published;
            let matures = published.checked_add(window).ok()?;
            inside.then(|| format!("in cooldown until about {}", matures.strftime("%Y-%m-%d")))
        } else {
            let freeze = Timestamp::from_str(cutoff).ok()?;
            (published > freeze).then(|| "after the freeze".to_string())
        }
    });
    Some(match position {
        Some(position) => format!("published {date}, {position}"),
        None => format!("published {date}"),
    })
}

/// Step 1: a requirer whose `requires-dist` recorded in the lock caps `held` below the target.
/// Order-stable across candidate requirers.
fn requirer_capping_in_lock(
    lock: &UvLock,
    held: &str,
    current: Option<&Version>,
    target: &Version,
) -> Option<Ceiling> {
    let held_normalized = pep503_normalize(held);
    let mut requirers: Vec<&Package> = lock
        .packages
        .iter()
        .filter(|package| package.name != held && package.metadata.is_some())
        .collect();
    requirers.sort_by(|a, b| a.name.cmp(&b.name));
    for requirer in requirers {
        let metadata = requirer.metadata.as_ref()?;
        let declared = metadata.requires_dist.iter().find_map(|spec| {
            let specifier = spec.specifier.as_deref()?;
            (pep503_normalize(&spec.name) == held_normalized
                && caps(specifier, current, target)
                && requirement_is_active(spec.marker.as_deref(), requirer, held))
            .then(|| Declared {
                requirement: format!("{}{specifier}", spec.name),
                marker: spec.marker.clone(),
            })
        });
        if let Some(declared) = declared {
            return Some(Ceiling {
                blocker: requirer.name.clone(),
                blocker_version: requirer.version.clone(),
                cause: Cause::Requires(declared),
            });
        }
    }
    None
}

/// Step 2: a registry requirer whose release metadata caps `held` below the target.
/// Only packages the canonical PyPI index served are read, since only their metadata lives
/// where the client looks.
/// Order-stable across candidate requirers.
async fn requirer_capping_in_metadata(
    lock: &UvLock,
    held: &str,
    current: Option<&Version>,
    target: &Version,
    registry: &dyn DeclaredRequirements,
) -> Option<Ceiling> {
    let mut requirers: Vec<&Package> = lock
        .packages
        .iter()
        .filter(|package| package.name != held && reaches(package, held) && from_pypi(package))
        .collect();
    requirers.sort_by(|a, b| a.name.cmp(&b.name));
    for requirer in requirers {
        let Some(version) = requirer.version.as_deref() else {
            continue;
        };
        let Ok(Some(lines)) = registry.requires_dist(&requirer.name, version).await else {
            continue;
        };
        let active = |marker: Option<&str>| requirement_is_active(marker, requirer, held);
        if let Some(declared) = capping_requirement(&lines, held, current, target, active) {
            return Some(Ceiling {
                blocker: requirer.name.clone(),
                blocker_version: Some(version.to_string()),
                cause: Cause::Requires(declared),
            });
        }
    }
    None
}

/// Step 3: the target release of `held` requires a sibling under a specifier the sibling's
/// locked version violates, so the sibling, which the same resolve declined to move, is what
/// holds the candidate.
/// Requirements under an extra count only when the lock activates that extra of `held`, and a
/// requirement on a sibling the project overrides counts not at all, since uv replaces it.
/// Order-stable across siblings.
async fn sibling_violated_by_target(
    lock: &UvLock,
    held: &str,
    target: &str,
    local: &LocalConstraints,
    registry: &dyn DeclaredRequirements,
) -> Option<Ceiling> {
    let held_package = lock.packages.iter().find(|package| package.name == held)?;
    if !from_pypi(held_package) {
        return None;
    }
    let lines = registry.requires_dist(held, target).await.ok()??;
    let active_extras: BTreeSet<String> = held_package
        .optional_dependencies
        .keys()
        .map(|extra| pep503_normalize(extra))
        .collect();
    let mut violated: Vec<(String, String, Declared)> = lines
        .iter()
        .filter_map(|line| {
            let parsed = requirement::parse(line)?;
            if !parsed.has_version_specifier()
                || !extras_permit(parsed.marker, &active_extras)
                || local.overrides(parsed.name)
            {
                return None;
            }
            let sibling = lock
                .packages
                .iter()
                .find(|package| package.name == pep503_normalize(parsed.name))?;
            let locked = version::parse(sibling.version.as_deref()?)?;
            let specifiers = VersionSpecifiers::from_str(parsed.specifier).ok()?;
            (!specifiers.contains(&locked)).then(|| {
                (
                    sibling.name.clone(),
                    locked.to_string(),
                    declared_from(line, parsed.marker),
                )
            })
        })
        .collect();
    violated.sort_by(|a, b| a.0.cmp(&b.0));
    let (blocker, blocker_version, declared) = violated.into_iter().next()?;
    Some(Ceiling {
        blocker,
        blocker_version: Some(blocker_version),
        cause: Cause::RequiredBy {
            requirer_version: target.to_string(),
            declared,
        },
    })
}

/// The first line of `lines` that names `held` under an active marker and caps it: its
/// specifier admits `current` when one is known and excludes `target`.
fn capping_requirement(
    lines: &[String],
    held: &str,
    current: Option<&Version>,
    target: &Version,
    active: impl Fn(Option<&str>) -> bool,
) -> Option<Declared> {
    let held_normalized = pep503_normalize(held);
    lines.iter().find_map(|line| {
        let parsed = requirement::parse(line)?;
        (pep503_normalize(parsed.name) == held_normalized
            && parsed.has_version_specifier()
            && caps(parsed.specifier, current, target)
            && active(parsed.marker))
        .then(|| declared_from(line, parsed.marker))
    })
}

/// Whether `specifier` admits `current` (when known) and excludes `target`.
/// An unparsable specifier caps nothing.
fn caps(specifier: &str, current: Option<&Version>, target: &Version) -> bool {
    let Ok(specifiers) = VersionSpecifiers::from_str(specifier) else {
        return false;
    };
    current.is_none_or(|current| specifiers.contains(current)) && !specifiers.contains(target)
}

/// The requirement of `line` without its marker, kept as written.
fn declared_from(line: &str, marker: Option<&str>) -> Declared {
    Declared {
        requirement: line
            .split_once(';')
            .map_or(line, |(head, _)| head)
            .trim()
            .to_string(),
        marker: marker.map(str::to_string),
    }
}

/// Whether a requirement of `requirer` on `held` under `marker` is in force in the lock.
/// A requirement gated on an extra counts only when the lock lists `held` under that extra of
/// `requirer`; one without an extra clause counts when `held` is among the requirer's plain or
/// group dependencies.
/// Other marker terms are not evaluated, since uv resolves every environment at once and a
/// platform-gated cap still holds the universal lock.
fn requirement_is_active(marker: Option<&str>, requirer: &Package, held: &str) -> bool {
    let extras = extras_in_marker(marker);
    if extras.is_empty() {
        return requirer
            .dependencies
            .iter()
            .chain(requirer.dev_dependencies.values().flatten())
            .any(|dep| dep.name == held);
    }
    extras.iter().any(|extra| {
        requirer.optional_dependencies.iter().any(|(name, deps)| {
            pep503_normalize(name) == *extra && deps.iter().any(|dep| dep.name == held)
        })
    })
}

/// Whether the extras `marker` names are all among `active`; a marker without an extra clause
/// permits.
fn extras_permit(marker: Option<&str>, active: &BTreeSet<String>) -> bool {
    extras_in_marker(marker)
        .iter()
        .all(|extra| active.contains(extra))
}

/// The PEP 503-normalized extras an environment marker tests for with `extra == "<name>"`.
fn extras_in_marker(marker: Option<&str>) -> BTreeSet<String> {
    let mut extras = BTreeSet::new();
    let Some(marker) = marker else {
        return extras;
    };
    let mut rest = marker;
    while let Some(at) = rest.find("extra") {
        let after = rest.get(at + "extra".len()..).unwrap_or_default();
        let after = after.trim_start();
        if let Some(value) = after.strip_prefix("==") {
            let value = value.trim_start();
            if let Some(quoted) = value.strip_prefix('"').or_else(|| value.strip_prefix('\''))
                && let Some(end) = quoted.find(['"', '\''])
            {
                extras.insert(pep503_normalize(quoted.get(..end).unwrap_or_default()));
            }
        }
        rest = after;
    }
    extras
}

/// Whether `package` has a resolved edge to `name`: a plain, group, or extra dependency.
fn reaches(package: &Package, name: &str) -> bool {
    package.all_direct_dep_names().any(|dep| dep == name)
}

/// Whether the canonical PyPI index served `package`, so its release metadata is where the
/// client reads it.
fn from_pypi(package: &Package) -> bool {
    package
        .source
        .as_ref()
        .and_then(|source| source.registry.as_deref())
        .is_some_and(crate::pypi::is_pypi_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cooldown_core::CoreError;
    use indoc::indoc;
    use std::collections::HashMap;

    /// A registry that answers from memory: release metadata keyed by `(name, version)` and a
    /// release list per name.
    /// A `(name, version)` it does not know is "unknown", as PyPI's `null` would be, not an
    /// error.
    #[derive(Default)]
    struct Fake {
        requirements: HashMap<(String, String), Vec<String>>,
        releases: HashMap<String, Vec<RawRelease>>,
        failing: bool,
    }

    impl Fake {
        fn requires(mut self, name: &str, version: &str, lines: &[&str]) -> Self {
            self.requirements.insert(
                (name.to_string(), version.to_string()),
                lines.iter().map(ToString::to_string).collect(),
            );
            self
        }

        fn release(mut self, name: &str, version: &str, published: &str) -> Self {
            self.releases
                .entry(name.to_string())
                .or_default()
                .push(RawRelease {
                    version: cooldown_core::Version::new(version.to_string()),
                    published_at: Some(published.parse().expect("a valid instant")),
                    yanked: false,
                    artifacts: Vec::new(),
                });
            self
        }
    }

    #[async_trait]
    impl DeclaredRequirements for Fake {
        async fn requires_dist(&self, name: &str, version: &str) -> Result<Option<Vec<String>>> {
            if self.failing {
                return Err(CoreError::OfflineMiss(format!("{name}@{version}")));
            }
            Ok(self
                .requirements
                .get(&(name.to_string(), version.to_string()))
                .cloned())
        }

        async fn releases(&self, name: &str) -> Result<Vec<RawRelease>> {
            if self.failing {
                return Err(CoreError::OfflineMiss(name.to_string()));
            }
            Ok(self.releases.get(name).cloned().unwrap_or_default())
        }
    }

    fn parse_lock(content: &str) -> UvLock {
        UvLock::parse(content).expect("lock parses")
    }

    /// A project that declares no uv constraints.
    fn no_local() -> LocalConstraints {
        LocalConstraints::default()
    }

    /// The uv constraints of a `pyproject.toml` holding `contents`.
    fn local(contents: &str) -> LocalConstraints {
        let directory = tempfile::tempdir().expect("tempdir");
        let manifest = camino::Utf8PathBuf::from_path_buf(directory.path().join("pyproject.toml"))
            .expect("utf8 path");
        std::fs::write(&manifest, contents).expect("write manifest");
        LocalConstraints::read(&manifest).expect("a valid manifest")
    }

    /// The luup4 shape: `transformers` is pulled by two docling packages, neither of which the
    /// lock carries `requires-dist` for, and one of them caps it on darwin.
    fn docling_lock() -> UvLock {
        parse_lock(indoc! {r#"
            version = 1
            revision = 3

            [[package]]
            name = "docling-core"
            version = "2.92.0"
            source = { registry = "https://pypi.org/simple" }
            dependencies = [{ name = "pydantic" }]

            [package.optional-dependencies]
            chunking = [{ name = "transformers" }]

            [[package]]
            name = "docling-ibm-models"
            version = "3.14.0"
            source = { registry = "https://pypi.org/simple" }
            dependencies = [{ name = "transformers" }]

            [[package]]
            name = "pydantic"
            version = "2.12.0"
            source = { registry = "https://pypi.org/simple" }

            [[package]]
            name = "transformers"
            version = "5.8.1"
            source = { registry = "https://pypi.org/simple" }
        "#})
    }

    const NOW: &str = "2026-09-04T12:00:00Z";

    fn now() -> Timestamp {
        NOW.parse().expect("a valid instant")
    }

    #[tokio::test]
    async fn a_registry_requirers_metadata_names_the_cap_with_its_marker() {
        // Neither requirer's lock entry carries a specifier, so only the metadata can say that
        // docling-ibm-models caps transformers below 5.9.0 on darwin.
        // docling-core's cap sits under its `chunking` extra, which the lock activates, so it is
        // a valid attribution too; the alphabetically first requirer wins for a stable row.
        let registry = Fake::default()
            .requires(
                "docling-core",
                "2.92.0",
                &[
                    "pydantic>=2.6.0,<3.0.0",
                    r#"transformers<5.9.0,>=4.34.0; sys_platform == "darwin" and extra == "chunking""#,
                ],
            )
            .requires(
                "docling-ibm-models",
                "3.14.0",
                &[r#"transformers!=5.0.*,<5.9.0,>=4.42.0; sys_platform == "darwin""#],
            );
        let ceiling = attribute(
            &docling_lock(),
            "transformers",
            "5.15.1",
            &no_local(),
            &registry,
        )
        .await
        .expect("the metadata attributes the cap");
        assert_eq!(ceiling.blocker, "docling-core");
        assert_eq!(ceiling.blocker_version.as_deref(), Some("2.92.0"));
        assert_eq!(
            ceiling.cause,
            Cause::Requires(Declared {
                requirement: "transformers<5.9.0,>=4.34.0".into(),
                marker: Some(r#"sys_platform == "darwin" and extra == "chunking""#.into()),
            })
        );
        assert_eq!(
            ceiling.detail("transformers", None, Some("14 days"), now()),
            r#"held: conflicts with docling-core 2.92.0, which requires transformers<5.9.0,>=4.34.0 on sys_platform == "darwin" and extra == "chunking""#
        );
    }

    #[tokio::test]
    async fn an_inactive_extra_or_a_satisfied_specifier_caps_nothing() {
        // docling-core's cap lives under an extra the lock does not activate for it, and
        // docling-ibm-models admits the target: nothing in the metadata caps transformers, so
        // the hold stays unattributed.
        let registry = Fake::default()
            .requires(
                "docling-core",
                "2.92.0",
                &[r#"transformers<5.9.0; extra == "vlm""#],
            )
            .requires("docling-ibm-models", "3.14.0", &["transformers>=4.42.0"]);
        assert_eq!(
            attribute(
                &docling_lock(),
                "transformers",
                "5.15.1",
                &no_local(),
                &registry
            )
            .await,
            None
        );
        // A requirement the lock already violates is not in force: uv resolved that marker away,
        // so it cannot be what holds the candidate.
        let registry = Fake::default().requires(
            "docling-ibm-models",
            "3.14.0",
            &["transformers>=6.0; python_full_version >= '3.14'"],
        );
        let ceiling = attribute(
            &docling_lock(),
            "transformers",
            "5.15.1",
            &no_local(),
            &registry,
        )
        .await;
        assert_eq!(ceiling, None);
    }

    #[tokio::test]
    async fn a_registry_failure_leaves_attribution_to_the_lock() {
        // Offline or a registry outage must not turn a held row into an error: the read fails and
        // the hold stays unattributed.
        let registry = Fake {
            failing: true,
            ..Fake::default()
        };
        assert_eq!(
            attribute(
                &docling_lock(),
                "transformers",
                "5.15.1",
                &no_local(),
                &registry
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn the_lift_is_the_first_newer_release_admitting_the_target() {
        // docling-ibm-models 3.14.1 still caps transformers, 4.0.0 admits it, and 4.0.2 would
        // too; the first lifting release is the one that matures first, so it is the one named,
        // and a prerelease or yanked release never counts.
        let registry = Fake::default()
            .requires(
                "docling-ibm-models",
                "3.14.0",
                &[r#"transformers<5.9.0,>=4.42.0; sys_platform == "darwin""#],
            )
            .requires(
                "docling-ibm-models",
                "3.14.1",
                &["transformers<5.9.0,>=4.42.0"],
            )
            .requires(
                "docling-ibm-models",
                "4.0.0",
                &["transformers<6.0.0,>=4.42.0"],
            )
            .requires(
                "docling-ibm-models",
                "4.0.2",
                &["transformers<6.0.0,>=4.42.0"],
            )
            .release("docling-ibm-models", "3.14.1", "2026-08-20T00:00:00Z")
            .release("docling-ibm-models", "4.0.0rc1", "2026-08-22T00:00:00Z")
            .release("docling-ibm-models", "4.0.0", "2026-08-25T00:00:00Z")
            .release("docling-ibm-models", "4.0.2", "2026-09-03T00:00:00Z");
        let lock = parse_lock(indoc! {r#"
            version = 1
            revision = 3

            [[package]]
            name = "docling-ibm-models"
            version = "3.14.0"
            source = { registry = "https://pypi.org/simple" }
            dependencies = [{ name = "transformers" }]

            [[package]]
            name = "transformers"
            version = "5.8.1"
            source = { registry = "https://pypi.org/simple" }
        "#});
        let ceiling = attribute(&lock, "transformers", "5.15.1", &no_local(), &registry)
            .await
            .expect("the metadata attributes the cap");
        let lift = lift(&lock, &ceiling, "transformers", "5.15.1", &registry)
            .await
            .expect("4.0.0 lifts the cap");
        assert_eq!(lift.version, "4.0.0");

        // Under a 14-day window the lifting release is still cooling, and the row says until
        // when; under an absolute freeze it says which side of the freeze it falls on; without
        // a window only the publish date is stated.
        let detail = ceiling.detail("transformers", Some(&lift), Some("14 days"), now());
        assert_eq!(
            detail,
            r#"held: conflicts with docling-ibm-models 3.14.0, which requires transformers<5.9.0,>=4.42.0 on sys_platform == "darwin"; the cap lifts in docling-ibm-models 4.0.0 (published 2026-08-25, in cooldown until about 2026-09-08)"#
        );
        let frozen = ceiling.detail(
            "transformers",
            Some(&lift),
            Some("2026-08-01T00:00:00Z"),
            now(),
        );
        assert!(
            frozen.ends_with("(published 2026-08-25, after the freeze)"),
            "{frozen:?}"
        );
        let unwindowed = ceiling.detail("transformers", Some(&lift), None, now());
        assert!(
            unwindowed.ends_with("(published 2026-08-25)"),
            "{unwindowed:?}"
        );
        // A release older than the window is simply dated: it was admissible, so something else
        // kept the resolve from taking it, and the row must not claim it is cooling.
        let matured = ceiling.detail("transformers", Some(&lift), Some("3 days"), now());
        assert!(matured.ends_with("(published 2026-08-25)"), "{matured:?}");
    }

    #[tokio::test]
    async fn the_targets_own_requirement_names_a_sibling_the_lock_cannot_satisfy() {
        // litellm 1.91.5 requires openai below 3, and the lock holds openai 3.1.2 (pinned there
        // by the project): the sibling is what blocks the candidate, and the row says which
        // requirement of the target it violates.
        let lock = parse_lock(indoc! {r#"
            version = 1
            revision = 3

            [[package]]
            name = "rag"
            version = "0.1.0"
            source = { virtual = "." }
            dependencies = [{ name = "litellm" }, { name = "openai" }]

            [package.metadata]
            requires-dist = [
                { name = "litellm", specifier = ">=1.83.0" },
                { name = "openai", specifier = ">=3.1.0" },
            ]

            [[package]]
            name = "litellm"
            version = "1.83.0"
            source = { registry = "https://pypi.org/simple" }
            dependencies = [{ name = "openai" }]

            [[package]]
            name = "openai"
            version = "3.1.2"
            source = { registry = "https://pypi.org/simple" }
        "#});
        let registry = Fake::default().requires(
            "litellm",
            "1.91.5",
            &[
                "openai>=2.20.0,<3.0.0",
                r#"fastapi>=0.100; extra == "proxy""#,
            ],
        );
        let ceiling = attribute(&lock, "litellm", "1.91.5", &no_local(), &registry)
            .await
            .expect("the target's metadata attributes the sibling");
        assert_eq!(ceiling.blocker, "openai");
        assert_eq!(
            ceiling.detail("litellm", None, Some("14 days"), now()),
            "held: conflicts with openai 3.1.2: litellm 1.91.5 requires openai>=2.20.0,<3.0.0"
        );
        // The sibling's cap is not the sibling's to lift, so no lift is looked for.
        assert_eq!(
            lift(&lock, &ceiling, "litellm", "1.91.5", &registry).await,
            None
        );
    }

    #[tokio::test]
    async fn a_workspace_requirers_lock_metadata_wins_without_a_registry_read() {
        // `huggingface-hub`'s requirement is in the lock (a workspace source records
        // `requires-dist`), including its marker, so the cap is attributed without asking the
        // registry, which here would fail.
        let lock = parse_lock(indoc! {r#"
            version = 1
            revision = 3

            [[package]]
            name = "huggingface-hub"
            version = "1.18.0"
            source = { editable = "libs/hub" }
            dependencies = [{ name = "typer" }]

            [package.metadata]
            requires-dist = [{ name = "typer", specifier = ">=0.20.0,<0.26.0", marker = "sys_platform == 'linux'" }]

            [[package]]
            name = "typer"
            version = "0.25.1"
            source = { registry = "https://pypi.org/simple" }
        "#});
        let registry = Fake {
            failing: true,
            ..Fake::default()
        };
        let ceiling = attribute(&lock, "typer", "0.26.7", &no_local(), &registry)
            .await
            .expect("the lock attributes the cap");
        assert_eq!(ceiling.blocker, "huggingface-hub");
        assert_eq!(
            ceiling.detail("typer", None, None, now()),
            "held: conflicts with huggingface-hub 1.18.0, which requires typer>=0.20.0,<0.26.0 on sys_platform == 'linux'"
        );
        // A target within the bound is not capped by that requirement.
        assert_eq!(
            requirer_capping_in_lock(
                &lock,
                "typer",
                version::parse("0.25.1").as_ref(),
                &version::parse("0.25.5").expect("a version")
            ),
            None
        );
    }

    /// The single-requirer lock the issue #97 rows were blamed on: `pymupdf` alone reaches
    /// `onnxruntime`, and nothing it declares caps it.
    fn sole_requirer_lock() -> UvLock {
        parse_lock(indoc! {r#"
            version = 1
            revision = 3

            [[package]]
            name = "pymupdf"
            version = "1.26.0"
            source = { registry = "https://pypi.org/simple" }
            dependencies = [{ name = "onnxruntime" }]

            [[package]]
            name = "onnxruntime"
            version = "1.31.0"
            source = { registry = "https://pypi.org/simple" }
        "#})
    }

    #[tokio::test]
    async fn a_resolved_edge_alone_blames_nothing() {
        // A real `uv.lock` often records only resolved edges and the registry knows nothing.
        // The sole package whose edge reaches the held candidate has not been shown to cap it, so
        // naming it would send the user to change a package that cannot release the hold.
        let registry = Fake::default().requires("pymupdf", "1.26.0", &["onnxruntime"]);
        assert_eq!(
            attribute(
                &sole_requirer_lock(),
                "onnxruntime",
                "1.32.0",
                &no_local(),
                &registry
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn the_projects_own_constraint_is_named_with_its_file() {
        // The issue #97 shape: the project pins onnxruntime through `constraint-dependencies`.
        // The constraint is the cause, so the row names it and its manifest, and the reason is
        // the exact pin's.
        let local = local(indoc! {r#"
            [tool.uv]
            constraint-dependencies = ["onnxruntime==1.31.0"]
        "#});
        let ceiling = attribute(
            &sole_requirer_lock(),
            "onnxruntime",
            "1.32.0",
            &local,
            &Fake::default(),
        )
        .await
        .expect("the constraint attributes the hold");
        assert_eq!(ceiling.blocker, "onnxruntime");
        assert_eq!(
            ceiling.detail("onnxruntime", None, None, now()),
            "held: pyproject.toml [tool.uv] constraint-dependencies declares onnxruntime==1.31.0, which excludes 1.32.0"
        );
        assert_eq!(ceiling.skip_reason(false), SkipReason::PinHeld);
        assert_eq!(ceiling.skip_reason(true), SkipReason::PinHeld);
        // The project's own pin is not the blocker's to lift.
        assert_eq!(
            lift(
                &sole_requirer_lock(),
                &ceiling,
                "onnxruntime",
                "1.32.0",
                &Fake::default()
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn the_projects_own_range_holds_as_a_bound_or_a_floor() {
        // A range the project declares holds a forward move as an explicit upper bound and a
        // downgrade as a floor, and the row names the override list it comes from.
        let local = local(indoc! {r#"
            [tool.uv]
            override-dependencies = ["onnxruntime>=1.30,<1.32"]
        "#});
        let forward = attribute(
            &sole_requirer_lock(),
            "onnxruntime",
            "1.32.0",
            &local,
            &Fake::default(),
        )
        .await
        .expect("the override attributes the forward hold");
        assert_eq!(forward.skip_reason(false), SkipReason::DeclaredBoundHeld);
        assert_eq!(
            forward.detail("onnxruntime", None, None, now()),
            "held: pyproject.toml [tool.uv] override-dependencies declares onnxruntime>=1.30,<1.32, which excludes 1.32.0"
        );
        let downgrade = attribute(
            &sole_requirer_lock(),
            "onnxruntime",
            "1.29.0",
            &local,
            &Fake::default(),
        )
        .await
        .expect("the override attributes the downgrade hold");
        assert_eq!(downgrade.skip_reason(true), SkipReason::GraphHeld);
    }

    #[tokio::test]
    async fn a_constraint_the_lock_violates_or_admitting_the_target_blames_nothing() {
        // The constraint admits the target, so something else holds it.
        let admits = local(indoc! {r#"
            [tool.uv]
            constraint-dependencies = ["onnxruntime<2"]
        "#});
        // The constraint excludes the locked version too, so it is not what the graph resolved
        // under (a stale lock), and blaming it would be a guess.
        let violated = local(indoc! {r#"
            [tool.uv]
            constraint-dependencies = ["onnxruntime==1.30.0"]
        "#});
        for local in [admits, violated] {
            assert_eq!(
                attribute(
                    &sole_requirer_lock(),
                    "onnxruntime",
                    "1.32.0",
                    &local,
                    &Fake::default()
                )
                .await,
                None
            );
        }
    }

    #[tokio::test]
    async fn an_override_replaces_the_requirements_that_would_otherwise_be_blamed() {
        // docling-ibm-models caps transformers below 5.9.0, but the project overrides
        // transformers with a range that admits the target, so uv never saw that cap.
        let registry = Fake::default().requires(
            "docling-ibm-models",
            "3.14.0",
            &["transformers<5.9.0,>=4.42.0"],
        );
        let overridden = local(indoc! {r#"
            [tool.uv]
            override-dependencies = ["transformers>=4.42.0"]
        "#});
        assert_eq!(
            attribute(
                &docling_lock(),
                "transformers",
                "5.15.1",
                &overridden,
                &registry
            )
            .await,
            None
        );
        // Without the override the same metadata attributes the cap.
        assert!(
            attribute(
                &docling_lock(),
                "transformers",
                "5.15.1",
                &no_local(),
                &registry
            )
            .await
            .is_some()
        );

        // The target's own requirement on an overridden sibling is replaced as well.
        let lock = parse_lock(indoc! {r#"
            version = 1
            revision = 3

            [[package]]
            name = "litellm"
            version = "1.83.0"
            source = { registry = "https://pypi.org/simple" }
            dependencies = [{ name = "openai" }]

            [[package]]
            name = "openai"
            version = "3.1.2"
            source = { registry = "https://pypi.org/simple" }
        "#});
        let registry = Fake::default().requires("litellm", "1.91.5", &["openai>=2.20.0,<3.0.0"]);
        let overridden = local(indoc! {r#"
            [tool.uv]
            override-dependencies = ["openai>=3"]
        "#});
        assert_eq!(
            attribute(&lock, "litellm", "1.91.5", &overridden, &registry).await,
            None
        );
    }

    #[test]
    fn extras_are_read_from_either_quote_style_and_normalized() {
        let extras = extras_in_marker(Some(
            r#"(sys_platform == "darwin") and (extra == "Chunking" or extra == 'vlm_models')"#,
        ));
        assert_eq!(
            extras.into_iter().collect::<Vec<_>>(),
            vec!["chunking".to_string(), "vlm-models".to_string()]
        );
        assert!(extras_in_marker(Some("python_version >= '3.9'")).is_empty());
        assert!(extras_in_marker(None).is_empty());
    }
}
