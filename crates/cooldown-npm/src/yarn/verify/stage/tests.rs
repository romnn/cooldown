use super::{Entry, Materialized, Merge, Plan, Planned, merge, plan};
use crate::lock::yarn_berry_fixtures::{EVERY_SOURCE, ProjectFile, SCHEMA_FILES, SCHEMAS};
use crate::manifest::workspaces::YarnEnvironment;
use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre;
use std::collections::BTreeSet;

/// A temporary directory standing in for the filesystem around a project: a home directory, the
/// places a project may sit, and the scratch bases a copy may be created in.
struct World {
    /// Owns the temporary directory; dropping it deletes the world, so it stays bound.
    _guard: tempfile::TempDir,
    top: Utf8PathBuf,
    home: Utf8PathBuf,
}

fn new_world() -> eyre::Result<World> {
    let guard = tempfile::tempdir()?;
    let top = Utf8PathBuf::from_path_buf(dunce::canonicalize(guard.path())?)
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    let home = top.join("home");
    std::fs::create_dir_all(&home)?;
    Ok(World {
        _guard: guard,
        top,
        home,
    })
}

impl World {
    fn environment(&self) -> YarnEnvironment {
        YarnEnvironment {
            home: Some(self.home.clone()),
            ..YarnEnvironment::default()
        }
    }

    /// A scratch base outside the home directory.
    fn outside_base(&self) -> Utf8PathBuf {
        self.top.join("tmp")
    }

    /// A scratch base inside the home directory, as cooldown's cache directory is.
    fn inside_base(&self) -> Utf8PathBuf {
        self.home.join(".cache").join("yarn-verify")
    }

    /// Both bases, the outside one first, as the adapter orders them.
    fn bases(&self) -> Vec<Utf8PathBuf> {
        vec![self.outside_base(), self.inside_base()]
    }

    /// The [`SCHEMA_FILES`] workspace with its Yarn 4 lock at `relative` below the world.
    fn project(&self, relative: &str) -> eyre::Result<Utf8PathBuf> {
        let root = self.top.join(relative);
        let lock = SCHEMAS
            .last()
            .expect("the schema fixtures hold a lock")
            .lock;
        for ProjectFile { path, contents } in SCHEMA_FILES {
            write(&root.join(path), contents)?;
        }
        write(&root.join("yarn.lock"), lock)?;
        Ok(root)
    }

    fn planned(&self, root: &Utf8Path) -> eyre::Result<Planned> {
        planned_in(root, &self.environment(), &self.bases())
    }

    fn ready(&self, root: &Utf8Path) -> eyre::Result<Plan> {
        match self.planned(root)? {
            Planned::Ready(plan) => Ok(plan),
            Planned::Declined(reason) => Err(eyre::eyre!("declined: {reason}")),
        }
    }

    fn declined(&self, root: &Utf8Path) -> eyre::Result<String> {
        declined(self.planned(root)?)
    }
}

fn planned_in(
    root: &Utf8Path,
    environment: &YarnEnvironment,
    bases: &[Utf8PathBuf],
) -> eyre::Result<Planned> {
    let lock = std::fs::read_to_string(root.join("yarn.lock"))?;
    Ok(plan(root, &lock, environment, bases)?)
}

fn declined(planned: Planned) -> eyre::Result<String> {
    match planned {
        Planned::Declined(reason) => Ok(reason),
        Planned::Ready(plan) => Err(eyre::eyre!("expected a decline, planned {plan:?}")),
    }
}

fn write(path: &Utf8Path, contents: &str) -> eyre::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}

/// The plan's entries below the project, relative to it and `/`-separated.
fn staged(plan: &Plan) -> BTreeSet<String> {
    let Ok(prefix) = plan.root.strip_prefix(&plan.boundary) else {
        return BTreeSet::new();
    };
    plan.entries
        .keys()
        .filter_map(|relative| relative.strip_prefix(prefix).ok())
        .map(|relative| relative.as_str().replace('\\', "/"))
        .collect()
}

fn assert_mentions(reason: &str, expected: &str) {
    assert!(reason.contains(expected), "{reason:?} lacks {expected:?}");
}

/// Every manifest outside the directories Yarn's glob ignores is copied, wherever it sits, and
/// nothing inside them is: a superset of the workspaces, whatever the patterns say.
#[test]
fn every_manifest_outside_ignored_directories_is_copied() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("project")?;
    for nested in [
        "packages/vendor/v/package.json",
        "testdata/t/package.json",
        "target/x/package.json",
        ".hidden/h/package.json",
        "node_modules/chalk/package.json",
        "packages/a/node_modules/dep/package.json",
        ".git/p/package.json",
        "packages/a/.yarn/cache/package.json",
    ] {
        write(&root.join(nested), "{}")?;
    }
    write(&root.join("packages/a/index.js"), "")?;

    let plan = world.ready(&root)?;

    assert_eq!(
        staged(&plan),
        [
            ".hidden/h/package.json",
            "package.json",
            "packages/a/package.json",
            "packages/b/package.json",
            "packages/vendor/v/package.json",
            "target/x/package.json",
            "testdata/t/package.json",
            "yarn.lock",
        ]
        .map(String::from)
        .into()
    );
    Ok(())
}

