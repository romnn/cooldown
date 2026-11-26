//! Parsing `mix.lock` (the resolved graph) and `mix.exs` (the direct deps).
//!
//! `mix.lock` is an Elixir map literal with one entry per line, keyed by the app name Mix gives the
//! dependency.
//! A Hex entry is the tuple `{:hex, :package, "version", "inner checksum", [managers], [deps],
//! "repo", "outer checksum"}`, whose package differs from the app name for an aliased dependency
//! (`{:parsec, "~> 1.2", hex: :nimble_parsec}`).
//! `mix.lock` does not record which deps are direct, so that split is recovered from the `deps`
//! list in `mix.exs`.

use std::collections::{BTreeMap, HashMap, HashSet};

/// The Hex release a `mix.lock` entry locks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HexPin {
    /// The Hex package, which an aliased dependency names apart from its app name.
    pub package: String,
    /// The locked version.
    pub version: String,
    /// The repository the release comes from: `hexpm` for the public hex.pm, `hexpm:<org>` for an
    /// organization's private packages, or another configured repository.
    /// It is `None` for an old-format entry that does not record its repository.
    pub repo: Option<String>,
}

impl HexPin {
    /// Whether the release comes from the public hex.pm, the repository whose release history
    /// cooldown reads and whose packages OSV's `Hex` ecosystem describes.
    ///
    /// A private repository's package merely shares the name, and an old-format entry names no
    /// repository at all, so neither is one.
    #[must_use]
    pub fn is_hexpm(&self) -> bool {
        self.repo.as_deref() == Some(crate::registry::HEXPM)
    }
}

/// Every entry of a `mix.lock`, keyed by the app name Mix gives the dependency, with the Hex
/// release it locks, or `None` for a Git or other non-Hex source.
///
/// A line that is not an entry, such as the map's braces, is skipped.
#[must_use]
pub fn parse(content: &str) -> BTreeMap<String, Option<HexPin>> {
    content.lines().filter_map(parse_entry).collect()
}

/// Parses one lock line `"app": {…},` into its app name and the Hex release it locks.
fn parse_entry(line: &str) -> Option<(String, Option<HexPin>)> {
    let (app, rest) = line.trim().strip_prefix('"')?.split_once("\":")?;
    let elements = tuple_elements(rest.trim_start())?;
    let hex = match elements.as_slice() {
        [":hex", package, version, rest @ ..] => Some(HexPin {
            package: package.trim_start_matches(':').to_owned(),
            version: unquote(version)?.to_owned(),
            // The repository is the seventh element, after the checksum, the managers and the
            // dependencies.
            repo: rest
                .get(3)
                .and_then(|repo| unquote(repo))
                .map(str::to_owned),
        }),
        _ => None,
    };
    Some((app.to_owned(), hex))
}

