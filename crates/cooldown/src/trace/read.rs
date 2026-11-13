//! Reading trace files, compressed or written before traces were compressed.

use cooldown_core::CoreError;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

/// The four bytes every zstd frame starts with.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// The records of one trace file, decoded one at a time in file order.
pub struct Records {
    path: PathBuf,
    reader: BufReader<Content<std::fs::File>>,
    line: Vec<u8>,
    partial_line: bool,
}

/// Opens the trace at `path` for reading its records one at a time.
///
/// Both zstd-compressed traces and uncompressed JSON Lines traces are accepted.
/// A trace that ends mid-frame or mid-line yields the records before the cut, after which
/// [`Records::truncated`] is `true`.
///
/// # Errors
///
/// Returns [`CoreError::Io`] when the file cannot be opened or its first bytes cannot be read.
pub fn records(path: &Path) -> Result<Records, CoreError> {
    Ok(Records {
        path: path.to_owned(),
        reader: decoded(std::fs::File::open(path)?)?,
        line: Vec::new(),
        partial_line: false,
    })
}

impl Records {
    /// Whether the records ended at a cut-off tail, as a trace does after an abrupt termination
    /// or while its run is still recording.
    ///
    /// Every record yielded before the cut is complete.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.partial_line || self.reader.get_ref().cut_short()
    }
}

impl Iterator for Records {
    type Item = Result<serde_json::Value, CoreError>;

    /// Yields the next complete record.
    ///
    /// A read error of the file, or a complete line that is not JSON, is yielded as an error.
    fn next(&mut self) -> Option<Self::Item> {
        if self.partial_line {
            return None;
        }
        self.line.clear();
        if let Err(err) = self.reader.read_until(b'\n', &mut self.line) {
            return Some(Err(err.into()));
        }
        match self.line.split_last() {
            None => None,
            Some((b'\n', record)) => Some(serde_json::from_slice(record).map_err(|err| {
                CoreError::System(format!(
                    "trace {} has an invalid record: {err}",
                    self.path.display()
                ))
            })),
            Some(_) => {
                self.partial_line = true;
                None
            }
        }
    }
}

/// The decoded content of a trace file, which ends early instead of failing where compressed
/// data is cut off.
pub(super) fn decoded<R: Read>(reader: R) -> std::io::Result<BufReader<Content<R>>> {
    let mut buffered = BufReader::new(reader);
    let content = if buffered.fill_buf()?.starts_with(&ZSTD_MAGIC) {
        Content::Compressed {
            decoder: zstd::stream::read::Decoder::with_buffer(buffered)?,
            cut_short: false,
        }
    } else {
        Content::Plain(buffered)
    };
    Ok(BufReader::new(content))
}

pub(super) enum Content<R: Read> {
    Plain(BufReader<R>),
    Compressed {
        decoder: zstd::stream::read::Decoder<'static, BufReader<R>>,
        /// Whether decoding stopped at a truncated or damaged frame.
        cut_short: bool,
    },
}

impl<R: Read> Content<R> {
    fn cut_short(&self) -> bool {
        matches!(
            self,
            Self::Compressed {
                cut_short: true,
                ..
            }
        )
    }
}

