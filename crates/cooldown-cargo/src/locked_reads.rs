//! Run-scoped reuse of `cargo metadata --locked` reads.
//!
//! A locked read never writes the lock, and its outcome depends on the native inputs the
//! [rejection memo](crate::rejection_memo) captures, given the stable index the memo assumes for
//! one run, and on the target files Cargo discovers in path packages.
//! Two reads of the same root with the same inputs and target files therefore return the same
//! graph or the same stale-lock refusal, and an upgrade's trials reread identical files many
//! times over.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, Project};
use sha2::{Digest as _, Sha256};

use crate::cargocmd::{Cargo, ResolvedGraph};
use crate::rejection_memo::{MemoKey, MemoMetadataMode, MemoOperation, RejectionMemo};

/// The recent locked reads of the run, newest last.
#[derive(Debug, Default)]
pub(crate) struct LockedReads {
    reads: Mutex<VecDeque<LockedRead>>,
}

#[derive(Debug)]
struct LockedRead {
    /// The directory Cargo ran in, which the graph's paths name.
    root: Utf8PathBuf,
    inputs: MemoKey,
    outcome: ReadOutcome,
}

#[derive(Debug, Clone)]
enum ReadOutcome {
    /// Cargo's metadata output, with the path packages it lists and what their target discovery
    /// read.
    Graph {
        stdout: Arc<str>,
        packages: Arc<[PathPackage]>,
        discovery: [u8; 32],
    },
    /// The detail of Cargo's refusal to read a lock that needs updating.
    Stale(String),
}

impl ReadOutcome {
    /// Whether the files this outcome was read from still stand.
    ///
    /// Cargo discovers each path package's targets from its source tree, which the input key does
    /// not capture: a lost target source, or a new file competing for a target's name, fails a
    /// read that once succeeded.
    fn current(&self) -> bool {
        match self {
            ReadOutcome::Graph {
                packages,
                discovery,
                ..
            } => discovery_digest(packages) == *discovery,
            ReadOutcome::Stale(_) => true,
        }
    }
}

/// A path package of a locked read: its directory and the source files of the targets Cargo
/// listed for it.
#[derive(Debug)]
struct PathPackage {
    directory: Utf8PathBuf,
    targets: Vec<Utf8PathBuf>,
}

/// The path packages in Cargo's metadata output, or `None` when the output does not parse.
fn path_packages(stdout: &str) -> Option<Vec<PathPackage>> {
    #[derive(serde::Deserialize)]
    struct Metadata {
        packages: Vec<Package>,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        source: Option<String>,
        manifest_path: Utf8PathBuf,
        #[serde(default)]
        targets: Vec<Target>,
    }
    #[derive(serde::Deserialize)]
    struct Target {
        src_path: Utf8PathBuf,
    }
    let metadata: Metadata = serde_json::from_str(stdout).ok()?;
    metadata
        .packages
        .into_iter()
        .filter(|package| package.source.is_none())
        .map(|package| {
            Some(PathPackage {
                directory: package.manifest_path.parent()?.to_owned(),
                targets: package
                    .targets
                    .into_iter()
                    .map(|target| target.src_path)
                    .collect(),
            })
        })
        .collect()
}

/// The files beside a package's manifest Cargo infers a target from.
const INFERRED_FILES: [&str; 3] = ["build.rs", "src/lib.rs", "src/main.rs"];

/// The directories beside a package's manifest whose `*.rs` files and `*/main.rs` subdirectories
/// Cargo infers targets from.
const INFERRED_DIRECTORIES: [&str; 4] = ["src/bin", "examples", "tests", "benches"];

/// One entry of a directory Cargo infers targets from.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct InferredEntry {
    name: String,
    /// The entry's own type, unfollowed: Cargo infers a target from a real directory holding
    /// `main.rs`, never from a symbolic link to one.
    kind: EntryKind,
    /// Whether the entry, followed, is a directory holding `main.rs`.
    main: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl InferredEntry {
    fn of(entry: &std::fs::DirEntry) -> Self {
        let kind = match entry.file_type() {
            Ok(kind) if kind.is_symlink() => EntryKind::Symlink,
            Ok(kind) if kind.is_dir() => EntryKind::Directory,
            Ok(kind) if kind.is_file() => EntryKind::File,
            Ok(_) | Err(_) => EntryKind::Other,
        };
        let path = entry.path();
        InferredEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
            main: path.is_dir() && path.join("main.rs").exists(),
        }
    }
}

