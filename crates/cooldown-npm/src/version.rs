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

/// Whether `target` satisfies the declared `package.json` range `spec` under **npm's** range
/// semantics — used to decide whether a lock-only move (pnpm `update --no-save`) stays inside the
/// author's constraint or needs a manifest rewrite instead.
///
/// npm and the Rust `semver` parser disagree on bare tokens: npm reads a bare full version as an
/// *equality* (`1.2.3` ⇔ `=1.2.3`) and a bare partial as an *X-range* (`1.2` ⇔ `1.2.x`), while the
/// Rust parser would assume an implicit caret for both. [`normalized_requirement`] translates each
/// clause accordingly before parsing; operator-led partials need no translation (the parser's
/// `>I.J`-style desugarings already match node-semver's X-range rules).
///
/// Conservative by design: whitespace-separated comparator clauses are normalized into the
/// parser's comma-separated `AND` form, while ranges it still cannot represent (npm hyphen ranges,
/// `||` unions, `workspace:`/`catalog:` protocols) return `false`. The caller then falls back to
/// rewriting the manifest — always correct, just less minimal than a lock-only move would have
/// been.
///
/// # Examples
///
/// ```
/// use cooldown_npm::version::version_in_range;
///
/// assert!(version_in_range("^3.0.0", "3.4.1"));
/// assert!(!version_in_range("^3.0.0", "4.0.0"));
/// // npm token semantics: a bare version is an equality, a bare partial an X-range.
/// assert!(!version_in_range("3.0.0", "3.4.1"));
/// assert!(version_in_range("3.4", "3.4.1"));
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

/// How a declared npm range relates to one target version, for decisions that must distinguish
/// "provably excluded" from "not understood".
///
/// A boolean matcher collapses both non-matches into `false`, which is safe only for positive
/// queries; a caller that *negates* it (the peer-feasibility gate proving a target excluded) would
/// turn every range form the parser cannot represent into a false incompatibility. The tri-state
/// keeps proof and ignorance apart: [`Excludes`](RangeMatch::Excludes) is only reported when every
/// `||` branch was understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeMatch {
    /// An understood `||` branch admits the target.
    Matches,
    /// Every `||` branch is understood and none admits the target — exclusion is proven.
    Excludes,
    /// No understood branch admits the target, but some branch (or the target itself) is not
    /// representable — nothing is proven either way.
    Unknown,
}

/// Judges `target` against `spec` with npm `semver.satisfies` union semantics: each `||` branch is
/// an AND comparator set judged like [`version_in_range`], and the verdict is the tri-state
/// [`RangeMatch`].
///
/// This is the matcher for *declared peer ranges*, which routinely union majors
/// (`eslint: ^7.0.0 || ^8.0.0`); [`version_in_range`] deliberately stays union-free because its
/// callers must fall back to a manifest rewrite for anything they cannot represent.
///
/// # Examples
///
/// ```
/// use cooldown_npm::version::{RangeMatch, range_match};
///
/// assert_eq!(range_match("^7.0.0 || ^8.0.0", "8.57.0"), RangeMatch::Matches);
/// assert_eq!(range_match("^7.0.0 || ^8.0.0", "9.8.0"), RangeMatch::Excludes);
/// // An npm hyphen-range branch cannot be represented, so exclusion is never proven.
/// assert_eq!(
///     range_match("^16.0.0 || 17.0.0 - 17.4.0", "18.0.0"),
///     RangeMatch::Unknown
/// );
/// ```
#[must_use]
pub fn range_match(spec: &str, target: &str) -> RangeMatch {
    let Some(target) = parse(target) else {
        return RangeMatch::Unknown;
    };
    let mut every_branch_understood = true;
    for branch in spec.split("||") {
        match branch_admits(branch, &target) {
            Some(true) => return RangeMatch::Matches,
            Some(false) => {}
            None => every_branch_understood = false,
        }
    }
    if every_branch_understood {
        RangeMatch::Excludes
    } else {
        RangeMatch::Unknown
    }
}

/// Whether one `||`-free branch admits `target`, or [`None`] when the branch is not representable
/// (hyphen ranges, protocols, an empty branch — npm's "match anything" spelling).
fn branch_admits(branch: &str, target: &Version) -> Option<bool> {
    let normalized = normalized_requirement(branch)?;
    let requirement = semver::VersionReq::parse(&normalized).ok()?;
    Some(requirement.matches(target))
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
            clauses.push(npm_clause(token));
        }
    }
    Some(clauses.join(", "))
}