/// The project's environment file, Yarn releases, and patches are copied, and a `yarnPath` a
/// copied rc file names outside `.yarn/releases` too.
#[test]
fn yarn_folders_the_environment_file_and_a_relative_yarn_path_are_copied() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("project")?;
    write(&root.join(".env.yarn"), "FOO=1\n")?;
    write(&root.join(".yarn/releases/yarn-4.18.1.cjs"), "// yarn")?;
    write(&root.join(".yarn/patches/chalk.patch"), "--- a\n")?;
    write(&root.join(".yarn/cache/chalk.zip"), "zip")?;
    write(&root.join("tools/yarn.cjs"), "// yarn")?;
    write(&root.join(".yarnrc.yml"), "yarnPath: tools/yarn.cjs\n")?;

    let plan = world.ready(&root)?;
    let staged = staged(&plan);

    for copied in [
        ".env.yarn",
        ".yarn/releases/yarn-4.18.1.cjs",
        ".yarn/patches/chalk.patch",
        ".yarnrc.yml",
        "tools/yarn.cjs",
    ] {
        assert!(staged.contains(copied), "{copied} missing from {staged:?}");
    }
    assert!(!staged.contains(".yarn/cache/chalk.zip"), "{staged:?}");
    Ok(())
}

/// Under the home directory, the rc files between the home directory and the project are copied
/// to their mirrored positions, while the home directory's own rc files are read in place.
#[test]
fn ancestor_rc_files_are_mirrored_below_the_home_directory() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("home/work/project")?;
    write(&world.home.join("work/.yarnrc.yml"), "nodeLinker: pnp\n")?;
    write(&world.home.join(".yarnrc.yml"), "enableTelemetry: false\n")?;

    let plan = world.ready(&root)?;

    assert_eq!(plan.boundary, world.home);
    assert!(plan.base.starts_with(&world.home), "{}", plan.base);
    assert!(
        plan.entries.contains_key(Utf8Path::new("work/.yarnrc.yml")),
        "{:?}",
        plan.entries.keys()
    );
    assert!(plan.in_place.contains_key(&world.home.join(".yarnrc.yml")));
    assert!(!plan.entries.contains_key(Utf8Path::new(".yarnrc.yml")));
    Ok(())
}

/// Outside the home directory, an rc file in a directory the scratch base shares is read in
/// place, and one in a directory only the project has is mirrored.
#[test]
fn ancestor_rc_files_are_shared_or_mirrored_outside_the_home_directory() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("a/b/project")?;
    write(&world.top.join("a/.yarnrc.yml"), "nodeLinker: pnp\n")?;
    write(&world.top.join(".yarnrc.yml"), "enableTelemetry: false\n")?;

    let plan = world.ready(&root)?;

    assert_eq!(plan.base, world.outside_base());
    let mirrored = world.top.join("a/.yarnrc.yml");
    let mirrored = mirrored.strip_prefix(&plan.boundary)?;
    assert!(
        plan.entries.contains_key(mirrored),
        "{:?}",
        plan.entries.keys()
    );
    assert!(plan.in_place.contains_key(&world.top.join(".yarnrc.yml")));
    Ok(())
}

