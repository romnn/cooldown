//! The Maven [`Tool`](cooldown_core::Tool): one project per reactor, the direct dependencies of
//! every member read from the POMs as Maven builds their models, and Maven Central publish times.
//!
//! Reading needs no Maven run.
//! An apply replaces exactly the text of each version element it plans to move in the prepared
//! POM bytes, verifies the edited reactor with Maven's own resolution in a scratch copy, and
//! publishes only the POMs Maven confirmed.
//! Whatever cooldown reads wrongly therefore becomes a refusal, never a published edit Maven
//! resolves differently.

mod land;
mod pom;
mod reactor;
mod verify;

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::{Driver, verify_current_unknown};
use cooldown_core::{
    ApplyAttempt, ApplyObserver, ApplyReport, CandidateScope, Capabilities, Change, CoreError,
    DepScope, Dependency, Diagnostic, DiagnosticKind, FetchContext, LockVerifyReport, MemberRef,
    NativePolicyLayer, NestedOwnership, PackageId, PackageRegistry, Plan, PreparedMutation,
    Project, ProjectMarker, ProjectMutationJournal, ProjectMutationState, Release, ReleaseFetcher,
    ReleaseOrder, Result, SkipReason, Skipped, ToolId, ToolRead, ToolWrite, UpdateKind,
    VerifyReport, Version,
};
use cooldown_registry::SharedHttp;

use crate::registry::{MAVEN_CENTRAL, MavenCentral};
use crate::version;
use land::Edit;
use reactor::{Reactor, Resolution};
use verify::{Run, Scratch};

/// The [`ToolId`] for the Maven adapter (`"maven"`).
pub const MAVEN_ID: ToolId = ToolId("maven");

/// The Maven implementation of the [`Tool`](cooldown_core::Tool) port.
pub struct MavenTool {
    registry: MavenCentral,
    mvn: Driver,
}

impl MavenTool {
    /// Creates the adapter from a configured [`MavenCentral`] client.
    #[must_use]
    pub fn new(registry: MavenCentral) -> Self {
        MavenTool {
            registry,
            mvn: Driver::new("mvn"),
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`MavenCentral`] client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        MavenTool::new(MavenCentral::new(http))
    }

    /// Lands `plan` on the reactor of `project` and publishes the edited POMs through `owned`, the
    /// write set as it was prepared, which it leaves holding exactly what the apply wrote.
    async fn land(
        &self,
        project: &Project,
        plan: &Plan,
        owned: &mut ProjectMutationState,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        if plan.changes.is_empty() {
            return Ok(report);
        }
        if let Some(option) = reactor::narrowing_option(&project.root) {
            return Err(CoreError::System(format!(
                "Maven's launcher arguments pass {option}, which narrows or redirects the \
                 reactor, so cooldown cannot verify an edit against every project Maven builds"
            )));
        }

        // The reactor as prepared, which the publish below refuses to write over once any file
        // differs from it
        let prepared = owned
            .files()
            .iter()
            .map(|file| {
                let text = file
                    .contents()
                    .and_then(|contents| std::str::from_utf8(contents).ok())
                    .ok_or_else(|| {
                        CoreError::LockUnreadable(format!(
                            "{} is missing or not UTF-8",
                            file.path()
                        ))
                    })?;
                Ok((file.path().to_owned(), text.to_owned()))
            })
            .collect::<Result<BTreeMap<Utf8PathBuf, String>>>()?;
        let reactor = Reactor::read(&project.root, &prepared)?;
        let reactor_paths: BTreeSet<&Utf8Path> = reactor.owned_paths().collect();
        if reactor_paths != prepared.keys().map(Utf8PathBuf::as_path).collect() {
            return Err(CoreError::LockConflict(format!(
                "the modules of the reactor at {} changed after cooldown prepared it",
                project.root
            )));
        }

        // The elements each change edits, or why it cannot move
        let entries = reactor.entries();
        let mut landings: Vec<(&Change, Vec<Edit>)> = Vec::new();
        for change in &plan.changes {
            match land::edits_for(change, &entries, &plan.excluded_members) {
                Ok(edits) => landings.push((change, edits)),
                Err(detail) => report.skipped.push(not_eligible(change, detail)),
            }
        }
        if landings.is_empty() {
            return Ok(report);
        }

        // Maven's reading of the reactor as it stands
        let externals: Vec<&Utf8Path> = reactor.external_paths().collect();
        let scratch = Scratch::stage(
            &project.root,
            prepared
                .iter()
                .map(|(path, text)| (path.as_path(), text.as_str())),
            &externals,
        )?;
        let before = scratch.verdict(Run::Before, &self.mvn).await?;
        landings.retain(|(change, _)| match land::refusal(change, &before) {
            Some(detail) => {
                report.skipped.push(not_eligible(change, detail));
                false
            }
            None => true,
        });
        if landings.is_empty() {
            return Ok(report);
        }

        // Maven's reading of the edited reactor
        let edited = land::edited(&reactor, &landings)?;
        for (path, text) in &edited {
            scratch.write(path, text)?;
        }
        let after = scratch.verdict(Run::After, &self.mvn).await?;
        if after.central_mirrored {
            return Err(CoreError::System(
                "cooldown cannot judge artifacts that a mirror serves in place of Maven Central"
                    .to_owned(),
            ));
        }
        let landed: Vec<&Change> = landings.iter().map(|(change, _)| *change).collect();
        land::check_moves(&before, &after, &landed)?;

        // Publication of exactly the POMs Maven verified
        for (path, text) in &edited {
            owned.write_owned(path, text.as_bytes())?;
        }
        report.applied.extend(landed.into_iter().cloned());
        Ok(report)
    }
}

