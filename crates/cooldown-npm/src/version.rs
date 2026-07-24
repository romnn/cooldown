//! npm version semantics via the `semver` crate (npm's own version model is `SemVer` 2.0). The
//! cooldown core treats versions opaquely; this module supplies the ordering token, the update
//! kind, and the compatibility-major used to gate `--major`.

use cooldown_core::{MajorKey, UpdateKind};
use semver::Version;
use std::cmp::Ordering;

/// Parses `v` as a [`semver::Version`], returning [`None`] when it is not valid `SemVer`.
///
/// This is the single parse point for the module; every other function funnels through it so that
/// an unparsable string degrades gracefully (it is treated as "below" any valid version) rather
/// than panicking.
///
/// # Examples
///
/// ```
/// use cooldown_npm::version::parse;
///
/// assert_eq!(parse("1.2.3").map(|v| v.minor), Some(2));
/// assert!(parse("not-a-version").is_none());
/// ```
#[must_use]
pub fn parse(v: &str) -> Option<Version> {
    Version::parse(v).ok()
}

/// Whether `target` satisfies the declared `package.json` range `spec` — used to decide whether a
/// lock-only move (pnpm `update --no-save`) stays inside the author's constraint or needs a manifest
/// rewrite instead.
///
/// Conservative by design: whitespace-separated comparator clauses are normalized into the
/// parser's comma-separated `AND` form, while ranges it still cannot represent (npm hyphen ranges,
/// `||` unions, `x`/`X` wildcards, `workspace:`/`catalog:` protocols) return `false`. The caller
/// then falls back to rewriting the manifest — always correct, just less minimal than a lock-only
/// move would have been.
///
/// # Examples
///
/// ```
/// use cooldown_npm::version::version_in_range;
///
/// assert!(version_in_range("^3.0.0", "3.4.1"));
/// assert!(!version_in_range("^3.0.0", "4.0.0"));
/// assert!(!version_in_range("workspace:*", "3.4.1")); // unrepresentable → rewrite
/// ```
#[must_use]
pub fn version_in_range(spec: &str, target: &str) -> bool {
    let Some(spec) = normalized_requirement(spec) else {
        return false;
    };
    let (Ok(requirement), Ok(version)) = (semver::VersionReq::parse(&spec), Version::parse(target))
    else {
        return false;
    };
    requirement.matches(&version)
}

fn normalized_requirement(spec: &str) -> Option<String> {
    let spec = spec.trim();
    if spec.contains("||") || spec.contains(" - ") {
        return None;
    }
    let mut tokens = spec.split_whitespace().peekable();
    let mut clauses = Vec::new();
    while let Some(token) = tokens.next() {
        if matches!(token, "<" | "<=" | ">" | ">=" | "=" | "~" | "^")
            && let Some(version) = tokens.next()
        {
            clauses.push(format!("{token}{version}"));
        } else {
            clauses.push(token.to_string());
        }
    }
    Some(clauses.join(", "))
}

#[derive(Clone)]
struct UpperBound {
    version: Version,
    inclusive: bool,
}

fn explicit_upper_bound(spec: &str) -> Option<UpperBound> {
    let normalized = normalized_requirement(spec)?;
    let parsed = semver::VersionReq::parse(&normalized).ok()?;
    parsed
        .comparators
        .iter()
        .filter_map(|comparator| {
            let inclusive = match comparator.op {
                semver::Op::Less => false,
                semver::Op::LessEq => true,
                _ => return None,
            };
            let mut version = Version::new(
                comparator.major,
                comparator.minor.unwrap_or(0),
                comparator.patch.unwrap_or(0),
            );
            version.pre = comparator.pre.clone();
            Some(UpperBound { version, inclusive })
        })
        .min_by(|a, b| {
            a.version
                .cmp(&b.version)
                .then_with(|| a.inclusive.cmp(&b.inclusive))
        })
}

/// Chooses the most restrictive explicit upper-bound range from `ranges`.
#[must_use]
pub fn most_restrictive_declared_bound(ranges: impl IntoIterator<Item = String>) -> Option<String> {
    let mut best: Option<(UpperBound, String)> = None;
    for range in ranges {
        let Some(upper) = explicit_upper_bound(&range) else {
            continue;
        };
        let stricter = best.as_ref().is_none_or(|(current, _)| {
            upper.version < current.version
                || (upper.version == current.version && !upper.inclusive && current.inclusive)
        });
        if stricter {
            best = Some((upper, range));
        }
    }
    best.map(|(_, range)| range)
}

