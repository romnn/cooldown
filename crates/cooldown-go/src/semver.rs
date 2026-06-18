//! A faithful Rust port of the `golang.org/x/mod` semantics this adapter needs: `semver.Compare`,
//! `Canonical`, `Major`/`Prerelease`/`Build`, `module.IsPseudoVersion` (the *exact* x/mod regex),
//! `PseudoVersionTime`, and `module.SplitPathVersion`. Versions are never compared as lexicographic
//! strings; publish times are typed instants.

use jiff::Timestamp;
use jiff::civil;
use jiff::tz::TimeZone;
use regex::Regex;
use std::cmp::Ordering;
use std::sync::OnceLock;

/// The source for the exact `golang.org/x/mod/module` pseudo-version regex.
const PSEUDO_RE_SRC: &str = r"^v[0-9]+\.(0\.0-|\d+\.\d+-([^+]*\.)?0\.)\d{14}-[A-Za-z0-9]+(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$";

/// Returns the compiled, cached `golang.org/x/mod/module` pseudo-version regex.
///
/// Returns `None` only if [`PSEUDO_RE_SRC`] fails to compile, which is impossible
/// for the constant pattern (it is exercised by the unit tests). Callers therefore
/// treat a `None` here as "not a pseudo-version" rather than panicking.
fn pseudo_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(PSEUDO_RE_SRC).ok()).as_ref()
}

struct Parsed {
    major: String,
    minor: String,
    patch: String,
    /// Prerelease including the leading `-`, or empty.
    prerelease: String,
    /// Build including the leading `+`, or empty.
    build: String,
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Parse one numeric component (no leading zeros, ≥1 digit). Returns the digits and the rest.
fn parse_int(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    match bytes {
        // A leading `0` followed by another digit is an illegal leading zero.
        [b'0', d, ..] if d.is_ascii_digit() => None,
        [first, ..] if first.is_ascii_digit() => {
            let end = s.bytes().take_while(u8::is_ascii_digit).count();
            // `end` counts ASCII digits from the start, so it is a valid char boundary.
            Some(s.split_at(end))
        }
        _ => None,
    }
}

fn valid_identifiers(s: &str, numeric_no_leading_zero: bool) -> bool {
    if s.is_empty() {
        return false;
    }
    for ident in s.split('.') {
        if ident.is_empty() {
            return false;
        }
        if !ident
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return false;
        }
        if numeric_no_leading_zero && is_digits(ident) && ident.len() > 1 && ident.starts_with('0')
        {
            return false;
        }
    }
    true
}

fn parse(v: &str) -> Option<Parsed> {
    // Mirror Go's `semver.parse` control flow exactly: a prerelease/build suffix is only legal
    // after a full `vMAJOR.MINOR.PATCH`. `v1` / `v1.2` are accepted (and canonicalised), but
    // `v1-pre` / `v1.2-pre` are NOT — after a missing minor/patch the only legal continuation is
    // `.` for the next numeric component.
    let rest = v.strip_prefix('v')?;
    let (major, rest) = parse_int(rest)?;
    if rest.is_empty() {
        return Some(parsed(major, "0", "0", "", ""));
    }
    let rest = rest.strip_prefix('.')?; // must be `.MINOR`
    let (minor, rest) = parse_int(rest)?;
    if rest.is_empty() {
        return Some(parsed(major, minor, "0", "", ""));
    }
    let rest = rest.strip_prefix('.')?; // must be `.PATCH`
    let (patch, rest) = parse_int(rest)?;

    let (prerelease, rest) = if let Some(r) = rest.strip_prefix('-') {
        // The prerelease runs up to the build separator `+` (if any); `rest` keeps the `+…` build,
        // or is empty when no build is present. `+` is ASCII, so this is a valid split point.
        let (pre, rest) = match r.find('+') {
            Some(plus) => r.split_at(plus),
            None => (r, ""),
        };
        if !valid_identifiers(pre, true) {
            return None;
        }
        (format!("-{pre}"), rest)
    } else {
        (String::new(), rest)
    };

    let build = if let Some(r) = rest.strip_prefix('+') {
        if !valid_identifiers(r, false) {
            return None;
        }
        format!("+{r}")
    } else if rest.is_empty() {
        String::new()
    } else {
        return None; // trailing garbage
    };

    Some(Parsed {
        major: major.to_string(),
        minor: minor.to_string(),
        patch: patch.to_string(),
        prerelease,
        build,
    })
}

