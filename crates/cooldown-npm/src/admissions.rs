//! The exact `minimumReleaseAgeExclude` entries a settled pnpm graph needs, judged from the
//! cooldown gate's verdicts on its locked versions.
//!
//! pnpm keeps applying its own `minimumReleaseAge` after cooldown accepts a graph: its frozen
//! install verifies every locked entry against that window.
//! A young version that only a shorter cooldown rule admits therefore needs an exact exemption,
//! and a young version cooldown does not admit is one pnpm refuses.
//! cooldown's publish times come from the public npm registry, so a version earns an exemption
//! only when the lock proves pnpm resolved it from there too.

use crate::exclusions::{
    MatchSemantics, Persisted, exempts, persisted_rules, prune_exact_members, set_exact_permission,
    version_identity,
};
use crate::lock::{LockOrigin, NameVersion};
use crate::native::{EmptyList, NativeAge, render_yaml_block_list};
use crate::npmrc::{RegistryOverrides, advisory_identity};
use camino::Utf8Path;
use cooldown_core::{
    AdmittedVersion, CoreError, JudgedPin, LockedVersion, NativeAssessment, NativeEdit, Result,
    Version,
};
use jiff::{SignedDuration, Timestamp};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// The pnpm setting that lists the packages and versions its release-age gate exempts.
const EXCLUDE_KEY: &str = "minimumReleaseAgeExclude";

/// The `minimumReleaseAge` pnpm applies when nothing configures one, from pnpm 11 on.
const PNPM_11_DEFAULT_MINUTES: i64 = 24 * 60;

/// The versions a lock holds, by package name.
pub(crate) type VersionLines = HashMap<String, BTreeSet<String>>;

/// pnpm's release-age window as the bare manager applies it, in whole minutes.
///
/// Windows order by length, so the greater of two is the stricter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NativeFloor {
    minutes: i64,
}

impl NativeFloor {
    /// The floor of `minutes`, or `None` when the window is not positive and pnpm applies no age
    /// gate at all.
    pub(crate) fn new(minutes: i64) -> Option<Self> {
        (minutes > 0).then_some(NativeFloor { minutes })
    }

    /// The longer of two windows, or the one present.
    pub(crate) fn stricter(left: Option<Self>, right: Option<Self>) -> Option<Self> {
        match (left, right) {
            (Some(left), Some(right)) => Some(if left.minutes >= right.minutes {
                left
            } else {
                right
            }),
            (window, None) | (None, window) => window,
        }
    }

    /// Whether this window is longer than `duration`.
    pub(crate) fn longer_than(self, duration: SignedDuration) -> bool {
        self.minutes.saturating_mul(60) > duration.as_secs()
    }

    /// Whether a version published `age` before the evaluation instant is still inside the
    /// window, as pnpm's cutoff of now minus the window judges it.
    pub(crate) fn holds(self, age: SignedDuration) -> bool {
        age.as_secs() / 60 < self.minutes
    }
}

/// The `minimumReleaseAge` pnpm `version` applies when no configuration sets one, in minutes.
///
/// pnpm 11 gates every resolve by a day unless configured otherwise, and its `config get`
/// reports nothing for that built-in default (`minimum-release-age` in pnpm's defaults).
/// Earlier releases apply none.
/// An unreadable version is read as pnpm 11, the release cooldown is tested against.
pub(crate) fn builtin_minimum_release_age(version: &str) -> i64 {
    let major = version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|major| major.parse::<u64>().ok());
    match major {
        Some(major) if major < 11 => 0,
        _ => PNPM_11_DEFAULT_MINUTES,
    }
}

/// The first pnpm release that records its lockfile verifications in `lockfile-verified.jsonl`
/// under its cache directory, found by searching the published bundles: 11.1.2 has no such file,
/// 11.1.3 writes it.
const FIRST_RECORDING_PNPM: (u64, u64, u64) = (11, 1, 3);

/// The first pnpm release that decides each package by the first `minimumReleaseAgeExclude` rule
/// matching it, found by comparing the published bundles: 10.18.3 folds the whole list into one
/// name matcher (`createMatcher`), while 10.19.0 builds a version policy whose
/// `evaluateVersionPolicy` returns at the first matching rule, as 10.34.9, 11.0.0, 11.4.0 and
/// 11.8.0 do.
const FIRST_FIRST_MATCH_PNPM: (u64, u64, u64) = (10, 19, 0);

/// The first pnpm release that stops deciding by the first matching rule: 11.9.0 joins the
/// versions of every matching exact rule up to the first matching rule without versions, and
/// 11.23.0 lets any matching rule without versions exempt every version.
const FIRST_UNION_PNPM: (u64, u64, u64) = (11, 9, 0);

/// The release pnpm `version` names, as `(major, minor, patch)`, a prerelease read as its
/// release, or `None` when it cannot be read.
fn pnpm_release(version: &str) -> Option<(u64, u64, u64)> {
    let release = version
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or_default();
    let mut parts = release.split('.').map(str::parse::<u64>);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch))) => Some((major, minor, patch)),
        _ => None,
    }
}

/// Whether pnpm `version` records each lockfile verification it runs under its cache directory.
///
/// A prerelease counts as its release, and a version that cannot be read as recording nothing.
pub(crate) fn records_verifications(version: &str) -> bool {
    pnpm_release(version).is_some_and(|release| release >= FIRST_RECORDING_PNPM)
}

