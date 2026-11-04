//! How Yarn Berry reads an `npmPreapprovedPackages` entry, mirrored so `sync` writes only entries
//! that never exempt a release cooldown still gates.
//!
//! Yarn approves a release when any entry matches it, in no order and without negation
//! (`isPreapproved` in [npmConfigUtils.ts][npm-config-utils]).
//! An entry is parsed as a descriptor (`tryParseDescriptor` in [structUtils.ts][struct-utils]):
//! an optional `@scope/`, a name without `@` or `/`, and an optional `@range`.
//! Its ident matches a package by equality or by micromatch, whose `*` stops at `/`, and an entry
//! without a range covers every version, while a ranged one covers the versions its node-semver
//! range admits.
//! The rule is the same from Yarn 4.10.0, which introduced the setting, through 4.18.1.
//!
//! [npm-config-utils]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/plugin-npm/sources/npmConfigUtils.ts
//! [struct-utils]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/structUtils.ts

/// How Yarn reads a cooldown package selector written as an `npmPreapprovedPackages` entry,
/// compared over every valid npm package name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectorReading {
    /// Yarn matches exactly the names cooldown's selector matches.
    Exact,
    /// Yarn matches only the unscoped names among them, because the selector opens with a `*`
    /// that cooldown lets cross a scope's `/` and Yarn does not.
    UnscopedOnly,
    /// Yarn reads the selector differently or as no descriptor at all, so it is not written.
    Unreadable,
}

/// How Yarn reads the cooldown package `selector`.
///
/// cooldown compiles a selector as a glob whose `*` crosses `/`.
/// The readings can agree only on a selector built from single `*` wildcards and characters both
/// treat literally: ASCII letters and digits, `.`, `_`, `~`, `-`, a `/`, and an `@` opening a
/// scope, since Yarn splits a range off at any later `@`.
/// `?`, brackets, braces, `!` and `**` are glob syntax with another meaning in micromatch, and
/// Yarn's descriptor grammar refuses a second `/`, so such a selector is [`Unreadable`].
///
/// A valid npm name holds at most one `/`, the one after its scope.
/// A selector with a scope therefore matches the same names under both readings, since its `/`
/// must meet the name's.
/// An unscoped selector opening with a literal character matches only unscoped names in either
/// reading, while one opening with `*` also matches scoped names for cooldown alone.
///
/// [`Unreadable`]: SelectorReading::Unreadable
pub(super) fn reading(selector: &str) -> SelectorReading {
    let Some(ident) = Ident::parse(selector) else {
        return SelectorReading::Unreadable;
    };
    if selector.contains("**") || !ident.characters().all(is_literal_or_wildcard) {
        return SelectorReading::Unreadable;
    }
    if ident.scope.is_none() && selector.starts_with('*') {
        SelectorReading::UnscopedOnly
    } else {
        SelectorReading::Exact
    }
}

/// Whether a character means the same to cooldown's glob and to micromatch.
fn is_literal_or_wildcard(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '~' | '-' | '*')
}

/// Whether Yarn's ident matcher matches the package `name` with the written selector `pattern`.
///
/// `pattern` must be a selector [`reading`] does not call [`Unreadable`]: a `*` in it then matches
/// any run of characters but `/`, and every other character itself.
///
/// [`Unreadable`]: SelectorReading::Unreadable
pub(super) fn yarn_matches(pattern: &str, name: &str) -> bool {
    let name: Vec<char> = name.chars().collect();
    // `matched[index]` says whether the pattern read so far matches `name[..index]`.
    let mut matched = vec![false; name.len() + 1];
    if let Some(first) = matched.first_mut() {
        *first = true;
    }
    for token in pattern.chars() {
        let mut next = vec![false; name.len() + 1];
        if token == '*' {
            // A wildcard extends a match by any run of characters that crosses no `/`.
            for index in 0..=name.len() {
                let extended = index > 0
                    && next.get(index - 1).copied().unwrap_or(false)
                    && name
                        .get(index - 1)
                        .is_some_and(|&character| character != '/');
                let here = matched.get(index).copied().unwrap_or(false);
                if let Some(slot) = next.get_mut(index) {
                    *slot = here || extended;
                }
            }
        } else {
            for index in 1..=name.len() {
                let reached = matched.get(index - 1).copied().unwrap_or(false)
                    && name.get(index - 1) == Some(&token);
                if let Some(slot) = next.get_mut(index) {
                    *slot = reached;
                }
            }
        }
        matched = next;
    }
    matched.last().copied().unwrap_or(false)
}

/// An `npmPreapprovedPackages` entry split the way Yarn's descriptor parser splits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Entry<'a> {
    /// The ident, `@scope/name` or `name`, possibly a micromatch pattern.
    pub(super) ident: &'a str,
    /// The range after the ident's `@`, or `None` for an entry covering every version.
    pub(super) range: Option<&'a str>,
}

impl<'a> Entry<'a> {
    /// Splits `entry` as Yarn does, or `None` when Yarn parses no descriptor from it, in which
    /// case it matches no package at all.
    pub(super) fn parse(entry: &'a str) -> Option<Self> {
        // A scope's leading `@` is part of the ident, so the range separator is searched after it.
        let search_from = usize::from(entry.starts_with('@'));
        let (ident, range) = match entry
            .get(search_from..)
            .and_then(|rest| rest.find('@'))
            .map(|offset| offset + search_from)
        {
            Some(at) => (entry.get(..at)?, Some(entry.get(at + 1..)?)),
            None => (entry, None),
        };
        if range.is_some_and(str::is_empty) {
            return None;
        }
        Ident::parse(ident)?;
        Some(Entry { ident, range })
    }

