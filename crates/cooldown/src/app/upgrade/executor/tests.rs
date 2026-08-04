use super::planning::{plan_baseline_violations, target_package};
use super::{
    BatchOutcome, CommittedBatch, PlanMode, TrialRollback, candidate_scope, collapse_applied_legs,
    collateral_rows, combine_lock_status, conflict_skip_message, insert_graph_violation,
    is_downgrade, newly_introduced_violations, package_label, planned_changes_landed,
    preserve_rollback_entries, retained_trial_outcomes, sort_planned_changes,
    verify_applied_targets, violation_identity,
};
use crate::app::{TransitiveGate, UpgradeItem};
use color_eyre::eyre;
use cooldown_core::{
    ApplyReport, BaselineViolation, Change, DepScope, Dependency, Diagnostic, DiagnosticKind,
    EdgeBindingAction, LockStatus, MajorKey, MemberRef, PackageId, ProjectMutationJournal, Release,
    ReleaseOrder, ReleaseQuality, SkipReason, ToolId, UpdateKind, Version,
};
use std::collections::HashSet;

#[test]
fn upgrade_scopes_candidates_to_direct_requires() {
    // `upgrade` hands only direct requires to the resolver; MVS promotes indirect deps as a
    // consequence, so indirect deps are never attempt-and-rejected as candidates.
    assert_eq!(candidate_scope(PlanMode::Upgrade), DepScope::Direct);
}

#[test]
fn fix_walks_the_graph_unless_transitive_hidden() {
    // `fix` must see the resolved graph to downgrade too-fresh transitives.
    assert_eq!(
        candidate_scope(PlanMode::Fix {
            transitive: TransitiveGate::Enforce,
            downgrade_pinned: false,
        }),
        DepScope::Graph
    );
    assert_eq!(
        candidate_scope(PlanMode::Fix {
            transitive: TransitiveGate::Allow,
            downgrade_pinned: false,
        }),
        DepScope::Graph
    );
    // `--transitive hide` narrows `fix` candidates to direct deps.
    assert_eq!(
        candidate_scope(PlanMode::Fix {
            transitive: TransitiveGate::Hide,
            downgrade_pinned: false,
        }),
        DepScope::Direct
    );
}

#[test]
fn rollback_journal_keeps_the_first_snapshot_for_each_path() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = camino::Utf8Path::from_path(directory.path())
        .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
    std::fs::write(root.join("package-lock.json"), b"baseline lock")?;
    let mut rollback =
        ProjectMutationJournal::capture(root, [camino::Utf8Path::new("package-lock.json")])?;
    std::fs::write(root.join("package-lock.json"), b"trial lock")?;
    std::fs::write(root.join("package.json"), b"baseline manifest")?;
    let later = ProjectMutationJournal::capture(
        root,
        [
            camino::Utf8Path::new("package-lock.json"),
            camino::Utf8Path::new("package.json"),
        ],
    )?;

    preserve_rollback_entries(&mut rollback, &later)?;

    assert_eq!(rollback.files().len(), 2);
    let mut files = rollback.files().iter();
    let lock = files
        .next()
        .ok_or_else(|| eyre::eyre!("rollback journal omitted the lockfile"))?;
    let manifest = files
        .next()
        .ok_or_else(|| eyre::eyre!("rollback journal omitted the manifest"))?;
    assert_eq!(lock.contents(), Some(b"baseline lock".as_slice()));
    assert_eq!(manifest.path(), "package.json");
    assert!(files.next().is_none());
    Ok(())
}

