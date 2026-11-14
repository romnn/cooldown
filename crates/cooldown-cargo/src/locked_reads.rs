//! Run-scoped reuse of `cargo metadata --locked` reads.
//!
//! A locked read never writes the lock, and its outcome depends on the native inputs the
//! [rejection memo](crate::rejection_memo) captures, given the stable index the memo assumes for
//! one run, and on the target files Cargo discovers in path packages.
//! The key does not capture those target files, so a read is reused only in a staged copy, under
//! the [rule](crate::command_memo::StagedTrees::reusable_inputs) command replay follows, where they
//! stay as staged.
//! Two reads of the same staged root with the same inputs therefore return the same graph or the
//! same stale-lock refusal, and an upgrade's trials reread identical files many times over.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, Project};

use crate::cargocmd::{Cargo, LOCKED_METADATA_ARGS, ResolvedGraph};
use crate::command_memo::{StagedInputs, StagedTrees};
use crate::rejection_memo::{LockSource, MemoOperation, RejectionMemo};

/// The recent locked reads of the run, newest last.
#[derive(Debug, Default)]
pub(crate) struct LockedReads {
    reads: Mutex<VecDeque<LockedRead>>,
}

#[derive(Debug)]
struct LockedRead {
    /// The directory Cargo ran in, which the graph's paths name.
    root: Utf8PathBuf,
    inputs: StagedInputs,
    outcome: ReadOutcome,
}

#[derive(Debug, Clone)]
enum ReadOutcome {
    /// Cargo's metadata output.
    Graph(Arc<str>),
    /// The detail of Cargo's refusal to read a lock that needs updating.
    Stale(String),
}

impl LockedReads {
    /// The reads kept; each holds one metadata output of the workspace.
    const CAPACITY: usize = 16;

