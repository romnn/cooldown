//! The Gradle [`Tool`](cooldown_core::Tool): the resolved graph from `gradle.lockfile`, the direct
//! split from `build.gradle`, and Maven Central publish times under the Maven version model.
//!
//! Reading needs no Gradle run.
//! An apply relocks the project in a scratch copy of the whole build that owns it, with each
//! planned module constrained strictly to its target, has Gradle resolve the new lock there, and
//! publishes only the project's `gradle.lockfile`.
//! Gradle applies a lock entry as a strict constraint, so the lock alone moves a module and an
//! apply never edits a build script: a target the build's own requirement excludes is held.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::Hasher as _;

use crate::lock;
use crate::registry::{CENTRAL_URL, MAVEN_CENTRAL, MavenCentral};
use crate::tree::{files_below, slash_path, utf8};
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::{Driver, single_lock_journal, verify_current_unknown};
use cooldown_core::{
    ApplyAttempt, ApplyObserver, ApplyReport, CandidateScope, Capabilities, Change, CoreError,
    DepScope, Dependency, FetchContext, LockVerifyReport, NativePolicyLayer, PackageId,
    PackageRegistry, Plan, PreparedMutation, Project, ProjectMarker, ProjectMutationFile,
    ProjectMutationJournal, ProjectMutationState, Release, ReleaseFetcher, ReleaseOrder, Result,
    ToolId, ToolRead, ToolWrite, UpdateKind, VerifyReport, Version, failure_detail,
};
use cooldown_registry::SharedHttp;
use indoc::indoc;

/// The [`ToolId`] for the Gradle adapter (`"gradle"`).
pub const GRADLE_ID: ToolId = ToolId("gradle");

/// The lock Gradle's dependency locking writes, which is the project marker and the resolved graph.
const LOCKFILE: &str = "gradle.lockfile";

/// The build script whose dependency literals name the direct dependencies.
const MANIFEST: &str = "build.gradle";

/// The files that make a directory the root of a Gradle build.
const SETTINGS: [&str; 2] = ["settings.gradle", "settings.gradle.kts"];