#[test]
fn trial_rollback_refuses_to_overwrite_external_drift() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = camino::Utf8PathBuf::from_path_buf(directory.path().to_owned())
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    let path = root.join("Cargo.toml");
    std::fs::write(&path, b"original")?;
    let journal = ProjectMutationJournal::capture(&root, [camino::Utf8Path::new("Cargo.toml")])?;
    let mut rollback = TrialRollback::default();
    rollback.preserve(&journal)?;

    std::fs::write(&path, b"cooldown candidate")?;
    rollback.accept(journal.capture_state()?)?;
    std::fs::write(&path, b"external edit")?;

    let error = rollback
        .restore()
        .err()
        .ok_or_else(|| eyre::eyre!("independent drift unexpectedly allowed trial rollback"))?;
    std::assert_matches!(error, cooldown_core::CoreError::LockConflict(_));
    assert_eq!(std::fs::read(path)?, b"external edit");
    Ok(())
}

#[test]
fn conflict_skip_message_names_a_different_offender() {
    // A resolver conflict blamed on another package — adopting it would have regressed that
    // package — names it, so the report explains which dependency holds the candidate back.
    assert_eq!(
        conflict_skip_message(
            SkipReason::ResolverConflict,
            Some("typer"),
            "huggingface-hub"
        ),
        "held: conflicts with typer"
    );
}

#[test]
fn conflict_skip_message_keeps_generic_message_when_offender_is_self() {
    // The resolver rejected the pin itself (no other package to blame): keep the generic message.
    assert_eq!(
        conflict_skip_message(SkipReason::ResolverConflict, Some("foo"), "foo"),
        SkipReason::ResolverConflict.message()
    );
    assert_eq!(
        conflict_skip_message(SkipReason::ResolverConflict, None, "foo"),
        SkipReason::ResolverConflict.message()
    );
}

#[test]
fn conflict_skip_message_passes_through_non_conflict_reasons() {
    assert_eq!(
        conflict_skip_message(SkipReason::NeedsMajor, Some("bar"), "foo"),
        SkipReason::NeedsMajor.message()
    );
}

fn rel(version: &str, order: u8) -> Release {
    Release {
        version: Version::new(version),
        order: ReleaseOrder(vec![order]),
        major: MajorKey(String::new()),
        major_number: version
            .trim_start_matches('v')
            .split('.')
            .next()
            .and_then(|major| major.parse().ok()),
        kind_from_current: None,
        beyond_declared_bound: false,
        beyond_latest_tag: false,
        published_at: None,
        yanked: false,
        quality: ReleaseQuality::Stable,
    }
}

fn dep(name: &str, version: &str) -> Dependency {
    Dependency {
        package: PackageId::new(ToolId("mock"), name, None),
        current: Version::new(version),
        current_quality: ReleaseQuality::Stable,
        direct: true,
        artifacts: Vec::new(),
        graph_floor: None,
        graph_ceiling: None,
        declared_bound: None,
        members: Vec::new(),
        pinned: false,
    }
}

fn member(name: &str, path: &str) -> MemberRef {
    MemberRef {
        name: name.to_string(),
        path: path.to_string(),
    }
}

fn change(name: &str, from: &str, to: &str) -> Change {
    Change {
        package: PackageId::new(ToolId("mock"), name, None),
        from: Version::new(from),
        to: Version::new(to),
        kind: UpdateKind::Minor,
        downgrade: false,
        direct: true,
        members: Vec::new(),
    }
}

#[test]
fn planned_changes_restore_stable_order_after_concurrent_fetches() {
    let mut changes = vec![
        change("referencing", "0.46.5", "0.46.6"),
        change("jsonschema", "0.46.5", "0.46.6"),
    ];

    sort_planned_changes(&mut changes);

    assert_eq!(
        changes
            .iter()
            .map(|change| change.package.name.as_str())
            .collect::<Vec<_>>(),
        vec!["jsonschema", "referencing"]
    );
}

#[test]
fn collateral_rows_keep_a_held_candidates_real_movement() {
    let planned = vec![change("referencing", "0.46.5", "0.46.6")];
    // The adapter reported the held candidate's real float (an off-target row for a planned
    // package) and an unplanned transitive move; the plan's own claimed row is the only one
    // excluded from collateral.
    let applied = vec![
        change("referencing", "0.46.5", "0.46.6"),
        change("referencing", "0.46.5", "0.46.10"),
        change("quote", "1.0.44", "1.0.45"),
    ];

    let collateral = collateral_rows(&applied, &planned);

    assert_eq!(
        collateral
            .iter()
            .map(|change| (change.package.name.as_str(), change.to.as_str()))
            .collect::<Vec<_>>(),
        vec![("referencing", "0.46.10"), ("quote", "1.0.45")],
        "a package-level filter would drop the held candidate's movement row"
    );
}

