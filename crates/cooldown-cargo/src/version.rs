//! Cargo version semantics via the `semver` crate (Cargo's own version model). The cooldown core
//! treats versions opaquely; this module supplies the ordering token, the update kind, and the
//! compatibility-major used to gate `--major`.

use cooldown_core::{MajorKey, UpdateKind};
use semver::Version;
use std::cmp::Ordering;

/// Parses `v` as a [`semver::Version`], returning [`None`] when it is not valid semver.
///
/// This is the single parse point for the module; every other function funnels
/// through it so that an unparsable string degrades gracefully (it is treated
/// as "below" any valid version) rather than panicking.
///
/// # Examples
///
/// ```
/// use cooldown_cargo::version::parse;
///
/// assert_eq!(parse("1.2.3").map(|v| v.minor), Some(2));
/// assert!(parse("not-a-version").is_none());
/// ```
#[must_use]
pub fn parse(v: &str) -> Option<Version> {
    Version::parse(v).ok()
}

/// Returns `true` when `v` carries a prerelease segment (e.g. `1.0.0-rc1`).
///
/// A stable release ⟺ no prerelease segment. An unparsable version is treated
/// as not a prerelease.
///
/// # Examples
///
/// ```
/// use cooldown_cargo::version::is_prerelease;
///
/// assert!(is_prerelease("1.0.0-rc1"));
/// assert!(!is_prerelease("1.0.0"));
/// ```
#[must_use]
pub fn is_prerelease(v: &str) -> bool {
    parse(v).is_some_and(|s| !s.pre.is_empty())
}

/// Returns the Cargo compatibility "major" key for `v`, used to gate `--major` jumps.
///
/// Cargo's compatibility "major": `^1.2` is compatible within `1.x`, but `^0.1` is NOT compatible
/// with `0.2` — so for `0.x` the minor acts as the breaking axis. `--major` gates a jump across
/// this key. An unparsable version yields an empty [`MajorKey`].
///
/// # Examples
///
/// ```
/// use cooldown_cargo::version::major_key;
/// use cooldown_core::MajorKey;
///
/// assert_eq!(major_key("1.2.3"), MajorKey("1".into()));
/// // For 0.x the minor is the breaking axis.
/// assert_eq!(major_key("0.2.0"), MajorKey("0.2".into()));
/// ```
#[must_use]
pub fn major_key(v: &str) -> MajorKey {
    match parse(v) {
        Some(s) if s.major > 0 => MajorKey(format!("{}", s.major)),
        Some(s) => MajorKey(format!("0.{}", s.minor)),
        None => MajorKey(String::new()),
    }
}

/// Classifies the [`UpdateKind`] of moving from `current` to `cand` by semver axis.
///
/// Update kind by semver: differing major → [`UpdateKind::Major`], differing minor →
/// [`UpdateKind::Minor`], else [`UpdateKind::Patch`]. Returns [`None`] when either version
/// is unparsable.
///
/// # Examples
///
/// ```
/// use cooldown_cargo::version::classify_kind;
/// use cooldown_core::UpdateKind;
///
/// assert_eq!(classify_kind("1.2.3", "2.0.0"), Some(UpdateKind::Major));
/// assert_eq!(classify_kind("1.2.3", "1.2.4"), Some(UpdateKind::Patch));
/// ```
#[must_use]
pub fn classify_kind(current: &str, cand: &str) -> Option<UpdateKind> {
    let (c, n) = (parse(current)?, parse(cand)?);
    if c.major != n.major {
        Some(UpdateKind::Major)
    } else if c.minor != n.minor {
        Some(UpdateKind::Minor)
    } else {
        Some(UpdateKind::Patch)
    }
}

/// Compares two version strings, yielding a total [`Ordering`].
///
/// Total order over versions; invalid versions sort below valid ones (and equal to each other).
/// This lets [`build_releases`](crate::tool::build_releases) sort a mixed list without
/// discarding unparsable entries up front.
///
/// # Examples
///
/// ```
/// use cooldown_cargo::version::compare;
/// use std::cmp::Ordering;
///
/// assert_eq!(compare("1.2.3", "1.2.4"), Ordering::Less);
/// // A release outranks its own prerelease.
/// assert_eq!(compare("1.0.0", "1.0.0-rc1"), Ordering::Greater);
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
    fn ordering_and_prerelease() {
        assert_eq!(compare("1.2.3", "1.2.4"), Less);
        assert_eq!(compare("1.0.0", "1.0.0-rc1"), Greater); // release outranks prerelease
        assert!(is_prerelease("1.0.0-rc1"));
        assert!(!is_prerelease("1.0.0"));
    }

    #[test]
    fn compatibility_major_key() {
        assert_eq!(major_key("1.2.3"), MajorKey("1".into()));
        assert_eq!(major_key("2.0.0"), MajorKey("2".into()));
        // 0.x: the minor is the breaking axis.
        assert_eq!(major_key("0.1.5"), MajorKey("0.1".into()));
        assert_eq!(major_key("0.2.0"), MajorKey("0.2".into()));
    }

    #[test]
    fn kinds() {
        assert_eq!(classify_kind("1.2.3", "2.0.0"), Some(UpdateKind::Major));
        assert_eq!(classify_kind("1.2.3", "1.3.0"), Some(UpdateKind::Minor));
        assert_eq!(classify_kind("1.2.3", "1.2.4"), Some(UpdateKind::Patch));
    }
}