/// The init script cooldown runs Gradle with, which selects the one project whose directory below
/// the settings directory is `cooldown.dir`, and adds root tasks that act on that project alone.
///
/// `:cooldownResolve` resolves every configuration `cooldown.configurations` lists, failing on any
/// one Gradle cannot resolve, and prints `cooldown-repository <url>` for every repository the
/// project resolves from.
/// Each `group:artifact:version=configuration;…` of `cooldown.targets` forces the module in those
/// configurations.
/// `:cooldownBuild` assembles the project.
/// The script prints `cooldown-refused <reason>` and fails where cooldown cannot vouch for the
/// result: no project or several live in `cooldown.dir`, Gradle routes repositories through a
/// Maven settings file's mirrors, or the project locks leniently.
const PROBE: &str = indoc! {r#"
    // buildSrc and included builds run init scripts too, and hold no project of this
    // invocation's own build.
    if (gradle.parent != null) {
        return
    }
    def property = { String name -> gradle.startParameter.projectProperties[name] ?: '' }
    def dir = property('cooldown.dir')
    def refuse = { String reason ->
        println "cooldown-refused $reason"
        throw new GradleException(reason)
    }
    def settingsRepositories = []
    def settingsDir = null
    gradle.settingsEvaluated { settings ->
        settingsRepositories = settings.dependencyResolutionManagement.repositories
        settingsDir = settings.settingsDir
    }
    // The settings, and the callbacks they register, may move any project, the root included, so
    // only the directory a project ends up in tells which one lives in `dir`.
    // The settings directory itself never moves, so the first settings callback can read it.
    def selects = { project -> project.projectDir == new File(settingsDir, dir) }
    // Gradle picks the tasks to run before it configures every project when it configures on
    // demand, so the root tasks exist from the start and name their project's tasks by path.
    gradle.rootProject { root ->
        def selected = root.allprojects.findAll(selects)
        if (selected.size() != 1) {
            refuse("${selected.size()} projects of the Gradle build live in ${dir ?: '.'}, where " +
                'cooldown needs exactly one')
        }
        def path = selected.first().path
        def task = { String name -> (path == ':' ? '' : path) + ":$name" }
        root.tasks.register('cooldownResolve') {
            dependsOn task('cooldownResolveProject')
        }
        root.tasks.register('cooldownBuild') {
            dependsOn task('assemble')
        }
    }
    // A resolution strategy, unlike a dependency, reaches no configuration that extends the one it
    // belongs to, and each is set as Gradle creates the configuration, before anything resolves it.
    def targets = property('cooldown.targets').tokenize(',').collect { it.tokenize('=') }
    gradle.beforeProject { project ->
        if (!selects(project)) {
            return
        }
        project.configurations.configureEach { configuration ->
            targets.each { target, names ->
                if (configuration.name in names.tokenize(';')) {
                    configuration.resolutionStrategy.force(target)
                }
            }
        }
        project.tasks.register('cooldownResolveProject') {
            doLast {
                def startParameter = gradle.startParameter
                if (startParameter.hasProperty('sharedMavenMirrorSettings')
                        && startParameter.sharedMavenMirrorSettings) {
                    refuse('Gradle routes repositories through the mirrors of a Maven settings ' +
                        'file (org.gradle.mirror.maven.settings), whose artifacts cooldown ' +
                        'cannot judge')
                }
                if (project.dependencyLocking.lockMode.getOrNull() == LockMode.LENIENT) {
                    refuse('the project locks its dependencies leniently, so Gradle does not ' +
                        'enforce the lock cooldown verifies')
                }
                property('cooldown.configurations').tokenize(',').each {
                    project.configurations.getByName(it).resolvedConfiguration.rethrowFailure()
                }
                (settingsRepositories + project.repositories).each { repository ->
                    def url = repository.hasProperty('url') ? repository.url : repository.name
                    println "cooldown-repository $url"
                }
            }
        }
    }
"#};

/// The Gradle implementation of the [`Tool`](cooldown_core::Tool) port.
pub struct GradleTool {
    registry: MavenCentral,
    driver: Driver,
}

/// Where a project sits in the Gradle build that owns it.
struct Build {
    /// The nearest directory at or above the project that holds a settings file.
    root: Utf8PathBuf,
    /// The project's directory below `root`, `/`-separated, and empty for a project in `root`.
    dir: String,
}

impl Build {
    /// The build that owns the project at `project_root`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::System`] when no directory at or above the project holds a settings
    /// file.
    fn locate(project_root: &Utf8Path) -> Result<Self> {
        let root = project_root
            .ancestors()
            .find(|dir| SETTINGS.iter().any(|name| dir.join(name).is_file()))
            .ok_or_else(|| {
                CoreError::System(
                    "no settings.gradle or settings.gradle.kts lies at or above the project, so \
                     cooldown cannot tell which Gradle build owns it"
                        .to_owned(),
                )
            })?;
        // `ancestors` starts at the project root, so every one of them is a prefix of it.
        let rel = project_root.strip_prefix(root).unwrap_or(Utf8Path::new(""));
        let parts: Vec<&str> = rel.components().map(|part| part.as_str()).collect();
        Ok(Build {
            root: root.to_owned(),
            dir: parts.join("/"),
        })
    }

    /// Every file of the build that a copy holds, for a preview and an apply alike.
    fn files(&self) -> Result<Vec<Utf8PathBuf>> {
        files_below(&self.root, &unstaged)
    }
}

impl GradleTool {
    /// Creates the adapter from a configured [`MavenCentral`] client.
    #[must_use]
    pub fn new(registry: MavenCentral) -> Self {
        GradleTool {
            registry,
            driver: Driver::new("gradle"),
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`MavenCentral`] client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        GradleTool::new(MavenCentral::new(http))
    }

    /// The launcher of the build rooted at `root`: the wrapper it ships, or `gradle` from `PATH`.
    fn launcher(&self, root: &Utf8Path) -> Driver {
        let wrapper = root.join(if cfg!(windows) {
            "gradlew.bat"
        } else {
            "gradlew"
        });
        if wrapper.is_file() {
            Driver::from_program(wrapper.as_str())
        } else {
            self.driver.clone()
        }
    }

    /// Relocks `project` to land `plan` and publishes the new lock through `owned`, the write set
    /// as it was prepared, which it leaves holding exactly what the apply wrote.
    async fn relock(
        &self,
        project: &Project,
        plan: &Plan,
        owned: &mut ProjectMutationState,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        if plan.changes.is_empty() {
            return Ok(report);
        }
        let lock_path = Utf8Path::new(LOCKFILE);
        // The publish below refuses to write once any file differs from `owned`, so this is the
        // lock the relock starts from.
        let before = owned
            .files()
            .iter()
            .find(|file| file.path() == lock_path)
            .and_then(ProjectMutationFile::contents)
            .and_then(|lock| std::str::from_utf8(lock).ok())
            .ok_or_else(|| {
                let path = project.root.join(lock_path);
                CoreError::LockUnreadable(format!("{path} is missing or not UTF-8"))
            })?
            .to_owned();
        let build = Build::locate(&project.root)?;
        let scratch = Scratch::stage(self, &build, project, &before)?;

        // Gradle relocks the planned modules onto their targets, each pinned in the configurations
        // that locked it at the version it moves from.
        let locked = locked_versions(&before);
        let names: BTreeSet<&str> = plan
            .changes
            .iter()
            .map(|change| change.package.name.as_str())
            .collect();
        let targets: Vec<String> = plan
            .changes
            .iter()
            .map(|change| {
                let name = change.package.name.as_str();
                let configurations: Vec<&str> = locked
                    .get(name)
                    .into_iter()
                    .flatten()
                    .filter(|(_, version)| version.as_str() == change.from.as_str())
                    .map(|(configuration, _)| configuration.as_str())
                    .collect();
                format!("{name}:{}={}", change.to, configurations.join(";"))
            })
            .collect();
        let output = scratch
            .run(&[
                "--update-locks".to_owned(),
                names.into_iter().collect::<Vec<_>>().join(","),
                format!("-Pcooldown.targets={}", targets.join(",")),
            ])
            .await?;
        if let Some(reason) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("cooldown-refused "))
        {
            return Err(CoreError::System(reason.to_owned()));
        }
        if !output.status.success() {
            return Err(CoreError::UnacceptableResolve(format!(
                "Gradle cannot relock the project: {}",
                what_went_wrong(&output)
            )));
        }

        let after = scratch.verify().await?;
        let collateral = lock_moves(&before, &after, &plan.changes)?;
        owned.write_owned(lock_path, after.as_bytes())?;
        report.applied.extend(plan.changes.iter().cloned());
        report.applied.extend(collateral);
        Ok(report)
    }
}

