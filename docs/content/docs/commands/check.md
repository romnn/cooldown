---
title: check
weight: 4
---

# `check`

`check` is the **fail-closed CI gate**. It evaluates the resolved lockfile graph — direct and transitive — and exits non-zero if anything is younger than its cooldown. It reads the lock as-is and never mutates it; `--lock` is the one opt-in exception.

```bash
cooldown check
```

{{< terminal name="check" >}}

A clean run exits `0`. When a dependency is too fresh, `check` lists the violation and exits `1`:

```text
 Package   Version   Cooldown   Status      Notes
──────────────────────────────────────────────────────────
 left-pad  3.1.0     2d/7d      too fresh   published 2d ago

checked 41 (7 direct) · 1 violation · 0 acknowledged · …
```

Because the gate is fail-closed, a non-zero exit is its contract — wire it into CI as-is and a too-fresh dependency stops the build. See [Exit codes]({{< relref "cli-reference.md" >}}#exit-codes) for what each code means.

## The risk surface is the resolved graph

`check` evaluates **direct and transitive** dependencies by default, because the resolved graph — not your manifest — is what actually ends up in your build. A [`floor`]({{< relref "../configuration/precedence.md" >}}) applies to transitive dependencies too.

For a too-fresh transitive you can't act on (a graph-held pin, or one you'd rather not block CI on), `--transitive` relaxes the gate — but the strict default stays opt-out, never opt-in:

- **`--transitive allow`** — keep too-fresh transitives visible but non-fatal.
- **`--transitive hide`** — skip evaluating transitive dependencies entirely (a direct-only gate).

## Flags

| Flag | Effect |
|---|---|
| `--transitive <mode>` | `allow` (visible, non-fatal) or `hide` (direct-only). Default: fail on too-fresh transitives. |
| `--all-artifacts` | Gate every artifact in a universal lock, not just the ones relevant to the current environment. |
| `--fail-on-unknown-age` | Fail (rather than warn) on dependencies with no known publish time. |
| `--lock` | Refresh lockfiles before checking (mutates lockfiles; ignored under `--dry-run`). Cargo runs `cargo update --workspace` (a missing lock is generated and the members' own requirements re-resolved; a locked version the manifests still admit stays put, one they no longer admit is re-resolved or dropped) and pnpm runs `pnpm install --lockfile-only`; the other tools have no standalone refresh, say so with a warning, and read the existing lock as-is. Only the projects the run evaluates are refreshed (`-C incubator` refreshes the nested workspace's lock, not the enclosing one's), and every such project is refreshed before `--package` narrows the rows. A refresh the tool rejects is an error even under `--allow-stale-lock`, and `--offline` rejects the flag outright because the refresh needs the resolver's network access. |
| `--fail-on-stricter-native` | Fail when repo policy overrides a *stricter* native cooldown value. |
| `--no-fail-on-stricter-native` | Turn off a config-set `strict-native` (the only way to disable it on the CLI). |
| `--fail-on-advisory-source` | Fail (exit `4`) when the enabled [advisory feed]({{< relref "../configuration/advisories.md" >}}) yields no usable evidence — unreachable, unimplemented, or too stale to shorten — instead of warning. An uncovered ecosystem stays a warning. |

## Security-relevant pins

With the [advisory feed]({{< relref "../configuration/advisories.md" >}}) enabled, a locked version
that is itself an advisory's fix version is counted in the summary (`… · 1 security-relevant`) and
carries a `security` object in `--json`. Under the feed's `shorten` mode such a pin resolves
against the shorter security window, so merging a security bot's bump stops failing the next gate
run. The TTY Notes column adds `security window applied`, and the Cooldown cell names the advisory
that shortened it, so the shorter number cannot be mistaken for ordinary configuration.
`check` never *fails* because a pin is vulnerable — that job stays with `govulncheck` /
`cargo audit`, so exit `1` keeps exactly one meaning.

## Handling a red gate

When `check` goes red, there are three ways forward — covered in [Quick start]({{< relref "../quick-start.md" >}}#4-when-the-gate-goes-red):

1. **Wait** for the version to mature.
2. [**`baseline`**]({{< relref "other.md" >}}) it — acknowledge the specific version so `check` adopts it cleanly.
3. [**`fix`**]({{< relref "fix.md" >}}) it — downgrade the violating dependency to a matured version.

> [!NOTE]
> `check` never trusts a backdated timestamp: a cached publish time may only ever move *later* on refresh (a monotonic floor), so an upstream that reports an earlier date is rejected, not adopted. See the [Security model]({{< relref "../security.md" >}}).

## Unknown ages and stale locks

- A dependency whose **publish time can't be determined** is reported as `unknown-age`. By default that is a warning; `--fail-on-unknown-age` makes it fatal, and `--offline` turns every cache miss into `unknown-age` rather than a false "ok".
- A **stale or absent lock** is an error (exit `4`) by default, because gating an out-of-date graph is meaningless. The error names the project whose lock is stale (`Cargo.lock is stale in /repo/incubator; run …`), so in a repository with several lockfiles you know which one to refresh. `--allow-stale-lock` (or `COOLDOWN_ALLOW_STALE_LOCK`) demotes it to a warning and skips dependency evaluation for that project; the stale graph is never treated as checked. Refresh in place with `--lock` when the project must be evaluated.