/// A `file:` folder is copied whole, its `node_modules` and modes included, and its own local
/// dependencies are followed; a `portal:` target contributes only its manifest, whose own
/// dependencies are followed too.
#[test]
fn local_packages_are_copied_as_yarn_reads_them() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("project")?;
    write(
        &root.join("packages/a/package.json"),
        r#"{"name":"pkg-a","dependencies":{"local":"file:../../local","portal":"portal:../../portal","bare":"../../bare","archive":"file:../../pkg.tgz"}}"#,
    )?;
    write(
        &root.join("local/package.json"),
        r#"{"name":"local","dependencies":{"inner":"file:../inner"}}"#,
    )?;
    write(&root.join("local/index.js"), "module.exports = 1;\n")?;
    write(&root.join("local/node_modules/dep/index.js"), "")?;
    std::fs::create_dir_all(root.join("local/empty"))?;
    write(&root.join("inner/package.json"), r#"{"name":"inner"}"#)?;
    write(&root.join("inner/lib.js"), "")?;
    write(
        &root.join("portal/package.json"),
        r#"{"name":"portal","dependencies":{"deep":"file:../deep.tgz"}}"#,
    )?;
    write(&root.join("portal/index.js"), "")?;
    write(&root.join("deep.tgz"), "tgz")?;
    write(&root.join("bare/package.json"), r#"{"name":"bare"}"#)?;
    write(&root.join("bare/index.js"), "")?;
    write(&root.join("pkg.tgz"), "tgz")?;

    let plan = world.ready(&root)?;
    let staged = staged(&plan);

    for copied in [
        "local",
        "local/empty",
        "local/index.js",
        "local/node_modules/dep/index.js",
        "inner/lib.js",
        "portal/package.json",
        "deep.tgz",
        "bare/index.js",
        "pkg.tgz",
    ] {
        assert!(staged.contains(copied), "{copied} missing from {staged:?}");
    }
    assert!(!staged.contains("portal/index.js"), "{staged:?}");
    let local = root.join("local");
    assert!(plan.folders.contains(&local), "{:?}", plan.folders);
    assert!(matches!(
        plan.entries.get(local.strip_prefix(&plan.boundary)?),
        Some(Entry::Directory { .. })
    ));
    Ok(())
}

/// A `patch:` range's patch files are copied, relative to the declaring package or, with `~/`,
/// to the project, flags stripped and Yarn's builtin patches skipped; its source range is
/// followed too.
#[test]
fn patch_files_are_copied() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("project")?;
    write(
        &root.join("packages/a/package.json"),
        r#"{"name":"pkg-a","dependencies":{"chalk":"patch:chalk@npm%3A4.1.2#./fix.patch&optional!~/patches/root.patch&optional!builtin<compat/fsevents>","local":"patch:local@file%3A../../local#../../patches/local.patch"}}"#,
    )?;
    write(&root.join("packages/a/fix.patch"), "--- a\n")?;
    write(&root.join("patches/root.patch"), "--- a\n")?;
    write(&root.join("patches/local.patch"), "--- a\n")?;
    write(&root.join("local/package.json"), r#"{"name":"local"}"#)?;

    let staged = staged(&world.ready(&root)?);

    for copied in [
        "packages/a/fix.patch",
        "patches/root.patch",
        "patches/local.patch",
        "local/package.json",
    ] {
        assert!(staged.contains(copied), "{copied} missing from {staged:?}");
    }
    Ok(())
}

/// An `exec:` package in the lock declines, since its generator may read anything.
#[test]
fn an_exec_package_in_the_lock_declines() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.top.join("project");
    for ProjectFile { path, contents } in EVERY_SOURCE.files {
        write(&root.join(path), contents)?;
    }
    write(&root.join("yarn.lock"), EVERY_SOURCE.lock)?;

    assert_mentions(
        &world.declined(&root)?,
        "yarn.lock records the `exec:` package",
    );
    Ok(())
}

/// An `exec:` range in any manifest declines, in a dependency field or in `resolutions`, and in
/// a local package's manifest as much as a workspace's.
#[test]
fn an_exec_range_in_a_manifest_declines() -> eyre::Result<()> {
    for (path, manifest) in [
        (
            "packages/a/package.json",
            r#"{"name":"pkg-a","devDependencies":{"gen":"exec:./gen.js"}}"#,
        ),
        (
            "package.json",
            r#"{"name":"berry-root","private":true,"workspaces":["packages/*"],"resolutions":{"chalk":"Exec:./gen.js"}}"#,
        ),
    ] {
        let world = new_world()?;
        let root = world.project("project")?;
        write(&root.join(path), manifest)?;
        assert_mentions(&world.declined(&root)?, "declares the `exec:` range");
    }

    let world = new_world()?;
    let root = world.project("project")?;
    write(
        &root.join("packages/a/package.json"),
        r#"{"name":"pkg-a","dependencies":{"portal":"portal:../../portal"}}"#,
    )?;
    write(
        &root.join("portal/package.json"),
        r#"{"name":"portal","dependencies":{"gen":"exec:./gen.js"}}"#,
    )?;
    assert_mentions(
        &world.declined(&root)?,
        "portal/package.json declares the `exec:`",
    );
    Ok(())
}

/// A workspace pattern with a parent step or an absolute path declines, in the root manifest or
/// in a nested workspace's.
#[test]
fn a_workspace_pattern_that_may_leave_the_project_declines() -> eyre::Result<()> {
    for (path, manifest) in [
        (
            "package.json",
            r#"{"name":"berry-root","private":true,"workspaces":["packages/*","../shared/*"]}"#,
        ),
        (
            "package.json",
            r#"{"name":"berry-root","private":true,"workspaces":{"packages":["/abs/*"]}}"#,
        ),
        (
            "packages/a/package.json",
            r#"{"name":"pkg-a","private":true,"workspaces":["sub/../../../x"]}"#,
        ),
    ] {
        let world = new_world()?;
        let root = world.project("project")?;
        write(&root.join(path), manifest)?;
        assert_mentions(&world.declined(&root)?, "declares the workspace pattern");
    }
    Ok(())
}

/// A local package named by an absolute path, or by a path leaving the boundary, declines.
#[test]
fn a_local_package_the_copy_cannot_mirror_declines() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("project")?;
    let absolute = serde_json::to_string(&format!("portal:{}", world.top.join("elsewhere")))?;
    write(
        &root.join("packages/a/package.json"),
        &format!(r#"{{"name":"pkg-a","dependencies":{{"x":{absolute}}}}}"#),
    )?;
    assert_mentions(&world.declined(&root)?, "by an absolute path");

    let world = new_world()?;
    let root = world.project("home/project")?;
    write(
        &root.join("packages/a/package.json"),
        r#"{"name":"pkg-a","dependencies":{"x":"file:../../../../outside"}}"#,
    )?;
    assert_mentions(
        &world.declined(&root)?,
        "below which the verification copy mirrors",
    );
    Ok(())
}