/// A copy of the build that owns a project, deleted when dropped, which Gradle runs [`PROBE`]
/// over.
struct Scratch {
    #[expect(dead_code, reason = "the field keeps the copied build alive")]
    dir: tempfile::TempDir,
    /// Where the build root sits in the copy.
    root: Utf8PathBuf,
    /// Where the project's lock sits in the copy.
    lock: Utf8PathBuf,
    /// A hash of each file of the copy as staged.
    staged: BTreeMap<Utf8PathBuf, u64>,
    gradle: Driver,
    /// The arguments every run of [`PROBE`] over the copy passes.
    probe: Vec<String>,
}

impl Scratch {
    /// Copies `build`, with `before` in place of the lock of `project`, and readies `tool`'s
    /// launcher to run [`PROBE`] over the copy.
    fn stage(tool: &GradleTool, build: &Build, project: &Project, before: &str) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let base = utf8(dir.path())?;
        // A root project named after its directory keeps its name in the copy.
        let root = base.join(build.root.file_name().unwrap_or("build"));
        let source_lock = project.root.join(LOCKFILE);
        for source in build.files()? {
            if source == source_lock {
                continue;
            }
            let rel = source
                .strip_prefix(&build.root)
                .map_err(|_| CoreError::System(format!("{source} lies outside {}", build.root)))?;
            let staged = root.join(rel);
            if let Some(parent) = staged.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&source, &staged)?;
        }
        std::fs::create_dir_all(root.join(&build.dir))?;
        let lock = root.join(&build.dir).join(LOCKFILE);
        std::fs::write(&lock, before)?;
        let script = base.join("cooldown.gradle");
        std::fs::write(&script, PROBE)?;
        let configurations: Vec<String> = lock::locked_configurations(before).into_iter().collect();
        Ok(Scratch {
            staged: fingerprint(&root)?,
            gradle: tool.launcher(&root),
            // The task reads the project's configurations as it runs, which the configuration
            // cache refuses.
            probe: vec![
                "-q".to_owned(),
                "--no-configuration-cache".to_owned(),
                "--init-script".to_owned(),
                script.to_string(),
                ":cooldownResolve".to_owned(),
                format!("-Pcooldown.dir={}", build.dir),
                format!("-Pcooldown.configurations={}", configurations.join(",")),
            ],
            dir,
            root,
            lock,
        })
    }

    /// Runs [`PROBE`] over the copy with `extra` arguments.
    async fn run(&self, extra: &[String]) -> Result<std::process::Output> {
        let mut args = self.probe.clone();
        args.extend_from_slice(extra);
        self.gradle.output_with_env(&self.root, &args, &[]).await
    }

    /// The lock the relock wrote, read before Gradle resolves it as it stands, from Maven Central
    /// alone, in a run that writes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::UnacceptableResolve`] when the relock wrote another file, which
    /// cooldown does not publish, Gradle rejects the lock, or the verifying run writes the copy,
    /// and [`CoreError::System`] when the project resolves from another repository.
    async fn verify(&self) -> Result<String> {
        let named = |paths: BTreeSet<&Utf8PathBuf>| {
            let names: Vec<String> = paths
                .into_iter()
                .map(|path| slash_path(path.strip_prefix(&self.root).unwrap_or(path)))
                .collect();
            names.join(", ")
        };
        let written = fingerprint(&self.root)?;
        let mut unpublished = differing(&self.staged, &written);
        unpublished.remove(&self.lock);
        if !unpublished.is_empty() {
            return Err(CoreError::UnacceptableResolve(format!(
                "Gradle's relock also writes {}, which cooldown does not publish",
                named(unpublished)
            )));
        }
        let lock = std::fs::read_to_string(&self.lock)?;
        let output = self.run(&[]).await?;
        if !output.status.success() {
            return Err(CoreError::UnacceptableResolve(format!(
                "Gradle rejects the relocked {LOCKFILE}: {}",
                what_went_wrong(&output)
            )));
        }
        let verified = fingerprint(&self.root)?;
        let rewritten = differing(&written, &verified);
        if !rewritten.is_empty() {
            return Err(CoreError::UnacceptableResolve(format!(
                "the run that verifies the relocked lock writes {}",
                named(rewritten)
            )));
        }
        let foreign = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix("cooldown-repository "))
            .any(|url| url.trim_end_matches('/') != CENTRAL_URL);
        if foreign {
            return Err(CoreError::System(
                "the project resolves from a repository other than Maven Central, whose artifacts \
                 cooldown cannot judge"
                    .to_owned(),
            ));
        }
        Ok(lock)
    }
}