#[test]
fn verify_applied_targets_turns_unreached_planned_success_into_skip() {
    let planned = vec![change("serde", "1.0.0", "1.0.1")];
    let report = ApplyReport {
        applied: planned.clone(),
        ..ApplyReport::default()
    };

    let verified = verify_applied_targets(report, &planned, &[dep("serde", "1.0.0")]);

    assert!(verified.applied.is_empty());
    assert_eq!(verified.skipped.len(), 1);
    assert_eq!(verified.skipped[0].reason, SkipReason::ResolverConflict);
    assert_eq!(verified.skipped[0].change.package.name, "serde");
}

#[test]
fn verify_applied_targets_keys_same_package_by_target_version() {
    let landed = change("serde", "1.0.0", "1.0.1");
    let missed = change("serde", "1.0.0", "1.0.2");
    let planned = vec![landed.clone(), missed.clone()];
    let report = ApplyReport {
        applied: planned.clone(),
        ..ApplyReport::default()
    };

    let verified = verify_applied_targets(report, &planned, &[dep("serde", "1.0.1")]);

    assert_eq!(verified.applied, vec![landed]);
    assert_eq!(verified.skipped.len(), 1);
    assert_eq!(verified.skipped[0].change, missed);
}

#[test]
fn verify_applied_targets_requires_the_planned_direct_member_to_land() {
    let mut planned_change = change("nix", "0.28.0", "0.31.3");
    planned_change.members = vec![member("micromux-mcp", "crates/micromux-mcp")];
    let planned = vec![planned_change.clone()];
    let report = ApplyReport {
        applied: planned.clone(),
        ..ApplyReport::default()
    };
    let mut old_dep = dep("nix", "0.28.0");
    old_dep.members = vec![member("micromux-mcp", "crates/micromux-mcp")];
    let mut other_member_dep = dep("nix", "0.31.3");
    other_member_dep.members = vec![member("micromux", "crates/micromux")];

    let verified = verify_applied_targets(report, &planned, &[old_dep, other_member_dep]);

    assert!(verified.applied.is_empty());
    assert_eq!(verified.skipped.len(), 1);
    assert_eq!(verified.skipped[0].change, planned_change);
}

#[test]
fn verify_applied_targets_rejects_a_target_reached_through_a_sibling_entry() {
    // A member that declares the crate twice ([dependencies] toml = "1" beside
    // [build-dependencies] toml = "0.5") resolves the target version before the planned 0.5.x
    // move is even attempted.
    // As long as the member's old direct line survives, the change has not landed — the sibling
    // edge must not verify it as applied (it would report an upgrade the lock never took, on every
    // run, forever).
    let mut planned_change = change("toml", "0.5.11", "1.1.2");
    planned_change.members = vec![member("rawloader", "rawloader")];
    let planned = vec![planned_change.clone()];
    let report = ApplyReport {
        applied: planned.clone(),
        ..ApplyReport::default()
    };
    let mut old_line = dep("toml", "0.5.11");
    old_line.members = vec![member("rawloader", "rawloader")];
    let mut new_line = dep("toml", "1.1.2");
    new_line.members = vec![member("rawloader", "rawloader")];

    let verified = verify_applied_targets(report, &planned, &[old_line.clone(), new_line.clone()]);

    assert!(verified.applied.is_empty());
    assert_eq!(verified.skipped.len(), 1);
    assert_eq!(verified.skipped[0].change, planned_change);

    // Once the old line is gone — or survives only transitively (another crate's requirement,
    // not the member's own entry) — the member's move has landed.
    let report = ApplyReport {
        applied: planned.clone(),
        ..ApplyReport::default()
    };
    old_line.direct = false;
    let verified = verify_applied_targets(report, &planned, &[old_line, new_line]);
    assert_eq!(verified.applied.len(), 1);
    assert!(verified.skipped.is_empty());
}

