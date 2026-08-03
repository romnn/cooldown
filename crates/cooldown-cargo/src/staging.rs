//! Stages Cargo's locked filesystem topology for isolated resolver mutations.

use crate::cargocmd::{Cargo, StagingMetadata};
use crate::tool::CargoTool;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    AcceptedProjectState, AcceptedPublication, CoreError, IsolatedMutation,
    IsolatedMutationStrategy, Project, ProjectInputSnapshot, ProjectMutationJournal,
    ProjectMutationState, Result,
};
use std::collections::{BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};

/// A Cargo trial paired with every source input and output that authorized it.
struct CargoMutationStage {
    _scratch: tempfile::TempDir,
    source: Project,
    staged: Project,
    preimage: ProjectMutationJournal,
    inputs: ProjectInputSnapshot,
    cargo: Cargo,
    source_topology: StagingMetadata,
    layout: StagingLayout,
    source_files: BTreeSet<Utf8PathBuf>,
}

#[derive(Clone)]
struct StagingLayout {
    common_root: Utf8PathBuf,
    source_root: Utf8PathBuf,
    package_roots: BTreeSet<Utf8PathBuf>,
    target_paths: BTreeSet<Utf8PathBuf>,
    config_dirs: BTreeSet<Utf8PathBuf>,
    ambient_config_dirs: BTreeSet<Utf8PathBuf>,
    toolchain_dirs: BTreeSet<Utf8PathBuf>,
    vendor_roots: BTreeSet<Utf8PathBuf>,
    ambient_vendor_roots: BTreeSet<Utf8PathBuf>,
    output_paths: Vec<Utf8PathBuf>,
}

#[async_trait]
impl IsolatedMutationStrategy for CargoTool {
    async fn prepare(&self, source: &Project) -> Result<Box<dyn IsolatedMutation>> {
        Ok(Box::new(
            CargoMutationStage::prepare(self.cargo(), source).await?,
        ))
    }
}

#[async_trait]
impl IsolatedMutation for CargoMutationStage {
    fn project(&self) -> &Project {
        &self.staged
    }

    fn accepted_state(&self) -> Result<AcceptedProjectState> {
        let candidate = ProjectMutationState::capture(&self.staged.root, self.preimage.files())?;
        AcceptedProjectState::new(self.preimage.clone(), candidate, self.inputs.clone())
    }

    async fn publish(&self, accepted: &AcceptedProjectState) -> Result<AcceptedPublication> {
        self.validate_source_topology().await?;
        self.validate_source_shape()?;
        accepted.validate_source(&self.source.root)?;
        crate::publication::publish_accepted(&self.source, accepted)
    }
}

impl CargoMutationStage {
    async fn prepare(cargo: &Cargo, source: &Project) -> Result<Self> {
        reject_custom_lockfile(&source.root)?;
        reject_unsupported_config_environment(std::env::vars_os())?;
        let metadata = cargo
            .staging_metadata(&source.root)
            .await
            .map_err(|error| {
                isolation_error(format!(
                    "Cargo could not describe the locked source topology: {error}"
                ))
            })?;
        let layout = StagingLayout::new(source, metadata.clone())?;
        let source_files = layout.source_files()?;
        let preimage = capture_outputs(&source.root, &layout.output_paths)?;
        let scratch = tempfile::tempdir()?;
        let topology = utf8_path(scratch.path().join("tree"))?;
        std::fs::create_dir_all(&topology)?;
        let input_paths = source_files
            .iter()
            .filter(|path| !layout.is_output(path))
            .cloned()
            .collect::<Vec<_>>();
        let inputs = ProjectInputSnapshot::capture_with_contents(
            &input_paths,
            |source, contents, permissions| {
                if layout.is_ambient(source) {
                    return Ok(());
                }
                let destination = layout.staged_path(&topology, source)?;
                let parent = destination.parent().ok_or_else(|| {
                    isolation_error(format!("staged Cargo input has no parent: {destination}"))
                })?;
                std::fs::create_dir_all(parent)?;
                std::fs::write(&destination, contents).map_err(|error| {
                    isolation_error(format!(
                        "cannot faithfully isolate Cargo input {source}: {error}"
                    ))
                })?;
                std::fs::set_permissions(&destination, permissions.clone())?;
                Ok(())
            },
        )?;
        layout.copy_outputs_to(&topology, &source_files)?;
        inputs.validate().map_err(|error| {
            isolation_error(format!(
                "Cargo's resolution inputs changed while cooldown staged them: {error}"
            ))
        })?;

        let staged_root = layout.staged_path(&topology, &source.root)?;
        let staged_manifest = layout.staged_path(&topology, &source.manifest)?;
        let staged = Project {
            root: canonical_utf8(&staged_root)?,
            manifest: canonical_utf8(&staged_manifest)?,
            kind: source.kind,
            exclude_newer: source.exclude_newer.clone(),
        };
        let initial = ProjectMutationState::capture(&staged.root, preimage.files())?;
        let initial =
            AcceptedProjectState::new(preimage.clone(), initial, ProjectInputSnapshot::default())?;
        if initial.changed_files().next().is_some() {
            return Err(isolation_error(
                "Cargo outputs changed while cooldown staged the isolated project",
            ));
        }

        Ok(CargoMutationStage {
            _scratch: scratch,
            source: source.clone(),
            staged,
            preimage,
            inputs,
            cargo: cargo.clone(),
            source_topology: metadata,
            layout,
            source_files,
        })
    }