/// Whether a copy leaves the directory `dir` out: version control metadata, Gradle's caches, and
/// the build output beside a build or settings script, unless it holds a build script and is a
/// project itself.
fn unstaged(dir: &Utf8Path) -> bool {
    match dir.file_name() {
        Some(".git" | ".hg" | ".svn" | ".jj" | ".gradle") => true,
        Some("build") => {
            let holds = |dir: &Utf8Path, scripts: &[&str]| {
                scripts.iter().any(|script| dir.join(script).is_file())
            };
            let build_scripts = [MANIFEST, "build.gradle.kts"];
            !holds(dir, &build_scripts)
                && dir
                    .parent()
                    .is_some_and(|parent| holds(parent, &build_scripts) || holds(parent, &SETTINGS))
        }
        _ => false,
    }
}

/// Gradle's own account of a failed run, the section it heads `* What went wrong:`, or all it
/// printed when it has none.
fn what_went_wrong(output: &std::process::Output) -> String {
    let printed = failure_detail(output);
    match printed.split_once("* What went wrong:") {
        Some((_, rest)) => rest.split("\n* ").next().unwrap_or(rest).trim().to_owned(),
        None => printed,
    }
}

/// The paths that two fingerprints of a copy hold differently.
fn differing<'a>(
    before: &'a BTreeMap<Utf8PathBuf, u64>,
    after: &'a BTreeMap<Utf8PathBuf, u64>,
) -> BTreeSet<&'a Utf8PathBuf> {
    before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .collect()
}