fn parsed(major: &str, minor: &str, patch: &str, pre: &str, build: &str) -> Parsed {
    Parsed {
        major: major.to_string(),
        minor: minor.to_string(),
        patch: patch.to_string(),
        prerelease: pre.to_string(),
        build: build.to_string(),
    }
}

/// Reports whether `v` is a valid `v`-prefixed semver string.
///
/// Mirrors `semver.IsValid` from `golang.org/x/mod`. Short forms such as `v1` and
/// `v1.2` are accepted (Go canonicalises them), but a prerelease or build suffix is
/// only legal after a full `vMAJOR.MINOR.PATCH` triple.
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// assert!(semver::is_valid("v1.2.3"));
/// assert!(semver::is_valid("v1")); // short form is accepted
/// assert!(!semver::is_valid("1.2.3")); // missing `v` prefix
/// assert!(!semver::is_valid("v1-pre")); // suffix needs the full triple
/// ```
#[must_use]
pub fn is_valid(v: &str) -> bool {
    parse(v).is_some()
}

/// Returns the canonical form of `v`, or `""` if `v` is invalid.
///
/// Mirrors `semver.Canonical`: missing minor/patch components are filled with `0`
/// and build metadata is stripped. The prerelease suffix is preserved.
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// assert_eq!(semver::canonical("v1.2"), "v1.2.0");
/// assert_eq!(semver::canonical("v1.2.3+meta"), "v1.2.3");
/// assert_eq!(semver::canonical("v1.2.3-pre+meta"), "v1.2.3-pre");
/// assert_eq!(semver::canonical("nope"), "");
/// ```
#[must_use]
pub fn canonical(v: &str) -> String {
    match parse(v) {
        Some(p) => format!("v{}.{}.{}{}", p.major, p.minor, p.patch, p.prerelease),
        None => String::new(),
    }
}

/// Returns the canonical form of `v`, preserving a `+incompatible` build suffix.
///
/// Mirrors `module.CanonicalVersion`: like [`canonical`], but the Go module system's
/// special `+incompatible` marker is retained (all other build metadata is dropped).
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// assert_eq!(semver::canonical_version("v3.0.0+incompatible"), "v3.0.0+incompatible");
/// assert_eq!(semver::canonical_version("v1.2.3+meta"), "v1.2.3");
/// ```
#[must_use]
pub fn canonical_version(v: &str) -> String {
    let c = canonical(v);
    if c.is_empty() {
        return c;
    }
    if build(v) == "+incompatible" {
        format!("{c}+incompatible")
    } else {
        c
    }
}

/// Returns the major-version prefix `vN` of `v`, or `""` if `v` is invalid.
///
/// Mirrors `semver.Major`.
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// assert_eq!(semver::major("v2.1.0"), "v2");
/// assert_eq!(semver::major("nope"), "");
/// ```
#[must_use]
pub fn major(v: &str) -> String {
    match parse(v) {
        Some(p) => format!("v{}", p.major),
        None => String::new(),
    }
}

/// Returns the major/minor prefix `vN.M` of `v`, or `""` if `v` is invalid.
///
/// Mirrors `semver.MajorMinor`.
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// assert_eq!(semver::major_minor("v2.1.0"), "v2.1");
/// ```
#[must_use]
pub fn major_minor(v: &str) -> String {
    match parse(v) {
        Some(p) => format!("v{}.{}", p.major, p.minor),
        None => String::new(),
    }
}

/// Returns the prerelease suffix of `v` including the leading `-`, or `""` if absent or invalid.
///
/// Mirrors `semver.Prerelease`.
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// assert_eq!(semver::prerelease("v2.1.0-pre+meta"), "-pre");
/// assert_eq!(semver::prerelease("v2.1.0"), "");
/// ```
#[must_use]
pub fn prerelease(v: &str) -> String {
    parse(v).map(|p| p.prerelease).unwrap_or_default()
}

/// Returns the build-metadata suffix of `v` including the leading `+`, or `""` if absent or invalid.
///
/// Mirrors `semver.Build`.
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// assert_eq!(semver::build("v2.1.0-pre+meta"), "+meta");
/// assert_eq!(semver::build("v2.1.0"), "");
/// ```
#[must_use]
pub fn build(v: &str) -> String {
    parse(v).map(|p| p.build).unwrap_or_default()
}

