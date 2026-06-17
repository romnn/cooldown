//! Cargo version semantics via the `semver` crate (Cargo's own version model). The cooldown core
//! treats versions opaquely; this module supplies the ordering token, the update kind, and the
//! compatibility-major used to gate `--major`.

use cooldown_core::{MajorKey, UpdateKind};
use semver::Version;
use std::cmp::Ordering;

pub fn parse(v: &str) -> Option<Version> {
    Version::parse(v).ok()
}

/// A stable release ⟺ no prerelease segment.
pub fn is_prerelease(v: &str) -> bool {
    parse(v).map(|s| !s.pre.is_empty()).unwrap_or(false)
}

/// Cargo's compatibility "major": `^1.2` is compatible within `1.x`, but `^0.1` is NOT compatible
/// with `0.2` — so for `0.x` the minor acts as the breaking axis. `--major` gates a jump across
/// this key.
pub fn major_key(v: &str) -> MajorKey {
    match parse(v) {
        Some(s) if s.major > 0 => MajorKey(format!("{}", s.major)),
        Some(s) => MajorKey(format!("0.{}", s.minor)),
        None => MajorKey(String::new()),
    }
}

/// Update kind by semver: differing major → Major, differing minor → Minor, else Patch.
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

/// Total order over versions; invalid versions sort below valid ones (and equal to each other).
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
