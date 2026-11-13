use super::Recorder;
use serde_json::{Map, Value, value::RawValue};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

pub(crate) struct TraceLayer {
    recorder: Arc<Recorder>,
    excluded_artifacts: Mutex<HashSet<u64>>,
    /// The first artifact this recorder wrote for each content hash.
    ///
    /// Repeated captures of unchanged files, such as every manifest a mutation checkpoint
    /// snapshots, are then written once per run.
    written_contents: Mutex<HashMap<String, u64>>,
    /// Artifacts recorded as references to an earlier identical artifact, whose chunks are omitted.
    duplicate_artifacts: Mutex<HashSet<u64>>,
}

impl TraceLayer {
    pub(crate) fn new(recorder: Arc<Recorder>) -> Self {
        Self {
            recorder,
            excluded_artifacts: Mutex::new(HashSet::new()),
            written_contents: Mutex::new(HashMap::new()),
            duplicate_artifacts: Mutex::new(HashSet::new()),
        }
    }

    /// Whether `artifact` is a recorded duplicate whose chunk this event carries.
    ///
    /// The terminal record of a duplicate still passes, so its outcome and byte count stay in the
    /// trace.
    fn skips_duplicate_chunk(&self, artifact: u64, finished: bool) -> bool {
        let mut duplicates = self
            .duplicate_artifacts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !duplicates.contains(&artifact) {
            return false;
        }
        if finished {
            duplicates.remove(&artifact);
        }
        !finished
    }

    /// Records `artifact` as the first copy of `sha256`, or returns the earlier artifact that
    /// already holds identical content.
    fn earlier_copy(&self, artifact: u64, sha256: String) -> Option<u64> {
        let mut contents = self
            .written_contents
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(&first) = contents.get(&sha256) {
            self.duplicate_artifacts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(artifact);
            return Some(first);
        }
        contents.insert(sha256, artifact);
        None
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
        let payload = if event.metadata().target() == cooldown_core::trace::TARGET {
            let Some(Value::String(payload)) = fields.values.remove("trace_event") else {
                self.recorder.fail(format!(
                    "typed event could not be serialized: {:?}",
                    fields.values
                ));
                return;
            };
            match RawValue::from_string(payload) {
                Ok(payload) => payload,
                Err(err) => {
                    self.recorder.fail(format!("invalid typed event: {err}"));
                    return;
                }
            }
        } else {
            let event = serde_json::json!({
                "kind": "diagnostic",
                "level": event.metadata().level().as_str(),
                "target": event.metadata().target(),
                "fields": fields.values,
            });
            match serde_json::value::to_raw_value(&event) {
                Ok(event) => event,
                Err(err) => {
                    self.recorder.fail(err.to_string());
                    return;
                }
            }
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
