//! The background thread that compresses a run's records and writes them to its trace file.
//!
//! The recording path only serializes a record and queues it.
//! Compression with zstd's own worker threads and every write happen on this thread, so a slow
//! disk or a busy compressor delays the run only once the bounded queue is full.

use cooldown_core::trace::Detail;
use std::fs::File;
use std::io::Write as _;
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use zstd::zstd_safe::CParameter;

/// Level 19 compresses run traces within about 1% of level 20 at a higher throughput.
const LEVEL: i32 = 19;
/// A 128 MiB window, the largest a stock `zstd -d` decodes without `--long` or `--memory`.
const WINDOW_LOG: u32 = 27;
/// Compression workers for a compact trace, besides this thread.
///
/// Each worker holds level 19's match tables of about 80 MiB, and a compact trace is a fraction
/// of a full one.
const COMPACT_WORKERS: usize = 2;
/// Compression workers for a full trace, which must keep up with several times the volume.
const FULL_WORKERS: usize = 4;
/// The input size at which zstd hands a job to a worker without a flush.
///
/// Small jobs keep output flowing while the writer is behind, when it does not flush.
const JOB_SIZE: u32 = 8 << 20;
/// A 1 MiB prefix per job instead of the level's default 16 MiB, which each job would index
/// again; long-distance matching still finds repeats across the whole window.
const OVERLAP_LOG: u32 = 5;
/// How long a caught-up writer keeps records in the compressor before it flushes them to the file.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
/// The serialized bytes the recording path may queue before it waits for the writer.
const QUEUE_BUDGET: usize = 256 << 20;

/// The error a recorder reports once the writer has stopped, until the writer's own error is known.
pub(super) const STOPPED: &str = "trace writer stopped";

enum Message {
    Record(Vec<u8>),
    /// Flushes everything queued before it, then acknowledges.
    Flush(mpsc::Sender<()>),
}

/// A handle to the writer thread of one trace file.
pub(super) struct Writer {
    sender: mpsc::Sender<Message>,
    budget: Arc<Budget>,
    thread: JoinHandle<Result<(), String>>,
}

impl Writer {
    /// Starts compressing into `file`, which the writer owns, and so keeps locked, until it ends.
    pub(super) fn spawn(file: File, detail: Detail) -> std::io::Result<Self> {
        let encoder = encoder(file, detail)?;
        let (sender, receiver) = mpsc::channel();
        let budget = Arc::new(Budget::default());
        let thread = std::thread::Builder::new()
            .name("cooldown-trace".to_string())
            .spawn({
                let budget = Arc::clone(&budget);
                move || {
                    let written = write(&receiver, &budget, encoder).map_err(|err| err.to_string());
                    budget.close();
                    written
                }
            })?;
        Ok(Self {
            sender,
            budget,
            thread,
        })
    }

    /// Queues one serialized record, waiting while the queue is over its budget.
    pub(super) fn send(&self, record: Vec<u8>) -> Result<(), &'static str> {
        self.budget.acquire(record.len())?;
        self.sender
            .send(Message::Record(record))
            .map_err(|_| STOPPED)
    }

    /// Returns once every record queued so far is in the file.
    pub(super) fn flush(&self) -> Result<(), &'static str> {
        let (acknowledge, acknowledged) = mpsc::channel();
        self.sender
            .send(Message::Flush(acknowledge))
            .map_err(|_| STOPPED)?;
        acknowledged.recv().map_err(|_| STOPPED)
    }

    /// Writes the queued records, ends the frame, and syncs the file.
    pub(super) fn finish(self) -> Result<(), String> {
        drop(self.sender);
        self.thread
            .join()
            .unwrap_or_else(|_| Err("trace writer panicked".to_string()))
    }
}

fn encoder(
    file: File,
    detail: Detail,
) -> std::io::Result<zstd::stream::write::Encoder<'static, File>> {
    let workers = match detail {
        Detail::Compact => COMPACT_WORKERS,
        Detail::Full => FULL_WORKERS,
    };
    let workers = std::thread::available_parallelism().map_or(1, |count| count.get().min(workers));
    let mut encoder = zstd::stream::write::Encoder::new(file, LEVEL)?;
    encoder.long_distance_matching(true)?;
    encoder.window_log(WINDOW_LOG)?;
    encoder.include_checksum(true)?;
    encoder.multithread(u32::try_from(workers).unwrap_or(1))?;
    encoder.set_parameter(CParameter::JobSize(JOB_SIZE))?;
    encoder.set_parameter(CParameter::OverlapSizeLog(OVERLAP_LOG))?;
    Ok(encoder)
}

/// Compresses queued records until every sender is gone, then ends the frame.
///
/// A flush is a barrier across zstd's workers, so the writer flushes only when it has caught up
/// with the run.
/// While it is behind, compression stays parallel and output appears as jobs fill.
fn write(
    receiver: &mpsc::Receiver<Message>,
    budget: &Budget,
    mut encoder: zstd::stream::write::Encoder<'static, File>,
) -> std::io::Result<()> {
    let mut unflushed_since: Option<Instant> = None;
    loop {
        let message = match receiver.try_recv() {
            Ok(message) => message,
            Err(TryRecvError::Disconnected) => break,
            // Caught up
            Err(TryRecvError::Empty) => {
                let waited = match unflushed_since {
                    None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    Some(since) => {
                        receiver.recv_timeout(FLUSH_INTERVAL.saturating_sub(since.elapsed()))
                    }
                };
                match waited {
                    Ok(message) => message,
                    Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        encoder.flush()?;
                        unflushed_since = None;
                        continue;
                    }
                }
            }
        };
        match message {
            Message::Record(record) => {
                encoder.write_all(&record)?;
                budget.release(record.len());
                unflushed_since.get_or_insert_with(Instant::now);
            }
            Message::Flush(acknowledge) => {
                encoder.flush()?;
                unflushed_since = None;
                // The requester waits for this, so a failed send only means it no longer does.
                acknowledge.send(()).ok();
            }
        }
    }
    encoder.finish()?.sync_data()
}

/// The serialized bytes queued for the writer, bounded so the queue never holds a run's history.
#[derive(Default)]
struct Budget {
    state: Mutex<BudgetState>,
    released: Condvar,
}

#[derive(Default)]
struct BudgetState {
    queued: usize,
    closed: bool,
}

impl Budget {
    fn acquire(&self, bytes: usize) -> Result<(), &'static str> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        // A record larger than the whole budget still goes through once the queue is empty.
        while !state.closed && state.queued > 0 && state.queued + bytes > QUEUE_BUDGET {
            state = self
                .released
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.closed {
            return Err(STOPPED);
        }
        state.queued += bytes;
        Ok(())
    }

    fn release(&self, bytes: usize) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.queued = state.queued.saturating_sub(bytes);
        drop(state);
        self.released.notify_all();
    }

    /// Wakes every waiting sender once the writer has stopped, so none waits for it forever.
    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
        self.released.notify_all();
    }
}