    fn get(&self, root: &Utf8Path, inputs: &StagedInputs) -> Option<ReadOutcome> {
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find(|read| read.root == root && &read.inputs == inputs)
            .map(|read| read.outcome.clone())
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
    /// The run's staged copies, the only places reads are reused in.
    pub(crate) trees: &'a StagedTrees,
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
                ReadOutcome::Graph(stdout) => {
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
                Ok(stdout) => Some(ReadOutcome::Graph(Arc::from(stdout.as_str()))),
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

    /// Keeps `stdout` as the locked read of `project`'s current `inputs`, keyed for a
    /// [`MemoOperation::LockedRead`] by the gate of a command in the same staged copy.
    ///
    /// The caller vouches that a locked read of these inputs would succeed with this output now.
    pub(crate) fn keep_settled(&self, project: &Project, inputs: StagedInputs, stdout: &str) {
        self.reads.insert(LockedRead {
            root: project.root.clone(),
            inputs,
            outcome: ReadOutcome::Graph(Arc::from(stdout)),
        });
    }

    /// The inputs of a locked read of `project`, or `None` when its outcome is not reusable.
    fn inputs(&self, project: &Project) -> Option<StagedInputs> {
        let command = self
            .cargo
            .command_line(&project.root, &LOCKED_METADATA_ARGS)
            .ok()?;
        self.trees
            .reusable_inputs(self.memo, project, &command, LockSource::Disk)
            .map(|inputs| inputs.with_operation(MemoOperation::LockedRead))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use indoc::{formatdoc, indoc};
    use std::os::unix::fs::PermissionsExt as _;

    /// The fake `cargo`, run in `<fixture>/project`: a locked read logs itself to
    /// `<fixture>/reads` and is refused as stale while `<fixture>/stale` exists, and otherwise
    /// prints the member `app` with its library target.
    ///
    /// With `<fixture>/race`, the read changes the lock while it runs.
    const SCRIPT: &str = indoc! {r#"
        #!/bin/sh
        printf '%s\n' "$*" >> ../reads
        if [ -f ../race ]; then printf '# raced\n' >> Cargo.lock; fi
        if [ -f ../stale ]; then
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

    /// The manifest of a path package `dep`.
    const DEPENDENCY: &str = indoc! {r#"
        [package]
        name = "dep"
        version = "0.1.0"
    "#};

    struct ReadFixture {
        /// The canonical temporary directory holding the project, the fake `cargo` and its log.
        fixture: Utf8PathBuf,
        project: Project,
        cargo: Cargo,
        memo: RejectionMemo,
        trees: StagedTrees,
        reads: LockedReads,
        #[expect(dead_code, reason = "dropping the guard deletes the fixture directory")]
        directory: tempfile::TempDir,
    }

    /// A canonical temporary directory, as staging registers its copies (a temporary directory is
    /// a symbolic link on macOS).
    fn canonical_tempdir() -> eyre::Result<(tempfile::TempDir, Utf8PathBuf)> {
        let directory = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(std::fs::canonicalize(directory.path())?)
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        Ok((directory, path))
    }

    impl ReadFixture {
        /// A project in its source root, which no staged copy registered.
        fn source_root() -> eyre::Result<Self> {
            let (directory, fixture) = canonical_tempdir()?;
            let root = fixture.join("project");
            std::fs::create_dir_all(root.join("src"))?;
            std::fs::write(root.join("Cargo.toml"), MANIFEST)?;
            std::fs::write(root.join("Cargo.lock"), "version = 4\n")?;
            std::fs::write(root.join("src/lib.rs"), "")?;
            let fake = fixture.join("fake-cargo");
            std::fs::write(&fake, SCRIPT)?;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
            let project = Project {
                manifest: root.join("Cargo.toml"),
                root,
                kind: crate::CARGO_ID,
                exclude_newer: None,
                exclude_newer_package: std::collections::BTreeMap::new(),
                generated_members: cooldown_core::GeneratedMembers::undeclared(),
            };
            Ok(Self {
                cargo: Cargo::with_bin(fake.as_str()),
                fixture,
                project,
                memo: RejectionMemo::default(),
                trees: StagedTrees::default(),
                reads: LockedReads::default(),
                directory,
            })
        }

        /// A project registered as the staged copy of another directory, with the whole fixture
        /// directory as its private tree.
        fn staged() -> eyre::Result<Self> {
            let fixture = Self::source_root()?;
            let original = fixture.fixture.join("original");
            std::fs::create_dir_all(&original)?;
            fixture.stage(&fixture.fixture, &original)?;
            Ok(fixture)
        }

        /// Registers the project as the staged copy of `original` living in `tree`.
        fn stage(&self, tree: &Utf8Path, original: &Utf8Path) -> eyre::Result<()> {
            self.memo.register_origin(&self.project.root, original)?;
            self.trees.register(&self.project.root, tree);
            Ok(())
        }

        fn reader(&self) -> LockedReader<'_> {
            LockedReader {
                cargo: &self.cargo,
                memo: &self.memo,
                trees: &self.trees,
                reads: &self.reads,
            }
        }

        async fn read(&self) -> Result<ResolvedGraph, CoreError> {
            self.reader().graph(&self.project).await
        }

        fn path(&self, relative: &str) -> Utf8PathBuf {
            self.project.root.join(relative)
        }

        fn mark(&self, name: &str) -> eyre::Result<()> {
            Ok(std::fs::write(self.fixture.join(name), "")?)
        }

        fn count(&self) -> eyre::Result<usize> {
            Ok(std::fs::read_to_string(self.fixture.join("reads"))?
                .lines()
                .count())
        }
    }

    /// A read of a staged copy with unchanged inputs reuses the earlier one, whether it returned
    /// a graph or a stale-lock refusal, and any changed input reads again.
    #[tokio::test]
    async fn identical_inputs_in_a_staged_copy_reuse_a_locked_read() -> eyre::Result<()> {
        let fixture = ReadFixture::staged()?;
        let lock = fixture.path("Cargo.lock");

        // Unchanged inputs
        fixture.read().await?;
        fixture.read().await?;
        assert_eq!(fixture.count()?, 1);

        // A changed lock that Cargo refuses as stale, twice
        std::fs::write(&lock, "version = 4\n\n# changed\n")?;
        fixture.mark("stale")?;
        for _ in 0..2 {
            let read = fixture.read().await.err();
            assert!(matches!(read, Some(CoreError::StaleLock(_))), "{read:?}");
        }
        assert_eq!(fixture.count()?, 2);

        // Back to the first inputs, still kept
        std::fs::write(&lock, "version = 4\n")?;
        fixture.read().await?;
        assert_eq!(fixture.count()?, 2);
        Ok(())
    }

    /// A change in a source root that alters what Cargo reports without touching the lock, a
    /// manifest or the configuration.
    struct SourceChange {
        name: &'static str,
        setup: fn(&ReadFixture) -> eyre::Result<()>,
        change: fn(&ReadFixture) -> eyre::Result<()>,
        /// Whether Cargo refuses the lock as stale before the change.
        stale: bool,
    }

    /// Cargo discovers a source root's targets from files the input key does not capture, so every
    /// locked read of a source root runs Cargo instead of serving an earlier read stale.
    #[tokio::test]
    async fn source_root_target_changes_read_again() -> eyre::Result<()> {
        let changes = [
            SourceChange {
                name: "a bin-only path dependency gains a library",
                setup: |fixture| {
                    let dependency = fixture.fixture.join("dep");
                    std::fs::create_dir_all(dependency.join("src"))?;
                    std::fs::write(dependency.join("src/main.rs"), "")?;
                    std::fs::write(dependency.join("Cargo.toml"), DEPENDENCY)?;
                    let manifest = formatdoc! {r#"
                        {MANIFEST}
                        [dependencies]
                        dep = {{ path = "../dep" }}
                    "#};
                    Ok(std::fs::write(&fixture.project.manifest, manifest)?)
                },
                change: |fixture| Ok(std::fs::write(fixture.fixture.join("dep/src/lib.rs"), "")?),
                stale: false,
            },
            SourceChange {
                name: "an edition-2015 legacy target file is removed",
                setup: |fixture| {
                    std::fs::write(fixture.path("src/tool.rs"), "")?;
                    let manifest = indoc! {r#"
                        [package]
                        name = "app"
                        version = "0.1.0"
                        edition = "2015"

                        [[bin]]
                        name = "tool"
                    "#};
                    Ok(std::fs::write(&fixture.project.manifest, manifest)?)
                },
                change: |fixture| Ok(std::fs::remove_file(fixture.path("src/tool.rs"))?),
                stale: false,
            },
            SourceChange {
                name: "a symlinked target source's referent is deleted",
                setup: |fixture| {
                    std::fs::write(fixture.fixture.join("referent.rs"), "")?;
                    Ok(std::os::unix::fs::symlink(
                        fixture.fixture.join("referent.rs"),
                        fixture.path("src/main.rs"),
                    )?)
                },
                change: |fixture| Ok(std::fs::remove_file(fixture.fixture.join("referent.rs"))?),
                stale: false,
            },
            SourceChange {
                name: "build.rs becomes a directory",
                setup: |fixture| Ok(std::fs::write(fixture.path("build.rs"), "")?),
                change: |fixture| {
                    std::fs::remove_file(fixture.path("build.rs"))?;
                    Ok(std::fs::create_dir(fixture.path("build.rs"))?)
                },
                stale: false,
            },
            SourceChange {
                name: "a target source is removed after a stale-lock refusal",
                setup: |fixture| fixture.mark("stale"),
                change: |fixture| Ok(std::fs::remove_file(fixture.path("src/lib.rs"))?),
                stale: true,
            },
        ];
        for SourceChange {
            name,
            setup,
            change,
            stale,
        } in changes
        {
            let fixture = ReadFixture::source_root()?;
            setup(&fixture)?;
            for step in ["before", "after"] {
                // Each read reports what Cargo does: the graph, or the stale-lock refusal.
                let failure = fixture.read().await.err();
                let expected = if stale {
                    matches!(failure, Some(CoreError::StaleLock(_)))
                } else {
                    failure.is_none()
                };
                assert!(expected, "{name}, {step} the change: {failure:?}");
                if step == "before" {
                    change(&fixture)?;
                }
            }
            // Cargo ran for the read after the change as well.
            assert_eq!(fixture.count()?, 2, "{name}");
        }
        Ok(())
    }

    /// A staged copy registered again, as a new stage reusing a removed copy's path is, reuses no
    /// read of the earlier registration, whose copy may have held other target files.
    #[tokio::test]
    async fn a_reregistered_staged_copy_reads_again() -> eyre::Result<()> {
        let fixture = ReadFixture::staged()?;

        fixture.read().await?;
        fixture
            .trees
            .register(&fixture.project.root, &fixture.fixture);
        fixture.read().await?;
        fixture.read().await?;

        // The first read of the new registration ran, and the second reused it.
        assert_eq!(fixture.count()?, 2);
        Ok(())
    }

    /// Cargo configuration that appears above a staged copy after staging applies to Cargo there,
    /// although the input key, which captures the original project's configuration, misses it.
    #[tokio::test]
    async fn configuration_above_the_staged_copy_reads_again() -> eyre::Result<()> {
        let fixture = ReadFixture::source_root()?;
        let (_original_directory, original) = canonical_tempdir()?;
        let tree = fixture.project.root.clone();
        fixture.stage(&tree, &original)?;

        fixture.read().await?;
        std::fs::create_dir_all(fixture.fixture.join(".cargo"))?;
        std::fs::write(fixture.fixture.join(".cargo/config.toml"), "[net]\n")?;
        fixture.read().await?;
        fixture.read().await?;

        assert_eq!(fixture.count()?, 3);
        Ok(())
    }

    /// A staged project with a path package outside its copy reads again, since anything may
    /// change the target files Cargo discovers there.
    #[tokio::test]
    async fn a_package_outside_the_staged_copy_reads_again() -> eyre::Result<()> {
        let fixture = ReadFixture::staged()?;
        let (_outside_directory, outside) = canonical_tempdir()?;
        std::fs::create_dir_all(outside.join("src"))?;
        std::fs::write(outside.join("src/lib.rs"), "")?;
        std::fs::write(outside.join("Cargo.toml"), DEPENDENCY)?;
        // The path is serialized, so it stays a valid TOML string whatever characters it holds.
        let path = toml::Value::String(outside.to_string());
        let manifest = formatdoc! {"
            {MANIFEST}
            [dependencies]
            dep = {{ path = {path} }}
        "};
        std::fs::write(&fixture.project.manifest, manifest)?;

        fixture.read().await?;
        fixture.read().await?;

        assert_eq!(fixture.count()?, 2);
        Ok(())
    }

    /// A read during which the inputs changed is not kept for the inputs it started from, since
    /// Cargo may have read them in their new state.
    #[tokio::test]
    async fn a_read_racing_an_input_change_is_not_kept() -> eyre::Result<()> {
        let fixture = ReadFixture::staged()?;
        fixture.mark("race")?;

        fixture.read().await?;
        std::fs::remove_file(fixture.fixture.join("race"))?;
        std::fs::write(fixture.path("Cargo.lock"), "version = 4\n")?;
        fixture.read().await?;

        assert_eq!(fixture.count()?, 2);
        Ok(())
    }
}
