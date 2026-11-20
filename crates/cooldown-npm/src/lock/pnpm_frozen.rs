//! Tells pnpm's own refusals of a lock apart from every other way its frozen verification can fail.
//!
//! `pnpm install --lockfile-only --frozen-lockfile` exits non-zero for a lock that no longer
//! matches its inputs, but equally for a registry, authentication, configuration, CLI, or store
//! failure.
//! Only the first is staleness, which `--allow-stale-lock` may waive, so the verdict is read from
//! the error codes pnpm prints rather than from the exit status.
//! pnpm 11 also verifies every locked entry against its supply-chain policies, and a refusal of
//! entries younger than its `minimumReleaseAge` names each of them.

use super::MinimumAgeRefusal;
use cooldown_core::{LockedVersion, Version};
use std::borrow::Cow;

/// The codes pnpm's frozen install raises for a lock that its inputs no longer match, or that is
/// absent, in every supported pnpm (9, 10, and 11).
///
/// - `OUTDATED_LOCKFILE`: an importer's specifiers no longer satisfy its manifest.
/// - `LOCKFILE_CONFIG_MISMATCH`: a setting the lock records (overrides, catalogs, patched
///   dependencies, a pnpmfile checksum, …) differs from the current configuration.
/// - `NO_LOCKFILE`: the project declares dependencies but has no lock, which the
///   `--allow-stale-lock` contract treats as stale.
/// - `FROZEN_LOCKFILE_WITH_OUTDATED_LOCKFILE`: the lock needs a full re-resolution (another lock
///   major version, or a pnpmfile hook forcing one), or pnpm 11's configDependencies are not in it.
const STALE_CODES: [&str; 4] = [
    "OUTDATED_LOCKFILE",
    "LOCKFILE_CONFIG_MISMATCH",
    "NO_LOCKFILE",
    "FROZEN_LOCKFILE_WITH_OUTDATED_LOCKFILE",
];

/// The code pnpm 11's lockfile verification raises when every entry it refuses is younger than
/// `minimumReleaseAge`; refusals of several kinds share `LOCKFILE_RESOLUTION_VERIFICATION`.
const MINIMUM_AGE_CODE: &str = "MINIMUM_RELEASE_AGE_VIOLATION";

/// What one error header pnpm's reporter printed says about the lock.
#[derive(Debug, PartialEq, Eq)]
enum Header {
    /// `ERR_PNPM_<code>` with one of the [`STALE_CODES`].
    Stale,
    /// Any other `ERR_PNPM_<code>`, or an uncoded `ERROR` (an unknown option, unparsable
    /// configuration).
    Other,
}

/// One error header pnpm's reporter printed.
#[derive(Debug, PartialEq, Eq)]
enum ErrorHeader<'a> {
    /// `ERR_PNPM_<code>`, holding the code.
    Coded(&'a str),
    /// An uncoded `ERROR`.
    Uncoded,
}

/// Whether `output`, from a failed frozen verification, is pnpm refusing a stale or absent lock.
///
/// It is when pnpm printed at least one error header and every header carries one of the
/// [`STALE_CODES`].
/// Any other header, or none at all, is some other failure, so a stale code next to a network
/// error still proves nothing.
///
/// Only header lines count, never a code quoted elsewhere, such as in the source excerpt pnpm
/// shows under a configuration parse error.
/// The verification pins pnpm's reporter, log level, and JSON output, so the user's
/// configuration cannot silence or reshape the header; an inherited `FORCE_COLOR` still colors
/// it, so ANSI escapes are dropped before a line is read.
///
/// pnpm 11 reports a changed configDependency before its reporter starts, as a bare message
/// without the code.
/// That refusal is treated as a tool error, which fails closed.
pub(crate) fn frozen_refusal_is_stale(output: &str) -> bool {
    let mut headers = output
        .lines()
        .filter_map(|line| header(&strip_ansi(line)))
        .peekable();
    headers.peek().is_some() && headers.all(|header| header == Header::Stale)
}

