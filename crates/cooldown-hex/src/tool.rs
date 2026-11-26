//! The Elixir/Hex [`Tool`](cooldown_core::Tool): detection by `mix.lock`, the resolved
//! graph from that lock, the direct split from `mix.exs`, and hex.pm publish times.
//! The core owns the verdict; `mix` is driven only to apply.

use crate::lock::{self, HexPin};
use crate::registry::{HEXPM, Hex};
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
use std::collections::{BTreeMap, BTreeSet};

/// The [`ToolId`] for the Elixir/Hex adapter (`"hex"`).
pub const HEX_ID: ToolId = ToolId("hex");

/// The Elixir program `apply` relocks a project with, so that each planned dependency lands on
/// exactly its target.
///
/// `mix deps.update` takes no version, and picks the newest release a requirement allows.
/// The program replaces each named app's lock entry with one Hex builds for exactly the target
/// from hex.pm's signed metadata, and then runs one `mix deps.get`, which keeps every locked
/// version the project's requirements still accept.
/// It does so in a scratch directory.
/// Mix reads the project's own `mix.exs` but resolves into the scratch lock, and fetches and
/// builds every dependency in scratch directories, which the environment names and the program
/// checks dependency by dependency.
/// It reads hex.pm's metadata into a fresh registry cache, and installs any tool into a Mix home of
/// its own.
/// A Git dependency fetched during the resolve can still have Mix fetch its own dependencies
/// outside the scratch directory, as a native `mix deps.get` would, but then the program refuses
/// to vouch for the lock.
/// Beside the new lock it leaves the apps whose Hex dependency the project still reaches through
/// exactly that lock entry, since Mix keeps the entry of an app nothing depends on any more, and
/// the apps whose entry changed in a way cooldown cannot judge: added, dropped or changed in any
/// way when it is not a hex.pm release, as Mix decodes the lock before and after.
///
/// Its arguments are the project root, the scratch directory holding the lock to start from and
/// the `entries` file naming every app cooldown read from that lock, and each target as an app
/// name, its Hex package and the version.
/// The user's Hex home and configuration pass through, so private organizations keep their
/// credentials.
/// The environment turns Hex's own cooldown and policy off, and the program exits with
/// [`REFUSED`] when they, or the scratch paths, did not take, when hex.pm is mirrored or replaced
/// or its registry goes unverified, when the project names another lock than `mix.lock` or Mix
/// reads other entries from it than cooldown did, when a path dependency's source is missing, and
/// when the Elixir or Hex it runs is older than the release its behavior needs or lacks a function
/// it calls.
/// Project aliases never stand in for the Mix tasks it runs.
const EXACT_LOCK: &str = indoc! {r#"
    # Relocks a Mix project with each named dependency on exactly cooldown's target.
    [root, scratch | targets] = System.argv()
    scratch = Path.expand(scratch)

    refuse = fn message ->
      IO.puts(:stderr, message)
      System.halt(3)
    end

    # The oldest releases whose behavior the relock relies on, beyond the functions it calls.
    unless Version.match?(System.version(), ">= 1.15.6") do
      refuse.("cooldown needs Elixir >= 1.15.6 to land exact versions, " <>
                "but runs Elixir #{System.version()}")
    end

    unless Code.ensure_loaded?(Hex) do
      refuse.("cooldown needs Hex >= 2.5.2 to land exact versions, but Hex is not installed")
    end

    unless function_exported?(Hex, :version, 0) do
      refuse.("cooldown cannot land exact versions with this Hex, which has no Hex.version/0")
    end

    unless Version.match?(Hex.version(), ">= 2.5.2") do
      refuse.("cooldown needs Hex >= 2.5.2 to land exact versions, " <>
                "but loaded Hex #{Hex.version()}")
    end

    # Every Mix and Hex function the relock calls, at the arity it calls it.
    calls = [
      {"Elixir #{System.version()}",
       [
         {Mix.Project, :in_project, 4},
         {Mix.Project, :config, 0},
         {Mix.Project, :deps_path, 0},
         {Mix.Project, :build_path, 0},
         {Mix.Dep.Lock, :read, 0},
         {Mix.Dep.Lock, :write, 1},
         {Mix.Dep.Converger, :converge, 1},
         {Mix.Task, :run, 2},
         {Mix.Hex, :start, 0}
       ]},
      {"Hex #{Hex.version()}",
       [
         {Hex.State, :fetch!, 1},
         {Hex.Cooldown, :build_cutoff, 1},
         {Hex.Repo, :fetch_repo, 1},
         {Hex.Repo, :default_hexpm_repo, 0},
         {Hex.Registry.Server, :open, 1},
         {Hex.Registry.Server, :prefetch, 1},
         {Hex.Mix, :to_lock, 1}
       ]}
    ]

    for {release, functions} <- calls,
        {module, function, arity} <- functions,
        not (Code.ensure_loaded?(module) and function_exported?(module, function, arity)) do
      refuse.("cooldown cannot land exact versions with #{release}, " <>
                "which has no #{inspect(module)}.#{function}/#{arity}")
    end

    # Mix installs tools such as Rebar into its home, so the relock gets a home of its own.
    # Hex stays loaded, since Mix put the archives on the code path as it started.
    System.put_env("MIX_HOME", Path.join(scratch, "mix"))
    System.put_env("MIX_ARCHIVES", Path.join([scratch, "mix", "archives"]))

    # A project can name another lock, whose pins the new one would then carry.
    own_lock =
      Mix.Project.in_project(:cooldown_project, root, [], fn _ ->
        Path.expand(Mix.Project.config()[:lockfile]) == Path.expand("mix.lock")
      end)

    unless own_lock do
      refuse.("cooldown relocks only mix.lock, but the project resolves another lock")
    end

    lock = Path.join(scratch, "mix.lock")
    in_scratch = fn path ->
      prefix = Path.split(scratch)
      Enum.take(Path.split(Path.expand(path)), length(prefix)) == prefix
    end

    # Mix gives each dependency its checkout and build directory as it loads it, and a
    # dependency's `system_env` can move its children's anywhere.
    # A path dependency is read where it is, so it has to be there, or Mix resolves a smaller
    # graph, as in a copy of the project that left a sibling behind.
    check_dependencies = fn deps ->
      for %Mix.Dep{app: app, scm: scm, opts: opts} <- deps do
        cond do
          scm == Mix.SCM.Path and not File.dir?(opts[:dest]) ->
            refuse.("cooldown needs the source of the path dependency #{app}, which is missing")

          (scm != Mix.SCM.Path and not in_scratch.(opts[:dest])) or
              not in_scratch.(opts[:build]) ->
            refuse.("cooldown relocks in a scratch directory, " <>
                      "but Mix would fetch or build #{app} outside it")

          true ->
            :ok
        end
      end
    end

    # The scratch lock outranks the project's, and no alias stands in for a task.
    Mix.Project.in_project(:cooldown_project, root, [lockfile: lock, aliases: []], fn _ ->
      # Elixir before 1.19 hands `MIX_BUILD_PATH` back as spelled, so every path is expanded first.
      owned = [
        {Path.expand(Mix.Project.config()[:lockfile]), lock},
        {Path.expand(Mix.Project.deps_path()), Path.join(scratch, "deps")},
        {Path.expand(Mix.Project.build_path()), Path.join(scratch, "_build")}
      ]

      unless Enum.all?(owned, fn {effective, expected} -> effective == expected end) do
        refuse.("cooldown relocks in a scratch directory, but Mix would resolve outside it")
      end

      # Hex reads its settings as it starts, so it starts with this project's.
      Mix.Hex.start()

      cooldown = Hex.Cooldown.build_cutoff(Hex.State.fetch!(:cooldown))

      unless cooldown == :disabled and Hex.State.fetch!(:policy) == nil do
        refuse.("cooldown cannot turn off Hex's own cooldown or policy for the relock")
      end

      # A mirror, a repository configured in hex.pm's place, or an unverified registry can serve
      # something else under a public package's name and version.
      {:ok, hexpm} = Hex.Repo.fetch_repo("hexpm")
      canonical = Hex.Repo.default_hexpm_repo()

      verified =
        hexpm.url == canonical.url and hexpm.public_key == canonical.public_key and
          !Hex.State.fetch!(:unsafe_registry) and !Hex.State.fetch!(:no_verify_repo_origin)

      unless verified do
        refuse.("cooldown cannot judge packages served in place of hex.pm or left unverified")
      end

      # Hex keeps decoded registry records across runs, including ones an earlier unverified run
      # cached, so the relock reads hex.pm into a cache of its own.
      Hex.Registry.Server.open(registry_path: Path.join(scratch, "registry.ets"))

      seeds =
        for [app, package, version] <- Enum.chunk_every(targets, 3),
            do: {"hexpm", package, app, version}

      # Mix reads a lock it cannot parse, such as one cut off before its closing brace, as empty,
      # and would then resolve without the entries cooldown judged it by.
      original = Mix.Dep.Lock.read()
      entries = File.read!(Path.join(scratch, "entries")) |> String.split("\n", trim: true)

      unless Enum.sort(entries) == Enum.sort(Enum.map(Map.keys(original), &Atom.to_string/1)) do
        refuse.("cooldown reads other entries from mix.lock than Mix does")
      end

      Hex.Registry.Server.prefetch(for {repo, package, _, _} <- seeds, do: {repo, package})
      Mix.Dep.Lock.write(Map.merge(original, Hex.Mix.to_lock(seeds)))

      # Path dependencies are loaded before anything is fetched, and a Git dependency's children
      # only once it is, so the second pass catches those, though only after Mix fetched them, as
      # a native `mix deps.get` would.
      check_dependencies.(Mix.Dep.Converger.converge([]))
      Mix.Task.run("deps.get", [])
      deps = Mix.Dep.Converger.converge([])
      check_dependencies.(deps)

      # Every environment's dependencies, as `mix deps.unlock --unused` counts them, where only a
      # Hex dependency that uses the entry under its app counts: a path or Git dependency of the
      # same name leaves that entry unused.
      locked = Mix.Dep.Lock.read()

      reached =
        for %Mix.Dep{app: app, scm: Hex.SCM, opts: opts} <- deps,
            {:hex, package, _, _, _, _, repo, _} = entry <- [locked[app]],
            opts[:lock] == entry and to_string(opts[:hex]) == Atom.to_string(package) and
              opts[:repo] == repo,
            do: [Atom.to_string(app), "\n"]

      File.write!(Path.join(scratch, "reached"), reached)

      # cooldown judges a hex.pm release that moves, or that the relock adds or drops, and nothing
      # else, so any other entry has to come out exactly as it went in.
      # An older entry ends at its repository, without the outer checksum Hex adds as it relocks.
      hexpm_package = fn
        {:hex, package, _, _, _, _, "hexpm", _} -> package
        {:hex, package, _, _, _, _, "hexpm"} -> package
        _ -> nil
      end

      judged = fn
        same, same -> true
        nil, to -> hexpm_package.(to) != nil
        from, nil -> hexpm_package.(from) != nil
        from, to -> hexpm_package.(from) != nil and hexpm_package.(from) == hexpm_package.(to)
      end

      planned = for {_, _, app, _} <- seeds, do: String.to_atom(app)

      unjudged =
        for app <- Enum.uniq(Map.keys(original) ++ Map.keys(locked)),
            app not in planned,
            not judged.(original[app], locked[app]),
            do: [Atom.to_string(app), "\n"]

      File.write!(Path.join(scratch, "unjudged"), unjudged)
    end)
"#};

/// The Elixir program `build` fetches a project's dependencies with, exactly as its lock records
/// them.
///
/// It runs Mix's own `deps.get --check-locked`, which fails rather than rewrite the lock the run
/// accepted, in the project its argument names, with no project alias standing in for the task.
const FETCH_LOCKED: &str = indoc! {r#"
    [root] = System.argv()

    # An older Mix ignores `--check-locked` and relocks whatever it would.
    unless Version.match?(System.version(), ">= 1.15.0") do
      IO.puts(:stderr, "cooldown needs Elixir >= 1.15.0 to fetch exactly the locked versions")
      System.halt(3)
    end

    Mix.Project.in_project(:cooldown_project, root, [aliases: []], fn _ ->
      Mix.Task.run("deps.get", ["--check-locked"])
    end)
"#};

/// The status [`EXACT_LOCK`] exits with when it cannot vouch for the lock it would write.
const REFUSED: i32 = 3;

/// The Elixir/Hex implementation of the [`Tool`](cooldown_core::Tool) port.
pub struct HexTool {
    registry: Hex,
    mix: Driver,
}

impl HexTool {
    /// Creates the adapter from a configured [`Hex`] client.
    #[must_use]
    pub fn new(registry: Hex) -> Self {
        HexTool {
            registry,
            mix: Driver::new("mix"),
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`Hex`] client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        HexTool::new(Hex::new(http))
    }

    /// Relocks `project` from the lock `before`, in which cooldown read the entries of `apps`, with
    /// every target on exactly its version through [`EXACT_LOCK`], and returns what it hands back
    /// without writing the project.
    async fn exact_lock(
        &self,
        project: &Project,
        before: &str,
        apps: impl Iterator<Item = &str>,
        targets: &[ExactTarget<'_>],
    ) -> Result<Relocked> {
        let scratch = tempfile::tempdir()?;
        let scratch_path =
            Utf8PathBuf::from_path_buf(scratch.path().to_path_buf()).map_err(|path| {
                CoreError::PathEncoding(format!("{} is not valid UTF-8", path.display()))
            })?;
        let helper = scratch_path.join("exact_lock.exs");
        std::fs::write(&helper, EXACT_LOCK)?;
        std::fs::write(scratch_path.join("mix.lock"), before)?;
        std::fs::write(
            scratch_path.join("entries"),
            apps.collect::<Vec<_>>().join("\n"),
        )?;

        let mut args = vec![
            "run".to_string(),
            "--no-mix-exs".to_string(),
            helper.to_string(),
            project.root.to_string(),
            scratch_path.to_string(),
        ];
        for target in targets {
            args.extend([target.app, target.package, target.version].map(str::to_string));
        }
        // `MIX_EXS` names a file that does not exist, so the Mix command line loads no project of
        // its own before the program loads this one.
        let absent_project = scratch_path.join("mix.exs");
        let deps = scratch_path.join("deps");
        let build = scratch_path.join("_build");
        let env = [
            ("MIX_EXS", absent_project.as_str()),
            ("MIX_DEPS_PATH", deps.as_str()),
            ("MIX_BUILD_PATH", build.as_str()),
            // An empty `HEX_COOLDOWN` falls through to the configured one, while an empty
            // `HEX_POLICY` turns the policy off.
            ("HEX_COOLDOWN", "0d"),
            ("HEX_POLICY", ""),
        ];
        // Mix runs in the project, where a version manager's shim finds the project's Elixir.
        run_exact_lock(&self.mix, &project.root, &args, &env).await?;
        let apps = |file: &str| -> Result<BTreeSet<String>> {
            Ok(std::fs::read_to_string(scratch_path.join(file))?
                .lines()
                .map(str::to_owned)
                .collect())
        };
        Ok(Relocked {
            lock: std::fs::read_to_string(scratch_path.join("mix.lock"))?,
            reached: apps("reached")?,
            unjudged: apps("unjudged")?,
        })
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
        let lock_path = Utf8Path::new("mix.lock");
        // The publish below refuses to write once any file differs from `owned`, so this is the
        // lock the relock starts from.
        let before_text = owned
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
        let before = lock::parse(&before_text);
        let targets = plan
            .changes
            .iter()
            .map(|change| {
                let app = change.package.name.as_str();
                Ok(ExactTarget {
                    app,
                    package: &locked_hexpm(&before, app)?.package,
                    version: change.to.as_str(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        // The whole batch resolves at once, so moves that are only valid together land together,
        // and a conflict fails the batch for the caller to isolate.
        let relocked = self
            .exact_lock(
                project,
                &before_text,
                before.keys().map(String::as_str),
                &targets,
            )
            .await?;
        let collateral = relock_moves(&before, &relocked, &plan.changes)?;
        owned.write_owned(lock_path, relocked.lock.as_bytes())?;

        let mut applied = plan.changes.clone();
        applied.extend(collateral);
        Ok(ApplyReport {
            applied,
            ..ApplyReport::default()
        })
    }
}

/// One planned change as [`EXACT_LOCK`] takes it.
struct ExactTarget<'a> {
    /// The app name the lock keys the dependency by.
    app: &'a str,
    /// The Hex package the app locks.
    package: &'a str,
    /// The version to lock.
    version: &'a str,
}

/// What [`EXACT_LOCK`] hands back.
struct Relocked {
    /// The new lock, as Mix wrote it.
    lock: String,
    /// The apps whose Hex dependency the project reaches through their entry in the new lock, in
    /// any environment.
    reached: BTreeSet<String>,
    /// The apps whose entry the relock added, dropped or changed although it is no hex.pm release
    /// cooldown can judge.
    unjudged: BTreeSet<String>,
}

/// The hex.pm release `lock` holds for `app`.
///
/// # Errors
///
/// Returns [`CoreError::UnacceptableResolve`] when the lock holds no hex.pm release for `app`.
fn locked_hexpm<'a>(lock: &'a BTreeMap<String, Option<HexPin>>, app: &str) -> Result<&'a HexPin> {
    hexpm(lock, app).ok_or_else(|| {
        CoreError::UnacceptableResolve(format!("mix.lock holds no hex.pm release of {app}"))
    })
}

/// The release `lock` holds for `app` when it comes from hex.pm, the one source cooldown judges.
fn hexpm<'a>(lock: &'a BTreeMap<String, Option<HexPin>>, app: &str) -> Option<&'a HexPin> {
    lock.get(app)
        .and_then(Option::as_ref)
        .filter(|pin| pin.is_hexpm())
}

/// Runs [`EXACT_LOCK`] as `mix <args>` in `dir`, with `env` added to the environment.
///
/// A refusal to vouch for the lock is a broken environment rather than a conflict between the
/// planned dependencies, so it is a [`CoreError::System`] that fails the run instead of holding
/// each dependency in turn.
async fn run_exact_lock(
    mix: &Driver,
    dir: &Utf8Path,
    args: &[String],
    env: &[(&str, &str)],
) -> Result<()> {
    let output = mix.output_with_env(dir, args, env).await?;
    if output.status.code() == Some(REFUSED) {
        return Err(CoreError::System(failure_detail(&output)));
    }
    if !output.status.success() {
        return Err(mix.failure(&output));
    }
    Ok(())
}

/// The collateral rows of a relock from the lock `before` to what it `relocked` to land `planned`,
/// sorted by name, so that no dependency's version changes in silence.
///
/// Mix still moves a dependency a target needs moved, such as a child whose locked version the
/// target's requirement excludes.
/// A planned app's own row reports its whole move, and a hex.pm release the relock adds or drops
/// has no version to move from or to.
///
/// # Errors
///
/// Returns [`CoreError::UnacceptableResolve`] when a planned app is not on exactly its target
/// release from hex.pm, or no Hex dependency reaches it through that entry, and when the relock
/// changed an entry that is not a hex.pm release, which cooldown cannot judge.
fn relock_moves(
    before: &BTreeMap<String, Option<HexPin>>,
    relocked: &Relocked,
    planned: &[Change],
) -> Result<Vec<Change>> {
    if let Some(app) = relocked.unjudged.first() {
        return Err(CoreError::UnacceptableResolve(format!(
            "Mix would change {app}, which is not a hex.pm package cooldown can judge"
        )));
    }
    let after = lock::parse(&relocked.lock);
    for change in planned {
        let app = change.package.name.as_str();
        let package = &locked_hexpm(before, app)?.package;
        let landed = hexpm(&after, app);
        let on_target = landed.is_some_and(|pin| {
            pin.package == *package && version::compare(&pin.version, change.to.as_str()).is_eq()
        });
        if !on_target {
            let held = landed.map_or_else(
                || format!("Mix's lock no longer holds {app} from hex.pm"),
                |pin| format!("Mix locked {app} {}, not {}", pin.version, change.to),
            );
            return Err(CoreError::UnacceptableResolve(held));
        }
        // Mix keeps the entry of an app nothing depends on, so a target only lands where a Hex
        // dependency still reaches it.
        if !relocked.reached.contains(app) {
            return Err(CoreError::UnacceptableResolve(format!(
                "no dependency reaches {app} {} any more",
                change.to
            )));
        }
    }

    let mut collateral = Vec::new();
    for app in before.keys() {
        if planned.iter().any(|change| change.package.name == *app) {
            continue;
        }
        let (Some(from), Some(to)) = (hexpm(before, app), hexpm(&after, app)) else {
            continue;
        };
        if from.package == to.package && version::compare(&from.version, &to.version).is_ne() {
            collateral.push(Change {
                package: PackageId::new(HEX_ID, app.clone(), Some(HEXPM.to_string())),
                from: Version::new(from.version.clone()),
                to: Version::new(to.version.clone()),
                // A collateral move is consistency churn, not a declared bump, so its update kind
                // is informational only and `Minor` is the neutral label the renderer shows.
                kind: UpdateKind::Minor,
                downgrade: version::compare(&to.version, &from.version).is_lt(),
                direct: false,
                members: Vec::new(),
            });
        }
    }
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
            is_valid: |value| version::parse(value).is_some(),
            compare: version::compare,
            major_key: version::major_key,
            major_number: version::major_number,
            classify_kind: version::classify_kind,
            classify_quality,
        },
    )
}

/// The hex.pm package `dep` locks, under which its releases are published.
///
/// The dependency is named by its app, which an aliased dependency (`parsec`) names apart from
/// its package (`nimble_parsec`).
/// [`dependencies`](ToolRead::dependencies) records the package as the advisory identity, which
/// only a hex.pm package has.
fn hex_package(dep: &Dependency) -> Result<PackageId> {
    let package = dep.advisory_identity.as_deref().ok_or_else(|| {
        CoreError::NotFound(format!("{} is not a hex.pm package", dep.package.name))
    })?;
    Ok(PackageId::new(HEX_ID, package, Some(HEXPM.to_string())))
}

#[async_trait]
impl ToolRead for HexTool {
    fn version_command(&self) -> Option<cooldown_core::trace::ToolVersionCommand> {
        Some(cooldown_core::trace::ToolVersionCommand {
            program: self.mix.program().to_string(),
            arguments: vec!["--version".into()],
        })
    }

    fn evidence(&self, project: &Project) -> Result<cooldown_core::trace::Evidence> {
        use cooldown_core::trace::GraphCoverage;
        let mut evidence = cooldown_adapter_util::evidence::native(
            project,
            "mix.lock",
            GraphCoverage::Resolved,
            &["config/config.exs", "config/runtime.exs"],
        );
        cooldown_adapter_util::evidence::ancestors(&mut evidence, &project.root, &[]);
        cooldown_adapter_util::evidence::environment(
            &mut evidence,
            &["HEX_HOME", "HEX_MIRROR", "MIX_ENV", "MIX_HOME"],
        );
        Ok(evidence)
    }

    fn id(&self) -> ToolId {
        HEX_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: true,
            artifact_granular: false,
            advisory_ecosystem: Some("Hex"),
            honors_generated_members: false,
        }
    }

    fn project_marker(&self) -> cooldown_core::ProjectMarker {
        ProjectMarker {
            marker: "mix.lock",
            manifest: "mix.exs",
            alternate_manifests: &[],
            workspace_root: false,
        }
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        version::classify_kind(from, to)
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let content = std::fs::read_to_string(project.root.join("mix.lock"))?;
        let manifest = std::fs::read_to_string(&project.manifest).unwrap_or_default();
        let direct = lock::parse_direct(&manifest);
        let ceilings = lock::graph_ceilings(&content);

        let mut deps = Vec::new();
        for (app, entry) in lock::parse(&content) {
            // Only a hex.pm release is the package whose releases cooldown reads and OSV's `Hex`
            // ecosystem describes.
            // A private repository's package, or a Git or path source, merely shares the name, so
            // it is never planned, while Mix still honors its requirements when it relocks.
            let Some(pin) = entry.filter(HexPin::is_hexpm) else {
                continue;
            };
            let is_direct = direct.contains(&app);
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            deps.push(Dependency {
                package: PackageId::new(HEX_ID, app.clone(), Some(HEXPM.to_string())),
                advisory_identity: Some(pin.package),
                current: Version::new(pin.version.clone()),
                current_quality: classify_quality(&pin.version),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                // A requirer pinning this dep `== X` caps it at its resolved version. The active
                // check (pin equals the resolved version) records the canonical resolved form so it
                // matches a fetched release, mirroring the uv adapter.
                graph_ceiling: ceilings.get(&app).and_then(|ceiling| {
                    version::compare(ceiling, &pin.version)
                        .is_eq()
                        .then(|| Version::new(pin.version.clone()))
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
        Ok(verify_current_unknown("mix.lock"))
    }
}

#[async_trait]
impl ReleaseFetcher for HexTool {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.registry.releases(&hex_package(dep)?).await?;
        Ok(build_releases(dep.current.as_str(), raw))
    }

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
        let time = self
            .registry
            .published_at(&hex_package(dep)?, &dep.current, &[])
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

    // A dependency is named by its app, and which package an app locks is up to each project, so
    // one project's `parsec` can be another's `nimble_parsec`.
    fn releases_are_project_scoped(&self) -> bool {
        true
    }
}

#[async_trait]
impl ToolWrite for HexTool {
    fn mutation_tool(&self) -> ToolId {
        HEX_ID
    }

    fn supports_transitive_advance(&self) -> bool {
        // `EXACT_LOCK` pins any locked app, declared or not.
        true
    }

    fn resolve_inputs(&self) -> ResolveInputs {
        // `mix deps.get`/`deps.update` COMPILES `mix.exs` (Elixir) to resolve, and a project's
        // `mix.exs` frequently reads sibling source (a `@version` from a module, `Code.require_file`,
        // umbrella `apps/*/mix.exs`), so the throwaway probe copy must carry `.ex`/`.exs` source.
        ResolveInputs {
            source_extensions: &["ex", "exs"],
            ..ResolveInputs::DEFAULT
        }
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        // The relock reads the requirements of an umbrella's member apps too, which Mix keeps
        // under `apps/` unless the umbrella names another directory.
        let mut files = vec![Utf8PathBuf::from("mix.exs"), Utf8PathBuf::from("mix.lock")];
        let apps = project.root.join("apps");
        if apps.is_dir() {
            for member in apps.read_dir_utf8()? {
                let manifest = Utf8Path::new("apps")
                    .join(member?.file_name())
                    .join("mix.exs");
                if project.root.join(&manifest).is_file() {
                    files.push(manifest);
                }
            }
        }
        ProjectMutationJournal::capture(&project.root, files)
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
        let scratch = tempfile::tempdir()?;
        let scratch_path =
            Utf8PathBuf::from_path_buf(scratch.path().to_path_buf()).map_err(|path| {
                CoreError::PathEncoding(format!("{} is not valid UTF-8", path.display()))
            })?;
        let helper = scratch_path.join("fetch_locked.exs");
        std::fs::write(&helper, FETCH_LOCKED)?;
        let args = [
            "run".to_string(),
            "--no-mix-exs".to_string(),
            helper.to_string(),
            project.root.to_string(),
        ];
        // As in the relock, the Mix command line loads no project of its own.
        let absent_project = scratch_path.join("mix.exs");
        let output = self
            .mix
            .output_with_env(
                &project.root,
                &args,
                &[("MIX_EXS", absent_project.as_str())],
            )
            .await?;
        Ok(VerifyReport {
            ok: output.status.success(),
            detail: if output.status.success() {
                "mix deps.get --check-locked succeeded".to_string()
            } else {
                failure_detail(&output)
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::formatdoc;

    /// The Hex project rooted at `root`.
    fn project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            kind: HEX_ID,
            manifest: root.join("mix.exs"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        }
    }

    fn tool() -> Result<(tempfile::TempDir, HexTool)> {
        let cache = tempfile::tempdir()?;
        let tool = HexTool::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        Ok((cache, tool))
    }

    #[test]
    fn advisory_ecosystem_matches_osv() -> Result<()> {
        let (_cache, tool) = tool()?;
        // The apply only relocks in a scratch directory, so it takes no environment turn, and it
        // pins transitives too.
        assert!(!tool.mutation_installs());
        assert!(tool.supports_transitive_advance());
        assert_eq!(tool.capabilities().advisory_ecosystem, Some("Hex"));
        Ok(())
    }

    /// A `mix.lock` entry for `app` locking `package` at `version` from `repo`.
    fn entry(app: &str, package: &str, version: &str, repo: &str) -> String {
        format!(
            r#"  "{app}": {{:hex, :{package}, "{version}", "in", [:mix], [], "{repo}", "out"}},"#
        )
    }

    /// A `mix.lock` holding `entries`.
    fn mix_lock(entries: &[String]) -> String {
        formatdoc! {"
            %{{
            {entries}
            }}
        ", entries = entries.join("\n")}
    }

    #[tokio::test]
    async fn dependencies_split_uses_mix_exs() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
        std::fs::write(
            root.join("mix.lock"),
            mix_lock(&[
                entry("jason", "jason", "1.4.0", "hexpm"),
                entry("mime", "mime", "1.6.0", "hexpm"),
            ]),
        )?;
        std::fs::write(
            root.join("mix.exs"),
            "defp deps do\n  [\n    {:jason, \"~> 1.4\"}\n  ]\nend\n",
        )?;
        let (_cache, tool) = tool()?;

        let direct = tool.dependencies(&project(&root), DepScope::Direct).await?;
        let graph = tool.dependencies(&project(&root), DepScope::Graph).await?;

        let names: Vec<&str> = direct.iter().map(|dep| dep.package.name.as_str()).collect();
        assert_eq!(names, ["jason"]);
        assert_eq!(graph.len(), 2);
        Ok(())
    }

    /// Only hex.pm releases are in the graph, each named by its app and looked up under its
    /// package, so a same-named package from a private repository, an old-format entry that names
    /// no repository, and a Git checkout are never planned by the public package's releases.
    #[tokio::test]
    async fn dependencies_plan_only_hexpm_releases_under_their_app_name() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
        std::fs::write(
            root.join("mix.lock"),
            mix_lock(&[
                entry("jason", "jason", "1.4.0", "hexpm:acme"),
                r#"  "legacy": {:hex, :legacy, "0.2.0", "inner", [:mix], []},"#.to_owned(),
                r#"  "local": {:git, "https://example.com/local.git", "0123abc", []},"#.to_owned(),
                entry("parsec", "nimble_parsec", "1.2.3", "hexpm"),
            ]),
        )?;
        let (_cache, tool) = tool()?;

        let graph = tool.dependencies(&project(&root), DepScope::Graph).await?;

        let identities: Vec<(&str, Option<&str>)> = graph
            .iter()
            .map(|dep| (dep.package.name.as_str(), dep.advisory_identity.as_deref()))
            .collect();
        assert_eq!(identities, [("parsec", Some("nimble_parsec"))]);
        assert_eq!(hex_package(&graph[0])?.name, "nimble_parsec");
        Ok(())
    }

    /// The planned move of `app` from `from` to `to`.
    fn change(app: &str, from: &str, to: &str) -> Change {
        Change {
            package: PackageId::new(HEX_ID, app.to_string(), Some(HEXPM.to_string())),
            from: Version::new(from.to_string()),
            to: Version::new(to.to_string()),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        }
    }

    /// The apps `apps` as [`Relocked`] lists them.
    fn apps(apps: &[&str]) -> BTreeSet<String> {
        apps.iter().map(|app| (*app).to_owned()).collect()
    }

    /// A relock to the lock holding `entries`, reaching the apps `reached`, that changed nothing
    /// cooldown cannot judge.
    fn relocked(entries: &[String], reached: &[&str]) -> Relocked {
        Relocked {
            lock: mix_lock(entries),
            reached: apps(reached),
            unjudged: BTreeSet::new(),
        }
    }

    /// An app the relock moves beyond the plan is a collateral row, while a planned app's move, a
    /// hex.pm release the relock adds or drops, and an unmoved app are not.
    #[test]
    fn relock_moves_report_only_unplanned_moves() -> Result<()> {
        let before = lock::parse(&mix_lock(&[
            entry("makeup", "makeup", "1.1.0", "hexpm"),
            entry("nimble_parsec", "nimble_parsec", "1.4.1", "hexpm"),
            entry("jason", "jason", "1.4.0", "hexpm"),
            entry("mime", "mime", "2.0.6", "hexpm"),
        ]));
        let after = relocked(
            &[
                entry("makeup", "makeup", "1.2.1", "hexpm"),
                entry("nimble_parsec", "nimble_parsec", "1.4.0", "hexpm"),
                entry("jason", "jason", "1.4.0", "hexpm"),
                entry("telemetry", "telemetry", "1.2.1", "hexpm"),
            ],
            &["makeup", "nimble_parsec", "jason", "telemetry"],
        );

        let collateral = relock_moves(&before, &after, &[change("makeup", "1.1.0", "1.2.1")])?;

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
        assert_eq!(moves, [("nimble_parsec", "1.4.1", "1.4.0", true)]);
        Ok(())
    }

    /// A relock that lands its target fails when Mix reports changing an entry that is not a
    /// hex.pm release, since cooldown cannot judge that release.
    #[test]
    fn relock_moves_refuse_changing_what_cooldown_cannot_judge() {
        let before = lock::parse(&mix_lock(&[entry("makeup", "makeup", "1.1.0", "hexpm")]));
        let landed = Relocked {
            unjudged: apps(&["local"]),
            ..relocked(&[entry("makeup", "makeup", "1.1.2", "hexpm")], &["makeup"])
        };

        match relock_moves(&before, &landed, &[change("makeup", "1.1.0", "1.1.2")]) {
            Err(CoreError::UnacceptableResolve(detail)) => {
                assert!(detail.contains("Mix would change local"), "{detail}");
            }
            other => panic!("changing local must fail, got {other:?}"),
        }
    }

    /// A relock that leaves a planned app off its target, locks another package or repository
    /// under its name, drops it, or keeps it only as an entry no Hex dependency reaches fails.
    #[test]
    fn relock_moves_refuse_a_target_that_did_not_land() {
        let planned = [change("parsec", "1.2.3", "1.4.0")];
        let before = lock::parse(&mix_lock(&[entry(
            "parsec",
            "nimble_parsec",
            "1.2.3",
            "hexpm",
        )]));
        let landed = entry("parsec", "nimble_parsec", "1.4.0", "hexpm");
        let cases = [
            (
                entry("parsec", "nimble_parsec", "1.4.2", "hexpm"),
                &["parsec"][..],
            ),
            (entry("parsec", "parsec", "1.4.0", "hexpm"), &["parsec"][..]),
            (
                entry("parsec", "nimble_parsec", "1.4.0", "hexpm:acme"),
                &["parsec"][..],
            ),
            (entry("other", "other", "1.0.0", "hexpm"), &["other"][..]),
            (landed.clone(), &[][..]),
        ];
        for (after, reached) in cases {
            assert!(
                matches!(
                    relock_moves(
                        &before,
                        &relocked(std::slice::from_ref(&after), reached),
                        &planned
                    ),
                    Err(CoreError::UnacceptableResolve(_))
                ),
                "{after} reached by {reached:?}"
            );
        }
        assert!(relock_moves(&before, &relocked(&[landed], &["parsec"]), &planned).is_ok());
    }

    /// An umbrella Hex project with one member app, whose relock runs a stand-in `mix` script,
    /// planned to move its `makeup` from 1.1.0 to 1.1.2.
    #[cfg(unix)]
    struct StandIn {
        _dirs: [tempfile::TempDir; 2],
        root: Utf8PathBuf,
        manifest: &'static str,
        lock: String,
        tool: HexTool,
        plan: Plan,
    }

    #[cfg(unix)]
    impl StandIn {
        fn new(script: &str) -> Result<Self> {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir()?;
            let root = Utf8PathBuf::from_path_buf(std::fs::canonicalize(dir.path())?)
                .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
            let manifest = "defp deps do\n  [\n    {:makeup, \"~> 1.0\"}\n  ]\nend\n";
            let lock = mix_lock(&[entry("makeup", "makeup", "1.1.0", "hexpm")]);
            std::fs::write(root.join("mix.exs"), manifest)?;
            std::fs::write(root.join("mix.lock"), &lock)?;
            std::fs::create_dir_all(root.join("apps/child"))?;
            std::fs::write(root.join("apps/child/mix.exs"), manifest)?;
            let mix = root.join("mix");
            std::fs::write(&mix, script)?;
            std::fs::set_permissions(&mix, std::fs::Permissions::from_mode(0o755))?;
            let (cache, tool) = tool()?;
            let tool = HexTool {
                mix: Driver::from_program(mix.as_str()),
                ..tool
            };
            let plan = Plan {
                changes: vec![change("makeup", "1.1.0", "1.1.2")],
                ..Plan::default()
            };
            Ok(Self {
                _dirs: [dir, cache],
                root,
                manifest,
                lock,
                tool,
                plan,
            })
        }

        /// Prepares the planned mutation.
        async fn prepare(&self) -> Result<PreparedMutation> {
            PreparedMutation::prepare(&self.tool, &project(&self.root), &self.plan).await
        }

        /// Asserts that `attempt` failed and handed back the `mix.exs` as it was prepared, so that
        /// the rollback recovery runs refuses to overwrite the edit since made to it.
        fn assert_edit_survives(&self, mutation: &PreparedMutation, attempt: ApplyAttempt) {
            let Ok(cooldown_core::ApplyAttemptOutcome::Finished { report, postimage }) =
                attempt.into_outcome_for(mutation)
            else {
                panic!("the apply must finish");
            };
            assert!(report.is_err(), "{report:?}");
            let postimage_manifest = postimage
                .files()
                .iter()
                .find(|file| file.path() == Utf8Path::new("mix.exs"))
                .and_then(ProjectMutationFile::contents);
            assert_eq!(postimage_manifest, Some(self.manifest.as_bytes()));
            assert!(matches!(
                mutation.journal().restore_if_unchanged(&postimage),
                Err(CoreError::LockConflict(_))
            ));
            let read = |file: &str| std::fs::read_to_string(self.root.join(file)).ok();
            assert!(read("mix.exs").is_some_and(|manifest| manifest.contains("jason")));
            assert_eq!(read("mix.lock"), Some(self.lock.clone()));
        }
    }

    /// When Mix fails while someone else edits `mix.exs`, the apply hands back the state it was
    /// prepared with, so rolling it back refuses to overwrite the edit.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_relock_never_claims_a_concurrent_edit() -> Result<()> {
        // A stand-in Mix that edits `mix.exs` in the project it runs in, as an editor saving it
        // meanwhile would, and then fails like a conflicting resolve.
        let stand_in = StandIn::new(indoc! {r#"
            #!/bin/sh
            echo '# {:jason, "~> 1.4"}' >> mix.exs
            exit 1
        "#})?;
        let mutation = stand_in.prepare().await?;

        let attempt = stand_in.tool.apply_with_observer(&mutation, &()).await?;

        stand_in.assert_edit_survives(&mutation, attempt);
        Ok(())
    }

    /// An edit made between preparing the mutation and applying it is neither published over by a
    /// relock that lands nor taken for cooldown's own.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_edit_made_after_preparing_is_never_overwritten() -> Result<()> {
        let stand_in = StandIn::new(&landing(""))?;
        let mutation = stand_in.prepare().await?;
        std::fs::write(
            stand_in.root.join("mix.exs"),
            format!("{}# {{:jason, \"~> 1.4\"}}\n", stand_in.manifest),
        )?;

        let attempt = stand_in.tool.apply_with_observer(&mutation, &()).await?;

        stand_in.assert_edit_survives(&mutation, attempt);
        Ok(())
    }

    /// A stand-in Mix script that runs `edit` in the project and then hands back a lock landing the
    /// plan, as a successful relock would, into the scratch directory, its fifth argument.
    #[cfg(unix)]
    fn landing(edit: &str) -> String {
        let landed = mix_lock(&[entry("makeup", "makeup", "1.1.2", "hexpm")]);
        formatdoc! {r#"
            #!/bin/sh
            {edit}
            cat > "$5/mix.lock" <<'LOCK'
            {landed}LOCK
            echo makeup > "$5/reached"
            : > "$5/unjudged"
        "#}
    }

    /// An edit to an umbrella member's `mix.exs` while Mix relocks, whose requirements the relock
    /// may have read before or after it, is never published over.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_umbrella_member_edited_during_the_relock_is_never_published_over() -> Result<()> {
        let stand_in = StandIn::new(&landing(
            r#"echo '# {:jason, "~> 1.4"}' >> apps/child/mix.exs"#,
        ))?;
        let mutation = stand_in.prepare().await?;

        let report = stand_in.tool.apply(&mutation).await;

        assert!(
            matches!(report, Err(CoreError::LockConflict(_))),
            "{report:?}"
        );
        let read = |file: &str| std::fs::read_to_string(stand_in.root.join(file)).ok();
        assert_eq!(read("mix.lock"), Some(stand_in.lock.clone()));
        assert!(read("apps/child/mix.exs").is_some_and(|manifest| manifest.contains("jason")));
        Ok(())
    }

    /// A refusal to vouch for the lock is a broken environment, never a conflict to isolate
    /// package by package, and the lock stays as it was.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_relock_fails_the_run() -> Result<()> {
        let stand_in = StandIn::new(indoc! {r#"
            #!/bin/sh
            echo "cooldown cannot judge packages served in place of hex.pm" >&2
            exit 3
        "#})?;
        let mutation = stand_in.prepare().await?;

        let report = stand_in.tool.apply(&mutation).await;

        match &report {
            Err(CoreError::System(detail)) => assert!(detail.contains("in place of hex.pm")),
            other => panic!("the refusal must fail the run, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(stand_in.root.join("mix.lock"))?,
            stand_in.lock
        );
        Ok(())
    }
}
