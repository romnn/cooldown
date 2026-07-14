//! A thin wrapper around a package manager's own binary, used by adapters only as the
//! resolution/apply engine. The cooldown verdict itself is computed in [`cooldown_core`]; a driver
//! merely re-pins a dependency and (optionally) installs/locks the resolved graph.

use camino::Utf8Path;
use cooldown_core::{CoreError, Result, ToolTermination, VerifyReport, failure_detail};
use std::ffi::OsStr;
#[cfg(any(test, windows))]
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[cfg(windows)]
const WINDOWS_COMMAND_EXTENSIONS: &str = ".COM;.EXE;.BAT;.CMD";

/// Finds a program on `PATH` using the executable forms that [`std::process::Command`] can launch.
///
/// Windows requires an explicit extension for batch launchers such as `npm.cmd`; Rust's process
/// launcher only infers `.exe`. Extensionless Unix shell shims are therefore ignored on Windows in
/// favour of supported `PATHEXT` candidates.
#[must_use]
pub fn program_on_path(program: &str) -> Option<PathBuf> {
    let program_path = Path::new(program);
    if program_path.components().count() > 1 {
        return program_path.is_file().then(|| program_path.to_path_buf());
    }

    let search_path = std::env::var_os("PATH")?;
    #[cfg(windows)]
    if program_path.extension().is_none() {
        let path_extensions = std::env::var_os("PATHEXT")
            .unwrap_or_else(|| OsString::from(WINDOWS_COMMAND_EXTENSIONS));
        return find_with_extensions(program_path.as_os_str(), &search_path, &path_extensions);
    }

    find_exact(program_path.as_os_str(), &search_path)
}

/// Resolves a program to a launchable path when possible, preserving the original value when it
/// is absent so the eventual spawn error retains the caller's configured binary name.
#[must_use]
pub fn resolve_program(program: &str) -> PathBuf {
    program_on_path(program).unwrap_or_else(|| PathBuf::from(program))
}

fn find_exact(program: &OsStr, search_path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(search_path).find_map(|dir| {
        let candidate = dir.join(program);
        is_launchable(&candidate).then_some(candidate)
    })
}

