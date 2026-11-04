use super::{RcFile, is_exec, screen};
use crate::manifest::workspaces::{YarnEnvironment, YarnVariable};
use camino::{Utf8Path, Utf8PathBuf};
use indoc::indoc;
use serde_json::{Map, Value};

/// An rc file at `/project/.yarnrc.yml` holding the YAML `settings`, copied or read in place.
fn rc_file(settings: &str, staged: bool) -> RcFile {
    let settings: Map<String, Value> = match serde_saphyr::from_str::<Value>(settings) {
        Ok(Value::Object(settings)) => settings,
        other => panic!("test settings are no mapping: {other:?}"),
    };
    RcFile {
        path: Utf8PathBuf::from("/project/.yarnrc.yml"),
        staged,
        settings,
    }
}

/// The reason screening gives for `rc_files` in `environment`, or `None` when it passes them.
fn declined(rc_files: &[RcFile], environment: &YarnEnvironment) -> Option<String> {
    screen(rc_files, environment, |path: &Utf8Path| {
        path.file_name().unwrap_or_default().to_string()
    })
    .err()
}

fn declined_rc(settings: &str) -> Option<String> {
    declined(&[rc_file(settings, true)], &YarnEnvironment::default())
}

/// An environment setting each `(name, value)` as a `YARN_*` variable, `name` lower-cased.
fn environment(variables: &[[&str; 2]]) -> YarnEnvironment {
    YarnEnvironment {
        variables: variables
            .iter()
            .map(|[name, value]| YarnVariable {
                name: (*name).to_string(),
                value: (*value).to_string(),
            })
            .collect(),
        ..YarnEnvironment::default()
    }
}

fn declined_env(variables: &[[&str; 2]]) -> Option<String> {
    declined(&[], &environment(variables))
}

/// Settings the copy reproduces as they are pass: the default environment file, the default
/// lockfile, the network and hardened mode provably off, and an output folder inside a copied
/// rc file.
#[test]
fn settings_the_copy_reproduces_pass() {
    let reproducible = rc_file(
        indoc! {r#"
            nodeLinker: node-modules
            plugins: []
            injectEnvironmentFiles: [".env.yarn?"]
            lockfileFilename: yarn.lock
            enableNetwork: false
            enableHardenedMode: "0"
            packageExtensions:
              "left-pad@*":
                dependencies:
                  is-number: "^7.0.0"
            pnpDataPath: ./.pnp.data.json
        "#},
        true,
    );
    let in_place = rc_file(
        "enableHardenedMode: false\nenableNetwork: \"false\"\n",
        false,
    );
    let quiet = environment(&[
        ["yarn_enable_hardened_mode", "false"],
        ["yarn_lockfile_filename", "yarn.lock"],
        ["yarn_enable_network", "1"],
    ]);

    assert_eq!(declined(&[reproducible, in_place], &quiet), None);
}

/// A plugin in any rc file, or any `YARN_PLUGINS` value, declines: its hooks may read files the
/// copy leaves out.
#[test]
fn a_configured_plugin_declines() {
    let reason = declined_rc("plugins:\n  - .yarn/plugins/x.cjs\n");
    assert!(
        reason
            .as_deref()
            .is_some_and(|reason| reason.contains(".yarnrc.yml configures Yarn plugins")),
        "{reason:?}"
    );

    let loading = YarnEnvironment {
        plugins: vec!["/x.cjs".to_string()],
        ..YarnEnvironment::default()
    };
    let reason = declined(&[], &loading);
    assert!(
        reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("YARN_PLUGINS loads")),
        "{reason:?}"
    );
}

/// Environment files other than the default, or any `YARN_INJECT_ENVIRONMENT_FILES`, decline.
#[test]
fn injected_environment_files_other_than_the_default_decline() {
    for files in [
        "injectEnvironmentFiles: [\".env\"]\n",
        "injectEnvironmentFiles: [\".env.yarn?\", \".env\"]\n",
        "injectEnvironmentFiles: []\n",
    ] {
        let reason = declined_rc(files);
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("sets `injectEnvironmentFiles`")),
            "{files}: {reason:?}"
        );
    }
    assert_eq!(declined_rc("injectEnvironmentFiles: .env.yarn?\n"), None);

    let reason = declined_env(&[["yarn_inject_environment_files", ".env.yarn?"]]);
    assert!(
        reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("YARN_INJECT_ENVIRONMENT_FILES")),
        "{reason:?}"
    );
}

