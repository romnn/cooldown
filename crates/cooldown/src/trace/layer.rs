use super::Recorder;
use cooldown_core::trace::Detail;
use serde_json::{Map, Value, value::RawValue};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

pub(crate) struct TraceLayer {
    recorder: Arc<Recorder>,
    excluded_artifacts: Mutex<HashSet<u64>>,
    contents: Mutex<Contents>,
    /// The release sets this recorder wrote, by hash.
    ///
    /// Its lock is held while a new set is recorded, so a set always precedes the evaluations
    /// that refer to it.
    release_sets: Mutex<HashSet<String>>,
}

/// Which artifact content this recorder wrote.
///
/// Repeated captures of unchanged content, such as every manifest a mutation checkpoint snapshots
/// or the same native output, are then written once per run.
#[derive(Default)]
struct Contents {
    /// The first artifact recorded completely for each content hash.
    ///
    /// An artifact becomes a reference target only after its terminal record, so a reference
    /// never precedes the bytes it names, even when the trace is cut short.
    written: HashMap<String, u64>,
    /// Artifacts whose content is still being recorded, with their hashes.
    pending: HashMap<u64, String>,
    /// Artifacts recorded as references to an earlier identical artifact, whose chunks are omitted.
    duplicates: HashSet<u64>,
}

impl TraceLayer {
    /// The run's recording layer: every event of cooldown's own crates, with typed observations
    /// enabled up to `detail`.
    ///
    /// Typed observations are emitted at DEBUG, and enabling TRACE as well is what makes them
    /// record in full (see [`Detail`]).
    pub(crate) fn recording<S>(recorder: Arc<Recorder>, detail: Detail) -> impl Layer<S>
    where
        S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        let typed_level = match detail {
            Detail::Full => tracing::Level::TRACE,
            Detail::Compact => tracing::Level::DEBUG,
        };
        Self::new(recorder).with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
            metadata.target().starts_with("cooldown")
                && (metadata.target() != cooldown_core::trace::TARGET
                    || *metadata.level() <= typed_level)
        }))
    }

    pub(crate) fn new(recorder: Arc<Recorder>) -> Self {
        Self {
            recorder,
            excluded_artifacts: Mutex::new(HashSet::new()),
            contents: Mutex::new(Contents::default()),
            release_sets: Mutex::new(HashSet::new()),
        }
    }

    /// Whether `artifact` is a recorded duplicate whose chunk this event carries.
    ///
    /// The terminal record of a duplicate still passes, so its outcome and byte count stay in the
    /// trace.
    fn skips_duplicate_chunk(&self, artifact: u64, finished: bool) -> bool {
        let mut contents = self.contents.lock().unwrap_or_else(PoisonError::into_inner);
        if !contents.duplicates.contains(&artifact) {
            return false;
        }
        if finished {
            contents.duplicates.remove(&artifact);
        }
        !finished
    }

    /// Returns the earlier artifact that already holds content `sha256` completely, or marks
    /// `artifact` as pending to become that artifact once it is complete.
    fn earlier_copy(&self, artifact: u64, sha256: String) -> Option<u64> {
        let mut contents = self.contents.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(&first) = contents.written.get(&sha256) {
            contents.duplicates.insert(artifact);
            return Some(first);
        }
        contents.pending.insert(artifact, sha256);
        None
    }

    /// Makes a pending artifact a reference target after its terminal record, when it captured
    /// its whole content.
    fn finished_copy(&self, artifact: u64, complete: bool) {
        let mut contents = self.contents.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(sha256) = contents.pending.remove(&artifact)
            && complete
        {
            contents.written.entry(sha256).or_insert(artifact);
        }
    }

    /// The lock on the written release sets when `sha256` is not among them yet, held until the
    /// new set is recorded.
    fn unwritten_release_set(&self, sha256: &str) -> Option<MutexGuard<'_, HashSet<String>>> {
        let release_sets = self
            .release_sets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (!release_sets.contains(sha256)).then_some(release_sets)
    }

    /// The JSON an event records: the payload of a typed observation, or a diagnostic of any other
    /// event.
    ///
    /// A payload that cannot be recorded fails the recording and yields `None`.
    fn payload(
        &self,
        metadata: &tracing::Metadata<'_>,
        mut fields: Fields,
    ) -> Option<Box<RawValue>> {
        if metadata.target() == cooldown_core::trace::TARGET {
            let Some(Value::String(payload)) = fields.values.remove("trace_event") else {
                self.recorder.fail(format!(
                    "typed event could not be serialized: {:?}",
                    fields.values
                ));
                return None;
            };
            RawValue::from_string(payload)
                .inspect_err(|err| self.recorder.fail(format!("invalid typed event: {err}")))
                .ok()
        } else {
            let event = serde_json::json!({
                "kind": "diagnostic",
                "level": metadata.level().as_str(),
                "target": metadata.target(),
                "fields": fields.values,
            });
            serde_json::value::to_raw_value(&event)
                .inspect_err(|err| self.recorder.fail(err.to_string()))
                .ok()
        }
    }

    /// Whether an event of `artifact` is dropped: it belongs to an excluded artifact, or it is a
    /// chunk of a duplicate.
    fn skips_artifact_event(&self, artifact: u64, finished: bool) -> bool {
        let excluded = {
            let mut excluded = self
                .excluded_artifacts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let contains = excluded.contains(&artifact);
            if contains && finished {
                excluded.remove(&artifact);
            }
            contains
        };
        excluded || self.skips_duplicate_chunk(artifact, finished)
    }

    /// `payload` as the trace records it: the start of a repeated artifact refers to the first
    /// copy instead of carrying its own.
    fn deduplicated(
        &self,
        payload: Box<RawValue>,
        artifact: Option<u64>,
        sha256: Option<String>,
        trace_input: bool,
    ) -> Result<Box<RawValue>, serde_json::Error> {
        // The trace's own bytes are excluded from the trace, so they never become the copy a
        // later artifact refers to.
        match (artifact, sha256) {
            (Some(artifact), Some(sha256)) if !trace_input => {
                match self.earlier_copy(artifact, sha256) {
                    Some(first) => duplicate_payload(&payload, first),
                    None => Ok(payload),
                }
            }
            _ => Ok(payload),
        }
    }
}