/// How pnpm `version` reads a `minimumReleaseAgeExclude` list: first-match from 10.19.0 up to
/// 11.9.0, and unverified for any other or unreadable version.
pub(crate) fn match_semantics(version: &str) -> MatchSemantics {
    match pnpm_release(version) {
        Some(release) if (FIRST_FIRST_MATCH_PNPM..FIRST_UNION_PNPM).contains(&release) => {
            MatchSemantics::FirstMatch
        }
        _ => MatchSemantics::Unverified,
    }
}

/// How completely pnpm's configuration queries report the routing that sends a package to a
/// registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportedRouting {
    /// The queries report every setting that routes a package.
    Complete,
    /// A pnpmfile may be loaded, whose `updateConfig` hook can route a scope, through
    /// `registries`, without the queries reporting it.
    MayBeOverridden,
}

/// The registry versions of `entries` that pnpm provably resolved from the public npm registry,
/// the one cooldown's publish times come from.
///
/// The proof is the one advisory identity needs: the lock entry's own origin record, either an
/// integrity-only resolution from the configured registry or a tarball URL under the public
/// registry, which no routing in the project's files (`file_overrides`) or in pnpm's effective
/// configuration (`effective`) may send elsewhere.
/// Only the recorded URL proves an origin when the reported routing may be overridden
/// (`routing`), since which registry the configured one is then rests on routing pnpm does not
/// report.
/// Returns `None` without an effective configuration, when pnpm could not be asked: the evidence
/// is then incomplete, which is not the same as an origin the evidence disproves.
pub(crate) fn origin_proven_versions(
    entries: &[NameVersion],
    file_overrides: &RegistryOverrides,
    effective: Option<&RegistryOverrides>,
    routing: ReportedRouting,
) -> Option<VersionLines> {
    let effective = effective?;
    let mut proven = VersionLines::new();
    for NameVersion {
        name,
        version,
        origin,
    } in entries
    {
        // A non-registry resolution has no registry publish time to compare.
        if version.contains(':') || matches!(origin, LockOrigin::Unrecorded) {
            continue;
        }
        if routing == ReportedRouting::MayBeOverridden
            && matches!(origin, LockOrigin::ConfiguredRegistry)
        {
            continue;
        }
        if advisory_identity(name, origin, file_overrides).is_some() && !effective.reroutes(name) {
            proven
                .entry(name.clone())
                .or_default()
                .insert(version.clone());
        }
    }
    Some(proven)
}

/// The facts the grant rule judges one locked version by.
pub(crate) struct GrantEvidence<'a> {
    /// The window the bare manager applies.
    pub(crate) floor: NativeFloor,
    /// The locked versions whose publish time is attributable ([`origin_proven_versions`]).
    pub(crate) proven: &'a VersionLines,
}

impl GrantEvidence<'_> {
    /// Whether the locked `version` of `name`, published `age` before the evaluation, is young
    /// under the native window and its publish time comes from the registry pnpm resolved it
    /// from: the rule every exact grant, from an upgrade or from `sync`, has to pass.
    pub(crate) fn young(&self, name: &str, version: &str, age: SignedDuration) -> bool {
        self.floor.holds(age)
            && self
                .proven
                .get(name)
                .is_some_and(|versions| versions.contains(version))
    }
}

/// Cooldown's verdicts on the locked versions, which `sync` mirrors into the native list.
pub(crate) struct MirroredVerdicts<'a> {
    /// The locked versions whose publish time is attributable ([`origin_proven_versions`]).
    pub(crate) proven: &'a VersionLines,
    /// The locked versions cooldown admits, with their ages: every one young under `floor`
    /// and every one of a package an existing exact entry names.
    pub(crate) admitted: &'a [AdmittedVersion],
    /// The window the bare manager applies, or `None` when it applies none.
    pub(crate) floor: Option<NativeFloor>,
}

/// What lets an existing exact `minimumReleaseAgeExclude` member go, the one rule every writer
/// of the list follows.
pub(crate) struct ExemptionEvidence<'a> {
    /// Every version the lock holds.
    pub(crate) locked: &'a VersionLines,
    /// Cooldown's verdicts, where the list mirrors them as `sync` writes it, or `None` where it
    /// keeps every exemption the lock still holds, as an upgrade's assessment does.
    pub(crate) mirrored: Option<MirroredVerdicts<'a>>,
}

impl ExemptionEvidence<'_> {
    /// Whether the existing exact exemption of the version of `name` whose identity is `identity`
    /// may go: the lock no longer holds that version, or the mirrored verdicts show, by a publish
    /// time of proven origin, that cooldown does not admit it or that it is no longer young under
    /// the floor.
    ///
    /// An unproven origin, or missing verdicts, keeps it, since bare pnpm may still need it to
    /// accept the lock.
    pub(crate) fn drops(&self, name: &str, identity: &str) -> bool {
        let holds = |lines: &VersionLines| {
            lines.get(name).is_some_and(|versions| {
                versions
                    .iter()
                    .any(|version| version_identity(version).as_deref() == Some(identity))
            })
        };
        if !holds(self.locked) {
            return true;
        }
        let Some(MirroredVerdicts {
            proven,
            admitted,
            floor,
        }) = &self.mirrored
        else {
            return false;
        };
        holds(proven)
            && admitted
                .iter()
                .find(|version| {
                    version.name == name
                        && version_identity(version.version.as_str()).as_deref() == Some(identity)
                })
                .is_none_or(|version| !floor.is_some_and(|floor| floor.holds(version.age)))
    }
}

