//! Run-scoped JSON recording; domain observations never own user files or directories.

mod layer;
mod lock;
mod paths;
pub(crate) mod prune;
mod read;
mod writer;

pub(crate) use layer::TraceLayer;
pub use read::{Records, records};

use cooldown_core::CoreError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};
use std::io::{BufRead as _, Read as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

/// Version 2 records repeated identical artifact content as a `duplicate_of` reference.
/// Version 3 compresses the file and, unless the trace is full, records release lists as
/// `release_set`s, rejected releases in their evaluation, and native output once complete.
const SCHEMA_VERSION: u32 = 3;
/// The oldest schema whose files this producer still recognizes as its own, so pruning keeps
/// removing traces written before the current version.
const OLDEST_OWNED_SCHEMA_VERSION: u32 = 1;
const PRODUCER: &str = "cooldown";

#[derive(Clone, Copy)]
pub(crate) enum TraceCommand {
    Upgrade,
    Fix,
    Outdated,
}

impl TraceCommand {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Upgrade => "upgrade",
            Self::Fix => "fix",
            Self::Outdated => "outdated",
        }
    }
}

/// The file name suffix of every trace this producer writes.
const EXTENSION: &str = ".jsonl.zst";
/// The file name suffix of traces written before they were compressed, which pruning still owns.
const UNCOMPRESSED_EXTENSION: &str = ".jsonl";

pub(crate) struct Recorder {
    path: PathBuf,
    run_id: String,
    started: Instant,
    /// The trace file's identity, since only the writer thread holds the file itself.
    identity: same_file::Handle,
    state: Mutex<State>,
}

struct State {
    /// The writer of a recording that has not finished.
    writer: Option<writer::Writer>,
    sequence: u64,
    error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Record<'a> {
    producer: &'static str,
    schema_version: u32,
    run_id: &'a str,
    sequence: u64,
    timestamp: jiff::Timestamp,
    elapsed_us: u64,
    event: &'a RawValue,
    spans: &'a [Value],
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    producer: String,
    schema_version: u32,
    sequence: u64,
    run_id: String,
    event: HeaderEvent,
}

#[derive(Deserialize)]
struct HeaderEvent {
    kind: String,
}

impl Header {
    fn read(file: &std::fs::File) -> std::io::Result<Option<Self>> {
        let mut header = String::new();
        read::decoded(file)?
            .take(1024 * 1024)
            .read_line(&mut header)?;
        Ok(serde_json::from_str(&header).ok())
    }

    fn owned(&self) -> bool {
        self.producer == PRODUCER
            && (OLDEST_OWNED_SCHEMA_VERSION..=SCHEMA_VERSION).contains(&self.schema_version)
            && self.sequence == 0
            && self.event.kind == "run_started"
            && !self.run_id.is_empty()
    }
}

impl Recorder {
    pub(crate) fn start(
        directory: &Path,
        command: TraceCommand,
        override_directory: Option<&Path>,
        detail: cooldown_core::trace::Detail,
    ) -> Result<Arc<Self>, CoreError> {
        Self::in_directory(
            &override_directory.map_or_else(paths::directory, |path| Ok(path.to_owned()))?,
            directory,
            command,
            detail,
        )
    }