/// The hold of a `change` cooldown cannot land, for the reason `detail` gives.
fn not_eligible(change: &Change, detail: String) -> Skipped {
    Skipped {
        change: change.clone(),
        reason: SkipReason::NotEligible,
        offending: Some(change.package.clone()),
        detail: Some(detail),
    }
}

#[async_trait]
impl ToolRead for MavenTool {
    fn version_command(&self) -> Option<cooldown_core::trace::ToolVersionCommand> {
        Some(cooldown_core::trace::ToolVersionCommand {
            program: self.mvn.program().to_string(),
            arguments: vec!["--version".into()],
        })
    }

    fn evidence(&self, project: &Project) -> Result<cooldown_core::trace::Evidence> {
        use cooldown_core::trace::{EvidenceInput, GraphCoverage, InputRole};
        let mut evidence = cooldown_adapter_util::evidence::native(
            project,
            "pom.xml",
            GraphCoverage::Inventory {
                reason: "POMs declare direct dependencies, not the graph Maven resolves".into(),
            },
            &[
                ".mvn/maven.config",
                ".mvn/jvm.config",
                ".mvn/extensions.xml",
            ],
        );
        // Every module's POM and every local parent's is as much an input as the root's.
        let reactor = Reactor::read(&project.root, &BTreeMap::new())?;
        let poms = reactor
            .owned_paths()
            .map(|path| project.root.join(path))
            .chain(reactor.external_paths().map(Utf8Path::to_owned));
        for path in poms {
            if !evidence.inputs.iter().any(|input| input.path == path) {
                evidence.inputs.push(EvidenceInput {
                    path,
                    role: InputRole::Manifest,
                });
            }
        }
        cooldown_adapter_util::evidence::ancestors(&mut evidence, &project.root, &[]);
        cooldown_adapter_util::evidence::environment(
            &mut evidence,
            &["JAVA_HOME", "MAVEN_OPTS", "MAVEN_ARGS"],
        );
        Ok(evidence)
    }

