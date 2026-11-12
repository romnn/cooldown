---
title: Run traces
description: Inspect dependency decisions and resolver timings from their original evidence.
---

Cooldown records a diagnostic trace for `upgrade`, `fix`, and `outdated` by default. The trace includes the
invocation, host, evaluation clock, effective policy, native tool versions, input files and native
graph documents, registry responses, candidate decisions, native commands and output, resolver
trials and rollbacks, final project observations, and command results. Console verbosity does not
affect recording.

The trace location is printed to stderr on success and failure, keeping `--json` stdout readable by
other programs. Recording errors produce a warning and preserve the command's normal result.
`--help`, `--version`, and argument-parser failures do not execute a command or create a trace.

Disable recording for a command with `--no-trace`, or set `COOLDOWN_NO_TRACE=true`. Other commands
do not record traces.

## Storage

`--trace-dir <PATH>` overrides the trace directory, as does `COOLDOWN_TRACE_DIR`. Relative overrides
resolve against the invocation working directory. `--no-trace` takes precedence over either override.
The same override selects the trace directory for `prune`; the HTTP cache location is unchanged.
Choose a directory outside inputs consumed by native package managers: writing inside a local
`file:` package changes its contents. Diagnostic trace artifacts are explicitly excluded from input
capture, including aliases, so the recorder cannot recursively capture its own output.

Without an override, an absolute `XDG_CONFIG_HOME` takes precedence. Otherwise traces live in:

| Platform | Directory |
| --- | --- |
| Linux | `~/.config/cooldown/traces/` |
| macOS | `~/Library/Application Support/cooldown/traces/` |
| Windows | `%APPDATA%\cooldown\traces\` |

Names contain the actual UTC start time, repository directory name, and an atomically allocated
random suffix: `upgrade-<time>-<repository>-<suffix>.jsonl` (or `fix-` / `outdated-`). Concurrent commands use separate files.
Nothing uploads automatically. Inspect a trace before sharing it: native configuration, registry
URLs, environment settings, local paths, and command output can contain credentials or private data.

## Reading a trace

Each line is an independent JSON record with `producer: "cooldown"`, `schemaVersion`, `runId`, `sequence`, actual UTC
`timestamp`, monotonic `elapsedUs`, `event`, and its enclosing `spans`. Schema version 1 uses the
tagged event types defined in `cooldown-core/src/trace.rs`. Diagnostic log fields supplement these
events. Operation records and span references carry stable IDs and parent IDs; command start/end
pairs also have their own operation ID and duration.

The first event is `run_started`; its record contains the actual UTC start `timestamp`, command,
original working directory and cooldown version. The following `invocation` event contains the
arguments, environment overrides and selected directory, keeping the header small even for large
invocations. Read dates and identity from
these fields rather than parsing filenames, whose format is only a browsing convenience. A completed recording ends with `run_finished`, including the exit
code, interruption status, and any dispatch error. A missing terminal record means the recording is
incomplete. After an abrupt termination, ignore a partial final line; preceding lines remain useful.
Events are flushed as they are completed, rather than being held until the command exits. Native
stdout and stderr are captured as they arrive, preserving partial output before interruption or a
version-query timeout.

Actual wall-clock timestamps describe execution. The project's `evaluationAt` separately identifies
the frozen clock used for release-age decisions, which can differ because of a configured horizon.
Operation and command durations measure elapsed time, including waiting for tools, locks, or I/O.

Snapshots declare exact adapter-owned inputs and native graph sources. Each file or process stream
has `artifact_started`, bounded `artifact_chunk` records with byte offsets, and `artifact_finished`.
Chunks use UTF-8 text where lossless and byte arrays otherwise. Concatenate decoded chunks by
artifact ID and offset to recover the original bytes. Only a `complete` file artifact establishes
available graph evidence; `missing`, `failed`, and `changed` outcomes never do.

Native lock documents retain distinct copies, sources, workspace packages, and dependency bindings.
Formats containing only pins explicitly declare inventory coverage. Missing locks, unreadable
inputs, and additional inputs that an adapter cannot establish are recorded as limitations. Cooldown
does not run an extra dependency resolve to fill those gaps. Dynamic build scripts and arbitrary
native plugins can read additional files, so a trace is diagnostic evidence, not a guaranteed
hermetic replay bundle.

Initial snapshots precede project changes. Before-mutation and final snapshots run under the same
project lease as their associated operation. A final snapshot describes that observation's input
closure; subsequent operations have their own snapshots. Native command output and validated
mutation inputs also preserve evidence read while resolving staged projects.

## Pruning

Run `cooldown prune` to remove inactive traces and disposable cache artifacts older than 30 days.
Use `--older-than 7d` to change the threshold, `--dry-run` to inspect the selection, and `--json` for a
structured summary.

Pruning uses the last modification time for age and validates trace ownership from JSON headers,
without parsing dates or command names from filenames. It covers owned HTTP cache entries and abandoned Yarn verification copies in cooldown's cache.
It skips active trace files, skips caches held by a running registry-backed adapter, and leaves
unrelated files alone. The monotonic `publish-times.json` trust store is always retained: deleting
that evidence could make a backdated registry publication appear safe. Cargo's resolver-rejection
memo is in memory and has no persisted files to prune.

The [architecture decision](https://github.com/romnn/cooldown/blob/main/adr/0001-default-run-traces.md)
explains ownership, graph fidelity, and incremental recording.