/// The `minimumReleaseAgeExclude` rules `sync` writes: the `selectors`, then the `existing` exact
/// entries without the members `evidence` lets go ([`ExemptionEvidence::drops`]), pruned as an
/// upgrade prunes them under the `semantics` of the pnpm reading the list
/// ([`prune_exact_members`]), then the `granted` versions, joined to the first rule that matches
/// their package as pnpm reads it.
///
/// Without evidence, as when cooldown's verdicts, pnpm's window or its routing could not be read,
/// every existing exact entry pnpm still reads is kept as written.
pub(crate) fn synced_rules(
    selectors: &[String],
    existing: &[String],
    semantics: MatchSemantics,
    evidence: Option<&ExemptionEvidence<'_>>,
    granted: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<String> {
    let listed: Vec<String> = selectors.iter().chain(existing).cloned().collect();
    let mut rules = prune_exact_members(&listed, semantics, |name, identity| {
        evidence.is_some_and(|evidence| evidence.drops(name, identity))
    });
    for (name, versions) in granted {
        set_exact_permission(&mut rules, name, &BTreeSet::new(), versions);
    }
    rules
}

/// One pnpm release-age gate: its window and its ordered exclusion rules.
#[derive(Clone, Copy)]
pub(crate) struct NativeGate<'a> {
    /// The window, or `None` when the gate applies none.
    pub(crate) floor: Option<NativeFloor>,
    /// The ordered exclusion rules, which pnpm reads by first match.
    pub(crate) rules: &'a [String],
}

impl NativeGate<'_> {
    /// Whether this gate refuses `version` of `name`, published `age` before the evaluation:
    /// young under its window and exempt under none of its rules.
    pub(crate) fn refuses(self, name: &str, version: &str, age: SignedDuration) -> bool {
        self.floor.is_some_and(|floor| floor.holds(age)) && !exempts(self.rules, name, version)
    }
}

/// The `admitted` versions, as `name@version`, that the `written` settings file admits on its
/// own but pnpm's `effective` gate on this machine refuses, which only a setting above the file
/// explains.
pub(crate) fn overridden_admissions(
    admitted: &[AdmittedVersion],
    written: NativeGate<'_>,
    effective: NativeGate<'_>,
) -> Vec<String> {
    admitted
        .iter()
        .filter(|version| {
            let name = version.name.as_str();
            let number = version.version.as_str();
            !written.refuses(name, number, version.age)
                && effective.refuses(name, number, version.age)
        })
        .map(|version| format!("{}@{}", version.name, version.version))
        .collect()
}

/// The native settings file the grants are written into, as the batch's journal captured it.
pub(crate) struct NativeFile<'a> {
    /// The file, relative to the project root.
    pub(crate) path: &'a Utf8Path,
    /// Its text, empty when the file is absent.
    pub(crate) text: &'a str,
}

/// The facts [`assess`] judges a settled graph from.
pub(crate) struct AssessmentInput<'a> {
    /// Every locked registry version, as the cooldown gate judged it.
    pub(crate) pins: &'a [JudgedPin],
    /// The versions pnpm refused in the lock the run started from.
    pub(crate) refused_at_start: &'a HashSet<LockedVersion>,
    /// The instant the verdicts were reached at.
    pub(crate) now: Timestamp,
    /// What makes a version earn a grant.
    pub(crate) evidence: GrantEvidence<'a>,
    /// Every version the lock holds, whose exemptions the written list keeps.
    pub(crate) locked: &'a VersionLines,
    /// How the pnpm reading the list decides a package among its rules.
    pub(crate) semantics: MatchSemantics,
    /// The native settings file the grants are written into.
    pub(crate) native: NativeFile<'a>,
}