#[test]
fn verify_applied_targets_keeps_each_held_member_when_targets_collide() {
    // Two members bump the same crate to the same target from different current versions, so the
    // changes share (name, registry, to) and differ only by member.
    // Both are held; neither may be dropped by a member-blind key collapsing them into a single
    // skip.
    let mut held_a = change("nix", "0.28.0", "0.31.3");
    held_a.members = vec![member("app-a", "crates/app-a")];
    let mut held_b = change("nix", "0.30.0", "0.31.3");
    held_b.members = vec![member("app-b", "crates/app-b")];
    let planned = vec![held_a.clone(), held_b.clone()];
    let report = ApplyReport {
        applied: planned.clone(),
        ..ApplyReport::default()
    };
    let mut a_dep = dep("nix", "0.28.0");
    a_dep.members = vec![member("app-a", "crates/app-a")];
    let mut b_dep = dep("nix", "0.30.0");
    b_dep.members = vec![member("app-b", "crates/app-b")];

    let verified = verify_applied_targets(report, &planned, &[a_dep, b_dep]);

    assert!(verified.applied.is_empty());
    assert_eq!(verified.skipped.len(), 2);
    let held: Vec<&Change> = verified.skipped.iter().map(|skip| &skip.change).collect();
    assert!(held.contains(&&held_a));
    assert!(held.contains(&&held_b));
}

#[test]
fn verify_applied_targets_does_not_split_transitive_attribution_by_member() {
    let mut held_a = change("shared", "1.0.0", "1.2.0");
    held_a.direct = false;
    held_a.members = vec![member("app-a", "crates/app-a")];
    let mut held_b = change("shared", "1.1.0", "1.2.0");
    held_b.direct = false;
    held_b.members = vec![member("app-b", "crates/app-b")];
    let planned = vec![held_a.clone(), held_b];
    let report = ApplyReport {
        applied: planned.clone(),
        ..ApplyReport::default()
    };

    let verified = verify_applied_targets(report, &planned, &[dep("shared", "1.0.0")]);

    assert!(verified.applied.is_empty());
    assert_eq!(verified.skipped.len(), 1);
    assert_eq!(verified.skipped[0].change, held_a);
}

#[test]
fn verify_applied_targets_keeps_lock_diff_collateral_outside_dependency_graph() {
    let planned_change = change("serde", "1.0.0", "1.0.1");
    let collateral = change("itoa", "1.0.0", "1.0.1");
    let report = ApplyReport {
        applied: vec![planned_change.clone(), collateral.clone()],
        ..ApplyReport::default()
    };

    let verified = verify_applied_targets(
        report,
        std::slice::from_ref(&planned_change),
        &[dep("serde", "1.0.1")],
    );

    assert_eq!(verified.applied, vec![planned_change, collateral]);
    assert!(verified.skipped.is_empty());
}

#[test]
fn planned_changes_landed_rejects_collateral_only_results() {
    let planned = change("serde", "1.0.0", "1.0.1");
    let collateral = change("itoa", "1.0.0", "1.0.1");
    let applied = HashSet::from([super::change_target_key(&collateral)]);

    assert!(!planned_changes_landed(&[planned], &applied));
}

fn applied_item(name: &str, from: &str, to: &str, downgrade: bool) -> UpgradeItem {
    UpgradeItem {
        name: name.to_string(),
        tool: "cargo".to_string(),
        project: ".".to_string(),
        direct: false,
        downgrade,
        members: Vec::new(),
        registry: Some("crates.io".to_string()),
        from: from.to_string(),
        to: to.to_string(),
        kind: UpdateKind::Minor,
        applied: true,
        skipped: None,
        error: None,
        edge: None,
    }
}

