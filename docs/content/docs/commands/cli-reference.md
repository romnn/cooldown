---
title: CLI reference
weight: 6
---

# CLI reference

The commands are documented on their own pages ([`outdated`]({{< relref "outdated.md" >}}), [`upgrade`]({{< relref "upgrade.md" >}}), [`fix`]({{< relref "fix.md" >}}), [`check`]({{< relref "check.md" >}}), [others]({{< relref "other.md" >}})). This page lists the **global flags** they share and the **exit-code** contract. `cooldown --help` prints the authoritative, version-specific list.

Global flags may appear before or after the subcommand: `cooldown --dry-run upgrade` and `cooldown upgrade --dry-run` are equivalent.

## Policy and window

| Flag | Effect |
|---|---|
| `--min-age <DUR>` | The cooldown window: `7d`, `2 weeks`, `36h`, ISO-8601 `P7D` (default `7d`). |
| `--min-age-major <DUR>` | Per-kind window for major-version jumps. |
| `--min-age-minor <DUR>` | Per-kind window for minor jumps. |
| `--min-age-patch <DUR>` | Per-kind window for patch jumps. |
| `--latest` | Opt out — window `0` (alias `--no-min-age`). The explicit, audited escape hatch. |
| `--freeze <DATE>` | An absolute cutoff instead of a rolling window (reproducible). |
| `--allow <GLOB>` | Exempt matching packages from the cooldown (repeatable, audited). |

`--min-age`, `--latest`, and `--freeze` are mutually exclusive. Every escape hatch shows up in [`explain`]({{< relref "other.md" >}}).

## Scope and selection

| Flag | Effect |
|---|---|
| `--major` | Allow cross-major changes. On by default for `outdated`, off for the mutating commands. |
| `--no-major` | Stay within the current major (alias `--minor`). |
| `-p, --package <GLOB>` | Scope the command to matching packages (repeatable). |
| `--tool <TOOL>` | Restrict to tool(s) — `cargo`, `go`, `uv`, … (comma-separated / repeatable; default: all detected). |
| `--cargo` | Only the Rust/Cargo tool — shorthand for `--tool cargo`. |
| `--exclude-folders <GLOB>` | Directories never scanned, `.gitignore`-style (repeatable). |
| `--exclude-packages <GLOB>` | Workspace members dropped from reports by package-name glob (repeatable). |
| `--no-gitignore` | Don't honor `.gitignore` during project detection. |

See [Exclusions]({{< relref "../configuration/excludes.md" >}}) for the folder and package glob semantics.

## Execution and network

| Flag | Effect |
|---|---|
| `--sync` | Sync the policy into native config before running (no-op under `--dry-run`). |
| `-n, --dry-run` | Resolve and print the plan; never mutate. |
| `--offline` | Cache only; a cache miss becomes `unknown-age`, never a false "ok". |
| `--fresh` | Ignore the local cache and always hit the registry (alias `--no-cache`; use in CI gates). |
| `--concurrency <N>` | Registry request fan-out width and per-host in-flight cap (default `16`). |
| `--allow-stale-lock` | Demote a stale/absent lock from an error to a warning. |

## Config layers

| Flag | Effect |
|---|---|
| `--config <PATH>` | Load one extra, highest-precedence file layer (still below env / flags). |
| `--no-native` | Ignore the native config layer (reproducibility / debugging). |
| `--no-global` | Ignore the global config layer. |
| `-C, --dir <PATH>` | Run as if from `<path>`. |

See [Precedence]({{< relref "../configuration/precedence.md" >}}) for how the layers combine.

## Output and presentation

| Flag | Effect |
|---|---|
| `--json` | Machine-readable output (never changes the exit code). |
| `--color <WHEN>` | `auto` (TTY + `NO_COLOR` unset), `always`, or `never`. |
| `--log-level <LEVEL>` | Diagnostic log on stderr: `off`, `error`, `warn`, `info`, `debug`, `trace` (`RUST_LOG` overrides). |
| `--no-progress` | Suppress the human-facing progress display (alias `--quiet`). |
| `--list-packages` | List every source package on its own line instead of `first (+N others)`. |
| `--paths` | Show the **Used by** column as workspace paths instead of package names. |
| `--show-projects` | Add a **Project** column attributing each row to its project. |
| `--no-suggestions` | Suppress actionable tips (the reports and their counts are unaffected). |

## Environment variables

Several flags mirror an environment variable, so CI can set policy once:

| Variable | Equivalent |
|---|---|
| `COOLDOWN_TOOL` | `--tool` |
| `COOLDOWN_CONFIG` | `--config` |
| `COOLDOWN_DRY_RUN` | `--dry-run` |
| `COOLDOWN_OFFLINE` | `--offline` |
| `COOLDOWN_CONCURRENCY` | `--concurrency` |
| `COOLDOWN_ALLOW_STALE_LOCK` | `--allow-stale-lock` |
| `COOLDOWN_LOG` | `--log-level` |
| `RUST_LOG` | overrides `--log-level` |

A `COOLDOWN_*` environment layer also feeds config values; it sits above the file cascade and below CLI flags in [precedence]({{< relref "../configuration/precedence.md" >}}).

## Exit codes

`check` is the CI gate, so a non-zero exit is its contract:

| Code | Meaning |
|---|---|
| `0` | Clean / nothing to do. |
| `1` | Policy violation (`check`), or an incomplete mutation under `upgrade` / `fix --strict`. |
| `2` | Usage / config error (bad duration, unknown `--tool`, mutually-exclusive flags, …). |
| `3` | No tool detected. |
| `4` | Stale/absent lock, registry unreachable, a tool failed, or unknown-age under a flag. |
