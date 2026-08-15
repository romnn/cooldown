//! Parsing `Gemfile.lock`. Bundler's lock is an indented, section-based text format: the `GEM`
//! section's `specs:` block lists every resolved gem as `    name (version)` (with its own
//! dependencies indented further), and the top-level `DEPENDENCIES` section names the gems the
//! `Gemfile` declares directly.

use std::collections::{HashMap, HashSet};

/// A gem and the exact version a `Gemfile.lock` `specs:` line resolves it to.
#[derive(Debug, PartialEq)]
pub struct ResolvedPin {
    /// The gem name.
    pub name: String,
    /// The resolved version, with any platform suffix dropped.
    pub version: String,
    /// Whether the gem's section is served by the public rubygems.org remote — the registry
    /// behind OSV's `RubyGems` ecosystem. `false` for a private/alternate `GEM` remote and for
    /// `GIT`/`PATH` sections, whose gems merely share a name with the public one.
    pub rubygems: bool,
}

/// Returns the [`ResolvedPin`] of every gem in a `specs:` block.
/// Nested lines (a gem's own dependencies, indented past four spaces) are skipped, as are non-spec
/// lines.
#[must_use]
pub fn parse_resolved(content: &str) -> Vec<ResolvedPin> {
    let mut out = Vec::new();
    let mut in_specs = false;
    // The `remote:` lines of the current top-level section, seen before its `specs:` block. A
    // section provably serves from rubygems.org only when every one of its remotes is it: the
    // lock does not say which remote served which gem.
    let mut remotes: Vec<String> = Vec::new();
    for line in content.lines() {
        if line.starts_with("  specs:") {
            in_specs = true;
            continue;
        }
        // Any non-indented, non-blank line starts a new top-level section, ending the specs block.
        if !line.starts_with(' ') && !line.trim().is_empty() {
            in_specs = false;
            remotes.clear();
        }
        if let Some(remote) = line.strip_prefix("  remote: ") {
            remotes.push(remote.trim().to_string());
        }
        if !in_specs {
            continue;
        }
        if let Some(rest) = line.strip_prefix("    ") {
            if rest.starts_with(' ') {
                continue; // a gem's own dependency, indented further
            }
            if let Some(spec) = parse_spec(rest) {
                let rubygems = !remotes.is_empty() && remotes.iter().all(|r| is_rubygems_remote(r));
                out.push(ResolvedPin { rubygems, ..spec });
            }
        }
    }
    out
}

/// Whether `remote` is the public rubygems.org registry.
fn is_rubygems_remote(remote: &str) -> bool {
    remote
        .trim_end_matches('/')
        .eq_ignore_ascii_case("https://rubygems.org")
}

/// Parses a single `name (version)` spec line, dropping any platform suffix on the version
/// (`1.2.3-x86_64-linux` → `1.2.3`).
fn parse_spec(line: &str) -> Option<ResolvedPin> {
    let line = line.trim_end();
    let open = line.find(" (")?;
    let name = &line[..open];
    let version = line.get(open + 2..)?.strip_suffix(')')?;
    // A native-extension gem records `version-platform`; cooldown reasons about the version alone.
    let version = version.split('-').next().unwrap_or(version);
    Some(ResolvedPin {
        name: name.to_string(),
        version: version.to_string(),
        // Provenance is a section-level fact the caller stamps; a lone spec line proves nothing.
        rubygems: false,
    })
}

/// The graph ceiling for each gem some *requirer* pins exactly in the `specs:` block: a nested
/// `dependency (= X)` line caps that gem at `X` — the upgrade-direction mirror of a floor. Bundler
/// resolves one version per gem, so a name-keyed map suffices. Only a lone `= X` names a ceiling;
/// ranges (`~> 1.4`, `>= 1`, `< 2`) and compound requirements impose none and are skipped. Direct
/// `Gemfile` pins live in the `DEPENDENCIES` section, not here, and are out of scope (a `pinned`
/// concern).
#[must_use]
pub fn graph_ceilings(content: &str) -> HashMap<String, String> {
    let mut ceilings = HashMap::new();
    let mut in_specs = false;
    for line in content.lines() {
        if line.starts_with("  specs:") {
            in_specs = true;
            continue;
        }
        if !line.starts_with(' ') && !line.trim().is_empty() {
            in_specs = false;
        }
        if !in_specs {
            continue;
        }
        // A gem's own dependency edge is indented past the 4-space spec indent: `      dep (= X)`.
        let Some(rest) = line.strip_prefix("    ") else {
            continue;
        };
        if !rest.starts_with(' ') {
            continue; // the spec line itself (the requirer), not one of its dependency edges
        }
        if let Some(pin) = exact_dep_pin(rest.trim()) {
            ceilings.insert(pin.name.to_string(), pin.version.to_string());
        }
    }
    ceilings
}