/// A lockfile named other than `yarn.lock`, in an rc file or the environment, declines.
#[test]
fn another_lockfile_name_declines() {
    let reason = declined_rc("lockfileFilename: other.lock\n");
    assert!(
        reason
            .as_deref()
            .is_some_and(|reason| reason.contains("names the lockfile `other.lock`")),
        "{reason:?}"
    );
    let reason = declined_env(&[["yarn_lockfile_filename", "other.lock"]]);
    assert!(
        reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("YARN_LOCKFILE_FILENAME")),
        "{reason:?}"
    );
}

/// Per-host network settings decline wherever they are set, since they take precedence over the
/// offline switch; so does a global switch that is not provably off.
#[test]
fn network_settings_and_an_enabled_network_decline() {
    let reason = declined_rc(indoc! {r#"
        networkSettings:
          "registry.example.com":
            enableNetwork: true
    "#});
    assert!(
        reason
            .as_deref()
            .is_some_and(|reason| reason.contains("sets `networkSettings`")),
        "{reason:?}"
    );
    for network in ["true", "\"1\"", "\"${NETWORK}\"", ""] {
        let reason = declined_rc(&format!("enableNetwork: {network}\n"));
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("sets `enableNetwork`")),
            "{network}: {reason:?}"
        );
    }
    let reason = declined_env(&[["yarn_network_settings", "{}"]]);
    assert!(reason.is_some());
}

/// Hardened mode declines unless it is provably off: explicitly on, interpolated, empty, or
/// misspelled, in an rc file or the environment.
#[test]
fn hardened_mode_that_is_not_provably_off_declines() {
    for hardened in ["true", "1", "\"true\"", "\"${CI}\"", "", "\"no\""] {
        let reason = declined_rc(&format!("enableHardenedMode: {hardened}\n"));
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("hardened mode checks the lock")),
            "{hardened}: {reason:?}"
        );
    }
    for hardened in ["false", "0", "\"false\"", "\"0\""] {
        assert_eq!(
            declined_rc(&format!("enableHardenedMode: {hardened}\n")),
            None,
            "{hardened}"
        );
    }
    for hardened in ["1", "true", "", "${CI}"] {
        let reason = declined_env(&[["yarn_enable_hardened_mode", hardened]]);
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with("YARN_ENABLE_HARDENED_MODE")),
            "{hardened}: {reason:?}"
        );
    }
}

/// A `packageExtensions` entry that declares an `exec:` dependency declines, however deep it
/// sits and however it is cased.
#[test]
fn an_exec_package_extension_declines() {
    let reason = declined_rc(indoc! {r#"
        packageExtensions:
          "left-pad@*":
            dependencies:
              generated: "EXEC:./generate.js"
    "#});
    assert!(
        reason
            .as_deref()
            .is_some_and(|reason| reason.contains("`exec:` dependency")),
        "{reason:?}"
    );
    assert!(is_exec("patch:generated@exec%3A./generate.js#./fix.patch"));
    assert!(!is_exec("npm:^1.0.0"));
}

/// An output folder no override redirects declines when set in the environment, in an rc file
/// read in place, or as a path that leaves the copied rc file's directory.
#[test]
fn an_unredirected_output_folder_outside_the_copy_declines() {
    for (settings, staged) in [
        ("pnpDataPath: ./.pnp.data.json\n", false),
        ("pnpmStoreFolder: /var/store\n", true),
        ("bstatePath: ../build-state.yml\n", true),
        ("pnpDataPath: \"${DATA}/pnp.json\"\n", true),
    ] {
        let reason = declined(&[rc_file(settings, staged)], &YarnEnvironment::default());
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("outside the verification copy")),
            "{settings}: {reason:?}"
        );
    }
    let reason = declined_env(&[["yarn_pnpm_store_folder", "/var/store"]]);
    assert!(
        reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("YARN_PNPM_STORE_FOLDER")),
        "{reason:?}"
    );
}

/// An rc file using Yarn's conflict-marker form anywhere declines, since Yarn unwraps it: here
/// the top-level `onConflict` hides per-host network settings that turn the network on, a nested
/// one hides an output folder outside the copy, and one inside an array declines too.
#[test]
fn a_conflict_marker_anywhere_declines() {
    for settings in [
        indoc! {"
            onConflict: reset
            value:
              networkSettings:
                registry.yarnpkg.com:
                  enableNetwork: true
        "},
        indoc! {"
            pnpmStoreFolder:
              onConflict: reset
              value: /var/store
        "},
        indoc! {r#"
            packageExtensions:
              "left-pad@*":
                dependencies:
                  - onConflict: extend
        "#},
    ] {
        let reason = declined_rc(settings);
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("uses Yarn's `onConflict` form")),
            "{settings}: {reason:?}"
        );
    }
}
