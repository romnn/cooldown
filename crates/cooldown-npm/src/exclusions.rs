//! pnpm's `minimumReleaseAgeExclude` rule semantics, mirrored so cooldown can grant or withhold
//! exact versions without changing what any other rule permits.
//!
//! pnpm v11 evaluates the rules in order, and the first rule whose name pattern matches a package
//! decides alone: a rule without versions exempts every version, an exact rule only its `||` union
//! (`config/version-policy`).
//! Its name patterns are not globs: `*` matches any run of characters including `/`, a leading `!`
//! negates the pattern, and every other character is literal (`config/matcher`).
//! A union member is compared by its node-semver identity ([`version_identity`]).

use crate::native::ConfigStringList;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// One `minimumReleaseAgeExclude` entry split the way pnpm's version-policy parser splits it.
struct Rule<'a> {
    /// The package-name pattern.
    name: &'a str,
    /// The raw `||` union of exact versions, or `None` for a rule covering every version.
    versions: Option<&'a str>,
}

impl<'a> Rule<'a> {
    fn parse(entry: &'a str) -> Self {
        // A scope's leading `@` is part of the name, so the version separator is searched after it.
        let search_from = usize::from(entry.starts_with('@'));
        match entry
            .get(search_from..)
            .and_then(|rest| rest.find('@'))
            .map(|offset| offset + search_from)
        {
            Some(at) => Rule {
                name: entry.get(..at).unwrap_or(entry),
                versions: entry.get(at + 1..),
            },
            None => Rule {
                name: entry,
                versions: None,
            },
        }
    }

    /// Whether the name pattern matches exactly one package: no wildcard and no negation.
    fn is_literal(&self) -> bool {
        !self.name.contains('*') && !self.name.starts_with('!')
    }
}

/// Matches a package name against a pnpm name pattern.
pub(crate) fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_prefix('!') {
        Some(negated) => !wildcard_matches(negated, name),
        None => wildcard_matches(pattern, name),
    }
}