/// Translates one range clause into the Rust parser's syntax with **npm's** token semantics: a
/// BARE version is an equality in npm (`1.2.3` ⇔ `=1.2.3`; a bare partial `1.2` ⇔ the X-range
/// `1.2.x`, which `=1.2` desugars to identically), never the implicit caret the Rust parser would
/// assume. Operator-led clauses and explicit X-ranges pass through unchanged — their parser
/// desugarings already match node-semver's. A clause neither form can represent simply fails the
/// downstream parse, keeping the callers' conservative fallbacks.
fn npm_clause(clause: &str) -> String {
    if clause.starts_with(['<', '>', '=', '~', '^']) || is_x_range(clause) {
        clause.to_string()
    } else {
        format!("={clause}")
    }
}

/// Whether `clause` is an npm X-range (`*`, `1.x`, `1.2.X`): a wildcard *version component* before
/// any prerelease/build suffix — so `1.0.0-x.7` (a prerelease tag that merely contains `x`) is not
/// one.
pub(crate) fn is_x_range(clause: &str) -> bool {
    clause
        .split(['-', '+'])
        .next()
        .unwrap_or(clause)
        .split('.')
        .any(|part| matches!(part, "x" | "X" | "*"))
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

/// Whether an npm/pnpm specifier is an exact pin: a bare version (`2.11.0`, `1.0.0-rc.1`) or single
/// equals range (`=2.11.0`) with no range operator, wildcard, or union. A pinned dependency cannot
/// move without editing the manifest.
pub(crate) fn is_exact_npm_specifier(specifier: &str) -> bool {
    let specifier = specifier.trim();
    let specifier = specifier
        .strip_prefix('=')
        .filter(|version| !version.starts_with('='))
        .map_or(specifier, str::trim);
    Version::parse(specifier).is_ok()
}

/// Why `--major` cannot carry a declared range to a target the range excludes.
///
/// `--major` authorizes one rewrite: lifting the implicit ceiling of a plain caret or tilde range
/// across a major line, the same widen it performs for a lone declaration.
/// Every other way a range can exclude the target needs `--rewrite`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CeilingHold {
    /// The range is an exact version, which no implicit ceiling bounds.
    ExactPin,
    /// The range is not a plain `^` or `~` range: a union, hyphen range, comparator set, wildcard,
    /// explicit bound, or a form cooldown cannot read.
    NotPlain,
    /// A plain range excludes the target on its own major line, as `~7.3.0` excludes `7.4.0`.
    WithinLine,
    /// A plain range excludes the target by its floor, which sits above the target.
    BelowFloor,
}

/// Judges whether `--major` may widen `specifier` to `target`, given that it excludes `target`.
///
/// Returns [`None`] when `specifier` is a plain caret or tilde range whose base lies on a lower
/// major line than `target` (by [`major_key`], so a `0.x` minor counts as the major), and the
/// reason it is held otherwise.
///
/// # Examples
///
/// ```
/// use cooldown_npm::version::{CeilingHold, ceiling_hold};
///
/// assert_eq!(ceiling_hold("^20.9.0", "21.0.4"), None);
/// assert_eq!(ceiling_hold("~20.11.1", "21.0.4"), None);
/// assert_eq!(ceiling_hold("^0.3.1", "0.4.0"), None);
/// assert_eq!(ceiling_hold("~7.3.0", "7.4.0"), Some(CeilingHold::WithinLine));
/// assert_eq!(ceiling_hold("^21.0.0", "20.14.6"), Some(CeilingHold::BelowFloor));
/// assert_eq!(ceiling_hold("20.14.5", "21.0.4"), Some(CeilingHold::ExactPin));
/// assert_eq!(ceiling_hold(">=20 <21", "21.0.4"), Some(CeilingHold::NotPlain));
/// ```
#[must_use]
pub fn ceiling_hold(specifier: &str, target: &str) -> Option<CeilingHold> {
    if is_exact_npm_specifier(specifier) {
        return Some(CeilingHold::ExactPin);
    }
    let (Some(base), Some(target)) = (plain_caret_or_tilde_base(specifier), parse(target)) else {
        return Some(CeilingHold::NotPlain);
    };
    if target <= base {
        Some(CeilingHold::BelowFloor)
    } else if major_key(&target.to_string()) == major_key(&base.to_string()) {
        Some(CeilingHold::WithinLine)
    } else {
        None
    }
}

/// The base version of a plain caret or tilde range, with a numeric partial (`^20`, `~20.9`)
/// padded with zeros.
///
/// A union, comparator, wildcard component, spacing, or `v` prefix makes the range not plain.
fn plain_caret_or_tilde_base(specifier: &str) -> Option<Version> {
    let specifier = specifier.trim();
    let rest = specifier
        .strip_prefix('^')
        .or_else(|| specifier.strip_prefix('~'))?;
    if let Ok(version) = Version::parse(rest) {
        return Some(version);
    }
    let mut parts = rest.split('.');
    let major = numeric_component(parts.next()?)?;
    let minor = parts.next().map_or(Some(0), numeric_component)?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version::new(major, minor, 0))
}