/// A project that is the home directory itself, or without any home directory, declines.
#[test]
fn a_project_without_a_distinct_home_directory_declines() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("home")?;
    assert_mentions(&world.declined(&root)?, "the home directory itself");

    let root = world.project("project")?;
    let homeless = YarnEnvironment::default();
    assert_mentions(
        &declined(planned_in(&root, &homeless, &world.bases())?)?,
        "no home directory is established",
    );
    Ok(())
}

/// No base with the project's relation to the home directory declines, and so does a base whose
/// unshared ancestors hold an rc file under any name Yarn reads; another base is used instead
/// when one qualifies.
#[test]
fn a_scratch_base_that_changes_what_yarn_reads_is_not_used() -> eyre::Result<()> {
    let world = new_world()?;
    let outside = world.project("project")?;
    let environment = world.environment();

    // Relation to the home directory
    let reason = declined(planned_in(&outside, &environment, &[world.inside_base()])?)?;
    assert_mentions(&reason, "lies inside the home directory");
    let inside = world.project("home/work/project")?;
    let reason = declined(planned_in(&inside, &environment, &[world.outside_base()])?)?;
    assert_mentions(&reason, "lies outside the home directory");

    // An rc file above the base that the project does not share, under either name
    let renamed = YarnEnvironment {
        rc_filename: Some(".custom.yml".to_string()),
        ..environment.clone()
    };
    for (name, environment) in [(".yarnrc.yml", &environment), (".custom.yml", &renamed)] {
        let rc_file = world.outside_base().join(name);
        write(&rc_file, "nodeLinker: pnp\n")?;
        let reason = declined(planned_in(&outside, environment, &[world.outside_base()])?)?;
        assert_mentions(&reason, &format!("Yarn would read {rc_file}"));
        std::fs::remove_file(rc_file)?;
    }
    write(&world.home.join(".cache/.yarnrc.yml"), "nodeLinker: pnp\n")?;
    let reason = declined(planned_in(&inside, &environment, &world.bases())?)?;
    assert_mentions(&reason, "Yarn would read");

    // A later base serves when the first does not
    write(
        &world.outside_base().join(".yarnrc.yml"),
        "nodeLinker: pnp\n",
    )?;
    let other = world.top.join("other-tmp");
    let Planned::Ready(plan) = planned_in(
        &outside,
        &environment,
        &[world.outside_base(), other.clone()],
    )?
    else {
        eyre::bail!("the second base qualifies");
    };
    assert_eq!(plan.base, other);
    Ok(())
}

/// A base inside the project or inside a copied folder is not used, since the copy would land
/// in what it copies.
#[test]
fn a_scratch_base_inside_what_is_copied_is_not_used() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("project")?;
    write(
        &root.join("packages/a/package.json"),
        r#"{"name":"pkg-a","dependencies":{"up":"file:../../../shared"}}"#,
    )?;
    write(&world.top.join("shared/package.json"), r#"{"name":"up"}"#)?;
    let environment = world.environment();

    for base in [root.join("tmp"), world.top.join("shared/tmp")] {
        let reason = declined(planned_in(&root, &environment, &[base])?)?;
        assert_mentions(&reason, "inside the project or a folder it copies");
    }
    Ok(())
}

/// A scratch directory that overlaps what the plan copies declines when it is created, should a
/// base have slipped past planning.
///
/// The plan is assembled by hand, with its base inside the project.
#[test]
fn a_scratch_directory_overlapping_the_project_declines() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("project")?;
    let plan = Plan {
        base: root.join("tmp"),
        ..world.ready(&root)?
    };

    let Materialized::Declined(reason) = plan.materialize()? else {
        eyre::bail!("a scratch directory inside the project is refused");
    };
    assert_mentions(&reason, "overlaps the project");
    Ok(())
}

/// An rc file Yarn would refuse to read declines, and so does a setting screening refuses in an
/// rc file read in place.
#[test]
fn an_unreadable_or_refused_rc_file_declines() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("project")?;
    write(&root.join(".yarnrc.yml"), "plugins: [\n")?;
    assert_mentions(&world.declined(&root)?, ".yarnrc.yml");

    let world = new_world()?;
    let root = world.project("project")?;
    write(&world.home.join(".yarnrc.yml"), "plugins:\n  - x.cjs\n")?;
    assert_mentions(&world.declined(&root)?, "configures Yarn plugins");
    Ok(())
}

