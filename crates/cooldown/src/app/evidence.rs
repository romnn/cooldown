use camino::Utf8Path;
use cooldown_core::trace::{
    ArtifactOutcome, ArtifactSource, Content, Event, Evidence, SnapshotPhase,
};
use cooldown_core::{Project, ToolRead};
use std::io::Read as _;

#[derive(Debug)]
pub(crate) struct Session {
    project: String,
    tool: String,
    evidence: Evidence,
}

impl Session {
    pub(crate) fn start(
        reader: &dyn ToolRead,
        project: &Project,
        phase: SnapshotPhase,
    ) -> Option<Self> {
        if !cooldown_core::trace::enabled() {
            return None;
        }
        let evidence = reader.evidence(project).unwrap_or_else(|err| Evidence {
            inputs: vec![cooldown_core::trace::EvidenceInput {
                path: project.manifest.clone(),
                role: cooldown_core::trace::InputRole::Manifest,
            }],
            limitations: vec![err.to_string()],
            ..Evidence::default()
        });
        let session = Self {
            project: project.root.to_string(),
            tool: reader.id().to_string(),
            evidence,
        };
        session.capture(phase);
        Some(session)
    }

    fn capture(&self, phase: SnapshotPhase) {
        let snapshot = cooldown_core::trace::operation_id();
        Event::SnapshotStarted {
            snapshot,
            project: &self.project,
            tool: &self.tool,
            phase,
            evidence: &self.evidence,
        }
        .emit();
        for input in &self.evidence.inputs {
            capture_file(snapshot, &input.path, input.role);
        }
        Event::SnapshotFinished { snapshot }.emit();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.capture(SnapshotPhase::Final);
    }
}

fn capture_file(snapshot: u64, path: &Utf8Path, role: cooldown_core::trace::InputRole) {
    let artifact = cooldown_core::trace::operation_id();
    let declared = std::fs::metadata(path);
    Event::ArtifactStarted {
        artifact,
        snapshot: Some(snapshot),
        source: ArtifactSource::File {
            path: path.as_str(),
            role,
        },
        size: declared.as_ref().ok().map(std::fs::Metadata::len),
        sha256: None,
    }
    .emit();
    let mut bytes = 0;
    let captured = (|| -> std::io::Result<bool> {
        if !declared?.is_file() {
            return Err(std::io::Error::other("input is not a regular file"));
        }
        let mut file = std::fs::File::open(path)?;
        let initial = file.metadata()?;
        if !initial.is_file() {
            return Err(std::io::Error::other("input is not a regular file"));
        }
        {
            let mut reader = (&mut file).take(initial.len().max(1));
            let mut buffer = vec![0; 64 * 1024];
            loop {
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                if let Some(chunk) = buffer.get(..count) {
                    Event::ArtifactChunk {
                        artifact,
                        offset: bytes,
                        content: Content::from_bytes(chunk),
                    }
                    .emit();
                    bytes += u64::try_from(count).unwrap_or(u64::MAX);
                }
            }
        }
        let final_state = file.metadata()?;
        Ok(initial.len() == bytes
            && final_state.len() == bytes
            && initial.modified()? == final_state.modified()?
            && same_file::Handle::from_file(file)? == same_file::Handle::from_path(path)?)
    })();
    let reason = captured.as_ref().err().map(ToString::to_string);
    let outcome = match &captured {
        Ok(true) => ArtifactOutcome::Complete,
        Ok(false) => ArtifactOutcome::Changed,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && bytes == 0 => {
            ArtifactOutcome::Missing
        }
        Err(_) => ArtifactOutcome::Failed {
            reason: reason.as_deref().unwrap_or("input capture failed"),
        },
    };
    Event::ArtifactFinished {
        artifact,
        bytes,
        outcome,
    }
    .emit();
}

impl super::Workspace {
    pub(crate) async fn trace_initial(&self) {
        if !cooldown_core::trace::enabled() {
            return;
        }
        for project in self.projects() {
            Event::Project {
                project: project.project.root.as_str(),
                tool: project.tool.as_str(),
                evaluation_at: self.now(),
                policy: &project.policy,
            }
            .emit();

            match self
                .project_read_guard_phase(project, SnapshotPhase::Initial)
                .await
            {
                Ok(_guard) => {
                    if let Some(reader) = self.adapter(project.tool)
                        && let Some(version) = reader.version_command()
                    {
                        let driver = cooldown_adapter_util::Driver::from_program(&version.program);
                        let result = tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            driver.version_stdout(&project.project.root, &version.arguments),
                        )
                        .await;
                        let result = match result {
                            Ok(Ok(version)) => serde_json::json!({"version": version}),
                            Ok(Err(err)) => serde_json::json!({"error": err.to_string()}),
                            Err(_) => {
                                serde_json::json!({"error": "version query timed out after 5s"})
                            }
                        };
                        Event::ToolVersion {
                            project: project.project.root.as_str(),
                            program: &version.program,
                            result: &result,
                        }
                        .emit();
                    }
                }
                Err(err) => {
                    tracing::warn!(project = %project.rel_path, error = %err, "cannot capture initial project evidence");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use std::io::Write as _;
    use std::sync::{Arc, Mutex, PoisonError};
    use tracing_subscriber::prelude::*;

    #[derive(Default)]
    struct Captures {
        records: Vec<serde_json::Value>,
        chunks: usize,
        error: Option<String>,
    }

    struct GrowingInput {
        path: std::path::PathBuf,
        captures: Arc<Mutex<Captures>>,
    }

    #[derive(Default)]
    struct Payload {
        json: Option<String>,
    }

    impl tracing::field::Visit for Payload {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "trace_event" {
                self.json = Some(value.to_string());
            }
        }

        fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for GrowingInput {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut payload = Payload::default();
            event.record(&mut payload);
            let Some(json) = payload.json else {
                return;
            };
            let mut captures = self.captures.lock().unwrap_or_else(PoisonError::into_inner);
            let record = match serde_json::from_str::<serde_json::Value>(&json) {
                Ok(record) => record,
                Err(err) => {
                    captures.error = Some(err.to_string());
                    return;
                }
            };
            if record.get("kind").and_then(serde_json::Value::as_str) == Some("artifact_chunk") {
                captures.chunks += 1;
                // A bounded writer makes a regressed reader fail its assertions instead of hanging.
                if captures.chunks <= 3
                    && let Err(err) = std::fs::OpenOptions::new()
                        .append(true)
                        .open(&self.path)
                        .and_then(|mut file| file.write_all(b"x"))
                {
                    captures.error = Some(err.to_string());
                }
            }
            captures.records.push(record);
        }
    }

    #[test]
    fn growing_inputs_capture_only_the_initial_length_and_report_drift() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("growing-input");
        std::fs::write(&path, "original")?;
        let utf8 = camino::Utf8Path::from_path(&path)
            .ok_or_else(|| eyre::eyre!("UTF-8 fixture path expected"))?;
        let captures = Arc::new(Mutex::new(Captures::default()));
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(GrowingInput {
            path: path.clone(),
            captures: Arc::clone(&captures),
        }));
        tracing::dispatcher::with_default(&dispatch, || {
            capture_file(1, utf8, cooldown_core::trace::InputRole::ResolverInput);
        });
        let captures = captures.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(captures.error, None);
        assert_eq!(captures.chunks, 1);
        assert_eq!(std::fs::read(&path)?, b"originalx");
        assert_eq!(
            captures
                .records
                .last()
                .and_then(|record| record.pointer("/outcome/kind"))
                .and_then(serde_json::Value::as_str),
            Some("changed")
        );
        Ok(())
    }
}