/// The versions a failed frozen verification refused as younger than pnpm's `minimumReleaseAge`,
/// when that refusal is all `output` reports.
///
/// The one error header must carry the minimum-age code, and every refused entry pnpm lists must
/// be one it found published within the window.
/// An entry pnpm could not check, such as one whose publish time the registry did not serve, is a
/// registry failure rather than a refusal, and so is any other header, so either yields `None`.
/// pnpm raises that failure under the same code and lists at most 20 entries, closing a longer
/// list with `…and <n> more`, so the refusal also counts the entries it left out, which may
/// include such a failure; a list that accounts for fewer entries than the header counts yields
/// `None`.
pub(crate) fn minimum_age_refusals(output: &str) -> Option<MinimumAgeRefusal> {
    let lines: Vec<Cow<'_, str>> = output.lines().map(strip_ansi).collect();
    let headers: Vec<&str> = lines
        .iter()
        .map(AsRef::as_ref)
        .filter(|line| error_header(line).is_some())
        .collect();
    let [header] = headers.as_slice() else {
        return None;
    };
    if error_header(header) != Some(ErrorHeader::Coded(MINIMUM_AGE_CODE)) {
        return None;
    }
    let counted = refused_count(header)?;
    let mut refused = Vec::new();
    let mut omitted = None;
    for line in &lines {
        // An entry is indented by exactly two spaces: `  <name>@<version> <reason>`.
        let Some(entry) = line
            .strip_prefix("  ")
            .filter(|entry| !entry.starts_with(' '))
        else {
            continue;
        };
        if let Some(count) = omitted_count(entry) {
            if omitted.replace(count).is_some() {
                return None;
            }
            continue;
        }
        let Some((spec, reason)) = entry.split_once(' ') else {
            continue;
        };
        // A scope's leading `@` belongs to the name, so the version follows the last `@`.
        let Some((name, version)) = spec
            .rsplit_once('@')
            .filter(|(name, version)| !name.is_empty() && !version.is_empty())
        else {
            continue;
        };
        if !reason.starts_with("was published at ") {
            return None;
        }
        refused.push(LockedVersion {
            name: name.to_string(),
            version: Version::new(version),
        });
    }
    let omitted = omitted.unwrap_or(0);
    (!refused.is_empty() && refused.len().checked_add(omitted) == Some(counted)).then_some(
        MinimumAgeRefusal {
            listed: refused,
            omitted,
        },
    )
}

/// The number of entries the tail line pnpm closes a truncated list with, `…and <n> more`,
/// leaves out.
fn omitted_count(entry: &str) -> Option<usize> {
    entry
        .strip_prefix("\u{2026}and ")?
        .strip_suffix(" more")?
        .parse()
        .ok()
}

/// The number of entries a verification error `header` says failed:
/// `[ERR_PNPM_<code>] <count> lockfile entries failed verification:`.
fn refused_count(header: &str) -> Option<usize> {
    let (before, _) = header.split_once(" lockfile entries failed verification")?;
    before.rsplit(char::is_whitespace).next()?.parse().ok()
}

/// The error header `line` opens with, if any, as the stale-lock verdict reads it.
fn header(line: &str) -> Option<Header> {
    error_header(line).map(|header| match header {
        ErrorHeader::Coded(code) if STALE_CODES.contains(&code) => Header::Stale,
        ErrorHeader::Coded(_) | ErrorHeader::Uncoded => Header::Other,
    })
}

/// The error header `line` opens with, if any.
///
/// pnpm 11 brackets the header (`[ERR_PNPM_OUTDATED_LOCKFILE] …`, `[ERROR] …`).
/// pnpm 9 and 10 pad it with thin spaces instead (`\u{2009}ERR_PNPM_OUTDATED_LOCKFILE\u{2009} …`),
/// which [`str::trim_start`] and [`char::is_whitespace`] both treat as whitespace.
fn error_header(line: &str) -> Option<ErrorHeader<'_>> {
    let line = line.trim_start();
    let (body, bracketed) = match line.strip_prefix('[') {
        Some(body) => (body, true),
        None => (line, false),
    };
    let closes = |rest: &str| {
        if bracketed {
            rest.starts_with(']')
        } else {
            rest.chars().next().is_none_or(char::is_whitespace)
        }
    };
    if let Some(after) = body.strip_prefix("ERR_PNPM_") {
        let end = after
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(after.len());
        let (code, rest) = after.split_at(end);
        if code.is_empty() || !closes(rest) {
            return None;
        }
        return Some(ErrorHeader::Coded(code));
    }
    body.strip_prefix("ERROR")
        .is_some_and(closes)
        .then_some(ErrorHeader::Uncoded)
}

/// `line` without its ANSI control sequences (`ESC [` parameters, then one final byte).
fn strip_ansi(line: &str) -> Cow<'_, str> {
    if !line.contains('\u{1b}') {
        return Cow::Borrowed(line);
    }
    let mut plain = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            plain.push(c);
        } else if chars.clone().next() == Some('[') {
            // A control sequence ends at its first byte in `@`..=`~`.
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    Cow::Owned(plain)
}

