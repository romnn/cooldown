use crate::gocmd::Go;
use crate::gomod::{self, GoModFacts, Require};
use crate::mutation;
use crate::semver;
use cooldown_core::{
    ApplyReport, Change, CoreError, PackageId, Plan, Project, ProjectMutationJournal, Result,
    SkipReason, Skipped, UpdateKind, Version,
};
use std::collections::{HashMap, HashSet};

use super::GO_ID;
use super::conflicts::{self, Conflict};

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
    // Every command runs against the workspace the journal captured, or none, so `go get` can
    // only rewrite a `go.work` the caller can roll back and has leased.
    // It is named as `go env GOWORK` named it, since `go` resolves the `use` entries against
    // that spelling.
    let go = &go.bound_to_workspace(journal.shared_files_as_named().next());

    // The pre-apply `go.mod` requires, read before the re-resolve touches the file (the journal holds
    // the same bytes for restoring). The whole-graph `go get` emits one consistent `go.mod`; the
    // report is the diff of this snapshot against the result, so *every* net version change is
    // surfaced — the planned moves, the MVS floor-raises another candidate forced, and the indirect
    // churn `go mod tidy` introduced. Go parses both snapshots itself (see `gomod`).
    let before = read_go_mod(go, project).await?;
    let protected = protected_pins(&before, plan);

    // The whole graph is settled in one MVS pass: every active `module@version` target is handed to
    // a single `go get`, so Go runs one joint resolve over the entire graph rather than a sequence of
    // per-module re-resolves that would silently move other modules between candidates. MVS raises
    // each named module's floor to its cooldown-matured target; there is no `<` upper bound, so the
    // result is the unique fixed point and no candidate can ping-pong another down.
    // A conflict `go` names is settled here and the resolve retried (see `Isolation`); any other
    // failure propagates, so the caller's `apply_resilient` can isolate the offending candidates and
    // apply the rest instead of holding every candidate. The caller restores the journal, so no
    // partial `go.mod`/`go.sum` is kept, and a failed `go get` writes neither file.
    let mut isolation = Isolation::new(plan);
    loop {
        let targets: Vec<(String, String)> = isolation
            .active
            .iter()
            .map(|change| (change.package.name.clone(), change.to.as_str().to_string()))
            .collect();
        match go.get_many(&project.root, &targets).await {
            Ok(()) => break,
            // A `go` that was killed or ran in a broken environment may have printed a conflict, but
            // it proves nothing, so that failure propagates as the environment failure it is.
            Err(err) if err.is_local_environment_failure() => return Err(err),
            Err(CoreError::Tool { stderr, .. }) if isolation.settle(&stderr) => {}
            Err(err) => return Err(err),
        }
    }
    report.skipped.append(&mut isolation.held);
    report
        .skipped
        .extend(isolation.superseded.iter().map(Supersession::skipped));

    // Cross-major path changes → rewrite `/vN` import paths old→new for the active set. The single
    // joint resolve already landed, so this runs once over the whole set rather than per candidate.
    for change in &isolation.active {
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
    let after = read_go_mod(go, project).await?;

    // A direct commit pin the plan did not name must stay on its commit.
    // The check runs after tidy, because tidy can raise a floor `go get` never loaded (a new test
    // dependency of an upgraded module, say), and before any report is built, so the caller restores
    // the journal and isolates the candidate that moved the pin.
    let moves = moved_pins(&protected, &before, &after);
    if !moves.is_empty() {
        return Err(CoreError::UnacceptableResolve(describe_pin_moves(&moves)));
    }

    // Only an active change's own row reports its module; a held or superseded module, and the old
    // path a cross-major change leaves, are reported by the diff below like any other module.
    let active: HashSet<&str> = isolation
        .active
        .iter()
        .map(|change| change.package.name.as_str())
        .collect();

    // Each active candidate either reached cooldown's target (its newest-within-window) — reported
    // applied — or fell short because the joint MVS pass could not place it there — reported held.
    // "Reached" respects the move's direction: a forward candidate must land at or above its target,
    // a downgrade at or below it.
    for change in &isolation.active {
        if reached(&after.requires, change) {
            report.applied.push((*change).clone());
        } else {
            report.skipped.push(graph_ceiling_hold(change));
        }
    }

    // The hard requirement: no net version change to *any* module may be omitted. A module the plan
    // did not name that the joint resolve moved — an MVS floor raised by another candidate, or an
    // indirect `go mod tidy` added/changed — is surfaced as its own collateral applied row. This
    // closes the silent-drift gap the per-change loop left: `go mod tidy` and MVS floor-raises could
    // move modules the old per-change report never recorded.
    let mut collateral: Vec<Change> = before
        .requires
        .iter()
        .filter(|(name, _)| !active.contains(name.as_str()))
        .filter_map(|(name, from)| {
            let to = &after.requires.get(name)?.version;
            (semver::compare(&from.version, to) != std::cmp::Ordering::Equal).then(|| {
                collateral_change(CollateralMove {
                    name,
                    from: &from.version,
                    to,
                    direct: !from.indirect,
                })
            })
        })
        .collect();
    collateral.sort_by(|a, b| a.package.name.cmp(&b.package.name));
    report.applied.extend(collateral);
    Ok(report)
}

/// The project's `go.mod` as Go parses it.
async fn read_go_mod(go: &Go, project: &Project) -> Result<GoModFacts> {
    gomod::from_json(&go.mod_edit_json(&project.root).await?)
}

/// Whether a planned candidate landed at or beyond its target in `after`, respecting the move's
/// direction: a forward move must reach at/above its target, a downgrade at/below it. A module the
/// resolve dropped from the requires (no entry) counts as not reached.
fn reached(after: &HashMap<String, Require>, change: &Change) -> bool {
    after.get(&change.package.name).is_some_and(|landed| {
        let ordering = semver::compare(&landed.version, change.to.as_str());
        if change.downgrade {
            ordering != std::cmp::Ordering::Greater
        } else {
            ordering != std::cmp::Ordering::Less
        }
    })
}

fn graph_ceiling_hold(change: &Change) -> Skipped {
    let reported = reported_change(change);
    Skipped {
        offending: Some(reported.package.clone()),
        change: reported,
        reason: SkipReason::GraphCeilingHeld,
        detail: None,
    }
}

