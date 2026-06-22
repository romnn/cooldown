//! Parsing `mix.lock` (the resolved graph) and `mix.exs` (the direct deps). `mix.lock` is an Elixir
//! map literal whose every entry is `"name": {:hex, :name, "version", ...}`; the third tuple element
//! is the resolved version, and a `:hex` source tag marks it as coming from hex.pm. `mix.lock` does
//! not record which deps are direct, so that split is recovered from the `deps` list in `mix.exs`.

use std::collections::{HashMap, HashSet};

/// Returns the resolved `(name, version)` of every hex.pm-sourced entry in a `mix.lock`. Non-hex
/// sources (`:git`, `:path`) are skipped, as cooldown has no registry publish time for them.
#[must_use]
pub fn parse_resolved(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        if let Some(entry) = parse_entry(line.trim()) {
            out.push(entry);
        }
    }
    out
}

/// Parses one lock line `"name": {:hex, :name, "version", ...}`. The version is the first quoted
/// string inside the tuple (the atoms before it are unquoted).
fn parse_entry(line: &str) -> Option<(String, String)> {
    let line = line.strip_prefix('"')?;
    let (name, after) = line.split_once("\":")?;
    let after = after.trim_start();
    if !after.starts_with("{:hex") {
        return None; // only registry-backed (hex.pm) packages have a publish time to reason about
    }
    let start = after.find('"')?;
    let rest = after.get(start + 1..)?;
    let end = rest.find('"')?;
    Some((name.to_string(), rest.get(..end)?.to_string()))
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
        for (name, version) in exact_dep_pins(line.trim()) {
            ceilings.insert(name.to_string(), version.to_string());
        }
    }
    ceilings
}

/// The `(name, version)` of every exact (`== X`) dependency edge on a `mix.lock` line. A dependency
/// tuple is `{:name, "requirement", …}`: an atom immediately followed by a quoted requirement. This
/// distinguishes it from the entry's own opening tuple `{:hex, :name, "version", …}` (whose second
/// element is another atom, not a quoted string), so that is never mistaken for a dependency edge.
fn exact_dep_pins(line: &str) -> Vec<(&str, &str)> {
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
            pins.push((name, version));
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

    const LOCK: &str = "%{\n  \"jason\": {:hex, :jason, \"1.4.0\", \"abc\", [:mix], [], \"hexpm\", \"def\"},\n  \"plug\": {:hex, :plug, \"1.14.0\", \"ghi\", [:mix], [{:mime, \"~> 1.0\", [hex: :mime]}], \"hexpm\", \"jkl\"},\n  \"local\": {:path, \"../local\"},\n}\n";

    const MIX_EXS: &str = "defmodule Demo.MixProject do\n  use Mix.Project\n  def project do\n    [app: :demo, deps: deps()]\n  end\n  defp deps do\n    [\n      {:jason, \"~> 1.4\"},\n      {:plug, \"~> 1.14\"}\n    ]\n  end\nend\n";

    #[test]
    fn resolved_skips_non_hex_sources() {
        let mut got = parse_resolved(LOCK);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("jason".to_string(), "1.4.0".to_string()),
                ("plug".to_string(), "1.14.0".to_string()),
            ]
        );
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
