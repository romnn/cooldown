use crate::gocmd::Go;
use crate::mutation;
use crate::semver;
use camino::Utf8Path;
use cooldown_core::{
    ApplyReport, Change, PackageId, Plan, Project, ProjectMutationJournal, Result, SkipReason,
    Skipped, UpdateKind, Version,
};
use std::collections::HashMap;

use super::GO_ID;

pub(super) async fn apply(
    go: &Go,
    project: &Project,
    plan: &Plan,
    journal: &ProjectMutationJournal,
) -> Result<ApplyReport> {
    // Go's `go.mod` *is* the version source (the MVS minimum), so `go get <path>@<version>` always
    // rewrites it — there is no separate lock to move within an unchanged constraint. The
    // `plan.rewrite` mode is therefore not consulted: both modes rewrite `go.mod`. A cross-major bump
    // additionally rewrites the `/vN` import paths below.
    let mut report = ApplyReport::default();
    if plan.changes.is_empty() {
        return Ok(report);
    }

    // The pre-apply `go.mod` requires, taken from the journal (`mutation_journal` captured `go.mod`
    // before the re-resolve). The whole-graph `go get` emits one consistent `go.mod`; the report is
    // the diff of this snapshot against the result, so *every* net version change is surfaced — the
    // planned moves, the MVS floor-raises another candidate forced, and the indirect churn `go mod
    // tidy` introduced. A missing/unparsable snapshot leaves `before` empty, so a module that moved
    // is still reported (never silent).
    let before = journal
        .files
        .iter()
        .find(|file| file.path == Utf8Path::new("go.mod"))
        .and_then(|file| file.contents.as_deref())
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(parse_requires)
        .unwrap_or_default();

    // The whole graph is settled in one MVS pass: every planned `module@version` target is handed to
    // a single `go get`, so Go runs one joint resolve over the entire graph rather than a sequence of
    // per-module re-resolves that would silently move other modules between candidates. MVS raises
    // each named module's floor to its cooldown-matured target; there is no `<` upper bound, so the
    // result is the unique fixed point and no candidate can ping-pong another down.
    let targets: Vec<(String, String)> = plan
        .changes
        .iter()
        .map(|change| (change.package.name.clone(), change.to.as_str().to_string()))
        .collect();
    match go.get_many(&project.root, &targets).await {
        Ok(()) => {}
        Err(err) if err.is_tool_spawn_failure() => return Err(err),
        // The joint resolve/compile was rejected (an incompatible API surfaced by `go build` inside
        // `go get`, or an otherwise-unsatisfiable set): every candidate is held. The caller restores
        // the journal, so no partial `go.mod`/`go.sum` is kept.
        Err(_) => {
            for change in &plan.changes {
                report.skipped.push(resolver_conflict(change));
            }
            return Ok(report);
        }
    }

    // Cross-major path changes → rewrite `/vN` import paths old→new for the planned set. The single
    // joint resolve already landed, so this runs once over the whole plan rather than per candidate.
    for change in &plan.changes {
        let target_path = &change.package.name;
        if let Some(old_path) = mutation::old_import_path(change)
            && old_path != *target_path
        {
            mutation::rewrite_imports(&project.root, &old_path, target_path, journal)?;
        }
    }

    // Re-tidy once after the joint resolve: prune/add indirects and sync `go.sum`.
    go.mod_tidy(&project.root).await?;

    // The resolved `go.mod` requires after the joint resolve + tidy.
    let after = parse_requires(&std::fs::read_to_string(project.root.join("go.mod"))?);
    let planned: std::collections::HashSet<&str> = plan
        .changes
        .iter()
        .map(|change| change.package.name.as_str())
        .collect();

    // Each planned candidate either reached cooldown's target (its newest-within-window) — reported
    // applied — or fell short because the joint MVS pass could not place it there — reported held.
    // "Reached" respects the move's direction: a forward candidate must land at or above its target,
    // a downgrade at or below it.
    for change in &plan.changes {
        if reached(&after, change) {
            report.applied.push(change.clone());
        } else {
            report.skipped.push(resolver_conflict(change));
        }
    }

    // The hard requirement: no net version change to *any* module may be omitted. A module the plan
    // did not name that the joint resolve moved — an MVS floor raised by another candidate, or an
    // indirect `go mod tidy` added/changed — is surfaced as its own collateral applied row. This
    // closes the silent-drift gap the per-change loop left: `go mod tidy` and MVS floor-raises could
    // move modules the old per-change report never recorded.
    let mut collateral: Vec<Change> = before
        .iter()
        .filter(|(name, _)| !planned.contains(name.as_str()))
        .filter_map(|(name, from)| {
            let to = after.get(name)?;
            (semver::compare(from, to) != std::cmp::Ordering::Equal)
                .then(|| collateral_change(name, from, to))
        })
        .collect();
    collateral.sort_by(|a, b| a.package.name.cmp(&b.package.name));
    report.applied.extend(collateral);
    Ok(report)
}

/// Whether a planned candidate landed at or beyond its target in `after`, respecting the move's
/// direction: a forward move must reach at/above its target, a downgrade at/below it. A module the
/// resolve dropped from the requires (no entry) counts as not reached.
fn reached(after: &HashMap<String, String>, change: &Change) -> bool {
    after.get(&change.package.name).is_some_and(|landed| {
        let ordering = semver::compare(landed, change.to.as_str());
        if change.downgrade {
            ordering != std::cmp::Ordering::Greater
        } else {
            ordering != std::cmp::Ordering::Less
        }
    })
}

