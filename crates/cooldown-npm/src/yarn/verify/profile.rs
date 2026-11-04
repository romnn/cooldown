//! How each Yarn Berry release line is asked for its verdict, and the environment every run
//! shares.
//!
//! Yarn refuses an unknown `YARN_*` setting outright, so every variable here exists in each
//! release line it is set for: the shared ones in Yarn 2.4 through 4.18, and hardened mode only
//! from Yarn 4.0 ([Configuration.ts][configuration]).
//!
//! [configuration]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/Configuration.ts

use super::stage::Outputs;
use crate::nodecmd::EnvOverride;
use std::ffi::OsString;

/// The command line and settings a Yarn release line verifies a lock with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Profile {
    /// Yarn 2.4, whose install skips builds through `--skip-builds`, which 2.0 through 2.3 lack.
    Yarn2,
    /// Yarn 3, which replaced `--skip-builds` with `--mode=skip-build`.
    Yarn3,
    /// Yarn 4, which also enables hardened mode by default on public pull requests.
    Yarn4,
}

/// The release `yarn --version` reported and the profile that verifies with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Probed {
    pub(super) profile: Profile,
    /// The version as Yarn printed it.
    pub(super) version: String,
}

impl Profile {
    /// The profile for the version `yarn --version` printed, or why none verifies with it.
    pub(super) fn of(version: &str) -> Result<Probed, String> {
        let parsed = semver::Version::parse(version).map_err(|_| {
            format!("`yarn --version` printed `{version}`, which is no Yarn version")
        })?;
        let profile = match (parsed.major, parsed.minor) {
            (0 | 1, _) => {
                return Err(format!(
                    "`yarn` there runs Yarn {version}, a Yarn Classic (v1) release, which cannot \
                     read a Yarn Berry lockfile"
                ));
            }
            (2, 4..) => Profile::Yarn2,
            (3, _) => Profile::Yarn3,
            (4, _) => Profile::Yarn4,
            _ => {
                return Err(format!(
                    "`yarn` there runs Yarn {version}, and cooldown verifies Yarn Berry locks only \
                     with Yarn 2.4 through 4"
                ));
            }
        };
        Ok(Probed {
            profile,
            version: version.to_string(),
        })
    }

    /// The install command line that verifies a lock under this profile.
    ///
    /// `--immutable` turns the install into Yarn's lockfile check, `--json` makes its report
    /// readable, and skipping builds keeps dependency scripts from running should the install
    /// reach them.
    pub(super) fn args(self) -> Vec<String> {
        let skip_builds = match self {
            Profile::Yarn2 => "--skip-builds",
            Profile::Yarn3 | Profile::Yarn4 => "--mode=skip-build",
        };
        ["install", "--immutable", skip_builds, "--json"]
            .map(ToString::to_string)
            .to_vec()
    }

    /// The settings this profile adds to [`base_overrides`].
    ///
    /// Yarn 4 enables hardened mode on its own on a public pull request, and hardened mode
    /// refreshes every package from the registry, which an offline run cannot reach.
    /// Settings screening has already declined any explicit hardened mode, so turning it off here
    /// only overrides that automatic default.
    pub(super) fn overrides(self) -> Vec<EnvOverride> {
        match self {
            Profile::Yarn2 | Profile::Yarn3 => Vec::new(),
            Profile::Yarn4 => vec![set("YARN_ENABLE_HARDENED_MODE", "0")],
        }
    }
}

/// The settings every verification run sets, the version probe included.
///
/// Yarn, Corepack, and git are kept off the network.
/// The cache starts empty and writable, so Yarn can still pack local packages while resolving,
/// and every folder Yarn writes outside the project lands in the scratch directory.
pub(super) fn base_overrides(outputs: &Outputs) -> Vec<EnvOverride> {
    let mut overrides = vec![
        set("YARN_ENABLE_NETWORK", "0"),
        set("COREPACK_ENABLE_NETWORK", "0"),
        set("GIT_ALLOW_PROTOCOL", "file"),
        set("YARN_ENABLE_GLOBAL_CACHE", "0"),
        set("YARN_ENABLE_MIRROR", "0"),
        set("YARN_ENABLE_IMMUTABLE_CACHE", "0"),
        set("YARN_ENABLE_TELEMETRY", "0"),
        set("YARN_ENABLE_COLORS", "0"),
    ];
    for (name, path) in [
        ("YARN_CACHE_FOLDER", &outputs.cache),
        ("YARN_GLOBAL_FOLDER", &outputs.global),
        ("YARN_INSTALL_STATE_PATH", &outputs.install_state),
        ("YARN_VIRTUAL_FOLDER", &outputs.virtual_folder),
        ("YARN_PNP_UNPLUGGED_FOLDER", &outputs.unplugged),
    ] {
        overrides.push(EnvOverride {
            name,
            value: OsString::from(path.as_str()),
        });
    }
    overrides
}

