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
use std::collections::{BTreeMap, BTreeSet};

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

/// The package name and version members of `entry` when pnpm reads it as an exact rule
/// (`parseVersionPolicyRule`): a literal name, then a `||` union whose every member node-semver
/// reads as a valid version ([`version_identity`]), such as `foo@1.2.3||v2.0.0`.
///
/// A rule without versions, a name pattern, and a union pnpm rejects, such as a range, yield
/// `None`.
pub(crate) fn exact_exclusion(entry: &str) -> Option<(&str, Vec<&str>)> {
    let rule = Rule::parse(entry);
    let versions = rule.versions?;
    if rule.name.is_empty() || !rule.is_literal() {
        return None;
    }
    let members: Vec<&str> = versions.split("||").collect();
    members
        .iter()
        .all(|member| version_identity(member).is_some())
        .then_some((rule.name, members))
}

/// Matches a package name against a pnpm name pattern.
pub(crate) fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_prefix('!') {
        Some(negated) => !wildcard_matches(negated, name),
        None => wildcard_matches(pattern, name),
    }
}

/// Whether pnpm's name matcher reads a cooldown package `selector` exactly as cooldown does, for
/// every valid npm package name.
///
/// cooldown compiles a selector as a glob whose `*` crosses `/`, which is how pnpm reads `*` too.
/// The readings therefore agree on a selector built only from single `*` wildcards and
/// characters both treat literally: ASCII letters and digits, `.`, `_`, `~`, `/` and `-`.
/// An `@` is allowed only first, as a scope, because pnpm splits a version off at any later `@`.
/// `**` is excluded because the glob gives `**/` and `/**` a recursive-directory meaning.
/// `?`, brackets, braces, `\` and `!` are glob syntax for cooldown but literal text or a negation
/// for pnpm, so a selector using them is excluded as well.
pub(crate) fn is_pnpm_safe_selector(selector: &str) -> bool {
    !selector.is_empty()
        && !selector.contains("**")
        && selector.char_indices().all(|(index, character)| {
            character.is_ascii_alphanumeric()
                || matches!(character, '.' | '_' | '~' | '/' | '-' | '*')
                || (character == '@' && index == 0)
        })
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
        // Survivors keep their raw spelling: trimming could turn a member pnpm rejects into a
        // valid one.
        join(
            surviving
                .into_iter()
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

/// How the pnpm reading a `minimumReleaseAgeExclude` list decides a package among its rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchSemantics {
    /// The first rule matching the package decides alone (`evaluateVersionPolicy`), as pnpm reads
    /// the list from 10.19.0 up to 11.9.0 ([`crate::admissions::match_semantics`]).
    FirstMatch,
    /// Not known to be first-match, so a rule behind an earlier match for its package may still
    /// count: releases before 10.19.0 fold every rule into one name matcher, where a later rule
    /// re-includes a package an earlier negation excluded, and 11.9.0 on joins the versions of
    /// every matching exact rule.
    Unverified,
}

/// Whether some literal rule of `exclusions` is one a first-match reader never reads
/// ([`drop_shadowed_rules`]), the only lists whose pruning depends on [`MatchSemantics`].
pub(crate) fn has_shadowed_rule(exclusions: &[String]) -> bool {
    drop_shadowed_rules(exclusions).len() < exclusions.len()
}

/// Drops the versions of literal exact rules that `drops` lets go, given the package name and
/// the version's identity, such as the ones the lock no longer resolves, removing stale
/// exemptions without widening any.
///
/// A first-match reader ([`MatchSemantics::FirstMatch`]) reads only the first rule matching a
/// package, so every literal rule for a package an earlier rule already matches is removed first
/// ([`drop_shadowed_rules`]), which changes nothing it exempts, and no rule left can then be
/// activated by a removal before it.
/// Under other semantics no rule is removed for being shadowed, since a later rule may still
/// count there.
/// Removing members from a rule that keeps at least one cannot expose a later rule either.
/// A rule left with none is deleted unless a later rule matches its package, since deleting it
/// could let that rule exempt every version of the package: a first-match pnpm would read the
/// later rule instead, and one joining the matching exact rules stops at the first rule without
/// versions only when no exact rule came before it.
/// A pnpm before 10.19.0 reads no exact rule as matching any package, so pruning one changes
/// nothing there.
/// An emptied list is the caller's to write as `[]`, which keeps masking the exclusions pnpm
/// inherits.
/// Negated, wildcard and versionless rules, and members pnpm would reject, are kept verbatim, and
/// surviving members keep their raw spelling.
pub(crate) fn prune_exact_members(
    exclusions: &[String],
    semantics: MatchSemantics,
    drops: impl Fn(&str, &str) -> bool,
) -> Vec<String> {
    let exclusions = match semantics {
        MatchSemantics::FirstMatch => drop_shadowed_rules(exclusions),
        MatchSemantics::Unverified => exclusions.to_vec(),
    };
    let mut kept: Vec<String> = Vec::with_capacity(exclusions.len());
    for (index, entry) in exclusions.iter().enumerate() {
        let rule = Rule::parse(entry);
        let Some(versions) = rule.versions.filter(|_| rule.is_literal()) else {
            kept.push(entry.clone());
            continue;
        };
        let members: Vec<&str> = versions.split("||").collect();
        let surviving: Vec<&str> = members
            .iter()
            .copied()
            .filter(|member| {
                version_identity(member).is_none_or(|identity| !drops(rule.name, &identity))
            })
            .collect();
        if surviving.len() == members.len() {
            kept.push(entry.clone());
        } else if !surviving.is_empty() {
            kept.push(format!("{}@{}", rule.name, join(surviving.into_iter())));
        } else if exclusions
            .iter()
            .skip(index + 1)
            .any(|later| name_matches(Rule::parse(later).name, rule.name))
        {
            kept.push(entry.clone());
        }
    }
    kept
}

/// Removes every literal rule pnpm never reads: one whose package an earlier rule already
/// matches, whether by its exact name, a name pattern or a negation, since pnpm decides each
/// package by the first rule matching it (`evaluateVersionPolicy`).
///
/// A name pattern or negation matches packages no single earlier rule accounts for, so it is
/// kept.
fn drop_shadowed_rules(exclusions: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::with_capacity(exclusions.len());
    for entry in exclusions {
        let rule = Rule::parse(entry);
        let shadowed = rule.is_literal()
            && kept
                .iter()
                .any(|earlier| name_matches(Rule::parse(earlier).name, rule.name));
        if !shadowed {
            kept.push(entry.clone());
        }
    }
    kept
}

/// Drops every rule without versions that is not in `keep`, keeping the rest verbatim and in order.
///
/// A rule without versions exempts every version of each package it is the first match for, so
/// removing one never widens what the list grants: such a package falls through to a later rule,
/// which grants at most every version, or to none.
/// Every other package keeps its first match, because only rules are removed, never added or
/// reordered.
/// Removing a version-qualified rule could instead expose a broader later one it shadows, so those
/// are always kept, whatever their spelling.
pub(crate) fn drop_versionless_rules(rules: &[String], keep: &[String]) -> Vec<String> {
    rules
        .iter()
        .filter(|rule| keep.contains(rule) || Rule::parse(rule).versions.is_some())
        .cloned()
        .collect()
}

/// What persisting admitted versions does to the native `minimumReleaseAgeExclude` setting.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Persisted {
    /// The setting stays as written.
    Unchanged,
    /// The setting becomes these rules, written as `[]` when there are none.
    Rules(Vec<String>),
    /// The setting is a scalar and versions must be granted.
    /// pnpm may expand `${VAR}` in a scalar before reading it one pattern per character, which no
    /// rewritten list reproduces, so the user has to make it a list.
    ScalarSetting,
}