/// A `yarnPath` that Yarn would interpolate declines, and one in an rc file read in place, or an
/// absolute one, is read in place rather than copied.
#[test]
fn a_yarn_path_is_copied_read_in_place_or_declined() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("project")?;
    write(
        &root.join(".yarnrc.yml"),
        "yarnPath: \"${YARN}/yarn.cjs\"\n",
    )?;
    assert_mentions(&world.declined(&root)?, "which Yarn interpolates");

    let world = new_world()?;
    let root = world.project("project")?;
    write(
        &world.home.join(".yarnrc.yml"),
        "yarnPath: tools/yarn.cjs\n",
    )?;
    write(&world.home.join("tools/yarn.cjs"), "// yarn")?;
    let absolute = world.top.join("yarn.cjs");
    write(
        &root.join(".yarnrc.yml"),
        &format!("yarnPath: {}\n", serde_json::to_string(absolute.as_str())?),
    )?;
    let plan = world.ready(&root)?;
    assert!(
        plan.in_place
            .contains_key(&world.home.join("tools/yarn.cjs"))
    );
    assert!(plan.in_place.contains_key(&absolute));
    Ok(())
}

/// A manifest that cannot be read fails verification rather than being left out of the copy.
#[cfg(unix)]
#[test]
fn an_unreadable_manifest_is_an_error() -> eyre::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let world = new_world()?;
    let root = world.project("project")?;
    let manifest = root.join("packages/b/package.json");
    std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o000))?;
    // A process that reads the file anyway, such as one running as root, proves nothing here.
    if std::fs::read(&manifest).is_ok() {
        return Ok(());
    }

    let planned = world.planned(&root);

    std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o644))?;
    let err = planned
        .err()
        .ok_or_else(|| eyre::eyre!("planning failed"))?;
    assert!(err.to_string().contains("packages/b/package.json"), "{err}");
    Ok(())
}