/// Returns `true` when `v` carries a prerelease segment (e.g. `1.0.0-rc.1`).
///
/// A stable release ⟺ no prerelease segment. An unparsable version is treated as not a prerelease.
///
/// # Examples
///
/// ```
/// use cooldown_npm::version::is_prerelease;
///
/// assert!(is_prerelease("1.0.0-rc.1"));
/// assert!(!is_prerelease("1.0.0"));
/// ```
#[must_use]
pub fn is_prerelease(v: &str) -> bool {
    parse(v).is_some_and(|s| !s.pre.is_empty())
}

/// Returns the npm compatibility "major" key for `v`, used to gate `--major` jumps.
///
/// npm's caret semantics: `^1.2` is compatible within `1.x`, but `^0.1` is NOT compatible with
/// `0.2` — so for `0.x` the minor acts as the breaking axis. `--major` gates a jump across this
/// key. An unparsable version yields an empty [`MajorKey`].
///
/// # Examples
///
/// ```
/// use cooldown_npm::version::major_key;
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

/// Returns the numeric `SemVer` major ordinal for `v`.
#[must_use]
pub fn major_number(v: &str) -> Option<u64> {
    parse(v).map(|version| version.major)
}

/// Classifies the [`UpdateKind`] of moving from `current` to `cand` by `SemVer` axis.
///
/// Differing major → [`UpdateKind::Major`], differing minor → [`UpdateKind::Minor`], else
/// [`UpdateKind::Patch`]. Returns [`None`] when either version is unparsable.
///
/// # Examples
///
/// ```
/// use cooldown_npm::version::classify_kind;
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
/// This lets the release builder sort a mixed list without discarding unparsable entries up front.
///
/// # Examples
///
/// ```
/// use cooldown_npm::version::compare;
/// use std::cmp::Ordering;
///
/// assert_eq!(compare("1.2.3", "1.2.4"), Ordering::Less);
/// // A release outranks its own prerelease.
/// assert_eq!(compare("1.0.0", "1.0.0-rc.1"), Ordering::Greater);
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
        assert_eq!(compare("1.0.0", "1.0.0-rc.1"), Greater); // release outranks prerelease
        assert!(is_prerelease("1.0.0-rc.1"));
        assert!(!is_prerelease("1.0.0"));
    }

    #[test]
    fn compatibility_major_key() {
        assert_eq!(major_key("1.2.3"), MajorKey("1".into()));
        assert_eq!(major_key("2.0.0"), MajorKey("2".into()));
        // For 0.x the minor is the breaking axis (npm caret semantics).
        assert_eq!(major_key("0.2.0"), MajorKey("0.2".into()));
        assert_eq!(major_key("0.2.7"), MajorKey("0.2".into()));
    }

    #[test]
    fn update_kind_by_axis() {
        assert_eq!(classify_kind("1.2.3", "2.0.0"), Some(UpdateKind::Major));
        assert_eq!(classify_kind("1.2.3", "1.3.0"), Some(UpdateKind::Minor));
        assert_eq!(classify_kind("1.2.3", "1.2.4"), Some(UpdateKind::Patch));
        assert_eq!(classify_kind("bad", "1.2.4"), None);
    }

    #[test]
    fn declared_bounds_require_a_simple_explicit_upper_comparator() {
        assert_eq!(
            most_restrictive_declared_bound([">=5 <6".to_string()]),
            Some(">=5 <6".to_string())
        );
        assert_eq!(
            most_restrictive_declared_bound([">= 5 < 6".to_string()]),
            Some(">= 5 < 6".to_string())
        );
        assert_eq!(most_restrictive_declared_bound(["^5".to_string()]), None);
        assert_eq!(
            most_restrictive_declared_bound(["^5 || ^6".to_string()]),
            None
        );
        assert_eq!(
            most_restrictive_declared_bound([">=5 <7".to_string(), ">=5 <6".to_string(),]),
            Some(">=5 <6".to_string())
        );
    }

    #[test]
    fn native_range_matching_handles_prerelease_bounds() {
        assert!(version_in_range(">=5 <6", "5.9.0"));
        assert!(version_in_range(">= 5 < 6", "5.9.0"));
        assert!(!version_in_range(">=5 <6", "6.0.0-rc.1"));
    }

    #[test]
    fn numeric_major_uses_the_semver_major() {
        assert_eq!(major_number("0.9.0"), Some(0));
        assert_eq!(major_number("12.0.0-rc.1"), Some(12));
        assert_eq!(major_number("not-a-version"), None);
    }
}