impl<R: Read> Read for Content<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(reader) => reader.read(buffer),
            Self::Compressed {
                cut_short: true, ..
            } => Ok(0),
            Self::Compressed { decoder, cut_short } => match decoder.read(buffer) {
                // A frame cut off by an abrupt termination ends where its last complete block
                // does: zstd reports the missing remainder as an unexpected end, and damaged data
                // as an invalid frame.
                // Errors of the file itself keep their own kinds and still fail the read.
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::Other
                    ) =>
                {
                    *cut_short = true;
                    Ok(0)
                }
                read => read,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{Recorder, TraceCommand};
    use color_eyre::eyre;
    use serde_json::Value;

    fn recorder(directory: &Path) -> eyre::Result<std::sync::Arc<Recorder>> {
        Ok(Recorder::in_directory(
            directory,
            Path::new("project"),
            TraceCommand::Upgrade,
            cooldown_core::trace::Detail::Compact,
        )?)
    }

    /// Records a diagnostic with distinct, compressible text.
    fn record(recorder: &Recorder, index: usize) -> eyre::Result<()> {
        let event = serde_json::value::to_raw_value(&serde_json::json!({
            "kind": "diagnostic",
            "index": index,
            "text": format!("record {index} ").repeat(200),
        }))?;
        recorder.record(&event, &[]);
        Ok(())
    }

    fn sequences(records: &[Value]) -> Vec<u64> {
        records
            .iter()
            .filter_map(|record| record["sequence"].as_u64())
            .collect()
    }

    /// A finished trace is a zstd stream that decodes to every record in order.
    #[test]
    fn a_finished_trace_round_trips_through_compression() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let recorder = recorder(directory.path())?;
        for index in 0..500 {
            record(&recorder, index)?;
        }
        recorder.finish();
        let bytes = std::fs::read(&recorder.path)?;
        assert!(bytes.starts_with(&ZSTD_MAGIC));
        assert!(
            recorder.path.to_string_lossy().ends_with(".jsonl.zst"),
            "{}",
            recorder.path.display()
        );
        // Repeated text compresses well below its JSON size.
        assert!(bytes.len() < 500 * 2000 / 10, "{} bytes", bytes.len());

        let mut records = records(&recorder.path)?;
        let read = records.by_ref().collect::<Result<Vec<_>, _>>()?;
        assert!(!records.truncated());
        assert_eq!(sequences(&read), (0..500).collect::<Vec<_>>());
        assert!(
            read.iter()
                .enumerate()
                .all(|(index, record)| record["event"]["index"] == index
                    && record["runId"] == recorder.run_id)
        );
        Ok(())
    }

    /// A trace cut at any byte, as an abrupt termination leaves it, yields a gap-free prefix of
    /// its records, and every record flushed before the cut survives it.
    #[test]
    fn a_cut_trace_keeps_every_record_before_the_cut() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let recorder = recorder(directory.path())?;
        // Flush points: the file length after each flush, with the records written by then.
        let mut flushed = Vec::new();
        for index in 0..400 {
            record(&recorder, index)?;
            if index % 50 == 49 {
                recorder.flush();
                flushed.push(FlushPoint {
                    length: std::fs::metadata(&recorder.path)?.len(),
                    records: index + 1,
                });
            }
        }
        recorder.finish();
        let bytes = std::fs::read(&recorder.path)?;
        let length = u64::try_from(bytes.len())?;
        let cut = directory.path().join("cut.jsonl.zst");

        let mut cuts: Vec<u64> = (1..64).map(|step| length * step / 64).collect();
        cuts.extend(flushed.iter().map(|point| point.length));
        cuts.extend(flushed.iter().map(|point| point.length + 3));
        for at in cuts {
            std::fs::write(&cut, bytes.get(..usize::try_from(at)?).unwrap_or(&bytes))?;
            let mut records = records(&cut)?;
            let read = records.by_ref().collect::<Result<Vec<_>, _>>()?;
            let count = u64::try_from(read.len())?;

            // Whatever survives is the start of the run, without a gap.
            assert_eq!(
                sequences(&read),
                (0..count).collect::<Vec<_>>(),
                "cut at {at}"
            );
            assert_eq!(records.truncated(), at < length, "cut at {at}");

            // Nothing flushed before the cut is lost.
            let durable = flushed
                .iter()
                .filter(|point| point.length <= at)
                .map(|point| point.records)
                .max()
                .unwrap_or(0);
            assert!(
                read.len() >= durable,
                "cut at {at} kept {count} of {durable}"
            );
        }
        Ok(())
    }

    struct FlushPoint {
        length: u64,
        records: usize,
    }

    /// Traces written before compression keep reading as plain JSON Lines, including a final
    /// line cut short.
    #[test]
    fn an_uncompressed_trace_stays_readable() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("upgrade-old.jsonl");
        std::fs::write(&path, "{\"sequence\":0}\n{\"sequence\":1}\n{\"seq")?;
        let mut records = records(&path)?;
        let read = records.by_ref().collect::<Result<Vec<_>, _>>()?;
        assert_eq!(sequences(&read), [0, 1]);
        assert!(records.truncated());
        Ok(())
    }
}