    fn in_directory(
        storage: &Path,
        directory: &Path,
        command: TraceCommand,
        detail: cooldown_core::trace::Detail,
    ) -> Result<Arc<Self>, CoreError> {
        std::fs::create_dir_all(storage)?;
        let repository = directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("run");
        let repository: String = repository
            .chars()
            .take(64)
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                    character
                } else {
                    '_'
                }
            })
            .collect();
        let timestamp = jiff::Timestamp::now().to_string().replace([':', '.'], "-");
        let prefix = format!("{}-{timestamp}-{repository}-", command.as_str());
        let temporary = tempfile::Builder::new()
            .prefix(&prefix)
            .suffix(EXTENSION)
            .tempfile_in(storage)?;
        lock::try_lock(temporary.as_file()).map_err(|err| {
            CoreError::System(format!("lock trace {}: {err}", temporary.path().display()))
        })?;
        let (file, path) = temporary.keep().map_err(|err| CoreError::from(err.error))?;
        let run_id = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(EXTENSION))
            .unwrap_or("run")
            .to_string();
        let identity = same_file::Handle::from_file(file.try_clone()?)?;
        Ok(Arc::new(Self {
            path,
            run_id,
            started: Instant::now(),
            identity,
            state: Mutex::new(State {
                writer: Some(writer::Writer::spawn(file, detail)?),
                sequence: 0,
                error: None,
            }),
        }))
    }

    fn is_trace_input(&self, path: &Path) -> bool {
        if !std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
            return false;
        }
        let Ok(file) = std::fs::File::open(path) else {
            return false;
        };
        if file
            .try_clone()
            .and_then(same_file::Handle::from_file)
            .is_ok_and(|candidate| candidate == self.identity)
        {
            return true;
        }
        has_trace_extension(path)
            && Header::read(&file).is_ok_and(|header| header.is_some_and(|header| header.owned()))
    }

    pub(crate) fn record(&self, event: &RawValue, spans: &[Value]) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.error.is_some() {
            return;
        }
        let Some(writer) = &state.writer else {
            return;
        };
        let record = Record {
            producer: PRODUCER,
            schema_version: SCHEMA_VERSION,
            run_id: &self.run_id,
            sequence: state.sequence,
            timestamp: jiff::Timestamp::now(),
            elapsed_us: cooldown_core::trace::duration_us(self.started.elapsed()),
            event,
            spans,
        };
        let written = serde_json::to_vec(&record)
            .map_err(|err| err.to_string())
            .and_then(|mut line| {
                line.push(b'\n');
                writer.send(line).map_err(str::to_string)
            })
            // The header is in the file before the run goes on, so pruning and other runs that
            // capture this file as an input recognize it as an active trace.
            .and_then(|()| {
                if state.sequence == 0 {
                    writer.flush().map_err(str::to_string)
                } else {
                    Ok(())
                }
            });
        match written {
            Ok(()) => state.sequence += 1,
            Err(err) => state.error = Some(err),
        }
    }

    /// Returns once every record so far is in the file.
    #[cfg(test)]
    fn flush(&self) {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(writer) = &state.writer
            && let Err(err) = writer.flush()
        {
            drop(state);
            self.fail(err.to_string());
        }
    }

    pub(crate) fn fail(&self, error: String) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.error.is_none() {
            state.error = Some(error);
        }
    }

    pub(crate) fn finish(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(writer) = state.writer.take() else {
            return;
        };
        // Records that raced with the end of the run see no writer and are dropped, so the
        // terminal record stays last.
        drop(state);
        let finished = writer.finish();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(err) = finished
            && state
                .error
                .as_deref()
                .is_none_or(|error| error == writer::STOPPED)
        {
            state.error = Some(err);
        }
        if let Some(error) = &state.error {
            eprintln!(
                "cooldown: warning: trace is incomplete at {}: {error}",
                self.path.display()
            );
        } else {
            eprintln!("cooldown: trace recorded at {}", self.path.display());
        }
    }
}

impl Drop for Recorder {
    /// Ends a recording that was never finished, so its frame is complete and its file unlocked
    /// once the recorder is gone.
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(writer) = state.writer.take()
            && let Err(err) = writer.finish()
        {
            eprintln!(
                "cooldown: warning: trace is incomplete at {}: {err}",
                self.path.display()
            );
        }
    }
}