    fn id(&self) -> ToolId {
        MAVEN_ID
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

    fn project_marker(&self) -> ProjectMarker {
        ProjectMarker {
            marker: "pom.xml",
            manifest: "pom.xml",
            alternate_manifests: &[],
            workspace_root: true,
        }
    }

    // A nested `pom.xml` belongs to the reactor whose `<modules>` reach it, and is a project of its
    // own otherwise.
    // Path order puts every directory after the ones above it, so an independent reactor nested
    // in another tree is settled before the modules it claims.
    fn nested_ownership(
        &self,
        primary: &[Utf8PathBuf],
        nested: &[Utf8PathBuf],
    ) -> Vec<NestedOwnership> {
        let mut answers = vec![NestedOwnership::Standalone; nested.len()];
        let claim = |root: &Utf8Path, answers: &mut Vec<NestedOwnership>| {
            let claimed = reactor::claimed_dirs(root);
            for (dir, answer) in nested.iter().zip(answers.iter_mut()) {
                if *answer == NestedOwnership::Standalone && claimed.contains(dir) {
                    *answer = NestedOwnership::Root(root.to_owned());
                }
            }
        };
        for root in primary {
            claim(root, &mut answers);
        }
        let mut order: Vec<&Utf8PathBuf> = nested.iter().collect();
        order.sort();
        for dir in order {
            let standalone = nested
                .iter()
                .zip(&answers)
                .any(|(other, answer)| other == dir && *answer == NestedOwnership::Standalone);
            if standalone {
                claim(dir, &mut answers);
            }
        }
        answers
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        version::classify_kind(from, to)
    }

    async fn dependencies(&self, project: &Project, _scope: DepScope) -> Result<Vec<Dependency>> {
        let reactor = Reactor::read(&project.root, &BTreeMap::new())?;
        // One row per dependency and version, attributed to every member that selects it.
        let mut rows: Vec<(String, String, Vec<MemberRef>)> = Vec::new();
        for entry in reactor.entries() {
            let Resolution::Known { current, .. } = entry.version else {
                continue;
            };
            match rows
                .iter_mut()
                .find(|(name, version, _)| *name == entry.name && *version == current)
            {
                Some((_, _, members)) => {
                    if !members.contains(&entry.member) {
                        members.push(entry.member);
                    }
                }
                None => rows.push((entry.name, current, vec![entry.member])),
            }
        }
        // Without Maven's resolution the graph is unknown, so every row is a direct dependency.
        Ok(rows
            .into_iter()
            .map(|(name, current, members)| Dependency {
                package: PackageId::new(MAVEN_ID, name, Some(MAVEN_CENTRAL.to_string())),
                // No POM records where an artifact comes from, and the routing that decides it
                // lives in settings and parents cooldown does not read, so no coordinate earns
                // the identity of OSV's Maven Central advisories.
                advisory_identity: None,
                current_quality: version::classify_quality(&current),
                current: Version::new(current),
                direct: true,
                artifacts: Vec::new(),
                graph_floor: None,
                graph_ceiling: None,
                declared_bound: None,
                members,
                pinned: false,
                hold_edges: Vec::new(),
            })
            .collect())
    }

    // Each dependency cooldown cannot judge without Maven is named once, rather than guessed at.
    async fn manifest_notices(&self, project: &Project) -> Result<Vec<Diagnostic>> {
        let reactor = Reactor::read(&project.root, &BTreeMap::new())?;
        let mut notices: Vec<Diagnostic> = reactor
            .foreign_repositories()
            .into_iter()
            .map(|(member, reason)| {
                Diagnostic::new(
                    DiagnosticKind::UnknownAge,
                    format!(
                        "cooldown does not judge the dependencies of {}: {reason}",
                        member.path
                    ),
                )
            })
            .collect();
        let mut unread: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for entry in reactor.entries() {
            match entry.version {
                Resolution::Floating(range) => notices.push(Diagnostic::new(
                    DiagnosticKind::UnknownAge,
                    format!(
                        "{} in {} floats on the version range {range}: Maven picks the newest \
                         release in it, which cooldown neither judges nor pins",
                        entry.name, entry.member.path
                    ),
                )),
                Resolution::Unread(reason) => unread
                    .entry(reason)
                    .or_default()
                    .push(format!("{} in {}", entry.name, entry.member.path)),
                Resolution::Known { .. } => {}
            }
        }
        notices.extend(unread.into_iter().map(|(reason, names)| {
            Diagnostic::new(
                DiagnosticKind::UnknownAge,
                format!("cooldown does not judge {}: {reason}", names.join(", ")),
            )
        }));
        Ok(notices)
    }

    async fn native_policy(&self, _project: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }

    async fn verify_lock_current(&self, _project: &Project) -> Result<LockVerifyReport> {
        Ok(verify_current_unknown("pom.xml"))
    }
}

#[async_trait]
impl ReleaseFetcher for MavenTool {
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
impl ToolWrite for MavenTool {
    fn mutation_tool(&self) -> ToolId {
        MAVEN_ID
    }

