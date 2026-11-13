use super::{
    AgeSource, CargoEnvironment, EffectiveAge, PublishAgeSettings, parse_age, render_age,
    writable_config_file,
};
use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre;
use cooldown_core::CoreError;
use jiff::SignedDuration;

const DAY: i64 = 24 * 60 * 60;

/// A directory tree whose cargo config files the test lays out explicitly.
struct Tree {
    /// Owns the temporary directory; dropping it deletes the tree.
    _directory: tempfile::TempDir,
    root: Utf8PathBuf,
}

impl Tree {
    fn new() -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
            .map_err(|path| eyre::eyre!("non-UTF-8 temp dir {}", path.display()))?;
        Ok(Self {
            _directory: directory,
            root,
        })
    }

    fn write(&self, relative: &str, contents: &str) -> eyre::Result<Utf8PathBuf> {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents)?;
        Ok(path)
    }

    fn dir(&self, relative: &str) -> eyre::Result<Utf8PathBuf> {
        let path = self.root.join(relative);
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }
}

/// An environment with no cargo variables and a Cargo home outside every test tree.
fn bare_env() -> CargoEnvironment {
    CargoEnvironment::default()
}

fn effective(dir: &Utf8Path, env: &CargoEnvironment) -> Result<Option<EffectiveAge>, CoreError> {
    PublishAgeSettings::discover(dir, env)?.effective_crates_io_age()
}

fn days(count: i64) -> SignedDuration {
    SignedDuration::from_secs(count * DAY)
}

#[test]
fn parse_age_accepts_cargo_spellings() {
    // Units, plurals and the optional space
    assert_eq!(parse_age("14 days"), Ok(days(14)));
    assert_eq!(parse_age("14days"), Ok(days(14)));
    assert_eq!(parse_age("1 day"), Ok(days(1)));
    assert_eq!(parse_age("2 weeks"), Ok(days(14)));
    assert_eq!(parse_age("36 hours"), Ok(SignedDuration::from_hours(36)));
    assert_eq!(parse_age("90 minutes"), Ok(SignedDuration::from_mins(90)));
    assert_eq!(parse_age("5 seconds"), Ok(SignedDuration::from_secs(5)));

    // Cargo's month is exactly 30 days.
    assert_eq!(parse_age("1 month"), Ok(days(30)));

    // A bare zero disables the age.
    assert_eq!(parse_age("0"), Ok(SignedDuration::ZERO));
}

#[test]
fn parse_age_rejects_what_cargo_rejects() {
    for raw in [
        "14d",
        "336h",
        "P14D",
        "",
        "days",
        "7",
        "1 fortnight",
        "-3 days",
    ] {
        assert!(parse_age(raw).is_err(), "{raw:?} must be rejected");
    }
}

#[test]
fn render_age_uses_the_largest_exact_unit() {
    assert_eq!(render_age(days(14)).as_deref(), Some("2 weeks"));
    assert_eq!(render_age(days(7)).as_deref(), Some("1 week"));
    assert_eq!(render_age(days(10)).as_deref(), Some("10 days"));
    assert_eq!(render_age(days(30)).as_deref(), Some("30 days"));
    assert_eq!(
        render_age(SignedDuration::from_hours(36)).as_deref(),
        Some("36 hours")
    );
    assert_eq!(
        render_age(SignedDuration::from_secs(61)).as_deref(),
        Some("61 seconds")
    );

    // Values cargo cannot spell
    assert_eq!(render_age(SignedDuration::from_millis(1500)), None);
    assert_eq!(render_age(days(-1)), None);
}

/// Every rendered age parses back to the same duration, so `sync` round-trips.
#[test]
fn rendered_ages_parse_back() {
    for age in [
        days(1),
        days(14),
        days(45),
        SignedDuration::from_hours(5),
        SignedDuration::from_secs(59),
    ] {
        let rendered = render_age(age).unwrap_or_default();
        assert_eq!(parse_age(&rendered), Ok(age), "{rendered}");
    }
}