fn set(name: &'static str, value: &str) -> EnvOverride {
    EnvOverride {
        name,
        value: OsString::from(value),
    }
}

#[cfg(test)]
mod tests {
    use super::{Profile, base_overrides};
    use crate::yarn::verify::stage::Outputs;
    use camino::Utf8PathBuf;

    /// Each release line gets the install flags it understands, Yarn 2.4 its own build-skipping
    /// flag, and only Yarn 4 the hardened-mode setting that earlier releases would refuse.
    #[test]
    fn each_release_line_verifies_with_flags_it_understands() -> Result<(), String> {
        let profile = |version: &str| Profile::of(version).map(|probed| probed.profile);
        assert_eq!(profile("2.4.3")?, Profile::Yarn2);
        assert_eq!(profile("2.4.0")?, Profile::Yarn2);
        assert_eq!(profile("3.0.0")?, Profile::Yarn3);
        assert_eq!(profile("3.8.7")?, Profile::Yarn3);
        assert_eq!(profile("4.0.2")?, Profile::Yarn4);
        assert_eq!(profile("4.18.1")?, Profile::Yarn4);

        assert_eq!(
            Profile::Yarn2.args(),
            ["install", "--immutable", "--skip-builds", "--json"]
        );
        for profile in [Profile::Yarn3, Profile::Yarn4] {
            assert_eq!(
                profile.args(),
                ["install", "--immutable", "--mode=skip-build", "--json"]
            );
        }
        let names = |profile: Profile| -> Vec<&str> {
            profile
                .overrides()
                .into_iter()
                .map(|variable| variable.name)
                .collect()
        };
        assert_eq!(names(Profile::Yarn2), [] as [&str; 0]);
        assert_eq!(names(Profile::Yarn3), [] as [&str; 0]);
        assert_eq!(names(Profile::Yarn4), ["YARN_ENABLE_HARDENED_MODE"]);
        Ok(())
    }

    /// A release without a verifying profile declines with the reason: Yarn Classic cannot read
    /// the lock, Yarn 2.0 through 2.3 cannot skip builds, and a newer major is unproven.
    #[test]
    fn releases_without_a_profile_decline() {
        for (version, reason) in [
            ("1.22.22", "Yarn Classic (v1) release"),
            ("2.3.0", "only with Yarn 2.4 through 4"),
            ("5.0.0", "only with Yarn 2.4 through 4"),
            ("not-a-version", "which is no Yarn version"),
        ] {
            let declined = Profile::of(version);
            assert!(
                declined
                    .as_ref()
                    .is_err_and(|declined| declined.contains(reason)),
                "{version}: {declined:?}"
            );
        }
    }

    /// The shared settings keep Yarn, Corepack, and git offline, and point every folder Yarn
    /// writes outside the project into the scratch directory; the list is pinned because Yarn
    /// refuses any setting a release line does not know.
    #[test]
    fn every_run_is_offline_and_writes_into_the_scratch_directory() {
        let scratch = Utf8PathBuf::from("/scratch");
        let outputs = Outputs::under(&scratch);
        let overrides: Vec<(&str, String)> = base_overrides(&outputs)
            .into_iter()
            .map(|variable| (variable.name, variable.value.to_string_lossy().into_owned()))
            .collect();
        let expected: Vec<(&str, String)> = [
            ("YARN_ENABLE_NETWORK", "0".to_string()),
            ("COREPACK_ENABLE_NETWORK", "0".to_string()),
            ("GIT_ALLOW_PROTOCOL", "file".to_string()),
            ("YARN_ENABLE_GLOBAL_CACHE", "0".to_string()),
            ("YARN_ENABLE_MIRROR", "0".to_string()),
            ("YARN_ENABLE_IMMUTABLE_CACHE", "0".to_string()),
            ("YARN_ENABLE_TELEMETRY", "0".to_string()),
            ("YARN_ENABLE_COLORS", "0".to_string()),
            ("YARN_CACHE_FOLDER", outputs.cache.to_string()),
            ("YARN_GLOBAL_FOLDER", outputs.global.to_string()),
            ("YARN_INSTALL_STATE_PATH", outputs.install_state.to_string()),
            ("YARN_VIRTUAL_FOLDER", outputs.virtual_folder.to_string()),
            ("YARN_PNP_UNPLUGGED_FOLDER", outputs.unplugged.to_string()),
        ]
        .into();
        assert_eq!(overrides, expected);
        assert!(
            [
                &outputs.cache,
                &outputs.global,
                &outputs.install_state,
                &outputs.virtual_folder,
                &outputs.unplugged
            ]
            .iter()
            .all(|path| path.starts_with(&scratch))
        );
        assert!(outputs.virtual_folder.ends_with("__virtual__"));
    }
}