/// `change` under the identity a hold reports it by.
///
/// A cross-major candidate is planned under its *target* path (`…/vN` — `go get` needs it), but a
/// skip means the tree still holds the from-major module, so the row reports the identity that
/// actually exists; the target path stays visible in the To column's major.
fn reported_change(change: &Change) -> Change {
    let mut reported = change.clone();
    if let Some(old_path) = mutation::old_import_path(change) {
        reported.package = PackageId::new(GO_ID, old_path, change.package.registry.clone());
    }
    reported
}

/// The planned changes a joint `go get` still requests, and the ones it settled from the conflicts
/// `go` named.
///
/// A requirement conflict means the requirer, at its target, needs a newer version of a module than
/// the plan requests for it:
///
/// - For a forward change of an `// indirect` module, the plan only advanced it because no
///   requirer did, so its pin is dropped and the module follows its requirers through MVS.
///   It is *superseded*, and the age gate judges where it lands, as for any unplanned module.
/// - Otherwise the plan's target is the newest version it accepts for that module, so the requirer
///   needs a version the plan does not accept, and the requirer is held.
///
/// A version whose `go.mod` declares another module path is held too.
/// Holds are settled before supersessions, and a superseded module needs a requirer that is still
/// requested: a later round that holds or supersedes every requirer of an earlier supersession
/// gives up, and the caller's bisection settles the rest.
struct Isolation<'a> {
    active: Vec<&'a Change>,
    superseded: Vec<Supersession<'a>>,
    /// The rows of the held changes.
    held: Vec<Skipped>,
}

/// A change whose pin was dropped for the requirers that need its module newer.
struct Supersession<'a> {
    change: &'a Change,
    /// The requiring changes, as their `path@version` arguments.
    requirers: Vec<String>,
}

impl Supersession<'_> {
    /// The row reporting the dropped target.
    /// The module's real movement is reported by the collateral diff, like any unplanned module's.
    fn skipped(&self) -> Skipped {
        Skipped {
            change: self.change.clone(),
            offending: None,
            reason: SkipReason::NotEligible,
            detail: Some(format!(
                "{} require a newer version, so it follows them",
                self.requirers.join(", ")
            )),
        }
    }
}

/// The exact `path@version` argument `go get` receives for `change`.
fn argument(change: &Change) -> String {
    format!("{}@{}", change.package.name, change.to)
}

impl<'a> Isolation<'a> {
    fn new(plan: &'a Plan) -> Self {
        Isolation {
            active: plan.changes.iter().collect(),
            superseded: Vec::new(),
            held: Vec::new(),
        }
    }

    fn active_change(&self, argument_text: &str) -> Option<&'a Change> {
        self.active
            .iter()
            .copied()
            .find(|change| argument(change) == argument_text)
    }

    /// Settles the conflicts `stderr` names, returning whether the resolve can be retried.
    ///
    /// It returns `false` when no conflict names an active change it can settle, or when settling
    /// them leaves an earlier supersession without a requested requirer.
    fn settle(&mut self, stderr: &str) -> bool {
        let mut holds: Vec<Skipped> = Vec::new();
        let mut held: HashSet<String> = HashSet::new();
        let mut supersessions: Vec<(&'a Change, String)> = Vec::new();
        for conflict in conflicts::conflicts(stderr) {
            match conflict {
                Conflict::ForeignPath { argument, detail } => {
                    if let Some(change) = self.active_change(argument)
                        && held.insert(argument.to_string())
                    {
                        let reported = reported_change(change);
                        holds.push(Skipped {
                            offending: Some(reported.package.clone()),
                            change: reported,
                            reason: SkipReason::ResolverConflict,
                            detail: Some(detail),
                        });
                    }
                }
                Conflict::Requirement {
                    requirer,
                    constrained,
                    line,
                } => {
                    let (Some(requiring), Some(requested)) = (
                        self.active_change(requirer),
                        self.active_change(constrained),
                    ) else {
                        continue;
                    };
                    if requirer == constrained || requiring.downgrade {
                        continue;
                    }
                    if !requested.direct && !requested.downgrade {
                        supersessions.push((requested, requirer.to_string()));
                    } else if held.insert(requirer.to_string()) {
                        holds.push(Skipped {
                            change: reported_change(requiring),
                            offending: Some(reported_change(requested).package),
                            reason: SkipReason::ResolverConflict,
                            detail: Some(line.to_string()),
                        });
                    }
                }
            }
        }

        // Holds first, so a supersession only counts requirers that are still requested.
        let mut superseded: HashSet<String> = HashSet::new();
        for (change, requirer) in supersessions {
            if held.contains(&requirer) || superseded.contains(&requirer) {
                continue;
            }
            let module = argument(change);
            if held.contains(&module) {
                continue;
            }
            superseded.insert(module);
            match self
                .superseded
                .iter_mut()
                .find(|entry| std::ptr::eq(entry.change, change))
            {
                Some(entry) => entry.requirers.push(requirer),
                None => self.superseded.push(Supersession {
                    change,
                    requirers: vec![requirer],
                }),
            }
        }
        if holds.is_empty() && superseded.is_empty() {
            return false;
        }
        self.active.retain(|change| {
            let module = argument(change);
            !held.contains(&module) && !superseded.contains(&module)
        });
        let requested: HashSet<String> =
            self.active.iter().map(|change| argument(change)).collect();
        if self.superseded.iter().any(|entry| {
            !entry
                .requirers
                .iter()
                .any(|requirer| requested.contains(requirer))
        }) {
            return false;
        }
        self.held.append(&mut holds);
        true
    }
}

/// A module the joint resolve moved without the plan naming it, as [`collateral_change`] reports it.
#[derive(Clone, Copy)]
struct CollateralMove<'a> {
    name: &'a str,
    from: &'a str,
    to: &'a str,
    /// Whether the pre-apply `go.mod` required the module directly (no `// indirect` marker).
    direct: bool,
}

