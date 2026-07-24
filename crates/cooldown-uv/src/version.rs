//! PEP 440 version semantics via `pep440_rs` (the implementation uv itself uses). PEP 440 is *not*
//! semver — epochs, release segments, and pre/post/dev suffixes order differently — so the core
//! relies on this module for the opaque ordering token, the update kind, and the major key.

use cooldown_core::{MajorKey, UpdateKind};
use pep440_rs::{Operator, Version, VersionSpecifiers};
use std::cmp::Ordering;
use std::str::FromStr;

/// Parses a string as a [PEP 440] [`Version`], returning `None` if it is invalid.
///
/// This is the single entry point every other function here builds on, so an
/// unparsable input degrades gracefully (e.g. sorts last) rather than panicking.
///
/// [PEP 440]: https://peps.python.org/pep-0440/
///
/// # Examples
///
/// ```
/// use cooldown_uv::version::parse;
///
/// assert!(parse("1!2.3.4rc1").is_some());
/// assert!(parse("not-a-version").is_none());
/// ```
#[must_use]
pub fn parse(v: &str) -> Option<Version> {
    Version::from_str(v).ok()
}

/// Returns `true` if `v` is a pre-release: it has a pre or dev segment.
///
/// Post-releases (e.g. `1.0.post1`) are *stable*, and an unparsable version is
/// treated as not a pre-release.
///
/// # Examples
///
/// ```
/// use cooldown_uv::version::is_prerelease;
///
/// assert!(is_prerelease("2.0.0rc1"));
/// assert!(!is_prerelease("2.0.0"));
/// assert!(!is_prerelease("1.0.post1"));
/// ```
#[must_use]
pub fn is_prerelease(v: &str) -> bool {
    parse(v).is_some_and(|x| x.any_prerelease())
}

fn seg(v: &Version, i: usize) -> u64 {
    v.release().get(i).copied().unwrap_or(0)
}

/// Returns the [`MajorKey`] gating `--major`: `epoch!major`.
///
/// Two versions share a major key iff a step between them is *not* a major bump.
/// The epoch is included because an epoch difference is always breaking. An
/// unparsable version yields an empty key.
///
/// # Examples
///
/// ```
/// use cooldown_uv::version::major_key;
/// use cooldown_core::MajorKey;
///
/// assert_eq!(major_key("2.0.0"), MajorKey("0!2".into()));
/// assert_eq!(major_key("1!1.0"), MajorKey("1!1".into()));
/// ```
#[must_use]
pub fn major_key(v: &str) -> MajorKey {
    match parse(v) {
        Some(x) => MajorKey(format!("{}!{}", x.epoch(), seg(&x, 0))),
        None => MajorKey(String::new()),
    }
}

/// Returns the first PEP 440 release segment, ignoring any epoch.
#[must_use]
pub fn major_number(v: &str) -> Option<u64> {
    parse(v).and_then(|version| version.release().first().copied())
}

#[derive(Clone)]
struct UpperBound {
    version: Version,
    inclusive: bool,
}

fn explicit_upper_bound(specifier: &str) -> Option<UpperBound> {
    let parsed = VersionSpecifiers::from_str(specifier).ok()?;
    parsed
        .iter()
        .filter_map(|specifier| {
            let inclusive = match specifier.operator() {
                Operator::LessThan => false,
                Operator::LessThanEqual => true,
                _ => return None,
            };
            Some(UpperBound {
                version: specifier.version().clone(),
                inclusive,
            })
        })
        .min_by(|a, b| {
            a.version
                .cmp(&b.version)
                .then_with(|| a.inclusive.cmp(&b.inclusive))
        })
}

/// Chooses the most restrictive PEP 440 specifier with an explicit upper comparator.
#[must_use]
pub fn most_restrictive_declared_bound(
    specifiers: impl IntoIterator<Item = String>,
) -> Option<String> {
    let mut best: Option<(UpperBound, String)> = None;
    for specifier in specifiers {
        let Some(upper) = explicit_upper_bound(&specifier) else {
            continue;
        };
        let stricter = best.as_ref().is_none_or(|(current, _)| {
            upper.version < current.version
                || (upper.version == current.version && !upper.inclusive && current.inclusive)
        });
        if stricter {
            best = Some((upper, specifier));
        }
    }
    best.map(|(_, specifier)| specifier)
}

/// Returns whether `v` satisfies a PEP 440 specifier set.
#[must_use]
pub fn version_in_range(specifier: &str, v: &str) -> bool {
    let (Ok(specifiers), Some(version)) = (VersionSpecifiers::from_str(specifier), parse(v)) else {
        return false;
    };
    specifiers.contains(&version)
}

