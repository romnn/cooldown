//! Parsing `Package.resolved` — `SwiftPM`'s pin file. It records the fully resolved dependency graph
//! as a list of pins, each a git `location` plus a resolved `state.version`. cooldown only reasons
//! about version-pinned, GitHub-hosted dependencies (the publish-time source); branch/revision pins
//! and non-GitHub remotes are skipped. Both the current (v2/v3, top-level `pins`) and legacy (v1,
//! nested `object.pins` with `repositoryURL`) shapes are handled.

use cooldown_core::{CoreError, Result};

#[derive(serde::Deserialize)]
struct Resolved {
    #[serde(default)]
    pins: Vec<Pin>,
    #[serde(default)]
    object: Option<LegacyObject>,
}

#[derive(serde::Deserialize)]
struct LegacyObject {
    #[serde(default)]
    pins: Vec<Pin>,
}

#[derive(serde::Deserialize)]
struct Pin {
    #[serde(default)]
    location: Option<String>,
    #[serde(default, rename = "repositoryURL")]
    repository_url: Option<String>,
    #[serde(default)]
    state: Option<PinState>,
}

#[derive(serde::Deserialize)]
struct PinState {
    #[serde(default)]
    version: Option<String>,
}

/// Parses `Package.resolved` into version-pinned `(owner/repo, version)` pairs for GitHub-hosted
/// dependencies.
///
/// # Errors
///
/// Returns a [`CoreError`] if the file is not valid JSON.
pub fn parse_resolved(content: &str) -> Result<Vec<(String, String)>> {
    let doc: Resolved = serde_json::from_str(content)
        .map_err(|e| CoreError::Parse(format!("Package.resolved: {e}")))?;
    let pins = if doc.pins.is_empty() {
        doc.object.map(|o| o.pins).unwrap_or_default()
    } else {
        doc.pins
    };
    let mut out = Vec::new();
    for pin in pins {
        let location = pin.location.or(pin.repository_url);
        let version = pin.state.and_then(|s| s.version);
        if let (Some(loc), Some(ver)) = (location, version)
            && let Some(repo) = github_repo(&loc)
        {
            out.push((repo, ver));
        }
    }
    Ok(out)
}

/// Extracts `owner/repo` from a GitHub git URL (`https://github.com/apple/swift-log.git`,
/// `git@github.com:apple/swift-log.git`), or `None` for a non-GitHub remote.
fn github_repo(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v3_pins_and_skips_non_github() {
        let resolved = r#"{
            "pins": [
                {
                    "identity": "swift-argument-parser",
                    "kind": "remoteSourceControl",
                    "location": "https://github.com/apple/swift-argument-parser.git",
                    "state": { "version": "1.2.0" }
                },
                {
                    "identity": "internal",
                    "kind": "remoteSourceControl",
                    "location": "https://gitlab.example.com/team/internal.git",
                    "state": { "version": "0.1.0" }
                },
                {
                    "identity": "branchpin",
                    "kind": "remoteSourceControl",
                    "location": "https://github.com/apple/swift-nio.git",
                    "state": { "branch": "main", "revision": "abc" }
                }
            ],
            "version": 3
        }"#;
        assert_eq!(
            parse_resolved(resolved).unwrap(),
            vec![(
                "apple/swift-argument-parser".to_string(),
                "1.2.0".to_string()
            )]
        );
    }

    #[test]
    fn parses_legacy_v1_object() {
        let resolved = r#"{
            "object": {
                "pins": [
                    {
                        "package": "Alamofire",
                        "repositoryURL": "https://github.com/Alamofire/Alamofire.git",
                        "state": { "version": "5.6.0" }
                    }
                ]
            },
            "version": 1
        }"#;
        assert_eq!(
            parse_resolved(resolved).unwrap(),
            vec![("Alamofire/Alamofire".to_string(), "5.6.0".to_string())]
        );
    }
}