/// Feeds `bytes` to `digest` behind their length, so adjacent fields cannot run together.
fn frame(digest: &mut Sha256, bytes: &[u8]) {
    digest.update(bytes.len().to_le_bytes());
    digest.update(bytes);
}

/// A digest of what target discovery reads in `packages`: whether each listed target source
/// exists, and every file and directory in the places Cargo infers targets from.
///
/// It is taken after Cargo returns, so a target file an outside process changes while Cargo runs
/// can be missed; cooldown itself never writes target files.
fn discovery_digest(packages: &[PathPackage]) -> [u8; 32] {
    let mut digest = Sha256::new();
    for package in packages {
        frame(&mut digest, package.directory.as_str().as_bytes());
        for target in &package.targets {
            frame(&mut digest, target.as_str().as_bytes());
            digest.update([u8::from(target.exists())]);
        }
        for file in INFERRED_FILES {
            digest.update([u8::from(package.directory.join(file).exists())]);
        }
        for directory in INFERRED_DIRECTORIES {
            let mut entries: Vec<InferredEntry> =
                std::fs::read_dir(package.directory.join(directory))
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .map(|entry| InferredEntry::of(&entry))
                    .collect();
            entries.sort();
            frame(&mut digest, directory.as_bytes());
            for InferredEntry { name, kind, main } in entries {
                frame(&mut digest, name.as_bytes());
                digest.update([kind as u8, u8::from(main)]);
            }
        }
    }
    digest.finalize().into()
}

impl LockedReads {
    /// The reads kept; each holds one metadata output of the workspace.
    const CAPACITY: usize = 16;

    fn get(&self, root: &Utf8Path, inputs: &MemoKey) -> Option<ReadOutcome> {
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find(|read| read.root == root && &read.inputs == inputs)
            .map(|read| read.outcome.clone())
            .filter(ReadOutcome::current)
    }

    /// Forgets every read.
    #[cfg(all(test, unix))]
    pub(crate) fn clear(&self) {
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn insert(&self, read: LockedRead) {
        let mut reads = self
            .reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reads.retain(|kept| kept.root != read.root || kept.inputs != read.inputs);
        if reads.len() == Self::CAPACITY {
            reads.pop_front();
        }
        reads.push_back(read);
    }
}

/// Reads locked graphs through [`LockedReads`].
#[derive(Clone, Copy)]
pub(crate) struct LockedReader<'a> {
    pub(crate) cargo: &'a Cargo,
    pub(crate) memo: &'a RejectionMemo,
    pub(crate) reads: &'a LockedReads,
}