/// A hash of each file below `root` but Gradle's caches and build output, by path.
fn fingerprint(root: &Utf8Path) -> Result<BTreeMap<Utf8PathBuf, u64>> {
    files_below(root, &unstaged)?
        .into_iter()
        .map(|path| {
            let mut hasher = std::hash::DefaultHasher::new();
            hasher.write(&std::fs::read(&path)?);
            Ok((path, hasher.finish()))
        })
        .collect()
}

/// The version each configuration of a `gradle.lockfile` locks each module at, by module.
fn locked_versions(lock: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut versions: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for pin in lock::parse_gradle_lock(lock) {
        let locked = versions.entry(pin.name).or_default();
        for configuration in pin.configurations {
            locked.insert(configuration, pin.version.clone());
        }
    }
    versions
}

/// The collateral rows of a relock from the lock `before` to the lock `after` that was asked to
/// land `planned`, so that no version moves in silence.
///
/// # Errors
///
/// Returns [`CoreError::UnacceptableResolve`] unless every planned module sits at exactly its
/// target in each configuration that locked it at its old version.
fn lock_moves(before: &str, after: &str, planned: &[Change]) -> Result<Vec<Change>> {
    let (before, after) = (locked_versions(before), locked_versions(after));
    let mut collateral: Vec<Change> = Vec::new();
    for (name, locked) in &before {
        for (configuration, from) in locked {
            let to = after.get(name).and_then(|locked| locked.get(configuration));
            if let Some(change) = planned
                .iter()
                .find(|change| change.package.name == *name && change.from.as_str() == from)
            {
                if to.map(String::as_str) != Some(change.to.as_str()) {
                    return Err(CoreError::UnacceptableResolve(format!(
                        "Gradle does not lock {name} {} in {configuration}",
                        change.to
                    )));
                }
                continue;
            }
            let Some(to) = to else {
                continue;
            };
            let reported = collateral.iter().any(|row| {
                row.package.name == *name && row.from.as_str() == from && row.to.as_str() == to
            });
            if to == from || reported {
                continue;
            }
            collateral.push(Change {
                package: PackageId::new(GRADLE_ID, name.clone(), Some(MAVEN_CENTRAL.to_string())),
                kind: version::classify_kind(from, to).unwrap_or(UpdateKind::Minor),
                downgrade: version::compare(to, from).is_lt(),
                from: Version::new(from.clone()),
                to: Version::new(to.clone()),
                direct: false,
                members: Vec::new(),
            });
        }
    }
    Ok(collateral)
}

#[async_trait]
impl ToolRead for GradleTool {
    fn version_command(&self) -> Option<cooldown_core::trace::ToolVersionCommand> {
        Some(cooldown_core::trace::ToolVersionCommand {
            program: self.driver.program().to_string(),
            arguments: vec!["--version".into()],
        })
    }

    fn evidence(&self, project: &Project) -> Result<cooldown_core::trace::Evidence> {
        use cooldown_core::trace::GraphCoverage;
        let mut evidence = cooldown_adapter_util::evidence::native(
            project,
            LOCKFILE,
            GraphCoverage::Inventory {
                reason: "native declarations or pins do not contain the full resolved graph".into(),
            },
            &[
                "settings.gradle",
                "settings.gradle.kts",
                "build.gradle",
                "build.gradle.kts",
                "gradle.properties",
            ],
        );
        cooldown_adapter_util::evidence::environment(
            &mut evidence,
            &["JAVA_HOME", "GRADLE_USER_HOME", "GRADLE_OPTS"],
        );
        Ok(evidence)
    }