/// Grants the `granted` versions in the native rules, then prunes the version members `drops`
/// lets go ([`set_exact_permission`], [`prune_exact_members`]), keeping the rules in pnpm's order.
///
/// Granting comes first, so a version admitted now is never pruned.
/// An empty list with nothing to grant stays as written; a list pruning empties becomes `[]`, and
/// either keeps masking the exclusions pnpm inherits.
pub(crate) fn persisted_rules(
    native: ConfigStringList,
    granted: &BTreeMap<String, BTreeSet<String>>,
    semantics: MatchSemantics,
    drops: impl Fn(&str, &str) -> bool,
) -> Persisted {
    let mut rules = match native {
        ConfigStringList::Many(rules) if rules.is_empty() && granted.is_empty() => {
            return Persisted::Unchanged;
        }
        ConfigStringList::Many(rules) => rules,
        ConfigStringList::One(_) if granted.is_empty() => return Persisted::Unchanged,
        ConfigStringList::One(_) => return Persisted::ScalarSetting,
    };
    for (name, versions) in granted {
        set_exact_permission(&mut rules, name, &BTreeSet::new(), versions);
    }
    Persisted::Rules(prune_exact_members(&rules, semantics, drops))
}

/// Whether `version` of `name` is exempt under `exclusions`, evaluated the way pnpm does.
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
    use std::collections::HashMap;

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
                &["foo@ 1.1.0||2.0.0", "*"],
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
            // pnpm never reads the shadowed second rule, so it goes before the stale first one,
            // which deleting then cannot activate.
            (&["foo@1.0.0", "foo@3.0.0"], &[]),
            // An emptied rule nothing follows for its package is deleted.
            (&["foo@1.0.0", "bar@1.0.0"], &["bar@1.0.0"]),
            // So is the last one, the emptied list being written as `[]`.
            (&["foo@1.0.0"], &[]),
            // A negated rule names no single package and is never pruned, while it shadows the
            // rule for every other package after it.
            (&["!other@1.0.0", "bar@1.0.0"], &["!other@1.0.0"]),
            // A member pnpm would reject is not this function's to remove.
            (
                &["foo@not-a-version||1.0.0", "bar@1.0.0"],
                &["foo@not-a-version", "bar@1.0.0"],
            ),
        ];
        for (configured, expected) in cases {
            let pruned = prune_exact_members(
                &strings(configured),
                MatchSemantics::FirstMatch,
                unlocked(&locked),
            );
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

    /// Dropping the rules without versions never widens a grant, whatever the kept rules' version
    /// spellings: a `v`-prefixed or spaced union stays, and an exact rule keeps shadowing the
    /// broader rule after it.
    #[test]
    fn dropping_versionless_rules_never_widens_an_exemption() {
        let cases: [(&[&str], &[&str], &[&str]); 6] = [
            // pnpm-valid exact spellings are kept, so no version moves between them.
            (
                &["foo@v1.0.0", "foo@2.0.0", "*"],
                &[],
                &["foo@v1.0.0", "foo@2.0.0"],
            ),
            (
                &["foo@1.0.0 || 2.0.0", "@corp/*"],
                &[],
                &["foo@1.0.0 || 2.0.0"],
            ),
            // The exact rule keeps shadowing the later name-wide rule, which goes.
            (&["foo@1.0.0", "foo"], &[], &["foo@1.0.0"]),
            // A negated rule without versions exempts nearly every package and goes too.
            (&["!foo", "bar@1.0.0"], &[], &["bar@1.0.0"]),
            // A kept selector stays where it is.
            (
                &["*-plugin", "*", "foo@1.0.0"],
                &["*-plugin"],
                &["*-plugin", "foo@1.0.0"],
            ),
            // Version-qualified rules of any shape are kept, even ones pnpm would reject.
            (
                &["@corp/*@1.0.0", "foo@not-a-version", "!bar@1.0.0"],
                &[],
                &["@corp/*@1.0.0", "foo@not-a-version", "!bar@1.0.0"],
            ),
        ];
        for (configured, keep, expected) in cases {
            let before = strings(configured);
            let after = drop_versionless_rules(&before, &strings(keep));
            assert_eq!(after, strings(expected), "{configured:?}");
            for name in ["foo", "bar", "@corp/ui", "eslint-plugin", "zed"] {
                for version in ["1.0.0", "2.0.0", "3.0.0"] {
                    assert!(
                        !exempts(&after, name, version) || exempts(&before, name, version),
                        "dropping from {configured:?} widened {name}@{version}"
                    );
                }
            }
        }
    }

    /// A member pnpm rejects keeps its exact bytes through a rebuild, so a configuration pnpm
    /// refuses never becomes one it accepts.
    #[test]
    fn rebuilding_a_union_keeps_rejected_members_byte_for_byte() {
        let nel = "foo@\u{85}1.0.0\u{85}||2.0.0";
        let padded = format!("foo@1.0.0{}||2.0.0", " ".repeat(260));
        for configured in [nel.to_string(), padded] {
            let invalid = configured
                .strip_suffix("||2.0.0")
                .expect("the fixture ends with the valid member");
            let mut withheld = vec![configured.clone(), "bar@1.0.0".to_string()];
            set_exact_permission(
                &mut withheld,
                "foo",
                &versions(&["2.0.0"]),
                &versions(&["3.0.0"]),
            );
            assert_eq!(withheld[0], format!("{invalid}||3.0.0"));

            let locked: HashMap<String, BTreeSet<String>> =
                [("bar".to_string(), versions(&["1.0.0"]))].into();
            let pruned = prune_exact_members(
                &[configured.clone(), "bar@1.0.0".to_string()],
                MatchSemantics::FirstMatch,
                unlocked(&locked),
            );
            assert_eq!(pruned[0], invalid);
        }
    }

    /// An exact rule is one pnpm reads as such: a literal name and a union of versions node-semver
    /// reads as valid, a `v` prefix, padding and build metadata included, while `=` and ranges
    /// are not versions to it.
    #[test]
    fn exact_exclusion_claims_the_unions_pnpm_reads_as_exact() {
        assert_eq!(
            exact_exclusion("@scope/a@1.2.2||2.0.1"),
            Some(("@scope/a", vec!["1.2.2", "2.0.1"]))
        );
        assert_eq!(exact_exclusion("foo@v1.2.3"), Some(("foo", vec!["v1.2.3"])));
        assert_eq!(
            exact_exclusion("foo@ 1.2.3 ||1.2.4+build"),
            Some(("foo", vec![" 1.2.3 ", "1.2.4+build"]))
        );
        for entry in [
            "a",
            "@scope/*",
            "a@^1.2.2",
            "a@1.2.2||*",
            "a@=1.2.2",
            "a@",
            "@1.2.2",
            "f*@1.0.0",
            "!a@1.0.0",
        ] {
            assert_eq!(exact_exclusion(entry), None, "{entry}");
        }
    }

    /// A rule for a package an earlier rule already matches, by name, pattern or negation, is one
    /// pnpm never reads, so it goes before anything is pruned, and nothing pnpm exempts changes.
    #[test]
    fn shadowed_rules_go_first() {
        let locked: HashMap<String, BTreeSet<String>> =
            [("foo".to_string(), versions(&["2.0.0"]))].into();
        let cases: [(&[&str], &[&str]); 4] = [
            (&["foo@1.0.0", "foo@2.0.0"], &[]),
            (&["f*", "foo@2.0.0"], &["f*"]),
            (&["foo", "foo@2.0.0"], &["foo"]),
            // A later pattern is kept, and keeps the emptied rule it would otherwise expose.
            (&["foo@1.0.0", "f*"], &["foo@1.0.0", "f*"]),
        ];
        for (configured, expected) in cases {
            let configured = strings(configured);
            let pruned =
                prune_exact_members(&configured, MatchSemantics::FirstMatch, unlocked(&locked));
            assert_eq!(pruned, strings(expected), "{configured:?}");
            for version in ["1.0.0", "2.0.0", "3.0.0"] {
                assert!(
                    !exempts(&pruned, "foo", version) || exempts(&configured, "foo", version),
                    "pruning {configured:?} widened foo@{version}"
                );
            }
        }
    }

    /// Only a first-match reader never reads a rule behind an earlier match for its package: an
    /// earlier pnpm lets `@team/safe-legacy` re-include the package `!*-legacy` excluded, so
    /// without first-match semantics the rule stays, and pruning still never touches a
    /// versionless rule.
    #[test]
    fn shadowed_rules_go_only_under_first_match() {
        let rules = strings(&["@team/*", "!*-legacy", "@team/safe-legacy"]);
        let nothing_locked = HashMap::new();
        assert!(has_shadowed_rule(&rules));
        assert_eq!(
            prune_exact_members(
                &rules,
                MatchSemantics::FirstMatch,
                unlocked(&nothing_locked)
            ),
            strings(&["@team/*", "!*-legacy"])
        );
        assert_eq!(
            prune_exact_members(
                &rules,
                MatchSemantics::Unverified,
                unlocked(&nothing_locked)
            ),
            rules
        );
        // A stale exact member still goes, and an emptied rule a later one matches stays.
        let stale = strings(&["foo@1.0.0||2.0.0", "bar@1.0.0", "b*"]);
        let locked: HashMap<String, BTreeSet<String>> =
            [("foo".to_string(), versions(&["2.0.0"]))].into();
        assert_eq!(
            prune_exact_members(&stale, MatchSemantics::Unverified, unlocked(&locked)),
            strings(&["foo@2.0.0", "bar@1.0.0", "b*"])
        );
    }

    /// Lets go of every version member the lock no longer holds.
    fn unlocked(locked: &HashMap<String, BTreeSet<String>>) -> impl Fn(&str, &str) -> bool + '_ {
        |name, identity| {
            !locked
                .get(name)
                .is_some_and(|versions| versions.contains(identity))
        }
    }

    #[test]
    fn persisting_never_removes_the_setting_or_rewrites_a_scalar() {
        let locked: HashMap<String, BTreeSet<String>> =
            [("foo".to_string(), versions(&["2.0.0"]))].into();
        let granted: BTreeMap<String, BTreeSet<String>> =
            [("foo".to_string(), versions(&["2.0.0"]))].into();
        let nothing = BTreeMap::new();
        // An explicit empty list masks inherited exclusions and stays as written.
        assert_eq!(
            persisted_rules(
                ConfigStringList::Many(Vec::new()),
                &nothing,
                MatchSemantics::FirstMatch,
                unlocked(&locked)
            ),
            Persisted::Unchanged
        );
        assert_eq!(
            persisted_rules(
                ConfigStringList::Many(Vec::new()),
                &granted,
                MatchSemantics::FirstMatch,
                unlocked(&locked)
            ),
            Persisted::Rules(strings(&["foo@2.0.0"]))
        );
        // A scalar is left alone unless something must be granted, which it cannot record.
        for scalar in ["", "${COOLDOWN_EXCLUDES}"] {
            assert_eq!(
                persisted_rules(
                    ConfigStringList::One(scalar.to_string()),
                    &nothing,
                    MatchSemantics::FirstMatch,
                    unlocked(&locked)
                ),
                Persisted::Unchanged
            );
            assert_eq!(
                persisted_rules(
                    ConfigStringList::One(scalar.to_string()),
                    &granted,
                    MatchSemantics::FirstMatch,
                    unlocked(&locked)
                ),
                Persisted::ScalarSetting
            );
        }
        // Granting before pruning keeps the admitted version, and the stale one goes.
        assert_eq!(
            persisted_rules(
                ConfigStringList::Many(strings(&["foo@1.0.0", "*"])),
                &granted,
                MatchSemantics::FirstMatch,
                unlocked(&locked)
            ),
            Persisted::Rules(strings(&["foo@2.0.0", "*"]))
        );
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

    /// The selectors `sync` writes verbatim are the ones pnpm's matcher cannot misread: literal
    /// characters, single `*` wildcards and a leading scope `@`, which pnpm parses as a
    /// versionless, non-negated rule.
    #[test]
    fn pnpm_safe_selectors_use_only_syntax_both_matchers_share() {
        // Safe
        for selector in [
            "*-plugin",
            "@types/*",
            "*",
            "JSONStream",
            "lodash.merge",
            "@scope/a_b~c-d",
            "a*b*c",
        ] {
            assert!(is_pnpm_safe_selector(selector), "{selector}");
            let rule = Rule::parse(selector);
            assert_eq!(rule.name, selector, "{selector} carries no version");
            assert!(rule.versions.is_none(), "{selector} carries no version");
            assert!(!selector.starts_with('!'), "{selector} is not negated");
        }

        // Unsafe
        for selector in [
            "",
            "**",
            "@types/**",
            "**/foo",
            "foo{,bar}",
            "{safe,!internal-*}",
            "\\!x",
            "!internal-*",
            "react-?om",
            "[!a-c]*",
            "*@types/*",
            "foo@1.0.0",
            "foo bar",
            "react\u{e9}",
        ] {
            assert!(!is_pnpm_safe_selector(selector), "{selector}");
        }
    }

    /// Over valid npm package names, cooldown's glob and pnpm's matcher agree on every pnpm-safe
    /// selector.
    ///
    /// The selectors are derived from the sample names by replacing one or two runs with `*`, so
    /// each selector matches at least its source name and misses most others.
    /// Names cover scopes, the legacy uppercase spelling, and every punctuation character npm
    /// allows, and a case-flipped selector checks that both matchers are case sensitive.
    #[test]
    fn safe_selectors_match_npm_names_as_cooldown_globs_do() -> color_eyre::eyre::Result<()> {
        let names = [
            "a",
            "react",
            "react-dom",
            "react.dom",
            "react_dom",
            "react~dom",
            "reactdom",
            "JSONStream",
            "jsonstream",
            "eslint-plugin-react",
            "@types/node",
            "@types/react",
            "@types/react-dom",
            "@typescript-eslint/eslint-plugin",
            "@scope/a",
            "@scope.js/pkg",
            "@scope/pkg.js",
            "lodash.merge",
            "x-plugin-y-plugin",
        ];
        let mut selectors = BTreeSet::new();
        for name in names {
            selectors.insert(name.to_string());
            selectors.insert(name.to_ascii_uppercase());
            let length = name.len();
            for start in 0..=length {
                for end in start..=length {
                    selectors.insert(format!("{}*{}", &name[..start], &name[end..]));
                }
            }
            // Two wildcards around a short literal run, with an empty or one-character prefix
            // and suffix, keep the sample small while still separating the `*`s.
            for start in 0..=1.min(length) {
                for end in start..length {
                    for middle_end in end + 1..=(end + 2).min(length) {
                        for tail in [length.saturating_sub(1), length] {
                            if tail >= middle_end {
                                selectors.insert(format!(
                                    "{}*{}*{}",
                                    &name[..start],
                                    &name[end..middle_end],
                                    &name[tail..]
                                ));
                            }
                        }
                    }
                }
            }
        }
        let mut positives = 0;
        let mut negatives = 0;
        for selector in selectors
            .iter()
            .filter(|selector| is_pnpm_safe_selector(selector))
        {
            let glob = cooldown_core::PatternGlob::new(selector)?;
            for name in names {
                let pnpm = name_matches(selector, name);
                assert_eq!(glob.is_match(name), pnpm, "selector {selector} on {name}");
                if pnpm {
                    positives += 1;
                } else {
                    negatives += 1;
                }
            }
        }
        // Both outcomes are exercised many times over, so the agreement is not vacuous.
        assert!(positives > 1000, "{positives} positive matches");
        assert!(negatives > 1000, "{negatives} negative matches");

        // The same sample tells the matchers apart on each kind of unsafe syntax.
        for selector in ["react-?om", "@types/{node,react}", "[!a-c]*", "**/react"] {
            let glob = cooldown_core::PatternGlob::new(selector)?;
            assert!(
                names
                    .iter()
                    .any(|name| glob.is_match(name) != name_matches(selector, name)),
                "selector {selector} reads the same in both matchers"
            );
        }
        Ok(())
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
