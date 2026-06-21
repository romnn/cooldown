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
    let (prefix, path_major, ok) = semver::split_path_version(new_path);
    // `ok` only means the path is well-formed; `path_major` is empty when the new path carries no
    // `/vN` suffix. A module with no suffix is not path-versioned — a `+incompatible` module like
    // `github.com/docker/cli` stays on one import path across its v2+ majors — so there is nothing
    // to rewrite. Without this guard the v2+ `from` major would synthesize a bogus `…/vN` old path
    // and trigger a spurious import-tree scan.
    if !ok || path_major.is_empty() {
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
            // Skip exactly what the `go` tool itself ignores: directories whose names begin with `.`
            // or `_` (so `.git`, `_data` container volumes, `_cache`, … are never descended into),
            // plus `vendor` and `testdata`. Beyond matching Go's own package discovery, this keeps
            // the scan off non-source trees that may be large or unreadable (e.g. a Docker volume
            // owned by another user), which would otherwise fail the whole upgrade.
            if name.starts_with('.')
                || name.starts_with('_')
                || matches!(name.as_ref(), "vendor" | "testdata")
            {
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

#[cfg(test)]
mod tests {
    use super::*;
    use cooldown_core::{PackageId, ToolId, UpdateKind, Version};

    fn change(name: &str, from: &str, to: &str) -> Change {
        Change {
            package: PackageId::new(ToolId("go"), name, None),
            from: Version::new(from),
            to: Version::new(to),
            kind: UpdateKind::Minor,
            direct: true,
            members: Vec::new(),
        }
    }

    #[test]
    fn incompatible_module_has_no_import_path_to_rewrite() {
        // A `+incompatible` module (a v2+ major that never adopted `/vN` paths, e.g.
        // github.com/docker/cli) stays on one import path across its majors, so a within-line bump
        // must not synthesize a bogus `…/v29` old path — doing so would trigger a spurious,
        // potentially failing import-tree scan.
        assert_eq!(
            old_import_path(&change(
                "github.com/docker/cli",
                "v29.2.1+incompatible",
                "v29.5.2+incompatible",
            )),
            None,
        );
    }

    #[test]
    fn import_scan_skips_dot_underscore_vendor_and_testdata_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8 root");
        let src = "package x\nimport \"example.com/foo/v2\"\n";
        std::fs::write(root.join("main.go"), src).expect("write main.go");
        // Each of these directories would match the import too, but `go` (and so cooldown) ignores
        // them — `_data` stands in for an unreadable Docker volume that must not break the scan.
        for skip in ["_data", ".git", "vendor", "testdata"] {
            std::fs::create_dir_all(root.join(skip)).expect("mkdir");
            std::fs::write(root.join(skip).join("x.go"), src).expect("write skipped .go");
        }

        let mut seen = HashSet::new();
        let mut out = Vec::new();
        capture_import_targets(root, root, "example.com/foo/v2", &mut seen, &mut out)
            .expect("scan succeeds");

        let captured: Vec<&str> = out.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            captured,
            vec!["main.go"],
            "only top-level source is captured"
        );
    }

    #[test]
    fn versioned_module_rewrites_from_the_old_major_path() {
        // A real `/vN` module crossing majors: the old import path drops to the `from` major.
        assert_eq!(
            old_import_path(&change("example.com/foo/v3", "v2.4.0", "v3.0.0")).as_deref(),
            Some("example.com/foo/v2"),
        );
        // From v1, the old path is the unversioned base.
        assert_eq!(
            old_import_path(&change("example.com/foo/v2", "v1.5.0", "v2.0.0")).as_deref(),
            Some("example.com/foo"),
        );
    }
}