#[test]
fn no_config_means_no_age() -> eyre::Result<()> {
    let tree = Tree::new()?;
    assert_eq!(effective(&tree.root, &bare_env())?, None);
    Ok(())
}

#[test]
fn nearest_file_wins_per_key() -> eyre::Result<()> {
    let tree = Tree::new()?;
    tree.write(
        ".cargo/config.toml",
        "[registries.crates-io]\nmin-publish-age = \"30 days\"\n",
    )?;
    let near = tree.write(
        "member/.cargo/config.toml",
        "[registries.crates-io]\nmin-publish-age = \"3 days\"\n",
    )?;
    let member = tree.dir("member")?;

    // The member reads its own file before its parent's.
    assert_eq!(
        effective(&member, &bare_env())?,
        Some(EffectiveAge {
            age: days(3),
            source: AgeSource::File(near),
        })
    );
    // The root does not see the member's file.
    assert_eq!(
        effective(&tree.root, &bare_env())?.map(|found| found.age),
        Some(days(30))
    );
    Ok(())
}

#[test]
fn crates_io_key_beats_a_nearer_global_key() -> eyre::Result<()> {
    let tree = Tree::new()?;
    tree.write(
        ".cargo/config.toml",
        "[registries.crates-io]\nmin-publish-age = \"30 days\"\n",
    )?;
    tree.write(
        "member/.cargo/config.toml",
        "[registry]\nglobal-min-publish-age = \"3 days\"\n",
    )?;
    let member = tree.dir("member")?;

    // Key specificity decides before file distance, as in cargo.
    assert_eq!(
        effective(&member, &bare_env())?.map(|found| found.age),
        Some(days(30))
    );
    Ok(())
}

#[test]
fn global_key_applies_without_a_crates_io_key() -> eyre::Result<()> {
    let tree = Tree::new()?;
    tree.write(
        ".cargo/config.toml",
        "[registry]\nglobal-min-publish-age = \"2 weeks\"\n",
    )?;
    assert_eq!(
        effective(&tree.root, &bare_env())?.map(|found| found.age),
        Some(days(14))
    );
    Ok(())
}

#[test]
fn extensionless_config_wins_over_config_toml() -> eyre::Result<()> {
    let tree = Tree::new()?;
    let legacy = tree.write(
        ".cargo/config",
        "[registries.crates-io]\nmin-publish-age = \"5 days\"\n",
    )?;
    tree.write(
        ".cargo/config.toml",
        "[registries.crates-io]\nmin-publish-age = \"30 days\"\n",
    )?;

    // Cargo reads only `config` when both exist, so `sync` must write there too.
    assert_eq!(
        effective(&tree.root, &bare_env())?.map(|found| found.age),
        Some(days(5))
    );
    assert_eq!(writable_config_file(&tree.root.join(".cargo")), legacy);
    Ok(())
}

#[test]
fn sync_writes_a_new_config_toml_when_none_exists() -> eyre::Result<()> {
    let tree = Tree::new()?;
    let config_dir = tree.root.join(".cargo");
    assert_eq!(
        writable_config_file(&config_dir),
        config_dir.join("config.toml")
    );
    Ok(())
}

#[test]
fn cargo_home_ranks_below_every_project_file() -> eyre::Result<()> {
    let tree = Tree::new()?;
    // The home sits below the project, so it is also one of the project's ancestors.
    tree.write(
        "project/home/config.toml",
        "[registries.crates-io]\nmin-publish-age = \"30 days\"\n",
    )?;
    tree.write(
        ".cargo/config.toml",
        "[registries.crates-io]\nmin-publish-age = \"3 days\"\n",
    )?;
    let project = tree.dir("project")?;
    let env = CargoEnvironment {
        cargo_home: Some(Utf8PathBuf::from("home")),
        ..bare_env()
    };

    // A relative home resolves against the project, yet still ranks after the repo-root file.
    assert_eq!(
        effective(&project, &env)?.map(|found| found.age),
        Some(days(3))
    );
    Ok(())
}