    /// The exact versions a cooldown-owned exact entry admits: a literal package name with a
    /// version or an `||` union of versions, or `None` for any other entry.
    ///
    /// cooldown writes one version per entry, but a union a user wrote is recognized too, so an
    /// exact admission survives a sync that could not evaluate admissions.
    pub(super) fn exact_versions(&self) -> Option<Vec<&'a str>> {
        let range = self.range?;
        let literal = Ident::parse(self.ident)?
            .characters()
            .all(|character| is_literal_or_wildcard(character) && character != '*');
        if !literal {
            return None;
        }
        let versions: Vec<&str> = range.split("||").map(str::trim).collect();
        versions
            .iter()
            .all(|version| crate::version::parse(version).is_some())
            .then_some(versions)
    }
}

/// The ident of a descriptor Yarn's loose grammar accepts: `^(?:@([^/]+?)\/)?([^@/]+?)$`.
struct Ident<'a> {
    scope: Option<&'a str>,
    name: &'a str,
}

impl<'a> Ident<'a> {
    fn parse(ident: &'a str) -> Option<Self> {
        let (scope, name) = match ident.strip_prefix('@') {
            Some(scoped) => {
                let (scope, name) = scoped.split_once('/')?;
                (Some(scope), name)
            }
            None => (None, ident),
        };
        let valid = |part: &str| !part.is_empty() && !part.contains(['@', '/']);
        (scope.is_none_or(valid) && valid(name)).then_some(Ident { scope, name })
    }

    /// The characters of the scope and the name, without the `@` and `/` around the scope.
    fn characters(&self) -> impl Iterator<Item = char> + 'a {
        self.scope
            .unwrap_or_default()
            .chars()
            .chain(self.name.chars())
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, SelectorReading, reading, yarn_matches};

    /// A selector is written only when Yarn matches no name cooldown's own glob does not, and
    /// one Yarn matches more narrowly is told apart from one it reads identically.
    #[test]
    fn selectors_are_classified_by_how_yarn_reads_them() {
        // Literal names and patterns whose `*` never needs to cross a `/`
        for selector in [
            "has-flag",
            "@corp/ui",
            "@corp/*",
            "@*/eslint-config",
            "eslint-*",
            "lodash.*",
            "JSONStream",
        ] {
            assert_eq!(reading(selector), SelectorReading::Exact, "{selector}");
        }

        // A leading `*` without a scope also reaches scoped names for cooldown alone.
        for selector in ["*", "*-plugin", "*eslint*"] {
            assert_eq!(
                reading(selector),
                SelectorReading::UnscopedOnly,
                "{selector}"
            );
        }

        // Glob syntax micromatch reads differently, and selectors Yarn parses as no ident
        for selector in [
            "",
            "**",
            "@corp/**",
            "react-?om",
            "@types/{node,react}",
            "[a-c]*",
            "!foo",
            "@*",
            "@corp",
            "a/b",
            "@a/b/c",
            "foo@1.0.0",
            "foo+bar",
        ] {
            assert_eq!(reading(selector), SelectorReading::Unreadable, "{selector}");
        }
    }

    /// Yarn's `*` matches any run of characters but `/`, so a pattern opening with `*` never
    /// reaches a scoped name.
    #[test]
    fn yarns_wildcard_stops_at_the_scope_separator() {
        assert!(yarn_matches("has-flag", "has-flag"));
        assert!(!yarn_matches("has-flag", "has-flags"));
        assert!(yarn_matches("@corp/*", "@corp/ui"));
        assert!(!yarn_matches("@corp/*", "@other/ui"));
        assert!(yarn_matches("*-plugin", "eslint-plugin"));
        assert!(yarn_matches("*", "anything"));
        assert!(yarn_matches("eslint-*", "eslint-"));
        assert!(yarn_matches("*a*b*", "xaybz"));
        assert!(!yarn_matches("*-plugin", "@corp/x-plugin"));
        assert!(!yarn_matches("*", "@corp/ui"));
        assert!(yarn_matches("@*/ui", "@corp/ui"));
    }

    /// An entry splits at the `@` after its scope, and only a literal name with exact versions
    /// counts as an exact admission.
    #[test]
    fn exact_entries_are_literal_names_with_exact_versions() {
        fn exact(entry: &str) -> Option<Vec<&str>> {
            Entry::parse(entry)?.exact_versions()
        }
        assert_eq!(exact("is-number@7.0.0"), Some(vec!["7.0.0"]));
        assert_eq!(exact("@scope/a@1.2.2||2.0.1"), Some(vec!["1.2.2", "2.0.1"]));
        assert_eq!(
            exact("a@1.0.0 || 2.0.0-rc.1"),
            Some(vec!["1.0.0", "2.0.0-rc.1"])
        );
        for entry in [
            "a",
            "@scope/*",
            "a@^1.2.2",
            "a@1.2.2||*",
            "a-*@1.0.0",
            "a@",
            "@1.2.2",
            "${PKG}@1.0.0",
        ] {
            assert_eq!(exact(entry), None, "{entry}");
        }

        // Yarn reads a range only after the scope's own `@`, and no descriptor from a bare `@`.
        assert_eq!(
            Entry::parse("@scope/a@^1"),
            Some(Entry {
                ident: "@scope/a",
                range: Some("^1"),
            })
        );
        assert_eq!(
            Entry::parse("@scope/a"),
            Some(Entry {
                ident: "@scope/a",
                range: None,
            })
        );
        assert_eq!(Entry::parse("@*"), None);
    }
}