    async fn validate_source_topology(&self) -> Result<()> {
        let current = self
            .cargo
            .staging_metadata(&self.source.root)
            .await
            .map_err(|error| {
                CoreError::LockConflict(format!(
                    "could not revalidate Cargo's workspace topology: {error}; left source project files untouched"
                ))
            })?;
        if current != self.source_topology {
            return Err(CoreError::LockConflict(
                "Cargo's workspace members, package identities, manifests, or targets changed during the isolated trial; left source project files untouched"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn validate_source_shape(&self) -> Result<()> {
        let current = self.layout.source_files().map_err(|error| {
            CoreError::LockConflict(format!(
                "could not revalidate Cargo's resolution-input shape: {error}; left source project files untouched"
            ))
        })?;
        if current != self.source_files {
            return Err(CoreError::LockConflict(
                "Cargo's resolution inputs changed shape during the isolated trial; left source project files untouched"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

impl StagingLayout {
    fn new(source: &Project, metadata: StagingMetadata) -> Result<Self> {
        let canonical_source = canonical_utf8(&source.root)?;
        let canonical_workspace = canonical_utf8(&metadata.workspace_root)?;
        if canonical_source != canonical_workspace {
            return Err(isolation_error(format!(
                "Cargo reported workspace root {canonical_workspace}, but cooldown selected {}",
                source.root
            )));
        }

        let mut package_roots = BTreeSet::new();
        let mut target_paths = BTreeSet::new();
        let mut output_paths = vec![
            Utf8PathBuf::from("Cargo.lock"),
            Utf8PathBuf::from("Cargo.toml"),
        ];
        package_roots.insert(canonical_source.clone());
        for package in metadata.packages {
            let manifest_parent = package.manifest_path.parent().ok_or_else(|| {
                isolation_error(format!(
                    "Cargo package manifest has no parent: {}",
                    package.manifest_path
                ))
            })?;
            let local = package.workspace_member
                || package.source.is_none()
                || package.manifest_path.starts_with(&canonical_source);
            if !local {
                continue;
            }
            package_roots.insert(manifest_parent.to_owned());
            target_paths.extend(package.target_paths);
            if package.workspace_member {
                let relative = package
                    .manifest_path
                    .strip_prefix(&canonical_source)
                    .map_err(|_| {
                        isolation_error(format!(
                            "workspace member manifest is outside the selected project: {}",
                            package.manifest_path
                        ))
                    })?
                    .to_owned();
                output_paths.push(relative);
            }
        }

        let (config_dirs, ambient_config_dirs) = cargo_config_dirs(&canonical_source)?;
        let toolchain_dirs = canonical_source
            .ancestors()
            .map(Utf8Path::to_owned)
            .collect::<BTreeSet<_>>();
        let config_paths = cargo_config_paths(&config_dirs)?;
        let toolchain_paths = rust_toolchain_paths(&toolchain_dirs)?;
        let ambient_config_paths = cargo_config_paths(&ambient_config_dirs)?;
        let vendor_roots = discover_vendor_roots(&config_paths)?;
        let ambient_vendor_roots = discover_vendor_roots(&ambient_config_paths)?;
        package_roots.extend(path_dependency_roots(&package_roots)?);
        let mut topology_paths = vec![canonical_source.clone()];
        topology_paths.extend(package_roots.iter().cloned());
        topology_paths.extend(vendor_roots.iter().cloned());
        topology_paths.extend(
            config_paths
                .iter()
                .filter_map(|path| path.parent().map(Utf8Path::to_owned)),
        );
        topology_paths.extend(
            toolchain_paths
                .iter()
                .filter_map(|path| path.parent().map(Utf8Path::to_owned)),
        );
        let common_root = common_ancestor(&topology_paths)?;
        output_paths.sort();
        output_paths.dedup();
        Ok(StagingLayout {
            common_root,
            source_root: canonical_source,
            package_roots,
            target_paths,
            config_dirs,
            ambient_config_dirs,
            toolchain_dirs,
            vendor_roots,
            ambient_vendor_roots,
            output_paths,
        })
    }

    fn source_files(&self) -> Result<BTreeSet<Utf8PathBuf>> {
        let mut files = BTreeSet::new();
        for root in &self.package_roots {
            let manifest = root.join("Cargo.toml");
            require_regular_file(&manifest, "Cargo package manifest")?;
            files.insert(manifest);
        }
        for root in &self.vendor_roots {
            collect_vendor_files(root, root, &mut files)?;
        }
        for root in &self.ambient_vendor_roots {
            collect_vendor_files(root, root, &mut files)?;
        }
        files.extend(self.target_paths.iter().cloned());
        files.extend(cargo_config_paths(&self.config_dirs)?);
        files.extend(cargo_config_paths(&self.ambient_config_dirs)?);
        files.extend(rust_toolchain_paths(&self.toolchain_dirs)?);
        for relative in &self.output_paths {
            let source = self.source_root.join(relative);
            match std::fs::symlink_metadata(&source) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    files.insert(source);
                }
                Ok(_) => {
                    return Err(isolation_error(format!(
                        "Cargo mutation output is not a regular file: {source}"
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(files)
    }

    fn copy_outputs_to(&self, topology: &Utf8Path, files: &BTreeSet<Utf8PathBuf>) -> Result<()> {
        for source in files {
            if !self.is_output(source) {
                continue;
            }
            let destination = self.staged_path(topology, source)?;
            let parent = destination.parent().ok_or_else(|| {
                isolation_error(format!("staged Cargo input has no parent: {destination}"))
            })?;
            std::fs::create_dir_all(parent)?;
            std::fs::copy(source, &destination).map_err(|error| {
                isolation_error(format!(
                    "cannot faithfully isolate Cargo input {source}: {error}"
                ))
            })?;
        }
        Ok(())
    }

    fn staged_path(&self, topology: &Utf8Path, source: &Utf8Path) -> Result<Utf8PathBuf> {
        let relative = source.strip_prefix(&self.common_root).map_err(|_| {
            isolation_error(format!(
                "Cargo input {source} is outside staging topology {}",
                self.common_root
            ))
        })?;
        Ok(topology.join(relative))
    }

    fn is_output(&self, path: &Utf8Path) -> bool {
        path.strip_prefix(&self.source_root)
            .is_ok_and(|relative| self.output_paths.contains(&relative.to_owned()))
    }

    fn is_ambient(&self, path: &Utf8Path) -> bool {
        self.ambient_config_dirs
            .iter()
            .any(|directory| path.starts_with(directory))
            || self
                .ambient_vendor_roots
                .iter()
                .any(|directory| path.starts_with(directory))
    }
}

fn collect_vendor_files(
    root: &Utf8Path,
    directory: &Utf8Path,
    files: &mut BTreeSet<Utf8PathBuf>,
) -> Result<()> {
    for entry in std::fs::read_dir(directory).map_err(|error| {
        isolation_error(format!(
            "cannot faithfully isolate vendored Cargo source {directory}: {error}"
        ))
    })? {
        let entry = entry?;
        let path = utf8_path(entry.path())?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_vendor_files(root, &path, files)?;
        } else if file_type.is_file() {
            files.insert(path);
        } else {
            return Err(isolation_error(format!(
                "cannot faithfully isolate symlink or special entry {path} below vendored source {root}"
            )));
        }
    }
    Ok(())
}

fn cargo_config_dirs(
    workspace: &Utf8Path,
) -> Result<(BTreeSet<Utf8PathBuf>, BTreeSet<Utf8PathBuf>)> {
    let cargo_home = match std::env::var_os("CARGO_HOME") {
        Some(path) => Some((utf8_path(std::path::PathBuf::from(path))?, true)),
        None => std::env::var_os("HOME")
            .map(|home| std::path::PathBuf::from(home).join(".cargo"))
            .map(utf8_path)
            .transpose()?
            .map(|path| (path, false)),
    };
    let mut config_dirs = workspace
        .ancestors()
        .map(|ancestor| ancestor.join(".cargo"))
        .collect::<BTreeSet<_>>();
    let mut ambient_config_dirs = BTreeSet::new();
    if let Some((cargo_home, configured)) = cargo_home {
        if configured && !cargo_home.is_absolute() {
            config_dirs.insert(workspace.join(cargo_home));
        } else {
            config_dirs.remove(&cargo_home);
            ambient_config_dirs.insert(cargo_home);
        }
    }
    Ok((config_dirs, ambient_config_dirs))
}

fn cargo_config_paths(config_dirs: &BTreeSet<Utf8PathBuf>) -> Result<BTreeSet<Utf8PathBuf>> {
    let mut paths = BTreeSet::new();
    for config_dir in config_dirs {
        for name in ["config", "config.toml"] {
            let path = config_dir.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    require_regular_file(&path, "Cargo configuration")?;
                    paths.insert(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(isolation_error(format!(
                        "cannot faithfully isolate Cargo configuration {path}: {error}"
                    )));
                }
            }
        }
    }
    Ok(paths)
}

fn rust_toolchain_paths(directories: &BTreeSet<Utf8PathBuf>) -> Result<BTreeSet<Utf8PathBuf>> {
    let mut paths = BTreeSet::new();
    for directory in directories {
        for name in ["rust-toolchain", "rust-toolchain.toml"] {
            let path = directory.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    require_regular_file(&path, "Rust toolchain configuration")?;
                    paths.insert(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(isolation_error(format!(
                        "cannot faithfully isolate Rust toolchain configuration {path}: {error}"
                    )));
                }
            }
        }
    }
    Ok(paths)
}

fn discover_vendor_roots(configs: &BTreeSet<Utf8PathBuf>) -> Result<BTreeSet<Utf8PathBuf>> {
    let mut roots = BTreeSet::new();
    for config in configs {
        let value: toml::Value =
            toml::from_str(&std::fs::read_to_string(config)?).map_err(|error| {
                isolation_error(format!(
                    "cannot parse Cargo configuration {config}: {error}"
                ))
            })?;
        reject_unsupported_config_inputs(config, &value)?;
        let Some(sources) = value.get("source").and_then(toml::Value::as_table) else {
            continue;
        };
        for source in sources.values() {
            let Some(directory) = source.get("directory").and_then(toml::Value::as_str) else {
                continue;
            };
            let base = config.parent().and_then(Utf8Path::parent).ok_or_else(|| {
                isolation_error(format!("Cargo config has no project base: {config}"))
            })?;
            let root = canonical_utf8(&base.join(directory))?;
            roots.insert(root);
        }
    }
    Ok(roots)
}

fn reject_unsupported_config_inputs(config: &Utf8Path, value: &toml::Value) -> Result<()> {
    if custom_lockfile_configured(value) {
        return Err(unsupported_config(
            config,
            "sets `resolver.lockfile-path`; cooldown currently requires the workspace-root `Cargo.lock`",
        ));
    }
    if value.get("include").is_some() {
        return Err(unsupported_config(
            config,
            "uses `include`, whose recursive input closure is not yet supported",
        ));
    }
    if value.get("paths").is_some() {
        return Err(unsupported_config(
            config,
            "uses local `paths` overrides, which are not yet supported",
        ));
    }
    if config_patch_uses_path(value) {
        return Err(unsupported_config(
            config,
            "uses a path-based `[patch]`, whose local source closure is not yet supported",
        ));
    }
    let has_local_registry = value
        .get("source")
        .and_then(toml::Value::as_table)
        .is_some_and(|sources| {
            sources.values().any(|source| {
                source
                    .get("local-registry")
                    .and_then(toml::Value::as_str)
                    .is_some()
            })
        });
    if has_local_registry {
        return Err(unsupported_config(
            config,
            "uses a `local-registry` source, which is not yet supported",
        ));
    }
    if config_uses_file_registry(value) {
        return Err(unsupported_config(
            config,
            "uses a file-backed registry URL, whose local input closure is not yet supported",
        ));
    }
    Ok(())
}

fn unsupported_config(config: &Utf8Path, detail: &str) -> CoreError {
    CoreError::Config(format!("Cargo configuration {config} {detail}"))
}

fn custom_lockfile_configured(value: &toml::Value) -> bool {
    value
        .get("resolver")
        .and_then(toml::Value::as_table)
        .is_some_and(|resolver| resolver.contains_key("lockfile-path"))
}

fn config_patch_uses_path(value: &toml::Value) -> bool {
    value
        .get("patch")
        .and_then(toml::Value::as_table)
        .is_some_and(|registries| {
            registries.values().any(|registry| {
                registry.as_table().is_some_and(|dependencies| {
                    dependencies.values().any(|dependency| {
                        dependency
                            .as_table()
                            .is_some_and(|specification| specification.contains_key("path"))
                    })
                })
            })
        })
}

fn config_uses_file_registry(value: &toml::Value) -> bool {
    let is_file_url = |candidate: &toml::Value, key: &str| {
        candidate
            .get(key)
            .and_then(toml::Value::as_str)
            .is_some_and(is_file_registry_url)
    };
    value
        .get("source")
        .and_then(toml::Value::as_table)
        .is_some_and(|sources| {
            sources
                .values()
                .any(|source| is_file_url(source, "registry"))
        })
        || value
            .get("registries")
            .and_then(toml::Value::as_table)
            .is_some_and(|registries| {
                registries
                    .values()
                    .any(|registry| is_file_url(registry, "index"))
            })
}

fn is_file_registry_url(value: &str) -> bool {
    let value = value.trim_start().to_ascii_lowercase();
    value.starts_with("file:") || value.starts_with("sparse+file:")
}

fn reject_unsupported_config_environment(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<()> {
    for (key, value) in variables {
        if !cargo_registry_index_variable(&key) {
            continue;
        }
        let value = value.into_string().map_err(|_| {
            CoreError::Config(format!(
                "Cargo registry index environment variable {} is not valid UTF-8; cooldown cannot audit its resolver input",
                key.to_string_lossy()
            ))
        })?;
        if is_file_registry_url(&value) {
            return Err(CoreError::Config(format!(
                "Cargo registry index environment variable {} uses a file-backed URL, whose local input closure is not yet supported",
                key.to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn cargo_registry_index_variable(key: &OsStr) -> bool {
    key.to_str().is_some_and(|key| {
        #[cfg(windows)]
        let key = key.to_ascii_uppercase();
        key.starts_with("CARGO_REGISTRIES_")
            && key.ends_with("_INDEX")
            && key.len() > "CARGO_REGISTRIES__INDEX".len()
    })
}

pub(crate) fn reject_custom_lockfile(workspace: &Utf8Path) -> Result<()> {
    if std::env::var_os("CARGO_RESOLVER_LOCKFILE_PATH").is_some() {
        return Err(CoreError::Config(
            "`CARGO_RESOLVER_LOCKFILE_PATH` is set; cooldown currently requires Cargo's workspace-root `Cargo.lock`"
                .to_string(),
        ));
    }
    let (config_dirs, ambient_config_dirs) = cargo_config_dirs(workspace)?;
    let configs = cargo_config_paths(&config_dirs)?
        .into_iter()
        .chain(cargo_config_paths(&ambient_config_dirs)?)
        .collect::<BTreeSet<_>>();
    for config in configs {
        let contents = std::fs::read_to_string(&config).map_err(|error| {
            isolation_error(format!("cannot read Cargo configuration {config}: {error}"))
        })?;
        let value: toml::Value = toml::from_str(&contents).map_err(|error| {
            CoreError::Config(format!(
                "cannot parse Cargo configuration {config}: {error}"
            ))
        })?;
        reject_custom_lockfile_config(&config, &value)?;
    }
    Ok(())
}

fn reject_custom_lockfile_config(config: &Utf8Path, value: &toml::Value) -> Result<()> {
    if custom_lockfile_configured(value) {
        return Err(unsupported_config(
            config,
            "sets `resolver.lockfile-path`; cooldown currently requires the workspace-root `Cargo.lock`",
        ));
    }
    if value.get("include").is_some() {
        return Err(unsupported_config(
            config,
            "uses `include`; cooldown cannot prove that included configuration preserves the workspace-root `Cargo.lock`",
        ));
    }
    Ok(())
}

fn path_dependency_roots(initial: &BTreeSet<Utf8PathBuf>) -> Result<BTreeSet<Utf8PathBuf>> {
    let mut discovered = initial.clone();
    let mut pending: VecDeque<_> = initial.iter().cloned().collect();
    while let Some(root) = pending.pop_front() {
        let manifest = root.join("Cargo.toml");
        let value: toml::Value =
            toml::from_str(&std::fs::read_to_string(&manifest)?).map_err(|error| {
                isolation_error(format!("cannot parse Cargo manifest {manifest}: {error}"))
            })?;
        let paths = manifest_dependency_paths(&value);
        for path in paths {
            let resolved = canonical_utf8(&root.join(path))?;
            if discovered.insert(resolved.clone()) {
                pending.push_back(resolved);
            }
        }
    }
    Ok(discovered)
}

fn manifest_dependency_paths(value: &toml::Value) -> Vec<String> {
    let mut paths = Vec::new();
    let Some(root) = value.as_table() else {
        return paths;
    };
    collect_dependency_groups(root, &mut paths);
    if let Some(workspace) = root.get("workspace").and_then(toml::Value::as_table) {
        collect_dependency_groups(workspace, &mut paths);
    }
    if let Some(targets) = root.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            collect_dependency_groups(target, &mut paths);
        }
    }
    if let Some(patches) = root.get("patch").and_then(toml::Value::as_table) {
        for registry in patches.values() {
            collect_dependency_specs(registry, &mut paths);
        }
    }
    if let Some(replacements) = root.get("replace") {
        collect_dependency_specs(replacements, &mut paths);
    }
    paths
}

fn collect_dependency_groups(table: &toml::map::Map<String, toml::Value>, paths: &mut Vec<String>) {
    for name in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(group) = table.get(name) {
            collect_dependency_specs(group, paths);
        }
    }
}

fn collect_dependency_specs(value: &toml::Value, paths: &mut Vec<String>) {
    let Some(specs) = value.as_table() else {
        return;
    };
    for spec in specs.values() {
        if let Some(path) = spec
            .as_table()
            .and_then(|table| table.get("path"))
            .and_then(toml::Value::as_str)
        {
            paths.push(path.to_string());
        }
    }
}

fn require_regular_file(path: &Utf8Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = std::fs::metadata(path).map_err(|error| {
                isolation_error(format!("cannot resolve {label} symlink {path}: {error}"))
            })?;
            if target.is_file() {
                Ok(())
            } else {
                Err(isolation_error(format!(
                    "{label} symlink does not resolve to a regular file: {path}"
                )))
            }
        }
        Ok(_) => Err(isolation_error(format!(
            "{label} is not a regular file: {path}"
        ))),
        Err(error) => Err(isolation_error(format!(
            "cannot read {label} {path}: {error}"
        ))),
    }
}

fn capture_outputs(root: &Utf8Path, paths: &[Utf8PathBuf]) -> Result<ProjectMutationJournal> {
    ProjectMutationJournal::capture(root, paths)
}

fn common_ancestor(paths: &[Utf8PathBuf]) -> Result<Utf8PathBuf> {
    let Some(first) = paths.first() else {
        return Err(isolation_error(
            "Cargo staging topology has no source roots",
        ));
    };
    let mut common = first.clone();
    while !paths.iter().all(|path| path.starts_with(&common)) {
        if !common.pop() {
            return Err(isolation_error(
                "Cargo staging inputs do not share an absolute filesystem root",
            ));
        }
    }
    Ok(common)
}

fn canonical_utf8(path: &Utf8Path) -> Result<Utf8PathBuf> {
    utf8_path(std::fs::canonicalize(path)?)
}

fn utf8_path(path: std::path::PathBuf) -> Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(path).map_err(|path| {
        CoreError::PathEncoding(format!("non-UTF-8 Cargo path: {}", path.display()))
    })
}

fn isolation_error(detail: impl Into<String>) -> CoreError {
    CoreError::Filesystem(format!(
        "cannot faithfully isolate this Cargo project: {}",
        detail.into()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CARGO_ID;
    use color_eyre::eyre;
    use indoc::indoc;

    fn write(path: &Utf8Path, contents: &str) -> eyre::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
        Ok(())
    }

    fn package(root: &Utf8Path, name: &str) -> eyre::Result<()> {
        write(
            &root.join("Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )?;
        write(&root.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n")
    }

    fn generate_lock(root: &Utf8Path) -> eyre::Result<()> {
        let cargo = std::env::var_os("COOLDOWN_CARGO").unwrap_or_else(|| "cargo".into());
        let output = std::process::Command::new(cargo)
            .args(["generate-lockfile", "--offline"])
            .current_dir(root)
            .output()?;
        if output.status.success() {
            return Ok(());
        }
        Err(eyre::eyre!(
            "cargo generate-lockfile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }

    fn project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            kind: CARGO_ID,
            manifest: root.join("Cargo.toml"),
            exclude_newer: None,
        }
    }

    #[test]
    fn dependency_path_parser_ignores_unrelated_path_keys() -> eyre::Result<()> {
        let manifest: toml::Value = toml::from_str(indoc! {r#"
        [package]
        name = "app"
        version = "0.1.0"

        [package.metadata]
        path = "not-a-dependency"

        [dependencies]
        direct = { path = "../direct" }

        [target.'cfg(unix)'.build-dependencies]
        build = { path = "../build" }

        [patch.crates-io]
        patched = { path = "../patched" }
    "#})?;

        assert_eq!(
            manifest_dependency_paths(&manifest),
            ["../direct", "../build", "../patched"]
        );
        Ok(())
    }

    #[test]
    fn unsupported_cargo_configuration_inputs_fail_explicitly() -> eyre::Result<()> {
        let config = Utf8Path::new("/project/.cargo/config.toml");
        for (contents, expected) in [
            ("include = [\"shared.toml\"]", "uses `include`"),
            ("paths = [\"../override\"]", "uses local `paths`"),
            (
                "[resolver]\nlockfile-path = \"../state/Cargo.lock\"",
                "sets `resolver.lockfile-path`",
            ),
            (
                "[patch.crates-io]\nlocal = { path = \"../local\" }",
                "uses a path-based `[patch]`",
            ),
            (
                "[source.local]\nlocal-registry = \"registry\"",
                "uses a `local-registry`",
            ),
            (
                "[registries.private]\nindex = \"sparse+file:///srv/index\"",
                "uses a file-backed registry URL",
            ),
        ] {
            let value: toml::Value = toml::from_str(contents)?;
            let error = reject_unsupported_config_inputs(config, &value)
                .err()
                .ok_or_else(|| eyre::eyre!("unsupported Cargo configuration was accepted"))?;
            assert!(
                error.to_string().contains(expected),
                "unexpected diagnostic for {contents:?}: {error}"
            );
        }
        Ok(())
    }

    #[test]
    fn custom_lockfile_config_is_rejected_before_project_use() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = utf8_path(directory.path().join("workspace"))?;
        write(&root.join("Cargo.toml"), "[workspace]\nresolver = \"3\"\n")?;
        write(
            &root.join(".cargo/config.toml"),
            "[resolver]\nlockfile-path = \"../state/Cargo.lock\"\n",
        )?;

        let error = reject_custom_lockfile(&root)
            .err()
            .ok_or_else(|| eyre::eyre!("custom lockfile configuration was accepted"))?;
        assert!(error.to_string().contains("resolver.lockfile-path"));
        Ok(())
    }

    #[test]
    fn included_cargo_config_is_rejected_before_project_use() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = utf8_path(directory.path().join("workspace"))?;
        write(&root.join("Cargo.toml"), "[workspace]\nresolver = \"3\"\n")?;
        write(
            &root.join(".cargo/config.toml"),
            "include = [\"custom-lock.toml\"]\n",
        )?;
        write(
            &root.join(".cargo/custom-lock.toml"),
            "[resolver]\nlockfile-path = \"../state/Cargo.lock\"\n",
        )?;

        let error = reject_custom_lockfile(&root)
            .err()
            .ok_or_else(|| eyre::eyre!("included Cargo configuration was accepted"))?;
        assert!(error.to_string().contains("uses `include`"));
        Ok(())
    }

    #[test]
    fn file_registry_environment_is_rejected_before_staging() -> eyre::Result<()> {
        let error = reject_unsupported_config_environment([(
            OsString::from("CARGO_REGISTRIES_PRIVATE_INDEX"),
            OsString::from("sparse+file:///srv/registry/index"),
        )])
        .err()
        .ok_or_else(|| eyre::eyre!("file-backed registry environment was accepted"))?;

        assert!(error.to_string().contains("CARGO_REGISTRIES_PRIVATE_INDEX"));
        assert!(error.to_string().contains("file-backed URL"));
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn mixed_case_file_registry_environment_is_rejected_on_windows() -> eyre::Result<()> {
        let error = reject_unsupported_config_environment([(
            OsString::from("cargo_registries_private_index"),
            OsString::from("file:///private/index"),
        )])
        .err()
        .ok_or_else(|| eyre::eyre!("mixed-case file-backed registry environment was accepted"))?;

        assert!(error.to_string().contains("file-backed URL"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_registry_environment_fails_closed() -> eyre::Result<()> {
        use std::os::unix::ffi::OsStringExt as _;

        let error = reject_unsupported_config_environment([(
            OsString::from("CARGO_REGISTRIES_PRIVATE_INDEX"),
            OsString::from_vec(vec![b'f', b'i', b'l', b'e', b':', 0xff]),
        )])
        .err()
        .ok_or_else(|| eyre::eyre!("non-UTF-8 registry environment was accepted"))?;

        assert!(error.to_string().contains("not valid UTF-8"));
        Ok(())
    }

    #[tokio::test]
    async fn sibling_path_dependency_keeps_its_relative_topology() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = utf8_path(directory.path().join("app"))?;
        let shared = utf8_path(directory.path().join("shared"))?;
        package(&shared, "shared")?;
        write(
            &root.join("Cargo.toml"),
            indoc! {r#"
            [package]
            name = "app"
            version = "0.1.0"
            edition = "2024"

            [dependencies]
            shared = { path = "../shared" }
        "#},
        )?;
        write(&root.join("src/lib.rs"), "pub fn app() {}\n")?;
        generate_lock(&root)?;

        let stage = CargoMutationStage::prepare(&Cargo::new(), &project(&root)).await?;

        assert!(stage.staged.root.join("../shared/Cargo.toml").exists());
        Cargo::new().staging_metadata(&stage.staged.root).await?;
        Ok(())
    }

    #[tokio::test]
    async fn vendored_source_is_available_to_the_isolated_resolver() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = utf8_path(directory.path().join("app"))?;
        write(
            &root.join("Cargo.toml"),
            indoc! {r#"
            [package]
            name = "app"
            version = "0.1.0"
            edition = "2024"

            [dependencies]
            vendored = "1.0.0"
        "#},
        )?;
        write(&root.join("src/lib.rs"), "pub fn app() {}\n")?;
        write(
            &root.join(".cargo/config.toml"),
            indoc! {r#"
            [source.crates-io]
            replace-with = "vendored-source"

            [source.vendored-source]
            directory = "vendor"
        "#},
        )?;
        let vendor = root.join("vendor/vendored-1.0.0");
        write(
            &vendor.join("Cargo.toml"),
            indoc! {r#"
            [package]
            name = "vendored"
            version = "1.0.0"
            edition = "2024"
        "#},
        )?;
        write(&vendor.join("src/lib.rs"), "pub fn vendored() {}\n")?;
        write(
            &vendor.join(".cargo-checksum.json"),
            r#"{"files":{},"package":null}"#,
        )?;
        generate_lock(&root)?;

        let stage = CargoMutationStage::prepare(&Cargo::new(), &project(&root)).await?;

        assert!(
            stage
                .staged
                .root
                .join("vendor/vendored-1.0.0/Cargo.toml")
                .exists()
        );
        Cargo::new().staging_metadata(&stage.staged.root).await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_workspace_member_is_staged_without_following_it_in_place() -> eyre::Result<()>
    {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()?;
        let root = utf8_path(directory.path().join("workspace"))?;
        let member = utf8_path(directory.path().join("member"))?;
        package(&member, "member")?;
        write(
            &root.join("Cargo.toml"),
            indoc! {r#"
            [workspace]
            members = ["linked"]
            resolver = "3"
        "#},
        )?;
        symlink("../member", root.join("linked"))?;
        generate_lock(&root)?;

        let stage = CargoMutationStage::prepare(&Cargo::new(), &project(&root)).await?;

        assert!(stage.staged.root.join("linked/Cargo.toml").is_file());
        Cargo::new().staging_metadata(&stage.staged.root).await?;
        stage.accepted_state()?;
        Ok(())
    }

    #[tokio::test]
    async fn changed_cargo_config_invalidates_the_accepted_state() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = utf8_path(directory.path().join("app"))?;
        package(&root, "app")?;
        write(&root.join(".cargo/config.toml"), "[net]\noffline = true\n")?;
        generate_lock(&root)?;
        let stage = CargoMutationStage::prepare(&Cargo::new(), &project(&root)).await?;
        let source_lock = std::fs::read_to_string(root.join("Cargo.lock"))?;
        std::fs::write(stage.staged.root.join("Cargo.lock"), "accepted lock\n")?;
        let accepted = stage.accepted_state()?;

        write(&root.join(".cargo/config.toml"), "[net]\noffline = false\n")?;

        std::assert_matches!(
            stage.publish(&accepted).await,
            Err(CoreError::LockConflict(_))
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.lock"))?,
            source_lock
        );
        Ok(())
    }

    #[tokio::test]
    async fn newly_added_cargo_config_invalidates_publication() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = utf8_path(directory.path().join("app"))?;
        package(&root, "app")?;
        generate_lock(&root)?;
        let stage = CargoMutationStage::prepare(&Cargo::new(), &project(&root)).await?;
        let source_lock = std::fs::read_to_string(root.join("Cargo.lock"))?;
        std::fs::write(stage.staged.root.join("Cargo.lock"), "accepted lock\n")?;
        let accepted = stage.accepted_state()?;

        write(&root.join(".cargo/config.toml"), "[net]\noffline = true\n")?;

        std::assert_matches!(
            stage.publish(&accepted).await,
            Err(CoreError::LockConflict(_))
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.lock"))?,
            source_lock
        );
        Ok(())
    }

    #[tokio::test]
    async fn newly_globbed_workspace_member_invalidates_publication() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = utf8_path(directory.path().join("workspace"))?;
        write(
            &root.join("Cargo.toml"),
            indoc! {r#"
            [workspace]
            members = ["crates/*"]
            resolver = "3"
        "#},
        )?;
        package(&root.join("crates/first"), "first")?;
        generate_lock(&root)?;
        let stage = CargoMutationStage::prepare(&Cargo::new(), &project(&root)).await?;
        let source_lock = std::fs::read_to_string(root.join("Cargo.lock"))?;
        let accepted = stage.accepted_state()?;

        package(&root.join("crates/new"), "new")?;

        std::assert_matches!(
            stage.publish(&accepted).await,
            Err(CoreError::LockConflict(_))
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.lock"))?,
            source_lock
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_cargo_config_fails_with_an_isolation_diagnostic() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let root = utf8_path(directory.path().join("app"))?;
        package(&root, "app")?;
        let config = root.join(".cargo/config.toml");
        write(&config, "[net]\noffline = true\n")?;
        generate_lock(&root)?;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o000))?;

        let result = CargoMutationStage::prepare(&Cargo::new(), &project(&root)).await;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))?;
        let error = result
            .err()
            .ok_or_else(|| eyre::eyre!("unreadable Cargo configuration was accepted"))?;

        assert!(
            error
                .to_string()
                .contains("cannot faithfully isolate this Cargo project")
        );
        Ok(())
    }
}
