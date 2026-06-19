//! `RubyGems` version semantics. Gem versions are dot-separated segments where any segment may be
//! numeric or a letter run (a letter run marks a prerelease, e.g. `1.0.0.beta1`). The cooldown core
//! treats versions opaquely; this module supplies the ordering, the update kind, and the
//! compatibility-major used to gate `--major`, implementing `RubyGems`' own comparison rules.

use cooldown_core::{MajorKey, UpdateKind};
use std::cmp::Ordering;

/// One comparison segment of a gem version: a numeric run or a letter run.
#[derive(PartialEq, Eq)]
enum Seg {
    Num(u64),
    Str(String),
}

/// Tokenises a gem version into comparison segments. Following `RubyGems`, `-` is treated like `.`,
/// and within a dot-part each maximal run of digits or non-digits becomes its own segment
/// (`beta1` → `beta`, `1`).
fn segments(v: &str) -> Vec<Seg> {
    let mut out = Vec::new();
    for part in v.replace('-', ".").split('.') {
        let mut run = String::new();
        let mut run_is_digit: Option<bool> = None;
        for c in part.chars() {
            let is_digit = c.is_ascii_digit();
            if run_is_digit == Some(is_digit) {
                run.push(c);
            } else {
                push_run(&mut out, &run, run_is_digit);
                run = c.to_string();
                run_is_digit = Some(is_digit);
            }
        }
        push_run(&mut out, &run, run_is_digit);
    }
    out
}

fn push_run(out: &mut Vec<Seg>, run: &str, is_digit: Option<bool>) {
    match is_digit {
        Some(true) => out.push(Seg::Num(run.parse().unwrap_or(0))),
        Some(false) if !run.is_empty() => out.push(Seg::Str(run.to_string())),
        _ => {}
    }
}

/// Returns `true` when `v` is a prerelease — i.e. any segment is a letter run, per `RubyGems`.
///
/// # Examples
///
/// ```
/// use cooldown_rubygems::version::is_prerelease;
///
/// assert!(is_prerelease("1.0.0.beta1"));
/// assert!(!is_prerelease("1.0.0"));
/// ```
#[must_use]
pub fn is_prerelease(v: &str) -> bool {
    segments(v).iter().any(|s| matches!(s, Seg::Str(_)))
}

/// The first numeric segment (the major), and the next numeric segment after it (the minor).
fn major_minor(v: &str) -> (u64, u64) {
    let nums: Vec<u64> = segments(v)
        .into_iter()
        .filter_map(|s| match s {
            Seg::Num(n) => Some(n),
            Seg::Str(_) => None,
        })
        .collect();
    (
        nums.first().copied().unwrap_or(0),
        nums.get(1).copied().unwrap_or(0),
    )
}

/// Returns the compatibility "major" key for `v`, gating `--major`. As with SemVer/cargo, a `0.x`
/// gem treats its minor as the breaking axis, so `0.2 → 0.3` is a major jump.
///
/// # Examples
///
/// ```
/// use cooldown_rubygems::version::major_key;
/// use cooldown_core::MajorKey;
///
/// assert_eq!(major_key("1.2.3"), MajorKey("1".into()));
/// assert_eq!(major_key("0.2.0"), MajorKey("0.2".into()));
/// ```
#[must_use]
pub fn major_key(v: &str) -> MajorKey {
    let (major, minor) = major_minor(v);
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
/// use cooldown_rubygems::version::classify_kind;
/// use cooldown_core::UpdateKind;
///
/// assert_eq!(classify_kind("1.2.3", "2.0.0"), Some(UpdateKind::Major));
/// assert_eq!(classify_kind("1.2.3", "1.2.4"), Some(UpdateKind::Patch));
/// ```
#[must_use]
pub fn classify_kind(current: &str, cand: &str) -> Option<UpdateKind> {
    let (cm, cn) = major_minor(current);
    let (nm, nn) = major_minor(cand);
    if cm != nm {
        Some(UpdateKind::Major)
    } else if cn != nn {
        Some(UpdateKind::Minor)
    } else {
        Some(UpdateKind::Patch)
    }
}

/// Compares two gem versions per `RubyGems`' rules: segment by segment, numeric runs numerically,
/// letter runs lexically, a numeric run outranking a letter run at the same position (a release
/// beats a prerelease), and a missing segment treated as numeric `0`.
///
/// # Examples
///
/// ```
/// use cooldown_rubygems::version::compare;
/// use std::cmp::Ordering;
///
/// assert_eq!(compare("1.2.3", "1.2.4"), Ordering::Less);
/// // A release outranks its own prerelease.
/// assert_eq!(compare("1.0.0", "1.0.0.beta1"), Ordering::Greater);
/// ```
#[must_use]
pub fn compare(left: &str, right: &str) -> Ordering {
    let (lefts, rights) = (segments(left), segments(right));
    let zero = Seg::Num(0);
    for idx in 0..lefts.len().max(rights.len()) {
        let lhs = lefts.get(idx).unwrap_or(&zero);
        let rhs = rights.get(idx).unwrap_or(&zero);
        let ord = match (lhs, rhs) {
            (Seg::Num(ln), Seg::Num(rn)) => ln.cmp(rn),
            (Seg::Str(ls), Seg::Str(rs)) => ls.cmp(rs),
            (Seg::Num(_), Seg::Str(_)) => Ordering::Greater,
            (Seg::Str(_), Seg::Num(_)) => Ordering::Less,
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
    fn ordering_and_prerelease() {
        assert_eq!(compare("1.2.3", "1.2.4"), Less);
        assert_eq!(compare("1.10.0", "1.9.0"), Greater); // numeric, not lexical
        assert_eq!(compare("1.0.0", "1.0.0.beta1"), Greater); // release beats prerelease
        assert_eq!(compare("1.0", "1.0.0"), Equal); // trailing zero padding
        assert_eq!(compare("1.0.0.rc1", "1.0.0.rc2"), Less);
        assert!(is_prerelease("5.0.0.rc1"));
        assert!(!is_prerelease("5.0.0"));
    }

    #[test]
    fn keys_and_kinds() {
        assert_eq!(major_key("1.2.3"), MajorKey("1".into()));
        assert_eq!(major_key("0.2.7"), MajorKey("0.2".into()));
        assert_eq!(classify_kind("1.2.3", "2.0.0"), Some(UpdateKind::Major));
        assert_eq!(classify_kind("1.2.3", "1.3.0"), Some(UpdateKind::Minor));
        assert_eq!(classify_kind("1.2.3", "1.2.4"), Some(UpdateKind::Patch));
    }
}
