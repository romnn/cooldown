//! Run-scoped JSON recording; domain observations never own user files or directories.

mod layer;
mod lock;
mod paths;
pub(crate) mod prune;

pub(crate) use layer::TraceLayer;

use cooldown_core::CoreError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};
use std::io::{BufRead as _, BufReader, BufWriter, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

/// Version 2 records repeated identical artifact content as a `duplicate_of` reference.
const SCHEMA_VERSION: u32 = 2;
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

pub(crate) struct Recorder {
    path: PathBuf,
    run_id: String,
    started: Instant,
    state: Mutex<State>,
}

struct State {
    file: BufWriter<std::fs::File>,
    sequence: u64,
    error: Option<String>,
    finished: bool,
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
        BufReader::new(file)
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
    ) -> Result<Arc<Self>, CoreError> {
        Self::in_directory(
            &override_directory.map_or_else(paths::directory, |path| Ok(path.to_owned()))?,
            directory,
            command,
        )
    }

    fn in_directory(
        storage: &Path,
        directory: &Path,
        command: TraceCommand,
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
            .suffix(".jsonl")
            .tempfile_in(storage)?;
        lock::try_lock(temporary.as_file()).map_err(|err| {
            CoreError::System(format!("lock trace {}: {err}", temporary.path().display()))
        })?;
        let (file, path) = temporary.keep().map_err(|err| CoreError::from(err.error))?;
        let run_id = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("run")
            .to_string();
        Ok(Arc::new(Self {
            path,
            run_id,
            started: Instant::now(),
            state: Mutex::new(State {
                file: BufWriter::new(file),
                sequence: 0,
                error: None,
                finished: false,
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
        let candidate = file.try_clone().and_then(same_file::Handle::from_file);
        if let Ok(candidate) = candidate {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state
                .file
                .get_ref()
                .try_clone()
                .and_then(same_file::Handle::from_file)
                .is_ok_and(|owned| owned == candidate)
            {
                return true;
            }
        }
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            && Header::read(&file).is_ok_and(|header| header.is_some_and(|header| header.owned()))
    }

    pub(crate) fn record(&self, event: &RawValue, spans: &[Value]) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.error.is_some() || state.finished {
            return;
        }
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
        let written = serde_json::to_writer(&mut state.file, &record)
            .map_err(|err| err.to_string())
            .and_then(|()| {
                state
                    .file
                    .write_all(b"\n")
                    .and_then(|()| state.file.flush())
                    .map_err(|err| err.to_string())
            });
        match written {
            Ok(()) => state.sequence += 1,
            Err(err) => state.error = Some(err),
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
        state.finished = true;
        if let Err(err) = state
            .file
            .flush()
            .and_then(|()| state.file.get_ref().sync_data())
        {
            state.error.get_or_insert_with(|| err.to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;

    #[test]
    fn concurrent_recorders_use_unique_files_and_flush_each_event() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let first = Recorder::in_directory(
            directory.path(),
            Path::new("repository"),
            TraceCommand::Upgrade,
        )?;
        let second = Recorder::in_directory(
            directory.path(),
            Path::new("repository"),
            TraceCommand::Upgrade,
        )?;
        assert_ne!(first.path, second.path);
        let event = serde_json::value::to_raw_value(&serde_json::json!({"kind":"test"}))?;
        first.record(&event, &[]);
        first.record(&event, &[]);
        let lines = std::fs::read_to_string(&first.path)?;
        let records: Vec<Value> = lines
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
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
        )?;
        let peer =
            Recorder::in_directory(directory.path(), Path::new("peer"), TraceCommand::Upgrade)?;
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
        let records: Vec<Value> = std::fs::read_to_string(&recorder.path)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
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
        assert!(
            chunks
                .iter()
                .any(|record| record["event"]["content"]["data"]
                    == String::from_utf8_lossy(&bytes).as_ref())
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
        let records: Vec<Value> = std::fs::read_to_string(&recorder.path)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
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
