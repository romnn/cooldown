//! Maven version semantics, approximating Maven's `ComparableVersion`. A version is a sequence of
//! numeric and qualifier tokens split on `.`, `-`, `_`, and digit/letter boundaries; qualifiers
//! have a known ordering (`alpha < beta < milestone < rc < snapshot < release < sp`), and a release
//! (no prerelease qualifier) outranks any prerelease.
//!
//! This is a pragmatic subset of Maven's full algorithm — enough to order the common
//! `X.Y.Z[-qualifier]` shapes correctly — not a bit-exact reimplementation of every edge case.

use cooldown_core::{MajorKey, UpdateKind};
use std::cmp::Ordering;

#[derive(PartialEq, Eq)]
enum Tok {
    Num(u64),
    Str(String),
}

/// Tokenises a Maven version: lowercased, split on `.`/`-`/`_` and on digit↔letter boundaries.
fn tokens(v: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut run = String::new();
    let mut run_digit: Option<bool> = None;
    let flush = |out: &mut Vec<Tok>, run: &mut String, digit: &mut Option<bool>| {
        match *digit {
            Some(true) => out.push(Tok::Num(run.parse().unwrap_or(0))),
            Some(false) if !run.is_empty() => out.push(Tok::Str(std::mem::take(run))),
            _ => {}
        }
        run.clear();
        *digit = None;
    };
    for c in v.to_lowercase().chars() {
        if matches!(c, '.' | '-' | '_') {
            flush(&mut out, &mut run, &mut run_digit);
            continue;
        }
        let is_digit = c.is_ascii_digit();
        if run_digit == Some(is_digit) {
            run.push(c);
        } else {
            flush(&mut out, &mut run, &mut run_digit);
            run.push(c);
            run_digit = Some(is_digit);
        }
    }
    flush(&mut out, &mut run, &mut run_digit);
    out
}

/// The ordering rank of a qualifier relative to a final release (rank `0`). Prereleases are
/// negative; post-release patches (`sp`) are positive. Unknown qualifiers rank as release-level and
/// fall back to lexical comparison.
fn qualifier_rank(s: &str) -> i32 {
    match s {
        "alpha" | "a" => -5,
        "beta" | "b" => -4,
        "milestone" | "m" => -3,
        "rc" | "cr" => -2,
        "snapshot" => -1,
        "sp" => 1,
        _ => 0,
    }
}

/// Returns `true` when `v` carries a prerelease qualifier (`alpha`/`beta`/`milestone`/`rc`/
/// `snapshot`).
///
/// # Examples
///
/// ```
/// use cooldown_maven::version::is_prerelease;
/// assert!(is_prerelease("2.0.0-rc1"));
/// assert!(!is_prerelease("2.0.0"));
/// ```
#[must_use]
pub fn is_prerelease(v: &str) -> bool {
    tokens(v)
        .iter()
        .any(|t| matches!(t, Tok::Str(s) if qualifier_rank(s) < 0))
}

fn first_two_numbers(v: &str) -> (u64, u64) {
    let nums: Vec<u64> = tokens(v)
        .into_iter()
        .filter_map(|t| match t {
            Tok::Num(n) => Some(n),
            Tok::Str(_) => None,
        })
        .collect();
    (
        nums.first().copied().unwrap_or(0),
        nums.get(1).copied().unwrap_or(0),
    )
}

/// Returns the compatibility "major" key for `v`, gating `--major`. A `0.x` version treats its
/// minor as the breaking axis.
///
/// # Examples
///
/// ```
/// use cooldown_maven::version::major_key;
/// use cooldown_core::MajorKey;
/// assert_eq!(major_key("33.4.8"), MajorKey("33".into()));
/// assert_eq!(major_key("0.9.1"), MajorKey("0.9".into()));
/// ```
#[must_use]
pub fn major_key(v: &str) -> MajorKey {
    let (major, minor) = first_two_numbers(v);
    if major > 0 {
        MajorKey(major.to_string())
    } else {
        MajorKey(format!("0.{minor}"))
    }
}

/// Classifies the [`UpdateKind`] of moving from `current` to `cand` by the first two numeric axes.
///
/// # Examples
///
/// ```
/// use cooldown_maven::version::classify_kind;
/// use cooldown_core::UpdateKind;
/// assert_eq!(classify_kind("1.2.3", "2.0.0"), Some(UpdateKind::Major));
/// assert_eq!(classify_kind("1.2.3", "1.2.4"), Some(UpdateKind::Patch));
/// ```
#[must_use]
pub fn classify_kind(current: &str, cand: &str) -> Option<UpdateKind> {
    let (cm, cn) = first_two_numbers(current);
    let (nm, nn) = first_two_numbers(cand);
    if cm != nm {
        Some(UpdateKind::Major)
    } else if cn != nn {
        Some(UpdateKind::Minor)
    } else {
        Some(UpdateKind::Patch)
    }
}

/// Compares two Maven versions, approximating `ComparableVersion`: numeric tokens numerically,
/// qualifier tokens by rank then lexically, a numeric token outranking a prerelease qualifier (and
/// below a post-release one), and a missing token padded with numeric `0`.
///
/// # Examples
///
/// ```
/// use cooldown_maven::version::compare;
/// use std::cmp::Ordering;
/// assert_eq!(compare("1.7.30", "2.0.0"), Ordering::Less);
/// // A release outranks its own release candidate.
/// assert_eq!(compare("2.0.0", "2.0.0-rc1"), Ordering::Greater);
/// ```
#[must_use]
pub fn compare(left: &str, right: &str) -> Ordering {
    let (lefts, rights) = (tokens(left), tokens(right));
    let zero = Tok::Num(0);
    for idx in 0..lefts.len().max(rights.len()) {
        let lhs = lefts.get(idx).unwrap_or(&zero);
        let rhs = rights.get(idx).unwrap_or(&zero);
        let ord = match (lhs, rhs) {
            (Tok::Num(ln), Tok::Num(rn)) => ln.cmp(rn),
            (Tok::Str(ls), Tok::Str(rs)) => qualifier_rank(ls)
                .cmp(&qualifier_rank(rs))
                .then_with(|| ls.cmp(rs)),
            // A numeric token beats a prerelease qualifier and is below a post-release one.
            (Tok::Num(_), Tok::Str(qual)) => 0.cmp(&qualifier_rank(qual)).then(Ordering::Greater),
            (Tok::Str(qual), Tok::Num(_)) => qualifier_rank(qual).cmp(&0).then(Ordering::Less),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn numeric_and_qualifier_ordering() {
        assert_eq!(compare("1.7.30", "2.0.0"), Less);
        assert_eq!(compare("2.10.0", "2.9.0"), Greater); // numeric, not lexical
        assert_eq!(compare("2.0.0", "2.0.0-rc1"), Greater); // release beats rc
        assert_eq!(compare("2.0.0-alpha1", "2.0.0-beta1"), Less);
        assert_eq!(compare("1.0", "1.0.0"), Equal);
        assert_eq!(compare("1.0", "1.0-sp1"), Less); // sp is a post-release patch
        assert!(is_prerelease("2.0.0-SNAPSHOT"));
        assert!(!is_prerelease("2.0.0"));
    }

    #[test]
    fn keys_and_kinds() {
        assert_eq!(major_key("33.4.8"), MajorKey("33".into()));
        assert_eq!(classify_kind("2.12.0", "2.18.0"), Some(UpdateKind::Minor));
        assert_eq!(classify_kind("1.7.30", "2.0.0"), Some(UpdateKind::Major));
    }
}