    // A preview copies what an apply's scratch copy holds, so Maven activates the same profiles
    // and builds the same projects in both.
    fn external_resolve_roots(&self, project: &Project) -> Result<Vec<Utf8PathBuf>> {
        let reactor = Reactor::read(&project.root, &BTreeMap::new()).ok();
        let external: Vec<&Utf8Path> = reactor.iter().flat_map(Reactor::external_paths).collect();
        verify::sources(&project.root, &external)
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        let reactor = Reactor::read(&project.root, &BTreeMap::new())?;
        ProjectMutationJournal::capture(&project.root, reactor.owned_paths())
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        let (project, plan, journal) = mutation.parts_for(self)?;
        self.land(project, plan, &mut journal.state_for(journal)?)
            .await
    }

    async fn apply_with_observer(
        &self,
        mutation: &PreparedMutation,
        _observer: &dyn ApplyObserver,
    ) -> Result<ApplyAttempt> {
        let (project, plan, journal) = mutation.parts_for(self)?;
        // The apply starts from the prepared state and hands back that state, advanced only by its
        // own writes, never a capture of the disk, so a POM someone else changed after preparing
        // is neither published over nor taken for cooldown's own, and a rollback refuses to
        // overwrite it.
        let mut owned = journal.state_for(journal)?;
        let report = self.land(project, plan, &mut owned).await;
        mutation.finished_attempt(report, &owned)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        // `package` compiles and packages every module, which `validate` never does.
        let args = ["-B", "-ntp", "-DskipTests", "package"].map(str::to_owned);
        self.mvn
            .verify(&project.root, &args, "mvn package succeeded")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reactor root claims the modules its `<modules>` reach, and a POM no reactor claims is
    /// a project of its own, claiming its own modules in turn.
    #[test]
    fn nested_poms_belong_to_the_reactor_that_lists_them() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
        let aggregator = |modules: &[&str]| {
            let modules = modules
                .iter()
                .map(|module| format!("<module>{module}</module>"))
                .collect::<Vec<_>>()
                .concat();
            format!("<project><modules>{modules}</modules></project>")
        };
        for (path, text) in [
            ("pom.xml", aggregator(&["app", "libs"])),
            ("app/pom.xml", aggregator(&[])),
            ("libs/pom.xml", aggregator(&["core"])),
            ("libs/core/pom.xml", aggregator(&[])),
            ("tools/pom.xml", aggregator(&["cli"])),
            ("tools/cli/pom.xml", aggregator(&[])),
        ] {
            std::fs::create_dir_all(root.join(path).parent().unwrap_or(&root))?;
            std::fs::write(root.join(path), text)?;
        }
        let cache = tempfile::tempdir()?;
        let tool = MavenTool::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        let nested = ["app", "libs", "libs/core", "tools", "tools/cli"].map(|dir| root.join(dir));

        let answers = tool.nested_ownership(std::slice::from_ref(&root), &nested);

        assert_eq!(
            answers,
            [
                NestedOwnership::Root(root.clone()),
                NestedOwnership::Root(root.clone()),
                NestedOwnership::Root(root.clone()),
                NestedOwnership::Standalone,
                NestedOwnership::Root(root.join("tools")),
            ]
        );
        Ok(())
    }

    /// A preview copies every file of the project but its version control metadata and build
    /// output, so a file that activates a profile reaches it as it reaches the apply's copy,
    /// together with a local parent outside the project and a `.mvn` above it.
    #[test]
    fn preview_roots_hold_every_file_maven_can_read() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let base = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
        let root = base.join("project");
        let pom = indoc::indoc! {"
            <project>
              <parent><groupId>demo</groupId><artifactId>parent</artifactId><version>1</version>
                <relativePath>../parent/pom.xml</relativePath></parent>
              <artifactId>demo</artifactId>
            </project>
        "};
        for (path, text) in [
            ("project/pom.xml", pom),
            ("project/extra.enabled", ""),
            ("project/extra/extra.xml", "<project/>"),
            ("project/.git/config", ""),
            ("project/target/classes/Demo.class", ""),
            (
                "parent/pom.xml",
                "<project><groupId>demo</groupId><artifactId>parent</artifactId>\
                 <version>1</version></project>",
            ),
            (".mvn/maven.config", "-B"),
        ] {
            std::fs::create_dir_all(base.join(path).parent().unwrap_or(&base))?;
            std::fs::write(base.join(path), text)?;
        }
        let cache = tempfile::tempdir()?;
        let tool = MavenTool::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        let project = Project {
            root: root.clone(),
            kind: MAVEN_ID,
            manifest: root.join("pom.xml"),
            exclude_newer: None,
            exclude_newer_package: BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };

        let roots: BTreeSet<Utf8PathBuf> =
            tool.external_resolve_roots(&project)?.into_iter().collect();

        let expected = [
            "project/pom.xml",
            "project/extra.enabled",
            "project/extra/extra.xml",
            "parent/pom.xml",
            ".mvn/maven.config",
        ]
        .map(|path| base.join(path));
        assert_eq!(roots, BTreeSet::from(expected));
        Ok(())
    }
}

/// Applies driven by a stand-in `mvn` script, which needs a Unix shell.
#[cfg(all(test, unix))]
mod stand_in_tests {
    use super::*;
    use indoc::{formatdoc, indoc};

