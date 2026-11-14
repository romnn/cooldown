//! The staged copies of the run, and the rule deciding when a Cargo outcome in one may be reused.
//!
//! cooldown stages regular files in a staged copy's private tree and writes nothing there but the
//! lock and the manifests the native input key captures.
//! While every package Cargo loads and every target source it names lie in that tree, and Cargo
//! reads no configuration from outside it, the target files Cargo discovers stay as staged.
//! A Cargo outcome in the copy is then a function of the key, the directory and the arguments.
//! Nothing is reused in any other directory: a project's source root is the user's, and anything
//! may change the target files there between two runs of Cargo.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::Project;

use super::stable_resolution;
use crate::cargocmd::CommandLine;
use crate::publish_age::CargoEnvironment;
use crate::rejection_memo::{LockSource, MemoKey, MemoOperation, RejectionMemo};

/// The registered staged project roots of the run.
#[derive(Debug, Default)]
pub(crate) struct StagedTrees {
    trees: Mutex<HashMap<Utf8PathBuf, StagedTree>>,
    registrations: AtomicU64,
}

/// One registration of a staged project root.
#[derive(Debug, Clone)]
struct StagedTree {
    /// The canonical directory the copy lives in, which only cooldown writes.
    tree: Utf8PathBuf,
    registration: u64,
}

/// What a reusable outcome in a staged copy depends on besides its directory and arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedInputs {
    /// The registration of the copy, unique in the run.
    ///
    /// A later copy may reuse the path of one already removed, with other target files, so an
    /// outcome recorded in one registration is never reused in another.
    registration: u64,
    pub(crate) key: MemoKey,
}

impl StagedInputs {
    /// The same inputs, keyed for `operation`.
    pub(crate) fn with_operation(self, operation: MemoOperation) -> Self {
        Self {
            key: self.key.with_operation(operation),
            ..self
        }
    }
}

impl StagedTrees {
    /// Registers `staged` as a staged project root whose copy lives in the canonical directory
    /// `tree`, which only cooldown writes.
    pub(crate) fn register(&self, staged: &Utf8Path, tree: &Utf8Path) {
        let registration = self.registrations.fetch_add(1, Ordering::Relaxed);
        self.trees
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                staged.to_owned(),
                StagedTree {
                    tree: tree.to_owned(),
                    registration,
                },
            );
    }

    fn get(&self, root: &Utf8Path) -> Option<StagedTree> {
        self.trees
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .cloned()
    }

    /// The inputs of the Cargo command `command` run in `project`, with the lock taken from
    /// `lock`, or `None` when its outcome is not reusable.
    ///
    /// An outcome is reusable only in a registered staged copy, while the resolution depends on
    /// neither the time nor the crates downloaded so far, Cargo reads only the configuration the
    /// key captures, and every package Cargo loads and target source a manifest names lie in the
    /// copy's tree.
    pub(crate) fn reusable_inputs(
        &self,
        memo: &RejectionMemo,
        project: &Project,
        command: &CommandLine,
        lock: LockSource,
    ) -> Option<StagedInputs> {
        let StagedTree { tree, registration } = self.get(&project.root)?;
        if !stable_resolution(
            &project.root,
            &CargoEnvironment::current(),
            command.crates_io_age.as_deref(),
        ) {
            return None;
        }
        if !crate::staging::staged_configuration_captured(&tree) {
            tracing::debug!(
                dir = %project.root,
                "cargo outcome reuse bypassed because configuration outside the staged copy applies"
            );
            return None;
        }
        let inputs = memo
            .command_inputs(project, lock)
            .inspect_err(|err| tracing::debug!(%err, "cargo outcome reuse input capture bypassed"))
            .ok()?;
        if !inputs.key.is_cacheable() {
            return None;
        }
        let private = inputs.manifests.iter().all(|manifest| {
            within(&manifest.directory, &tree)
                && declared_targets(&manifest.directory, &manifest.document)
                    .iter()
                    .all(|target| within(target, &tree))
        });
        if !private {
            tracing::debug!(
                dir = %project.root,
                "cargo outcome reuse bypassed because a package or target source lies outside the staged copy"
            );
            return None;
        }
        Some(StagedInputs {
            registration,
            key: inputs.key,
        })
    }
}

/// Whether `path`, with every symbolic link resolved, lies in `tree`; `false` when it does not
/// exist.
fn within(path: &Utf8Path, tree: &Utf8Path) -> bool {
    std::fs::canonicalize(path)
        .ok()
        .and_then(|path| Utf8PathBuf::from_path_buf(path).ok())
        .is_some_and(|path| path.starts_with(tree))
}

/// The path a target table of a manifest names, if any.
fn path_of(table: &toml::Value) -> Option<&str> {
    table.get("path").and_then(toml::Value::as_str)
}

/// The target sources the manifest `document` in `directory` names by path.
///
/// A target named without a path is inferred from the package directory itself.
fn declared_targets(directory: &Utf8Path, document: &toml::Value) -> Vec<Utf8PathBuf> {
    let mut paths: Vec<&str> = Vec::new();
    paths.extend(document.get("lib").and_then(path_of));
    for kind in ["bin", "example", "test", "bench"] {
        let tables = document.get(kind).and_then(toml::Value::as_array);
        paths.extend(tables.into_iter().flatten().filter_map(path_of));
    }
    match document
        .get("package")
        .and_then(|package| package.get("build"))
    {
        Some(toml::Value::String(build)) => paths.push(build),
        Some(toml::Value::Array(builds)) => {
            paths.extend(builds.iter().filter_map(toml::Value::as_str));
        }
        _ => {}
    }
    paths.into_iter().map(|path| directory.join(path)).collect()
}
