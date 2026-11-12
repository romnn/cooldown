# 1. Default run traces preserve the evidence behind dependency decisions

**Status:** accepted · **Date:** 2026-11-12

## Context

Dependency resolution combines release history, declared requirements, existing lock bindings,
native package-manager behavior, and cooldown policy. An unexpected hold or false conflict is
difficult to reconstruct after the repository or registry has changed. Some runs last hours, so
the same evidence must also explain where time was spent.

Verbose console output is useful while watching a run but is neither a complete record of its
inputs nor a typed history of its decisions. A final report cannot explain discarded candidates,
failed intermediate resolves, or the baseline a retry restored.

## Decision

**`upgrade`, `fix`, and `outdated` record traces by default.** Other commands do not record
traces. `--no-trace` is the explicit opt-out. Console verbosity does not control recording.
`--trace-dir <PATH>` / `COOLDOWN_TRACE_DIR` override the storage directory; `--no-trace` /
`COOLDOWN_NO_TRACE` take precedence. Relative directory overrides resolve from the invocation
working directory and also select traces for pruning. Each run receives its own file below the
user's platform-appropriate cooldown configuration directory. Names start with the command
(`upgrade-`, `fix-`, `outdated-`), followed by UTC time, repository name, and an atomically
allocated unique suffix. The trace path is printed to stderr on success and failure, leaving JSON
report stdout intact. Every record identifies its producer, schema version, run ID, sequence,
actual UTC timestamp, and monotonic elapsed time. The first `run_started` record contains the
start date and command: consumers read JSON fields and never parse metadata from filenames.

**The record is typed evidence, not formatted log text.** A versioned event schema captures run
identity and options; actual UTC timestamps and monotonic elapsed times; the separate evaluation
clock and cutoff; project/tool/candidate identity; native executable versions and command outputs;
configuration and graph inputs; release metadata; candidate and policy decisions with reasons;
resolver trials, retries, fallbacks, acceptance, rollback, and the final report and state.
Existing diagnostic logs supplement those events but never substitute for them.

**Keep input knowledge with its existing owner.** `ToolRead::evidence` declares exact paths,
native formats and graph fidelity, relevant ambient settings, and inventory limitations. The
application streams those files under its existing read/write leases. Relative ambient
configuration preserves both cooldown's process-directory view and the native tool's
project-directory view when those differ. Authoritative bytes already captured by mutation
preparation are emitted at their capture boundary. Native process output retains graphs already
read by the resolver. There is no second universal dependency-graph parser or CLI-owned filename
inventory. Executable manifests and plugins can read arbitrary files, so this is a forensic record
with explicit coverage, not a promise of hermetic replay.

**Capture the state before changing it.** Starting snapshots precede lock refresh, native-policy
sync, and mutation. Preserve full source/version identities, roots, resolved edges, declarations,
and the actual lock/configuration inputs. Excluded packages remain part of the baseline evidence.
Subsequent snapshots identify the operation and state they describe, including staged dry runs. A
format that does not contain dependency edges must identify that missing evidence explicitly; the
recorder must never invent edges or label a flat inventory as a complete graph.

**Recording observes the resolver.** It preserves candidate order, policy, registry requests, and
dependency state, and does not run an extra resolve merely to collect evidence. Version-only
queries have a five-second timeout and failures are recorded as unavailable version evidence. On
Windows, version-query children enter a kill-on-close job before execution so cancellation also
stops wrappers' descendants. Ordinary native commands keep their existing lifetime semantics. Unix
children use process groups. Recording failures are visible but preserve the command's
dependency-policy result.

**Write throughout execution with bounded memory.** Use versioned JSON Lines, retaining evidence
already written when a run is interrupted. Files and HTTP bodies are emitted in bounded chunks.
Native stdout and stderr are emitted as they arrive, including partial output before interruption
or timeout; drivers retain their existing output buffer for parsing. Recording never accumulates
the complete run history in memory. Correlation uses scoped operation IDs and parent IDs across
overlapping projects and resolver attempts; timestamps alone are insufficient. The CLI uses a
run-scoped tracing subscriber and owns the single buffered file writer. A complete run has an
explicit terminal record, and an interrupted or abruptly terminated recording must not be mistaken
for success. Compression, binary storage, remote upload, and a replay engine are outside this
decision.

**Pruning is explicit and bounded by age.** `cooldown prune` removes old inactive trace files and
existing HTTP cache entries and abandoned Yarn verification copies, with `--older-than` defaulting
to 30 days. It supports dry runs and preserves active runs, unrelated files, and the monotonic
publication-time trust store. Eligibility uses last modification time and an owned JSON header,
independent of filename dates. That store is integrity evidence against backdated publication
times, not disposable cache data. The Cargo rejection memo is memory-only and adds no persisted
cleanup format.

## Consequences

- A report of unexpected behavior can include the original inputs, exact tools, decisions,
  warnings/errors, and timings from one run instead of relying on a later reproduction.
- Instrumentation belongs where decisions and effects happen. The CLI owns storage and notices;
  core/adapters expose typed observations without owning user directories or trace files.
- Traces can be large and contain repository details. Storage grows until the user prunes it;
  recording must stream rather than retain a run-sized buffer.
- Graph coverage is a contract to test per adapter. Native graph evidence and lock bindings must
  keep distinct package copies and sources distinct, including workspace and path packages.
- Tests must cover ordinary, failed, interrupted, concurrent, dry-run and disabled recordings;
  setup/write/tool-version failures; baseline timing; final-result agreement; pruning safety;
  and configuration paths on Linux, macOS, and Windows.
- The [user guide](../docs/content/docs/traces.md) documents storage, event reconstruction, opt-out,
  pruning, and coverage limits. Help/version, argument-parser exits, and commands outside the three recorded commands create no trace.

## References

- [Issue #83](https://github.com/romnn/cooldown/issues/83)
- [Draft PR #84](https://github.com/romnn/cooldown/pull/84)
- `crates/cooldown/src/cli/runtime.rs`, `crates/cooldown/src/cli/setup/`
- `crates/cooldown/src/app/upgrade/executor/`, `crates/cooldown/src/app/release_cache.rs`
- `crates/cooldown-core/src/evaluate.rs`, `crates/cooldown-core/src/ports.rs`
- `crates/cooldown-adapter-util/src/driver.rs`, `crates/cooldown-registry/src/http.rs`