/// The top-level elements, trimmed, of the tuple that `text` opens with.
///
/// Commas inside a nested list, tuple or string do not separate elements.
fn tuple_elements(text: &str) -> Option<Vec<&str>> {
    let body = text.strip_prefix('{')?;
    let mut elements = Vec::new();
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, character) in body.char_indices() {
        if in_string {
            match character {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' if depth == 0 => {
                elements.push(body.get(start..index)?.trim());
                return Some(elements);
            }
            '}' | ']' => depth = depth.checked_sub(1)?,
            ',' if depth == 0 => {
                elements.push(body.get(start..index)?.trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    None
}

/// The contents of the Elixir string literal `element`.
fn unquote(element: &str) -> Option<&str> {
    element.strip_prefix('"')?.strip_suffix('"')
}

/// The graph ceiling for each dependency some *requirer* pins exactly in `mix.lock`. Every lock
/// entry's tuple carries the requirer's dependency list `[{:dep, "requirement", [hex: :dep]}, …]`; a
/// lone `== X` requirement caps that dep at `X` — the upgrade-direction mirror of a floor. Hex
/// resolves one version per name, so a name-keyed map suffices. Ranges (`~> 1.0`, `>= 1`) and
/// compound requirements (`>= 1 and < 2`) impose none and are skipped. Direct `mix.exs` pins are out
/// of scope (a `pinned` concern).
#[must_use]
pub fn graph_ceilings(content: &str) -> HashMap<String, String> {
    let mut ceilings = HashMap::new();
    for line in content.lines() {
        for pin in exact_dep_pins(line.trim()) {
            ceilings.insert(pin.name.to_string(), pin.version.to_string());
        }
    }
    ceilings
}

/// An exact (`== X`) dependency edge on a `mix.lock` line, borrowing from that line.
struct ExactDepPin<'a> {
    /// The dependency's name (the tuple's leading atom).
    name: &'a str,
    /// The version the requirer pins the dependency to.
    version: &'a str,
}

/// Every exact (`== X`) dependency edge on a `mix.lock` line.
/// A dependency tuple is `{:name, "requirement", …}`: an atom immediately followed by a quoted
/// requirement.
/// This distinguishes it from the entry's own opening tuple `{:hex, :name, "version", …}` (whose
/// second element is another atom, not a quoted string), so that is never mistaken for a dependency
/// edge.
fn exact_dep_pins(line: &str) -> Vec<ExactDepPin<'_>> {
    let mut pins = Vec::new();
    for piece in line.split("{:").skip(1) {
        let name_len = piece
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(piece.len());
        let name = &piece[..name_len];
        if name.is_empty() {
            continue;
        }
        let Some(after) = piece[name_len..].strip_prefix(", \"") else {
            continue; // not a `{:name, "requirement"}` edge (e.g. the entry's `{:hex, :name,` open)
        };
        let Some(end) = after.find('"') else {
            continue;
        };
        if let Some(version) = exact_requirement(&after[..end]) {
            pins.push(ExactDepPin { name, version });
        }
    }
    pins
}

/// The version from a lone exact Hex requirement (`== X`), or `None` for a range (`~> 1.0`, `>= 1`),
/// a compound requirement (`>= 1 and < 2`, `1 or 2`), or a wildcard.
fn exact_requirement(requirement: &str) -> Option<&str> {
    let requirement = requirement.trim();
    if requirement.contains(" and ") || requirement.contains(" or ") || requirement.contains(',') {
        return None;
    }
    let version = requirement.strip_prefix("==")?.trim();
    (!version.is_empty() && !version.contains('*')).then_some(version)
}

/// Returns the set of dependency names declared directly in `mix.exs`, read from its `deps`
/// function. Each dep is an Elixir tuple `{:name, ...}`, so the names are the atoms opening a tuple
/// within the `deps do … end` block.
#[must_use]
pub fn parse_direct(manifest: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(block) = deps_block(manifest) else {
        return out;
    };
    let mut rest = block;
    while let Some(pos) = rest.find("{:") {
        let ident: String = rest[pos + 2..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !ident.is_empty() {
            out.insert(ident);
        }
        rest = &rest[pos + 2..];
    }
    out
}

/// Extracts the body of the `deps` function: the text between `deps do` and the function's closing
/// `end`. Returns `None` if no such function is found.
fn deps_block(src: &str) -> Option<&str> {
    let start = src.find("deps do")?;
    let after = &src[start..];
    let end = after
        .find("\n  end")
        .or_else(|| after.find("\nend"))
        .unwrap_or(after.len());
    Some(&after[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    const MIX_EXS: &str = "defmodule Demo.MixProject do\n  use Mix.Project\n  def project do\n    [app: :demo, deps: deps()]\n  end\n  defp deps do\n    [\n      {:jason, \"~> 1.4\"},\n      {:plug, \"~> 1.14\"}\n    ]\n  end\nend\n";

    fn hex(package: &str, version: &str, repo: Option<&str>) -> HexPin {
        HexPin {
            package: package.to_owned(),
            version: version.to_owned(),
            repo: repo.map(str::to_owned),
        }
    }

    /// Each entry is keyed by its app name and read by tuple position, so an alias keeps its
    /// package apart from its app name, a requirement string is never taken for the repository,
    /// and an entry from a private repository, an old-format entry and a Git checkout are told
    /// apart from a hex.pm release.
    #[test]
    fn entries_are_read_by_tuple_position() {
        let lock = indoc! {r#"
            %{
              "parsec": {:hex, :nimble_parsec, "1.4.0", "abc", [:mix], [], "hexpm", "def"},
              "plug": {:hex, :plug, "1.14.0", "ghi", [:mix], [{:mime, "~> 1.0", [hex: :mime, repo: "hexpm", optional: false]}], "hexpm", "jkl"},
              "private": {:hex, :private, "0.1.0", "abc", [:mix], [], "hexpm:acme", "def"},
              "legacy": {:hex, :legacy, "0.2.0", "abc", [:mix], [{:mime, "~> 1.0", [hex: :mime]}]},
              "local": {:git, "https://example.com/local.git", "0123abc", []},
            }
        "#};

        let entries = parse(lock);

        assert_eq!(
            entries,
            BTreeMap::from([
                (
                    "parsec".to_owned(),
                    Some(hex("nimble_parsec", "1.4.0", Some("hexpm")))
                ),
                (
                    "plug".to_owned(),
                    Some(hex("plug", "1.14.0", Some("hexpm")))
                ),
                (
                    "private".to_owned(),
                    Some(hex("private", "0.1.0", Some("hexpm:acme")))
                ),
                ("legacy".to_owned(), Some(hex("legacy", "0.2.0", None))),
                ("local".to_owned(), None),
            ])
        );
        // Only the public hex.pm's releases count as hex.pm packages.
        let hexpm: Vec<&str> = entries
            .iter()
            .filter(|(_, pin)| pin.as_ref().is_some_and(HexPin::is_hexpm))
            .map(|(app, _)| app.as_str())
            .collect();
        assert_eq!(hexpm, ["parsec", "plug"]);
    }

    #[test]
    fn direct_reads_deps_function() {
        let direct = parse_direct(MIX_EXS);
        assert!(direct.contains("jason"));
        assert!(direct.contains("plug"));
        assert!(!direct.contains("mime")); // only inside the lock as a transitive
    }

    #[test]
    fn graph_ceilings_records_only_exact_requirement_edges() {
        // `app` pins `protobuf` exactly (`== 6.33.5`, a ceiling) and `mime` as a range; `plug` pins
        // `cowboy` with a compound requirement — neither range names a ceiling.
        let lock = "%{\n  \"app\": {:hex, :app, \"1.0.0\", \"h1\", [:mix], [{:protobuf, \"== 6.33.5\", [hex: :protobuf]}, {:mime, \"~> 1.0\", [hex: :mime]}], \"hexpm\", \"h2\"},\n  \"plug\": {:hex, :plug, \"1.14.0\", \"g\", [:mix], [{:cowboy, \">= 2.7 and < 3.0\", [hex: :cowboy]}], \"hexpm\", \"j\"},\n}\n";
        let ceilings = graph_ceilings(lock);
        assert_eq!(ceilings.get("protobuf").map(String::as_str), Some("6.33.5"));
        assert_eq!(ceilings.get("mime"), None); // `~> 1.0` is a range
        assert_eq!(ceilings.get("cowboy"), None); // compound requirement
        assert_eq!(ceilings.len(), 1);
    }

    #[test]
    fn exact_requirement_recognises_only_a_lone_double_equals() {
        assert_eq!(exact_requirement("== 6.33.5"), Some("6.33.5"));
        assert_eq!(exact_requirement("==6.33.5"), Some("6.33.5"));
        assert_eq!(exact_requirement("~> 1.0"), None);
        assert_eq!(exact_requirement(">= 1.0"), None);
        assert_eq!(exact_requirement(">= 1.0 and < 2.0"), None);
    }
}
