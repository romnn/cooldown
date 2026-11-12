---
title: Run traces
weight: 8
---

# Run traces

Design draft for [issue #83](https://github.com/romnn/cooldown/issues/83).

Cooldown will record a typed JSON trace for each executed command by default. The trace is
diagnostic evidence for unexpected holds, resolver conflicts, and slow runs. Recording will be
independent of console verbosity, with `--no-trace` as the explicit opt-out.

The recorder will retain the actual UTC execution time separately from the policy evaluation
clock and freeze cutoff. It will capture repository/project identity, tool versions, starting
graphs and input files, effective policy, timed resolver activity and decisions, and the final
state and complete report, including warnings and errors.

Traces will live below the user's platform-appropriate cooldown configuration directory. A
timestamp, repository name, and unique suffix will distinguish concurrent runs. The command will
print the trace path on stderr on both success and failure, preserving JSON report stdout.

Events will be written throughout execution with bounded memory. Interrupted runs must retain the
events already recorded. Observing a run must not change its resolver decisions, refresh its lock,
or fetch registry data solely to populate the trace. Recording failures will be reported without
changing the dependency command's result.

`cooldown prune` will remove old inactive cooldown-owned artifacts, with an age threshold defaulting
to 30 days. Active runs, unrelated files, and the monotonic publication-time trust store must survive
pruning. The implementation will document the exact trace/cache scope and event schema here.
