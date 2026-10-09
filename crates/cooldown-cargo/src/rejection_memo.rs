use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{Change, CoreError, MemberRef, Project};
use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MemoOperation {
    Widen,
    Precise,
    Seed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MemoMetadataMode {
    Skip,
    Resolve,
}

struct CaptureContext {
    original_root: Utf8PathBuf,
    metadata_mode: MemoMetadataMode,
    environment: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ChangeIdentity {
    name: String,
    registry: Option<String>,
    from: String,
    to: String,
    direct: bool,
    members: Vec<MemberIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct MemberIdentity {
    name: String,
    path: String,
}

impl From<&MemberRef> for MemberIdentity {
    fn from(member: &MemberRef) -> Self {
        Self {
            name: member.name.clone(),
            path: member.path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct MemoKey {
    operation: MemoOperation,
    metadata_mode: MemoMetadataMode,
    original_root: Utf8PathBuf,
    environment: [u8; 32],
    cacheable: bool,
    changes: Vec<ChangeIdentity>,
    followers: Vec<MemberIdentity>,
    pub(crate) inputs: [u8; 32],
}

impl MemoKey {
    pub(crate) const fn is_cacheable(&self) -> bool {
        self.cacheable
    }

    #[cfg(test)]
    pub(crate) fn capture(
        project: &Project,
        operation: MemoOperation,
        changes: &[&Change],
        followers: &[MemberRef],
    ) -> Result<Self, CoreError> {
        RejectionMemo::default().key(
            project,
            operation,
            changes,
            followers,
            MemoMetadataMode::Resolve,
        )
    }

    fn capture_cached(
        project: &Project,
        operation: MemoOperation,
        changes: &[&Change],
        followers: &[MemberRef],
        cache: &InputCache,
        context: CaptureContext,
    ) -> Result<Self, CoreError> {
        let captured = capture_inputs(project, followers, cache, &context.original_root)?;
        let mut identities = changes
            .iter()
            .map(|change| {
                let mut members: Vec<_> = change.members.iter().map(MemberIdentity::from).collect();
                members.sort();
                ChangeIdentity {
                    name: change.package.name.clone(),
                    registry: change.package.registry.clone(),
                    from: change.from.to_string(),
                    to: change.to.to_string(),
                    direct: change.direct,
                    members,
                }
            })
            .collect::<Vec<_>>();
        identities.sort();
        let mut followers: Vec<_> = followers.iter().map(MemberIdentity::from).collect();
        followers.sort();
        followers.dedup();
        Ok(Self {
            operation,
            metadata_mode: context.metadata_mode,
            original_root: context.original_root,
            environment: context.environment,
            cacheable: captured.cacheable,
            changes: identities,
            followers,
            inputs: captured.digest,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RejectionEffect {
    pub(crate) key: (String, String, String),
    pub(crate) detail: String,
    pub(crate) overwrite: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MemoRejection {
    pub(crate) effects: Vec<RejectionEffect>,
}

impl MemoRejection {
    pub(crate) fn apply(&self, rejections: &mut BTreeMap<(String, String, String), String>) {
        for effect in &self.effects {
            if effect.overwrite {
                rejections.insert(effect.key.clone(), effect.detail.clone());
            } else {
                rejections
                    .entry(effect.key.clone())
                    .or_insert_with(|| effect.detail.clone());
            }
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct RejectionMemo {
    entries: Mutex<HashMap<MemoKey, MemoRejection>>,
    inputs: InputCache,
    origins: Mutex<HashMap<Utf8PathBuf, Option<Utf8PathBuf>>>,
}

#[derive(Debug, Default)]
struct InputCache {
    documents: Mutex<HashMap<[u8; 32], Option<Arc<toml::Value>>>>,
}

impl InputCache {
    fn document(&self, bytes: &[u8]) -> Option<Arc<toml::Value>> {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        if let Some(document) = self
            .documents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&digest)
        {
            return document.clone();
        }
        let document = std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| toml::from_str(text).ok())
            .map(Arc::new);
        self.documents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(digest, document.clone());
        document
    }
}

impl RejectionMemo {
    // A stable index is assumed within one run.
    // A release published midrun can hold a rejected candidate until the next run.
    // It cannot make an unsafe candidate land.
    pub(crate) fn key(
        &self,
        project: &Project,
        operation: MemoOperation,
        changes: &[&Change],
        followers: &[MemberRef],
        metadata_mode: MemoMetadataMode,
    ) -> Result<MemoKey, CoreError> {
        let original_root = match self
            .origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&project.root)
        {
            Some(Some(original)) => original.clone(),
            Some(None) => {
                return Err(CoreError::Filesystem(
                    "cargo rejection memo origin could not be established".into(),
                ));
            }
            None => resolve_reference(&project.root)?,
        };
        MemoKey::capture_cached(
            project,
            operation,
            changes,
            followers,
            &self.inputs,
            CaptureContext {
                original_root,
                metadata_mode,
                environment: environment_digest(std::env::vars_os()),
            },
        )
    }

    pub(crate) fn register_origin(
        &self,
        staged: &Utf8Path,
        original: &Utf8Path,
    ) -> Result<(), CoreError> {
        self.origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(staged.to_path_buf(), None);
        let canonical_staged = canonical_directory(staged)?;
        let canonical_original = canonical_directory(original)?;
        let mut origins = self
            .origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        origins.insert(staged.to_path_buf(), Some(canonical_original.clone()));
        origins.insert(canonical_staged, Some(canonical_original));
        Ok(())
    }

    pub(crate) fn followers(&self, project: &Project) -> Result<Vec<MemberRef>, CoreError> {
        if project.generated_members.names().is_empty() {
            return Ok(Vec::new());
        }
        let files = capture_files(project, &[], &self.inputs)?;
        let candidates = files
            .iter()
            .filter_map(|(path, bytes)| {
                (path.file_name() == Some(crate::CARGO_MANIFEST) && bytes.is_some())
                    .then(|| path.parent().map(Utf8Path::to_path_buf))
                    .flatten()
            })
            .collect::<Vec<_>>();
        let mut named = BTreeMap::<String, Vec<MemberRef>>::new();
        for member in crate::ownership::workspace_member_directories(&project.root, &candidates) {
            let manifest = member.join(crate::CARGO_MANIFEST);
            let bytes = files
                .get(&manifest)
                .and_then(Option::as_deref)
                .ok_or_else(|| {
                    CoreError::Config(format!("workspace member manifest is missing: {manifest}"))
                })?;
            let document = self.inputs.document(bytes).ok_or_else(|| {
                CoreError::Config(format!("cannot parse workspace member manifest {manifest}"))
            })?;
            let name = document
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    CoreError::Config(format!(
                        "workspace member manifest {manifest} has no package name"
                    ))
                })?;
            let relative = crate::ownership::relativize(&project.root, &member);
            named.entry(name.to_string()).or_default().push(MemberRef {
                name: name.to_string(),
                path: if relative.is_empty() {
                    ".".into()
                } else {
                    relative
                },
            });
        }
        let mut followers = Vec::new();
        for name in project.generated_members.names() {
            let Some(members) = named.get(name) else {
                return Err(CoreError::Config(format!(
                    "[tool.cargo] generated-members names `{name}`, but no workspace member in {} has that package name (members: {}); fix or remove the entry",
                    project.root,
                    named.keys().cloned().collect::<Vec<_>>().join(", ")
                )));
            };
            let [member] = members.as_slice() else {
                return Err(CoreError::Config(format!(
                    "[tool.cargo] generated-members name `{name}` is ambiguous in {}",
                    project.root
                )));
            };
            followers.push(member.clone());
        }
        followers.sort_by(|left, right| (&left.name, &left.path).cmp(&(&right.name, &right.path)));
        followers.dedup();
        Ok(followers)
    }

    pub(crate) fn get(&self, key: &MemoKey) -> Option<MemoRejection> {
        if !key.cacheable {
            return None;
        }
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    pub(crate) fn insert(&self, key: MemoKey, rejection: MemoRejection) {
        if !key.cacheable {
            return;
        }
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, rejection);
    }
}

fn filesystem_error(path: &Utf8Path, err: &std::io::Error) -> CoreError {
    CoreError::Filesystem(format!("{path}: {err}"))
}

fn capture_files(
    project: &Project,
    followers: &[MemberRef],
    cache: &InputCache,
) -> Result<BTreeMap<Utf8PathBuf, Option<Vec<u8>>>, CoreError> {
    let root = &project.root;
    let mut paths = BTreeSet::from([
        root.join(crate::CARGO_MANIFEST),
        root.join("Cargo.lock"),
        root.join(".cargo/config"),
        root.join(".cargo/config.toml"),
    ]);
    for follower in followers {
        paths.insert(root.join(&follower.path).join(crate::CARGO_MANIFEST));
    }
    let mut visited_directories = BTreeSet::new();
    collect_tree(root, &mut paths, &mut visited_directories)?;
    let mut manifests = paths
        .iter()
        .filter(|path| path.file_name() == Some(crate::CARGO_MANIFEST))
        .cloned()
        .collect::<Vec<_>>();
    let mut inspected = BTreeSet::new();
    let mut contents = BTreeMap::new();
    while let Some(manifest) = manifests.pop() {
        // Manifest aliases can resolve relative dependencies against different parent directories.
        if !inspected.insert(manifest.clone()) {
            continue;
        }
        let Some(bytes) = read_input(&manifest)? else {
            contents.insert(manifest, None);
            continue;
        };
        let references = manifest_references(&manifest, &bytes, cache)?;
        contents.insert(manifest, Some(bytes));
        for reference in references {
            let reference = resolve_reference(&reference)?;
            if reference.is_dir() {
                let mut discovered = BTreeSet::new();
                collect_tree(&reference, &mut discovered, &mut visited_directories)?;
                for input in discovered {
                    if paths.insert(input.clone())
                        && input.file_name() == Some(crate::CARGO_MANIFEST)
                    {
                        manifests.push(input);
                    }
                }
            }
            let dependency = reference.join(crate::CARGO_MANIFEST);
            if paths.insert(dependency.clone()) {
                manifests.push(dependency);
            }
            // External path packages can inherit workspace manifests and Cargo configuration.
            if !reference.starts_with(root) {
                for ancestor in reference.ancestors() {
                    for basename in ["Cargo.toml", ".cargo/config", ".cargo/config.toml"] {
                        let input = ancestor.join(basename);
                        if input.exists() && paths.insert(input.clone()) && basename == "Cargo.toml"
                        {
                            manifests.push(input);
                        }
                    }
                }
            }
        }
    }
    for path in paths {
        if let std::collections::btree_map::Entry::Vacant(entry) = contents.entry(path) {
            let bytes = read_input(entry.key())?;
            entry.insert(bytes);
        }
    }
    Ok(contents)
}

struct CapturedDigest {
    digest: [u8; 32],
    cacheable: bool,
}

struct ConfigurationInputs {
    files: BTreeMap<Utf8PathBuf, Option<Vec<u8>>>,
    originals: BTreeSet<Utf8PathBuf>,
    markers: BTreeSet<Utf8PathBuf>,
    cacheable: bool,
}

fn is_cargo_config(path: &Utf8Path) -> bool {
    path.parent().and_then(Utf8Path::file_name) == Some(".cargo")
        && matches!(path.file_name(), Some("config" | "config.toml"))
}

fn capture_configuration_inputs(
    project: &Project,
    followers: &[MemberRef],
    cache: &InputCache,
    original_root: &Utf8Path,
) -> Result<ConfigurationInputs, CoreError> {
    let mut inputs = ConfigurationInputs {
        files: capture_files(project, followers, cache)?,
        originals: BTreeSet::new(),
        markers: environment_local_sources(original_root)?,
        cacheable: true,
    };
    inputs.cacheable = inputs.markers.is_empty();
    let mut configs = inputs
        .files
        .keys()
        .filter(|path| is_cargo_config(path))
        .cloned()
        .collect::<BTreeSet<_>>();
    let (mut config_dirs, ambient_dirs) = crate::staging::cargo_config_dirs(original_root)?;
    config_dirs.extend(ambient_dirs);
    inputs.originals = crate::staging::cargo_config_paths(&config_dirs)?;
    for config in &inputs.originals {
        configs.insert(config.clone());
        if let std::collections::btree_map::Entry::Vacant(entry) =
            inputs.files.entry(config.clone())
        {
            entry.insert(read_input(config)?);
        }
    }
    let mut visited = BTreeSet::new();
    while let Some(config) = configs.pop_first() {
        if !visited.insert(config.clone()) {
            continue;
        }
        let Some(bytes) = inputs.files.get(&config).and_then(Option::as_deref) else {
            continue;
        };
        let document = cache.document(bytes).ok_or_else(|| {
            CoreError::Config(format!("cannot parse Cargo configuration {config}"))
        })?;
        let references = crate::staging::cargo_config_references(&config, &document)?;
        for directory in references.directories.iter().chain(&references.includes) {
            inputs.markers.insert(resolve_reference(directory)?);
            inputs.cacheable = false;
        }
        let from_original = inputs.originals.contains(&config);
        for package in references.packages {
            let package = resolve_reference(&package)?;
            let dependency = Project {
                manifest: package.join(crate::CARGO_MANIFEST),
                root: package,
                kind: crate::CARGO_ID,
                exclude_newer: None,
                generated_members: cooldown_core::GeneratedMembers::default(),
            };
            for (path, bytes) in capture_files(&dependency, &[], cache)? {
                if is_cargo_config(&path) {
                    configs.insert(path.clone());
                }
                if from_original {
                    inputs.originals.insert(path.clone());
                }
                inputs.files.insert(path, bytes);
            }
        }
    }
    Ok(inputs)
}

fn environment_local_sources(original_root: &Utf8Path) -> Result<BTreeSet<Utf8PathBuf>, CoreError> {
    let mut markers = BTreeSet::new();
    for (name, value) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if let Some(value) = value.to_str() {
            if let Some(path) = crate::staging::file_registry_path(value) {
                markers.insert(resolve_reference(&original_root.join(path))?);
            } else if is_local_source_environment(&name) {
                markers.insert(resolve_reference(&original_root.join(value))?);
            }
        }
    }
    Ok(markers)
}

fn is_local_source_environment(name: &str) -> bool {
    #[cfg(windows)]
    let name = name.to_ascii_uppercase();
    name == "CARGO_PATHS"
        || (name.starts_with("CARGO_PATCH_") && name.len() > "CARGO_PATCH_".len())
        || (name.starts_with("CARGO_SOURCE_")
            && ["_DIRECTORY", "_LOCAL_REGISTRY"].iter().any(|suffix| {
                name.ends_with(suffix) && name.len() > "CARGO_SOURCE_".len() + suffix.len()
            }))
}

fn has_complex_member_globs(
    files: &BTreeMap<Utf8PathBuf, Option<Vec<u8>>>,
    cache: &InputCache,
) -> bool {
    files.iter().any(|(path, bytes)| {
        path.file_name() == Some(crate::CARGO_MANIFEST)
            && bytes
                .as_deref()
                .and_then(|bytes| cache.document(bytes))
                .and_then(|document| {
                    document
                        .get("workspace")
                        .and_then(|workspace| workspace.get("members"))
                        .and_then(toml::Value::as_array)
                        .cloned()
                })
                .is_some_and(|members| {
                    members
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .any(|member| {
                            let components = Utf8Path::new(member).components().collect::<Vec<_>>();
                            components.iter().enumerate().any(|(index, component)| {
                                component.as_str().contains("**")
                                    || (index + 1 < components.len()
                                        && component.as_str().contains(['*', '?', '[', '{']))
                            })
                        })
                })
    })
}

fn capture_inputs(
    project: &Project,
    followers: &[MemberRef],
    cache: &InputCache,
    original_root: &Utf8Path,
) -> Result<CapturedDigest, CoreError> {
    let mut inputs = capture_configuration_inputs(project, followers, cache, original_root)?;
    inputs.cacheable &= !has_complex_member_globs(&inputs.files, cache);
    for (path, bytes) in &inputs.files {
        if path.file_name() != Some(crate::CARGO_MANIFEST) {
            continue;
        }
        let Some(document) = bytes.as_deref().and_then(|bytes| cache.document(bytes)) else {
            continue;
        };
        let Some(directory) = path.parent() else {
            continue;
        };
        let mut tables = vec![document.as_ref()];
        while let Some(table) = tables.pop() {
            if let Some(table) = table.as_table() {
                for (name, value) in table {
                    if name == "git"
                        && let Some(source) =
                            value.as_str().and_then(crate::staging::file_registry_path)
                    {
                        inputs
                            .markers
                            .insert(resolve_reference(&directory.join(source))?);
                        inputs.cacheable = false;
                    }
                    tables.push(value);
                }
            } else if let Some(values) = table.as_array() {
                tables.extend(values);
            }
        }
    }
    if !inputs.cacheable {
        tracing::debug!(
            "cargo rejection memo bypassed because local source directories, included configuration, or complex member globs require a larger input closure"
        );
    }
    let mut normalized = BTreeMap::new();
    for (path, bytes) in inputs.files {
        let identity = if inputs.originals.contains(&path) {
            format!("original:{}", resolve_reference(&path)?)
        } else {
            format!(
                "trial:{}",
                crate::ownership::relativize(&project.root, &path)
            )
        };
        normalized.insert(identity, bytes);
    }
    let mut digest = Sha256::new();
    for (identity, bytes) in normalized {
        frame(&mut digest, identity.as_bytes());
        if let Some(bytes) = bytes {
            digest.update([1]);
            frame(&mut digest, &bytes);
        } else {
            digest.update([0]);
        }
    }
    for marker in inputs.markers {
        frame(&mut digest, b"uncaptured-local-source");
        frame(&mut digest, marker.as_str().as_bytes());
    }
    Ok(CapturedDigest {
        digest: digest.finalize().into(),
        cacheable: inputs.cacheable,
    })
}

fn canonical_directory(path: &Utf8Path) -> Result<Utf8PathBuf, CoreError> {
    let canonical = std::fs::canonicalize(path).map_err(|err| filesystem_error(path, &err))?;
    Utf8PathBuf::from_path_buf(canonical)
        .map_err(|path| CoreError::PathEncoding(path.display().to_string()))
}

fn environment_digest(
    environment: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> [u8; 32] {
    let environment = environment.collect::<BTreeMap<_, _>>();
    let mut digest = Sha256::new();
    for (name, value) in environment {
        frame(&mut digest, name.as_encoded_bytes());
        frame(&mut digest, value.as_encoded_bytes());
    }
    digest.finalize().into()
}

fn collect_tree(
    root: &Utf8Path,
    paths: &mut BTreeSet<Utf8PathBuf>,
    visited: &mut BTreeSet<std::path::PathBuf>,
) -> Result<(), CoreError> {
    let mut pending = vec![root.to_path_buf()];
    // Discover member manifests independently of the candidate's owning-member list.
    // Explicit member and path references cover inputs outside the unpruned tree.
    while let Some(directory) = pending.pop() {
        // Capture direct config aliases before canonical directory deduplication.
        if directory.file_name() == Some(".cargo") {
            for basename in ["config", "config.toml"] {
                let config = directory.join(basename);
                match std::fs::symlink_metadata(&config) {
                    Ok(_) => {
                        paths.insert(config);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(filesystem_error(&config, &err)),
                }
            }
        }
        let canonical =
            std::fs::canonicalize(&directory).map_err(|err| filesystem_error(&directory, &err))?;
        if !visited.insert(canonical) {
            continue;
        }
        let mut entries = std::fs::read_dir(&directory)
            .map_err(|err| filesystem_error(&directory, &err))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| filesystem_error(&directory, &err))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
            let kind = entry
                .file_type()
                .map_err(|err| filesystem_error(&path, &err))?;
            if kind.is_symlink() {
                if path.file_name() == Some(".cargo") {
                    for basename in ["config", "config.toml"] {
                        let config = path.join(basename);
                        match std::fs::symlink_metadata(&config) {
                            Ok(_) => {
                                paths.insert(config);
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                            Err(err) => return Err(filesystem_error(&config, &err)),
                        }
                    }
                } else if path.file_name() == Some(crate::CARGO_MANIFEST)
                    || (path.parent().and_then(Utf8Path::file_name) == Some(".cargo")
                        && matches!(path.file_name(), Some("config" | "config.toml")))
                {
                    paths.insert(path);
                } else {
                    // A simple workspace glob can select the alias itself.
                    // Read its manifest directly without following the directory tree.
                    let manifest = path.join(crate::CARGO_MANIFEST);
                    match std::fs::symlink_metadata(&manifest) {
                        Ok(_) => {
                            paths.insert(manifest);
                            for basename in [".cargo/config", ".cargo/config.toml"] {
                                let config = path.join(basename);
                                match std::fs::symlink_metadata(&config) {
                                    Ok(_) => {
                                        paths.insert(config);
                                    }
                                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                                    Err(err) => return Err(filesystem_error(&config, &err)),
                                }
                            }
                        }
                        Err(err)
                            if matches!(
                                err.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                            ) => {}
                        Err(err) => return Err(filesystem_error(&manifest, &err)),
                    }
                }
                continue;
            }
            if kind.is_dir() {
                // Explicit member and path references are inventoried separately.
                // Rescanning discovers new members without trusting a stale path set.
                if matches!(path.file_name(), Some("target" | ".git" | "node_modules"))
                    || path.join("CACHEDIR.TAG").is_file()
                {
                    // A one-level workspace glob can name a pruned directory itself.
                    // Capture its direct inputs without walking its build or cache contents.
                    for basename in ["Cargo.toml", ".cargo/config", ".cargo/config.toml"] {
                        let input = path.join(basename);
                        match std::fs::symlink_metadata(&input) {
                            Ok(_) => {
                                paths.insert(input);
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                            Err(err) => return Err(filesystem_error(&input, &err)),
                        }
                    }
                    continue;
                }
                pending.push(path);
            } else if path.file_name() == Some(crate::CARGO_MANIFEST)
                || (path.parent().and_then(Utf8Path::file_name) == Some(".cargo")
                    && matches!(path.file_name(), Some("config" | "config.toml")))
            {
                paths.insert(path);
            }
        }
    }
    Ok(())
}

fn read_input(path: &Utf8Path) -> Result<Option<Vec<u8>>, CoreError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(filesystem_error(path, &err)),
    }
}

fn resolve_reference(path: &Utf8Path) -> Result<Utf8PathBuf, CoreError> {
    let mut existing = path.to_path_buf();
    let mut suffix = Vec::new();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(canonical) => {
                let mut resolved = Utf8PathBuf::from_path_buf(canonical)
                    .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
                // Retain missing components verbatim: collapsing `missing/..` could hash an
                // existing sibling even though the resolver still fails on the missing directory.
                for component in suffix.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let component = existing
                    .components()
                    .next_back()
                    .ok_or_else(|| filesystem_error(path, &err))?;
                suffix.push(component.as_str().to_owned());
                if !existing.pop() {
                    return Err(CoreError::Filesystem(format!(
                        "cannot find existing ancestor of {path}"
                    )));
                }
            }
            Err(err) => return Err(filesystem_error(path, &err)),
        }
    }
}

fn frame(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

fn manifest_references(
    path: &Utf8Path,
    bytes: &[u8],
    cache: &InputCache,
) -> Result<Vec<Utf8PathBuf>, CoreError> {
    let Some(document) = cache.document(bytes) else {
        return Ok(Vec::new());
    };
    let directory = path
        .parent()
        .ok_or_else(|| CoreError::Filesystem(format!("manifest has no parent: {path}")))?;
    let mut references = Vec::new();
    if let Some(members) = document
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
    {
        for member in members.iter().filter_map(toml::Value::as_str) {
            let joined = directory.join(member);
            // Scanning the literal prefix is a conservative glob expansion; every matching member
            // and config is included without depending on Cargo or the owning-member list.
            let mut prefix = Utf8PathBuf::new();
            for component in joined.components() {
                if component.as_str().contains(['*', '?', '[', '{']) {
                    break;
                }
                prefix.push(component.as_str());
            }
            references.push(prefix);
        }
    }
    let mut tables = vec![document.as_ref()];
    if let Some(workspace) = document.get("workspace") {
        tables.push(workspace);
    }
    if let Some(targets) = document.get("target").and_then(toml::Value::as_table) {
        tables.extend(targets.values());
    }
    for table in tables {
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(dependencies) = table.get(section).and_then(toml::Value::as_table) {
                collect_dependency_paths(directory, dependencies.values(), &mut references);
            }
        }
    }
    if let Some(patches) = document.get("patch").and_then(toml::Value::as_table) {
        for patch in patches.values().filter_map(toml::Value::as_table) {
            collect_dependency_paths(directory, patch.values(), &mut references);
        }
    }
    if let Some(replacements) = document.get("replace").and_then(toml::Value::as_table) {
        collect_dependency_paths(directory, replacements.values(), &mut references);
    }
    if let Some(workspace) = document
        .get("package")
        .and_then(|package| package.get("workspace"))
        .and_then(toml::Value::as_str)
    {
        references.push(directory.join(workspace));
    }
    Ok(references)
}

fn collect_dependency_paths<'a>(
    directory: &Utf8Path,
    dependencies: impl Iterator<Item = &'a toml::Value>,
    references: &mut Vec<Utf8PathBuf>,
) {
    for dependency in dependencies {
        if let Some(path) = dependency.get("path").and_then(toml::Value::as_str) {
            references.push(directory.join(path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use cooldown_core::{GeneratedMembers, PackageId, UpdateKind, Version};
    use indoc::indoc;

    fn fixture() -> eyre::Result<(tempfile::TempDir, Project)> {
        let directory = tempfile::tempdir()?;
        let root = crate::test_support::canonical_root(&directory)?;
        std::fs::create_dir_all(root.join("members/app"))?;
        std::fs::create_dir_all(root.join("members/other"))?;
        std::fs::write(
            root.join("Cargo.toml"),
            indoc! {r#"
            [workspace]
            members = ["members/*"]
        "#},
        )?;
        for member in ["app", "other"] {
            std::fs::write(
                root.join("members").join(member).join("Cargo.toml"),
                format!("[package]\nname = {member:?}\nversion = \"1.0.0\"\n"),
            )?;
        }
        let project = Project {
            manifest: root.join("Cargo.toml"),
            root,
            kind: crate::CARGO_ID,
            exclude_newer: None,
            generated_members: GeneratedMembers::default(),
        };
        Ok((directory, project))
    }

    fn change() -> Change {
        Change {
            package: PackageId::new(crate::CARGO_ID, "dep", None),
            from: Version::new("1.0.0"),
            to: Version::new("2.0.0"),
            kind: UpdateKind::Major,
            downgrade: false,
            direct: true,
            members: vec![MemberRef {
                name: "app".into(),
                path: "members/app".into(),
            }],
        }
    }

    fn key(project: &Project, change: &Change) -> eyre::Result<MemoKey> {
        Ok(MemoKey::capture(
            project,
            MemoOperation::Widen,
            &[change],
            &[],
        )?)
    }

    #[test]
    fn every_resolution_input_invalidates_the_key() -> eyre::Result<()> {
        for relative in [
            "Cargo.toml",
            "Cargo.lock",
            "members/other/Cargo.toml",
            "generated/Cargo.toml",
            ".cargo/config",
            "members/other/.cargo/config.toml",
        ] {
            let (_directory, project) = fixture()?;
            let change = change();
            let before = key(&project, &change)?;
            let path = project.root.join(relative);
            std::fs::create_dir_all(
                path.parent()
                    .ok_or_else(|| eyre::eyre!("input has no parent"))?,
            )?;
            let content = if relative.ends_with("Cargo.toml") {
                "[package]\nname = \"changed\"\nversion = \"1.0.0\"\n"
            } else if relative.contains(".cargo") {
                "[net]\noffline = true\n"
            } else {
                "changed"
            };
            std::fs::write(path, content)?;
            assert_ne!(before, key(&project, &change)?, "{relative}");
        }
        Ok(())
    }

    #[test]
    fn key_preserves_operation_source_members_and_followers() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        let original = change();
        let before = key(&project, &original)?;
        for operation in [MemoOperation::Precise, MemoOperation::Seed] {
            assert_ne!(
                before.clone(),
                MemoKey::capture(&project, operation, &[&original], &[])?
            );
        }
        let mut changed = original.clone();
        changed.package.registry = Some("other-index".into());
        assert_ne!(before, key(&project, &changed)?);
        changed = original.clone();
        changed.members.push(changed.members[0].clone());
        assert_ne!(before, key(&project, &changed)?);
        changed = original.clone();
        changed.direct = false;
        assert_ne!(before, key(&project, &changed)?);
        changed = original.clone();
        changed.members[0].path = "members/other".into();
        assert_ne!(before, key(&project, &changed)?);
        let followers = [MemberRef {
            name: "other".into(),
            path: "members/other".into(),
        }];
        assert_ne!(
            before,
            MemoKey::capture(&project, MemoOperation::Widen, &[&original], &followers)?
        );
        Ok(())
    }

    #[test]
    fn equivalent_roots_share_keys_and_new_members_invalidate() -> eyre::Result<()> {
        let (_first_directory, first) = fixture()?;
        let (_second_directory, second) = fixture()?;
        let change = change();
        assert_ne!(key(&first, &change)?, key(&second, &change)?);
        let memo = RejectionMemo::default();
        memo.register_origin(&second.root, &first.root)?;
        assert_eq!(
            memo.key(
                &first,
                MemoOperation::Widen,
                &[&change],
                &[],
                MemoMetadataMode::Resolve
            )?,
            memo.key(
                &second,
                MemoOperation::Widen,
                &[&change],
                &[],
                MemoMetadataMode::Resolve
            )?
        );
        std::fs::create_dir(first.root.join("members/new"))?;
        std::fs::write(
            first.root.join("members/new/Cargo.toml"),
            "[package]\nname = \"new\"\nversion = \"1.0.0\"\n",
        )?;
        assert_ne!(key(&first, &change)?, key(&second, &change)?);
        Ok(())
    }

    #[test]
    fn unrelated_directories_and_metadata_paths_preserve_the_key() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        std::fs::write(
            project.root.join("members/app/Cargo.toml"),
            indoc! {r#"
            [package]
            name = "app"
            version = "1.0.0"
            [package.metadata.example]
            path = "not-a-dependency"
        "#},
        )?;
        let change = change();
        let before = key(&project, &change)?;
        std::fs::create_dir_all(project.root.join("target/new/cache"))?;
        std::fs::write(project.root.join("target/new/cache/unrelated"), "bytes")?;
        assert_eq!(before, key(&project, &change)?);
        Ok(())
    }

    #[test]
    fn external_path_dependency_bytes_are_captured() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        let external_directory = tempfile::tempdir()?;
        let external = crate::test_support::canonical_root(&external_directory)?;
        let external_path = toml::Value::String(external.to_string()).to_string();
        std::fs::write(
            external.join("Cargo.toml"),
            "[package]\nname = \"external\"\nversion = \"1.0.0\"\n",
        )?;
        std::fs::write(
            project.root.join("members/app/Cargo.toml"),
            format!(
                "[package]\nname = \"app\"\nversion = \"1.0.0\"\n[dependencies.external]\npath = {external_path}\n"
            ),
        )?;
        let change = change();
        let before = key(&project, &change)?;
        std::fs::write(
            external.join("Cargo.toml"),
            "[package]\nname = \"external\"\nversion = \"2.0.0\"\n",
        )?;
        assert_ne!(before, key(&project, &change)?);
        Ok(())
    }

    #[test]
    fn parent_components_in_member_patterns_are_captured() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        std::fs::write(
            &project.manifest,
            "[workspace]\nmembers = [\"members/../members/*\"]\n",
        )?;
        let change = change();
        let before = key(&project, &change)?;
        std::fs::write(
            project.root.join("members/other/Cargo.toml"),
            "[package]\nname = \"changed\"\n",
        )?;
        assert_ne!(before, key(&project, &change)?);
        Ok(())
    }

    #[test]
    fn absolute_external_member_globs_capture_new_members_and_configs() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        let external_directory = tempfile::tempdir()?;
        let external = crate::test_support::canonical_root(&external_directory)?;
        let pattern = toml::Value::String(external.join("members/*").to_string()).to_string();
        std::fs::create_dir_all(external.join("members/first"))?;
        std::fs::write(
            external.join("members/first/Cargo.toml"),
            "[package]\nname = \"first\"\n",
        )?;
        std::fs::write(
            &project.manifest,
            format!("[workspace]\nmembers = [{pattern}]\n"),
        )?;
        let change = change();
        let before = key(&project, &change)?;
        std::fs::create_dir_all(external.join("members/new/.cargo"))?;
        std::fs::write(
            external.join("members/new/Cargo.toml"),
            "[package]\nname = \"new\"\n",
        )?;
        assert_ne!(before, key(&project, &change)?);
        let before_config = key(&project, &change)?;
        std::fs::write(
            external.join("members/new/.cargo/config.toml"),
            "[net]\noffline = true\n",
        )?;
        assert_ne!(before_config, key(&project, &change)?);
        Ok(())
    }

    #[test]
    fn each_change_identity_field_invalidates_the_key() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        let original = change();
        let before = key(&project, &original)?;
        let mut changed = original.clone();
        changed.package.name = "another".into();
        assert_ne!(before, key(&project, &changed)?);
        changed = original.clone();
        changed.from = Version::new("1.1.0");
        assert_ne!(before, key(&project, &changed)?);
        changed = original.clone();
        changed.to = Version::new("2.1.0");
        assert_ne!(before, key(&project, &changed)?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn nested_symlink_config_alias_is_hashed_after_target_was_visited() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        let shared = project.root.join("shared-config");
        std::fs::create_dir(&shared)?;
        std::fs::write(shared.join("config.toml"), "[net]\noffline = true\n")?;
        std::os::unix::fs::symlink(&shared, project.root.join("members/app/.cargo"))?;
        let change = change();
        let before = key(&project, &change)?;
        std::fs::write(shared.join("config.toml"), "[net]\noffline = false\n")?;
        assert_ne!(before, key(&project, &change)?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlink_parent_dependency_tracks_the_physical_sibling() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        let external_directory = tempfile::tempdir()?;
        let external = crate::test_support::canonical_root(&external_directory)?;
        std::fs::create_dir_all(external.join("linked"))?;
        std::fs::create_dir_all(external.join("dep"))?;
        std::fs::write(
            external.join("dep/Cargo.toml"),
            "[package]\nname = \"dep\"\nversion = \"1.0.0\"\n",
        )?;
        std::os::unix::fs::symlink(
            external.join("linked"),
            project.root.join("members/app/link"),
        )?;
        std::fs::write(
            project.root.join("members/app/Cargo.toml"),
            indoc! {r#"
            [package]
            name = "app"
            version = "1.0.0"
            [dependencies.dep]
            path = "link/../dep"
        "#},
        )?;
        let memo = RejectionMemo::default();
        let change = change();
        let before = memo.key(
            &project,
            MemoOperation::Precise,
            &[&change],
            &[],
            MemoMetadataMode::Resolve,
        )?;
        std::fs::write(
            external.join("dep/Cargo.toml"),
            "[package]\nname = \"dep\"\nversion = \"2.0.0\"\n",
        )?;
        assert_ne!(
            before,
            memo.key(
                &project,
                MemoOperation::Precise,
                &[&change],
                &[],
                MemoMetadataMode::Resolve
            )?
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn missing_reference_preserves_symlink_parent_semantics() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        let external_directory = tempfile::tempdir()?;
        let external = crate::test_support::canonical_root(&external_directory)?;
        std::fs::create_dir(external.join("linked"))?;
        std::os::unix::fs::symlink(
            external.join("linked"),
            project.root.join("members/app/link"),
        )?;
        let path = project.root.join("members/app/link/../missing");
        assert_eq!(resolve_reference(&path)?, external.join("missing"));
        let path = project.root.join("members/app/link/missing/../dep");
        assert_eq!(
            resolve_reference(&path)?,
            external.join("linked/missing/../dep")
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn manifest_aliases_capture_each_relative_dependency_context() -> eyre::Result<()> {
        let (_directory, mut project) = fixture()?;
        let outer = project.root.clone();
        project.root = outer.join("project");
        project.manifest = project.root.join("Cargo.toml");
        for relative in ["project/members/app", "project/b", "project/deps", "deps"] {
            std::fs::create_dir_all(outer.join(relative))?;
        }
        std::fs::write(
            &project.manifest,
            indoc! {r#"
            [workspace]
            members = ["members/*", "b"]
        "#},
        )?;
        let shared = outer.join("shared.toml");
        std::fs::write(
            &shared,
            indoc! {r#"
            [package]
            name = "owner"
            version = "1.0.0"
            [dependencies.dep]
            path = "../../deps"
        "#},
        )?;
        for relative in ["members/app/Cargo.toml", "b/Cargo.toml"] {
            std::os::unix::fs::symlink(&shared, project.root.join(relative))?;
        }
        for relative in ["project/deps/Cargo.toml", "deps/Cargo.toml"] {
            std::fs::write(
                outer.join(relative),
                "[package]\nname = \"dep\"\nversion = \"1.0.0\"\n",
            )?;
        }
        let change = change();
        let before = key(&project, &change)?;
        std::fs::write(
            outer.join("deps/Cargo.toml"),
            "[package]\nname = \"dep\"\nversion = \"2.0.0\"\n",
        )?;
        assert_ne!(before, key(&project, &change)?);
        Ok(())
    }

    #[test]
    fn filesystem_followers_select_named_workspace_members_only() -> eyre::Result<()> {
        let (_directory, mut project) = fixture()?;
        std::fs::create_dir(project.root.join("parked"))?;
        std::fs::write(
            project.root.join("parked/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\n",
        )?;
        project.generated_members = GeneratedMembers::declared(vec!["other".into(), "app".into()]);
        assert_eq!(
            RejectionMemo::default().followers(&project)?,
            vec![
                MemberRef {
                    name: "app".into(),
                    path: "members/app".into()
                },
                MemberRef {
                    name: "other".into(),
                    path: "members/other".into()
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn filesystem_followers_reject_excluded_and_ambiguous_names() -> eyre::Result<()> {
        let (_directory, mut project) = fixture()?;
        project.generated_members = GeneratedMembers::declared(vec!["other".into()]);
        std::fs::write(
            &project.manifest,
            "[workspace]\nmembers = [\"members/app\"]\nexclude = [\"members/other\"]\n",
        )?;
        assert!(matches!(
            RejectionMemo::default().followers(&project),
            Err(CoreError::Config(_))
        ));
        std::fs::write(
            &project.manifest,
            "[workspace]\nmembers = [\"members/*\"]\n",
        )?;
        std::fs::write(
            project.root.join("members/app/Cargo.toml"),
            "[package]\nname = \"other\"\nversion = \"1.0.0\"\n",
        )?;
        assert!(matches!(
            RejectionMemo::default().followers(&project),
            Err(CoreError::Config(_))
        ));
        Ok(())
    }

    #[test]
    fn filesystem_followers_include_implicit_path_members() -> eyre::Result<()> {
        let (_directory, mut project) = fixture()?;
        project.generated_members = GeneratedMembers::declared(vec!["other".into()]);
        std::fs::write(
            &project.manifest,
            "[workspace]\nmembers = [\"members/app\"]\n",
        )?;
        std::fs::write(
            project.root.join("members/app/Cargo.toml"),
            indoc! {r#"
            [package]
            name = "app"
            version = "1.0.0"
            [dependencies.other]
            path = "../other"
        "#},
        )?;
        assert_eq!(
            RejectionMemo::default().followers(&project)?,
            vec![MemberRef {
                name: "other".into(),
                path: "members/other".into()
            }]
        );
        Ok(())
    }

    fn nested_project(base: &Utf8Path, relative: &str) -> eyre::Result<Project> {
        let root = base.join(relative);
        std::fs::create_dir_all(&root)?;
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\n",
        )?;
        Ok(Project {
            manifest: root.join("Cargo.toml"),
            root,
            kind: crate::CARGO_ID,
            exclude_newer: None,
            generated_members: GeneratedMembers::default(),
        })
    }

    #[test]
    fn original_ancestor_configuration_changes_the_key() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let base = crate::test_support::canonical_root(&directory)?;
        let first = nested_project(&base, "first/project")?;
        let second = nested_project(&base, "second/project")?;
        for ancestor in ["first", "second"] {
            std::fs::create_dir(base.join(ancestor).join(".cargo"))?;
            std::fs::write(
                base.join(ancestor).join(".cargo/config.toml"),
                "[net]\noffline = true\n",
            )?;
        }
        let change = change();
        assert_ne!(key(&first, &change)?, key(&second, &change)?);
        let before = key(&first, &change)?;
        std::fs::write(
            base.join("first/.cargo/config.toml"),
            "[net]\noffline = false\n",
        )?;
        assert_ne!(before, key(&first, &change)?);
        Ok(())
    }

    #[test]
    fn staged_copies_share_origin_and_external_input_identities() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let base = crate::test_support::canonical_root(&directory)?;
        let source = nested_project(&base, "source")?;
        let first = nested_project(&base, "stage-a/tree/source")?;
        let second = nested_project(&base, "stage-b/tree/source")?;
        for project in [&source, &first, &second] {
            let dependency = project
                .root
                .parent()
                .ok_or_else(|| eyre::eyre!("source has no parent"))?
                .join("dep");
            std::fs::create_dir_all(&dependency)?;
            std::fs::write(
                dependency.join("Cargo.toml"),
                "[package]\nname = \"dep\"\nversion = \"1.0.0\"\n",
            )?;
            std::fs::write(
                &project.manifest,
                indoc! {r#"
                [package]
                name = "app"
                version = "1.0.0"
                [dependencies.dep]
                path = "../dep"
            "#},
            )?;
        }
        let memo = RejectionMemo::default();
        memo.register_origin(&first.root, &source.root)?;
        memo.register_origin(&second.root, &source.root)?;
        let change = change();
        let first_key = memo.key(
            &first,
            MemoOperation::Precise,
            &[&change],
            &[],
            MemoMetadataMode::Resolve,
        )?;
        let second_key = memo.key(
            &second,
            MemoOperation::Precise,
            &[&change],
            &[],
            MemoMetadataMode::Resolve,
        )?;
        assert_eq!(first_key, second_key);
        Ok(())
    }

    #[test]
    fn original_config_patch_closures_share_across_staged_root_depths() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let base = crate::test_support::canonical_root(&directory)?;
        let source = nested_project(&base, "source")?;
        let first = nested_project(&base, "stage-a/tree/source")?;
        let second = nested_project(&base, "stage-b/source")?;
        let patched = nested_project(&base, "patched")?;
        std::fs::create_dir(base.join(".cargo"))?;
        std::fs::write(
            base.join(".cargo/config.toml"),
            indoc! {r#"
            [patch.crates-io.dep]
            path = "patched"
        "#},
        )?;
        let memo = RejectionMemo::default();
        memo.register_origin(&first.root, &source.root)?;
        memo.register_origin(&second.root, &source.root)?;
        let first_key = memo.key(
            &first,
            MemoOperation::Seed,
            &[],
            &[],
            MemoMetadataMode::Skip,
        )?;
        let second_key = memo.key(
            &second,
            MemoOperation::Seed,
            &[],
            &[],
            MemoMetadataMode::Skip,
        )?;
        assert_eq!(first_key, second_key);
        std::fs::write(
            &patched.manifest,
            "[package]\nname = \"app\"\nversion = \"2.0.0\"\n",
        )?;
        assert_ne!(
            first_key,
            memo.key(
                &first,
                MemoOperation::Seed,
                &[],
                &[],
                MemoMetadataMode::Skip
            )?
        );
        Ok(())
    }

    #[test]
    fn every_file_registry_url_form_bypasses_storage() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        std::fs::create_dir(project.root.join(".cargo"))?;
        std::fs::create_dir(project.root.join("registry"))?;
        let registry = project.root.join("registry");
        for prefix in ["file:", "FiLe:", "  sparse+FILE:", "file://"] {
            let index = toml::Value::String(format!("{prefix}{registry}")).to_string();
            std::fs::write(
                project.root.join(".cargo/config.toml"),
                format!("[registries.local]\nindex = {index}\n"),
            )?;
            let memo = RejectionMemo::default();
            let key = memo.key(
                &project,
                MemoOperation::Seed,
                &[],
                &[],
                MemoMetadataMode::Skip,
            )?;
            assert!(!key.cacheable, "{prefix}");
            memo.insert(key.clone(), MemoRejection::default());
            assert!(memo.get(&key).is_none(), "{prefix}");
        }
        Ok(())
    }

    #[test]
    fn referenced_source_directories_never_replay_or_store() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        std::fs::create_dir_all(project.root.join(".cargo"))?;
        std::fs::create_dir(project.root.join("vendor"))?;
        std::fs::write(
            project.root.join(".cargo/config.toml"),
            indoc! {r#"
            [source.crates-io]
            replace-with = "vendored"
            [source.vendored]
            directory = "vendor"
        "#},
        )?;
        let memo = RejectionMemo::default();
        let key = memo.key(
            &project,
            MemoOperation::Seed,
            &[],
            &[],
            MemoMetadataMode::Skip,
        )?;
        assert!(!key.cacheable);
        memo.insert(key.clone(), MemoRejection::default());
        assert!(memo.get(&key).is_none());
        Ok(())
    }

    #[test]
    fn failed_origin_registration_disables_the_staged_root() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        let memo = RejectionMemo::default();
        assert!(
            memo.register_origin(&project.root, &project.root.join("missing"))
                .is_err()
        );
        assert!(
            memo.key(
                &project,
                MemoOperation::Seed,
                &[],
                &[],
                MemoMetadataMode::Skip
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn package_metadata_environment_is_not_a_local_resolver_source() {
        for name in [
            "CARGO_PKG_VERSION_PATCH",
            "CARGO_PKG_VERSION",
            "CARGO_PKG_NAME",
            "CARGO_MANIFEST_DIR",
            "CARGO_TARGET_DIR",
            "CARGO_SOURCE_DIRECTORY",
        ] {
            assert!(!is_local_source_environment(name), "{name}");
        }
        for name in [
            "CARGO_SOURCE_VENDOR_DIRECTORY",
            "CARGO_SOURCE_PRIVATE_LOCAL_REGISTRY",
            "CARGO_PATCH_CRATES_IO_DEP_PATH",
            "CARGO_PATHS",
        ] {
            assert!(is_local_source_environment(name), "{name}");
        }
    }

    #[test]
    fn metadata_mode_and_environment_values_are_key_inputs() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        let memo = RejectionMemo::default();
        assert_ne!(
            memo.key(
                &project,
                MemoOperation::Widen,
                &[],
                &[],
                MemoMetadataMode::Skip
            )?,
            memo.key(
                &project,
                MemoOperation::Widen,
                &[],
                &[],
                MemoMetadataMode::Resolve
            )?
        );
        let first = environment_digest(
            [(
                std::ffi::OsString::from("CARGO_NET_OFFLINE"),
                std::ffi::OsString::from("true"),
            )]
            .into_iter(),
        );
        let second = environment_digest(
            [(
                std::ffi::OsString::from("CARGO_NET_OFFLINE"),
                std::ffi::OsString::from("false"),
            )]
            .into_iter(),
        );
        assert_ne!(first, second);
        let first_key = MemoKey::capture_cached(
            &project,
            MemoOperation::Widen,
            &[],
            &[],
            &InputCache::default(),
            CaptureContext {
                original_root: project.root.clone(),
                metadata_mode: MemoMetadataMode::Skip,
                environment: first,
            },
        )?;
        let second_key = MemoKey::capture_cached(
            &project,
            MemoOperation::Widen,
            &[],
            &[],
            &InputCache::default(),
            CaptureContext {
                original_root: project.root.clone(),
                metadata_mode: MemoMetadataMode::Skip,
                environment: second,
            },
        )?;
        assert_ne!(first_key, second_key);
        Ok(())
    }

    #[test]
    fn recursive_member_globs_bypass_pruned_inventory() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        std::fs::write(
            &project.manifest,
            "[workspace]\nmembers = [\"members/**\"]\n",
        )?;
        let memo = RejectionMemo::default();
        let key = memo.key(
            &project,
            MemoOperation::Seed,
            &[],
            &[],
            MemoMetadataMode::Skip,
        )?;
        assert!(!key.cacheable);
        Ok(())
    }

    #[test]
    fn build_and_cache_directory_contents_are_pruned() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        for directory in ["target", ".git", "node_modules", "cache"] {
            let nested = project.root.join(directory).join("nested/.cargo");
            std::fs::create_dir_all(&nested)?;
            if directory == "cache" {
                std::fs::write(
                    project.root.join(directory).join("CACHEDIR.TAG"),
                    "Signature: 8a477f597d28d172789f06886806bc55a",
                )?;
            }
            std::fs::write(nested.join("config.toml"), "[net]\noffline = true\n")?;
        }
        let change = change();
        let before = key(&project, &change)?;
        for directory in ["target", ".git", "node_modules", "cache"] {
            std::fs::write(
                project
                    .root
                    .join(directory)
                    .join("nested/.cargo/config.toml"),
                "[net]\noffline = false\n",
            )?;
        }
        assert_eq!(before, key(&project, &change)?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unrelated_directory_symlinks_are_not_walked() -> eyre::Result<()> {
        let (_directory, project) = fixture()?;
        let external_directory = tempfile::tempdir()?;
        let external = crate::test_support::canonical_root(&external_directory)?;
        std::fs::create_dir(external.join(".cargo"))?;
        std::fs::write(
            external.join(".cargo/config.toml"),
            "[net]\noffline = true\n",
        )?;
        std::os::unix::fs::symlink(&external, project.root.join("assets"))?;
        let change = change();
        let before = key(&project, &change)?;
        std::fs::write(
            external.join(".cargo/config.toml"),
            "[net]\noffline = false\n",
        )?;
        assert_eq!(before, key(&project, &change)?);
        Ok(())
    }

    #[test]
    fn replay_retains_effect_order_and_overwrite_policy() {
        let key = ("dep".into(), "1".into(), "2".into());
        let effects = vec![
            RejectionEffect {
                key: key.clone(),
                detail: "first".into(),
                overwrite: false,
            },
            RejectionEffect {
                key: key.clone(),
                detail: "ignored".into(),
                overwrite: false,
            },
            RejectionEffect {
                key: key.clone(),
                detail: "last".into(),
                overwrite: true,
            },
        ];
        let mut rejections = BTreeMap::new();
        MemoRejection { effects }.apply(&mut rejections);
        assert_eq!(rejections.get(&key).map(String::as_str), Some("last"));
        MemoRejection::default().apply(&mut rejections);
        assert_eq!(rejections.len(), 1);
    }
}