/// Classifies the step from `current` to `cand` as an [`UpdateKind`].
///
/// A differing epoch or first release segment is [`UpdateKind::Major`]; a
/// differing second segment is [`UpdateKind::Minor`]; anything else is
/// [`UpdateKind::Patch`]. Returns `None` if either version is unparsable.
///
/// # Examples
///
/// ```
/// use cooldown_uv::version::classify_kind;
/// use cooldown_core::UpdateKind;
///
/// assert_eq!(classify_kind("1.2.3", "2.0.0"), Some(UpdateKind::Major));
/// assert_eq!(classify_kind("1.2.3", "1.3.0"), Some(UpdateKind::Minor));
/// assert_eq!(classify_kind("1.2.3", "1.2.4"), Some(UpdateKind::Patch));
/// assert_eq!(classify_kind("1.0", "bad"), None);
/// ```
#[must_use]
pub fn classify_kind(current: &str, cand: &str) -> Option<UpdateKind> {
    let (c, n) = (parse(current)?, parse(cand)?);
    if c.epoch() != n.epoch() || seg(&c, 0) != seg(&n, 0) {
        Some(UpdateKind::Major)
    } else if seg(&c, 1) != seg(&n, 1) {
        Some(UpdateKind::Minor)
    } else {
        Some(UpdateKind::Patch)
    }
}

/// Compares two version strings as a total order over PEP 440 versions.
///
/// Invalid versions sort below all valid ones, and two invalid versions compare
/// equal — so this is safe to pass to `sort_by` over arbitrary input.
///
/// # Examples
///
/// ```
/// use cooldown_uv::version::compare;
/// use std::cmp::Ordering;
///
/// assert_eq!(compare("1.0rc1", "1.0"), Ordering::Less);
/// assert_eq!(compare("1!1.0", "2.0"), Ordering::Greater); // epoch dominates
/// assert_eq!(compare("bad", "1.0"), Ordering::Less);
/// ```
#[must_use]
pub fn compare(a: &str, b: &str) -> Ordering {
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn pep440_ordering() {
        // From the PEP 440 spec, ascending.
        let ordered = [
            "1.0.dev456",
            "1.0a1",
            "1.0b2",
            "1.0rc1",
            "1.0",
            "1.0.post456",
            "1.1",
        ];
        for w in ordered.windows(2) {
            assert_eq!(compare(w[0], w[1]), Less, "{} < {}", w[0], w[1]);
        }
        // Epoch dominates.
        assert_eq!(compare("1!1.0", "2.0"), Greater);
    }

    #[test]
    fn prerelease_and_kinds() {
        assert!(is_prerelease("2.0.0rc1"));
        assert!(!is_prerelease("2.0.0"));
        assert!(!is_prerelease("1.0.post1"));
        assert_eq!(classify_kind("1.2.3", "2.0.0"), Some(UpdateKind::Major));
        assert_eq!(classify_kind("1.2.3", "1.3.0"), Some(UpdateKind::Minor));
        assert_eq!(classify_kind("1.2.3", "1.2.4"), Some(UpdateKind::Patch));
        assert_eq!(classify_kind("1.0", "1!1.0"), Some(UpdateKind::Major)); // epoch bump
    }

    #[test]
    fn major_key_includes_epoch() {
        assert_eq!(major_key("1.2.3"), MajorKey("0!1".into()));
        assert_eq!(major_key("2.0.0"), MajorKey("0!2".into()));
        assert_eq!(major_key("1!1.0"), MajorKey("1!1".into()));
    }

    #[test]
    fn declared_bounds_require_an_explicit_upper_comparator() {
        assert_eq!(
            most_restrictive_declared_bound(["<6".to_string()]),
            Some("<6".to_string())
        );
        assert_eq!(
            most_restrictive_declared_bound([">=5,<6".to_string()]),
            Some(">=5,<6".to_string())
        );
        assert_eq!(most_restrictive_declared_bound(["~=5.9".to_string()]), None);
        assert_eq!(
            most_restrictive_declared_bound(["==5.9.3".to_string()]),
            None
        );
        assert_eq!(
            most_restrictive_declared_bound([">=5,<7".to_string(), ">=5,<6".to_string()]),
            Some(">=5,<6".to_string())
        );
    }

    #[test]
    fn native_specifier_matching_handles_prerelease_bounds() {
        assert!(version_in_range(">=5,<6", "5.9"));
        assert!(!version_in_range(">=5,<6", "6.0rc1"));
    }

    #[test]
    fn numeric_major_ignores_the_pep440_epoch() {
        assert_eq!(major_number("2!5.9.0"), Some(5));
        assert_eq!(classify_kind("5.9.0", "1!5.9.0"), Some(UpdateKind::Major));
        assert_eq!(major_number("not-a-version"), None);
    }
}
