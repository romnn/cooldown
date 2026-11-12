use cooldown_core::CoreError;
use std::path::PathBuf;

pub(super) fn directory() -> Result<PathBuf, CoreError> {
    config_directory(
        std::env::consts::OS,
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::var_os("USERPROFILE").map(PathBuf::from),
        std::env::var_os("APPDATA").map(PathBuf::from),
    )
    .map(|directory| directory.join("cooldown").join("traces"))
    .ok_or_else(|| {
        CoreError::Config("no absolute user configuration directory for run traces".into())
    })
}

fn config_directory(
    os: &str,
    xdg: Option<PathBuf>,
    home: Option<PathBuf>,
    profile: Option<PathBuf>,
    appdata: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg.filter(|path| path.is_absolute()) {
        return Some(xdg);
    }
    let home = home
        .filter(|path| path.is_absolute())
        .or_else(|| profile.filter(|path| path.is_absolute()));
    match os {
        "windows" => appdata
            .filter(|path| path.is_absolute())
            .or_else(|| home.map(|home| home.join("AppData").join("Roaming"))),
        "macos" => home.map(|home| home.join("Library").join("Application Support")),
        _ => home.map(|home| home.join(".config")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_config_bases_and_explicit_xdg_are_absolute() {
        let base = std::env::temp_dir();
        let home = base.join("trace-home");
        for (os, expected) in [
            ("linux", home.join(".config")),
            ("macos", home.join("Library").join("Application Support")),
            ("windows", home.join("AppData").join("Roaming")),
        ] {
            assert_eq!(
                config_directory(os, None, Some(home.clone()), None, None),
                Some(expected)
            );
            let xdg = base.join("explicit-config");
            assert_eq!(
                config_directory(os, Some(xdg.clone()), Some(home.clone()), None, None),
                Some(xdg)
            );
        }
        assert_eq!(
            config_directory(
                "linux",
                Some("relative".into()),
                Some("relative".into()),
                None,
                None
            ),
            None
        );
    }
}
