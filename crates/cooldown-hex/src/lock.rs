//! Parsing `mix.lock` (the resolved graph) and `mix.exs` (the direct deps). `mix.lock` is an Elixir
//! map literal whose every entry is `"name": {:hex, :name, "version", ...}`; the third tuple element
//! is the resolved version, and a `:hex` source tag marks it as coming from hex.pm. `mix.lock` does
//! not record which deps are direct, so that split is recovered from the `deps` list in `mix.exs`.

use std::collections::HashSet;

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
}
