use super::Recorder;
use serde_json::{Map, Value, value::RawValue};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

pub(crate) struct TraceLayer {
    recorder: Arc<Recorder>,
    excluded_artifacts: Mutex<HashSet<u64>>,
}

impl TraceLayer {
    pub(crate) fn new(recorder: Arc<Recorder>) -> Self {
        Self {
            recorder,
            excluded_artifacts: Mutex::new(HashSet::new()),
        }
    }
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
        if let Some(artifact) = artifact {
            let excluded = {
                let mut excluded = self
                    .excluded_artifacts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let contains = excluded.contains(&artifact);
                if contains
                    && fields
                        .values
                        .get("trace_artifact_finished")
                        .and_then(Value::as_bool)
                        == Some(true)
                {
                    excluded.remove(&artifact);
                }
                contains
            };
            if excluded {
                return;
            }
        }
        let input_path = if event.metadata().target() == cooldown_core::trace::TARGET {
            match fields.values.remove("trace_input_path") {
                Some(Value::String(path)) => Some(path),
                _ => None,
            }
        } else {
            None
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
        let spans: Vec<Value> = ctx
            .event_scope(event)
            .into_iter()
            .flat_map(tracing_subscriber::registry::Scope::from_root)
            .filter_map(|span| span.extensions().get::<SpanFields>().cloned())
            .map(|span| serde_json::json!({"operation": span.operation, "parent": span.parent, "name": span.name, "fields": span.values}))
            .collect();
        self.recorder.record(&payload, &spans);
        if let Some(artifact) = artifact.filter(|_| {
            input_path
                .as_deref()
                .is_some_and(|path| self.recorder.is_trace_input(std::path::Path::new(path)))
        }) {
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
