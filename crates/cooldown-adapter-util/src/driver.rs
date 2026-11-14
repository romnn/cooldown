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

/// Runs `command` to completion and collects its output, as [`Command::output`] does, under
/// cooperative interruption.
///
/// Standard input is closed and both output streams are captured.
/// A closed input matters: a package manager that sees a terminal there may stop at an
/// interactive prompt (pnpm asks before adding to `minimumReleaseAgeExclude`) that nobody sees,
/// because its output is captured, and so wait forever.
///
/// On Unix the child leads its own process group, registered with [`cooldown_core::interrupt`],
/// so a signal to cooldown terminates it with every descendant.
/// The child is also killed if the returned future is dropped.
/// A spawn after the run was interrupted is refused, and a child that finishes once the run was
/// interrupted reports the interruption whatever its status: a resolver that exited early on the
/// signal neither counts as a candidate conflict nor hands back a partial result as success.
/// `invocation` renders the command line for the spawn error.
///
/// # Errors
///
/// Returns [`cooldown_core::interrupt::interrupted`] once the run is interrupted, or
/// [`CoreError::ToolSpawn`] if the process cannot be started or waited for.
/// A non-zero exit is not an error here; callers inspect the returned status.
pub async fn supervised_output(
    command: &mut Command,
    tool: &str,
    invocation: impl FnOnce() -> String,
) -> Result<std::process::Output> {
    observed_output(command, tool, invocation, InvocationKind::Native).await
}

#[derive(Clone, Copy)]
enum InvocationKind {
    Native,
    VersionQuery,
}

async fn observed_output(
    command: &mut Command,
    tool: &str,
    invocation: impl FnOnce() -> String,
    kind: InvocationKind,
) -> Result<std::process::Output> {
    let observation = cooldown_core::trace::enabled().then(|| {
        let operation = cooldown_core::trace::operation_id();
        let native = command.as_std();
        let arguments: Vec<String> = native
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        let environment: Vec<_> = native
            .get_envs()
            .map(|(name, value)| cooldown_core::trace::EnvironmentOverride {
                name: name.to_string_lossy().into_owned(),
                value: value.map(|value| value.to_string_lossy().into_owned()),
            })
            .collect();
        cooldown_core::trace::Event::CommandStarted {
            operation,
            tool,
            program: &native.get_program().to_string_lossy(),
            arguments: &arguments,
            directory: native.get_current_dir().and_then(Path::to_str),
            environment: &environment,
        }
        .emit();
        CommandObservation {
            completed: false,
            operation,
            started: std::time::Instant::now(),
        }
    });
    let operation = observation
        .as_ref()
        .map(|observation| observation.operation);
    let result = supervised_output_inner(command, tool, invocation, operation, kind).await;
    if let Some(mut observation) = observation {
        let termination = result
            .as_ref()
            .ok()
            .map(|output| ToolTermination::from_exit_status(output.status));
        let error = result.as_ref().err().map(ToString::to_string).or_else(|| {
            cooldown_core::interrupt::requested().then(|| {
                cooldown_core::interrupt::interrupted(&format!("running `{tool}`")).to_string()
            })
        });
        observation.completed = true;
        cooldown_core::trace::Event::CommandFinished {
            operation: observation.operation,
            duration_us: cooldown_core::trace::duration_us(observation.started.elapsed()),
            exit_code: result.as_ref().ok().and_then(|output| output.status.code()),
            termination,
            error: error.as_deref(),
        }
        .emit();
    }
    if cooldown_core::interrupt::requested() {
        return Err(cooldown_core::interrupt::interrupted(&format!(
            "running `{tool}`"
        )));
    }
    result
}

struct CommandObservation {
    completed: bool,
    operation: u64,
    started: std::time::Instant,
}

impl Drop for CommandObservation {
    fn drop(&mut self) {
        if !self.completed {
            cooldown_core::trace::Event::CommandFinished {
                operation: self.operation,
                duration_us: cooldown_core::trace::duration_us(self.started.elapsed()),
                exit_code: None,
                termination: None,
                error: Some("command future was cancelled before output collection completed"),
            }
            .emit();
        }
    }
}

struct RunningChild {
    registration: cooldown_core::interrupt::ChildRegistration,
    completed: bool,
}

impl Drop for RunningChild {
    fn drop(&mut self) {
        if !self.completed {
            self.registration.kill();
        }
    }
}

