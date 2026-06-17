//! A faithful Rust port of the `golang.org/x/mod` semantics this adapter needs: `semver.Compare`,
//! `Canonical`, `Major`/`Prerelease`/`Build`, `module.IsPseudoVersion` (the *exact* x/mod regex),
//! `PseudoVersionTime`, and `module.SplitPathVersion`. Versions are never compared as lexicographic
//! strings; publish times are typed instants.

use jiff::civil;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use regex::Regex;
use std::cmp::Ordering;
use std::sync::OnceLock;

/// The exact `golang.org/x/mod/module` pseudo-version regex.
fn pseudo_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^v[0-9]+\.(0\.0-|\d+\.\d+-([^+]*\.)?0\.)\d{14}-[A-Za-z0-9]+(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$")
            .expect("valid pseudo-version regex")
    })
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
    if s.is_empty() || !s.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    if s.as_bytes()[0] == b'0' && s.len() > 1 && s.as_bytes()[1].is_ascii_digit() {
        return None; // leading zero
    }
    let end = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    Some((&s[..end], &s[end..]))
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
        let end = r.find('+').unwrap_or(r.len());
        let pre = &r[..end];
        if !valid_identifiers(pre, true) {
            return None;
        }
        (format!("-{pre}"), &r[end..])
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

/// Is `v` a valid `v`-prefixed semver?
pub fn is_valid(v: &str) -> bool {
    parse(v).is_some()
}

/// The canonical form: fills missing minor/patch, strips build metadata. `""` if invalid.
pub fn canonical(v: &str) -> String {
    match parse(v) {
        Some(p) => format!("v{}.{}.{}{}", p.major, p.minor, p.patch, p.prerelease),
        None => String::new(),
    }
}

/// `module.CanonicalVersion`: like [`canonical`] but preserves a `+incompatible` build suffix.
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

/// `semver.Major` → `vN` (or `""`).
pub fn major(v: &str) -> String {
    match parse(v) {
        Some(p) => format!("v{}", p.major),
        None => String::new(),
    }
}

/// `semver.MajorMinor` → `vN.M` (or `""`).
pub fn major_minor(v: &str) -> String {
    match parse(v) {
        Some(p) => format!("v{}.{}", p.major, p.minor),
        None => String::new(),
    }
}

/// `semver.Prerelease` → the `-...` suffix including the leading `-`, or `""`.
pub fn prerelease(v: &str) -> String {
    parse(v).map(|p| p.prerelease).unwrap_or_default()
}

/// `semver.Build` → the `+...` suffix including the leading `+`, or `""`.
pub fn build(v: &str) -> String {
    parse(v).map(|p| p.build).unwrap_or_default()
}

/// A stable release ⟺ no prerelease segment (build metadata, incl. `+incompatible`, does not count).
pub fn is_stable(v: &str) -> bool {
    is_valid(v) && prerelease(v).is_empty()
}

pub fn is_incompatible(v: &str) -> bool {
    build(v) == "+incompatible"
}

/// `module.IsPseudoVersion`: at least two `-`, valid semver, and the exact x/mod regex.
pub fn is_pseudo(v: &str) -> bool {
    v.matches('-').count() >= 2 && is_valid(v) && pseudo_re().is_match(v)
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
        // drop a leading '-' or '.'
        x = &x[1..];
        y = &y[1..];
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
        x = &x[ident_x.len()..];
        y = &y[ident_y.len()..];
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

fn next_ident(s: &str) -> &str {
    let end = s.find('.').unwrap_or(s.len());
    &s[..end]
}

/// `semver.Compare`: invalid < valid; major.minor.patch numerically; prerelease per the algorithm;
/// build metadata fully ignored.
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

/// The embedded pseudo-version commit timestamp (UTC), or `None` if not a pseudo-version.
pub fn pseudo_time(v: &str) -> Option<Timestamp> {
    if !is_pseudo(v) {
        return None;
    }
    // The 14-digit timestamp is the run of digits immediately before the final `-rev[+build]`.
    let caps = pseudo_re().captures(v)?;
    let whole = caps.get(0)?.as_str();
    // Find the `\d{14}-` segment: scan for 14 digits followed by '-'.
    let bytes = whole.as_bytes();
    for i in 0..bytes.len().saturating_sub(14) {
        if bytes[i..i + 14].iter().all(|b| b.is_ascii_digit())
            && bytes.get(i + 14) == Some(&b'-')
            && (i == 0 || !bytes[i - 1].is_ascii_digit())
        {
            let ts = &whole[i..i + 14];
            return parse_pseudo_timestamp(ts);
        }
    }
    None
}

fn parse_pseudo_timestamp(ts: &str) -> Option<Timestamp> {
    if ts.len() != 14 || !ts.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: i16 = ts[0..4].parse().ok()?;
    let month: i8 = ts[4..6].parse().ok()?;
    let day: i8 = ts[6..8].parse().ok()?;
    let hour: i8 = ts[8..10].parse().ok()?;
    let min: i8 = ts[10..12].parse().ok()?;
    let sec: i8 = ts[12..14].parse().ok()?;
    civil::date(year, month, day)
        .at(hour, min, sec, 0)
        .to_zoned(TimeZone::UTC)
        .ok()
        .map(|z| z.timestamp())
}

/// `module.SplitPathVersion(path) -> (prefix, pathMajor, ok)`.
pub fn split_path_version(path: &str) -> (String, String, bool) {
    if let Some(rest) = path.strip_prefix("gopkg.in/") {
        return split_gopkg_in(path, rest);
    }
    let bytes = path.as_bytes();
    let mut i = bytes.len();
    let mut dot = false;
    while i > 0 && (bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'.') {
        if bytes[i - 1] == b'.' {
            dot = true;
        }
        i -= 1;
    }
    if i <= 1 || i == bytes.len() || bytes[i - 1] != b'v' || bytes[i - 2] != b'/' {
        return (path.to_string(), String::new(), true);
    }
    let prefix = &path[..i - 2];
    let path_major = &path[i - 2..];
    if dot || path_major.len() <= 2 || bytes[i] == b'0' || path_major == "/v1" {
        return (path.to_string(), String::new(), false);
    }
    (prefix.to_string(), path_major.to_string(), true)
}

fn split_gopkg_in(path: &str, _rest: &str) -> (String, String, bool) {
    // gopkg.in/pkg.vN or gopkg.in/user/pkg.vN (with an optional `-unstable`). Go strips `-unstable`
    // from the returned pathMajor, so the boundary is found on the stripped base and pathMajor is
    // a bare `.vN`. gopkg.in accepts `.v0`/`.v1`, but rejects a leading-zero major like `.v02`.
    let base = path.strip_suffix("-unstable").unwrap_or(path);
    if let Some(dot) = base.rfind(".v") {
        let num_part = &base[dot + 2..];
        let leading_zero = num_part.len() > 1 && num_part.starts_with('0');
        if !num_part.is_empty() && num_part.bytes().all(|b| b.is_ascii_digit()) && !leading_zero {
            let path_major = &base[dot..]; // ".vN" (without -unstable)
            return (base[..dot].to_string(), path_major.to_string(), true);
        }
    }
    (path.to_string(), String::new(), false)
}

/// Build the module path for major `n` (≥2) given a base path's `prefix`. Returns `prefix/vN`
/// (or `prefix.vN` for gopkg.in).
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
            ("example.com/foo".into(), "".into(), true)
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
