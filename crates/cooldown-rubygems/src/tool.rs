//! The Ruby/Bundler [`Tool`](cooldown_core::Tool): detection by `Gemfile.lock`,
//! the resolved graph read from that lock, and `RubyGems` publish times.
//! The core owns the verdict; Bundler is driven only to apply.

use crate::lock;
use crate::registry::{RUBYGEMS, RubyGems};
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::{
    Driver, RegistryVersionClassifier, build_registry_releases, verify_current_unknown,
};
use cooldown_core::{
    ApplyAttempt, ApplyObserver, ApplyReport, CandidateScope, Capabilities, Change, CoreError,
    DepScope, Dependency, FetchContext, LockVerifyReport, NativePolicyLayer, PackageId,
    PackageRegistry, Plan, PreparedMutation, Project, ProjectMarker, ProjectMutationFile,
    ProjectMutationJournal, ProjectMutationState, RawRelease, Release, ReleaseFetcher,
    ReleaseOrder, ReleaseQuality, ResolveInputs, Result, ToolId, ToolRead, ToolWrite, UpdateKind,
    VerifyReport, Version, failure_detail,
};
use cooldown_registry::SharedHttp;
use indoc::indoc;
use std::collections::HashMap;

/// The [`ToolId`] for the Ruby/Bundler adapter (`"bundler"`).
pub const BUNDLER_ID: ToolId = ToolId("bundler");