/// The exact exemptions a settled graph needs, and the native file with them written.
///
/// A grant is a young, origin-proven locked version that cooldown admits, where a version whose
/// publish time is unknown or unattributable earns none, or a version pnpm refused in the lock the
/// run started from that the lock still holds.
/// pnpm judged that one against the window and the registry it actually applies, and its
/// exemption only keeps a pin the project already had, so it needs neither cooldown's publish
/// time nor proof of its origin.
/// The edit writes the grants into the file's own `minimumReleaseAgeExclude` and prunes the exact
/// versions the lock no longer holds, keeping every other entry the user wrote, and is `None`
/// when the file already reads that way.
///
/// # Errors
///
/// Returns [`CoreError::Config`] when the workspace setting is a single string and versions must
/// be granted, since no rewritten list reproduces what pnpm reads from a scalar, and
/// [`CoreError::Parse`] when the file is no YAML document pnpm could read.
pub(crate) fn assess(input: AssessmentInput<'_>) -> Result<NativeAssessment> {
    let AssessmentInput {
        pins,
        refused_at_start,
        now,
        evidence,
        locked,
        semantics,
        native,
    } = input;
    let admitted = pins
        .iter()
        .filter(|pin| {
            pin.admitted
                && pin.published_at.is_some_and(|published_at| {
                    evidence.young(
                        &pin.name,
                        pin.version.as_str(),
                        now.duration_since(published_at),
                    )
                })
        })
        .map(|pin| (pin.name.as_str(), pin.version.as_str()));
    let still_refused = refused_at_start
        .iter()
        .filter(|refused| {
            locked
                .get(&refused.name)
                .is_some_and(|versions| versions.contains(refused.version.as_str()))
        })
        .map(|refused| (refused.name.as_str(), refused.version.as_str()));
    let mut granted = BTreeMap::<String, BTreeSet<String>>::new();
    for (name, version) in admitted.chain(still_refused) {
        granted
            .entry(name.to_string())
            .or_default()
            .insert(version.to_string());
    }
    let exclusions = NativeAge::parse(native.text, native.path)?.exclusions;
    let evidence = ExemptionEvidence {
        locked,
        mirrored: None,
    };
    let drops = |name: &str, identity: &str| evidence.drops(name, identity);
    let edit = match persisted_rules(exclusions, &granted, semantics, drops) {
        Persisted::Rules(rules) => render_yaml_block_list(
            native.text,
            native.path,
            EXCLUDE_KEY,
            &rules,
            EmptyList::IfPresent,
        )?
        .map(|text| NativeEdit {
            path: native.path.to_owned(),
            contents: text.into_bytes(),
        }),
        Persisted::Unchanged => None,
        Persisted::ScalarSetting => return Err(scalar_setting_error()),
    };
    let grants = granted
        .into_iter()
        .flat_map(|(name, versions)| {
            versions.into_iter().map(move |version| LockedVersion {
                name: name.clone(),
                version: Version::new(version),
            })
        })
        .collect();
    Ok(NativeAssessment { grants, edit })
}

/// Whether a native gate with `floor` has nothing to refuse in a lock holding `locked`: every
/// version there is older than the window by a publish time in `published`, which pnpm reads
/// from the same registry ([`origin_proven_versions`], `proven`).
///
/// A version without a known publish time, or with one another registry may disagree with,
/// counts as young.
pub(crate) fn all_mature(
    floor: NativeFloor,
    locked: &VersionLines,
    proven: &VersionLines,
    published: &HashMap<LockedVersion, Timestamp>,
    now: Timestamp,
) -> bool {
    locked.iter().all(|(name, versions)| {
        versions.iter().all(|version| {
            proven
                .get(name)
                .is_some_and(|proven| proven.contains(version))
                && published
                    .get(&LockedVersion {
                        name: name.clone(),
                        version: Version::new(version.as_str()),
                    })
                    .is_some_and(|published_at| !floor.holds(now.duration_since(*published_at)))
        })
    })
}

/// Whether pnpm's effective configuration `listing`, as `config list --json` prints it, has pnpm
/// load a pnpmfile besides the default ones: a `pnpmfile` or `globalPnpmfile`, under pnpm 11's
/// spelling or pnpm 10's, or a plugin among its `configDependencies`, whose own pnpmfile pnpm
/// loads too.
///
/// A listing that cannot be read may name one.
pub(crate) fn configures_pnpmfile(listing: &str) -> bool {
    let Ok(serde_json::Value::Object(config)) = serde_json::from_str(listing) else {
        return true;
    };
    config.iter().any(|(key, value)| {
        let key = key.replace('-', "").to_ascii_lowercase();
        match (key.as_str(), value) {
            (_, serde_json::Value::Null) => false,
            ("pnpmfile" | "globalpnpmfile", serde_json::Value::String(path)) => !path.is_empty(),
            ("pnpmfile" | "globalpnpmfile", serde_json::Value::Array(paths)) => !paths.is_empty(),
            ("configdependencies", serde_json::Value::Object(dependencies)) => {
                dependencies.keys().any(|name| is_pnpm_plugin(name))
            }
            ("pnpmfile" | "globalpnpmfile" | "configdependencies", _) => true,
            _ => false,
        }
    })
}

/// Whether pnpm loads the pnpmfile of the configuration dependency `name` as a plugin's, as
/// pnpm's `isPluginName` decides: `pnpm-plugin-*`, `@pnpm/plugin-*` or `@<scope>/pnpm-plugin-*`.
fn is_pnpm_plugin(name: &str) -> bool {
    name.starts_with("pnpm-plugin-")
        || name.starts_with("@pnpm/plugin-")
        || (name.starts_with('@') && name.contains("/pnpm-plugin-"))
}

