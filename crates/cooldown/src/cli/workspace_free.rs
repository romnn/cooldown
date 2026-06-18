use super::{Command, GlobalArgs};
use crate::app::Exit;
use crate::discovery;
use camino::Utf8PathBuf;
use cooldown_core::CoreError;
use cooldown_render as render;

/// Handle the commands that need neither a workspace nor the network (`schema`, `init`, `sync`).
///
/// Returns `Some` with the command's result when `command` is one of those; `None` otherwise, so
/// the caller proceeds to build a workspace.
pub(in crate::cli) fn run_workspace_free(
    command: &Command,
    global: &GlobalArgs,
) -> Option<Result<Exit, CoreError>> {
    match command {
        Command::Schema => Some(
            render::json_schema_string()
                .map_err(|error| CoreError::Serialization(format!("serialize schema: {error}")))
                .map(|schema| {
                    println!("{schema}");
                    Exit::Ok
                }),
        ),
        Command::Init => Some(cmd_init(global)),
        Command::Sync => {
            eprintln!("`cooldown sync` is not implemented in this build (a later phase).");
            Some(Ok(Exit::Usage))
        }
        _ => None,
    }
}

fn cmd_init(global: &GlobalArgs) -> Result<Exit, CoreError> {
    let dir = global.dir.clone().unwrap_or_else(|| Utf8PathBuf::from("."));
    let path = dir.join(discovery::CONFIG_FILE);
    if path.exists() {
        eprintln!("refusing to clobber existing {path}");
        return Ok(Exit::Usage);
    }
    std::fs::write(&path, STARTER_CONFIG)?;
    println!("wrote {path}");
    Ok(Exit::Ok)
}

const STARTER_CONFIG: &str = r#"# cooldown.toml — refuse to adopt dependency versions younger than a minimum release age.
# Docs: https://github.com/romnn/cooldown

# The one knob most repos ever set. Durations accept "7d", "2 weeks", ISO-8601 "P7D".
min-age = "7d"

# Risk-tiered windows (use INSTEAD of the scalar above):
# min-age = { default = "7d", patch = "3d", minor = "7d", major = "30d" }

# Per tool (npm is the most-attacked registry):
# [tool.node]
# min-age = "21d"

# First-party packages are trusted:
# [package."github.com/acme/*"]
# min-age = "0d"

# Exemptions (audited; shown in `cooldown explain`):
# allow = ["github.com/acme/*"]

# A hard minimum no nearer config can weaken:
# floor = "3d"

# Flag defaults: [global] applies to every subcommand; a [<command>] section overrides it; an
# explicit CLI flag overrides both. Keys are the kebab-case flag names. A few examples:
# [global]
# exclude = ["third_party"]   # directories never scanned (gitignore is honored by default)
# gitignore = true            # set false to scan gitignored paths too
# offline = false             # cache-only; concurrency = 8 tunes the registry fan-out
#
# [tool.cargo]
# exclude = ["vendor"]        # extra excludes for one tool
#
# [outdated]
# major = true                # outdated shows cross-major by default; set false for minor-only
# all = false                 # also list up-to-date deps; exit-code = 1 gates CI
#
# [upgrade]
# strict = true               # fail if any planned change was skipped; build = true to compile
"#;