fn edge_item(dependent: &str, from: &str, to: &str) -> UpgradeItem {
    let mut item = applied_item("dep", from, to, false);
    item.edge = Some(crate::app::UpgradeEdgeInfo {
        dependent: dependent.to_string(),
        dependent_version: "1.0.0".to_string(),
        dependent_source: None,
        action: EdgeBindingAction::Rebound,
        detail: None,
    });
    item
}

fn no_prior() -> HashSet<BaselineViolation> {
    HashSet::new()
}

fn violations(items: &[(&str, &str)]) -> HashSet<BaselineViolation> {
    items
        .iter()
        .map(|(name, version)| violation(name, version, Some("crates.io")))
        .collect()
}

fn violation(name: &str, version: &str, registry: Option<&str>) -> BaselineViolation {
    violation_for(ToolId("cargo"), name, version, registry)
}

fn violation_for(
    tool: ToolId,
    name: &str,
    version: &str,
    registry: Option<&str>,
) -> BaselineViolation {
    BaselineViolation {
        package: PackageId::new(tool, name, registry.map(ToString::to_string)),
        version: Version::new(version),
    }
}

fn no_kind(_: &str, _: &str) -> Option<UpdateKind> {
    None
}

#[test]
fn restore_conflict_retains_rows_from_an_earlier_committed_batch() {
    let mut committed = BatchOutcome::default();
    committed
        .items
        .push(applied_item("kept", "1.0.0", "1.1.0", false));
    committed.mark_committed(CommittedBatch {
        violations_after: HashSet::new(),
        reconcile_needed: false,
    });
    let mut failed = BatchOutcome::default();
    failed.errors.push(Diagnostic::new(
        DiagnosticKind::LockConflict,
        "later batch could not restore",
    ));

    let retained = retained_trial_outcomes(vec![committed, failed]);

    assert!(retained.has_restore_conflict());
    assert_eq!(retained.items.len(), 1);
    assert_eq!(retained.items[0].name, "kept");
    assert!(retained.items[0].applied);
    assert_eq!(retained.errors.len(), 1);
}

#[test]
fn residual_gate_allows_a_pre_existing_violation_to_float_versions() {
    let before = violations(&[("t", "0.5.0")]);
    let after = violations(&[("t", "0.6.0")]);

    assert!(
        newly_introduced_violations(&before, &after).is_empty(),
        "one dirty version line stayed one dirty version line"
    );
}

#[test]
fn residual_gate_flags_an_added_version_line_for_a_dirty_package() {
    let before = violations(&[("t", "0.5.0")]);
    let after = violations(&[("t", "0.5.0"), ("t", "1.0.0")]);

    assert_eq!(
        newly_introduced_violations(&before, &after),
        vec![violation("t", "1.0.0", Some("crates.io"))]
    );
}

#[test]
fn residual_gate_flags_a_new_dirty_package() {
    let before = violations(&[("t", "0.5.0")]);
    let after = violations(&[("other", "2.0.0"), ("t", "0.5.0")]);

    assert_eq!(
        newly_introduced_violations(&before, &after),
        vec![violation("other", "2.0.0", Some("crates.io"))]
    );
}

#[test]
fn residual_gate_distinguishes_same_version_from_two_sources() {
    let from_registry_a = violation("foo", "1.0.0", Some("registry-a"));
    let from_registry_b = violation("foo", "1.0.0", Some("registry-b"));
    let before = HashSet::from([from_registry_a.clone()]);
    let after = HashSet::from([from_registry_a, from_registry_b.clone()]);

    assert_eq!(
        newly_introduced_violations(&before, &after),
        vec![from_registry_b]
    );
}