fn wildcard_matches(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(prefix) = parts.next() else {
        return name.is_empty();
    };
    let Some(mut rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let mut middles: Vec<&str> = parts.collect();
    // Without a `*`, the prefix is the whole pattern.
    let Some(suffix) = middles.pop() else {
        return rest.is_empty();
    };
    for middle in middles {
        match rest.find(middle) {
            Some(at) => rest = rest.get(at + middle.len()..).unwrap_or_default(),
            None => return false,
        }
    }
    rest.ends_with(suffix)
}

/// The version a union member names, as node-semver's `valid` reports it, or [`None`] when pnpm
/// would reject the member.
///
/// node-semver rejects input longer than 256 UTF-16 code units before trimming it with
/// JavaScript's `trim`, accepts one optional lowercase `v`, requires strict `SemVer` with each
/// numeric component at most 2^53 - 1, and drops build metadata from the version it reports.
pub(crate) fn version_identity(member: &str) -> Option<String> {
    const MAX_LENGTH: usize = 256;
    const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
    if member.encode_utf16().count() > MAX_LENGTH {
        return None;
    }
    let trimmed = member.trim_matches(is_js_whitespace);
    let unprefixed = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let mut version = semver::Version::parse(unprefixed).ok()?;
    if [version.major, version.minor, version.patch]
        .into_iter()
        .any(|component| component > MAX_SAFE_INTEGER)
    {
        return None;
    }
    version.build = semver::BuildMetadata::EMPTY;
    Some(version.to_string())
}

/// JavaScript's `trim` set: the `WhiteSpace` and `LineTerminator` code points, which include the
/// byte-order mark but not U+0085, unlike [`char::is_whitespace`].
fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\u{B}' | '\u{C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Reads a configured `minimumReleaseAgeExclude` the way pnpm does.
///
/// pnpm iterates a scalar's characters, so a scalar is one pattern per character.
pub(crate) fn configured_rules(configured: ConfigStringList) -> Vec<String> {
    match configured {
        ConfigStringList::Many(rules) => rules,
        ConfigStringList::One(scalar) => scalar.chars().map(String::from).collect(),
    }
}

/// Sets package `name`'s exact exemption to its current one without the `withheld` identities and
/// with the `granted` versions, changing no other package's exemption and deleting no rule.
///
/// The shape of the first rule matching `name` decides:
///
/// - none: a rule for `name` with the granted versions is appended;
/// - a rule without versions: nothing changes, since it already exempts every version and pnpm
///   cannot express "every version but the withheld ones";
/// - an exact rule for the literal `name`: that rule is rewritten in place, keeping its surviving
///   member spellings;
/// - a negated exact rule: a rule for `name` carrying its surviving versions and the granted ones
///   is inserted just before it, so the negated rule keeps its meaning for every other package.
///
/// With nothing granted, a rule left without versions would read as covering every version, so
/// it is left unchanged.
pub(crate) fn set_exact_permission(
    exclusions: &mut Vec<String>,
    name: &str,
    withheld: &BTreeSet<String>,
    granted: &BTreeSet<String>,
) {
    struct FirstMatch {
        index: usize,
        literal: bool,
        versions: Option<String>,
    }

    let first_match = exclusions.iter().enumerate().find_map(|(index, entry)| {
        let rule = Rule::parse(entry);
        name_matches(rule.name, name).then(|| FirstMatch {
            index,
            literal: rule.is_literal(),
            versions: rule.versions.map(str::to_string),
        })
    });
    let Some(FirstMatch {
        index,
        literal,
        versions,
    }) = first_match
    else {
        if !granted.is_empty() {
            exclusions.push(format!("{name}@{}", join(granted.iter())));
        }
        return;
    };
    // A rule without versions already exempts every version.
    let Some(versions) = versions else {
        return;
    };
    let members: Vec<&str> = versions.split("||").collect();
    let surviving: Vec<&str> = members
        .iter()
        .copied()
        .filter(|member| {
            version_identity(member).is_none_or(|identity| !withheld.contains(&identity))
        })
        .collect();
    let present: BTreeSet<String> = surviving
        .iter()
        .filter_map(|member| version_identity(member))
        .collect();
    let added: Vec<&String> = granted
        .iter()
        .filter(|version| {
            version_identity(version).is_none_or(|identity| !present.contains(&identity))
        })
        .collect();
    if surviving.len() == members.len() && added.is_empty() {
        return;
    }
    if surviving.is_empty() && added.is_empty() {
        return;
    }
    let union = if surviving.len() == members.len() {
        // Nothing withheld: the configured spelling stays verbatim and the grants follow it.
        std::iter::once(versions.clone())
            .chain(added.into_iter().cloned())
            .collect::<Vec<_>>()
            .join("||")
    } else {
        join(
            surviving
                .into_iter()
                .map(str::trim)
                .map(str::to_string)
                .chain(added.into_iter().cloned()),
        )
    };
    let rule = format!("{name}@{union}");
    match exclusions.get_mut(index) {
        Some(entry) if literal => *entry = rule,
        _ => exclusions.insert(index, rule),
    }
}

fn join<S: AsRef<str>>(versions: impl Iterator<Item = S>) -> String {
    versions
        .map(|version| version.as_ref().to_string())
        .collect::<Vec<_>>()
        .join("||")
}

/// Adds each pin's exact version to the configured exclusions ([`set_exact_permission`]).
pub(crate) fn grant_pins(pins: &[(String, String)], configured: Vec<String>) -> Vec<String> {
    let mut granted = BTreeMap::<&str, BTreeSet<String>>::new();
    let mut order = Vec::new();
    for (name, version) in pins {
        if !granted.contains_key(name.as_str()) {
            order.push(name.as_str());
        }
        granted
            .entry(name.as_str())
            .or_default()
            .insert(version.clone());
    }
    let mut exclusions = configured;
    for name in order {
        if let Some(versions) = granted.get(name) {
            set_exact_permission(&mut exclusions, name, &BTreeSet::new(), versions);
        }
    }
    exclusions
}

/// Drops the versions of literal exact rules that the lock no longer resolves, never widening
/// what any package is exempt from.
///
/// Removing members from a rule that keeps at least one cannot expose a later rule, because pnpm
/// stops at the first rule whose name matches.
/// A rule left with none is deleted only when that changes nothing a package is exempt from: it is
/// shadowed by an earlier rule for its package, or no later rule matches its package.
/// The last remaining rule is never deleted either, since an empty list removes the setting and
/// exposes whatever exclusions pnpm inherits from user or global configuration.
/// Negated, wildcard and versionless rules, and members pnpm would reject, are kept verbatim.
pub(crate) fn prune_unlocked(
    exclusions: &[String],
    locked: &HashMap<String, BTreeSet<String>>,
) -> Vec<String> {
    let mut kept: Vec<String> = Vec::with_capacity(exclusions.len());
    for (index, entry) in exclusions.iter().enumerate() {
        let rule = Rule::parse(entry);
        let Some(versions) = rule.versions.filter(|_| rule.is_literal()) else {
            kept.push(entry.clone());
            continue;
        };
        let locked_identities: BTreeSet<String> = locked
            .get(rule.name)
            .into_iter()
            .flatten()
            .filter_map(|version| version_identity(version))
            .collect();
        let members: Vec<&str> = versions.split("||").collect();
        let surviving: Vec<&str> = members
            .iter()
            .copied()
            .filter(|member| {
                version_identity(member)
                    .is_none_or(|identity| locked_identities.contains(&identity))
            })
            .collect();
        if surviving.len() == members.len() {
            kept.push(entry.clone());
        } else if !surviving.is_empty() {
            kept.push(format!(
                "{}@{}",
                rule.name,
                join(surviving.into_iter().map(str::trim))
            ));
        } else {
            let shadowed = kept
                .iter()
                .any(|earlier| name_matches(Rule::parse(earlier).name, rule.name));
            let exposes_later = exclusions
                .iter()
                .skip(index + 1)
                .any(|later| name_matches(Rule::parse(later).name, rule.name));
            let last = kept.is_empty() && index + 1 == exclusions.len();
            if (shadowed || !exposes_later) && !last {
                continue;
            }
            kept.push(entry.clone());
        }
    }
    kept
}

/// Whether `version` of `name` is exempt under `exclusions`, evaluated the way pnpm does.
#[cfg(test)]
pub(crate) fn exempts(exclusions: &[String], name: &str, version: &str) -> bool {
    let Some(rule) = exclusions
        .iter()
        .map(|entry| Rule::parse(entry))
        .find(|rule| name_matches(rule.name, name))
    else {
        return false;
    };
    rule.versions.is_none_or(|versions| {
        versions
            .split("||")
            .any(|member| version_identity(member).as_deref() == Some(version))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(entries: &[&str]) -> Vec<String> {
        entries.iter().map(ToString::to_string).collect()
    }

    fn pin(name: &str, version: &str) -> (String, String) {
        (name.to_string(), version.to_string())
    }

    fn versions(versions: &[&str]) -> BTreeSet<String> {
        versions.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn grant_pins_keep_every_configured_rule_in_order() {
        let configured = strings(&[
            "@scope/limited@1.0.0",
            "@scope/*",
            "nanoid",
            "eslint@10.5.0",
            "flatted@3.4.1",
            "flatted@3.4.2",
        ]);
        let exclusions = grant_pins(
            &[
                pin("eslint", "10.6.0"),
                pin("@scope/limited", "1.1.0"),
                pin("@scope/other", "2.0.0"),
                pin("lodash", "4.17.21"),
            ],
            configured,
        );
        // A pin joins the first exact rule naming its package, a scope glob already exempts every
        // version of its other members, and an unnamed package gets a rule appended at the end.
        // The glob stays behind the narrower rule and the second `flatted` rule stays shadowed,
        // so no other package or version gains an exemption.
        assert_eq!(
            exclusions,
            strings(&[
                "@scope/limited@1.0.0||1.1.0",
                "@scope/*",
                "nanoid",
                "eslint@10.5.0||10.6.0",
                "flatted@3.4.1",
                "flatted@3.4.2",
                "lodash@4.17.21",
            ])
        );
    }

    #[test]
    fn grant_pins_extend_loosely_spelled_exact_unions() {
        // pnpm accepts spaces, `v` prefixes and build metadata in a union, so these rules are the
        // first match and must carry the pin rather than shadow an appended rule.
        assert_eq!(
            grant_pins(
                &[
                    pin("foo", "2.0.0"),
                    pin("bar", "1.1.0"),
                    pin("baz", "1.0.0")
                ],
                strings(&[
                    "foo@1.0.0 || 1.1.0",
                    "bar@v1.0.0 || v1.1.0",
                    "baz@1.0.0+build"
                ]),
            ),
            strings(&[
                "foo@1.0.0 || 1.1.0||2.0.0",
                "bar@v1.0.0 || v1.1.0",
                "baz@1.0.0+build"
            ])
        );
    }

    #[test]
    fn grant_pins_leave_a_negated_rule_alone() {
        // `!foo@1.0.0` exempts version 1.0.0 of every package but `foo`.
        // Widening it for `bar` would widen it for all of them, so `bar` gets its own rule ahead.
        assert_eq!(
            grant_pins(
                &[pin("bar", "2.0.0"), pin("foo", "3.0.0")],
                strings(&["!foo@1.0.0"]),
            ),
            strings(&["bar@1.0.0||2.0.0", "!foo@1.0.0", "foo@3.0.0"])
        );
    }

    #[test]
    fn grant_pins_are_the_pins_without_configured_rules() {
        assert_eq!(
            grant_pins(
                &[pin("lodash", "4.17.21"), pin("@types/node", "26.6.6")],
                Vec::new(),
            ),
            strings(&["lodash@4.17.21", "@types/node@26.6.6"])
        );
    }

    /// Withholding a rejected starting pin rewrites the package's first rule atomically, so no
    /// later rule is exposed and no other package's exemption moves.
    #[test]
    fn withholding_never_exposes_a_later_rule() {
        let withheld = versions(&["1.0.0"]);
        let granted = versions(&["2.0.0"]);
        let cases: [(&[&str], &[&str]); 5] = [
            // The literal rule is rewritten in place; the wildcard behind it stays behind it.
            (&["foo@1.0.0", "*"], &["foo@2.0.0", "*"]),
            // A loose spelling of the withheld pin, build metadata included, is still withheld.
            (
                &["foo@ v1.0.0+build || 1.1.0", "*"],
                &["foo@1.1.0||2.0.0", "*"],
            ),
            // An all-version rule already exempts every version and cannot express the exception.
            (&["foo", "foo@3.0.0"], &["foo", "foo@3.0.0"]),
            (&["*"], &["*"]),
            // A negated first match keeps its meaning for every other package.
            (&["!other@1.0.0"], &["foo@2.0.0", "!other@1.0.0"]),
        ];
        for (configured, expected) in cases {
            let mut exclusions = strings(configured);
            set_exact_permission(&mut exclusions, "foo", &withheld, &granted);
            assert_eq!(exclusions, strings(expected), "{configured:?}");
            // Only an all-version first match may keep exempting the withheld pin.
            if !exempts(&strings(configured), "foo", "9.9.9") {
                assert!(!exempts(&exclusions, "foo", "1.0.0"), "{configured:?}");
            }
            assert!(exempts(&exclusions, "foo", "2.0.0"), "{configured:?}");
            for other in ["bar", "other"] {
                assert_eq!(
                    exempts(&exclusions, other, "1.0.0"),
                    exempts(&strings(configured), other, "1.0.0"),
                    "{configured:?} {other}"
                );
            }
        }
    }

    #[test]
    fn pruning_never_widens_an_exemption() {
        let locked: HashMap<String, BTreeSet<String>> = [
            ("foo".to_string(), versions(&["2.0.0"])),
            ("bar".to_string(), versions(&["1.0.0"])),
        ]
        .into();
        let cases: [(&[&str], &[&str]); 7] = [
            // A stale member goes; a rule keeping one member cannot expose anything.
            (&["foo@1.0.0||2.0.0", "*"], &["foo@2.0.0", "*"]),
            // Deleting the emptied rule would let the wildcard exempt every `foo`.
            (&["foo@1.0.0", "*"], &["foo@1.0.0", "*"]),
            // Deleting the first rule would activate the shadowed second one.
            (&["foo@1.0.0", "foo@3.0.0"], &["foo@1.0.0"]),
            // An emptied rule nothing follows for its package is deleted.
            (&["foo@1.0.0", "bar@1.0.0"], &["bar@1.0.0"]),
            // Deleting the last rule would remove the setting and expose inherited exclusions.
            (&["foo@1.0.0"], &["foo@1.0.0"]),
            // A negated rule names no single package and is never pruned.
            (
                &["!other@1.0.0", "bar@1.0.0"],
                &["!other@1.0.0", "bar@1.0.0"],
            ),
            // A member pnpm would reject is not this function's to remove.
            (
                &["foo@not-a-version||1.0.0", "bar@1.0.0"],
                &["foo@not-a-version", "bar@1.0.0"],
            ),
        ];
        for (configured, expected) in cases {
            let pruned = prune_unlocked(&strings(configured), &locked);
            assert_eq!(pruned, strings(expected), "{configured:?}");
            for (name, version) in [
                ("foo", "2.0.0"),
                ("foo", "1.0.0"),
                ("foo", "4.0.0"),
                ("zed", "1.0.0"),
            ] {
                assert!(
                    !exempts(&pruned, name, version)
                        || exempts(&strings(configured), name, version),
                    "pruning {configured:?} widened {name}@{version}"
                );
            }
        }
    }

    #[test]
    fn name_patterns_match_like_pnpm() {
        assert!(name_matches("*", "@scope/pkg"));
        assert!(name_matches("@scope/*", "@scope/pkg"));
        assert!(name_matches("@types/*-utils", "@types/react-utils"));
        assert!(!name_matches("@types/*-utils", "@types/react"));
        assert!(name_matches("a*b*c", "aXbYc"));
        assert!(!name_matches("a*b*c", "aXcYb"));
        assert!(name_matches("foo", "foo"));
        assert!(!name_matches("foo", "foobar"));
        assert!(name_matches("!foo", "bar"));
        assert!(!name_matches("!foo", "foo"));
        // Glob metacharacters other than `*` are literal in pnpm patterns.
        assert!(!name_matches("lodas?", "lodash"));
        assert!(name_matches("lodas?", "lodas?"));
    }

    /// The expectations follow node-semver 7.8.4's `valid`, which pnpm uses to read a union.
    #[test]
    fn version_identity_follows_node_semver() {
        let cases = [
            (" 1.0.0 ", Some("1.0.0")),
            ("v1.0.0", Some("1.0.0")),
            ("1.0.0+build", Some("1.0.0")),
            ("1.0.0-rc.1+build.7", Some("1.0.0-rc.1")),
            ("\u{FEFF}v1.0.0\u{FEFF}", Some("1.0.0")),
            ("\t1.2.3\n", Some("1.2.3")),
            ("9007199254740991.0.0", Some("9007199254740991.0.0")),
            ("V1.0.0", None),
            ("vv1.0.0", None),
            ("=1.0.0", None),
            ("v 1.0.0", None),
            ("1.0.0+", None),
            ("\u{85}1.0.0\u{85}", None),
            ("01.0.0", None),
            ("1.0", None),
            ("1.0.0-01", None),
            ("9007199254740992.0.0", None),
        ];
        for (member, expected) in cases {
            assert_eq!(version_identity(member).as_deref(), expected, "{member:?}");
        }
        let padded = format!("1.0.0{}", " ".repeat(260));
        assert_eq!(
            version_identity(&padded),
            None,
            "input over 256 units is rejected untrimmed"
        );
    }

    #[test]
    fn a_scalar_setting_is_one_pattern_per_character() {
        assert_eq!(
            configured_rules(ConfigStringList::One("@a/*".to_string())),
            strings(&["@", "a", "/", "*"])
        );
        assert_eq!(
            configured_rules(ConfigStringList::Many(strings(&["@a/*"]))),
            strings(&["@a/*"])
        );
    }
}
