//! PEP 440 version semantics via `pep440_rs` (the implementation uv itself uses). PEP 440 is *not*
//! semver — epochs, release segments, and pre/post/dev suffixes order differently — so the core
//! relies on this module for the opaque ordering token, the update kind, and the major key.

use cooldown_core::{MajorKey, UpdateKind};
use pep440_rs::Version;
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
}