#[test]
fn graph_violation_state_remains_distinct_for_source_twins() {
    let mut registry_a = dep("foo", "1.0.0");
    registry_a.package.registry = Some("registry-a".to_string());
    registry_a.graph_floor = Some(Version::new("0.9.0"));
    let mut registry_b = dep("foo", "1.0.0");
    registry_b.package.registry = Some("registry-b".to_string());
    registry_b.graph_floor = Some(Version::new("1.0.0"));
    let mut violations = std::collections::HashMap::new();

    insert_graph_violation(&mut violations, &registry_a);
    insert_graph_violation(&mut violations, &registry_b);

    assert_eq!(violations.len(), 2);
    assert_eq!(
        violations.get(&violation_for(
            ToolId("mock"),
            "foo",
            "1.0.0",
            Some("registry-a")
        )),
        Some(&true)
    );
    assert_eq!(
        violations.get(&violation_for(
            ToolId("mock"),
            "foo",
            "1.0.0",
            Some("registry-b")
        )),
        Some(&false)
    );
}

#[test]
fn residual_gate_flags_replacement_by_a_source_distinct_package() {
    let before = HashSet::from([violation("foo", "1.0.0", Some("registry-a"))]);
    let replacement = violation("foo", "1.0.0", Some("registry-b"));
    let after = HashSet::from([replacement.clone()]);

    assert_eq!(
        newly_introduced_violations(&before, &after),
        vec![replacement]
    );
}

#[test]
fn planned_baseline_violations_preserve_source_identity() {
    let registry_a = violation("foo", "1.0.0", Some("registry-a"));
    let registry_b = violation("foo", "1.0.0", Some("registry-b"));

    let planned = plan_baseline_violations(&HashSet::from([registry_b, registry_a]));

    assert_eq!(planned.len(), 2);
    assert_eq!(planned[0].package.registry.as_deref(), Some("registry-a"));
    assert_eq!(planned[1].package.registry.as_deref(), Some("registry-b"));
}

#[test]
fn policy_violation_labels_include_a_redacted_source() {
    let violation = violation(
        "foo",
        "1.0.0",
        Some("git+https://token@example.com/private/repo.git?access_token=secret"),
    );

    assert_eq!(
        package_label(&violation.package),
        "foo from git:example.com/private/repo"
    );
    assert_eq!(
        violation_identity(&violation),
        "foo@1.0.0 from git:example.com/private/repo"
    );
}

#[test]
fn collapse_merges_float_then_reconcile_into_a_net_forward_row() {
    // The forward batch floats `quote` up (collateral); the reconcile pass matures it back down.
    let mut items = vec![
        applied_item("quote", "1.0.44", "1.0.46", false),
        applied_item("quote", "1.0.46", "1.0.45", true),
    ];
    collapse_applied_legs(&mut items, ".", "cargo", &no_prior(), no_kind);
    assert_eq!(items.len(), 1, "the two legs collapse to one net row");
    assert_eq!(items[0].from, "1.0.44");
    assert_eq!(items[0].to, "1.0.45");
    assert!(
        !items[0].downgrade,
        "the net move (1.0.44 → 1.0.45) is forward"
    );
}

#[test]
fn collapse_reclassifies_kind_against_the_net_target_when_available() {
    // The first leg is a minor float-up, but the committed net row is only a patch move.
    // The report kind should describe the collapsed row, not the phantom intermediate.
    let mut items = vec![
        applied_item("quote", "1.0.0", "1.1.0", false),
        applied_item("quote", "1.1.0", "1.0.1", true),
    ];
    collapse_applied_legs(&mut items, ".", "cargo", &no_prior(), |from, to| {
        if from == "1.0.0" && to == "1.0.1" {
            Some(UpdateKind::Patch)
        } else {
            None
        }
    });
    assert_eq!(items.len(), 1);
    assert_eq!(
        (items[0].from.as_str(), items[0].to.as_str()),
        ("1.0.0", "1.0.1")
    );
    assert_eq!(items[0].kind, UpdateKind::Patch);
    assert!(!items[0].downgrade);
}

#[test]
fn collapse_drops_a_package_that_floats_up_then_fully_back() {
    let mut items = vec![
        applied_item("quote", "1.0.44", "1.0.46", false),
        applied_item("quote", "1.0.46", "1.0.44", true),
    ];
    collapse_applied_legs(&mut items, ".", "cargo", &no_prior(), no_kind);
    assert!(
        items.is_empty(),
        "no net move: the package is dropped from the report"
    );
}