/// The Ruby program `apply` relocks a project with, so that each planned gem lands on exactly its
/// target.
///
/// Bundler has no option to lock a gem at a given version, and `bundle update` picks the newest
/// release a requirement allows, past cooldown's target and never below the locked version.
/// The program runs `bundle lock --conservative --update <gems>` with Bundler's resolver offered
/// only the target version of each named gem.
/// Bundler still evaluates the `Gemfile` and every other requirement, moves any other gem a target
/// needs moved, and writes the dependencies, platforms and checksums of the versions it picks.
///
/// Its arguments are the project's `Gemfile` and lock, the file to write the new lock to, a private
/// directory for Git checkouts, and each gem as a name followed by its target.
/// The user's Bundler environment passes through, so private sources keep their credentials, and
/// the program owns only what decides where it reads and writes: it binds the `Gemfile` and lock,
/// puts Bundler's install path in the private directory, and pins the loaded Bundler.
/// It then reads those back and exits with [`UNSUPPORTED_BUNDLER`] on any mismatch, as it does when
/// the Bundler it loads is outside the range its resolver hook was verified against or lacks the
/// hook, and when a mirror is configured for rubygems.org.
/// So it writes no project file and installs nothing, and `apply` takes no environment turn.
/// It fails like a resolver conflict when the new lock misses a gem on a platform the old one
/// lists that the old one did not miss.
const EXACT_LOCK: &str = indoc! {r#"
    # Relocks a project with each named gem on exactly cooldown's target.
    gemfile, lock, out, checkout, *pairs = ARGV
    # Bundler resolves this Gemfile against this lock, whatever the environment names.
    ENV["BUNDLE_GEMFILE"] = gemfile
    ENV["BUNDLE_LOCKFILE"] = lock

    begin
      require "bundler"
    rescue LoadError
      warn "cooldown: cannot load Bundler"
      exit 3
    end

    # 2.7.2 is the first release whose update can move a named gem below its locked version.
    supported = Gem::Requirement.new(">= 2.7.2", "< 5")
    unless supported.satisfied_by?(Gem::Version.new(Bundler::VERSION))
      warn "cooldown needs Bundler #{supported} to land exact versions, " \
           "but loaded Bundler #{Bundler::VERSION}"
      exit 3
    end
    # `Resolver#filter_specs` is private Bundler API.
    unless Bundler::Resolver.private_method_defined?(:filter_specs)
      warn "cooldown cannot land exact versions with Bundler #{Bundler::VERSION}, " \
           "whose resolver has no `filter_specs`"
      exit 3
    end

    # Every settings object Bundler hands out, ahead of any configured value, puts the install
    # path, where a Git source is checked out while it resolves, and the Git cache in the private
    # directory, and keeps Bundler from restarting under another release, which would run without
    # the resolver hook below.
    Bundler.singleton_class.prepend(Module.new do
      define_method(:settings) do
        super().tap do |settings|
          settings.temporary(path: checkout, global_gem_cache: false, version: "system")
        end
      end
    end)
    # A process started under `bundle exec` has set Bundler up already, with its paths memoized.
    Bundler.reset!
    unless Bundler.bundle_path.ascend.include?(Pathname.new(checkout).expand_path)
      warn "cooldown: Bundler would check sources out outside its private directory"
      exit 3
    end

    # A mirror can serve something else under a rubygems.org gem's name and version, and Bundler
    # may switch to one at any fetch, so a mirror for every source refuses the relock, as does one
    # that Bundler's own parse of the settings maps to rubygems.org's address or host, the two keys
    # it looks a source's mirror up by.
    rubygems = Bundler::Settings.normalize_uri("https://rubygems.org")
    mirrored = Bundler.settings.all.any? { |key| key.match?(%r{\Amirror\.all/?\z}) }
    Bundler.settings.gem_mirrors.each do |source, mirror|
      mirrored ||= !mirror.empty? && [rubygems, "rubygems.org"].include?(source)
    end
    if mirrored
      warn "cooldown cannot judge the gems a mirror serves in place of rubygems.org"
      exit 3
    end

    targets = pairs.each_slice(2).map { |name, version| [name, Gem::Version.new(version)] }.to_h
    Bundler::Resolver.prepend(Module.new do
      define_method(:filter_specs) do |specs, package|
        target = targets[package.name]
        specs = super(specs, package)
        target ? specs.select { |spec| spec.version == target } : specs
      end
    end)

    # The project's settings can name another Gemfile, which `--gemfile` outranks, and since 2.7.2
    # `--lockfile` only names the file the new lock is written to, so nothing Bundler or the
    # Gemfile prints can reach it.
    require "bundler/friendly_errors"
    Bundler.with_friendly_errors do
      require "bundler/cli"
      Bundler::CLI.start(
        ["lock", "--conservative", "--gemfile", gemfile, "--lockfile", out,
         "--update", *targets.keys],
        debug: true,
      )
    end

    definition = Bundler.definition
    same = ->(path, prepared) { path && File.expand_path(path) == File.expand_path(prepared) }
    # A Gemfile can name another lock, whose pins the new one would then carry.
    unless same.call(definition.gemfiles.first, gemfile) && same.call(definition.lockfile, lock)
      warn "cooldown: Bundler resolved another Gemfile or lock than the project's"
      exit 3
    end

    # Bundler checks only the platforms it adds, and only with the host platform's roots, so a
    # target can leave a gem missing on a platform the lock lists, which Bundler then keeps
    # listing or quietly drops.
    # The new lock has to list every platform the old one did, and on each miss no gem under a
    # root that the old one did not.
    before, after = [lock, out].map { |path| Bundler::LockfileParser.new(Bundler.read_file(path)) }
    holes = lambda do |parsed, platform|
      specs = Bundler::SpecSet.new(parsed.specs)
      definition.dependencies.flat_map do |root|
        next [] if root.gem_platforms([platform]).empty?
        # Bundler finds nothing missing in an empty set of specs.
        missing = specs.empty? ? [root] : specs.incomplete_specs_for_platform([root], platform)
        missing.map { |spec| [root.name, spec.name] }
      end
    end
    before.platforms.each do |platform|
      kept = after.platforms.include?(platform) &&
             (holes.call(after, platform) - holes.call(before, platform)).empty?
      abort "cooldown: the new lock lacks gems it needs on #{platform}" unless kept
    end
"#};

/// The status [`EXACT_LOCK`] exits with when it cannot vouch for the lock it would write.
const UNSUPPORTED_BUNDLER: i32 = 3;

/// The Ruby/Bundler implementation of the [`Tool`](cooldown_core::Tool) port.
pub struct BundlerTool {
    registry: RubyGems,
    bundle: Driver,
    /// The Ruby that runs [`EXACT_LOCK`], which loads Bundler as a library.
    ruby: Driver,
}

impl BundlerTool {
    /// Creates the adapter from a configured [`RubyGems`] client.
    #[must_use]
    pub fn new(registry: RubyGems) -> Self {
        BundlerTool {
            registry,
            bundle: Driver::new("bundle"),
            ruby: Driver::new("ruby"),
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`RubyGems`] client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        BundlerTool::new(RubyGems::new(http))
    }

    /// Relocks `project` with every change on exactly its target through [`EXACT_LOCK`], and
    /// returns the new lock without writing it.
    async fn exact_lock(&self, project: &Project, changes: &[Change]) -> Result<String> {
        let scratch = tempfile::tempdir()?;
        let scratch_path =
            Utf8PathBuf::from_path_buf(scratch.path().to_path_buf()).map_err(|path| {
                CoreError::PathEncoding(format!("{} is not valid UTF-8", path.display()))
            })?;
        let helper = scratch_path.join("exact_lock.rb");
        std::fs::write(&helper, EXACT_LOCK)?;
        let out = scratch_path.join("Gemfile.lock");
        let mut args = vec![
            helper.to_string(),
            project.root.join("Gemfile").to_string(),
            project.root.join("Gemfile.lock").to_string(),
            out.to_string(),
            scratch_path.join("checkout").to_string(),
        ];
        for change in changes {
            args.push(change.package.name.clone());
            args.push(change.to.as_str().to_string());
        }
        run_exact_lock(&self.ruby, &project.root, &args).await?;
        Ok(std::fs::read_to_string(&out)?)
    }

    /// Relocks `project` to land `plan` and publishes the new lock through `owned`, the write set
    /// as it was prepared, which it leaves holding exactly what the apply wrote.
    async fn relock(
        &self,
        project: &Project,
        plan: &Plan,
        owned: &mut ProjectMutationState,
    ) -> Result<ApplyReport> {
        if plan.changes.is_empty() {
            return Ok(ApplyReport::default());
        }
        let lock_path = Utf8Path::new("Gemfile.lock");
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
        // The whole batch resolves at once, so moves that are only valid together land together,
        // and a conflict fails the batch for the caller to isolate.
        let after = self.exact_lock(project, &plan.changes).await?;
        let collateral = relock_moves(&before, &after, &plan.changes)?;
        owned.write_owned(lock_path, after.as_bytes())?;

        let mut applied = plan.changes.clone();
        applied.extend(collateral);
        Ok(ApplyReport {
            applied,
            ..ApplyReport::default()
        })
    }
}

/// Runs [`EXACT_LOCK`] as `ruby <args>` in `dir`.
///
/// A refusal to vouch for the lock is a broken environment rather than a conflict between the
/// planned gems, so it is a [`CoreError::System`] that fails the run instead of holding each gem in
/// turn.
async fn run_exact_lock(ruby: &Driver, dir: &Utf8Path, args: &[String]) -> Result<()> {
    let output = ruby.output_with_env(dir, args, &[]).await?;
    if output.status.code() == Some(UNSUPPORTED_BUNDLER) {
        return Err(CoreError::System(failure_detail(&output)));
    }
    if !output.status.success() {
        return Err(ruby.failure(&output));
    }
    Ok(())
}

/// The collateral rows of a relock from the lock `before` to the lock `after` that was asked to
/// land `planned`, sorted by name, so that no gem's version changes in silence.
///
/// `--conservative` still moves a gem a target needs moved, such as `racc` when `i18n` 1.14.3
/// starts requiring `racc (~> 1.7)`.
/// A planned gem's own row reports its whole move, and a gem the relock adds or drops has no
/// version to move from or to.
///
/// # Errors
///
/// Returns [`CoreError::UnacceptableResolve`] when a planned gem is not on exactly its target from
/// rubygems.org, or when the relock moves a gem that rubygems.org does not serve, whose release
/// cooldown cannot judge.
fn relock_moves(before: &str, after: &str, planned: &[Change]) -> Result<Vec<Change>> {
    let gems = |lock: &str| -> HashMap<String, lock::ResolvedPin> {
        lock::parse_resolved(lock)
            .into_iter()
            .map(|pin| (pin.name.clone(), pin))
            .collect()
    };
    let (before, after) = (gems(before), gems(after));
    for change in planned {
        let name = change.package.name.as_str();
        let landed = after.get(name);
        let on_target = landed.is_some_and(|pin| {
            pin.rubygems && version::compare(&pin.version, change.to.as_str()).is_eq()
        });
        if !on_target {
            let held = landed.map_or_else(
                || format!("Bundler's lock no longer holds {name}"),
                |pin| format!("Bundler locked {name} {}, not {}", pin.version, change.to),
            );
            return Err(CoreError::UnacceptableResolve(held));
        }
    }

    let mut collateral = Vec::new();
    for (name, from) in &before {
        let Some(to) = after.get(name) else {
            continue;
        };
        if planned.iter().any(|change| change.package.name == *name)
            || version::compare(&from.version, &to.version).is_eq()
        {
            continue;
        }
        if !(from.rubygems && to.rubygems) {
            return Err(CoreError::UnacceptableResolve(format!(
                "Bundler would move {name} {} to {}, which is not a rubygems.org gem cooldown can \
                 judge",
                from.version, to.version
            )));
        }
        collateral.push(Change {
            package: PackageId::new(BUNDLER_ID, name.clone(), Some(RUBYGEMS.to_string())),
            from: Version::new(from.version.clone()),
            to: Version::new(to.version.clone()),
            // A collateral move is consistency churn, not a declared bump, so its update kind is
            // informational only and `Minor` is the neutral label the renderer shows.
            kind: UpdateKind::Minor,
            downgrade: version::compare(&to.version, &from.version).is_lt(),
            direct: false,
            members: Vec::new(),
        });
    }
    collateral.sort_by(|a, b| a.package.name.cmp(&b.package.name));
    Ok(collateral)
}

fn classify_quality(v: &str) -> ReleaseQuality {
    if version::is_prerelease(v) {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

fn build_releases(current: &str, raw: Vec<RawRelease>) -> Vec<Release> {
    build_registry_releases(
        current,
        raw,
        RegistryVersionClassifier {
            is_valid: |value| !value.is_empty(),
            compare: version::compare,
            major_key: version::major_key,
            major_number: version::major_number,
            classify_kind: version::classify_kind,
            classify_quality,
        },
    )
}

#[async_trait]
impl ToolRead for BundlerTool {
    fn version_command(&self) -> Option<cooldown_core::trace::ToolVersionCommand> {
        Some(cooldown_core::trace::ToolVersionCommand {
            program: self.bundle.program().to_string(),
            arguments: vec!["--version".into()],
        })
    }

    fn evidence(&self, project: &Project) -> Result<cooldown_core::trace::Evidence> {
        use cooldown_core::trace::GraphCoverage;
        let mut evidence = cooldown_adapter_util::evidence::native(
            project,
            "Gemfile.lock",
            GraphCoverage::Resolved,
            &[".ruby-version", ".bundle/config"],
        );
        cooldown_adapter_util::evidence::ancestors(&mut evidence, &project.root, &[]);
        cooldown_adapter_util::evidence::environment(
            &mut evidence,
            &[
                "BUNDLE_GEMFILE",
                "BUNDLE_PATH",
                "BUNDLE_WITHOUT",
                "BUNDLE_WITH",
                "GEM_HOME",
                "GEM_PATH",
            ],
        );
        Ok(evidence)
    }

    fn id(&self) -> ToolId {
        BUNDLER_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: true,
            artifact_granular: false,
            advisory_ecosystem: Some("RubyGems"),
            honors_generated_members: false,
        }
    }

    fn project_marker(&self) -> cooldown_core::ProjectMarker {
        ProjectMarker {
            marker: "Gemfile.lock",
            manifest: "Gemfile",
            alternate_manifests: &[],
            workspace_root: false,
        }
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        version::classify_kind(from, to)
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let content = std::fs::read_to_string(project.root.join("Gemfile.lock"))?;
        let direct = lock::parse_direct(&content);
        let ceilings = lock::graph_ceilings(&content);
        let mut deps = Vec::new();
        for resolved in lock::parse_resolved(&content) {
            // Only a gem served by the public rubygems.org remote is the gem whose releases
            // cooldown reads and OSV's `RubyGems` ecosystem describes.
            // A private remote's or a `GIT`/`PATH` section's gem merely shares the name, so it is
            // never planned, while Bundler still honors its requirements when it relocks.
            if !resolved.rubygems {
                continue;
            }
            let is_direct = direct.contains(&resolved.name);
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            deps.push(Dependency {
                package: PackageId::new(
                    BUNDLER_ID,
                    resolved.name.clone(),
                    Some(RUBYGEMS.to_string()),
                ),
                advisory_identity: Some(resolved.name.clone()),
                current: Version::new(resolved.version.clone()),
                current_quality: classify_quality(&resolved.version),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                // A requirer pinning this gem `(= X)` caps it at its resolved version. The active
                // check (pin equals the resolved version) records the canonical resolved form so it
                // matches a fetched release, mirroring the uv adapter.
                graph_ceiling: ceilings.get(&resolved.name).and_then(|pin| {
                    version::compare(pin, &resolved.version)
                        .is_eq()
                        .then(|| Version::new(resolved.version.clone()))
                }),
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
        Ok(verify_current_unknown("Gemfile.lock"))
    }
}

#[async_trait]
impl ReleaseFetcher for BundlerTool {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.registry.releases(&dep.package).await?;
        Ok(build_releases(dep.current.as_str(), raw))
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
impl ToolWrite for BundlerTool {
    fn mutation_tool(&self) -> ToolId {
        BUNDLER_ID
    }

    fn supports_transitive_advance(&self) -> bool {
        // `EXACT_LOCK` pins any locked gem, declared or not.
        true
    }

    fn resolve_inputs(&self) -> ResolveInputs {
        // A `gemspec` directive in the Gemfile makes bundler load the project's `*.gemspec` (Ruby),
        // which typically `require_relative`s a `lib/**/version.rb`. The throwaway probe copy must
        // carry both, so `.gemspec` and `.rb` source are included; a plain Gemfile ignores them.
        ResolveInputs {
            source_extensions: &["rb", "gemspec"],
            ..ResolveInputs::DEFAULT
        }
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        ProjectMutationJournal::capture(
            &project.root,
            [Utf8Path::new("Gemfile"), Utf8Path::new("Gemfile.lock")],
        )
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
        // own write, never a capture of the disk, so a file someone else changed after preparing
        // is neither published over nor taken for cooldown's own, and a rollback refuses to
        // overwrite it.
        let mut owned = journal.state_for(journal)?;
        let report = self.relock(project, plan, &mut owned).await;
        mutation.finished_attempt(report, &owned)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.bundle
            .verify(
                &project.root,
                &["lock".to_string()],
                "bundle lock succeeded",
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Bundler project rooted at `root`.
    fn project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            kind: BUNDLER_ID,
            manifest: root.join("Gemfile"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        }
    }

    #[test]
    fn advisory_ecosystem_matches_osv() {
        let cache = tempfile::tempdir().expect("cache");
        let tool = BundlerTool::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );
        // The apply only relocks, so it takes no environment turn, and it pins transitives too.
        assert!(!tool.mutation_installs());
        assert!(tool.supports_transitive_advance());
        assert_eq!(tool.capabilities().advisory_ecosystem, Some("RubyGems"));
    }

    /// The exact-lock program refuses a Bundler outside the range its resolver hook was verified
    /// against, or one without the hook, before it resolves anything.
    /// The refusal is a broken environment, never a conflict to isolate gem by gem.
    #[tokio::test]
    async fn the_exact_lock_refuses_an_unverified_bundler() -> Result<()> {
        if cooldown_adapter_util::program_on_path("ruby").is_none() {
            eprintln!("skipping: ruby is not on PATH");
            return Ok(());
        }
        let hook = indoc! {"
            private

            def filter_specs(specs, _package)
              specs
            end
        "};
        let cases = [
            ("2.7.1", hook, "needs Bundler"),
            ("5.0.0", hook, "needs Bundler"),
            ("4.0.10", "", "no `filter_specs`"),
        ];
        for (version, resolver, reason) in cases {
            // A stand-in Bundler, which `-I` puts ahead of the one Ruby ships.
            let scratch = tempfile::tempdir()?;
            let root = Utf8PathBuf::from_path_buf(scratch.path().to_path_buf())
                .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
            std::fs::write(
                root.join("bundler.rb"),
                indoc::formatdoc! {r#"
                    module Bundler
                      VERSION = "{version}"
                      class Resolver
                    {resolver}
                      end
                    end
                "#},
            )?;
            std::fs::write(root.join("exact_lock.rb"), EXACT_LOCK)?;
            let args: Vec<String> = [
                "-I",
                root.as_str(),
                root.join("exact_lock.rb").as_str(),
                root.join("Gemfile").as_str(),
                root.join("Gemfile.lock").as_str(),
                root.join("new.lock").as_str(),
                root.join("checkout").as_str(),
                "i18n",
                "1.14.5",
            ]
            .map(str::to_string)
            .to_vec();

            // Ruby runs in this crate, where a version manager's shim finds the pinned Ruby.
            let refused = run_exact_lock(
                &Driver::from_program("ruby"),
                Utf8Path::new(env!("CARGO_MANIFEST_DIR")),
                &args,
            )
            .await;

            // The refusal names the Bundler it loaded and why it is refused, and comes before
            // anything is checked out or locked.
            match refused {
                Err(CoreError::System(detail)) => {
                    assert!(detail.contains(&format!("Bundler {version}")), "{detail}");
                    assert!(detail.contains(reason), "{detail}");
                }
                other => panic!("Bundler {version} must be refused, got {other:?}"),
            }
            assert!(!root.join("checkout").exists());
            assert!(!root.join("new.lock").exists());
        }
        Ok(())
    }

    /// A `Gemfile.lock` with one rubygems.org section holding `public`, and one private
    /// remote's section holding `private`, each as `(gem, version)` pairs.
    fn gemfile_lock(public: &[(&str, &str)], private: &[(&str, &str)]) -> String {
        let specs = |gems: &[(&str, &str)]| -> String {
            gems.iter()
                .map(|(gem, version)| format!("    {gem} ({version})"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        indoc::formatdoc! {"
            GEM
              remote: https://gems.example.com/
              specs:
            {private}
            GEM
              remote: https://rubygems.org/
              specs:
            {public}
            DEPENDENCIES
              i18n
        ", private = specs(private), public = specs(public)}
    }

    /// The planned move of `gem` from `from` to `to`.
    fn change(gem: &str, from: &str, to: &str) -> Change {
        Change {
            package: PackageId::new(BUNDLER_ID, gem.to_string(), Some(RUBYGEMS.to_string())),
            from: Version::new(from.to_string()),
            to: Version::new(to.to_string()),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        }
    }

    /// A gem the relock moves beyond the plan is a collateral row, while a planned gem's move, a
    /// gem the relock adds, and an unmoved gem are not.
    #[test]
    fn relock_moves_report_only_unplanned_moves() -> Result<()> {
        let before = gemfile_lock(
            &[("i18n", "1.8.11"), ("racc", "1.6.2"), ("rake", "13.0.6")],
            &[],
        );
        let after = gemfile_lock(
            &[
                ("concurrent-ruby", "1.2.3"),
                ("i18n", "1.14.3"),
                ("racc", "1.8.1"),
                ("rake", "13.0.6"),
            ],
            &[],
        );

        let collateral = relock_moves(&before, &after, &[change("i18n", "1.8.11", "1.14.3")])?;

        let moves: Vec<(&str, &str, &str, bool)> = collateral
            .iter()
            .map(|change| {
                (
                    change.package.name.as_str(),
                    change.from.as_str(),
                    change.to.as_str(),
                    change.downgrade,
                )
            })
            .collect();
        assert_eq!(moves, [("racc", "1.6.2", "1.8.1", false)]);
        Ok(())
    }

    /// A relock that lands its target but moves a gem from a private remote fails, since cooldown
    /// cannot judge that gem's release, while a relock that leaves it in place passes.
    #[test]
    fn relock_moves_refuse_moving_a_private_gem() {
        let planned = [change("i18n", "1.8.11", "1.14.5")];
        let before = gemfile_lock(&[("i18n", "1.8.11")], &[("parent", "1.0")]);
        let forced = gemfile_lock(&[("i18n", "1.14.5")], &[("parent", "1.1")]);
        let kept = gemfile_lock(&[("i18n", "1.14.5")], &[("parent", "1.0")]);

        match relock_moves(&before, &forced, &planned) {
            Err(CoreError::UnacceptableResolve(detail)) => {
                assert!(detail.contains("parent 1.0 to 1.1"), "{detail}");
            }
            other => panic!("moving the private parent must fail, got {other:?}"),
        }
        assert!(matches!(relock_moves(&before, &kept, &planned), Ok(rows) if rows.is_empty()));
    }

    /// A relock that leaves a planned gem off its target, or lands it from another remote, fails.
    #[test]
    fn relock_moves_refuse_a_target_that_did_not_land() {
        let planned = [change("i18n", "1.8.11", "1.14.5")];
        let before = gemfile_lock(&[("i18n", "1.8.11")], &[]);
        for after in [
            gemfile_lock(&[("i18n", "1.15.2")], &[]),
            gemfile_lock(&[], &[("i18n", "1.14.5")]),
            gemfile_lock(&[], &[]),
        ] {
            assert!(
                matches!(
                    relock_moves(&before, &after, &planned),
                    Err(CoreError::UnacceptableResolve(_))
                ),
                "{after}"
            );
        }
    }

    /// A Bundler project whose relock runs a stand-in `ruby` script, planned to move its `i18n`
    /// from 1.8.11 to 1.14.5.
    #[cfg(unix)]
    struct StandIn {
        _dirs: [tempfile::TempDir; 2],
        root: Utf8PathBuf,
        gemfile: &'static str,
        lock: String,
        tool: BundlerTool,
        plan: Plan,
    }

    #[cfg(unix)]
    impl StandIn {
        fn new(script: &str) -> Result<Self> {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir()?;
            let root = Utf8PathBuf::from_path_buf(std::fs::canonicalize(dir.path())?)
                .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
            let gemfile = "source \"https://rubygems.org\"\ngem \"i18n\"\n";
            let lock = gemfile_lock(&[("i18n", "1.8.11")], &[]);
            std::fs::write(root.join("Gemfile"), gemfile)?;
            std::fs::write(root.join("Gemfile.lock"), &lock)?;
            let ruby = root.join("ruby");
            std::fs::write(&ruby, script)?;
            std::fs::set_permissions(&ruby, std::fs::Permissions::from_mode(0o755))?;
            let cache = tempfile::tempdir()?;
            let tool = BundlerTool {
                ruby: Driver::from_program(ruby.as_str()),
                ..BundlerTool::from_http(SharedHttp::new(
                    cache.path(),
                    cooldown_registry::HttpOptions::default(),
                )?)
            };
            let plan = Plan {
                changes: vec![change("i18n", "1.8.11", "1.14.5")],
                ..Plan::default()
            };
            Ok(Self {
                _dirs: [dir, cache],
                root,
                gemfile,
                lock,
                tool,
                plan,
            })
        }

        /// Asserts that `attempt` failed and handed back the `Gemfile` as it was prepared, so that
        /// the rollback recovery runs refuses to overwrite the edit since made to it.
        fn assert_edit_survives(&self, mutation: &PreparedMutation, attempt: ApplyAttempt) {
            let Ok(cooldown_core::ApplyAttemptOutcome::Finished { report, postimage }) =
                attempt.into_outcome_for(mutation)
            else {
                panic!("the apply must finish");
            };
            assert!(report.is_err(), "{report:?}");
            let postimage_gemfile = postimage
                .files()
                .iter()
                .find(|file| file.path() == Utf8Path::new("Gemfile"))
                .and_then(ProjectMutationFile::contents);
            assert_eq!(postimage_gemfile, Some(self.gemfile.as_bytes()));
            assert!(matches!(
                mutation.journal().restore_if_unchanged(&postimage),
                Err(CoreError::LockConflict(_))
            ));
            let read = |file: &str| std::fs::read_to_string(self.root.join(file)).ok();
            assert!(read("Gemfile").is_some_and(|gemfile| gemfile.contains("rake")));
            assert_eq!(read("Gemfile.lock"), Some(self.lock.clone()));
        }
    }

    /// When Bundler fails while someone else edits the `Gemfile`, the apply hands back the state it
    /// was prepared with, so rolling it back refuses to overwrite the edit.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_relock_never_claims_a_concurrent_edit() -> Result<()> {
        // A stand-in Ruby that edits the Gemfile, as an editor saving it meanwhile would, and
        // then fails like a conflicting resolve.
        let stand_in = StandIn::new(indoc! {r#"
            #!/bin/sh
            echo 'gem "rake"' >> Gemfile
            exit 1
        "#})?;
        let mutation =
            PreparedMutation::prepare(&stand_in.tool, &project(&stand_in.root), &stand_in.plan)
                .await?;

        let attempt = stand_in.tool.apply_with_observer(&mutation, &()).await?;

        stand_in.assert_edit_survives(&mutation, attempt);
        Ok(())
    }

    /// An edit made between preparing the mutation and applying it is neither published over by a
    /// relock that lands nor taken for cooldown's own.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_edit_made_after_preparing_is_never_overwritten() -> Result<()> {
        // A stand-in Ruby that writes a lock landing the plan, as a successful relock would.
        let landed = gemfile_lock(&[("i18n", "1.14.5")], &[]);
        let stand_in = StandIn::new(&indoc::formatdoc! {r#"
            #!/bin/sh
            cat > "$4" <<'LOCK'
            {landed}LOCK
        "#})?;
        let mutation =
            PreparedMutation::prepare(&stand_in.tool, &project(&stand_in.root), &stand_in.plan)
                .await?;
        std::fs::write(
            stand_in.root.join("Gemfile"),
            format!("{}gem \"rake\"\n", stand_in.gemfile),
        )?;

        let attempt = stand_in.tool.apply_with_observer(&mutation, &()).await?;

        stand_in.assert_edit_survives(&mutation, attempt);
        Ok(())
    }

    /// Only gems the public rubygems.org remote serves are in the graph, so a same-named gem from a
    /// private remote or a Git source is never planned against the public gem's releases.
    #[tokio::test]
    async fn dependencies_skip_gems_from_other_sources() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
        std::fs::write(
            root.join("Gemfile.lock"),
            indoc! {"
                GIT
                  remote: https://github.com/example/rails.git
                  revision: 0123456789abcdef
                  specs:
                    rails (7.1.0)

                GEM
                  remote: https://gems.example.com/
                  specs:
                    rack (3.0.0)

                GEM
                  remote: https://rubygems.org/
                  specs:
                    rake (13.0.6)

                DEPENDENCIES
                  rack
                  rails!
                  rake
            "},
        )?;
        let cache = tempfile::tempdir()?;
        let tool = BundlerTool::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);

        let graph = tool.dependencies(&project(&root), DepScope::Graph).await?;

        let names: Vec<&str> = graph.iter().map(|dep| dep.package.name.as_str()).collect();
        assert_eq!(names, ["rake"]);
        Ok(())
    }

    #[tokio::test]
    async fn dependencies_split_direct_from_transitive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        std::fs::write(
            root.join("Gemfile.lock"),
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    rake (13.0.6)\n    racc (1.6.0)\n\nDEPENDENCIES\n  rake (~> 13.0)\n",
        )
        .expect("write lock");
        let project = project(&root);
        let cache = tempfile::tempdir().expect("cache");
        let tool = BundlerTool::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );

        let direct = tool
            .dependencies(&project, DepScope::Direct)
            .await
            .expect("direct");
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].package.name, "rake");
        assert_eq!(direct[0].package.registry.as_deref(), Some(RUBYGEMS));

        let graph = tool
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("graph");
        assert_eq!(graph.len(), 2);
    }
}