    const TEXT: &str = "org.apache.commons:commons-text";

    /// A project whose `commons-text` version is the property `text.version`, used once.
    const POM: &str = indoc! {r#"
        <?xml version="1.0" encoding="UTF-8"?>
        <project xmlns="http://maven.apache.org/POM/4.0.0">
          <modelVersion>4.0.0</modelVersion>
          <groupId>demo</groupId>
          <artifactId>demo</artifactId>
          <version>0.1.0</version>
          <properties>
            <!-- the text release -->
            <text.version>1.10.0</text.version>
          </properties>
          <dependencies>
            <dependency>
              <groupId>org.apache.commons</groupId>
              <artifactId>commons-text</artifactId>
              <version>${text.version}</version>
            </dependency>
          </dependencies>
        </project>
    "#};

    /// The effective POM of [`POM`], reading only from Maven Central.
    const EFFECTIVE_POM: &str = indoc! {r#"
        <project xmlns="http://maven.apache.org/POM/4.0.0">
          <groupId>demo</groupId>
          <artifactId>demo</artifactId>
          <version>0.1.0</version>
          <repositories>
            <repository><id>central</id><url>https://repo.maven.apache.org/maven2</url></repository>
          </repositories>
        </project>
    "#};

    /// Effective settings without a mirror.
    const SETTINGS: &str = "<settings/>";

    /// The dependency tree of `demo:demo` selecting each `(coordinate, version)` of `selected`.
    fn tree(selected: &[(&str, &str)]) -> String {
        let children: Vec<String> = selected
            .iter()
            .map(|(coordinate, version)| {
                let (group, artifact) = coordinate.split_once(':').unwrap_or_default();
                format!(
                    r#"{{"groupId":"{group}","artifactId":"{artifact}","version":"{version}","type":"jar","classifier":""}}"#
                )
            })
            .collect();
        format!(
            r#"{{"groupId":"demo","artifactId":"demo","version":"0.1.0","type":"jar","children":[{}]}}"#,
            children.join(",")
        )
    }

    /// A stand-in `mvn` that writes each run's reports where the goals ask: the effective POM
    /// and the `before` tree for the run before the edit, and the effective POM, `settings` and
    /// the `after` tree for the run after it, but only once the copy's POM carries the planned
    /// edit.
    /// An empty tree is one the run leaves unwritten.
    fn stand_in(before: &str, settings: &str, after: &str) -> String {
        formatdoc! {r#"
            #!/bin/sh
            for arg in "$@"; do
              case "$arg" in
                -Doutput=*) output="${{arg#-Doutput=}}" ;;
                -DoutputFile=*) tree="${{arg#-DoutputFile=}}" ;;
              esac
            done
            report() {{
              printf '%s' "$2" > "$(printf '%s' "$output" | sed "s/[$]{{mojoExecution.goal}}/$1/")"
            }}
            tree="$PWD/pom.xml${{tree#'${{project.file}}'}}"
            report effective-pom '{EFFECTIVE_POM}'
            case "$tree" in
              *-before.json) contents='{before}' ;;
              *)
                grep -q '<text.version>1.12.0</text.version>' pom.xml || exit 1
                report effective-settings '{settings}'
                contents='{after}'
                ;;
            esac
            [ -z "$contents" ] || printf '%s' "$contents" > "$tree"
        "#}
    }

    /// A Maven project running a stand-in `mvn`, planned to move `commons-text` from 1.10.0 to
    /// 1.12.0.
    struct StandIn {
        _dirs: [tempfile::TempDir; 2],
        root: Utf8PathBuf,
        tool: MavenTool,
        plan: Plan,
    }