/// Reports whether `v` is a stable release, i.e. valid and with no prerelease segment.
///
/// Build metadata (including `+incompatible`) does not make a version unstable.
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// assert!(semver::is_stable("v1.2.3"));
/// assert!(semver::is_stable("v3.0.0+incompatible"));
/// assert!(!semver::is_stable("v1.2.3-rc1"));
/// ```
#[must_use]
pub fn is_stable(v: &str) -> bool {
    is_valid(v) && prerelease(v).is_empty()
}

/// Reports whether `v` carries the Go module system's `+incompatible` build marker.
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// assert!(semver::is_incompatible("v3.0.0+incompatible"));
/// assert!(!semver::is_incompatible("v3.0.0"));
/// ```
#[must_use]
pub fn is_incompatible(v: &str) -> bool {
    build(v) == "+incompatible"
}

/// Reports whether `v` is a Go pseudo-version.
///
/// Mirrors `module.IsPseudoVersion`: `v` must contain at least two `-`, be valid
/// semver, and match the exact `golang.org/x/mod/module` pseudo-version regex.
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// assert!(semver::is_pseudo("v0.0.0-20191109021931-daa7c04131f5")); // spellcheck:ignore-line
/// assert!(!semver::is_pseudo("v1.2.3"));
/// assert!(!semver::is_pseudo("v1.2.3-pre"));
/// ```
#[must_use]
pub fn is_pseudo(v: &str) -> bool {
    v.matches('-').count() >= 2 && is_valid(v) && pseudo_re().is_some_and(|re| re.is_match(v))
}

/// Numeric comparison: shorter (smaller magnitude, since no leading zeros) first, then bytewise.
fn compare_int(x: &str, y: &str) -> Ordering {
    x.len().cmp(&y.len()).then_with(|| x.cmp(y))
}