/// A net version change `apply` derived from the before/after `go.mod` diff for a module the plan
/// did not itself name — collateral movement the joint MVS resolve (or `go mod tidy`) forced.
/// Reported so no module's version change is ever silent: an indirect floor raised by another
/// candidate, or a tidy-driven indirect change, surfaces as its own report row.
fn collateral_change(moved: CollateralMove<'_>) -> Change {
    let downgrade = semver::compare(moved.to, moved.from) == std::cmp::Ordering::Less;
    Change {
        package: PackageId::new(GO_ID, moved.name.to_string(), None),
        from: Version::new(moved.from.to_string()),
        to: Version::new(moved.to.to_string()),
        // A collateral move is consistency churn, not a planned bump; its kind is informational only
        // and `Minor` is the neutral label the renderer shows.
        kind: UpdateKind::Minor,
        downgrade,
        direct: moved.direct,
        members: Vec::new(),
    }
}

/// A direct requirement at a pseudo-version that the plan does not name: a commit the project chose
/// deliberately.
/// Policy holds every commit pin (`commit_pin_verdict` in `cooldown-core`), so no plan moves one,
/// and only another candidate's minimum requirement can; the apply refuses that.
struct ProtectedPin<'a> {
    path: &'a str,
    version: &'a str,
}

/// The commit pins of the pre-apply `go.mod` that the apply must leave on their commit.
///
/// A requirement qualifies when it is direct, at a pseudo-version, and not named by `plan`, under its
/// planned path or, for a cross-major move, its old path.
/// Indirect requirements are left out: their versions are whatever the dependents require.
/// Whether a pin is protected never depends on which other candidates share the trial, so candidate
/// isolation cannot lose an upgrade to it.
fn protected_pins<'a>(before: &'a GoModFacts, plan: &Plan) -> Vec<ProtectedPin<'a>> {
    let planned: HashSet<String> = plan
        .changes
        .iter()
        .flat_map(|change| {
            std::iter::once(change.package.name.clone()).chain(mutation::old_import_path(change))
        })
        .collect();
    let mut pins: Vec<ProtectedPin<'a>> = before
        .requires
        .iter()
        .filter(|(path, require)| {
            !require.indirect
                && semver::is_pseudo(&require.version)
                && !planned.contains(path.as_str())
        })
        .map(|(path, require)| ProtectedPin {
            path,
            version: &require.version,
        })
        .collect();
    pins.sort_by(|a, b| a.path.cmp(b.path));
    pins
}

/// A protected commit pin the resolve moved.
struct PinMove<'a> {
    path: &'a str,
    from: &'a str,
    to: &'a str,
}

/// The protected pins `after` requires at any other version: another commit, a release or a
/// prerelease, in either direction.
///
/// Two version changes leave the module's source where it was and are not moves:
///
/// - A pin `after` no longer requires: nothing imports the module any more, so no version of it
///   reaches the build.
/// - A change between two versions that the same `replace` of `before` serves, such as the
///   placeholder pseudo-version Go writes for a local replacement.
///   The replacement is looked up as Go selects it, so a rule for exactly the pinned version wins
///   over a rule for every version.
fn moved_pins<'a>(
    protected: &[ProtectedPin<'a>],
    before: &GoModFacts,
    after: &'a GoModFacts,
) -> Vec<PinMove<'a>> {
    protected
        .iter()
        .filter_map(|pin| {
            let landed = after.requires.get(pin.path)?;
            if landed.version == pin.version {
                return None;
            }
            if before.same_replacement(pin.path, pin.version, &landed.version) {
                return None;
            }
            Some(PinMove {
                path: pin.path,
                from: pin.version,
                to: &landed.version,
            })
        })
        .collect()
}

