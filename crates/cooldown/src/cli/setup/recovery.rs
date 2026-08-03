use super::{detect, scan_root_for};
use crate::app::{Progress, RecoveryTarget};
use crate::cli::GlobalArgs;
use crate::{discovery, scan};
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_cargo::{CARGO_ID, RECOVERY_MARKER, recover_interrupted_mutation};
use cooldown_core::{CoreError, ToolId, recognized_tool_names, tool_id};

#[cfg(test)]
const RECOVERY_RELEVANT_GLOBAL_ARGUMENTS: &[&str] = &[
    "tool",
    "cargo",
    "no_gitignore",
    "log_level",
    "no_progress",
    "dir",
    "json",
    "color",
];

struct RejectedRecoveryOption {
    id: &'static str,
    flag: &'static str,
    is_set: fn(&GlobalArgs) -> bool,
}

const RECOVERY_REJECTED_OPTIONS: &[RejectedRecoveryOption] = &[
    RejectedRecoveryOption {
        id: "min_age",
        flag: "--min-age",
        is_set: |global| global.min_age.is_some(),
    },
    RejectedRecoveryOption {
        id: "min_age_major",
        flag: "--min-age-major",
        is_set: |global| global.min_age_major.is_some(),
    },
    RejectedRecoveryOption {
        id: "min_age_minor",
        flag: "--min-age-minor",
        is_set: |global| global.min_age_minor.is_some(),
    },
    RejectedRecoveryOption {
        id: "min_age_patch",
        flag: "--min-age-patch",
        is_set: |global| global.min_age_patch.is_some(),
    },
    RejectedRecoveryOption {
        id: "latest",
        flag: "--latest",
        is_set: |global| global.latest,
    },
    RejectedRecoveryOption {
        id: "freeze",
        flag: "--freeze",
        is_set: |global| global.freeze.is_some(),
    },
    RejectedRecoveryOption {
        id: "allow",
        flag: "--allow",
        is_set: |global| !global.allow.is_empty(),
    },
    RejectedRecoveryOption {
        id: "major",
        flag: "--major",
        is_set: |global| global.major,
    },
    RejectedRecoveryOption {
        id: "no_major",
        flag: "--no-major",
        is_set: |global| global.no_major,
    },
    RejectedRecoveryOption {
        id: "respect_dist_tags",
        flag: "--respect-dist-tags",
        is_set: |global| global.respect_dist_tags,
    },
    RejectedRecoveryOption {
        id: "no_respect_dist_tags",
        flag: "--no-respect-dist-tags",
        is_set: |global| global.no_respect_dist_tags,
    },
    RejectedRecoveryOption {
        id: "package",
        flag: "--package",
        is_set: |global| !global.package.is_empty(),
    },
    RejectedRecoveryOption {
        id: "exclude_folders",
        flag: "--exclude-folders",
        is_set: |global| !global.exclude_folders.is_empty(),
    },
    RejectedRecoveryOption {
        id: "exclude_packages",
        flag: "--exclude-packages",
        is_set: |global| !global.exclude_packages.is_empty(),
    },
    RejectedRecoveryOption {
        id: "list_packages",
        flag: "--list-packages",
        is_set: |global| global.list_packages,
    },
    RejectedRecoveryOption {
        id: "paths",
        flag: "--paths",
        is_set: |global| global.paths,
    },
    RejectedRecoveryOption {
        id: "show_projects",
        flag: "--show-projects",
        is_set: |global| global.show_projects,
    },
    RejectedRecoveryOption {
        id: "no_suggestions",
        flag: "--no-suggestions",
        is_set: |global| global.no_suggestions,
    },
    RejectedRecoveryOption {
        id: "allow_stale_lock",
        flag: "--allow-stale-lock",
        is_set: |global| global.allow_stale_lock,
    },
    RejectedRecoveryOption {
        id: "sync",
        flag: "--sync",
        is_set: |global| global.sync,
    },
    RejectedRecoveryOption {
        id: "dry_run",
        flag: "--dry-run",
        is_set: |global| global.dry_run,
    },
    RejectedRecoveryOption {
        id: "offline",
        flag: "--offline",
        is_set: |global| global.offline,
    },
    RejectedRecoveryOption {
        id: "fresh",
        flag: "--fresh",
        is_set: |global| global.fresh,
    },
    RejectedRecoveryOption {
        id: "concurrency",
        flag: "--concurrency",
        is_set: |global| global.concurrency.is_some(),
    },
    RejectedRecoveryOption {
        id: "no_native",
        flag: "--no-native",
        is_set: |global| global.no_native,
    },
    RejectedRecoveryOption {
        id: "no_global",
        flag: "--no-global",
        is_set: |global| global.no_global,
    },
    RejectedRecoveryOption {
        id: "config",
        flag: "--config",
        is_set: |global| global.config.is_some(),
    },
    #[cfg(debug_assertions)]
    RejectedRecoveryOption {
        id: "now",
        flag: "--now",
        is_set: |global| global.now.is_some(),
    },
];

pub(in crate::cli) struct PreparedRecovery {
    pub(in crate::cli) targets: Vec<RecoveryTarget>,
    pub(in crate::cli) json: bool,
    pub(in crate::cli) progress: Progress,
}

