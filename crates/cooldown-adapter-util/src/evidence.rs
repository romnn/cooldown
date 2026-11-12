//! Native evidence declarations shared by adapters with one authoritative lock document.

use camino::Utf8Path;
use cooldown_core::Project;
use cooldown_core::trace::{Evidence, EvidenceInput, GraphCoverage, GraphSource, InputRole};

/// Declares a project's exact manifest, native graph, and explicitly named configuration files.
///
/// Graph availability is determined by successful artifact capture, including when a lock is absent.
/// Callers add their workspace and external input closure using their own native inventory.
#[must_use]
pub fn native(
    project: &Project,
    lockfile: &str,
    coverage: GraphCoverage,
    configs: &[&str],
) -> Evidence {
    let graph = project.root.join(lockfile);
    let mut evidence = Evidence {
        inputs: vec![
            EvidenceInput {
                path: project.manifest.clone(),
                role: InputRole::Manifest,
            },
            EvidenceInput {
                path: graph.clone(),
                role: InputRole::Graph,
            },
        ],
        graphs: vec![GraphSource {
            path: graph,
            format: lockfile.into(),
            coverage,
        }],
        limitations: Vec::new(),
        environment: Vec::new(),
    };
    for config in configs {
        evidence.inputs.push(EvidenceInput {
            path: project.root.join(config),
            role: InputRole::Configuration,
        });
    }
    evidence
        .inputs
        .sort_by(|left, right| left.path.cmp(&right.path));
    evidence
        .inputs
        .dedup_by(|left, right| left.path == right.path);
    evidence
}

/// Adds exact ancestor configuration candidates in native search order, including absent files.
pub fn ancestors(evidence: &mut Evidence, root: &Utf8Path, names: &[&str]) {
    for directory in root.ancestors() {
        for name in names {
            let path = directory.join(name);
            if !evidence.inputs.iter().any(|input| input.path == path) {
                evidence.inputs.push(EvidenceInput {
                    path,
                    role: InputRole::Configuration,
                });
            }
        }
    }
}

/// Finds candidate manifests without following directory links or entering build and vendor trees.
///
/// This enumerates candidates only; each adapter still decides native workspace ownership.
///
/// # Errors
///
/// Returns the filesystem error for an unreadable directory or non-UTF-8 path.
pub fn candidates(
    root: &Utf8Path,
    names: &[&str],
) -> cooldown_core::Result<Vec<camino::Utf8PathBuf>> {
    let mut pending = vec![root.to_owned()];
    let mut paths = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = camino::Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                cooldown_core::CoreError::Filesystem(format!(
                    "non-UTF-8 input path: {}",
                    path.display()
                ))
            })?;
            let kind = entry.file_type()?;
            let name = path.file_name().unwrap_or("");
            if kind.is_dir() {
                if !name.starts_with('.')
                    && !matches!(
                        name,
                        "target" | "node_modules" | "vendor" | "build" | "dist" | "__pycache__"
                    )
                {
                    pending.push(path);
                }
            } else if names.contains(&name) {
                paths.push(path);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

/// Records explicitly named ambient native settings without inspecting unrelated variables.
pub fn environment(evidence: &mut Evidence, names: &[&str]) {
    evidence.environment.extend(names.iter().map(|name| {
        cooldown_core::trace::EnvironmentOverride {
            name: (*name).to_string(),
            value: std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()),
        }
    }));
}
