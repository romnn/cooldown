# 2. Run traces are compressed and compact by default

**Status:** accepted · **Date:** 2026-11-13 · amends [0001](0001-default-run-traces.md)

## Context

Default run traces ([0001](0001-default-run-traces.md)) record every observation as plain JSON
Lines. On a large polyglot monorepo one `upgrade --major --sync` run wrote a 12.5 GB trace, and
each `fix --tool cargo` run about 530 MB. Most of it repeats itself: each `release_filtered` record
carries the whole dependency next to the one release it rejected, each `evaluation` carries the
package's full release list although policy is re-evaluated far more often than the list changes,
and native command output such as `cargo metadata` graphs is captured again on every call. Writing
that volume also costs serialization and I/O time inside the run.

## Decision

**Traces are always zstd-compressed.** One streaming frame per trace, at level 19 with
long-distance matching, a 128 MiB window that a stock `zstd -d` decodes without options, a content
checksum, and 8 MiB jobs on two compression workers for a compact trace and four for a full one.
Level 20 compressed traces within about 1% of level 19 at a lower throughput. The file name ends in `.jsonl.zst`; the record envelope, the
`run_started` header, and the terminal `run_finished` record are unchanged.

**Compression runs off the recording path.** The recorder serializes a record and queues it for a
writer thread that owns the file. The queue holds at most 256 MiB, so memory stays bounded; the
run waits only when a burst exceeds it. A zstd flush is a barrier across its workers, so the
writer flushes only once it has caught up with the run and its last flush is at least a second
old. The header is flushed before the run continues, so pruning and concurrent runs recognize an
active trace. An abrupt termination loses what was not yet flushed and the partial final block or
line; every earlier record decompresses.

**The default trace is compact.** It keeps the same decisions and evidence without repetition:

- A release list is recorded once per run as a content-addressed `release_set`, and evaluations
  name it in `releaseSet`.
- Rejected releases are listed with their reason in their `evaluation` (`filtered`) instead of one
  `release_filtered` record each.
- Native stdout and stderr are recorded when the stream ends, including on interruption, timeout,
  or failure, with their hash, so identical output becomes a `duplicate_of` reference.

An artifact becomes a reference target only after its terminal `complete` record, so a reference
never precedes the bytes it names.

**`--full-trace` records everything as it happens**, as 0001 describes, still compressed: release
lists inline, per-release `release_filtered` records, and native output chunks as they arrive. The
`run_started` record states the `detail` (`compact` or `full`). The mode reaches core through the
tracing filter: typed observations are emitted at DEBUG, and a full trace also enables TRACE.

**Readers accept both formats.** The reader detects the zstd magic, so uncompressed traces of
earlier versions stay readable, and `prune` removes both.

## Consequences

- On this repository's `upgrade --major`, the trace shrinks from 280 MB to about 1.9 MB compact or
  2.9 MB full, and the recording path spends less time than before.
- Compression costs CPU and memory on the writer's threads: roughly 12 s of CPU and 400 MB peak
  resident memory for that compact run, against 156 MB without compression, and 30 s and 840 MB for
  a full one.
- A compact trace does not show the output of a native command that was still running when the
  process was killed abruptly; a full trace shows it up to the last flush.
- Traces are no longer plain text; reading one needs `zstd -dc` or the `cooldown::trace::records`
  reader.

## References

- [Issue #91](https://github.com/romnn/cooldown/issues/91)
- `crates/cooldown/src/trace/writer.rs`, `crates/cooldown/src/trace/read.rs`,
  `crates/cooldown/src/trace/layer.rs`
- `crates/cooldown-core/src/trace.rs`, `crates/cooldown-core/src/evaluate.rs`
- `crates/cooldown-adapter-util/src/driver.rs`
