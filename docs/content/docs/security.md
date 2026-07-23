---
title: Security model
weight: 8
---

# Security model

`cooldown` is a supply-chain control, so it is worth being precise about what it defends, what it doesn't, and the properties that make the defense hold.

## Threat model

The target is the **smash-and-grab window**: a malicious version is published to a registry and is detected and yanked within hours to a few days. A cooldown delays *adoption* until that window has passed, so the community's tooling and reports run before the code reaches your builds.

It is **not a malware scanner** — it never inspects code — and it does not replace the tools that do a different job. It pairs with `govulncheck`, `cargo audit`, and advisory feeds, which catch known vulnerabilities and published advisories rather than not-yet-known malicious releases.

## The risk surface is the resolved graph

What you declare is not what you ship — the resolved lockfile is. A single `^1.4` pulls in transitive dependencies, and a re-lock can silently advance any of them to a brand-new version. So [`check`]({{< relref "commands/check.md" >}}) evaluates **direct and transitive** dependencies by default, and a [`floor`]({{< relref "configuration/precedence.md" >}}) applies to transitives too.

For a too-fresh transitive you genuinely can't act on — one the graph pins, or one you'd rather not block CI on — the gate can be relaxed, but only deliberately:

- `check --transitive allow` — keep it visible but non-fatal.
- `check --transitive hide` — a direct-only gate.

The strict, whole-graph default stays **opt-out, never opt-in**.

## Mutations never leave a rejectable lock

`upgrade` and `fix` apply **one change at a time**. If a re-lock leaves a new too-fresh, non-acknowledged dependency in the graph, the tool **restores the lock snapshot and skips that change**. A mutation that reports success therefore never leaves a lock that a subsequent `check` would reject — the graph you end up on is gate-clean by construction.

## Cache hardening

Publish times are cached, and the cache is treated as adversarial input:

- A cached publish time may **never move earlier** on refresh — it is a monotonic floor.
- A **backdated** upstream timestamp (a release claiming to be older than what was already recorded) is rejected, not trusted.

This closes the obvious bypass — an attacker backdating a release so it appears to have already cleared the window.

Two flags control how the cache is used at the boundary: `--offline` turns every cache miss into `unknown-age` (never a false "ok"), and `--fresh` ignores the cache entirely and re-fetches — the right choice for a CI gate that must not pass on stale data.

## Escape hatches are explicit and audited

Loosening the policy is always deliberate and always visible:

- Exemptions (`--latest`, `--allow`, config `allow`) are audited — every one shows up in [`cooldown explain`]({{< relref "commands/other.md" >}}).
- A [`floor`]({{< relref "configuration/precedence.md" >}}) bounds config-level loosening: it is max-clamped across layers, so a more specific block can't quietly weaken it, and an `allow` can only bypass it when co-declared with it (or via an audited CLI flag).

There is no silent path to "adopt anything" — every one leaves a trail.

## Reporting a vulnerability

For a security issue in `cooldown` itself, please use GitHub's private vulnerability reporting on the [repository](https://github.com/romnn/cooldown) rather than a public issue.
