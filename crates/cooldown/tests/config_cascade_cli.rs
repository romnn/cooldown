//! Pins which `cooldown.toml` layers the CLI collects for a project when the repository root and
//! the project directory arrive spelled differently.

use color_eyre::eyre;
use indoc::indoc;
use std::process::Command;

/// A Hex project, which `config` detects and resolves without touching the network.
const MIX_EXS: &str = indoc! {r#"
    defmodule Web.MixProject do
      use Mix.Project
      def project, do: [app: :web, version: "0.1.0", deps: [{:jason, "~> 1.4"}]]
    end
"#};

const MIX_LOCK: &str = indoc! {r#"
    %{
      "jason": {:hex, :jason, "1.4.0", "e855647bc964a44e2f67df589ccf49105ae039d4179db7f6271dfd3843dc27e6", [:mix], [], "hexpm", "79a3791085b2a0f743ca04cec0f7be26443738779d09302e01318f97bdb82121"},
    }
"#};

/// A repository at the home directory bounds the cascade of a project below it, including when
/// `HOME` is spelled through a `..` component, so the intermediate layer between the workdir and
/// the project is collected exactly once.
///
/// The `..` spelling stands in for a symlinked home directory, and on Windows the plain spelling
/// is a second alias of the verbatim form the workdir resolves to, so the test needs no symlink
/// privilege on either platform.
#[test]
fn a_home_repository_root_spelled_differently_collects_every_layer() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let base = directory.path();
    let workdir = base.join("home").join("work");
    let project = workdir.join("apps").join("web");
    std::fs::create_dir_all(&project)?;
    // A configured temp directory may have its own Git ancestor; this fixture owns its boundary.
    std::fs::create_dir(base.join("home").join(".git"))?;
    std::fs::create_dir_all(base.join("other"))?;
    std::fs::write(project.join("mix.exs"), MIX_EXS)?;
    std::fs::write(project.join("mix.lock"), MIX_LOCK)?;
    std::fs::write(
        workdir.join("apps").join("cooldown.toml"),
        "min-age = \"18d\"\n",
    )?;
    std::fs::write(project.join("cooldown.toml"), "min-age = \"21d\"\n")?;
    // The raw temporary directory is not verbatim on Windows, so joining keeps the `..` the child
    // process must see.
    let home = base.join("other").join("..").join("home");

    let cache = tempfile::tempdir()?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_cooldown"));
    command.env("COOLDOWN_NO_TRACE", "true");
    // The user's own `COOLDOWN_*` settings could narrow detection, so none reaches the run.
    // Windows matches variable names regardless of case, so the prefix is compared the same way.
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("COOLDOWN_")
        {
            command.env_remove(name);
        }
    }
    let output = command
        .current_dir(&workdir)
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", cache.path())
        .args(["config", "--json", "--no-progress", "--no-global"])
        .output()?;
    let config: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|err| {
        eyre::eyre!(
            "cooldown config printed no JSON envelope ({err}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;

    // Both files below the root apply, nearest last, each named relative to the home root.
    let layers: Vec<_> = config
        .pointer("/items/0/layers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert_eq!(
        layers,
        [
            "default",
            "repo:work/apps/cooldown.toml",
            "repo:work/apps/web/cooldown.toml"
        ],
        "config envelope: {config:#}"
    );
    Ok(())
}
