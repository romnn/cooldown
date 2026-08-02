use super::{detect, scan_root_for};
use crate::app::{Progress, RecoveryTarget};
use crate::cli::GlobalArgs;
use crate::{discovery, scan};
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_cargo::{CARGO_ID, RECOVERY_MARKER, recover_interrupted_mutation};
use cooldown_core::{CoreError, ToolId, recognized_tool_names, tool_id};

pub(in crate::cli) struct PreparedRecovery {
    pub(in crate::cli) targets: Vec<RecoveryTarget>,
    pub(in crate::cli) json: bool,
    pub(in crate::cli) progress: Progress,
}

/// Discovers recovery artifacts without loading normal run configuration or package state.
pub(in crate::cli) fn prepare_recovery(global: &GlobalArgs) -> Result<PreparedRecovery, CoreError> {
    if global.dry_run {
        return Err(CoreError::Config(
            "`recover` cannot be combined with `--dry-run`; omit it to perform recovery"
                .to_string(),
        ));
    }
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

fn direct_cargo_recovery_roots(workdir: &Utf8Path, repo_root: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut roots = Vec::new();
    for ancestor in workdir.ancestors() {
        if ancestor.join(RECOVERY_MARKER).is_file() {
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
    let recovery_roots =
        scan::find_marker_dirs(root, RECOVERY_MARKER, respect_gitignore, &[], false)?;
    let mut authoritative = recovery_roots;
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
    use clap::Parser;

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
    fn explicit_ignored_project_is_still_a_recovery_target() -> color_eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| color_eyre::eyre::eyre!("temporary path is not UTF-8"))?;
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
    fn explicit_hidden_project_is_still_a_recovery_target() -> color_eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| color_eyre::eyre::eyre!("temporary path is not UTF-8"))?;
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
}