    impl StandIn {
        fn new(script: &str) -> Result<Self> {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir()?;
            let root = Utf8PathBuf::from_path_buf(std::fs::canonicalize(dir.path())?)
                .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
            std::fs::create_dir(root.join("project"))?;
            std::fs::write(root.join("project/pom.xml"), POM)?;
            let mvn = root.join("mvn");
            std::fs::write(&mvn, script)?;
            std::fs::set_permissions(&mvn, std::fs::Permissions::from_mode(0o755))?;
            let cache = tempfile::tempdir()?;
            let http = SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default())?;
            let tool = MavenTool {
                mvn: Driver::from_program(mvn.as_str()),
                ..MavenTool::from_http(http)
            };
            let plan = Plan {
                changes: vec![Change {
                    package: PackageId::new(MAVEN_ID, TEXT, Some(MAVEN_CENTRAL.to_string())),
                    from: Version::new("1.10.0"),
                    to: Version::new("1.12.0"),
                    kind: UpdateKind::Minor,
                    downgrade: false,
                    direct: true,
                    members: vec![MemberRef {
                        name: "demo".into(),
                        path: ".".into(),
                    }],
                }],
                ..Plan::default()
            };
            Ok(Self {
                _dirs: [dir, cache],
                root: root.join("project"),
                tool,
                plan,
            })
        }

        fn project(&self) -> Project {
            Project {
                root: self.root.clone(),
                kind: MAVEN_ID,
                manifest: self.root.join("pom.xml"),
                exclude_newer: None,
                exclude_newer_package: BTreeMap::new(),
                generated_members: cooldown_core::GeneratedMembers::undeclared(),
            }
        }

        async fn prepare(&self) -> Result<PreparedMutation> {
            PreparedMutation::prepare(&self.tool, &self.project(), &self.plan).await
        }

        fn pom(&self) -> Result<String> {
            Ok(std::fs::read_to_string(self.root.join("pom.xml"))?)
        }
    }

    /// A landing Maven confirms rewrites the property's text and nothing else of the POM, and
    /// writes no file of its own into the project.
    #[tokio::test]
    async fn a_verified_landing_rewrites_only_the_version_text() -> Result<()> {
        let stand_in = StandIn::new(&stand_in(
            &tree(&[(TEXT, "1.10.0")]),
            SETTINGS,
            &tree(&[(TEXT, "1.12.0")]),
        ))?;
        let mutation = stand_in.prepare().await?;

        let report = stand_in.tool.apply(&mutation).await?;

        assert_eq!(report.applied, stand_in.plan.changes);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(
            stand_in.pom()?,
            POM.replace("<text.version>1.10.0<", "<text.version>1.12.0<")
        );
        let files: Vec<_> = stand_in
            .root
            .read_dir_utf8()?
            .collect::<std::io::Result<_>>()?;
        assert_eq!(files.len(), 1, "{files:?}");
        Ok(())
    }

    /// An edit made between preparing the mutation and applying it is neither published over by a
    /// landing Maven confirms nor taken for cooldown's own.
    #[tokio::test]
    async fn an_edit_made_after_preparing_is_never_overwritten() -> Result<()> {
        let stand_in = StandIn::new(&stand_in(
            &tree(&[(TEXT, "1.10.0")]),
            SETTINGS,
            &tree(&[(TEXT, "1.12.0")]),
        ))?;
        let mutation = stand_in.prepare().await?;
        let edited = format!("{POM}<!-- edited meanwhile -->\n");
        std::fs::write(stand_in.root.join("pom.xml"), &edited)?;

        let attempt = stand_in.tool.apply_with_observer(&mutation, &()).await?;

        let Ok(cooldown_core::ApplyAttemptOutcome::Finished { report, postimage }) =
            attempt.into_outcome_for(&mutation)
        else {
            panic!("the apply must finish");
        };
        assert!(
            matches!(report, Err(CoreError::LockConflict(_))),
            "{report:?}"
        );
        // The postimage is the prepared POM, so the rollback refuses to write over the edit.
        let prepared = postimage
            .files()
            .iter()
            .find(|file| file.path() == Utf8Path::new("pom.xml"))
            .and_then(cooldown_core::ProjectMutationFile::contents);
        assert_eq!(prepared, Some(POM.as_bytes()));
        assert!(matches!(
            mutation.journal().restore_if_unchanged(&postimage),
            Err(CoreError::LockConflict(_))
        ));
        assert_eq!(stand_in.pom()?, edited);
        Ok(())
    }