/// Whether `candidate` is a file the platform's spawn would accept — an *executable* one here:
/// `execvp` skips a non-executable name and keeps searching `PATH`, so pre-resolution must not
/// let such a shadow eclipse the real binary further down.
#[cfg(unix)]
fn is_launchable(candidate: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    candidate
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Whether `candidate` is a file the platform's spawn would accept — any file on Windows, where
/// launchability comes from the extension the caller already selected.
#[cfg(not(unix))]
fn is_launchable(candidate: &Path) -> bool {
    candidate.is_file()
}

#[cfg(any(test, windows))]
fn find_with_extensions(
    program: &OsStr,
    search_path: &OsStr,
    path_extensions: &OsStr,
) -> Option<PathBuf> {
    let extensions = supported_windows_extensions(path_extensions);
    std::env::split_paths(search_path).find_map(|dir| {
        extensions.iter().find_map(|extension| {
            let mut filename = program.to_os_string();
            filename.push(extension);
            let candidate = dir.join(filename);
            candidate.is_file().then_some(candidate)
        })
    })
}

#[cfg(any(test, windows))]
fn supported_windows_extensions(path_extensions: &OsStr) -> Vec<OsString> {
    path_extensions
        .to_string_lossy()
        .split(';')
        .map(str::trim)
        .filter(|extension| {
            [".com", ".exe", ".bat", ".cmd"]
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        })
        .map(OsString::from)
        .collect()
}

/// A handle to one package manager binary (e.g. `bundle`, `mix`, `mvn`). The binary defaults to
/// its given name on `PATH` and is overridable via `COOLDOWN_<BIN>` (e.g. `COOLDOWN_BUNDLE`), with
/// any `-` in the name normalised to `_`; reproducible builds and the test suite use the override
/// to pin an exact executable.
#[derive(Clone)]
pub struct Driver {
    bin: String,
}

impl Driver {
    /// Creates a handle to `bin`, honouring the `COOLDOWN_<BIN>` override.
    #[must_use]
    pub fn new(bin: &str) -> Self {
        let env_key = format!("COOLDOWN_{}", bin.to_uppercase().replace('-', "_"));
        Driver {
            bin: std::env::var(&env_key).unwrap_or_else(|_| bin.to_string()),
        }
    }

    /// The resolved binary name (after any `COOLDOWN_<BIN>` override).
    #[must_use]
    pub fn program(&self) -> &str {
        &self.bin
    }

    async fn output(&self, dir: &Utf8Path, args: &[String]) -> Result<std::process::Output> {
        Command::new(resolve_program(&self.bin))
            .args(args)
            .current_dir(dir.as_std_path())
            .output()
            .await
            .map_err(|e| CoreError::ToolSpawn {
                tool: self.bin.clone(),
                detail: format!("`{} {}`: {e}", self.bin, args.join(" ")),
            })
    }

    /// Runs the driver, mapping a non-zero exit to a [`CoreError::Tool`].
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if the binary cannot be spawned, or [`CoreError::Tool`] if
    /// it exits non-zero (e.g. an unsatisfiable pin — a resolver conflict).
    pub async fn run(&self, dir: &Utf8Path, args: &[String]) -> Result<()> {
        let out = self.output(dir, args).await?;
        if out.status.success() {
            Ok(())
        } else {
            Err(CoreError::Tool {
                tool: self.bin.clone(),
                termination: ToolTermination::from_exit_status(out.status),
                stderr: failure_detail(&out),
            })
        }
    }

    /// Runs the driver as a build/verify step, folding the exit status into a [`VerifyReport`] so a
    /// failed install/lock is a reported outcome rather than a hard error.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] only if the binary cannot be spawned at all.
    pub async fn verify(
        &self,
        dir: &Utf8Path,
        args: &[String],
        ok_detail: &str,
    ) -> Result<VerifyReport> {
        let out = self.output(dir, args).await?;
        Ok(VerifyReport {
            ok: out.status.success(),
            detail: if out.status.success() {
                ok_detail.to_string()
            } else {
                failure_detail(&out)
            },
        })
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::find_exact;
    use super::find_with_extensions;
    use std::ffi::OsStr;

    #[cfg(unix)]
    #[test]
    fn exact_search_skips_a_non_executable_shadow() {
        use std::os::unix::fs::PermissionsExt;
        let shadow = tempfile::tempdir().expect("create shadow dir");
        let real = tempfile::tempdir().expect("create real dir");
        std::fs::write(shadow.path().join("go"), "not executable").expect("write shadow file");
        let real_binary = real.path().join("go");
        std::fs::write(&real_binary, "#!/bin/sh\n").expect("write real binary");
        std::fs::set_permissions(&real_binary, std::fs::Permissions::from_mode(0o755))
            .expect("mark real binary executable");
        let search_path =
            std::env::join_paths([shadow.path(), real.path()]).expect("build search path");

        assert_eq!(
            find_exact(OsStr::new("go"), &search_path),
            Some(real_binary)
        );
    }

    #[test]
    fn extension_search_ignores_an_extensionless_shell_shim() {
        let dir = tempfile::tempdir().expect("create temp dir");
        std::fs::write(dir.path().join("npm"), "#!/bin/sh\n").expect("write shell shim");
        std::fs::write(dir.path().join("npm.cmd"), "@echo off\n").expect("write cmd shim");
        let search_path = std::env::join_paths([dir.path()]).expect("build search path");

        let found = find_with_extensions(
            OsStr::new("npm"),
            &search_path,
            OsStr::new(".com;.exe;.bat;.cmd"),
        );

        assert_eq!(found, Some(dir.path().join("npm.cmd")));
    }
}