async fn supervised_output_inner(
    command: &mut Command,
    tool: &str,
    invocation: impl FnOnce() -> String,
    operation: Option<u64>,
    kind: InvocationKind,
) -> Result<std::process::Output> {
    let activity = || format!("running `{tool}`");
    cooldown_core::interrupt::ensure_not_requested(&activity())?;
    #[cfg(unix)]
    command.process_group(0);
    let spawn_error = |error: std::io::Error| CoreError::ToolSpawn {
        tool: tool.to_string(),
        detail: format!("`{}`: {error}", invocation()),
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match kind {
        InvocationKind::Native => command
            .spawn()
            .map(|child| Box::new(child) as Box<dyn process_wrap::tokio::ChildWrapper>),
        InvocationKind::VersionQuery => spawn_version_query(command),
    }
    .map_err(spawn_error)?;
    let mut registration = child.id().map(|pid| RunningChild {
        registration: cooldown_core::interrupt::register_child(pid),
        completed: false,
    });
    let stdout = child.stdout().take();
    let stderr = child.stderr().take();
    let waited = tokio::try_join!(
        child.wait(),
        read_output(
            stdout,
            operation.map(|operation| cooldown_core::trace::ArtifactSource::Stdout { operation })
        ),
        read_output(
            stderr,
            operation.map(|operation| cooldown_core::trace::ArtifactSource::Stderr { operation })
        ),
    )
    .map(|(status, stdout, stderr)| std::process::Output {
        status,
        stdout,
        stderr,
    });
    if let Some(registration) = &mut registration {
        registration.completed = waited.is_ok();
    }
    drop(registration);
    let output = waited.map_err(|error| CoreError::ToolSpawn {
        tool: tool.to_string(),
        detail: format!("waiting for `{tool}`: {error}"),
    })?;
    Ok(output)
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

#[cfg(not(windows))]
fn spawn_version_query(
    command: &mut Command,
) -> std::io::Result<Box<dyn process_wrap::tokio::ChildWrapper>> {
    command
        .spawn()
        .map(|child| Box::new(child) as Box<dyn process_wrap::tokio::ChildWrapper>)
}

#[cfg(windows)]
fn spawn_version_query(
    command: &mut Command,
) -> std::io::Result<Box<dyn process_wrap::tokio::ChildWrapper>> {
    use process_wrap::tokio::{CommandWrap, JobObject, KillOnDrop};
    let program = command.as_std().get_program().to_owned();
    let original = std::mem::replace(command, Command::new(program));
    let mut wrapped = CommandWrap::from(original);
    let spawned = wrapped.wrap(KillOnDrop).wrap(JobObject).spawn();
    *command = wrapped.into_command();
    // Job assignment temporarily suspends creation; a reused builder must not inherit suspension.
    command.creation_flags(0);
    spawned
}

/// One native output stream, retained for the driver and recorded in the trace's detail.
///
/// A full trace records each chunk as it arrives, keeping partial output of a process that is
/// still running.
/// A compact trace records the stream once it ends, including when the read fails or is
/// cancelled, so identical output refers to an earlier copy instead of repeating it.
struct OutputCapture<'s> {
    source: Option<cooldown_core::trace::ArtifactSource<'s>>,
    /// The run's subscriber, which the stream's end is recorded to.
    ///
    /// A cancelled capture is dropped wherever its future is, possibly outside the run's
    /// subscriber or inside another run's.
    dispatch: tracing::Dispatch,
    /// The artifact a full trace streams chunks into.
    streamed: Option<u64>,
    output: Vec<u8>,
    completed: bool,
}

impl<'s> OutputCapture<'s> {
    fn start(source: Option<cooldown_core::trace::ArtifactSource<'s>>) -> Self {
        let detail = cooldown_core::trace::Detail::current();
        let source = source.filter(|_| detail.is_some());
        let streamed = source
            .filter(|_| detail == Some(cooldown_core::trace::Detail::Full))
            .map(|source| {
                let artifact = cooldown_core::trace::operation_id();
                cooldown_core::trace::Event::ArtifactStarted {
                    artifact,
                    snapshot: None,
                    source,
                    size: None,
                    sha256: None,
                }
                .emit();
                artifact
            });
        Self {
            source,
            dispatch: tracing::dispatcher::get_default(tracing::Dispatch::clone),
            streamed,
            output: Vec::new(),
            completed: false,
        }
    }

    fn chunk(&mut self, bytes: &[u8]) {
        if let Some(artifact) = self.streamed {
            cooldown_core::trace::Event::ArtifactChunk {
                artifact,
                offset: u64::try_from(self.output.len()).unwrap_or(u64::MAX),
                content: cooldown_core::trace::Content::from_bytes(bytes),
            }
            .emit();
        }
        self.output.extend_from_slice(bytes);
    }

    /// Records the stream's end and returns everything it carried.
    fn finish(&mut self, outcome: cooldown_core::trace::ArtifactOutcome<'_>) -> Vec<u8> {
        tracing::dispatcher::with_default(&self.dispatch, || match (self.streamed, self.source) {
            (Some(artifact), _) => cooldown_core::trace::Event::ArtifactFinished {
                artifact,
                bytes: u64::try_from(self.output.len()).unwrap_or(u64::MAX),
                outcome,
            }
            .emit(),
            (None, Some(source)) => {
                cooldown_core::trace::captured_artifact(source, &self.output, outcome);
            }
            (None, None) => {}
        });
        self.completed = true;
        std::mem::take(&mut self.output)
    }
}

impl Drop for OutputCapture<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.finish(cooldown_core::trace::ArtifactOutcome::Failed {
                reason: "stream capture cancelled before EOF",
            });
        }
    }
}