impl LockedReader<'_> {
    /// Reads `project`'s locked graph, reusing an earlier read of the same inputs in this run.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::StaleLock`] when the lock must be updated, or the errors of
    /// [`Cargo::metadata_locked`] for a read that ran.
    pub(crate) async fn graph(&self, project: &Project) -> Result<ResolvedGraph, CoreError> {
        let inputs = self.inputs(project);
        if let Some(inputs) = &inputs
            && let Some(outcome) = self.reads.get(&project.root, inputs)
        {
            return match outcome {
                ReadOutcome::Graph { stdout, .. } => {
                    Cargo::parse_graph(&stdout, &project.generated_members)
                }
                ReadOutcome::Stale(detail) => Err(CoreError::StaleLock(detail)),
            };
        }
        let read = self.cargo.locked_metadata_output(&project.root).await;
        // An input that changed while Cargo ran may have been read in its new state, so the
        // outcome is kept only when the inputs it was keyed by still stand.
        if let Some(inputs) = inputs
            && self.inputs(project).as_ref() == Some(&inputs)
        {
            let outcome = match &read {
                Ok(stdout) => path_packages(stdout).map(|packages| ReadOutcome::Graph {
                    stdout: Arc::from(stdout.as_str()),
                    discovery: discovery_digest(&packages),
                    packages: Arc::from(packages),
                }),
                Err(CoreError::StaleLock(detail)) => Some(ReadOutcome::Stale(detail.clone())),
                Err(_) => None,
            };
            if let Some(outcome) = outcome {
                self.reads.insert(LockedRead {
                    root: project.root.clone(),
                    inputs,
                    outcome,
                });
            }
        }
        Cargo::parse_graph(&read?, &project.generated_members)
    }

    /// Keeps `stdout` as the locked read of `project`'s current `inputs`, whose key is captured for
    /// a [`MemoOperation::LockedRead`].
    ///
    /// The caller vouches that a locked read of these inputs would succeed with this output now.
    pub(crate) fn keep_settled(&self, project: &Project, inputs: MemoKey, stdout: &str) {
        if !inputs.is_cacheable() {
            return;
        }
        let Some(packages) = path_packages(stdout) else {
            return;
        };
        self.reads.insert(LockedRead {
            root: project.root.clone(),
            inputs,
            outcome: ReadOutcome::Graph {
                stdout: Arc::from(stdout),
                discovery: discovery_digest(&packages),
                packages: Arc::from(packages),
            },
        });
    }

    /// The key of `project`'s native inputs, or `None` when they cannot all be captured.
    fn inputs(&self, project: &Project) -> Option<MemoKey> {
        self.memo
            .key(
                project,
                MemoOperation::LockedRead,
                &[],
                &[],
                MemoMetadataMode::Resolve,
            )
            .ok()
            .filter(MemoKey::is_cacheable)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use indoc::indoc;
    use std::os::unix::fs::PermissionsExt as _;

    /// The fake `cargo`: a locked read logs itself and is refused as stale while a `stale` file
    /// exists, and otherwise prints the member `app` with its library target.
    ///
    /// With a `race` file, the read changes the lock while it runs.
    const SCRIPT: &str = indoc! {r#"
        #!/bin/sh
        printf '%s\n' "$*" >> reads
        if [ -f race ]; then printf '# raced\n' >> Cargo.lock; fi
        if [ -f stale ]; then
          echo 'error: cannot update the lock file Cargo.lock because --locked was passed to prevent this' >&2
          exit 101
        fi
        printf '{"packages": [{"id": "app", "name": "app", "version": "0.1.0", "source": null, "manifest_path": "%s/Cargo.toml", "targets": [{"src_path": "%s/src/lib.rs"}]}], "workspace_members": [], "workspace_root": "", "resolve": null}\n' "$PWD" "$PWD"
    "#};

    const MANIFEST: &str = indoc! {r#"
        [package]
        name = "app"
        version = "0.1.0"
        edition = "2024"
    "#};

    struct ReadFixture {
        root: Utf8PathBuf,
        project: Project,
        cargo: Cargo,
        memo: RejectionMemo,
        reads: LockedReads,
        #[expect(dead_code, reason = "dropping the guard deletes the project directory")]
        directory: tempfile::TempDir,
    }

    impl ReadFixture {
        fn new() -> eyre::Result<Self> {
            let directory = tempfile::tempdir()?;
            let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
                .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
            std::fs::write(root.join("Cargo.toml"), MANIFEST)?;
            std::fs::write(root.join("Cargo.lock"), "version = 4\n")?;
            std::fs::create_dir_all(root.join("src"))?;
            std::fs::write(root.join("src/lib.rs"), "")?;
            let fake = root.join("fake-cargo");
            std::fs::write(&fake, SCRIPT)?;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
            let project = Project {
                manifest: root.join("Cargo.toml"),
                root: root.clone(),
                kind: crate::CARGO_ID,
                exclude_newer: None,
                exclude_newer_package: std::collections::BTreeMap::new(),
                generated_members: cooldown_core::GeneratedMembers::undeclared(),
            };
            Ok(Self {
                cargo: Cargo::with_bin(fake.as_str()),
                root,
                project,
                memo: RejectionMemo::default(),
                reads: LockedReads::default(),
                directory,
            })
        }

        fn reader(&self) -> LockedReader<'_> {
            LockedReader {
                cargo: &self.cargo,
                memo: &self.memo,
                reads: &self.reads,
            }
        }

        fn count(&self) -> eyre::Result<usize> {
            Ok(std::fs::read_to_string(self.root.join("reads"))?
                .lines()
                .count())
        }
    }

    /// A read of unchanged inputs reuses the earlier one, whether it returned a graph or a
    /// stale-lock refusal, and any changed input reads again.
    #[tokio::test]
    async fn identical_inputs_reuse_a_locked_read() -> eyre::Result<()> {
        let fixture = ReadFixture::new()?;
        let ReadFixture { root, project, .. } = &fixture;
        let reader = fixture.reader();
        let count = || fixture.count();

        // Unchanged inputs
        reader.graph(project).await?;
        reader.graph(project).await?;
        assert_eq!(count()?, 1);

        // A changed lock that Cargo refuses as stale, twice
        std::fs::write(root.join("Cargo.lock"), "version = 4\n\n# changed\n")?;
        std::fs::write(root.join("stale"), "")?;
        for _ in 0..2 {
            let read = reader.graph(project).await.err();
            assert!(matches!(read, Some(CoreError::StaleLock(_))), "{read:?}");
        }
        assert_eq!(count()?, 2);

        // Back to the first inputs, still kept
        std::fs::write(root.join("Cargo.lock"), "version = 4\n")?;
        reader.graph(project).await?;
        assert_eq!(count()?, 2);
        Ok(())
    }

    /// A path package that lost a target source fails Cargo's read, although no input of the key
    /// changed, so the earlier read is not reused.
    #[tokio::test]
    async fn a_removed_target_source_reads_again() -> eyre::Result<()> {
        let fixture = ReadFixture::new()?;
        let reader = fixture.reader();

        reader.graph(&fixture.project).await?;
        std::fs::remove_file(fixture.root.join("src/lib.rs"))?;
        reader.graph(&fixture.project).await?;

        assert_eq!(fixture.count()?, 2);
        Ok(())
    }

    /// A symbolic link to a directory is no target source to Cargo, but a real directory holding
    /// the same `main.rs` competes for the target's name, so replacing one with the other reads
    /// again.
    #[tokio::test]
    async fn a_symlink_replaced_by_a_directory_reads_again() -> eyre::Result<()> {
        let fixture = ReadFixture::new()?;
        let reader = fixture.reader();
        std::fs::create_dir_all(fixture.root.join("linked"))?;
        std::fs::write(fixture.root.join("linked/main.rs"), "")?;
        std::fs::create_dir_all(fixture.root.join("src/bin"))?;
        std::os::unix::fs::symlink(
            fixture.root.join("linked"),
            fixture.root.join("src/bin/foo"),
        )?;

        reader.graph(&fixture.project).await?;
        std::fs::remove_file(fixture.root.join("src/bin/foo"))?;
        std::fs::create_dir_all(fixture.root.join("src/bin/foo"))?;
        std::fs::write(fixture.root.join("src/bin/foo/main.rs"), "")?;
        reader.graph(&fixture.project).await?;

        assert_eq!(fixture.count()?, 2);
        Ok(())
    }

    /// A new file competing for an inferred target's source fails Cargo's read, so the earlier
    /// read is not reused although every earlier target source remains.
    #[tokio::test]
    async fn a_new_competing_target_source_reads_again() -> eyre::Result<()> {
        let fixture = ReadFixture::new()?;
        let reader = fixture.reader();

        reader.graph(&fixture.project).await?;
        std::fs::create_dir_all(fixture.root.join("src/bin/foo"))?;
        std::fs::write(fixture.root.join("src/bin/foo/main.rs"), "")?;
        reader.graph(&fixture.project).await?;

        assert_eq!(fixture.count()?, 2);
        Ok(())
    }

    /// A read during which the inputs changed is not kept for the inputs it started from, since
    /// Cargo may have read them in their new state.
    #[tokio::test]
    async fn a_read_racing_an_input_change_is_not_kept() -> eyre::Result<()> {
        let fixture = ReadFixture::new()?;
        let reader = fixture.reader();
        std::fs::write(fixture.root.join("race"), "")?;

        reader.graph(&fixture.project).await?;
        std::fs::remove_file(fixture.root.join("race"))?;
        std::fs::write(fixture.root.join("Cargo.lock"), "version = 4\n")?;
        reader.graph(&fixture.project).await?;

        assert_eq!(fixture.count()?, 2);
        Ok(())
    }
}
