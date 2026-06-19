//! Hex version semantics via the `semver` crate. Hex requires strict `SemVer` 2.0, so the model is
//! the same as npm/cargo: this module supplies the ordering, the update kind, and the
//! compatibility-major used to gate `--major`.

use cooldown_core::{MajorKey, UpdateKind};
use semver::Version;
use std::cmp::Ordering;

/// Parses `v` as a [`semver::Version`], returning [`None`] when it is not valid `SemVer`.
///
/// # Examples
///
/// ```
/// use cooldown_hex::version::parse;
/// assert_eq!(parse("1.2.3").map(|v| v.minor), Some(2));
/// assert!(parse("not-a-version").is_none());
/// ```
#[must_use]
pub fn parse(v: &str) -> Option<Version> {
    Version::parse(v).ok()
}

/// Returns `true` when `v` carries a prerelease segment (e.g. `1.0.0-rc.1`).
///
/// # Examples
///
/// ```
/// use cooldown_hex::version::is_prerelease;
/// assert!(is_prerelease("1.0.0-rc.1"));
/// assert!(!is_prerelease("1.0.0"));
/// ```
#[must_use]
pub fn is_prerelease(v: &str) -> bool {
    parse(v).is_some_and(|s| !s.pre.is_empty())
}

/// Returns the compatibility "major" key for `v`, gating `--major`. As in `SemVer`, a `0.x`
/// release treats its minor as the breaking axis.
///
/// # Examples
///
/// ```
/// use cooldown_hex::version::major_key;
/// use cooldown_core::MajorKey;
/// assert_eq!(major_key("1.2.3"), MajorKey("1".into()));
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

/// Classifies the [`UpdateKind`] of moving from `current` to `cand` by `SemVer` axis.
///
/// # Examples
///
/// ```
/// use cooldown_hex::version::classify_kind;
/// use cooldown_core::UpdateKind;
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

/// Compares two version strings, yielding a total [`Ordering`]; invalid versions sort below valid
/// ones (and equal to each other).
///
/// # Examples
///
/// ```
/// use cooldown_hex::version::compare;
/// use std::cmp::Ordering;
/// assert_eq!(compare("1.2.3", "1.2.4"), Ordering::Less);
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
    fn semver_behaviour() {
        assert_eq!(compare("1.2.3", "1.2.4"), Less);
        assert_eq!(compare("1.0.0", "1.0.0-rc.1"), Greater);
        assert_eq!(major_key("0.2.0"), MajorKey("0.2".into()));
        assert_eq!(classify_kind("1.2.3", "2.0.0"), Some(UpdateKind::Major));
        assert!(is_prerelease("1.0.0-rc.1"));
    }
}