/// The error for a single-string `minimumReleaseAgeExclude` that exact versions must be added to.
pub(crate) fn scalar_setting_error() -> CoreError {
    CoreError::Config(
        "pnpm-workspace.yaml: minimumReleaseAgeExclude is a single string; make it a list so \
         cooldown can record the release-age exemptions it admitted"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::EffectiveRegistryQuery;
    use crate::npmrc::overrides_from_effective_config;
    use color_eyre::eyre;
    use indoc::indoc;

    const NOW: &str = "2026-11-20T00:00:00Z";

    fn timestamp(value: &str) -> Timestamp {
        value.parse().expect("fixed fixture timestamp")
    }

    /// A pnpm 11 floor of 14 days.
    fn floor() -> NativeFloor {
        NativeFloor::new(14 * 24 * 60).expect("a positive window")
    }

    fn pin(name: &str, version: &str, published_at: Option<&str>, admitted: bool) -> JudgedPin {
        JudgedPin {
            name: name.to_string(),
            version: Version::new(version),
            published_at: published_at.map(timestamp),
            admitted,
        }
    }

    fn locked(name: &str, version: &str) -> LockedVersion {
        LockedVersion {
            name: name.to_string(),
            version: Version::new(version),
        }
    }

    fn lines(entries: &[(&str, &str)]) -> VersionLines {
        let mut lines = VersionLines::new();
        for (name, version) in entries {
            lines
                .entry((*name).to_string())
                .or_default()
                .insert((*version).to_string());
        }
        lines
    }

    fn strings(entries: &[&str]) -> Vec<String> {
        entries.iter().map(ToString::to_string).collect()
    }

    /// The facts of one assessment.
    struct Case<'a> {
        pins: &'a [JudgedPin],
        refused_at_start: &'a HashSet<LockedVersion>,
        locked: &'a VersionLines,
        proven: &'a VersionLines,
        native: &'a str,
    }

    fn assess_case(case: &Case<'_>) -> Result<NativeAssessment> {
        assess(AssessmentInput {
            pins: case.pins,
            refused_at_start: case.refused_at_start,
            now: timestamp(NOW),
            evidence: GrantEvidence {
                floor: floor(),
                proven: case.proven,
            },
            locked: case.locked,
            semantics: MatchSemantics::FirstMatch,
            native: NativeFile {
                path: Utf8Path::new("pnpm-workspace.yaml"),
                text: case.native,
            },
        })
    }

    fn edited(assessment: &NativeAssessment) -> Option<String> {
        assessment
            .edit
            .as_ref()
            .map(|edit| String::from_utf8_lossy(&edit.contents).into_owned())
    }

    /// pnpm records its verifications from 11.1.3 on, pnpm 12 included, and an unreadable
    /// version records nothing.
    #[test]
    fn verification_records_start_with_pnpm_11_1_3() {
        assert!(records_verifications("11.1.3"));
        assert!(records_verifications("11.8.0\n"));
        assert!(records_verifications("12.21.0"));
        assert!(records_verifications("12.0.0-rc.5"));
        assert!(!records_verifications("11.1.2"));
        assert!(!records_verifications("11.0.0"));
        assert!(!records_verifications("10.34.9"));
        assert!(!records_verifications("9.15.9"));
        assert!(!records_verifications("unknown"));
    }

    /// pnpm reads the exclusion list by first match from 10.19.0 up to 11.9.0; 10.18 folds it into
    /// one name matcher, 11.9.0 on joins the matching exact rules, and an unreadable version is
    /// not known to read it either way.
    #[test]
    fn first_match_semantics_run_from_pnpm_10_19_to_11_9() {
        for version in ["10.19.0", "10.34.9", "11.0.0", "11.8.0\n", "v11.1.0-rc.1"] {
            assert_eq!(
                match_semantics(version),
                MatchSemantics::FirstMatch,
                "{version}"
            );
        }
        for version in [
            "10.18.3", "10.14.0", "9.15.9", "11.9.0", "11.16.0", "11.28.13", "12.14.0", "unknown",
            "",
        ] {
            assert_eq!(
                match_semantics(version),
                MatchSemantics::Unverified,
                "{version}"
            );
        }
    }

    /// pnpm 11 applies a day when nothing configures a window, earlier releases none, and an
    /// unreadable version reads as the pnpm 11 cooldown targets.
    #[test]
    fn the_builtin_floor_follows_the_pnpm_major() {
        assert_eq!(builtin_minimum_release_age("11.8.0"), 1440);
        assert_eq!(builtin_minimum_release_age("12.0.0-rc.1\n"), 1440);
        assert_eq!(builtin_minimum_release_age("10.14.0"), 0);
        assert_eq!(builtin_minimum_release_age("9.15.9"), 0);
        assert_eq!(builtin_minimum_release_age("unknown"), 1440);
        assert!(NativeFloor::new(0).is_none());
    }

    /// Only a young admitted version of proven origin is granted and written: a young unadmitted
    /// version, a mature one, one of unknown age and one whose origin is unproven earn nothing.
    #[test]
    fn only_young_admitted_versions_of_proven_origin_are_granted() -> Result<()> {
        let pins = [
            // Ten days old, admitted by a shorter package rule
            pin(
                "@sentry/utils",
                "5.30.0",
                Some("2026-11-10T00:00:00Z"),
                true,
            ),
            // Ten days old, held by the default window
            pin(
                "@sentry/types",
                "5.30.0",
                Some("2026-11-10T00:00:00Z"),
                false,
            ),
            // Mature under the native window
            pin("tslib", "1.14.1", Some("2020-01-01T00:00:00Z"), true),
            // Of unknown age
            pin("left-pad", "1.3.0", None, true),
            // Young, but from a registry the lock does not prove
            pin("@corp/ui", "2.0.0", Some("2026-11-19T00:00:00Z"), true),
        ];
        let locked_lines = lines(&[
            ("@sentry/utils", "5.30.0"),
            ("@sentry/types", "5.30.0"),
            ("tslib", "1.14.1"),
            ("left-pad", "1.3.0"),
            ("@corp/ui", "2.0.0"),
        ]);
        let proven = lines(&[
            ("@sentry/utils", "5.30.0"),
            ("@sentry/types", "5.30.0"),
            ("tslib", "1.14.1"),
            ("left-pad", "1.3.0"),
        ]);
        let assessment = assess_case(&Case {
            pins: &pins,
            refused_at_start: &HashSet::new(),
            locked: &locked_lines,
            proven: &proven,
            native: "packages: []\nminimumReleaseAge: 20160\n",
        })?;
        assert_eq!(assessment.grants, [locked("@sentry/utils", "5.30.0")]);
        assert_eq!(
            edited(&assessment).as_deref(),
            Some(indoc! {r#"
                packages: []
                minimumReleaseAge: 20160
                minimumReleaseAgeExclude:
                  - "@sentry/utils@5.30.0"
            "#})
        );
        Ok(())
    }

    /// A file that already reads the way the grants would write it is left alone.
    #[test]
    fn a_file_that_already_grants_everything_is_not_edited() -> Result<()> {
        let pins = [pin("granted", "1.0.0", Some("2026-11-15T00:00:00Z"), true)];
        let locked_lines = lines(&[("granted", "1.0.0")]);
        let assessment = assess_case(&Case {
            pins: &pins,
            refused_at_start: &HashSet::new(),
            locked: &locked_lines,
            proven: &locked_lines,
            native: "minimumReleaseAgeExclude:\n  - \"granted@1.0.0\"\n",
        })?;
        assert_eq!(assessment.edit, None);
        Ok(())
    }

    /// A version pnpm refused in the starting lock is granted while the lock still holds it at
    /// that version, on pnpm's verdict alone: one from a registry cooldown cannot prove, as a
    /// transparent mirror serves it, and one whose publish time cooldown does not know or reads
    /// as mature are granted too.
    #[test]
    fn a_version_pnpm_refused_at_the_start_is_granted_while_still_locked() -> Result<()> {
        let pins = [
            pin("migrated", "2.0.0", Some("2026-11-15T00:00:00Z"), false),
            pin("@corp/ui", "1.0.0", Some("2026-11-15T00:00:00Z"), false),
            pin("unknown-age", "1.0.0", None, false),
            pin("mature", "1.0.0", Some("2020-01-01T00:00:00Z"), false),
        ];
        let locked_lines = lines(&[
            ("migrated", "2.0.0"),
            ("@corp/ui", "1.0.0"),
            ("unknown-age", "1.0.0"),
            ("mature", "1.0.0"),
        ]);
        let proven = lines(&[("migrated", "2.0.0"), ("mature", "1.0.0")]);
        let refused_at_start = HashSet::from([
            locked("migrated", "2.0.0"),
            locked("@corp/ui", "1.0.0"),
            locked("unknown-age", "1.0.0"),
            locked("mature", "1.0.0"),
            // No longer locked at this version, so nothing is granted for it.
            locked("migrated", "1.0.0"),
        ]);
        let assessment = assess_case(&Case {
            pins: &pins,
            refused_at_start: &refused_at_start,
            locked: &locked_lines,
            proven: &proven,
            native: "packages: []\n",
        })?;
        assert_eq!(
            assessment.grants,
            [
                locked("@corp/ui", "1.0.0"),
                locked("mature", "1.0.0"),
                locked("migrated", "2.0.0"),
                locked("unknown-age", "1.0.0"),
            ]
        );
        assert_eq!(
            edited(&assessment).as_deref(),
            Some(indoc! {r#"
                packages: []
                minimumReleaseAgeExclude:
                  - "@corp/ui@1.0.0"
                  - "mature@1.0.0"
                  - "migrated@2.0.0"
                  - "unknown-age@1.0.0"
            "#})
        );
        Ok(())
    }

    /// A pre-existing violation earns no new grant, while the exact entry the user wrote for it
    /// survives the edit a grant for another version makes.
    #[test]
    fn a_pre_existing_violation_keeps_only_the_entry_the_user_wrote() -> Result<()> {
        let pins = [
            pin("legacy", "3.0.0", Some("2026-11-15T00:00:00Z"), false),
            pin("unlisted", "1.0.0", Some("2026-11-15T00:00:00Z"), false),
            pin("granted", "1.0.0", Some("2026-11-15T00:00:00Z"), true),
        ];
        let locked_lines = lines(&[
            ("legacy", "3.0.0"),
            ("unlisted", "1.0.0"),
            ("granted", "1.0.0"),
        ]);
        let assessment = assess_case(&Case {
            pins: &pins,
            refused_at_start: &HashSet::new(),
            locked: &locked_lines,
            proven: &locked_lines,
            native: "minimumReleaseAgeExclude:\n  - \"legacy@3.0.0\"\n",
        })?;
        assert_eq!(assessment.grants, [locked("granted", "1.0.0")]);
        assert_eq!(
            edited(&assessment).as_deref(),
            Some("minimumReleaseAgeExclude:\n  - \"legacy@3.0.0\"\n  - \"granted@1.0.0\"\n")
        );
        Ok(())
    }

    /// The native gate has nothing to refuse only when every locked version is known to be older
    /// than its window by a publish time of proven origin; a young version, one of unknown
    /// publish time, or one of unproven origin leaves it something.
    #[test]
    fn a_lock_is_all_mature_only_when_every_version_is_known_old() {
        let now = timestamp(NOW);
        let locked_lines = lines(&[("old", "1.0.0"), ("also-old", "2.0.0")]);
        let mut published = HashMap::new();
        published.insert(locked("old", "1.0.0"), timestamp("2020-01-01T00:00:00Z"));
        assert!(!all_mature(
            floor(),
            &locked_lines,
            &locked_lines,
            &published,
            now
        ));
        published.insert(
            locked("also-old", "2.0.0"),
            timestamp("2021-01-01T00:00:00Z"),
        );
        assert!(all_mature(
            floor(),
            &locked_lines,
            &locked_lines,
            &published,
            now
        ));
        // A publish time from a registry pnpm may not read proves nothing.
        let proven = lines(&[("old", "1.0.0")]);
        assert!(!all_mature(
            floor(),
            &locked_lines,
            &proven,
            &published,
            now
        ));
        published.insert(
            locked("also-old", "2.0.0"),
            timestamp("2026-11-19T00:00:00Z"),
        );
        assert!(!all_mature(
            floor(),
            &locked_lines,
            &locked_lines,
            &published,
            now
        ));
    }

    /// pnpm loads a pnpmfile that its configuration names under either spelling, or a plugin's
    /// among its configuration dependencies; an unreadable listing may name one too.
    #[test]
    fn a_configured_pnpmfile_is_found_in_the_effective_configuration() {
        let loads = [
            r#"{"pnpmfile":["/repo/hooks.cjs"]}"#,
            r#"{"pnpmfile":"hooks.cjs"}"#,
            r#"{"globalPnpmfile":"/home/user/global.cjs"}"#,
            r#"{"global-pnpmfile":"/home/user/global.cjs"}"#,
            r#"{"configDependencies":{"pnpm-plugin-policy":"1.0.0+sha512-a"}}"#,
            r#"{"configDependencies":{"@pnpm/plugin-better-defaults":"1.0.0+sha512-a"}}"#,
            r#"{"configDependencies":{"@corp/pnpm-plugin-policy":"1.0.0+sha512-a"}}"#,
            "not json",
        ];
        for listing in loads {
            assert!(configures_pnpmfile(listing), "{listing}");
        }
        let none = [
            r#"{"registry":"https://registry.npmjs.org/"}"#,
            r#"{"pnpmfile":[],"globalPnpmfile":null}"#,
            r#"{"configDependencies":{"@corp/patches":"1.0.0+sha512-a"}}"#,
        ];
        for listing in none {
            assert!(!configures_pnpmfile(listing), "{listing}");
        }
    }

    #[test]
    fn synced_rules_render_globs_then_exact_version_unions() {
        let globs = vec!["z-*".to_string(), "@scope/*".to_string(), "z-*".to_string()];
        let versions = BTreeMap::from([
            (
                "a".to_string(),
                BTreeSet::from(["2.0.1".to_string(), "1.2.2".to_string()]),
            ),
            (
                "@scope/a".to_string(),
                BTreeSet::from(["1.0.0".to_string()]),
            ),
            ("z-tool".to_string(), BTreeSet::from(["1.0.0".to_string()])),
            ("b".to_string(), BTreeSet::from(["3.0.0".to_string()])),
            ("empty".to_string(), BTreeSet::new()),
        ]);
        let selectors = crate::native::NativeSelectors::partition(&globs);
        assert_eq!(
            synced_rules(
                &selectors.written,
                &[],
                MatchSemantics::FirstMatch,
                None,
                &versions
            ),
            ["@scope/*", "z-*", "a@1.2.2||2.0.1", "b@3.0.0"]
        );
        assert_eq!(
            synced_rules(&[], &[], MatchSemantics::FirstMatch, None, &BTreeMap::new()),
            [] as [String; 0]
        );
    }

    /// An admitted exact version stays in the list unless a *written* selector covers its name
    /// under pnpm's matcher: an omitted brace selector cooldown matches it with covers nothing on
    /// pnpm's side, so dropping the union would leave the admitted version gated.
    #[test]
    fn an_exact_union_survives_a_selector_pnpm_cannot_read() {
        let selectors =
            crate::native::NativeSelectors::partition(&["@types/{node,react}".to_string()]);
        let versions = BTreeMap::from([(
            "@types/node".to_string(),
            BTreeSet::from(["22.0.0".to_string()]),
        )]);
        let exclusions = synced_rules(
            &selectors.written,
            &[],
            MatchSemantics::FirstMatch,
            None,
            &versions,
        );
        assert_eq!(exclusions, ["@types/node@22.0.0"]);
        assert!(crate::exclusions::exempts(
            &exclusions,
            "@types/node",
            "22.0.0"
        ));

        // A written wildcard that pnpm matches the name with still makes the union redundant.
        let selectors = crate::native::NativeSelectors::partition(&["@types/*".to_string()]);
        assert_eq!(
            synced_rules(
                &selectors.written,
                &[],
                MatchSemantics::FirstMatch,
                None,
                &versions
            ),
            ["@types/*"]
        );
    }

    /// A scalar setting cannot take an exact grant, which is a configuration error.
    #[test]
    fn a_scalar_setting_cannot_take_a_grant() {
        let pins = [pin("granted", "1.0.0", Some("2026-11-15T00:00:00Z"), true)];
        let locked_lines = lines(&[("granted", "1.0.0")]);
        let assessed = assess_case(&Case {
            pins: &pins,
            refused_at_start: &HashSet::new(),
            locked: &locked_lines,
            proven: &locked_lines,
            native: "minimumReleaseAgeExclude: left-pad\n",
        });
        std::assert_matches!(assessed, Err(CoreError::Config(_)));
    }

    /// Origin is proven from real pnpm lock text: an integrity-only entry is the configured
    /// registry's, which pnpm's effective routing must name as the public one for the package's
    /// scope, and a tarball URL entry, as `lockfileIncludeTarballUrl` writes one, must be under
    /// the public registry itself.
    #[test]
    fn origin_is_proven_by_the_lock_record_and_pnpms_routing() -> eyre::Result<()> {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            packages:

              lodash@4.17.21:
                resolution: {integrity: sha512-a}

              is-number@7.0.0:
                resolution: {integrity: sha512-b, tarball: https://registry.npmjs.org/is-number/-/is-number-7.0.0.tgz}

              mirrored@1.0.0:
                resolution: {integrity: sha512-c, tarball: https://npm.mirror.example/mirrored/-/mirrored-1.0.0.tgz}

              '@corp/ui@2.0.0':
                resolution: {integrity: sha512-d}

              pinned-git@1.2.0:
                resolution: {commit: abc123, repo: https://github.com/user/pinned-git.git, type: git}
        "};
        let entries = <crate::lock::Pnpm as crate::lock::NodeLock>::parse(lock)?;
        // pnpm 11 reports a workspace `registries:` map as its own object.
        let effective = overrides_from_effective_config(
            r#"{"registry":"https://registry.npmjs.org/","registries":{"default":"https://registry.npmjs.org/","@corp":"https://npm.corp.example/"}}"#,
            EffectiveRegistryQuery::Proves,
        );

        let proven_names = |routing: ReportedRouting| -> eyre::Result<Vec<String>> {
            let proven = origin_proven_versions(
                &entries,
                &RegistryOverrides::none(),
                effective.as_ref(),
                routing,
            )
            .ok_or_else(|| eyre::eyre!("the effective configuration is evidence"))?;
            let mut names: Vec<String> = proven.into_keys().collect();
            names.sort_unstable();
            Ok(names)
        };
        assert_eq!(
            proven_names(ReportedRouting::Complete)?,
            ["is-number", "lodash"]
        );
        // Routing pnpm may not report proves nothing about the configured registry, so only the
        // recorded public tarball URL stands.
        assert_eq!(
            proven_names(ReportedRouting::MayBeOverridden)?,
            ["is-number"]
        );

        // Without pnpm's effective configuration the evidence is incomplete, not a disproof.
        assert_eq!(
            origin_proven_versions(
                &entries,
                &RegistryOverrides::none(),
                None,
                ReportedRouting::Complete
            ),
            None
        );
        Ok(())
    }

    /// An admitted version is reported overridden only when the written file admits it on its own
    /// and pnpm's effective gate here refuses it: young under the effective window and exempt
    /// under none of the effective rules, read by first match.
    #[test]
    fn an_override_is_a_refusal_the_written_file_does_not_explain() {
        let days = |count: i64| SignedDuration::from_hours(24 * count);
        let admitted = |name: &str, age: SignedDuration| AdmittedVersion {
            name: name.to_string(),
            version: Version::new("1.0.0"),
            age,
        };
        let versions = [
            // Granted by the file, but the environment's rules replace its list
            admitted("granted", days(3)),
            // Mature under the file's week, young under the environment's month
            admitted("aged", days(10)),
            // Exempt under the environment's own rules too
            admitted("@corp/ui", days(3)),
            // Refused by the file itself, which a warning of its own reports
            admitted("unlisted", days(3)),
            // Mature everywhere
            admitted("old", days(60)),
        ];
        let written_rules = strings(&["granted@1.0.0"]);
        let effective_rules = strings(&["@corp/*", "granted@2.0.0"]);
        let overridden = overridden_admissions(
            &versions,
            NativeGate {
                floor: NativeFloor::new(7 * 24 * 60),
                rules: &written_rules,
            },
            NativeGate {
                floor: NativeFloor::new(30 * 24 * 60),
                rules: &effective_rules,
            },
        );
        assert_eq!(overridden, ["granted@1.0.0", "aged@1.0.0"]);

        // A gate without a window refuses nothing.
        let unwindowed = NativeGate {
            floor: None,
            rules: &[],
        };
        assert!(!unwindowed.refuses("granted", "1.0.0", days(0)));
    }

    /// A version earns a grant only while young under the native window and proven.
    #[test]
    fn the_grant_rule_needs_youth_and_proven_origin() {
        let proven = lines(&[("lodash", "4.17.21")]);
        let evidence = GrantEvidence {
            floor: floor(),
            proven: &proven,
        };
        let days = |count: i64| SignedDuration::from_hours(24 * count);
        assert!(evidence.young("lodash", "4.17.21", days(13)));
        assert!(!evidence.young("lodash", "4.17.21", days(14)));
        assert!(!evidence.young("lodash", "4.17.20", days(1)));
        assert!(!evidence.young("left-pad", "1.3.0", days(1)));
    }
}