/// An exactly pinned (`= X`) gem dependency edge, borrowing from its lock line.
#[derive(Debug, PartialEq)]
struct ExactDepPin<'a> {
    /// The dependency's gem name.
    name: &'a str,
    /// The version the requirer pins the gem to.
    version: &'a str,
}

/// The [`ExactDepPin`] of a gem dependency edge that pins exactly — `foo (= 1.2.3)` — or `None` for
/// an unconstrained edge (`foo`), a range (`foo (~> 1.4)`, `foo (>= 1, < 2)`), or a malformed line.
fn exact_dep_pin(line: &str) -> Option<ExactDepPin<'_>> {
    let open = line.find(" (")?;
    let name = &line[..open];
    let requirement = line.get(open + 2..)?.strip_suffix(')')?;
    if requirement.contains(',') {
        return None; // a compound requirement (`>= 1, < 2`) is not an exact pin
    }
    let version = requirement.trim().strip_prefix('=')?.trim();
    (!version.is_empty()).then_some(ExactDepPin { name, version })
}

/// Returns the set of gem names declared directly in the `Gemfile`, read from the lock's
/// `DEPENDENCIES` section. A trailing `!` (a gem pinned to a git/path source) is stripped.
#[must_use]
pub fn parse_direct(content: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut in_deps = false;
    for line in content.lines() {
        if line == "DEPENDENCIES" {
            in_deps = true;
            continue;
        }
        if !line.starts_with(' ') && !line.trim().is_empty() {
            in_deps = false;
        }
        if !in_deps {
            continue;
        }
        if let Some(rest) = line.strip_prefix("  ") {
            let name = rest.split([' ', '!']).next().unwrap_or("").trim();
            if !name.is_empty() {
                out.insert(name.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = "GEM\n  remote: https://rubygems.org/\n  specs:\n    nokogiri (1.13.0)\n      racc (~> 1.4)\n    racc (1.6.0)\n    rake (13.0.6)\n\nPLATFORMS\n  ruby\n\nDEPENDENCIES\n  nokogiri\n  rake (~> 13.0)\n\nBUNDLED WITH\n   2.3.0\n";

    fn pin(name: &str, version: &str) -> ResolvedPin {
        ResolvedPin {
            name: name.to_string(),
            version: version.to_string(),
            rubygems: true,
        }
    }

    #[test]
    fn resolved_skips_nested_deps() {
        let mut got = parse_resolved(LOCK);
        got.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            got,
            vec![
                pin("nokogiri", "1.13.0"),
                pin("racc", "1.6.0"),
                pin("rake", "13.0.6"),
            ]
        );
    }

    #[test]
    fn direct_reads_dependencies_section() {
        let direct = parse_direct(LOCK);
        assert!(direct.contains("nokogiri"));
        assert!(direct.contains("rake"));
        assert!(!direct.contains("racc")); // transitive only
    }

    #[test]
    fn graph_ceilings_records_only_exact_nested_pins() {
        // `rails` pins `actioncable (= 7.0.0)` exactly (a ceiling) and `rake (~> 13.0)` as a range;
        // `racc` pins `nokogiri (>= 1.13, < 2.0)` as a compound range — neither names a ceiling.
        let lock = "GEM\n  remote: https://rubygems.org/\n  specs:\n    rails (7.0.0)\n      actioncable (= 7.0.0)\n      rake (~> 13.0)\n    actioncable (7.0.0)\n    racc (1.6.0)\n      nokogiri (>= 1.13, < 2.0)\n\nDEPENDENCIES\n  rails\n";
        let ceilings = graph_ceilings(lock);
        assert_eq!(
            ceilings.get("actioncable").map(String::as_str),
            Some("7.0.0")
        );
        assert_eq!(ceilings.get("rake"), None); // range, not exact
        assert_eq!(ceilings.get("nokogiri"), None); // compound range
        assert_eq!(ceilings.len(), 1);
    }

    #[test]
    fn exact_dep_pin_recognises_only_a_lone_equals() {
        assert_eq!(
            exact_dep_pin("foo (= 1.2.3)"),
            Some(ExactDepPin {
                name: "foo",
                version: "1.2.3"
            })
        );
        assert_eq!(exact_dep_pin("foo (~> 1.4)"), None);
        assert_eq!(exact_dep_pin("foo (>= 1.0)"), None);
        assert_eq!(exact_dep_pin("foo (>= 1, < 2)"), None);
        assert_eq!(exact_dep_pin("foo"), None);
    }
}