#[test]
fn collapse_keeps_single_leg_rows_including_a_genuine_downgrade() {
    // A direct forward bump and a pre-existing too-fresh transitive matured down in one leg each
    // stay as they are — only multi-leg float-then-reconcile chains merge.
    let mut a = applied_item("a", "1.0.0", "1.1.0", false);
    a.direct = true;
    let mut items = vec![a, applied_item("referencing", "0.46.6", "0.46.5", true)];
    collapse_applied_legs(&mut items, ".", "cargo", &no_prior(), no_kind);
    assert_eq!(items.len(), 2);
    let refr = items
        .iter()
        .find(|item| item.name == "referencing")
        .expect("referencing row");
    assert_eq!((refr.from.as_str(), refr.to.as_str()), ("0.46.6", "0.46.5"));
    assert!(refr.downgrade, "a single-leg downgrade is preserved");
}

#[test]
fn collapse_does_not_merge_across_projects() {
    let mut first = applied_item("quote", "1.0.44", "1.0.46", false);
    first.project = "a".to_string();
    let mut second = applied_item("quote", "1.0.46", "1.0.45", true);
    second.project = "b".to_string();
    let mut items = vec![first, second];
    collapse_applied_legs(&mut items, "a", "cargo", &no_prior(), no_kind);
    // Only project `a` is in scope, and it has a single leg there, so nothing merges.
    assert_eq!(items.len(), 2);
}

#[test]
fn collapse_marks_a_net_downgrade_when_the_start_was_a_prior_violation() {
    // A pre-existing too-fresh `quote 1.0.5` floats up to 1.0.7, then the reconcile pass matures
    // its line down past the start to 1.0.4 — a genuine net downgrade, not a forward move.
    let mut items = vec![
        applied_item("quote", "1.0.5", "1.0.7", false),
        applied_item("quote", "1.0.7", "1.0.4", true),
    ];
    let prior: HashSet<BaselineViolation> = HashSet::from([BaselineViolation {
        package: PackageId::new(ToolId("cargo"), "quote", Some("crates.io".to_string())),
        version: Version::new("1.0.5"),
    }]);
    collapse_applied_legs(&mut items, ".", "cargo", &prior, no_kind);
    assert_eq!(items.len(), 1);
    assert_eq!(
        (items[0].from.as_str(), items[0].to.as_str()),
        ("1.0.5", "1.0.4")
    );
    assert!(
        items[0].downgrade,
        "a net move below a too-fresh start is a downgrade, not an upgrade"
    );
}

#[test]
fn collapse_does_not_merge_two_coexisting_majors_of_one_crate() {
    // Cargo keeps two majors of `serde` in the lock; both bump in one run.
    // They share the (name, registry) key but their versions do not chain, so neither row is merged
    // or dropped.
    let mut items = vec![
        applied_item("serde", "0.9.1", "0.9.3", false),
        applied_item("serde", "1.0.0", "1.0.5", false),
    ];
    collapse_applied_legs(&mut items, ".", "cargo", &no_prior(), no_kind);
    assert_eq!(items.len(), 2, "independent version lines stay distinct");
    let lines: HashSet<(String, String)> = items
        .iter()
        .map(|item| (item.from.clone(), item.to.clone()))
        .collect();
    assert!(lines.contains(&("0.9.1".to_string(), "0.9.3".to_string())));
    assert!(lines.contains(&("1.0.0".to_string(), "1.0.5".to_string())));
}

#[test]
fn collapse_leaves_reciprocal_edge_rows_independent() {
    let mut items = vec![
        edge_item("consumer-a", "1.0.0", "2.0.0"),
        edge_item("consumer-b", "2.0.0", "1.0.0"),
    ];

    collapse_applied_legs(&mut items, ".", "cargo", &no_prior(), no_kind);

    assert_eq!(items.len(), 2, "edge relationships are not version legs");
    assert_eq!(
        items
            .iter()
            .filter_map(|item| item.edge.as_ref().map(|edge| edge.dependent.as_str()))
            .collect::<HashSet<_>>(),
        HashSet::from(["consumer-a", "consumer-b"])
    );
}