/// The walk recreates a link Yarn's glob would follow to a walked directory, copies a linked
/// manifest as the file it reaches, and skips a dangling link; a link the copy cannot recreate
/// declines.
#[cfg(unix)]
#[test]
fn symbolic_links_are_recreated_or_declined() -> eyre::Result<()> {
    use std::os::unix::fs::symlink;

    // Recreated, copied, and skipped
    let world = new_world()?;
    let root = world.project("project")?;
    write(
        &root.join("libs/shared/package.json"),
        r#"{"name":"shared"}"#,
    )?;
    symlink("../libs/shared", root.join("packages/shared"))?;
    write(&root.join("templates/c.json"), r#"{"name":"pkg-c"}"#)?;
    std::fs::create_dir_all(root.join("packages/c"))?;
    symlink(
        "../../templates/c.json",
        root.join("packages/c/package.json"),
    )?;
    symlink("../missing", root.join("packages/dangling"))?;
    let plan = world.ready(&root)?;
    let relative = |path: &str| -> eyre::Result<Utf8PathBuf> {
        Ok(root.join(path).strip_prefix(&plan.boundary)?.to_owned())
    };
    assert_eq!(
        plan.entries.get(&relative("packages/shared")?),
        Some(&Entry::Link {
            target: Utf8PathBuf::from("../libs/shared"),
            directory: true
        })
    );
    assert_eq!(
        plan.entries.get(&relative("packages/c/package.json")?),
        Some(&Entry::Screened {
            source: root.join("packages/c/package.json"),
            bytes: br#"{"name":"pkg-c"}"#.to_vec()
        })
    );
    assert!(!plan.entries.contains_key(&relative("packages/dangling")?));

    // Declined: an absolute target, a target outside the project, and one in an ignored
    // directory
    for link in ["absolute", "outside", "ignored"] {
        let world = new_world()?;
        let root = world.project("project")?;
        write(&root.join("libs/shared/package.json"), "{}")?;
        write(&world.top.join("outside/package.json"), "{}")?;
        write(&root.join("node_modules/dep/package.json"), "{}")?;
        let target = match link {
            "absolute" => root.join("libs/shared").to_string(),
            "outside" => "../../outside".to_string(),
            _ => "../node_modules/dep".to_string(),
        };
        symlink(&target, root.join("packages").join(link))?;
        assert_mentions(&world.declined(&root)?, "cannot recreate");
    }

    // Declined: a local package reached through a link, and a link with an absolute target in a
    // `file:` folder
    let world = new_world()?;
    let root = world.project("project")?;
    write(&root.join("real/package.json"), r#"{"name":"real"}"#)?;
    symlink("real", root.join("alias"))?;
    write(
        &root.join("packages/a/package.json"),
        r#"{"name":"pkg-a","dependencies":{"real":"file:../../alias"}}"#,
    )?;
    assert_mentions(&world.declined(&root)?, "through a symbolic link");

    let world = new_world()?;
    let root = world.project("project")?;
    write(&root.join("local/package.json"), r#"{"name":"local"}"#)?;
    symlink(world.top.as_str(), root.join("local/escape"))?;
    write(
        &root.join("packages/a/package.json"),
        r#"{"name":"pkg-a","dependencies":{"local":"file:../../local"}}"#,
    )?;
    assert_mentions(&world.declined(&root)?, "cannot recreate");
    Ok(())
}

/// A file name that is not UTF-8 declines, since the copy cannot name it.
#[cfg(target_os = "linux")]
#[test]
fn a_name_that_is_not_utf8_declines() -> eyre::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let world = new_world()?;
    let root = world.project("project")?;
    let name = std::ffi::OsStr::from_bytes(b"bad\xff");
    std::fs::create_dir_all(root.as_std_path().join(name))?;

    assert_mentions(&world.declined(&root)?, "not UTF-8");
    Ok(())
}

/// A written copy holds exactly the planned bytes at the mirrored positions, recreates its
/// links, and has the digest planning gives; changing a copied file, adding a manifest, or
/// creating an rc file read in place changes the digest.
#[cfg(unix)]
#[test]
fn the_copy_and_its_digest_cover_every_input() -> eyre::Result<()> {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt as _;
    let world = new_world()?;
    let root = world.project("home/work/project")?;
    write(
        &root.join("packages/a/package.json"),
        r#"{"name":"pkg-a","dependencies":{"local":"file:../../local"}}"#,
    )?;
    write(&root.join("local/package.json"), r#"{"name":"local"}"#)?;
    write(&root.join("local/bin.sh"), "#!/bin/sh\n")?;
    std::fs::set_permissions(
        root.join("local/bin.sh"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    std::os::unix::fs::symlink("bin.sh", root.join("local/run"))?;
    write(&world.home.join("work/.yarnrc.yml"), "nodeLinker: pnp\n")?;

    let plan = world.ready(&root)?;
    let planned = plan.digest()?;
    assert!(planned.is_some());
    let Materialized::Ready(staged) = plan.materialize()? else {
        eyre::bail!("the copy is written");
    };

    // The copy mirrors the project below the boundary, beside the mirrored ancestor rc file.
    assert!(
        staged.copy_root.ends_with("tree/work/project"),
        "{}",
        staged.copy_root
    );
    assert!(staged.copy_root.starts_with(world.inside_base()));
    let copied = |path: &str| std::fs::read_to_string(staged.copy_root.join(path));
    assert_eq!(
        copied("yarn.lock")?,
        std::fs::read_to_string(root.join("yarn.lock"))?
    );
    assert_eq!(copied("local/bin.sh")?, "#!/bin/sh\n");
    assert_eq!(copied("../.yarnrc.yml")?, "nodeLinker: pnp\n");
    let mode = std::fs::metadata(staged.copy_root.join("local/bin.sh"))?
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    assert_eq!(
        std::fs::read_link(staged.copy_root.join("local/run"))?,
        std::path::Path::new("bin.sh")
    );
    assert_eq!(Some(&staged.digest), planned.as_ref());

    // Every kind of change is seen by planning again.
    let replan =
        || -> eyre::Result<Option<super::InputDigest>> { Ok(world.ready(&root)?.digest()?) };
    assert_eq!(replan()?, planned);
    let changes = [
        (root.join("local/bin.sh"), "#!/bin/sh\nexit 1\n"),
        (root.join("packages/new/package.json"), "{}"),
        (world.home.join(".yarnrc.yml"), "enableTelemetry: false\n"),
    ];
    let mut seen = BTreeMap::new();
    for (path, contents) in changes {
        write(&path, contents)?;
        let digest = replan()?;
        assert_ne!(digest, planned, "{path}");
        seen.insert(path.to_string(), digest);
    }
    assert_eq!(seen.len(), 3);

    // Dropping the copy deletes it.
    let scratch = staged.scratch.path().to_path_buf();
    drop(staged);
    assert!(!scratch.exists());
    Ok(())
}

/// A recreated link must resolve inside the copy, as the copy's filesystem resolves it.
///
/// In the source, `..` stops at the filesystem root, so a link climbing past it and back down
/// into the project reaches the project, while its copy climbs out of the scratch tree and back
/// into the source.
/// The same escape hides behind a second link that leads somewhere shallower than it sits, after
/// which a `..` climbs from where it leads.
#[cfg(unix)]
#[test]
fn a_link_that_resolves_outside_the_copy_declines() -> eyre::Result<()> {
    use std::os::unix::fs::symlink;

    // Climbing past the filesystem root, then back into the project's own `store`
    let world = new_world()?;
    let root = world.project("project")?;
    write(&root.join("store/package.json"), "{}")?;
    let below_root = root.strip_prefix("/")?;
    symlink(
        format!("{}{below_root}/store", "../".repeat(32)),
        root.join("out"),
    )?;
    assert_mentions(&world.declined(&root)?, "out links to");

    // The same climb, after a link that leads to the project from deep inside it
    let world = new_world()?;
    let root = world.project("project")?;
    write(&root.join("store/package.json"), "{}")?;
    let below_root = root.strip_prefix("/")?;
    let deep = root.join("d/".repeat(40));
    std::fs::create_dir_all(&deep)?;
    symlink("../".repeat(40), deep.join("up"))?;
    symlink(
        format!("up/{}{below_root}/store", "../".repeat(30)),
        deep.join("out"),
    )?;
    let reason = world.declined(&root)?;
    assert_mentions(&reason, "/out links to");

    // A link that stays inside the copy is recreated.
    let world = new_world()?;
    let root = world.project("project")?;
    write(&root.join("store/package.json"), "{}")?;
    symlink("../store", root.join("packages/out"))?;
    let plan = world.ready(&root)?;
    assert!(
        plan.entries
            .values()
            .any(|entry| matches!(entry, Entry::Link { target, .. } if target == "../store"))
    );
    Ok(())
}

/// A `portal:` path that climbs past the filesystem root and back down names the source from
/// the copy, so it declines although it names a folder inside the project.
#[test]
fn a_local_package_climbing_past_the_root_declines() -> eyre::Result<()> {
    let world = new_world()?;
    let root = world.project("project")?;
    write(&root.join("portal/package.json"), r#"{"name":"portal"}"#)?;
    let below_root = root
        .strip_prefix("/")
        .map_or_else(|_| root.to_string(), ToString::to_string);
    let range = serde_json::to_string(&format!("portal:{}{below_root}/portal", "../".repeat(32)))?;
    write(
        &root.join("packages/a/package.json"),
        &format!(r#"{{"name":"pkg-a","dependencies":{{"portal":{range}}}}}"#),
    )?;

    assert_mentions(&world.declined(&root)?, "names the local package");
    Ok(())
}

/// A `file:` folder keeps every directory's and file's permission bits in the copy, which Yarn
/// hashes into the folder's locator, and the digest changes with them; a copy holding read-only
/// directories is still deleted.
#[cfg(unix)]
#[test]
fn a_folder_keeps_its_permissions_and_they_are_digested() -> eyre::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let world = new_world()?;
    let root = world.project("project")?;
    write(
        &root.join("packages/a/package.json"),
        r#"{"name":"pkg-a","dependencies":{"local":"file:../../local"}}"#,
    )?;
    write(&root.join("local/package.json"), r#"{"name":"local"}"#)?;
    write(&root.join("local/lib/index.js"), "")?;
    let set = |path: &Utf8Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    };
    set(&root.join("local/lib"), 0o700)?;
    set(&root.join("local"), 0o750)?;

    let plan = world.ready(&root)?;
    let digest = plan.digest()?;
    let Materialized::Ready(staged) = plan.materialize()? else {
        eyre::bail!("the copy is written");
    };
    let mode = |path: &str| -> eyre::Result<u32> {
        Ok(std::fs::metadata(staged.copy_root.join(path))?
            .permissions()
            .mode()
            & 0o777)
    };
    assert_eq!(mode("local/lib")?, 0o700);
    assert_eq!(mode("local")?, 0o750);

    // A directory's mode alone changes the digest.
    set(&root.join("local/lib"), 0o755)?;
    assert_ne!(world.ready(&root)?.digest()?, digest);

    // A read-only directory in the copy does not keep it from being deleted.
    set(&root.join("local/lib"), 0o500)?;
    let Materialized::Ready(read_only) = world.ready(&root)?.materialize()? else {
        eyre::bail!("the copy is written");
    };
    assert_eq!(
        std::fs::metadata(read_only.copy_root.join("local/lib"))?
            .permissions()
            .mode()
            & 0o777,
        0o500
    );
    let scratch = read_only.scratch.path().to_path_buf();
    drop(read_only);
    assert!(!scratch.exists());
    set(&root.join("local/lib"), 0o755)?;
    drop(staged);
    Ok(())
}

/// One input changed between planning and writing the copy.
struct Swap {
    /// The root manifest the project is planned with, when not the fixture's own.
    root_manifest: Option<&'static str>,
    /// The changed file, relative to the world.
    changed: &'static str,
    contents: &'static str,
}

/// A screened file that changes between planning and writing the copy declines the copy, so Yarn
/// never runs on bytes no rule screened: an rc file copied or read in place, a manifest, and the
/// lock alike.
/// A workspace manifest that is also inside a `file:` folder stays screened, however the
/// traversal reaches it first.
#[test]
fn a_screened_input_changed_after_planning_declines_the_copy() -> eyre::Result<()> {
    let networked = "networkSettings:\n  registry.yarnpkg.com:\n    enableNetwork: true\n";
    let exec = r#"{"name":"pkg-a","dependencies":{"x":"exec:./x.js"}}"#;
    // The workspace `packages/a` is also the `file:` folder of a root dependency.
    let wrapping = r#"{"name":"berry-root","private":true,"workspaces":["packages/*"],"dependencies":{"wrapped-a":"file:./packages/a"}}"#;
    for Swap {
        root_manifest,
        changed,
        contents,
    } in [
        Swap {
            root_manifest: None,
            changed: "project/.yarnrc.yml",
            contents: networked,
        },
        Swap {
            root_manifest: None,
            changed: "home/.yarnrc.yml",
            contents: networked,
        },
        Swap {
            root_manifest: None,
            changed: "project/packages/a/package.json",
            contents: exec,
        },
        Swap {
            root_manifest: None,
            changed: "project/yarn.lock",
            contents: "",
        },
        Swap {
            root_manifest: Some(wrapping),
            changed: "project/packages/a/package.json",
            contents: exec,
        },
    ] {
        let world = new_world()?;
        let root = world.project("project")?;
        write(&root.join(".yarnrc.yml"), "enableTelemetry: false\n")?;
        if let Some(manifest) = root_manifest {
            write(&root.join("package.json"), manifest)?;
        }
        let plan = world.ready(&root)?;

        write(&world.top.join(changed), contents)?;

        let Materialized::Declined(reason) = plan.materialize()? else {
            eyre::bail!("{changed}: a copy of changed input was written");
        };
        // A project file is named from the project, `/`-separated, and one outside it as it is.
        let named = changed.strip_prefix("project/").map_or_else(
            || {
                changed
                    .split('/')
                    .fold(world.top.clone(), |path, part| path.join(part))
                    .to_string()
            },
            str::to_string,
        );
        assert!(
            reason.starts_with(&format!(
                "{named} changed while cooldown copied the project"
            )),
            "{changed}: {reason}"
        );
    }
    Ok(())
}

/// Two traversals reaching one position merge only into an entry faithful to both: an identical
/// proposal stays, a screened reading of a file wins over a plain copy of the same file in either
/// order, and every other pair conflicts, whichever comes first.
#[test]
fn colliding_entries_merge_only_when_both_agree() {
    let file = |source: &str| Entry::File {
        source: Utf8PathBuf::from(source),
    };
    let screened = |source: &str, bytes: &[u8]| Entry::Screened {
        source: Utf8PathBuf::from(source),
        bytes: bytes.to_vec(),
    };
    let directory = |source: &str| Entry::Directory {
        source: Utf8PathBuf::from(source),
    };
    let link = |target: &str, directory: bool| Entry::Link {
        target: Utf8PathBuf::from(target),
        directory,
    };
    let entries = [
        file("/p/a"),
        file("/p/b"),
        screened("/p/a", b"one"),
        screened("/p/a", b"two"),
        screened("/p/b", b"one"),
        directory("/p/a"),
        directory("/p/b"),
        link("real", false),
        link("other", false),
        link("real", true),
    ];

    // The named cases the rule exists for
    assert_eq!(
        merge(&file("/p/a"), &screened("/p/a", b"one")),
        Merge::Replace
    );
    assert_eq!(merge(&screened("/p/a", b"one"), &file("/p/a")), Merge::Keep);
    assert_eq!(merge(&file("/p/a"), &link("real", false)), Merge::Conflict);
    assert_eq!(merge(&link("real", false), &file("/p/a")), Merge::Conflict);
    assert_eq!(
        merge(&link("real", false), &link("other", false)),
        Merge::Conflict
    );
    assert_eq!(merge(&directory("/p/a"), &file("/p/a")), Merge::Conflict);
    assert_eq!(merge(&file("/p/a"), &file("/p/b")), Merge::Conflict);

    // Every ordered pair of kinds
    for existing in &entries {
        for proposed in &entries {
            let expected = match (existing, proposed) {
                _ if existing == proposed => Merge::Keep,
                (Entry::File { source }, Entry::Screened { source: other, .. })
                    if source == other =>
                {
                    Merge::Replace
                }
                (Entry::Screened { source, .. }, Entry::File { source: other })
                    if source == other =>
                {
                    Merge::Keep
                }
                _ => Merge::Conflict,
            };
            assert_eq!(
                merge(existing, proposed),
                expected,
                "{existing:?} then {proposed:?}"
            );
        }
    }
}

/// A patch file that is a symbolic link inside a `file:` folder is read through the link by the
/// patch and archived as the link by the folder, and no single copy is faithful to both, so the
/// copy declines whichever range planning follows first.
#[cfg(unix)]
#[test]
fn a_patch_read_through_a_link_the_folder_archives_declines() -> eyre::Result<()> {
    let patch = "patch:chalk@npm%3A4.1.2#./local/fix.patch";
    let mut admitted = Vec::new();
    // Manifest keys are followed in sorted order, so the second spelling reaches the folder first.
    for dependencies in [
        format!(r#"{{"chalk":"{patch}","local":"file:./local"}}"#),
        format!(r#"{{"a-local":"file:./local","z-chalk":"{patch}"}}"#),
    ] {
        let world = new_world()?;
        let root = world.project("project")?;
        write(
            &root.join("package.json"),
            &format!(
                r#"{{"name":"berry-root","private":true,"workspaces":["packages/*"],"dependencies":{dependencies}}}"#
            ),
        )?;
        write(&root.join("local/package.json"), r#"{"name":"local"}"#)?;
        write(&root.join("local/real.patch"), "--- a\n")?;
        std::os::unix::fs::symlink("real.patch", root.join("local/fix.patch"))?;

        match world.planned(&root)? {
            Planned::Declined(reason) => {
                assert_mentions(&reason, "local/fix.patch would be copied both as");
            }
            Planned::Ready(_) => admitted.push(dependencies),
        }
    }
    assert_eq!(admitted, Vec::<String>::new());
    Ok(())
}