/// Marks an `artifact_started` payload as a reference to the earlier artifact `first`.
fn duplicate_payload(payload: &RawValue, first: u64) -> Result<Box<RawValue>, serde_json::Error> {
    let mut event: Value = serde_json::from_str(payload.get())?;
    if let Value::Object(fields) = &mut event {
        fields.insert("duplicate_of".to_string(), first.into());
    }
    serde_json::value::to_raw_value(&event)
}

#[derive(Default)]
struct Fields {
    values: Map<String, Value>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.values.insert(field.name().to_string(), value.into());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.values.insert(field.name().to_string(), value.into());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.values.insert(field.name().to_string(), value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.values.insert(field.name().to_string(), value.into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.values
            .insert(field.name().to_string(), format!("{value:?}").into());
    }
}

#[derive(Clone)]
struct SpanFields {
    operation: u64,
    parent: Option<u64>,
    name: &'static str,
    values: Map<String, Value>,
    started: std::time::Instant,
}

impl<S> Layer<S> for TraceLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut fields = Fields::default();
        attributes.record(&mut fields);
        let parent_span = attributes.parent().and_then(|id| ctx.span(id)).or_else(|| {
            attributes
                .is_contextual()
                .then(|| ctx.lookup_current())
                .flatten()
        });
        let parent = parent_span.as_ref().and_then(|span| {
            span.extensions()
                .get::<SpanFields>()
                .map(|fields| fields.operation)
        });
        let operation = cooldown_core::trace::operation_id();
        if let Ok(event) = serde_json::value::to_raw_value(&serde_json::json!({
            "kind": "operation_started", "operation": operation, "parent": parent,
            "name": attributes.metadata().name(), "fields": fields.values,
        })) {
            self.recorder.record(&event, &[]);
        }
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields {
                operation,
                parent,
                name: attributes.metadata().name(),
                values: fields.values,
                started: std::time::Instant::now(),
            });
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        if let Some(span) = ctx.span(id)
            && let Some(fields) = span.extensions_mut().get_mut::<SpanFields>()
        {
            let mut updated = Fields::default();
            values.record(&mut updated);
            fields.values.extend(updated.values);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let artifact = if event.metadata().target() == cooldown_core::trace::TARGET {
            fields.values.get("trace_artifact").and_then(Value::as_u64)
        } else {
            None
        };
        let finished = fields
            .values
            .get("trace_artifact_finished")
            .and_then(Value::as_bool)
            == Some(true);
        if artifact.is_some_and(|artifact| self.skips_artifact_event(artifact, finished)) {
            return;
        }
        let complete = fields
            .values
            .get("trace_artifact_complete")
            .and_then(Value::as_bool)
            == Some(true);
        let release_set = match fields.values.remove("trace_release_set") {
            Some(Value::String(sha256))
                if event.metadata().target() == cooldown_core::trace::TARGET =>
            {
                Some(sha256)
            }
            _ => None,
        };
        let mut release_sets = match &release_set {
            Some(sha256) => match self.unwritten_release_set(sha256) {
                Some(release_sets) => Some(release_sets),
                None => return,
            },
            None => None,
        };
        let input_path = if event.metadata().target() == cooldown_core::trace::TARGET {
            match fields.values.remove("trace_input_path") {
                Some(Value::String(path)) => Some(path),
                _ => None,
            }
        } else {
            None
        };
        let trace_input = input_path
            .as_deref()
            .is_some_and(|path| self.recorder.is_trace_input(std::path::Path::new(path)));
        let sha256 = match fields.values.remove("trace_artifact_sha256") {
            Some(Value::String(sha256)) => Some(sha256),
            _ => None,
        };
        let Some(payload) = self.payload(event.metadata(), fields) else {
            return;
        };
        let payload = match self.deduplicated(payload, artifact, sha256, trace_input) {
            Ok(payload) => payload,
            Err(err) => {
                self.recorder.fail(format!("invalid typed event: {err}"));
                return;
            }
        };
        let spans: Vec<Value> = ctx
            .event_scope(event)
            .into_iter()
            .flat_map(tracing_subscriber::registry::Scope::from_root)
            .filter_map(|span| span.extensions().get::<SpanFields>().cloned())
            .map(|span| serde_json::json!({"operation": span.operation, "parent": span.parent, "name": span.name, "fields": span.values}))
            .collect();
        self.recorder.record(&payload, &spans);
        if let (Some(release_sets), Some(sha256)) = (&mut release_sets, release_set) {
            release_sets.insert(sha256);
        }
        drop(release_sets);
        if let Some(artifact) = artifact.filter(|_| finished) {
            self.finished_copy(artifact, complete);
        }
        if let Some(artifact) = artifact.filter(|_| trace_input) {
            self.excluded_artifacts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(artifact);
            let event = cooldown_core::trace::Event::ArtifactFinished {
                artifact,
                bytes: 0,
                outcome: cooldown_core::trace::ArtifactOutcome::Failed {
                    reason: "diagnostic trace output is excluded from resolver input capture",
                },
            };
            match serde_json::value::to_raw_value(&event) {
                Ok(event) => self.recorder.record(&event, &spans),
                Err(err) => self.recorder.fail(err.to_string()),
            }
        }
    }

    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let extensions = span.extensions();
        let Some(fields) = extensions.get::<SpanFields>() else {
            return;
        };
        match serde_json::value::to_raw_value(&serde_json::json!({
            "kind": "operation_finished", "operation": fields.operation, "parent": fields.parent,
            "name": fields.name, "fields": fields.values,
            "durationUs": cooldown_core::trace::duration_us(fields.started.elapsed()),
        })) {
            Ok(event) => self.recorder.record(&event, &[]),
            Err(err) => self.recorder.fail(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{Recorder, TraceCommand};
    use color_eyre::eyre;
    use cooldown_core::trace::{ArtifactOutcome, ArtifactSource, Content, Event, InputRole};
    use tracing_subscriber::prelude::*;

    /// The events a recorder at `detail` writes for what `emit` observes.
    fn recorded(detail: Detail, emit: impl FnOnce()) -> eyre::Result<Vec<Value>> {
        let directory = tempfile::tempdir()?;
        let recorder = Recorder::in_directory(
            directory.path(),
            std::path::Path::new("project"),
            TraceCommand::Upgrade,
            cooldown_core::trace::Detail::Compact,
        )?;
        let dispatch = tracing::Dispatch::new(
            tracing_subscriber::registry()
                .with(TraceLayer::recording(Arc::clone(&recorder), detail)),
        );
        tracing::dispatcher::with_default(&dispatch, emit);
        recorder.finish();
        let mut records = crate::trace::records(&recorder.path)?;
        let events = records
            .by_ref()
            .map(|record| record.map(|record| record["event"].clone()))
            .collect::<Result<Vec<_>, _>>()?;
        eyre::ensure!(!records.truncated(), "the finished trace is complete");
        Ok(events)
    }

    fn artifact_events<'e>(events: &'e [Value], kind: &str, artifact: u64) -> Vec<&'e Value> {
        events
            .iter()
            .filter(|event| event["kind"] == kind && event["artifact"] == artifact)
            .collect()
    }

    /// A capture that starts while an identical one is still being written must not refer to it:
    /// a trace cut off before the first capture's chunks would leave the reference without bytes.
    #[test]
    fn an_artifact_becomes_a_reference_target_only_once_complete() -> eyre::Result<()> {
        let content = "[package]";
        let sha256 = "a".repeat(64);
        let other = "b".repeat(64);
        let events = recorded(Detail::Compact, || {
            let started = |artifact, path, sha256| {
                Event::ArtifactStarted {
                    artifact,
                    snapshot: None,
                    source: ArtifactSource::File {
                        path,
                        role: InputRole::Manifest,
                    },
                    size: Some(9),
                    sha256: Some(sha256),
                }
                .emit();
            };
            let chunk = |artifact| {
                Event::ArtifactChunk {
                    artifact,
                    offset: 0,
                    content: Content::from_bytes(content.as_bytes()),
                }
                .emit();
            };
            let finished = |artifact, outcome| {
                Event::ArtifactFinished {
                    artifact,
                    bytes: 9,
                    outcome,
                }
                .emit();
            };
            // The second capture of the same content runs to completion while the first is
            // still pending.
            started(1, "/a/Cargo.toml", &sha256);
            started(2, "/b/Cargo.toml", &sha256);
            chunk(2);
            finished(2, ArtifactOutcome::Complete);
            chunk(1);
            finished(1, ArtifactOutcome::Complete);
            // A capture after both completed refers to the one that completed first.
            started(3, "/c/Cargo.toml", &sha256);
            chunk(3);
            finished(3, ArtifactOutcome::Complete);
            // An incomplete capture never becomes a reference target.
            started(4, "/d/Cargo.toml", &other);
            chunk(4);
            finished(4, ArtifactOutcome::Changed);
            started(5, "/e/Cargo.toml", &other);
            chunk(5);
            finished(5, ArtifactOutcome::Complete);
        })?;

        // The capture that finished first was pending against nothing complete, so it carries
        // its own bytes.
        let [second] = artifact_events(&events, "artifact_started", 2)[..] else {
            eyre::bail!("one start of the second capture expected");
        };
        assert!(second.get("duplicate_of").is_none(), "{second}");
        assert_eq!(artifact_events(&events, "artifact_chunk", 2).len(), 1);

        // A later capture refers to the first copy that was complete, without chunks.
        let [third] = artifact_events(&events, "artifact_started", 3)[..] else {
            eyre::bail!("one start of the third capture expected");
        };
        assert_eq!(third["duplicate_of"], 2);
        assert_eq!(
            artifact_events(&events, "artifact_chunk", 3),
            Vec::<&Value>::new()
        );
        assert_eq!(artifact_events(&events, "artifact_finished", 3).len(), 1);

        // Content captured only partially is written again by the next capture.
        let [fifth] = artifact_events(&events, "artifact_started", 5)[..] else {
            eyre::bail!("one start of the fifth capture expected");
        };
        assert!(fifth.get("duplicate_of").is_none(), "{fifth}");
        assert_eq!(artifact_events(&events, "artifact_chunk", 5).len(), 1);
        Ok(())
    }

    /// A compact trace writes each release list once, however many evaluations judge it.
    #[test]
    fn a_release_set_is_written_once_per_run() -> eyre::Result<()> {
        let first = serde_json::value::to_raw_value(&serde_json::json!([{"version": "1.0.0"}]))?;
        let second = serde_json::value::to_raw_value(&serde_json::json!([{"version": "2.0.0"}]))?;
        let events = recorded(Detail::Compact, || {
            for (sha256, releases) in [("1", &first), ("1", &first), ("2", &second), ("1", &first)]
            {
                Event::ReleaseSet { sha256, releases }.emit();
            }
        })?;
        let sets: Vec<&Value> = events
            .iter()
            .filter(|event| event["kind"] == "release_set")
            .collect();
        let hashes: Vec<&Value> = sets.iter().map(|set| &set["sha256"]).collect();
        let versions: Vec<&Value> = sets
            .iter()
            .map(|set| &set["releases"][0]["version"])
            .collect();
        assert_eq!(hashes, ["1", "2"]);
        assert_eq!(versions, ["1.0.0", "2.0.0"]);
        Ok(())
    }

    /// The recording layer enables the typed target at TRACE, which selects full detail, only for
    /// a full trace.
    #[test]
    fn the_recording_layer_selects_the_detail() -> eyre::Result<()> {
        for detail in [Detail::Compact, Detail::Full] {
            let mut observed = None;
            recorded(detail, || observed = Detail::current())?;
            assert_eq!(observed, Some(detail));
        }
        Ok(())
    }
}