fn numeric_component(part: &str) -> Option<u64> {
    if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
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
/// key. `0.0.x` deliberately maps to the single `0.0` line even though the caret pins each `0.0.z`
/// exactly (`^0.0.3` ⇔ `=0.0.3`): gating every `0.0.x` step behind `--major` would make such a
/// package unmovable in practice, and the same convention holds across the other adapters
/// (cargo's `0.0` line is keyed identically). The peer-feasibility gate refines `0.0.x` steps
/// itself where a provable peer violation is at stake. An unparsable version yields an empty
/// [`MajorKey`].
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
    fn ceiling_hold_widens_only_a_plain_ceiling_crossed_by_a_major() {
        // Numeric partials are plain ranges padded with zeros.
        assert_eq!(ceiling_hold("^20", "21.0.0"), None);
        assert_eq!(ceiling_hold("~20.9", "21.0.0"), None);
        assert_eq!(ceiling_hold("~20.0.0-next.1", "21.0.0"), None);
        assert_eq!(ceiling_hold("^0.3", "0.4.0"), None);
        // The `0.0` line is one major key, so a step within it is no major crossing.
        assert_eq!(
            ceiling_hold("^0.0.3", "0.0.4"),
            Some(CeilingHold::WithinLine)
        );
        // A caret excludes an in-major prerelease of a later minor only within its line.
        assert_eq!(
            ceiling_hold("^20.0.0", "20.1.0-rc.1"),
            Some(CeilingHold::WithinLine)
        );
        assert_eq!(
            ceiling_hold("=20.14.5", "21.0.4"),
            Some(CeilingHold::ExactPin)
        );
        for not_plain in [
            "^20.x",
            "~>1.2",
            "^v20.0.0",
            "^ 20.0.0",
            "^20.0.0 || ^21.0.0",
            "20.x",
            "<21",
            "latest",
        ] {
            assert_eq!(
                ceiling_hold(not_plain, "21.0.4"),
                Some(CeilingHold::NotPlain),
                "{not_plain}"
            );
        }
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
    fn npm_bare_versions_are_equalities_and_partials_are_x_ranges() {
        // npm reads a bare full version as an equality — never Rust's implicit caret.
        assert!(version_in_range("1.2.3", "1.2.3"));
        assert!(!version_in_range("1.2.3", "1.2.4"));
        assert_eq!(
            range_match("0.0.3-alpha", "0.0.3-alpha"),
            RangeMatch::Matches
        );
        // A bare prerelease is exact too: the stable 0.0.3 is OUTSIDE it.
        assert_eq!(range_match("0.0.3-alpha", "0.0.3"), RangeMatch::Excludes);
        // A bare partial is an X-range (`1.2` ⇔ `1.2.x`), not `^1.2`.
        assert!(version_in_range("1.2", "1.2.5"));
        assert!(!version_in_range("1.2", "1.9.0"));
        assert_eq!(range_match("1.2", "1.9.0"), RangeMatch::Excludes);
        assert!(version_in_range("1", "1.9.0"));
        assert!(!version_in_range("1", "2.0.0"));
        // Explicit X-ranges and operator-led partials keep their (already npm-matching) meaning.
        assert!(version_in_range("1.2.x", "1.2.5"));
        assert!(!version_in_range("1.2.x", "1.3.0"));
        assert!(version_in_range("*", "3.0.0"));
        assert!(version_in_range(">1.2", "1.3.0"));
        // A dist-tag used as a range is not representable and never matches (nor proves exclusion).
        assert!(!version_in_range("latest", "1.0.0"));
        assert_eq!(range_match("latest", "1.0.0"), RangeMatch::Unknown);
    }

    #[test]
    fn range_match_distinguishes_proof_from_ignorance() {
        use RangeMatch::{Excludes, Matches, Unknown};
        assert_eq!(range_match("^7.0.0 || ^8.0.0", "8.57.0"), Matches);
        assert_eq!(range_match("^7.0.0 || ^8.0.0", "9.8.0"), Excludes);
        // x-wildcard branches are representable, so the union is judged in full.
        assert_eq!(range_match("^16.0.0 || 17.x", "17.2.0"), Matches);
        assert_eq!(range_match("^16.0.0 || 17.x", "18.0.0"), Excludes);
        // A hyphen-range branch is not representable: exclusion is never proven through it.
        assert_eq!(range_match("^16.0.0 || 17.0.0 - 17.4.0", "18.0.0"), Unknown);
        assert_eq!(range_match("16.11.4 - 17.0.0", "18.0.0"), Unknown);
        // npm's empty range means "anything"; it is unrepresentable here and proves nothing.
        assert_eq!(range_match("", "1.0.0"), Unknown);
        assert_eq!(range_match("workspace:*", "1.0.0"), Unknown);
        // An unparsable target proves nothing either.
        assert_eq!(range_match("^1.0.0", "not-a-version"), Unknown);
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