/// `semver.comparePrerelease` — numeric < alphanumeric, numeric by magnitude, fewer fields < more.
fn compare_prerelease(x: &str, y: &str) -> Ordering {
    if x == y {
        return Ordering::Equal;
    }
    if x.is_empty() {
        return Ordering::Greater; // no prerelease outranks a prerelease
    }
    if y.is_empty() {
        return Ordering::Less;
    }
    let mut x = x;
    let mut y = y;
    loop {
        // Drop the single leading separator (`-` or `.`); both are one ASCII byte. The loop is
        // only entered/continued while `x` and `y` are non-empty, so a byte is always present.
        x = x.get(1..).unwrap_or("");
        y = y.get(1..).unwrap_or("");
        let ident_x = next_ident(x);
        let ident_y = next_ident(y);
        if ident_x != ident_y {
            let x_is_numeric = is_digits(ident_x);
            let y_is_numeric = is_digits(ident_y);
            if x_is_numeric != y_is_numeric {
                return if x_is_numeric {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            if x_is_numeric {
                return compare_int(ident_x, ident_y);
            }
            return ident_x.cmp(ident_y);
        }
        // `ident_*` is a prefix of the corresponding string, so the suffix slice is in bounds.
        x = x.get(ident_x.len()..).unwrap_or("");
        y = y.get(ident_y.len()..).unwrap_or("");
        if x.is_empty() || y.is_empty() {
            break;
        }
    }
    // one ran out: fewer fields < more fields
    if x.is_empty() && y.is_empty() {
        Ordering::Equal
    } else if x.is_empty() {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

/// Returns the leading dot-free identifier of `s` (everything up to the first `.`, or all of `s`).
fn next_ident(s: &str) -> &str {
    match s.split_once('.') {
        Some((ident, _)) => ident,
        None => s,
    }
}

/// Compares two semver strings, mirroring `semver.Compare`.
///
/// An invalid version sorts below any valid one. Valid versions are ordered by
/// major, then minor, then patch numerically, then by prerelease per the
/// `comparePrerelease` algorithm (numeric identifiers below alphanumeric, fewer
/// fields below more). Build metadata is ignored entirely.
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
/// use std::cmp::Ordering;
///
/// assert_eq!(semver::compare("v1.0.0", "v1.0.1"), Ordering::Less);
/// assert_eq!(semver::compare("v1.0.0-rc.1", "v1.0.0"), Ordering::Less);
/// assert_eq!(semver::compare("v1.0.0+meta", "v1.0.0"), Ordering::Equal);
/// ```
#[must_use]
pub fn compare(a: &str, b: &str) -> Ordering {
    match (parse(a), parse(b)) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(pa), Some(pb)) => compare_int(&pa.major, &pb.major)
            .then_with(|| compare_int(&pa.minor, &pb.minor))
            .then_with(|| compare_int(&pa.patch, &pb.patch))
            .then_with(|| compare_prerelease(&pa.prerelease, &pb.prerelease)),
    }
}

/// Returns the commit timestamp (UTC) embedded in a pseudo-version, or `None`.
///
/// Mirrors `module.PseudoVersionTime`: the 14-digit `YYYYMMDDhhmmss` run immediately
/// preceding the final `-rev[+build]` segment is decoded as a UTC instant. Returns
/// `None` when `v` is not a pseudo-version (see [`is_pseudo`]) or the encoded instant
/// is not a valid calendar time.
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// let t = semver::pseudo_time("v0.0.0-20191109021931-daa7c04131f5").unwrap(); // spellcheck:ignore-line
/// assert_eq!(t.to_string(), "2019-11-09T02:19:31Z");
/// assert_eq!(semver::pseudo_time("v1.2.3"), None);
/// ```
#[must_use]
pub fn pseudo_time(v: &str) -> Option<Timestamp> {
    if !is_pseudo(v) {
        return None;
    }
    // The 14-digit timestamp is the run of digits immediately before the final `-rev[+build]`.
    let caps = pseudo_re()?.captures(v)?;
    let whole = caps.get(0)?.as_str();
    // Find the `\d{14}-` segment: scan for a 15-byte window of 14 digits followed by '-',
    // where the byte before the window (if any) is not itself a digit.
    let bytes = whole.as_bytes();
    for (i, window) in bytes.windows(15).enumerate() {
        let (Some(digits), Some(&sep)) = (window.get(..14), window.last()) else {
            continue;
        };
        let preceded_by_digit = i
            .checked_sub(1)
            .and_then(|prev| bytes.get(prev))
            .is_some_and(u8::is_ascii_digit);
        if digits.iter().all(u8::is_ascii_digit) && sep == b'-' && !preceded_by_digit {
            return parse_pseudo_timestamp(digits);
        }
    }
    None
}

/// Parses a 14-byte `YYYYMMDDhhmmss` digit run into a UTC [`Timestamp`].
///
/// Returns `None` if `ts` is not exactly 14 ASCII digits or the encoded
/// calendar instant is invalid.
fn parse_pseudo_timestamp(ts: &[u8]) -> Option<Timestamp> {
    if ts.len() != 14 || !ts.iter().all(u8::is_ascii_digit) {
        return None;
    }
    // Each digit is ASCII `0..=9`; fold a fixed-width field into its integer value
    // without slicing or string re-parsing.
    let field = |range: std::ops::Range<usize>| -> Option<i64> {
        ts.get(range)?
            .iter()
            .try_fold(0_i64, |acc, &b| Some(acc * 10 + i64::from(b - b'0')))
    };
    let year = i16::try_from(field(0..4)?).ok()?;
    let month = i8::try_from(field(4..6)?).ok()?;
    let day = i8::try_from(field(6..8)?).ok()?;
    let hour = i8::try_from(field(8..10)?).ok()?;
    let min = i8::try_from(field(10..12)?).ok()?;
    let sec = i8::try_from(field(12..14)?).ok()?;
    civil::date(year, month, day)
        .at(hour, min, sec, 0)
        .to_zoned(TimeZone::UTC)
        .ok()
        .map(|z| z.timestamp())
}

/// Splits a module `path` into `(prefix, path_major, ok)`, mirroring `module.SplitPathVersion`.
///
/// `path_major` is the trailing major-version element (`/vN`, or `.vN` for `gopkg.in`
/// paths), and is empty for a base path at major v0/v1. `ok` is `false` when the path
/// carries a malformed version element (e.g. a dotted `/v2.0.0`, a leading-zero major,
/// or an explicit `/v1`), in which case `prefix` is the unchanged `path`.
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// assert_eq!(
///     semver::split_path_version("example.com/foo"),
///     ("example.com/foo".to_string(), String::new(), true)
/// );
/// assert_eq!(
///     semver::split_path_version("example.com/foo/v2"),
///     ("example.com/foo".to_string(), "/v2".to_string(), true)
/// );
/// assert_eq!(
///     semver::split_path_version("gopkg.in/yaml.v2"),
///     ("gopkg.in/yaml".to_string(), ".v2".to_string(), true)
/// );
/// assert!(!semver::split_path_version("example.com/foo/v1").2);
/// ```
#[must_use]
pub fn split_path_version(path: &str) -> (String, String, bool) {
    if let Some(rest) = path.strip_prefix("gopkg.in/") {
        return split_gopkg_in(path, rest);
    }
    let bytes = path.as_bytes();
    // Walk back over the trailing run of digits and dots; `i` lands on its first byte.
    let mut i = bytes.len();
    let mut dot = false;
    while let Some(&b) = i.checked_sub(1).and_then(|prev| bytes.get(prev)) {
        if b == b'.' {
            dot = true;
        } else if !b.is_ascii_digit() {
            break;
        }
        i -= 1;
    }
    // The major suffix must be `/vN`: at least `/v` before the run, and a non-empty run.
    let is_versioned = i > 1
        && i != bytes.len()
        && i.checked_sub(1).and_then(|p| bytes.get(p)) == Some(&b'v')
        && i.checked_sub(2).and_then(|p| bytes.get(p)) == Some(&b'/');
    if !is_versioned {
        return (path.to_string(), String::new(), true);
    }
    let (Some(prefix), Some(path_major)) = (path.get(..i - 2), path.get(i - 2..)) else {
        return (path.to_string(), String::new(), true);
    };
    // Reject a dotted run (e.g. `/v2.0.0`), a bare `/v`, a leading-zero major, and `/v1`.
    let leading_zero = bytes.get(i) == Some(&b'0');
    if dot || path_major.len() <= 2 || leading_zero || path_major == "/v1" {
        return (path.to_string(), String::new(), false);
    }
    (prefix.to_string(), path_major.to_string(), true)
}

fn split_gopkg_in(path: &str, _rest: &str) -> (String, String, bool) {
    // gopkg.in/pkg.vN or gopkg.in/user/pkg.vN (with an optional `-unstable`). Go strips `-unstable`
    // from the returned pathMajor, so the boundary is found on the stripped base and pathMajor is
    // a bare `.vN`. gopkg.in accepts `.v0`/`.v1`, but rejects a leading-zero major like `.v02`.
    let base = path.strip_suffix("-unstable").unwrap_or(path);
    // `.v` is two ASCII bytes, so `dot` and `dot + 2` are valid `str` boundaries.
    if let Some(dot) = base.rfind(".v")
        && let (Some(prefix), Some(path_major), Some(num_part)) =
            (base.get(..dot), base.get(dot..), base.get(dot + 2..))
    {
        let leading_zero = num_part.len() > 1 && num_part.starts_with('0');
        if !num_part.is_empty() && num_part.bytes().all(|b| b.is_ascii_digit()) && !leading_zero {
            // `path_major` is the bare `.vN` (with any `-unstable` suffix already stripped).
            return (prefix.to_string(), path_major.to_string(), true);
        }
    }
    (path.to_string(), String::new(), false)
}

/// Builds the module path for major version `n` (≥ 2) from a base path's `prefix`.
///
/// Returns `prefix/vN`, or `prefix.vN` for a `gopkg.in` prefix. The inverse of the
/// `prefix` returned by [`split_path_version`].
///
/// # Examples
///
/// ```
/// use cooldown_go::semver;
///
/// assert_eq!(semver::major_path("example.com/foo", 3), "example.com/foo/v3");
/// assert_eq!(semver::major_path("gopkg.in/yaml", 2), "gopkg.in/yaml.v2");
/// ```
#[must_use]
pub fn major_path(prefix: &str, n: u32) -> String {
    if prefix.starts_with("gopkg.in/") {
        format!("{prefix}.v{n}")
    } else {
        format!("{prefix}/v{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn validity_and_canonical() {
        assert!(is_valid("v1"));
        assert!(is_valid("v1.2.3"));
        assert!(is_valid("v1.2.3-pre+meta"));
        assert!(!is_valid("1.2.3"));
        assert!(!is_valid("v01.2.3"));
        assert!(!is_valid("v1.2.3-01"));
        assert_eq!(canonical("v1"), "v1.0.0");
        assert_eq!(canonical("v1.2"), "v1.2.0");
        assert_eq!(canonical("v1.2.3+meta"), "v1.2.3");
        assert_eq!(canonical("v1.2.3-pre+meta"), "v1.2.3-pre");
        assert_eq!(major("v2.1.0"), "v2");
        assert_eq!(major_minor("v2.1.0"), "v2.1");
        assert_eq!(prerelease("v2.1.0-pre+meta"), "-pre");
        assert_eq!(build("v2.1.0-pre+meta"), "+meta");
    }

    #[test]
    fn build_metadata_ignored_in_compare() {
        assert_eq!(compare("v1.0.0", "v1.0.0+meta"), Equal);
        assert_eq!(compare("v3.0.0+incompatible", "v3.0.0"), Equal);
        assert_eq!(compare("v1.0.0+a", "v1.0.0+b"), Equal);
    }

    #[test]
    fn prerelease_ordering_matches_go() {
        let ordered = [
            "v1.0.0-alpha",
            "v1.0.0-alpha.1",
            "v1.0.0-alpha.beta",
            "v1.0.0-beta",
            "v1.0.0-beta.2",
            "v1.0.0-beta.11",
            "v1.0.0-rc.1",
            "v1.0.0",
        ];
        for w in ordered.windows(2) {
            assert_eq!(compare(w[0], w[1]), Less, "{} < {}", w[0], w[1]);
            assert_eq!(compare(w[1], w[0]), Greater);
        }
    }

    #[test]
    fn incompatible_outranks_lower_and_prerelease() {
        assert_eq!(compare("v2.0.0+incompatible", "v2.0.0-pre"), Greater);
        assert_eq!(compare("v2.0.0+incompatible", "v1.9.9"), Greater);
        assert!(is_stable("v3.0.0+incompatible"));
        assert!(is_incompatible("v3.0.0+incompatible"));
    }

    #[test]
    fn pseudo_detection_and_time() {
        assert!(is_pseudo("v0.0.0-20191109021931-daa7c04131f5")); // spellcheck:ignore-line
        assert!(is_pseudo("v1.2.4-0.20191109021931-daa7c04131f5")); // spellcheck:ignore-line
        assert!(is_pseudo("v1.2.3-pre.0.20191109021931-daa7c04131f5")); // spellcheck:ignore-line
        assert!(is_pseudo("v2.0.0-20191109021931-daa7c04131f5+incompatible")); // spellcheck:ignore-line
        assert!(!is_pseudo("v1.2.3"));
        assert!(!is_pseudo("v1.2.3-pre"));

        let t = pseudo_time("v0.0.0-20191109021931-daa7c04131f5").unwrap(); // spellcheck:ignore-line
        assert_eq!(t.to_string(), "2019-11-09T02:19:31Z");
        assert_eq!(pseudo_time("v1.2.3"), None);
    }

    #[test]
    fn short_versions_reject_suffixes_like_go() {
        // Go accepts short versions but a prerelease/build suffix is only legal after a full triple.
        assert!(is_valid("v1"));
        assert!(is_valid("v1.2"));
        assert!(is_valid("v1.2.3-pre"));
        assert!(!is_valid("v1-pre"));
        assert!(!is_valid("v1.2-pre"));
        assert!(!is_valid("v1+meta"));
    }

    #[test]
    fn gopkg_in_unstable_and_leading_zero() {
        // `-unstable` is stripped from the returned pathMajor.
        assert_eq!(
            split_path_version("gopkg.in/check.v1-unstable"),
            ("gopkg.in/check".into(), ".v1".into(), true)
        );
        // A leading-zero major is rejected.
        assert!(!split_path_version("gopkg.in/foo.v02").2);
    }

    #[test]
    fn split_path_version_cases() {
        assert_eq!(
            split_path_version("example.com/foo"),
            ("example.com/foo".into(), String::new(), true)
        );
        assert_eq!(
            split_path_version("example.com/foo/v2"),
            ("example.com/foo".into(), "/v2".into(), true)
        );
        assert!(!split_path_version("example.com/foo/v1").2);
        assert!(!split_path_version("example.com/foo/v2.0.0").2);
        assert_eq!(
            split_path_version("gopkg.in/yaml.v2"),
            ("gopkg.in/yaml".into(), ".v2".into(), true)
        );
        assert_eq!(major_path("example.com/foo", 3), "example.com/foo/v3");
    }
}