/// Discovers recovery artifacts without loading normal run configuration or package state.
pub(in crate::cli) fn prepare_recovery(global: &GlobalArgs) -> Result<PreparedRecovery, CoreError> {
    validate_recovery_options(global)?;
    let tools = selected_tools(global)?;
    let workdir = detect::workdir(global)?;
    let repo_root = discovery::find_repo_root(&workdir);
    let scan_root = scan_root_for(&workdir, &repo_root);
    let mut targets = Vec::new();
    if tools.is_empty() || tools.contains(&CARGO_ID) {
        let mut roots = direct_cargo_recovery_roots(&workdir, &repo_root);
        roots.extend(cargo_recovery_roots(&scan_root, !global.no_gitignore)?);
        roots.sort();
        roots.dedup();
        for root in roots.into_iter().filter(|root| {
            workdir == scan_root || workdir.starts_with(root) || root.starts_with(&workdir)
        }) {
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
        json: global.json,
        progress: super::options::progress_mode(global),
    })
}

fn validate_recovery_options(global: &GlobalArgs) -> Result<(), CoreError> {
    if let Some(option) = RECOVERY_REJECTED_OPTIONS
        .iter()
        .find(|option| (option.is_set)(global))
    {
        tracing::debug!(argument = option.id, "rejecting irrelevant recovery option");
        return Err(CoreError::Config(format!(
            "`recover` does not use `{}`; remove it so recovery depends only on project location and recovery artifacts",
            option.flag
        )));
    }
    Ok(())
}

fn direct_cargo_recovery_roots(workdir: &Utf8Path, repo_root: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut roots = Vec::new();
    for ancestor in workdir.ancestors() {
        if std::fs::symlink_metadata(ancestor.join(RECOVERY_MARKER)).is_ok() {
            roots.push(ancestor.to_owned());
        }
        if ancestor == repo_root {
            break;
        }
    }
    roots
}

fn selected_tools(global: &GlobalArgs) -> Result<Vec<ToolId>, CoreError> {
    let names = if global.cargo {
        vec![CARGO_ID.as_str().to_string()]
    } else {
        global.tool.clone()
    };
    names
        .iter()
        .map(|name| {
            tool_id(name).ok_or_else(|| {
                CoreError::Config(format!(
                    "unknown --tool `{name}`; recognised: {}",
                    recognized_tool_names()
                ))
            })
        })
        .collect()
}

fn cargo_recovery_roots(
    root: &Utf8Path,
    respect_gitignore: bool,
) -> Result<Vec<Utf8PathBuf>, CoreError> {
    let mut authoritative = scan::find_recovery_marker_dirs(root, RECOVERY_MARKER)?;
    authoritative.extend(scan::find_marker_dirs(
        root,
        "Cargo.lock",
        respect_gitignore,
        &[],
        true,
    )?);
    authoritative.sort();
    authoritative.dedup();

    let manifests = scan::find_marker_dirs(root, "Cargo.toml", respect_gitignore, &[], false)?;
    let mut roots = authoritative.clone();
    roots.extend(manifests.into_iter().filter(|manifest| {
        !authoritative
            .iter()
            .any(|known| manifest.starts_with(known) || known.starts_with(manifest))
    }));
    roots.sort();
    roots.dedup();
    Ok(roots)
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
    use clap::{CommandFactory, Parser};
    use color_eyre::eyre;

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
    fn every_global_argument_has_an_explicit_recovery_classification() {
        let actual = Cli::command()
            .get_arguments()
            .filter(|argument| argument.is_global_set())
            .map(|argument| argument.get_id().as_str().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        let classified = RECOVERY_RELEVANT_GLOBAL_ARGUMENTS
            .iter()
            .copied()
            .chain(RECOVERY_REJECTED_OPTIONS.iter().map(|option| option.id))
            .map(str::to_string)
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(actual, classified);
    }

    #[test]
    fn recovery_marker_finds_project_without_cargo_lock() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(directory.path()).expect("UTF-8 tempdir");
        std::fs::write(root.join(RECOVERY_MARKER), "{}").expect("write marker");

        assert_eq!(
            cargo_recovery_roots(root, false).expect("discover roots"),
            vec![root.to_owned()]
        );
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
            direct_cargo_recovery_roots(root, root),
            vec![root.to_owned()]
        );
        assert_eq!(cargo_recovery_roots(root, false)?, vec![root.to_owned()]);
        Ok(())
    }

    #[test]
    fn nested_recovery_marker_is_not_hidden_by_an_outer_lock() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(directory.path()).expect("UTF-8 tempdir");
        let nested = root.join("tools/independent");
        std::fs::create_dir_all(&nested).expect("create nested project");
        std::fs::write(root.join("Cargo.lock"), "").expect("write outer lock");
        std::fs::write(nested.join(RECOVERY_MARKER), "{}").expect("write nested marker");

        assert_eq!(
            cargo_recovery_roots(root, false).expect("discover roots"),
            vec![root.to_owned(), nested]
        );
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

    #[test]
    fn repository_recovery_finds_hidden_and_ignored_descendants() -> eyre::Result<()> {
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
        std::fs::write(ignored.join(RECOVERY_MARKER), "{}")?;
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
}