    fn id(&self) -> ToolId {
        GRADLE_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: true,
            artifact_granular: false,
            advisory_ecosystem: Some("Maven"),
            honors_generated_members: false,
        }
    }

    fn project_marker(&self) -> cooldown_core::ProjectMarker {
        ProjectMarker {
            marker: LOCKFILE,
            manifest: MANIFEST,
            alternate_manifests: &[],
            workspace_root: false,
        }
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        version::classify_kind(from, to)
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let content = std::fs::read_to_string(project.root.join(LOCKFILE))?;
        let manifest = std::fs::read_to_string(&project.manifest).unwrap_or_default();
        let direct = lock::parse_gradle_direct(&manifest);
        let mut deps = Vec::new();
        for pin in lock::parse_gradle_lock(&content) {
            let is_direct = direct.contains(&pin.name);
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            // No advisory identity, ever: the lock records no per-artifact repository, and the
            // routing that decides real origin lives outside the files this adapter reads —
            // `settings.gradle` `dependencyResolutionManagement` blocks and init scripts.
            // Origin evidence that cannot be positive must decline: OSV's `Maven` advisories
            // describe Maven Central artifacts, and a same-named coordinate from a private
            // repository must not inherit their shortened windows.
            // Declining only fails to shorten — the ordinary window stands, and the app layer
            // says so once per run.
            let advisory_identity = None;
            deps.push(Dependency {
                package: PackageId::new(GRADLE_ID, pin.name, Some(MAVEN_CENTRAL.to_string())),
                advisory_identity,
                current: Version::new(pin.version.clone()),
                current_quality: version::classify_quality(&pin.version),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                graph_ceiling: None,
                declared_bound: None,
                members: Vec::new(),
                pinned: false,
                hold_edges: Vec::new(),
            });
        }
        Ok(deps)
    }

    async fn native_policy(&self, _project: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }

    async fn verify_lock_current(&self, _project: &Project) -> Result<LockVerifyReport> {
        Ok(verify_current_unknown(LOCKFILE))
    }
}

#[async_trait]
impl ReleaseFetcher for GradleTool {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.registry.releases(&dep.package).await?;
        Ok(version::build_releases(dep.current.as_str(), raw))
    }

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
        let time = self
            .registry
            .published_at(&dep.package, &dep.current, &[])
            .await?;
        Ok(Release {
            version: dep.current.clone(),
            order: ReleaseOrder(Vec::new()),
            major: version::major_key(dep.current.as_str()),
            major_number: version::major_number(dep.current.as_str()),
            kind_from_current: None,
            beyond_declared_bound: false,
            beyond_latest_tag: false,
            published_at: time,
            yanked: false,
            quality: dep.current_quality,
        })
    }
}

#[async_trait]
impl ToolWrite for GradleTool {
    fn mutation_tool(&self) -> ToolId {
        GRADLE_ID
    }

    // A preview copies what an apply's scratch copy holds, so Gradle reads the same build in
    // both.
    fn external_resolve_roots(&self, project: &Project) -> Result<Vec<Utf8PathBuf>> {
        Build::locate(&project.root)?.files()
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        single_lock_journal(&project.root, Utf8Path::new(LOCKFILE))
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        let (project, plan, journal) = mutation.parts_for(self)?;
        self.relock(project, plan, &mut journal.state_for(journal)?)
            .await
    }

