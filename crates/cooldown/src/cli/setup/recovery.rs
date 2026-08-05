use super::{detect, scan_root_for};
use crate::app::{Progress, RecoveryTarget};
use crate::cli::GlobalArgs;
use crate::{discovery, scan};
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_cargo::{
    CARGO_ID, RecoveryScope, is_recovery_artifact_name, recover_interrupted_mutation,
    recovery_authority_projects,
};
use cooldown_core::{
    CoreError, Diagnostic, ProjectDetection, ProjectMarker, ToolId, recognized_tool_names, tool_id,
};

struct RecoveryOption {
    id: &'static str,
    class: RecoveryOptionClass,
}

#[derive(Clone, Copy)]
enum RecoveryOptionClass {
    Used,
    Rejected {
        flag: &'static str,
        is_set: fn(&GlobalArgs) -> bool,
    },
}

macro_rules! recovery_options {
    (
        used: [$($used:literal),* $(,)?],
        rejected: [$($(#[$meta:meta])* $id:literal => ($flag:literal, $is_set:expr)),* $(,)?],
    ) => {
        const RECOVERY_OPTIONS: &[RecoveryOption] = &[
            $(RecoveryOption {
                id: $used,
                class: RecoveryOptionClass::Used,
            },)*
            $($(#[$meta])*
            RecoveryOption {
                id: $id,
                class: RecoveryOptionClass::Rejected {
                    flag: $flag,
                    is_set: $is_set,
                },
            },)*
        ];
    };
}

recovery_options! {
    used: [
        "tool",
        "cargo",
        "no_gitignore",
        "log_level",
        "no_progress",
        "dir",
        "json",
        "color",
    ],
    rejected: [
        "min_age" => ("--min-age", |global| global.min_age.is_some()),
        "min_age_major" => ("--min-age-major", |global| global.min_age_major.is_some()),
        "min_age_minor" => ("--min-age-minor", |global| global.min_age_minor.is_some()),
        "min_age_patch" => ("--min-age-patch", |global| global.min_age_patch.is_some()),
        "latest" => ("--latest", |global| global.latest),
        "freeze" => ("--freeze", |global| global.freeze.is_some()),
        "allow" => ("--allow", |global| !global.allow.is_empty()),
        "major" => ("--major", |global| global.major),
        "no_major" => ("--no-major", |global| global.no_major),
        "respect_dist_tags" => ("--respect-dist-tags", |global| global.respect_dist_tags),
        "no_respect_dist_tags" => ("--no-respect-dist-tags", |global| global.no_respect_dist_tags),
        "package" => ("--package", |global| !global.package.is_empty()),
        "exclude_folders" => ("--exclude-folders", |global| !global.exclude_folders.is_empty()),
        "exclude_packages" => ("--exclude-packages", |global| !global.exclude_packages.is_empty()),
        "list_packages" => ("--list-packages", |global| global.list_packages),
        "paths" => ("--paths", |global| global.paths),
        "show_projects" => ("--show-projects", |global| global.show_projects),
        "no_suggestions" => ("--no-suggestions", |global| global.no_suggestions),
        "allow_stale_lock" => ("--allow-stale-lock", |global| global.allow_stale_lock),
        "sync" => ("--sync", |global| global.sync),
        "dry_run" => ("--dry-run", |global| global.dry_run),
        "offline" => ("--offline", |global| global.offline),
        "fresh" => ("--fresh", |global| global.fresh),
        "concurrency" => ("--concurrency", |global| global.concurrency.is_some()),
        "no_native" => ("--no-native", |global| global.no_native),
        "no_global" => ("--no-global", |global| global.no_global),
        "config" => ("--config", |global| global.config.is_some()),
        #[cfg(debug_assertions)]
        "now" => ("--now", |global| global.now.is_some()),
    ],
}

pub(in crate::cli) fn configure_recovery_help(mut command: clap::Command) -> clap::Command {
    // Global arguments reach subcommands only when Clap builds the command graph.
    // Build first so the recovery subcommand can hide every option its minimal bootstrap rejects.
    command.build();
    command.mut_subcommand("recover", |subcommand| {
        subcommand.mut_args(|argument| {
            let id = argument.get_id().as_str();
            let rejected = RECOVERY_OPTIONS.iter().any(|option| {
                option.id == id && matches!(option.class, RecoveryOptionClass::Rejected { .. })
            });
            if rejected {
                argument.hide(true)
            } else {
                match id {
                    "tool" => argument.help(
                        "Select Cargo recovery (`cargo` or `rust`); other tools are rejected",
                    ),
                    "cargo" => argument
                        .help("Recover Cargo state explicitly (shorthand for `--tool cargo`)"),
                    _ => argument,
                }
            }
        })
    })
}

pub(in crate::cli) struct PreparedRecovery {
    pub(in crate::cli) targets: Vec<RecoveryTarget>,
    pub(in crate::cli) warnings: Vec<Diagnostic>,
    pub(in crate::cli) json: bool,
    pub(in crate::cli) progress: Progress,
}

/// Discovers recovery artifacts without loading normal run configuration or package state.
pub(in crate::cli) fn prepare_recovery(global: &GlobalArgs) -> Result<PreparedRecovery, CoreError> {
    validate_recovery_options(global)?;
    let tools = selected_recovery_tools(global)?;
    let workdir = detect::workdir(global)?;
    let repo_root = discovery::find_repo_root(&workdir);
    let scope = if global.dir.is_some() {
        RecoveryScope::explicit(&workdir)?
    } else {
        RecoveryScope::repository(&scan_root_for(&workdir, &repo_root))?
    };
    let mut targets = Vec::new();
    let mut warnings = Vec::new();
    if tools.is_empty() || tools.contains(&CARGO_ID) {
        let mut roots = direct_cargo_recovery_roots(&workdir, &repo_root)?;
        let discovery = cargo_recovery_discovery(&scope, !global.no_gitignore)?;
        roots.extend(discovery.roots);
        warnings.extend(discovery.warnings);
        roots.sort();
        roots.dedup();
        for root in roots.into_iter().filter(|root| scope.includes(root)) {
            let project = relative_project(&repo_root, &root);
            targets.push(RecoveryTarget::new(
                CARGO_ID,
                root,
                project,
                recover_interrupted_mutation,
            ));
        }
    }
    Ok(PreparedRecovery {
        targets,
        warnings,
        json: global.json,
        progress: super::options::progress_mode(global),
    })
}

fn validate_recovery_options(global: &GlobalArgs) -> Result<(), CoreError> {
    if let Some((option, flag)) = RECOVERY_OPTIONS.iter().find_map(|option| {
        let RecoveryOptionClass::Rejected { flag, is_set } = option.class else {
            return None;
        };
        is_set(global).then_some((option, flag))
    }) {
        tracing::debug!(argument = option.id, "rejecting irrelevant recovery option");
        return Err(CoreError::Config(format!(
            "`recover` does not use `{flag}`; remove it so recovery depends only on project location and recovery artifacts"
        )));
    }
    Ok(())
}

fn direct_cargo_recovery_roots(
    workdir: &Utf8Path,
    repo_root: &Utf8Path,
) -> Result<Vec<Utf8PathBuf>, CoreError> {
    let mut roots = Vec::new();
    for ancestor in workdir.ancestors() {
        if contains_cargo_recovery_artifact(ancestor)? {
            roots.push(ancestor.to_owned());
        }
        if ancestor == repo_root {
            break;
        }
    }
    Ok(roots)
}

fn contains_cargo_recovery_artifact(root: &Utf8Path) -> Result<bool, CoreError> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(is_recovery_artifact_name)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn selected_recovery_tools(global: &GlobalArgs) -> Result<Vec<ToolId>, CoreError> {
    let names = if global.cargo {
        vec![CARGO_ID.as_str().to_string()]
    } else {
        global.tool.clone()
    };
    let tools = names
        .iter()
        .map(|name| {
            tool_id(name).ok_or_else(|| {
                CoreError::Config(format!(
                    "unknown --tool `{name}`; recognised: {}",
                    recognized_tool_names()
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(tool) = tools.iter().find(|tool| **tool != CARGO_ID) {
        return Err(CoreError::Config(format!(
            "`recover --tool {}` is unsupported; recovery currently supports Cargo only; use `--tool cargo` or `--cargo`",
            tool.as_str()
        )));
    }
    Ok(tools)
}

#[cfg(test)]
fn cargo_recovery_roots(
    scope: &RecoveryScope,
    respect_gitignore: bool,
) -> Result<Vec<Utf8PathBuf>, CoreError> {
    Ok(cargo_recovery_discovery(scope, respect_gitignore)?.roots)
}

struct CargoRecoveryDiscovery {
    roots: Vec<Utf8PathBuf>,
    warnings: Vec<Diagnostic>,
}

fn cargo_recovery_discovery(
    scope: &RecoveryScope,
    respect_gitignore: bool,
) -> Result<CargoRecoveryDiscovery, CoreError> {
    let root = scope.root();
    let mut authoritative = scan::find_recovery_artifact_dirs(root, is_recovery_artifact_name)?;
    let authority = recovery_authority_projects(scope)?;
    authoritative.extend(authority.projects);
    let warnings = authority
        .warnings
        .into_iter()
        .map(|warning| {
            Diagnostic::new(
                warning.error.diagnostic_kind(),
                format!(
                    "could not validate Cargo recovery authority at {}: {}",
                    warning.path, warning.error
                ),
            )
            .with_tool(CARGO_ID.as_str())
            .with_path(warning.path.as_str())
        })
        .collect();
    let detected = scan::find_project_marker_dirs_batch(
        root,
        &[ProjectDetection::PrimaryWithValidation {
            primary: ProjectMarker {
                lockfile: "Cargo.lock",
                manifest: "Cargo.toml",
                alternate_manifests: &[],
                workspace_root: true,
            },
            validation_marker: "Cargo.toml",
        }],
        respect_gitignore,
        &[],
    )?
    .into_iter()
    .next()
    .ok_or_else(|| CoreError::System("Cargo recovery scan produced no result".to_string()))?;
    authoritative.extend(detected.primary);
    authoritative.sort();
    authoritative.dedup();

    let mut roots = authoritative.clone();
    roots.extend(detected.validation_only.into_iter().filter(|manifest| {
        !authoritative
            .iter()
            .any(|known| manifest.starts_with(known) || known.starts_with(manifest))
    }));
    roots.sort();
    roots.dedup();
    Ok(CargoRecoveryDiscovery { roots, warnings })
}

fn relative_project(repo_root: &Utf8Path, root: &Utf8Path) -> String {
    root.strip_prefix(repo_root).ok().map_or_else(
        || root.to_string(),
        |relative| {
            if relative.as_str().is_empty() {
                ".".to_string()
            } else {
                relative.to_string()
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;
    use color_eyre::eyre;
    use cooldown_cargo::RECOVERY_MARKER;

    fn explicit_scope(root: &Utf8Path) -> cooldown_core::Result<RecoveryScope> {
        RecoveryScope::explicit(root)
    }

    #[test]
    fn recovery_rejects_normal_run_options_it_does_not_use() -> eyre::Result<()> {
        let cli = Cli::parse_from(["cooldown", "recover", "--offline"]);

        let error = prepare_recovery(&cli.global)
            .err()
            .ok_or_else(|| eyre::eyre!("recovery accepted an unused registry option"))?;

        assert!(error.to_string().contains("--offline"));
        Ok(())
    }

    #[test]
    fn recovery_rejects_a_selected_unsupported_ecosystem() -> eyre::Result<()> {
        let cli = Cli::parse_from(["cooldown", "recover", "--tool", "npm"]);

        let error = prepare_recovery(&cli.global)
            .err()
            .ok_or_else(|| eyre::eyre!("recovery accepted an unsupported ecosystem"))?;

        std::assert_matches!(
            error,
            CoreError::Config(message)
                if message.contains("recover --tool npm")
                    && message.contains("currently supports Cargo only")
        );
        Ok(())
    }

    #[test]
    fn every_global_argument_has_an_explicit_recovery_classification() {
        let actual = Cli::command()
            .get_arguments()
            .filter(|argument| argument.is_global_set())
            .map(|argument| argument.get_id().as_str().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        let classified = RECOVERY_OPTIONS
            .iter()
            .map(|option| option.id)
            .map(str::to_string)
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(actual, classified);
    }

    #[test]
    fn recovery_help_lists_only_options_used_by_recovery() -> eyre::Result<()> {
        let error = Cli::command()
            .try_get_matches_from(["cooldown", "recover", "--help"])
            .err()
            .ok_or_else(|| eyre::eyre!("recovery help unexpectedly parsed as a command"))?;
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = error.to_string();

        for supported in ["--tool", "--cargo", "--no-gitignore", "--json", "--color"] {
            assert!(
                help.contains(supported),
                "recovery help omitted {supported}"
            );
        }
        assert!(help.contains("Select Cargo recovery"));
        assert!(help.contains("other tools are rejected"));
        for rejected in [
            "--min-age",
            "--allow",
            "--major",
            "--sync",
            "--dry-run",
            "--offline",
        ] {
            assert!(
                !help.contains(rejected),
                "recovery help advertised rejected option {rejected}"
            );
        }
        Ok(())
    }

    #[test]
    fn recovery_marker_finds_project_without_cargo_lock() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary directory is not UTF-8"))?;
        std::fs::write(root.join(RECOVERY_MARKER), "{}")?;

        assert_eq!(
            cargo_recovery_roots(&explicit_scope(root)?, false)?,
            vec![root.to_owned()]
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_marker_symlink_is_discovered_for_fail_closed_validation() -> eyre::Result<()> {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        symlink("missing-record", root.join(RECOVERY_MARKER))?;

        assert_eq!(
            direct_cargo_recovery_roots(root, root)?,
            vec![root.to_owned()]
        );
        assert_eq!(
            cargo_recovery_roots(&explicit_scope(root)?, false)?,
            vec![root.to_owned()]
        );
        Ok(())
    }

    #[test]
    fn nested_recovery_marker_is_not_hidden_by_an_outer_lock() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary directory is not UTF-8"))?;
        let nested = root.join("tools/independent");
        std::fs::create_dir_all(&nested)?;
        std::fs::write(root.join("Cargo.lock"), "")?;
        std::fs::write(nested.join(RECOVERY_MARKER), "{}")?;

        assert_eq!(
            cargo_recovery_roots(&explicit_scope(root)?, false)?,
            vec![root.to_owned(), nested]
        );
        Ok(())
    }

    #[test]
    fn recovery_setup_ignores_broken_normal_run_inputs() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(directory.path()).expect("UTF-8 tempdir");
        std::fs::create_dir(root.join(".git")).expect("create repository marker");
        std::fs::write(root.join("cooldown.toml"), "not valid = [").expect("write bad config");
        std::fs::write(root.join("Cargo.toml"), "not valid Cargo TOML").expect("write manifest");
        std::fs::write(root.join("Cargo.lock"), "not valid Cargo lock").expect("write lock");
        std::fs::create_dir(root.join(".cooldown-baseline.json"))
            .expect("create unreadable baseline shape");
        let cli = Cli::parse_from(["cooldown", "recover", "-C", root.as_str(), "--cargo"]);

        let prepared = prepare_recovery(&cli.global).expect("prepare recovery");

        assert_eq!(prepared.targets.len(), 1);
        assert_eq!(prepared.targets[0].root, root);
    }

    #[test]
    fn explicit_ignored_project_is_still_a_recovery_target() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let project = root.join("ignored/project");
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::create_dir_all(&project)?;
        std::fs::write(root.join(".gitignore"), "ignored/\n")?;
        std::fs::write(project.join(RECOVERY_MARKER), "{}")?;
        let cli = Cli::parse_from(["cooldown", "recover", "-C", project.as_str(), "--cargo"]);

        let prepared = prepare_recovery(&cli.global)?;

        assert_eq!(prepared.targets.len(), 1);
        assert_eq!(prepared.targets[0].root, project);
        Ok(())
    }

    #[test]
    fn explicit_hidden_project_is_still_a_recovery_target() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let project = root.join(".hidden/project");
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::create_dir_all(&project)?;
        std::fs::write(project.join(RECOVERY_MARKER), "{}")?;
        let cli = Cli::parse_from(["cooldown", "recover", "-C", project.as_str(), "--cargo"]);

        let prepared = prepare_recovery(&cli.global)?;

        assert_eq!(prepared.targets.len(), 1);
        assert_eq!(prepared.targets[0].root, project);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn explicit_recovery_excludes_sibling_authority_projects() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let first = root.join("projects/first");
        let second = root.join("projects/second");
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")?;
        for project in [&first, &second] {
            std::fs::create_dir_all(project)?;
            let canonical = Utf8PathBuf::from_path_buf(std::fs::canonicalize(project)?)
                .map_err(|path| eyre::eyre!("non-UTF-8 project path: {path:?}"))?;
            let coordination = cooldown_core::fs::ProjectCoordination::resolve(&canonical)?;
            let authority = coordination
                .recovery_authority()
                .ok_or_else(|| eyre::eyre!("test project has no recovery authority"))?;
            let anchor = authority.directory().join(format!(
                "{:016x}.cargo-recovery.anchor",
                cooldown_core::fs::fnv1a_64(canonical.as_str())
            ));
            std::fs::write(
                &anchor,
                serde_json::to_vec(&serde_json::json!({
                    "format": "cooldown-cargo-recovery-anchor-v1",
                    "project_root": canonical.as_str(),
                    "record_digest": "0".repeat(64),
                }))?,
            )?;
            std::fs::set_permissions(&anchor, std::fs::Permissions::from_mode(0o600))?;
        }
        let cli = Cli::parse_from(["cooldown", "recover", "-C", first.as_str(), "--cargo"]);

        let prepared = prepare_recovery(&cli.global)?;

        assert_eq!(prepared.targets.len(), 1);
        assert_eq!(prepared.targets[0].root, first);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn explicit_recovery_warns_about_an_unattributable_descendant_authority() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let parent = root.join("projects");
        let descendant = parent.join(".hidden/interrupted");
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")?;
        std::fs::create_dir_all(&descendant)?;
        let descendant = Utf8PathBuf::from_path_buf(std::fs::canonicalize(descendant)?)
            .map_err(|path| eyre::eyre!("non-UTF-8 project path: {path:?}"))?;
        let coordination = cooldown_core::fs::ProjectCoordination::resolve(&descendant)?;
        let authority = coordination
            .recovery_authority()
            .ok_or_else(|| eyre::eyre!("test project has no recovery authority"))?;
        let anchor = authority.directory().join(format!(
            "{:016x}.cargo-recovery.anchor",
            cooldown_core::fs::fnv1a_64(descendant.as_str())
        ));
        std::fs::write(&anchor, "not valid recovery authority")?;
        std::fs::set_permissions(&anchor, std::fs::Permissions::from_mode(0o600))?;
        let cli = Cli::parse_from(["cooldown", "recover", "-C", parent.as_str(), "--cargo"]);

        let prepared = prepare_recovery(&cli.global)?;

        assert_eq!(prepared.warnings.len(), 1);
        assert!(
            prepared.warnings[0]
                .message
                .contains(anchor.to_string_lossy().as_ref())
        );
        Ok(())
    }

    #[test]
    fn repository_recovery_finds_hidden_and_ignored_orphan_artifacts() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let hidden = root.join(".hidden/project");
        let ignored = root.join("ignored/project");
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::create_dir_all(&hidden)?;
        std::fs::create_dir_all(&ignored)?;
        std::fs::write(root.join(".gitignore"), "ignored/\n")?;
        std::fs::write(hidden.join(RECOVERY_MARKER), "{}")?;
        std::fs::write(
            ignored.join(".Cargo.lock.cooldown-recovery.123.0.publish"),
            "{}",
        )?;
        let cli = Cli::parse_from(["cooldown", "recover", "-C", root.as_str(), "--cargo"]);

        let prepared = prepare_recovery(&cli.global)?;
        let roots = prepared
            .targets
            .into_iter()
            .map(|target| target.root)
            .collect::<Vec<_>>();
        assert_eq!(roots, vec![hidden, ignored]);
        Ok(())
    }

    #[test]
    fn repository_recovery_finds_anchor_only_project_in_a_pruned_directory() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let project = root.join("target/.hidden/project");
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")?;
        std::fs::create_dir_all(&project)?;
        let project = Utf8PathBuf::from_path_buf(std::fs::canonicalize(&project)?)
            .map_err(|path| eyre::eyre!("non-UTF-8 project path: {path:?}"))?;
        let coordination = cooldown_core::fs::ProjectCoordination::resolve(&project)?;
        let authority = coordination
            .recovery_authority()
            .ok_or_else(|| eyre::eyre!("test project has no recovery authority"))?;
        let anchor = authority.directory().join(format!(
            "{:016x}.cargo-recovery.anchor",
            cooldown_core::fs::fnv1a_64(project.as_str())
        ));
        std::fs::write(
            &anchor,
            serde_json::to_vec(&serde_json::json!({
                "format": "cooldown-cargo-recovery-anchor-v1",
                "project_root": project.as_str(),
                "record_digest": "0".repeat(64),
            }))?,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&anchor, std::fs::Permissions::from_mode(0o600))?;
        }
        let cli = Cli::parse_from(["cooldown", "recover", "-C", root.as_str(), "--cargo"]);

        let prepared = prepare_recovery(&cli.global)?;

        assert!(prepared.targets.iter().any(|target| target.root == project));
        Ok(())
    }

    #[test]
    fn explicit_pruned_project_finds_an_orphan_publication() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let project = root.join("target/fixture");
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::create_dir_all(&project)?;
        std::fs::write(
            project.join(".Cargo.lock.cooldown-recovery.123.0.publish"),
            "{}",
        )?;
        let cli = Cli::parse_from(["cooldown", "recover", "-C", project.as_str(), "--cargo"]);

        let prepared = prepare_recovery(&cli.global)?;

        assert_eq!(prepared.targets.len(), 1);
        assert_eq!(prepared.targets[0].root, project);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn explicit_recovery_does_not_traverse_unrelated_repository_siblings() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let project = root.join("crates/target-project");
        let inaccessible = root.join("crates/unrelated");
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::create_dir_all(&project)?;
        std::fs::create_dir_all(&inaccessible)?;
        std::fs::write(project.join(RECOVERY_MARKER), "{}")?;
        std::fs::set_permissions(&inaccessible, std::fs::Permissions::from_mode(0o000))?;
        let cli = Cli::parse_from(["cooldown", "recover", "-C", project.as_str(), "--cargo"]);

        let result = prepare_recovery(&cli.global);
        std::fs::set_permissions(&inaccessible, std::fs::Permissions::from_mode(0o700))?;
        let prepared = result?;

        assert_eq!(prepared.targets.len(), 1);
        assert_eq!(prepared.targets[0].root, project);
        Ok(())
    }

    #[test]
    fn repository_recovery_from_a_nested_workdir_includes_sibling_projects() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let workdir = root.join("crates/current");
        let sibling = root.join("tools/interrupted");
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::create_dir_all(&workdir)?;
        std::fs::create_dir_all(&sibling)?;
        std::fs::write(sibling.join(RECOVERY_MARKER), "{}")?;
        let scope = RecoveryScope::repository(&scan_root_for(&workdir, root))?;

        let roots = cargo_recovery_roots(&scope, false)?;

        assert!(roots.contains(&sibling));
        Ok(())
    }
}