fn describe_pin_moves(moves: &[PinMove<'_>]) -> String {
    let moves: Vec<String> = moves
        .iter()
        .map(|moved| format!("{} from commit {} to {}", moved.path, moved.from, moved.to))
        .collect();
    format!(
        "the resolve moved a direct requirement pinned to a commit: {}",
        moves.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINNED: &str = "v0.0.0-20240101000000-aaaaaaaaaaaa";
    const OTHER_COMMIT: &str = "v0.0.0-20250101000000-bbbbbbbbbbbb";

    /// One requirement of a test `go.mod`.
    struct Requirement<'a> {
        path: &'a str,
        version: &'a str,
        indirect: bool,
    }

    fn direct<'a>(path: &'a str, version: &'a str) -> Requirement<'a> {
        Requirement {
            path,
            version,
            indirect: false,
        }
    }

    fn indirect<'a>(path: &'a str, version: &'a str) -> Requirement<'a> {
        Requirement {
            path,
            version,
            indirect: true,
        }
    }

    /// A `replace` rule as `go mod edit -json` prints it; `old_version` is `None` for a rule that
    /// replaces every version.
    fn replace(path: &str, old_version: Option<&str>, target: &str) -> serde_json::Value {
        let old = match old_version {
            Some(version) => serde_json::json!({"Path": path, "Version": version}),
            None => serde_json::json!({"Path": path}),
        };
        serde_json::json!({"Old": old, "New": {"Path": target}})
    }

    /// What `go mod edit -json` prints for a `go.mod` with these requirements and replacements.
    fn go_mod_json(requires: &[Requirement<'_>], replaces: &[serde_json::Value]) -> String {
        let requires: Vec<serde_json::Value> = requires
            .iter()
            .map(|require| {
                serde_json::json!({
                    "Path": require.path,
                    "Version": require.version,
                    "Indirect": require.indirect,
                })
            })
            .collect();
        serde_json::json!({
            "Module": {"Path": "example.com/demo"},
            "Go": "1.24",
            "Require": requires,
            "Replace": replaces,
        })
        .to_string()
    }

    fn facts(requires: &[Requirement<'_>], replaces: &[serde_json::Value]) -> GoModFacts {
        gomod::from_json(&go_mod_json(requires, replaces)).expect("valid go mod edit -json output")
    }

    fn plan_moving(name: &str) -> Plan {
        Plan {
            changes: vec![Change {
                package: PackageId::new(GO_ID, name, None),
                from: Version::new("v0.21.1"),
                to: Version::new("v0.21.2"),
                kind: UpdateKind::Patch,
                downgrade: false,
                direct: true,
                members: Vec::new(),
            }],
            ..Plan::default()
        }
    }

    fn protected_paths(before: &GoModFacts, plan: &Plan) -> Vec<String> {
        protected_pins(before, plan)
            .iter()
            .map(|pin| pin.path.to_string())
            .collect()
    }

    #[test]
    fn protected_pins_are_the_unplanned_direct_commits() {
        let before = facts(
            &[
                direct("example.com/pinned", PINNED),
                indirect("example.com/indirect-pin", PINNED),
                direct("example.com/tagged", "v1.2.3"),
                direct("example.com/planned", PINNED),
            ],
            &[],
        );
        let plan = plan_moving("example.com/planned");
        assert_eq!(protected_paths(&before, &plan), ["example.com/pinned"]);
    }

    #[test]
    fn a_cross_major_plan_does_not_protect_the_module_it_moves_off() {
        // The plan names the module under its target path, so the old path must be exempt too.
        let before = facts(&[direct("example.com/foo", PINNED)], &[]);
        let plan = Plan {
            changes: vec![Change {
                package: PackageId::new(GO_ID, "example.com/foo/v2", None),
                from: Version::new(PINNED),
                to: Version::new("v2.0.0"),
                kind: UpdateKind::Major,
                downgrade: false,
                direct: true,
                members: Vec::new(),
            }],
            ..Plan::default()
        };
        assert_eq!(protected_paths(&before, &plan), Vec::<String>::new());
    }

    #[test]
    fn any_version_change_moves_a_protected_pin() {
        let before = facts(
            &[
                direct("example.com/to-commit", PINNED),
                direct("example.com/to-release", PINNED),
                direct("example.com/to-prerelease", PINNED),
                direct("example.com/kept", PINNED),
                direct("example.com/dropped", PINNED),
            ],
            &[],
        );
        let after = facts(
            &[
                direct("example.com/to-commit", OTHER_COMMIT),
                direct("example.com/to-release", "v1.0.0"),
                direct("example.com/to-prerelease", "v1.0.0-rc.1"),
                direct("example.com/kept", PINNED),
            ],
            &[],
        );
        let protected = protected_pins(&before, &Plan::default());
        let moves = moved_pins(&protected, &before, &after);
        let moved_paths: Vec<&str> = moves.iter().map(|pin_move| pin_move.path).collect();
        // Another commit, a release and a prerelease all move the pin; an unchanged pin and a pin
        // nothing requires any more do not.
        assert_eq!(
            moved_paths,
            [
                "example.com/to-commit",
                "example.com/to-prerelease",
                "example.com/to-release"
            ]
        );
    }

    /// The paths `moved_pins` reports when `example.com/pinned` moves from [`PINNED`] to
    /// [`OTHER_COMMIT`] under the given `replace` rules.
    fn moved_under(replaces: &[serde_json::Value]) -> Vec<String> {
        let before = facts(&[direct("example.com/pinned", PINNED)], replaces);
        let after = facts(&[direct("example.com/pinned", OTHER_COMMIT)], replaces);
        let protected = protected_pins(&before, &Plan::default());
        moved_pins(&protected, &before, &after)
            .iter()
            .map(|pin_move| pin_move.path.to_string())
            .collect()
    }

    #[test]
    fn a_move_the_same_replacement_serves_leaves_the_source_in_place() {
        // An unversioned replacement serves both versions, as for Go's local-replacement
        // placeholder, so the source does not change.
        assert_eq!(
            moved_under(&[replace("example.com/pinned", None, "../local fork")]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_move_that_changes_the_effective_replacement_is_a_move() {
        // Without a replacement the required version selects the source.
        assert_eq!(moved_under(&[]), ["example.com/pinned"]);

        // A rule for exactly the pinned version stops serving the module once it moves.
        assert_eq!(
            moved_under(&[replace("example.com/pinned", Some(PINNED), "../pinned")]),
            ["example.com/pinned"]
        );

        // That rule wins over a rule for every version, so moving off it switches the source from
        // `../pinned fork` to `../default`.
        assert_eq!(
            moved_under(&[
                replace("example.com/pinned", None, "../default"),
                replace("example.com/pinned", Some(PINNED), "../pinned fork"),
            ]),
            ["example.com/pinned"]
        );
    }

    #[test]
    fn a_pin_move_names_the_module_and_both_versions() {
        let moves = [PinMove {
            path: "k8s.io/kube-openapi",
            from: PINNED,
            to: OTHER_COMMIT,
        }];
        assert_eq!(
            describe_pin_moves(&moves),
            format!(
                "the resolve moved a direct requirement pinned to a commit: \
                 k8s.io/kube-openapi from commit {PINNED} to {OTHER_COMMIT}"
            )
        );
    }

    /// A project whose `go.mod` holds `example.com/pinned` on a commit and requires
    /// `example.com/tagged` directly, plus a scripted `go`.
    /// `go mod edit -json` prints the current state, `go get` changes nothing, and `go mod tidy`
    /// switches the state to `tidied`, a `go mod edit -json` document.
    #[cfg(unix)]
    struct ScriptedProject {
        #[expect(dead_code, reason = "dropping the guard deletes the project directory")]
        directory: tempfile::TempDir,
        project: Project,
        go: Go,
    }

    #[cfg(unix)]
    fn scripted_project(tidied: &str) -> color_eyre::eyre::Result<ScriptedProject> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let root = camino::Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
            .map_err(|path| color_eyre::eyre::eyre!("non-UTF-8 path {}", path.display()))?;
        std::fs::write(root.join("go.mod"), "module example.com/demo\n")?;
        std::fs::write(
            root.join("go.mod.json"),
            tidied_go_mod(PINNED, "v1.0.0", "v0.21.1"),
        )?;
        std::fs::write(root.join("go.mod.tidied.json"), tidied)?;
        let script = root.join("go");
        std::fs::write(
            &script,
            indoc::indoc! {r#"
                #!/bin/sh
                case "$1 $2" in
                  "mod edit") cat go.mod.json ;;
                  "mod tidy") cp go.mod.tidied.json go.mod.json ;;
                  get\ *) printf '%s\n' "${GOWORK-unset}" >> gowork.log ;;
                  *) exit 1 ;;
                esac
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let project = Project {
            root: root.clone(),
            kind: GO_ID,
            manifest: root.join("go.mod"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        Ok(ScriptedProject {
            directory,
            project,
            go: Go::with_program(script.as_str()),
        })
    }

    /// The scripted project's `go mod edit -json` with the given versions of its three requirements.
    #[cfg(unix)]
    fn tidied_go_mod(pinned: &str, tagged: &str, upgraded: &str) -> String {
        go_mod_json(
            &[
                direct("example.com/pinned", pinned),
                direct("example.com/tagged", tagged),
                direct("example.com/upgraded", upgraded),
            ],
            &[],
        )
    }

    /// `go get` can leave a commit pin alone while `go mod tidy` raises it afterwards, for example
    /// for a new test dependency of an upgraded module, so the pin is checked after tidy.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_commit_pin_that_tidy_moves_rejects_the_apply() -> color_eyre::eyre::Result<()> {
        let scripted = scripted_project(&tidied_go_mod(OTHER_COMMIT, "v1.0.0", "v0.21.2"))?;
        let plan = plan_moving("example.com/upgraded");
        let journal = mutation::mutation_journal(&scripted.project.root, &plan, None)?;

        let result = apply(&scripted.go, &scripted.project, &plan, &journal).await;

        // The move refuses the resolve, naming the pin and both commits, so recovery holds the
        // candidate that moved it.
        let Err(CoreError::UnacceptableResolve(detail)) = result else {
            color_eyre::eyre::bail!("expected an unacceptable resolve, got {result:?}");
        };
        assert!(
            detail.contains(&format!(
                "example.com/pinned from commit {PINNED} to {OTHER_COMMIT}"
            )),
            "{detail}"
        );
        Ok(())
    }

    /// `go get` runs against exactly the workspace the journal captured, and with no workspace
    /// at all when the journal captured none, whatever `go` would discover on its own.
    /// The workspace keeps the spelling it was named by, since `go` resolves relative `use`
    /// entries against it, and through a symbolic link `../lib` names another directory.
    #[cfg(unix)]
    #[tokio::test]
    async fn go_get_is_bound_to_the_journaled_workspace() -> color_eyre::eyre::Result<()> {
        let scripted = scripted_project(&tidied_go_mod(PINNED, "v1.0.0", "v0.21.2"))?;
        let plan = plan_moving("example.com/upgraded");
        let directory = tempfile::tempdir()?;
        let base = camino::Utf8PathBuf::from_path_buf(std::fs::canonicalize(directory.path())?)
            .map_err(|path| color_eyre::eyre::eyre!("non-UTF-8 path {}", path.display()))?;
        std::fs::create_dir_all(base.join("real"))?;
        std::fs::write(base.join("real/go.work"), "go 1.24.0\n")?;
        std::os::unix::fs::symlink(base.join("real"), base.join("alias"))?;
        let work = base.join("alias/go.work");

        let unbound = mutation::mutation_journal(&scripted.project.root, &plan, None)?;
        apply(&scripted.go, &scripted.project, &plan, &unbound).await?;
        let bound = mutation::mutation_journal(&scripted.project.root, &plan, Some(&work))?;
        apply(&scripted.go, &scripted.project, &plan, &bound).await?;

        let log = std::fs::read_to_string(scripted.project.root.join("gowork.log"))?;
        assert_eq!(log.lines().collect::<Vec<_>>(), ["off", work.as_str()]);
        Ok(())
    }

    /// An unplanned move of a tagged direct requirement is reported as a direct collateral row.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_moved_direct_requirement_is_a_direct_collateral_row() -> color_eyre::eyre::Result<()>
    {
        let scripted = scripted_project(&tidied_go_mod(PINNED, "v1.1.0", "v0.21.2"))?;
        let plan = plan_moving("example.com/upgraded");
        let journal = mutation::mutation_journal(&scripted.project.root, &plan, None)?;

        let report = apply(&scripted.go, &scripted.project, &plan, &journal).await?;

        let names: Vec<&str> = report
            .applied
            .iter()
            .map(|change| change.package.name.as_str())
            .collect();
        assert_eq!(names, ["example.com/upgraded", "example.com/tagged"]);
        assert!(
            report.applied.iter().all(|change| change.direct),
            "the collateral row keeps the requirement direct: {:?}",
            report.applied
        );
        Ok(())
    }

    #[test]
    fn collateral_change_surfaces_an_unplanned_floor_raise() {
        // An indirect the plan never named moved up (an MVS floor a candidate raised): it must be a
        // collateral row, not silent.
        let from = "v0.30.1";
        let to = "v0.30.2";
        let change = collateral_change(CollateralMove {
            name: "k8s.io/apimachinery",
            from,
            to,
            direct: false,
        });
        assert_eq!(change.package.name, "k8s.io/apimachinery");
        assert_eq!(change.from.as_str(), from);
        assert_eq!(change.to.as_str(), to);
        assert!(!change.downgrade);
        assert!(!change.direct);
    }

    #[test]
    fn collateral_change_keeps_a_direct_requirement_direct() {
        let change = collateral_change(CollateralMove {
            name: "k8s.io/kube-openapi",
            from: PINNED,
            to: "v1.0.0",
            direct: true,
        });
        assert!(change.direct);
    }

    #[test]
    fn collateral_change_marks_a_forced_regression_as_a_downgrade() {
        let change = collateral_change(CollateralMove {
            name: "k8s.io/api",
            from: "v0.31.0",
            to: "v0.30.2",
            direct: false,
        });
        assert!(change.downgrade);
    }

    #[test]
    fn resolver_conflict_reports_a_cross_major_skip_under_the_from_identity() {
        // Planned under the target path (go get needs `…/v2`), but skipped — the tree still holds
        // the v1 module, so the row must not claim a `…/v2` module exists at v1.3.0.
        let change = Change {
            package: PackageId::new(GO_ID, "example.com/foo/v2", None),
            from: Version::new("v1.3.0"),
            to: Version::new("v2.0.0"),
            kind: UpdateKind::Major,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        };
        let skipped = graph_ceiling_hold(&change);
        assert_eq!(skipped.change.package.name, "example.com/foo");
        // Self-blame stays consistent with the rewritten identity so the renderer's self check
        // (which falls back to the generic resolver message) still recognizes it.
        assert_eq!(
            skipped
                .offending
                .as_ref()
                .map(|package| package.name.as_str()),
            Some("example.com/foo"),
        );
        // A within-major skip keeps its identity untouched.
        let minor = Change {
            package: PackageId::new(GO_ID, "example.com/foo", None),
            from: Version::new("v1.3.0"),
            to: Version::new("v1.4.0"),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        };
        assert_eq!(
            graph_ceiling_hold(&minor).change.package.name,
            "example.com/foo"
        );
    }

    #[test]
    fn resolver_conflict_reports_an_incompatible_cross_path_skip_under_the_base_identity() {
        // A `+incompatible` line moving onto a discovered `…/v3` path: a skip leaves the tree on
        // the base-path `+incompatible` require, so the row must carry the base identity — not
        // claim a `…/v3` module pinned at a `+incompatible` from-version, which cannot exist.
        let change = Change {
            package: PackageId::new(GO_ID, "example.com/foo/v3", None),
            from: Version::new("v2.9.0+incompatible"),
            to: Version::new("v3.0.2"),
            kind: UpdateKind::Major,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        };
        let skipped = graph_ceiling_hold(&change);
        assert_eq!(skipped.change.package.name, "example.com/foo");
        assert_eq!(
            skipped
                .offending
                .as_ref()
                .map(|package| package.name.as_str()),
            Some("example.com/foo"),
        );
        // A path-stable `+incompatible` skip (the common within-line bump) keeps its identity.
        let within_line = Change {
            package: PackageId::new(GO_ID, "github.com/docker/cli", None),
            from: Version::new("v29.2.1+incompatible"),
            to: Version::new("v29.5.2+incompatible"),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        };
        assert_eq!(
            graph_ceiling_hold(&within_line).change.package.name,
            "github.com/docker/cli"
        );
    }

    #[test]
    fn reached_respects_move_direction() {
        let mut after = HashMap::new();
        after.insert(
            "k8s.io/api".to_string(),
            Require {
                version: "v0.30.2".to_string(),
                indirect: false,
            },
        );

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

    /// A project whose scripted `go` fails chosen `go get` runs with a given stderr.
    #[cfg(unix)]
    struct ScriptedGets {
        #[expect(dead_code, reason = "dropping the guard deletes the project directory")]
        directory: tempfile::TempDir,
        project: Project,
        go: Go,
    }

    /// The scripted `go`: `go mod edit -json` prints `before` until `go mod tidy` switches it to
    /// `tidied`. The n-th `go get` (counting from 1) fails with `failures[n - 1]` as its stderr while
    /// one is given, and succeeds after that. A failing run exits with status 1, unless a
    /// `get.<n>.status` file names another. Every `go get` logs its arguments, one run per line.
    #[cfg(unix)]
    fn scripted_gets(
        before: &str,
        tidied: &str,
        failures: &[&str],
    ) -> color_eyre::eyre::Result<ScriptedGets> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let root = camino::Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
            .map_err(|path| color_eyre::eyre::eyre!("non-UTF-8 path {}", path.display()))?;
        std::fs::write(root.join("go.mod"), "module example.com/demo\n")?;
        std::fs::write(root.join("go.mod.json"), before)?;
        std::fs::write(root.join("go.mod.tidied.json"), tidied)?;
        for (index, stderr) in failures.iter().enumerate() {
            std::fs::write(root.join(format!("get.{}.stderr", index + 1)), stderr)?;
        }
        let script = root.join("go");
        std::fs::write(
            &script,
            indoc::indoc! {r#"
                #!/bin/sh
                case "$1 $2" in
                  "mod edit") cat go.mod.json ;;
                  "mod tidy") cp go.mod.tidied.json go.mod.json ;;
                  get\ *)
                    run=$(( $(cat gets.count 2>/dev/null || echo 0) + 1 ))
                    echo "$run" > gets.count
                    shift
                    echo "$*" >> gets.log
                    if [ -f "get.$run.stderr" ]; then
                      cat "get.$run.stderr" >&2
                      exit "$(cat "get.$run.status" 2>/dev/null || echo 1)"
                    fi ;;
                  *) exit 1 ;;
                esac
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let project = Project {
            root: root.clone(),
            kind: GO_ID,
            manifest: root.join("go.mod"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        Ok(ScriptedGets {
            directory,
            project,
            go: Go::with_program(script.as_str()),
        })
    }

    /// The argument lists of every `go get` the scripted project ran.
    #[cfg(unix)]
    fn get_runs(scripted: &ScriptedGets) -> color_eyre::eyre::Result<Vec<String>> {
        let log = std::fs::read_to_string(scripted.project.root.join("gets.log"))?;
        Ok(log.lines().map(str::to_string).collect())
    }

    #[cfg(unix)]
    fn forward(name: &str, from: &str, to: &str, direct: bool) -> Change {
        Change {
            package: PackageId::new(GO_ID, name, None),
            from: Version::new(from),
            to: Version::new(to),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct,
            members: Vec::new(),
        }
    }

    #[cfg(unix)]
    fn plan_of(changes: Vec<Change>) -> Plan {
        Plan {
            changes,
            ..Plan::default()
        }
    }

    #[cfg(unix)]
    fn moves(changes: &[Change]) -> Vec<String> {
        changes
            .iter()
            .map(|change| format!("{} {} -> {}", change.package.name, change.from, change.to))
            .collect()
    }

    #[cfg(unix)]
    fn holds(skipped: &[Skipped]) -> Vec<(String, SkipReason, Option<String>)> {
        skipped
            .iter()
            .map(|skip| {
                (
                    skip.change.package.name.clone(),
                    skip.reason,
                    skip.offending.as_ref().map(|package| package.name.clone()),
                )
            })
            .collect()
    }

    /// A requirer that needs a direct module newer than the plan's target for it is held, the rest
    /// of the resolve is retried without it, and its own resolver-forced movement is still reported.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_requirer_needing_a_direct_module_newer_than_planned_is_held()
    -> color_eyre::eyre::Result<()> {
        const CONFLICT: &str =
            "example.com/x@v1.2.0 requires example.com/d@v1.3.0, not example.com/d@v1.1.0";
        let before = go_mod_json(
            &[
                direct("example.com/x", "v1.0.0"),
                direct("example.com/d", "v1.0.0"),
            ],
            &[],
        );
        // `d`'s target raised `x` as far as it needed.
        let tidied = go_mod_json(
            &[
                direct("example.com/x", "v1.1.0"),
                direct("example.com/d", "v1.1.0"),
            ],
            &[],
        );
        let scripted = scripted_gets(&before, &tidied, &[&format!("go: {CONFLICT}\n")])?;
        let plan = plan_of(vec![
            forward("example.com/x", "v1.0.0", "v1.2.0", true),
            forward("example.com/d", "v1.0.0", "v1.1.0", true),
        ]);
        let journal = mutation::mutation_journal(&scripted.project.root, &plan, None)?;

        let report = apply(&scripted.go, &scripted.project, &plan, &journal).await?;

        // The retry requests everything but the held requirer.
        assert_eq!(
            get_runs(&scripted)?,
            [
                "example.com/x@v1.2.0 example.com/d@v1.1.0",
                "example.com/d@v1.1.0"
            ]
        );
        assert_eq!(
            holds(&report.skipped),
            [(
                "example.com/x".to_string(),
                SkipReason::ResolverConflict,
                Some("example.com/d".to_string())
            )]
        );
        assert_eq!(
            report
                .skipped
                .first()
                .and_then(|skip| skip.detail.as_deref()),
            Some(CONFLICT)
        );
        assert_eq!(
            moves(&report.applied),
            [
                "example.com/d v1.0.0 -> v1.1.0",
                "example.com/x v1.0.0 -> v1.1.0"
            ]
        );
        Ok(())
    }

    /// An `// indirect` module that a requirer's target needs newer than planned follows that
    /// requirer instead of holding it: its pin is dropped, and its real movement is reported.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_indirect_module_a_requirer_needs_newer_follows_the_requirer()
    -> color_eyre::eyre::Result<()> {
        let before = go_mod_json(
            &[
                direct("example.com/x", "v1.0.0"),
                indirect("example.com/m", "v0.21.0"),
            ],
            &[],
        );
        let tidied = go_mod_json(
            &[
                direct("example.com/x", "v1.2.0"),
                indirect("example.com/m", "v1.47.0"),
            ],
            &[],
        );
        let scripted = scripted_gets(
            &before,
            &tidied,
            &[
                "go: example.com/x@v1.2.0 requires example.com/m@v1.47.0, not example.com/m@v0.22.0\n",
            ],
        )?;
        let plan = plan_of(vec![
            forward("example.com/x", "v1.0.0", "v1.2.0", true),
            forward("example.com/m", "v0.21.0", "v0.22.0", false),
        ]);
        let journal = mutation::mutation_journal(&scripted.project.root, &plan, None)?;

        let report = apply(&scripted.go, &scripted.project, &plan, &journal).await?;

        assert_eq!(
            get_runs(&scripted)?,
            [
                "example.com/x@v1.2.0 example.com/m@v0.22.0",
                "example.com/x@v1.2.0"
            ]
        );
        // The dropped target is a conservative skip naming the requirer, and the module's real
        // move is reported beside the requirer's upgrade.
        assert_eq!(
            holds(&report.skipped),
            [("example.com/m".to_string(), SkipReason::NotEligible, None)]
        );
        assert!(
            report
                .skipped
                .first()
                .and_then(|skip| skip.detail.as_deref())
                .is_some_and(|detail| detail.contains("example.com/x@v1.2.0")),
            "{:?}",
            report.skipped
        );
        assert_eq!(
            moves(&report.applied),
            [
                "example.com/x v1.0.0 -> v1.2.0",
                "example.com/m v0.21.0 -> v1.47.0"
            ]
        );
        Ok(())
    }

    /// A version whose `go.mod` declares another module path is held with Go's explanation.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_version_declaring_another_path_is_held() -> color_eyre::eyre::Result<()> {
        let before = go_mod_json(
            &[
                direct("example.com/c", "v0.29.2"),
                direct("example.com/x", "v1.0.0"),
            ],
            &[],
        );
        let tidied = go_mod_json(
            &[
                direct("example.com/c", "v0.29.2"),
                direct("example.com/x", "v1.2.0"),
            ],
            &[],
        );
        let foreign = indoc::indoc! {"
            go: example.com/c@v0.32.0 requires example.com/c@v0.32.0: parsing go.mod:
            \tmodule declares its path as: example.dev/c
            \t        but was required as: example.com/c
        "};
        let scripted = scripted_gets(&before, &tidied, &[foreign])?;
        let plan = plan_of(vec![
            forward("example.com/c", "v0.29.2", "v0.32.0", true),
            forward("example.com/x", "v1.0.0", "v1.2.0", true),
        ]);
        let journal = mutation::mutation_journal(&scripted.project.root, &plan, None)?;

        let report = apply(&scripted.go, &scripted.project, &plan, &journal).await?;

        assert_eq!(
            get_runs(&scripted)?,
            [
                "example.com/c@v0.32.0 example.com/x@v1.2.0",
                "example.com/x@v1.2.0"
            ]
        );
        assert_eq!(
            holds(&report.skipped),
            [(
                "example.com/c".to_string(),
                SkipReason::ResolverConflict,
                Some("example.com/c".to_string())
            )]
        );
        assert_eq!(moves(&report.applied), ["example.com/x v1.0.0 -> v1.2.0"]);
        Ok(())
    }

    /// A failure that names no active change it can settle propagates unchanged, so the caller's
    /// bisection takes over, and so does one that would leave a supersession without its requirer.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unsettled_failure_propagates_after_one_resolve() -> color_eyre::eyre::Result<()> {
        let before = go_mod_json(
            &[
                direct("example.com/y", "v1.0.0"),
                indirect("example.com/x", "v0.21.0"),
                indirect("example.com/m", "v0.21.0"),
            ],
            &[],
        );
        let plan = plan_of(vec![
            forward("example.com/y", "v1.0.0", "v1.1.0", true),
            forward("example.com/x", "v0.21.0", "v0.22.0", false),
            forward("example.com/m", "v0.21.0", "v0.22.0", false),
        ]);
        let unknown = "go: example.com/y@v1.1.0: invalid version: unknown revision v1.1.0\n";
        // `x` first supersedes `m`, then is itself superseded by `y`, which leaves `m`'s pin dropped
        // for a requirer that is no longer requested.
        let chain: [&str; 2] = [
            "go: example.com/x@v0.22.0 requires example.com/m@v1.0.0, not example.com/m@v0.22.0\n",
            "go: example.com/y@v1.1.0 requires example.com/x@v1.0.0, not example.com/x@v0.22.0\n",
        ];
        for (failures, runs) in [(&[unknown][..], 1), (&chain[..], 2)] {
            let scripted = scripted_gets(&before, &before, failures)?;
            let journal = mutation::mutation_journal(&scripted.project.root, &plan, None)?;

            let result = apply(&scripted.go, &scripted.project, &plan, &journal).await;

            assert!(
                matches!(result, Err(CoreError::Tool { .. })),
                "{failures:?}: {result:?}"
            );
            assert_eq!(get_runs(&scripted)?.len(), runs, "{failures:?}");
        }
        Ok(())
    }

    /// A held cross-major change rewrites no import, and its row names the module that still exists.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_held_cross_major_change_rewrites_no_import() -> color_eyre::eyre::Result<()> {
        const MAIN: &str = indoc::indoc! {r#"
            package main

            import _ "example.com/foo"
        "#};
        let before = go_mod_json(
            &[
                direct("example.com/foo", "v1.3.0"),
                direct("example.com/d", "v1.0.0"),
            ],
            &[],
        );
        let tidied = go_mod_json(
            &[
                direct("example.com/foo", "v1.3.0"),
                direct("example.com/d", "v1.1.0"),
            ],
            &[],
        );
        let scripted = scripted_gets(
            &before,
            &tidied,
            &[
                "go: example.com/foo/v2@v2.0.0 requires example.com/d@v1.4.0, not example.com/d@v1.1.0\n",
            ],
        )?;
        std::fs::write(scripted.project.root.join("main.go"), MAIN)?;
        let mut cross_major = forward("example.com/foo/v2", "v1.3.0", "v2.0.0", true);
        cross_major.kind = UpdateKind::Major;
        let plan = plan_of(vec![
            cross_major,
            forward("example.com/d", "v1.0.0", "v1.1.0", true),
        ]);
        let journal = mutation::mutation_journal(&scripted.project.root, &plan, None)?;

        let report = apply(&scripted.go, &scripted.project, &plan, &journal).await?;

        assert_eq!(
            std::fs::read_to_string(scripted.project.root.join("main.go"))?,
            MAIN
        );
        assert_eq!(
            holds(&report.skipped),
            [(
                "example.com/foo".to_string(),
                SkipReason::ResolverConflict,
                Some("example.com/d".to_string())
            )]
        );
        Ok(())
    }

    /// A requirer held in a round supersedes nothing in that round: the module it needed newer keeps
    /// its pin, and a line naming the same argument on both sides settles nothing while the round's
    /// other conflicts still do.
    #[cfg(unix)]
    #[tokio::test]
    async fn only_a_still_requested_other_change_supersedes_a_pin() -> color_eyre::eyre::Result<()>
    {
        let before = go_mod_json(
            &[
                direct("example.com/x", "v1.0.0"),
                direct("example.com/d", "v1.0.0"),
                indirect("example.com/m", "v0.21.0"),
            ],
            &[],
        );
        let tidied = go_mod_json(
            &[
                direct("example.com/x", "v1.0.0"),
                direct("example.com/d", "v1.1.0"),
                indirect("example.com/m", "v0.22.0"),
            ],
            &[],
        );
        let round = indoc::indoc! {"
            go: example.com/m@v0.22.0 indirectly requires example.com/m@v0.23.0, not example.com/m@v0.22.0
            go: example.com/x@v1.2.0 requires example.com/m@v1.0.0, not example.com/m@v0.22.0
            go: example.com/x@v1.2.0 requires example.com/d@v1.3.0, not example.com/d@v1.1.0
        "};
        let scripted = scripted_gets(&before, &tidied, &[round])?;
        let plan = plan_of(vec![
            forward("example.com/x", "v1.0.0", "v1.2.0", true),
            forward("example.com/d", "v1.0.0", "v1.1.0", true),
            forward("example.com/m", "v0.21.0", "v0.22.0", false),
        ]);
        let journal = mutation::mutation_journal(&scripted.project.root, &plan, None)?;

        let report = apply(&scripted.go, &scripted.project, &plan, &journal).await?;

        assert_eq!(
            get_runs(&scripted)?,
            [
                "example.com/x@v1.2.0 example.com/d@v1.1.0 example.com/m@v0.22.0",
                "example.com/d@v1.1.0 example.com/m@v0.22.0"
            ]
        );
        assert_eq!(
            holds(&report.skipped),
            [(
                "example.com/x".to_string(),
                SkipReason::ResolverConflict,
                Some("example.com/d".to_string())
            )]
        );
        assert_eq!(
            moves(&report.applied),
            [
                "example.com/d v1.0.0 -> v1.1.0",
                "example.com/m v0.21.0 -> v0.22.0"
            ]
        );
        Ok(())
    }

    /// A `go` killed after printing a conflict it could settle proves nothing, so the failure
    /// propagates as an environment failure and no retry runs.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_killed_resolve_propagates_its_failure() -> color_eyre::eyre::Result<()> {
        let before = go_mod_json(
            &[
                direct("example.com/x", "v1.0.0"),
                direct("example.com/d", "v1.0.0"),
            ],
            &[],
        );
        let scripted = scripted_gets(
            &before,
            &before,
            &["go: example.com/x@v1.2.0 requires example.com/d@v1.3.0, not example.com/d@v1.1.0\n"],
        )?;
        // Exit status 143 is a SIGTERM delivered through a shell.
        std::fs::write(scripted.project.root.join("get.1.status"), "143")?;
        let plan = plan_of(vec![
            forward("example.com/x", "v1.0.0", "v1.2.0", true),
            forward("example.com/d", "v1.0.0", "v1.1.0", true),
        ]);
        let journal = mutation::mutation_journal(&scripted.project.root, &plan, None)?;

        let result = apply(&scripted.go, &scripted.project, &plan, &journal).await;

        assert!(
            result
                .as_ref()
                .is_err_and(CoreError::is_local_environment_failure),
            "{result:?}"
        );
        assert_eq!(get_runs(&scripted)?.len(), 1);
        Ok(())
    }
}