    async fn apply_with_observer(
        &self,
        mutation: &PreparedMutation,
        _observer: &dyn ApplyObserver,
    ) -> Result<ApplyAttempt> {
        let (project, plan, journal) = mutation.parts_for(self)?;
        // The apply starts from the prepared state and hands back that state, advanced only by its
        // own write, never a capture of the disk, so a lock someone else changed after preparing
        // is neither published over nor taken for cooldown's own, and a rollback refuses to
        // overwrite it.
        let mut owned = journal.state_for(journal)?;
        let report = self.relock(project, plan, &mut owned).await;
        mutation.finished_attempt(report, &owned)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        // `assemble` compiles and packages the project, which resolving its graph never does.
        let build = Build::locate(&project.root)?;
        let scratch = tempfile::tempdir()?;
        let script = utf8(scratch.path())?.join("cooldown.gradle");
        std::fs::write(&script, PROBE)?;
        let args = [
            "-q".to_owned(),
            "--no-configuration-cache".to_owned(),
            "--init-script".to_owned(),
            script.to_string(),
            ":cooldownBuild".to_owned(),
            format!("-Pcooldown.dir={}", build.dir),
        ];
        self.launcher(&build.root)
            .verify(&build.root, &args, "gradle assemble succeeded")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::formatdoc;

    #[test]
    fn advisory_ecosystem_matches_osv() {
        let cache = tempfile::tempdir().expect("cache");
        let http =
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http");
        assert_eq!(
            GradleTool::from_http(http)
                .capabilities()
                .advisory_ecosystem,
            Some("Maven")
        );
    }

    /// A copy keeps the name of the build's root, which a root project named after its directory
    /// takes, and leaves a `build` directory out only beside a build or settings script, where it
    /// is Gradle's output, unless it is a project of its own.
    #[test]
    fn a_copy_keeps_the_root_name_and_leaves_out_only_build_output() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = utf8(dir.path())?.join("demo-root");
        for file in [
            "settings.gradle",
            "gradle.lockfile",
            "build/classes/Demo.class",
            "app/build.gradle",
            "app/build/libs/app.jar",
            "src/build/Kept.java",
            "lib/build.gradle",
            "lib/build/build.gradle",
            ".gradle/cache",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap_or(&root))?;
            std::fs::write(path, "")?;
        }
        let cache = tempfile::tempdir()?;
        let http =
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http");
        let project = Project {
            root: root.clone(),
            kind: GRADLE_ID,
            manifest: root.join(MANIFEST),
            exclude_newer: None,
            exclude_newer_package: BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };

        let scratch = Scratch::stage(
            &GradleTool::from_http(http),
            &Build::locate(&root)?,
            &project,
            "empty=\n",
        )?;

        assert_eq!(scratch.root.file_name(), Some("demo-root"));
        let staged: Vec<String> = files_below(&scratch.root, &|_| false)?
            .iter()
            .map(|path| slash_path(path.strip_prefix(&scratch.root).unwrap_or(path)))
            .collect();
        assert_eq!(
            staged,
            [
                "app/build.gradle",
                "gradle.lockfile",
                "lib/build/build.gradle",
                "lib/build.gradle",
                "settings.gradle",
                "src/build/Kept.java"
            ]
        );
        Ok(())
    }

    /// A planned module that reaches its target in one configuration but keeps its old version
    /// in another has not landed, so the relock is refused rather than published half done.
    #[test]
    fn a_target_locked_in_only_some_configurations_is_refused() {
        let text = "org.apache.commons:commons-text";
        let change = Change {
            package: PackageId::new(GRADLE_ID, text, Some(MAVEN_CENTRAL.to_string())),
            from: Version::new("1.10.0"),
            to: Version::new("1.12.0"),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        };
        let before = formatdoc! {"
            {text}:1.10.0=compileClasspath,runtimeClasspath
            empty=
        "};
        let after = |runtime: &str| {
            formatdoc! {"
                {text}:1.12.0=compileClasspath
                {text}:{runtime}=runtimeClasspath
                empty=
            "}
        };

        let landed = lock_moves(&before, &after("1.12.0"), std::slice::from_ref(&change));
        let partial = lock_moves(&before, &after("1.10.0"), std::slice::from_ref(&change));

        assert!(matches!(landed.as_deref(), Ok([])), "{landed:?}");
        assert!(
            matches!(&partial, Err(CoreError::UnacceptableResolve(detail))
                if detail.contains("runtimeClasspath")),
            "{partial:?}"
        );
    }
}