#[test]
fn collapse_does_not_chain_an_edge_row_with_a_version_row() {
    let mut items = vec![
        applied_item("dep", "1.0.0", "2.0.0", false),
        edge_item("consumer", "2.0.0", "3.0.0"),
    ];

    collapse_applied_legs(&mut items, ".", "cargo", &no_prior(), no_kind);

    assert_eq!(items.len(), 2);
    assert_eq!(
        (items[0].from.as_str(), items[0].to.as_str()),
        ("1.0.0", "2.0.0")
    );
    let edge = items[1].edge.as_ref().expect("edge row");
    assert_eq!(edge.dependent, "consumer");
    assert_eq!(
        (items[1].from.as_str(), items[1].to.as_str()),
        ("2.0.0", "3.0.0")
    );
}

#[test]
fn lock_status_aggregation_keeps_unknown_distinct_from_verified_current() {
    let current_then_unknown = combine_lock_status(Some(LockStatus::Current), LockStatus::Unknown);
    assert_eq!(current_then_unknown, LockStatus::Unknown);

    let unknown_then_stale = combine_lock_status(Some(LockStatus::Unknown), LockStatus::Stale);
    assert_eq!(unknown_then_stale, LockStatus::Stale);
}

#[test]
fn is_downgrade_compares_release_order() {
    let releases = [rel("1.0.0", 0), rel("1.0.1", 1), rel("1.0.2", 2)];
    // Rolling a too-fresh pin back to an older release is a downgrade.
    assert!(is_downgrade(
        &releases,
        &Version::new("1.0.2"),
        &Version::new("1.0.1")
    ));
    // A forward move is not.
    assert!(!is_downgrade(
        &releases,
        &Version::new("1.0.0"),
        &Version::new("1.0.2")
    ));
    // A version not in the set is treated as not-a-downgrade.
    assert!(!is_downgrade(
        &releases,
        &Version::new("1.0.0"),
        &Version::new("9.9.9")
    ));
}

fn go(name: &str) -> PackageId {
    PackageId::new(ToolId("go"), name, None)
}

fn major(key: &str) -> MajorKey {
    MajorKey(key.to_string())
}

#[test]
fn target_package_rewrites_a_go_path_major_in_both_directions() {
    // Upgrade base → /v2 and /v2 → /v3.
    assert_eq!(
        target_package(&go("example.com/foo"), &major(""), &major("/v2")).name,
        "example.com/foo/v2"
    );
    assert_eq!(
        target_package(&go("example.com/foo/v2"), &major("/v2"), &major("/v3")).name,
        "example.com/foo/v3"
    );
    // Downgrade /v3 → /v2 and /v2 → the v1 base path — the moves `fix --major` now makes.
    assert_eq!(
        target_package(&go("example.com/foo/v3"), &major("/v3"), &major("/v2")).name,
        "example.com/foo/v2"
    );
    assert_eq!(
        target_package(&go("example.com/foo/v2"), &major("/v2"), &major("")).name,
        "example.com/foo"
    );
}

#[test]
fn target_package_keeps_the_name_when_the_major_is_version_derived() {
    // A registry tool's `MajorKey` is a bare version axis (`0.23` → `0.25`), not a path suffix,
    // so the package name is stable across majors.
    let cargo = PackageId::new(ToolId("cargo"), "toml_edit", Some("crates.io".to_string()));
    assert_eq!(
        target_package(&cargo, &major("0.23"), &major("0.25")).name,
        "toml_edit"
    );
    // A same-major move keeps the name too.
    assert_eq!(
        target_package(&go("example.com/foo/v2"), &major("/v2"), &major("/v2")).name,
        "example.com/foo/v2"
    );
}