async fn read_output<R: tokio::io::AsyncRead + Unpin>(
    stream: Option<R>,
    source: Option<cooldown_core::trace::ArtifactSource<'_>>,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut capture = OutputCapture::start(source);
    if let Some(mut stream) = stream {
        let mut buffer = vec![0; 64 * 1024];
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) => break,
                Ok(count) => {
                    if let Some(bytes) = buffer.get(..count) {
                        capture.chunk(bytes);
                    }
                }
                Err(err) => {
                    capture.finish(cooldown_core::trace::ArtifactOutcome::Failed {
                        reason: &err.to_string(),
                    });
                    return Err(err);
                }
            }
        }
    }
    Ok(capture.finish(cooldown_core::trace::ArtifactOutcome::Complete))
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

    /// Creates a handle to exactly `program`, bypassing the `COOLDOWN_<BIN>` override — for
    /// callers (hermetic tests, reproducible pipelines) that must pin the executable without
    /// touching the process environment, which parallel tests cannot share.
    #[must_use]
    pub fn from_program(program: impl Into<String>) -> Self {
        Driver {
            bin: program.into(),
        }
    }

    /// The resolved binary name (after any `COOLDOWN_<BIN>` override).
    #[must_use]
    pub fn program(&self) -> &str {
        &self.bin
    }

    async fn output(
        &self,
        dir: &Utf8Path,
        args: &[String],
        kind: InvocationKind,
    ) -> Result<std::process::Output> {
        // `--log-level trace` documents itself as covering subprocess argv; this is the one spawn
        // point every adapter driver funnels through, so the promise is kept here.
        tracing::trace!(tool = %self.bin, %dir, args = %args.join(" "), "spawning subprocess");
        let out = observed_output(
            Command::new(resolve_program(&self.bin))
                .args(args)
                .current_dir(dir.as_std_path()),
            &self.bin,
            || format!("{} {}", self.bin, args.join(" ")),
            kind,
        )
        .await?;
        if !out.status.success() {
            tracing::debug!(
                tool = %self.bin,
                status = %out.status,
                detail = %failure_detail(&out),
                "subprocess failed"
            );
        }
        Ok(out)
    }

    /// Runs the driver, mapping a non-zero exit to a [`CoreError::Tool`].
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if the binary cannot be spawned, or [`CoreError::Tool`] if
    /// it exits non-zero (e.g. an unsatisfiable pin — a resolver conflict).
    pub async fn run(&self, dir: &Utf8Path, args: &[String]) -> Result<()> {
        let out = self.output(dir, args, InvocationKind::Native).await?;
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

    /// Runs the driver and returns its standard output.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ToolSpawn`] if the binary cannot be spawned, or [`CoreError::Tool`] if
    /// it exits non-zero.
    pub async fn stdout(&self, dir: &Utf8Path, args: &[String]) -> Result<String> {
        self.stdout_kind(dir, args, InvocationKind::Native).await
    }

    /// Collects diagnostic version output, protecting Windows descendants when its future is cancelled.
    ///
    /// The caller supplies a deadline; ordinary driver commands retain their native process lifetime.
    ///
    /// # Errors
    ///
    /// Returns the same spawn and non-zero exit errors as [`Self::stdout`].
    pub async fn version_stdout(&self, dir: &Utf8Path, args: &[String]) -> Result<String> {
        self.stdout_kind(dir, args, InvocationKind::VersionQuery)
            .await
    }

    async fn stdout_kind(
        &self,
        dir: &Utf8Path,
        args: &[String],
        kind: InvocationKind,
    ) -> Result<String> {
        let out = self.output(dir, args, kind).await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
        let out = self.output(dir, args, InvocationKind::Native).await?;
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

    #[cfg(windows)]
    #[tokio::test]
    async fn cancelling_a_wrapper_stops_its_windows_descendant() -> cooldown_core::Result<()> {
        use std::time::Duration;
        let directory = tempfile::tempdir()?;
        let marker = directory.path().join("descendant.txt");
        let marker_argument = marker.to_string_lossy().replace('\'', "''");
        let script = indoc::formatdoc! {r#"
            $start = New-Object System.Diagnostics.ProcessStartInfo
            $start.FileName = 'powershell'
            $start.Arguments = '-NoProfile -NonInteractive -Command "Start-Sleep -Seconds 120"'
            $start.UseShellExecute = $false
            $child = [System.Diagnostics.Process]::Start($start)
            Set-Content -LiteralPath '{marker_argument}' -Value $child.Id
            Start-Sleep -Seconds 120
        "#};
        let mut command = tokio::process::Command::new("powershell");
        command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        let result = tokio::time::timeout(
            Duration::from_secs(8),
            super::observed_output(
                &mut command,
                "powershell",
                || "powershell descendant fixture".into(),
                super::InvocationKind::VersionQuery,
            ),
        )
        .await;
        assert!(
            result.is_err(),
            "the supervised wrapper must reach cancellation"
        );
        let pid = std::fs::read_to_string(marker)?
            .trim()
            .parse::<u32>()
            .map_err(|err| cooldown_core::CoreError::System(err.to_string()))?;
        let probe = format!(
            "Wait-Process -Id {pid} -Timeout 5 -ErrorAction SilentlyContinue; if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 1 }} else {{ exit 0 }}"
        );
        let mut command = tokio::process::Command::new("powershell");
        command
            .args(["-NoProfile", "-NonInteractive", "-Command", &probe])
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(15), command.output())
            .await
            .map_err(|err| cooldown_core::CoreError::System(err.to_string()))??;
        assert!(
            output.status.success(),
            "the descendant must be stopped by job closure"
        );
        Ok(())
    }

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

    mod output_capture {
        use cooldown_core::trace::{ArtifactSource, Detail, TARGET};
        use std::sync::{Arc, Mutex, PoisonError};
        use tokio::io::AsyncWriteExt as _;
        use tracing_subscriber::prelude::*;

        /// The typed observations a recorder at some detail receives, in order.
        #[derive(Clone, Default)]
        struct Observed(Arc<Mutex<Vec<serde_json::Value>>>);

        impl Observed {
            fn events(&self) -> Vec<serde_json::Value> {
                self.0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
            }

            fn kinds(&self) -> Vec<String> {
                self.events()
                    .iter()
                    .filter_map(|event| event["kind"].as_str().map(str::to_string))
                    .collect()
            }

            /// Installs a subscriber that records typed observations up to `detail`.
            fn install(&self, detail: Detail) -> tracing::dispatcher::DefaultGuard {
                tracing::dispatcher::set_default(&self.dispatch(detail))
            }

            /// A subscriber that records typed observations up to `detail`.
            fn dispatch(&self, detail: Detail) -> tracing::Dispatch {
                let level = match detail {
                    Detail::Full => tracing::Level::TRACE,
                    Detail::Compact => tracing::Level::DEBUG,
                };
                tracing::Dispatch::new(
                    tracing_subscriber::registry().with(self.clone().with_filter(
                        tracing_subscriber::filter::filter_fn(move |metadata| {
                            metadata.target() == TARGET && *metadata.level() <= level
                        }),
                    )),
                )
            }
        }

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Observed {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                struct Payload(Option<serde_json::Value>);
                impl tracing::field::Visit for Payload {
                    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                        if field.name() == "trace_event" {
                            self.0 = serde_json::from_str(value).ok();
                        }
                    }
                    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {
                    }
                }
                let mut payload = Payload(None);
                event.record(&mut payload);
                if let Some(payload) = payload.0 {
                    self.0
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(payload);
                }
            }
        }

        const STDOUT: ArtifactSource<'static> = ArtifactSource::Stdout { operation: 7 };

        /// A compact trace records a stream once it ends, with the hash that lets identical output
        /// refer to an earlier copy, and a full trace streams it as it arrives.
        #[tokio::test]
        async fn compact_output_is_recorded_whole_and_full_output_as_it_arrives()
        -> cooldown_core::Result<()> {
            for detail in [Detail::Compact, Detail::Full] {
                let observed = Observed::default();
                let _subscriber = observed.install(detail);
                let (mut native, stream) = tokio::io::duplex(1024);
                let reading = tokio::spawn(async move {
                    super::super::read_output(Some(stream), Some(STDOUT)).await
                });
                native.write_all(b"first line\n").await?;
                // Let the reader take the first chunk while the stream stays open.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let while_running = observed.kinds();
                native.write_all(b"second line\n").await?;
                drop(native);
                let output = reading
                    .await
                    .map_err(|err| cooldown_core::CoreError::System(err.to_string()))??;
                assert_eq!(output, b"first line\nsecond line\n");

                let events = observed.events();
                let started = events
                    .iter()
                    .find(|event| event["kind"] == "artifact_started")
                    .ok_or_else(|| cooldown_core::CoreError::System("no start".into()))?;
                let finished = events
                    .iter()
                    .find(|event| event["kind"] == "artifact_finished")
                    .ok_or_else(|| cooldown_core::CoreError::System("no finish".into()))?;
                assert_eq!(finished["outcome"]["kind"], "complete");
                assert_eq!(finished["bytes"], 23);
                match detail {
                    // Nothing is recorded while the process runs; the whole output then follows
                    // its hash in one chunk.
                    Detail::Compact => {
                        assert!(while_running.is_empty(), "{while_running:?}");
                        assert_eq!(
                            started["sha256"],
                            "c2097f55f01fc297fc7f4acf21438123e06e4d409a818524428534e850642f4f"
                        );
                        assert_eq!(started["size"], 23);
                    }
                    // The first chunk is in the trace before the stream ends.
                    Detail::Full => {
                        assert_eq!(while_running, ["artifact_started", "artifact_chunk"]);
                        assert!(started.get("sha256").is_none(), "{started}");
                    }
                }
            }
            Ok(())
        }

        /// A compact capture that is cancelled, as an interrupted or timed-out command's is, still
        /// records the output it had received.
        #[tokio::test]
        async fn a_cancelled_compact_capture_records_its_partial_output()
        -> cooldown_core::Result<()> {
            let observed = Observed::default();
            let _subscriber = observed.install(Detail::Compact);
            let (mut native, stream) = tokio::io::duplex(1024);
            native.write_all(b"partial").await?;
            let cancelled = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                super::super::read_output(Some(stream), Some(STDOUT)),
            )
            .await;
            assert!(cancelled.is_err(), "the read waits for more output");
            let events = observed.events();
            assert_eq!(
                observed.kinds(),
                ["artifact_started", "artifact_chunk", "artifact_finished"]
            );
            assert!(
                events
                    .iter()
                    .any(|event| event["content"]["data"] == "partial")
            );
            assert!(
                events
                    .iter()
                    .any(|event| event["outcome"]["kind"] == "failed" && event["bytes"] == 7)
            );
            drop(native);
            Ok(())
        }

        /// A run's capture that is dropped outside the run's subscriber, as a cancelled run's
        /// future can be, still records its output to that run and to no other.
        #[tokio::test]
        async fn a_capture_dropped_outside_its_run_records_to_that_run() -> cooldown_core::Result<()>
        {
            use tracing::instrument::WithSubscriber as _;
            let run = Observed::default();
            let other = Observed::default();
            let (mut native, stream) = tokio::io::duplex(1024);
            native.write_all(b"partial").await?;
            let mut reading = Box::pin(
                super::super::read_output(Some(stream), Some(STDOUT))
                    .with_subscriber(run.dispatch(Detail::Compact)),
            );
            // Poll the read under the run's subscriber until it waits for more output.
            let pending =
                tokio::time::timeout(std::time::Duration::from_millis(100), &mut reading).await;
            assert!(pending.is_err(), "the read waits for more output");
            assert_eq!(run.kinds(), Vec::<String>::new());

            // Cancel it while another run's subscriber is the current one.
            let _other = other.install(Detail::Compact);
            drop(reading);
            assert_eq!(
                run.kinds(),
                ["artifact_started", "artifact_chunk", "artifact_finished"]
            );
            assert_eq!(other.kinds(), Vec::<String>::new());
            drop(native);
            Ok(())
        }
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
