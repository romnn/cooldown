---
title: FAQ
weight: 9
---

# FAQ

## What is the default cooldown window?

**7 days.** Nothing younger than a week is adoptable until you say otherwise. Raise it repo-wide with one line — `min-age = "14d"` — or opt a package, registry, or ecosystem in or out with a [selector]({{< relref "configuration/selectors.md" >}}).

## Does `cooldown` block me from installing packages?

No. It doesn't intercept your package manager or your installs. It reasons about a **lockfile graph**: `check` reports and gates, `outdated` shows what's available, and `upgrade` / `fix` move the lock deliberately. You stay in control of when anything changes.

## Is this a malware scanner?

No — it never inspects code. It is a *timing* control: it refuses to be the first to adopt a brand-new release, so the window in which a smash-and-grab attack is live and undetected passes before the code reaches you. Run it alongside `govulncheck`, `cargo audit`, and advisory feeds, which catch a different class of problem. See the [Security model]({{< relref "security.md" >}}).

## Why did `check` fail on a transitive dependency I don't control?

Because the resolved graph — not just what you declared — is the real risk surface, and a re-lock can pull a brand-new transitive in. You have options: [`fix`]({{< relref "commands/fix.md" >}}) rolls it back to a matured version, `check --transitive allow` keeps it visible but non-fatal, and `check --transitive hide` gates direct dependencies only. If the graph pins it and nothing lower satisfies its requirers, `cooldown` names the dependency forcing the fresh pin so you can address the cause.

## A dependency shows as `unknown-age`. What does that mean?

`cooldown` couldn't determine that version's publish time (a registry gap, or a cache miss under `--offline`). By default that's a warning, not a failure — a `check` won't turn red on it. Make it fatal with `--fail-on-unknown-age` when you want the gate to insist on a known age.

## How is this different from Dependabot or Renovate?

Those tools *propose* updates; `cooldown` gates on *age*. They're complementary: let Renovate open the PRs, and let `cooldown check` make sure none of them adopts something too fresh. `cooldown upgrade` also only ever moves to versions that have already matured, so the two compose cleanly.

## Does it need network access?

Yes, to read publish times from each registry — unless you run `--offline` against a warm cache, in which case a cache miss becomes `unknown-age` rather than a guess. In CI, prefer `--fresh` so the gate re-fetches and can't pass on stale data.

## Can I pin an absolute cutoff instead of a rolling window?

Yes — `freeze = "2026-06-01"` (or `--freeze`) evaluates against a fixed instant, so a run is reproducible no matter when it happens. It's the reproducible counterpart to a rolling `min-age`.

## Why did `upgrade` also *downgrade* some packages?

When advancing the graph would pull a too-fresh transitive in, `upgrade` reconciles it back down to a matured version so the resulting lock is gate-clean. A single `upgrade` run can therefore show both `upgraded` and `downgraded` rows — that's the whole-graph guarantee at work, not a mistake. See [`upgrade`]({{< relref "commands/upgrade.md" >}}).

## Is the Rust API stable?

No. The CLI (and its `--json` output, whose schema [`cooldown schema`]({{< relref "commands/other.md" >}}) prints) is the supported interface. The crates exist for the project's own binaries and integration tests and carry no stability guarantees.