    /// A mirror standing in for Maven Central fails the run, a broken environment rather than a
    /// conflict to isolate, and names no address.
    #[tokio::test]
    async fn a_central_mirror_fails_the_run() -> Result<()> {
        let mirrored = "<settings><mirrors><mirror><mirrorOf>*</mirrorOf>\
                        <url>https://corp.example.com/maven2</url></mirror></mirrors></settings>";
        let stand_in = StandIn::new(&stand_in(
            &tree(&[(TEXT, "1.10.0")]),
            mirrored,
            &tree(&[(TEXT, "1.12.0")]),
        ))?;
        let mutation = stand_in.prepare().await?;

        let report = stand_in.tool.apply(&mutation).await;

        match &report {
            Err(CoreError::System(detail)) => {
                assert!(detail.contains("mirror"), "{detail}");
                assert!(!detail.contains("example.com"), "{detail}");
            }
            other => panic!("the mirror must fail the run, got {other:?}"),
        }
        assert_eq!(stand_in.pom()?, POM);
        Ok(())
    }

    /// Maven moving a dependency the plan does not name refuses the batch, so isolation can hold
    /// the change that caused it, and publishes nothing.
    #[tokio::test]
    async fn an_unplanned_move_publishes_nothing() -> Result<()> {
        let lang = "org.apache.commons:commons-lang3";
        let stand_in = StandIn::new(&stand_in(
            &tree(&[(TEXT, "1.10.0"), (lang, "3.12.0")]),
            SETTINGS,
            &tree(&[(TEXT, "1.12.0"), (lang, "3.14.0")]),
        ))?;
        let mutation = stand_in.prepare().await?;

        let report = stand_in.tool.apply(&mutation).await;

        assert!(
            matches!(&report, Err(CoreError::UnacceptableResolve(detail)) if detail.contains("commons-lang3")),
            "{report:?}"
        );
        assert_eq!(stand_in.pom()?, POM);
        Ok(())
    }

    /// A project Maven builds without a report fails closed: before the edit as a broken
    /// environment, after it as a resolve isolation can blame on the change, and either way
    /// nothing is published.
    #[tokio::test]
    async fn a_project_without_a_report_publishes_nothing() -> Result<()> {
        let landed = tree(&[(TEXT, "1.12.0")]);
        let unreported_before = StandIn::new(&stand_in("", SETTINGS, &landed))?;
        let unreported_after = StandIn::new(&stand_in(&tree(&[(TEXT, "1.10.0")]), SETTINGS, ""))?;

        let before = unreported_before
            .tool
            .apply(&unreported_before.prepare().await?)
            .await;
        let after = unreported_after
            .tool
            .apply(&unreported_after.prepare().await?)
            .await;

        assert!(matches!(&before, Err(CoreError::System(_))), "{before:?}");
        assert!(
            matches!(&after, Err(CoreError::UnacceptableResolve(_))),
            "{after:?}"
        );
        assert_eq!(unreported_before.pom()?, POM);
        assert_eq!(unreported_after.pom()?, POM);
        Ok(())
    }

    /// Maven selecting another version than cooldown read, as a profile or a setting can make it,
    /// holds the change before anything is edited.
    #[tokio::test]
    async fn a_version_maven_does_not_select_is_held() -> Result<()> {
        let stand_in = StandIn::new(&stand_in(
            &tree(&[(TEXT, "1.11.0")]),
            SETTINGS,
            &tree(&[(TEXT, "1.11.0")]),
        ))?;
        let mutation = stand_in.prepare().await?;

        let report = stand_in.tool.apply(&mutation).await?;

        assert_eq!(report.applied, []);
        let skipped: Vec<_> = report
            .skipped
            .iter()
            .map(|skip| (skip.reason, skip.detail.clone().unwrap_or_default()))
            .collect();
        assert!(
            matches!(skipped.as_slice(), [(SkipReason::NotEligible, detail)] if detail.contains("1.11.0")),
            "{skipped:?}"
        );
        assert_eq!(stand_in.pom()?, POM);
        Ok(())
    }
}