#[cfg(test)]
mod tests {
    use super::{MinimumAgeRefusal, frozen_refusal_is_stale, minimum_age_refusals};
    use cooldown_core::{LockedVersion, Version};
    use indoc::indoc;

    /// pnpm 11.8.0's refusal of a lock holding an entry younger than `minimumReleaseAge`, as it
    /// prints it with `--reporter=append-only --loglevel=error`.
    const PNPM_11_TOO_YOUNG: &str = indoc! {r#"
        [ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION] 2 lockfile entries failed verification:
          @sentry/types@5.30.0 was published at 2021-01-13T11:40:12.849Z, within the minimumReleaseAge cutoff (2020-09-01T00:00:38.279Z)
          is-number@7.0.0 was published at 2018-07-04T15:08:58.238Z, within the minimumReleaseAge cutoff (1836-10-02T17:00:19.949Z)

        The lockfile contains entries that the active policies reject. This can mean the lockfile is stale, or that someone committed a lockfile that bypassed the policy locally — inspect recent changes to pnpm-lock.yaml before trusting it. If the changes look expected, run "pnpm clean --lockfile" and then "pnpm install" to rebuild from a fresh resolution. Alternatively, relax the policy that flagged them.
    "#};

    fn locked(name: &str, version: &str) -> LockedVersion {
        LockedVersion {
            name: name.to_string(),
            version: Version::new(version),
        }
    }

    /// A minimum-age refusal names every listed entry, a scoped name included, even when colored.
    #[test]
    fn a_minimum_age_refusal_names_its_entries() {
        let expected = vec![
            locked("@sentry/types", "5.30.0"),
            locked("is-number", "7.0.0"),
        ];
        let complete = |listed: Vec<LockedVersion>| MinimumAgeRefusal { listed, omitted: 0 };
        assert_eq!(
            minimum_age_refusals(PNPM_11_TOO_YOUNG),
            Some(complete(expected.clone()))
        );
        let colored = PNPM_11_TOO_YOUNG.replacen(
            "[ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION]",
            "\u{1b}[41m\u{1b}[30m[ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION]\u{1b}[39m\u{1b}[49m",
            1,
        );
        assert_eq!(minimum_age_refusals(&colored), Some(complete(expected)));
        // A minimum-age refusal is no stale lock that `--allow-stale-lock` could waive.
        assert!(!frozen_refusal_is_stale(PNPM_11_TOO_YOUNG));
    }

    /// pnpm lists at most 20 refused entries and counts the rest in a tail line, which the
    /// refusal reports as omitted; a list that accounts for fewer entries than its header counts,
    /// or counts them in two tails, is inconclusive.
    #[test]
    fn a_truncated_refusal_counts_what_it_left_out() {
        let entry = |index: usize| {
            format!(
                "  pkg-{index:02}@1.0.0 was published at 2026-11-19T00:00:00.000Z, within the minimumReleaseAge cutoff (2026-11-06T00:00:00.000Z)"
            )
        };
        let listed: Vec<String> = (0..20).map(entry).collect();
        let report = |counted: usize, tails: &str| {
            format!(
                "[ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION] {counted} lockfile entries failed verification:\n{}\n{tails}",
                listed.join("\n")
            )
        };

        let truncated = minimum_age_refusals(&report(22, "  \u{2026}and 2 more\n"));
        assert_eq!(
            truncated.map(|refusal| (refusal.listed.len(), refusal.omitted)),
            Some((20, 2))
        );
        assert_eq!(
            minimum_age_refusals(&report(20, ""))
                .map(|refusal| (refusal.listed.len(), refusal.omitted)),
            Some((20, 0))
        );
        for inconclusive in [
            report(22, ""),
            report(23, "  \u{2026}and 2 more\n"),
            report(24, "  \u{2026}and 2 more\n  \u{2026}and 2 more\n"),
        ] {
            assert_eq!(minimum_age_refusals(&inconclusive), None, "{inconclusive}");
        }
    }

    /// Anything beside a plain minimum-age refusal is no refusal of young versions: an entry pnpm
    /// could not check, a refusal of several kinds, another failure next to it, or no entry at all.
    #[test]
    fn other_verification_failures_name_no_young_versions() {
        let cases = [
            // An entry whose publish time the registry did not serve
            indoc! {"
                [ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION] 1 lockfile entries failed verification:
                  left-pad@1.3.0 could not be checked against minimumReleaseAge (version not present in registry manifest)
            "},
            // A young entry beside one pnpm could not check
            indoc! {"
                [ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION] 2 lockfile entries failed verification:
                  is-number@7.0.0 was published at 2018-07-04T15:08:58.238Z, within the minimumReleaseAge cutoff (1836-10-02T17:00:19.949Z)
                  left-pad@1.3.0 could not be checked against minimumReleaseAge (fetch failed)
            "},
            // Refusals of several kinds
            indoc! {"
                [ERR_PNPM_LOCKFILE_RESOLUTION_VERIFICATION] 2 lockfile entries failed verification:
                  is-number@7.0.0 [MINIMUM_RELEASE_AGE_VIOLATION] was published at 2018-07-04T15:08:58.238Z, within the minimumReleaseAge cutoff (1836-10-02T17:00:19.949Z)
                  left-pad@1.3.0 [TRUST_DOWNGRADE] was published without provenance
            "},
            // Another failure beside the refusal
            indoc! {"
                [ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION] 1 lockfile entries failed verification:
                  is-number@7.0.0 was published at 2018-07-04T15:08:58.238Z, within the minimumReleaseAge cutoff (1836-10-02T17:00:19.949Z)
                [ERR_PNPM_META_FETCH_FAIL] GET https://registry.npmjs.org/left-pad failed
            "},
            // A stale lock
            "[ERR_PNPM_OUTDATED_LOCKFILE] Cannot install with \"frozen-lockfile\"",
            // The header without any entry
            "[ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION] 0 lockfile entries failed verification:",
            "",
        ];
        for output in cases {
            assert_eq!(minimum_age_refusals(output), None, "{output}");
        }
    }

    /// pnpm 11.8.0's refusal of a lock that misses a newly declared dependency, as it prints it
    /// with `--reporter=append-only --loglevel=error`.
    const PNPM_11_OUTDATED: &str = indoc! {r#"
        [ERR_PNPM_OUTDATED_LOCKFILE] Cannot install with "frozen-lockfile" because pnpm-lock.yaml is not up to date with <ROOT>/package.json

        Note that in CI environments this setting is true by default. If you still need to run install in such cases, use "pnpm install --no-frozen-lockfile"

          Failure reason:
          specifiers in the lockfile don't match specifiers in package.json:
        * 1 dependencies were added: is-number@^7.0.0
    "#};

    /// The header pnpm 10.34.9 and 9.15.9 print for the same refusal.
    const PNPM_10_OUTDATED: &str = "\u{2009}ERR_PNPM_OUTDATED_LOCKFILE\u{2009} Cannot install with \"frozen-lockfile\" because pnpm-lock.yaml is not up to date with <ROOT>/package.json";

    /// Every stale code is recognized in both header shapes.
    #[test]
    fn every_stale_code_is_stale_in_both_header_shapes() {
        for code in super::STALE_CODES {
            let bracketed = format!("[ERR_PNPM_{code}] Cannot install");
            let padded = format!("\u{2009}ERR_PNPM_{code}\u{2009} Cannot install");
            assert!(frozen_refusal_is_stale(&bracketed), "{bracketed}");
            assert!(frozen_refusal_is_stale(&padded), "{padded}");
        }
        assert!(frozen_refusal_is_stale(PNPM_11_OUTDATED));
        assert!(frozen_refusal_is_stale(PNPM_10_OUTDATED));
    }

    /// The refusal is still recognized behind pnpm's own progress line and a stray stderr warning,
    /// which `failure_detail` puts ahead of stdout.
    #[test]
    fn a_stale_refusal_survives_surrounding_output() {
        let output = indoc! {"
            (node:461123) [DEP0169] DeprecationWarning: `url.parse()` behavior is not standardized
            ✓ Lockfile passes supply-chain policies (verified 447ms ago)
            [ERR_PNPM_LOCKFILE_CONFIG_MISMATCH] Cannot proceed with the frozen installation. The current \"overrides\" configuration doesn't match the value found in the lockfile
        "};
        assert!(frozen_refusal_is_stale(output));
    }

    /// A header colored through an inherited `FORCE_COLOR`, as pnpm 11.8.0 and 10.34.9 print it,
    /// is still read.
    #[test]
    fn colored_headers_are_read() {
        let pnpm_11 = "\u{1b}[41m\u{1b}[31m[\u{1b}[39m\u{1b}[49m\u{1b}[41m\u{1b}[30mERR_PNPM_OUTDATED_LOCKFILE\u{1b}[39m\u{1b}[49m\u{1b}[41m\u{1b}[31m]\u{1b}[39m\u{1b}[49m \u{1b}[31mCannot install with \"frozen-lockfile\"";
        let pnpm_10 = "\u{1b}[41m\u{1b}[30m\u{2009}ERR_PNPM_OUTDATED_LOCKFILE\u{2009}\u{1b}[39m\u{1b}[49m \u{1b}[31mCannot install with \"frozen-lockfile\"";
        let uncoded = "\u{1b}[31m\u{1b}[41m\u{1b}[30m\u{2009}ERROR\u{2009}\u{1b}[39m\u{1b}[31m\u{1b}[49m \u{1b}[31mUnknown option: 'json'";
        assert!(frozen_refusal_is_stale(pnpm_11));
        assert!(frozen_refusal_is_stale(pnpm_10));
        assert!(!frozen_refusal_is_stale(uncoded));
    }

    /// Each failure kind that does not prove the lock stale stays a tool error.
    #[test]
    fn other_failures_are_not_stale() {
        let cases = [
            // Network
            "[ERR_PNPM_META_FETCH_FAIL] GET https://registry.npmjs.org/is-number: request to https://registry.npmjs.org/is-number failed, reason: getaddrinfo ENOTFOUND registry.npmjs.org",
            // Authentication
            "\u{2009}ERR_PNPM_FETCH_401\u{2009} GET https://npm.example.com/private: Unauthorized - 401",
            // Configuration
            indoc! {"
                [ERROR] missed comma between flow collection entries (2:3)

                 1 | packages: [
                 2 |   - x: :
                -------^
                For help, run: pnpm help install
            "},
            // Unsupported flag
            "\u{2009}ERROR\u{2009} Unknown option: 'frozen-lockfile'",
            // Corrupted store
            "[ERR_PNPM_TARBALL_INTEGRITY] Got unexpected checksum for \"https://registry.npmjs.org/is-number/-/is-number-7.0.0.tgz\"",
            // A lock this pnpm cannot read
            "[ERR_PNPM_LOCKFILE_BREAKING_CHANGE] Lockfile pnpm-lock.yaml not compatible with current pnpm",
            // A lock entry rejected by pnpm 11's supply-chain policies
            "[ERR_PNPM_LOCKFILE_RESOLUTION_VERIFICATION] 2 lockfile entries failed verification:",
            // pnpm 11's changed configDependency, reported without its code
            "Cannot resolve configDependencies with \"frozen-lockfile\" because the lockfile is not up to date",
            // JSON output, which the pinned flags prevent, carries no header
            indoc! {r#"
                {
                  "error": {
                    "code": "ERR_PNPM_OUTDATED_LOCKFILE",
                    "message": "Cannot install with \"frozen-lockfile\" because pnpm-lock.yaml is not up to date with <ROOT>/package.json"
                  }
                }
            "#},
            // No output at all
            "",
            "package manager exited with exit code 1",
        ];
        for output in cases {
            assert!(!frozen_refusal_is_stale(output), "{output}");
        }
    }

    /// A stale code proves nothing next to another failure, or outside a header line.
    #[test]
    fn stale_codes_count_only_as_the_sole_headers() {
        let cases = [
            // A stale refusal next to a network failure
            indoc! {"
                [ERR_PNPM_OUTDATED_LOCKFILE] Cannot install
                [ERR_PNPM_META_FETCH_FAIL] GET failed
            "},
            // A stale refusal next to an uncoded failure
            indoc! {"
                [ERROR] Unknown option: 'x'
                [ERR_PNPM_NO_LOCKFILE] Cannot install
            "},
            // A stale code quoted in a configuration excerpt under an uncoded header
            indoc! {"
                [ERROR] Flow sequence in block collection must be sufficiently indented (1:8)

                 1 | extra: [ERR_PNPM_OUTDATED_LOCKFILE, }
                -----------^
            "},
            // A stale code quoted mid-line
            "request failed: ERR_PNPM_OUTDATED_LOCKFILE",
            // Codes that merely contain a stale one
            "[ERR_PNPM_RUN_CHECK_DEPS_OUTDATED_LOCKFILE] Setting overrides of lockfile is outdated",
            "[ERR_PNPM_OUTDATED_LOCKFILE_X] Cannot install",
            "\u{2009}ERR_PNPM_OUTDATED_LOCKFILEX\u{2009} Cannot install",
            // A bracketed header that never closes
            "[ERR_PNPM_OUTDATED_LOCKFILE Cannot install",
        ];
        for output in cases {
            assert!(!frozen_refusal_is_stale(output), "{output}");
        }
    }
}
