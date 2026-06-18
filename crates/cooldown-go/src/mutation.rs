use crate::semver;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{Change, CoreError, Plan, ProjectMutationFile, ProjectMutationJournal, Result};
use std::collections::HashSet;

pub(crate) fn mutation_journal(root: &Utf8Path, plan: &Plan) -> Result<ProjectMutationJournal> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for rel in [Utf8Path::new("go.mod"), Utf8Path::new("go.sum")] {
        if let Some(file) = capture_once(root, rel, &mut seen)? {
            files.push(file);
        }
    }
    for change in &plan.changes {
        let Some(old_path) = old_import_path(change) else {
            continue;
        };
        if old_path == change.package.name {
            continue;
        }
        capture_import_targets(root, root, &old_path, &mut seen, &mut files)?;
    }
    Ok(ProjectMutationJournal { files })
}

pub(crate) fn rewrite_imports(
    root: &Utf8Path,
    old: &str,
    new: &str,
    journal: &ProjectMutationJournal,
) -> Result<usize> {
    let mut count = 0;
    for file in &journal.files {
        if file.path.extension() != Some("go") {
            continue;
        }
        let path = root.join(&file.path);
        let src = std::fs::read_to_string(&path)?;
        let replaced = rewrite_import_path(&src, old, new);
        if replaced != src {
            std::fs::write(&path, replaced)?;
            count += 1;
        }
    }
    Ok(count)
}

pub(crate) fn old_import_path(change: &Change) -> Option<String> {
    let new_path = &change.package.name;
    let (prefix, _, ok) = semver::split_path_version(new_path);
    if !ok {
        return None;
    }
    let from_major = semver::major(change.from.as_str());
    let n: u32 = from_major.trim_start_matches('v').parse().ok()?;
    if n <= 1 {
        Some(prefix)
    } else {
        Some(semver::major_path(&prefix, n))
    }
}

fn capture_import_targets(
    root: &Utf8Path,
    dir: &Utf8Path,
    old: &str,
    seen: &mut HashSet<Utf8PathBuf>,
    out: &mut Vec<ProjectMutationFile>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), "vendor" | ".git" | "testdata") {
                continue;
            }
            let child = utf8_path(path)?;
            capture_import_targets(root, &child, old, seen, out)?;
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("go") {
            continue;
        }
        let utf8 = utf8_path(path)?;
        let source = std::fs::read_to_string(&utf8)?;
        if !contains_import_target(&source, old) {
            continue;
        }
        let rel = utf8
            .strip_prefix(root)
            .map_err(|e| CoreError::PathEncoding(format!("{utf8}: {e}")))?;
        if let Some(file) = capture_once(root, rel, seen)? {
            out.push(file);
        }
    }
    Ok(())
}

fn capture_once(
    root: &Utf8Path,
    rel: &Utf8Path,
    seen: &mut HashSet<Utf8PathBuf>,
) -> Result<Option<ProjectMutationFile>> {
    let rel = rel.to_owned();
    if !seen.insert(rel.clone()) {
        return Ok(None);
    }
    Ok(Some(ProjectMutationJournal::capture_file(root, &rel)?))
}

fn utf8_path(path: std::path::PathBuf) -> Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(path)
        .map_err(|path| CoreError::PathEncoding(format!("{} is not valid UTF-8", path.display())))
}

fn contains_import_target(source: &str, old: &str) -> bool {
    source.contains(&format!("\"{old}\"")) || source.contains(&format!("\"{old}/"))
}

fn rewrite_import_path(source: &str, old: &str, new: &str) -> String {
    source
        .replace(&format!("\"{old}\""), &format!("\"{new}\""))
        .replace(&format!("\"{old}/"), &format!("\"{new}/"))
}