#[test]
fn cargo_home_supplies_an_age_on_its_own() -> eyre::Result<()> {
    let tree = Tree::new()?;
    let home = tree.dir("home")?;
    tree.write(
        "home/config.toml",
        "[registry]\nglobal-min-publish-age = \"1 week\"\n",
    )?;
    let project = tree.dir("project")?;
    let env = CargoEnvironment {
        cargo_home: Some(home),
        ..bare_env()
    };
    assert_eq!(
        effective(&project, &env)?.map(|found| found.age),
        Some(days(7))
    );
    Ok(())
}

#[test]
fn env_global_age_overrides_the_file_global_age_only() -> eyre::Result<()> {
    let tree = Tree::new()?;
    tree.write(
        ".cargo/config.toml",
        "[registry]\nglobal-min-publish-age = \"30 days\"\n",
    )?;
    let env = CargoEnvironment {
        global_age: Some("2 days".to_owned()),
        ..bare_env()
    };

    // The variable replaces the file's global key.
    assert_eq!(
        effective(&tree.root, &env)?,
        Some(EffectiveAge {
            age: days(2),
            source: AgeSource::Env,
        })
    );

    // A crates.io key still beats the variable, since it is the more specific key.
    tree.write(
        ".cargo/config.toml",
        "[registries.crates-io]\nmin-publish-age = \"30 days\"\n",
    )?;
    assert_eq!(
        effective(&tree.root, &env)?.map(|found| found.age),
        Some(days(30))
    );
    Ok(())
}

#[test]
fn allow_switches_enforcement_off() -> eyre::Result<()> {
    let tree = Tree::new()?;
    tree.write(
        ".cargo/config.toml",
        indoc::indoc! {r#"
            [registries.crates-io]
            min-publish-age = "30 days"

            [resolver]
            incompatible-publish-age = "allow"
        "#},
    )?;
    assert_eq!(effective(&tree.root, &bare_env())?, None);

    // The variable overrides the file's switch in both directions.
    let deny = CargoEnvironment {
        incompatible: Some("deny".to_owned()),
        ..bare_env()
    };
    assert_eq!(
        effective(&tree.root, &deny)?.map(|found| found.age),
        Some(days(30))
    );
    Ok(())
}

/// A configured zero is an explicit opt-out that overrides a broader age, not a missing setting.
#[test]
fn zero_age_is_kept_as_an_opt_out() -> eyre::Result<()> {
    let tree = Tree::new()?;
    tree.write(
        ".cargo/config.toml",
        "[registry]\nglobal-min-publish-age = \"30 days\"\n",
    )?;
    tree.write(
        "member/.cargo/config.toml",
        "[registries.crates-io]\nmin-publish-age = \"0\"\n",
    )?;
    let member = tree.dir("member")?;
    assert_eq!(
        effective(&member, &bare_env())?.map(|found| found.age),
        Some(SignedDuration::ZERO)
    );
    Ok(())
}

#[test]
fn invalid_settings_are_config_errors() -> eyre::Result<()> {
    let tree = Tree::new()?;
    tree.write(
        ".cargo/config.toml",
        "[registries.crates-io]\nmin-publish-age = \"14d\"\n",
    )?;
    std::assert_matches!(
        effective(&tree.root, &bare_env()),
        Err(CoreError::Config(_))
    );

    tree.write(
        ".cargo/config.toml",
        "[registries.crates-io]\nmin-publish-age = 14\n",
    )?;
    std::assert_matches!(
        effective(&tree.root, &bare_env()),
        Err(CoreError::Config(_))
    );

    let bogus = CargoEnvironment {
        incompatible: Some("fallback".to_owned()),
        ..bare_env()
    };
    tree.write(".cargo/config.toml", "")?;
    std::assert_matches!(effective(&tree.root, &bogus), Err(CoreError::Config(_)));
    Ok(())
}