/// Whether `path` is named like a trace, compressed or not.
fn has_trace_extension(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name.ends_with(EXTENSION) || name.ends_with(UNCOMPRESSED_EXTENSION)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;

    /// Every record of a trace that ended cleanly.
    fn read(path: &Path) -> eyre::Result<Vec<Value>> {
        let mut records = records(path)?;
        let read = records.by_ref().collect::<Result<Vec<_>, _>>()?;
        eyre::ensure!(
            !records.truncated(),
            "trace {} is truncated",
            path.display()
        );
        Ok(read)
    }

    #[test]
    fn concurrent_recorders_use_unique_files() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let first = Recorder::in_directory(
            directory.path(),
            Path::new("repository"),
            TraceCommand::Upgrade,
            cooldown_core::trace::Detail::Compact,
        )?;
        let second = Recorder::in_directory(
            directory.path(),
            Path::new("repository"),
            TraceCommand::Upgrade,
            cooldown_core::trace::Detail::Compact,
        )?;
        assert_ne!(first.path, second.path);
        let event = serde_json::value::to_raw_value(&serde_json::json!({"kind":"test"}))?;
        first.record(&event, &[]);
        first.record(&event, &[]);
        first.finish();
        second.finish();
        let records = read(&first.path)?;
        assert_eq!(records.len(), 2);
        let [start, end] = records.as_slice() else {
            eyre::bail!("two events expected");
        };
        assert_eq!(start["sequence"], 0);
        assert_eq!(end["sequence"], 1);
        assert!(start["elapsedUs"].as_u64() <= end["elapsedUs"].as_u64());
        assert_eq!(start["runId"], end["runId"]);
        Ok(())
    }

    #[test]
    fn trace_inputs_are_excluded_without_dropping_native_output() -> eyre::Result<()> {
        use cooldown_core::trace::{ArtifactSource, InputRole};
        use tracing_subscriber::prelude::*;
        let directory = tempfile::tempdir()?;
        let recorder = Recorder::in_directory(
            directory.path(),
            Path::new("project"),
            TraceCommand::Outdated,
            cooldown_core::trace::Detail::Compact,
        )?;
        let peer = Recorder::in_directory(
            directory.path(),
            Path::new("peer"),
            TraceCommand::Upgrade,
            cooldown_core::trace::Detail::Compact,
        )?;
        let header = serde_json::value::to_raw_value(&serde_json::json!({"kind": "run_started"}))?;
        recorder.record(&header, &[]);
        peer.record(&header, &[]);
        let bytes = std::fs::read(&recorder.path)?;
        let alias = directory.path().join("trace-input-alias");
        std::fs::hard_link(&recorder.path, &alias)?;
        let native = directory.path().join("native-input");
        std::fs::write(&native, "actual resolver input")?;
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(
            TraceLayer::new(Arc::clone(&recorder)).with_filter(
                tracing_subscriber::filter::filter_fn(|metadata| {
                    metadata.target().starts_with("cooldown")
                }),
            ),
        ));
        let paths = [&recorder.path, &alias, &peer.path]
            .into_iter()
            .map(|path| {
                path.to_str()
                    .ok_or_else(|| eyre::eyre!("UTF-8 fixture path expected"))
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        let native_path = native
            .to_str()
            .ok_or_else(|| eyre::eyre!("UTF-8 fixture path expected"))?;
        tracing::dispatcher::with_default(&dispatch, || {
            for path in paths {
                cooldown_core::trace::artifact(
                    ArtifactSource::File {
                        path,
                        role: InputRole::ResolverInput,
                    },
                    &bytes,
                );
            }
            cooldown_core::trace::artifact(ArtifactSource::Stdout { operation: 42 }, &bytes);
            cooldown_core::trace::artifact(
                ArtifactSource::File {
                    path: native_path,
                    role: InputRole::ResolverInput,
                },
                b"actual resolver input",
            );
        });
        recorder.finish();
        peer.finish();
        let records = read(&recorder.path)?;
        let excluded = records
            .iter()
            .filter(|record| {
                record["event"]["kind"] == "artifact_finished"
                    && record["event"]["outcome"]["kind"] == "failed"
            })
            .count();
        assert_eq!(excluded, 3);
        let chunks: Vec<_> = records
            .iter()
            .filter(|record| record["event"]["kind"] == "artifact_chunk")
            .collect();
        assert_eq!(chunks.len(), 2);
        let trace_bytes = serde_json::to_value(cooldown_core::trace::Content::from_bytes(&bytes))?;
        assert!(
            chunks
                .iter()
                .any(|record| record["event"]["content"] == trace_bytes)
        );
        assert!(
            chunks
                .iter()
                .any(|record| record["event"]["content"]["data"] == "actual resolver input")
        );
        Ok(())
    }

    /// A run that captures the same bytes again, as every mutation checkpoint does for unchanged
    /// manifests, writes them once and records later captures as references to that copy.
    #[test]
    fn identical_artifact_content_is_written_once_per_run() -> eyre::Result<()> {
        use cooldown_core::trace::{ArtifactSource, InputRole};
        use tracing_subscriber::prelude::*;
        let directory = tempfile::tempdir()?;
        let recorder = Recorder::in_directory(
            directory.path(),
            Path::new("project"),
            TraceCommand::Upgrade,
            cooldown_core::trace::Detail::Compact,
        )?;
        let header = serde_json::value::to_raw_value(&serde_json::json!({"kind": "run_started"}))?;
        recorder.record(&header, &[]);
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(
            TraceLayer::new(Arc::clone(&recorder)).with_filter(
                tracing_subscriber::filter::filter_fn(|metadata| {
                    metadata.target().starts_with("cooldown")
                }),
            ),
        ));
        let manifest = |path| ArtifactSource::File {
            path,
            role: InputRole::ResolverInput,
        };
        tracing::dispatcher::with_default(&dispatch, || {
            cooldown_core::trace::artifact(manifest("/a/Cargo.toml"), b"[package]");
            cooldown_core::trace::artifact(manifest("/b/Cargo.toml"), b"[package]");
            cooldown_core::trace::artifact(manifest("/c/Cargo.toml"), b"[workspace]");
        });
        recorder.finish();
        let records = read(&recorder.path)?;
        let events = |kind: &str| -> Vec<Value> {
            records
                .iter()
                .filter(|record| record["event"]["kind"] == kind)
                .map(|record| record["event"].clone())
                .collect()
        };
        let started = events("artifact_started");
        let [first, repeat, other] = started.as_slice() else {
            eyre::bail!("expected three artifacts, got {started:?}");
        };

        // Every capture states its content hash; only the repeat refers back to the first copy.
        assert_eq!(first["sha256"], repeat["sha256"]);
        assert_ne!(first["sha256"], other["sha256"]);
        assert_eq!(repeat["duplicate_of"], first["artifact"]);
        assert!(first.get("duplicate_of").is_none() && other.get("duplicate_of").is_none());

        // The repeat's bytes are written once, under the first artifact, and every capture still
        // records its complete outcome and original length.
        let chunk_owners: Vec<Value> = events("artifact_chunk")
            .iter()
            .map(|chunk| chunk["artifact"].clone())
            .collect();
        assert_eq!(
            chunk_owners,
            [first["artifact"].clone(), other["artifact"].clone()]
        );
        let finished = events("artifact_finished");
        assert_eq!(finished.len(), 3);
        assert!(
            finished
                .iter()
                .all(|record| record["outcome"]["kind"] == "complete")
        );
        assert!(
            finished
                .iter()
                .any(|record| record["artifact"] == repeat["artifact"] && record["bytes"] == 9)
        );
        Ok(())
    }
}