fn resolver_conflict(change: &Change) -> Skipped {
    Skipped {
        change: change.clone(),
        reason: SkipReason::ResolverConflict,
        offending: Some(change.package.clone()),
    }
}

/// A net version change `apply` derived from the before/after `go.mod` diff for a module the plan
/// did not itself name — collateral movement the joint MVS resolve (or `go mod tidy`) forced.
/// Reported so no module's version change is ever silent: an indirect floor raised by another
/// candidate, or a tidy-driven indirect change, surfaces as its own report row.
fn collateral_change(name: &str, from: &str, to: &str) -> Change {
    let downgrade = semver::compare(to, from) == std::cmp::Ordering::Less;
    Change {
        package: PackageId::new(GO_ID, name.to_string(), None),
        from: Version::new(from.to_string()),
        to: Version::new(to.to_string()),
        // A collateral move is transitive consistency churn, not a directly-declared bump; its kind
        // is informational only and `Minor` is the neutral label the renderer shows.
        kind: UpdateKind::Minor,
        downgrade,
        direct: false,
        members: Vec::new(),
    }
}

/// The `module path → version` map of every `require` directive in a `go.mod`, the symmetric
/// before/after snapshot `apply` diffs. Both the single-line form (`require path v1.2.3`) and the
/// grouped block (`require ( … )`) are handled; an inline `// indirect` marker and trailing comments
/// are ignored. `go` owns the canonical `go.mod` format, so this reads only the requires the diff
/// needs.
fn parse_requires(go_mod: &str) -> HashMap<String, String> {
    let mut requires = HashMap::new();
    let mut in_block = false;
    for raw in go_mod.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if in_block {
            if line == ")" {
                in_block = false;
            } else if let Some((path, version)) = parse_require_pair(line) {
                requires.insert(path, version);
            }
            continue;
        }
        if line == "require (" {
            in_block = true;
        } else if let Some(rest) = line.strip_prefix("require ")
            && let Some((path, version)) = parse_require_pair(rest.trim())
        {
            requires.insert(path, version);
        }
    }
    requires
}

/// Drop a trailing `//` line comment (e.g. ` // indirect`), keeping the directive text.
fn strip_comment(line: &str) -> &str {
    match line.find("//") {
        Some(index) => &line[..index],
        None => line,
    }
}

/// Parse a `module/path v1.2.3` pair into `(path, version)`. Returns `None` for a malformed line
/// (no second field, or a second field that is not a valid Go version).
fn parse_require_pair(line: &str) -> Option<(String, String)> {
    let mut fields = line.split_whitespace();
    let path = fields.next()?;
    let version = fields.next()?;
    semver::is_valid(version).then(|| (path.to_string(), version.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_requires_reads_block_and_single_forms_dropping_indirect_marker() {
        let go_mod = "module example.com/demo\n\n\
            go 1.24\n\n\
            require example.com/single v1.2.3\n\n\
            require (\n\
            \texample.com/foo v1.4.0\n\
            \texample.com/bar v0.9.1 // indirect\n\
            \tk8s.io/api v0.30.2\n\
            )\n";
        let requires = parse_requires(go_mod);
        assert_eq!(
            requires.get("example.com/single").map(String::as_str),
            Some("v1.2.3")
        );
        assert_eq!(
            requires.get("example.com/foo").map(String::as_str),
            Some("v1.4.0")
        );
        // The `// indirect` marker is stripped; the require itself is still captured.
        assert_eq!(
            requires.get("example.com/bar").map(String::as_str),
            Some("v0.9.1")
        );
        assert_eq!(
            requires.get("k8s.io/api").map(String::as_str),
            Some("v0.30.2")
        );
        // Non-require directives are ignored.
        assert!(!requires.contains_key("example.com/demo"));
        assert!(!requires.contains_key("go"));
    }

    #[test]
    fn collateral_change_surfaces_an_unplanned_floor_raise() {
        // An indirect the plan never named moved up (an MVS floor a candidate raised): it must be a
        // collateral row, not silent.
        let from = "v0.30.1";
        let to = "v0.30.2";
        let change = collateral_change("k8s.io/apimachinery", from, to);
        assert_eq!(change.package.name, "k8s.io/apimachinery");
        assert_eq!(change.from.as_str(), from);
        assert_eq!(change.to.as_str(), to);
        assert!(!change.downgrade);
        assert!(!change.direct);
    }

    #[test]
    fn collateral_change_marks_a_forced_regression_as_a_downgrade() {
        let change = collateral_change("k8s.io/api", "v0.31.0", "v0.30.2");
        assert!(change.downgrade);
    }

    #[test]
    fn reached_respects_move_direction() {
        let mut after = HashMap::new();
        after.insert("k8s.io/api".to_string(), "v0.30.2".to_string());

        let forward = Change {
            package: PackageId::new(GO_ID, "k8s.io/api", None),
            from: Version::new("v0.30.0"),
            to: Version::new("v0.30.2"),
            kind: UpdateKind::Patch,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        };
        assert!(reached(&after, &forward));

        let forward_short = Change {
            to: Version::new("v0.30.5"),
            ..forward.clone()
        };
        assert!(!reached(&after, &forward_short));

        let downgrade = Change {
            from: Version::new("v0.31.0"),
            to: Version::new("v0.30.2"),
            downgrade: true,
            ..forward.clone()
        };
        assert!(reached(&after, &downgrade));

        let downgrade_short = Change {
            from: Version::new("v0.31.0"),
            to: Version::new("v0.30.0"),
            downgrade: true,
            ..forward
        };
        // Landed at v0.30.2 but the downgrade target was v0.30.0 (lower) — not reached.
        assert!(!reached(&after, &downgrade_short));
    }
}
