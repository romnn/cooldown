use crate::gocmd::Go;
use crate::mutation;
use cooldown_core::{
    ApplyReport, Change, CoreError, Plan, Project, ProjectMutationJournal, Result, SkipReason,
    Skipped,
};

pub(super) async fn apply(
    go: &Go,
    project: &Project,
    plan: &Plan,
    journal: &ProjectMutationJournal,
) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    for change in &plan.changes {
        let target_path = &change.package.name;
        match go.get(&project.root, target_path, change.to.as_str()).await {
            Ok(()) => {
                // Cross-major path change → rewrite imports old→new before accepting the trial.
                if let Some(old_path) = mutation::old_import_path(change)
                    && old_path != *target_path
                {
                    mutation::rewrite_imports(&project.root, &old_path, target_path, journal)?;
                }
                report.applied.push(change.clone());
            }
            Err(error) => report.skipped.push(skipped_on_apply_error(change, error)?),
        }
    }
    // Re-tidy once after applying the (single-change) plan.
    if !report.applied.is_empty() {
        go.mod_tidy(&project.root).await?;
    }
    Ok(report)
}

pub(super) fn skipped_on_apply_error(change: &Change, error: CoreError) -> Result<Skipped> {
    if error.is_tool_spawn_failure() {
        return Err(error);
    }
    Ok(Skipped {
        change: change.clone(),
        reason: SkipReason::ResolverConflict,
        offending: Some(change.package.clone()),
    })
}
