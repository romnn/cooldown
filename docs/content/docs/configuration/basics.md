---
title: Basics
weight: 1
---

# Configuration basics

`cooldown.toml` at the repo root is the policy surface. This page covers the keys you set on it; [Selectors]({{< relref "selectors.md" >}}) covers scoping them per tool, registry, or package, and [Precedence]({{< relref "precedence.md" >}}) covers how they combine.

## `min-age`

The cooldown window — the one knob most repos ever set. In its **scalar** form it is a single duration:

```toml
min-age = "14d"
```

In its **table** form it sets a different window per version-change kind:

```toml
min-age = { default = "14d", major = "30d", minor = "14d", patch = "7d" }
```

A bigger jump is a bigger change, so a longer window for majors than patches is a common shape — wait longer before adopting a brand-new major, less for a patch.

## Durations

Every duration field accepts the same flavors:

- Compact: `"7d"`, `"36h"`, `"2w"`.
- Human: `"2 weeks"`, `"10 days"`.
- ISO-8601: `"P7D"`.

## `latest` and `freeze`

Two ways to express the extremes of a window:

```toml
latest = true            # sugar for min-age = "0d" — adopt anything, no cooldown
freeze = "2026-06-01"    # an absolute cutoff instead of a rolling window
```

`freeze` pins the "as-of" instant: nothing published after that date is adoptable, no matter when you run `cooldown`. It makes a run **reproducible** — useful for pinning an audited state, or for a CI job that should evaluate against a fixed point in time rather than a moving one.

## `allow`

An **exemption set** — packages the cooldown does not apply to:

```toml
allow = ["acme/*"]
```

Exemptions are audited: every one shows up in [`cooldown explain`]({{< relref "../commands/other.md" >}}), so an exemption is always visible rather than silent. An `allow` glob uses the same flavor as the `[package."…"]` [selector]({{< relref "selectors.md" >}}).

## `floor`

A **hard minimum** that no nearer, more-specific config can weaken:

```toml
floor = "3d"
```

Where `min-age` is authority-first (a higher layer overrides a lower one), `floor` only ever ratchets *stricter*: it is max-clamped across layers, so a `floor = "3d"` set high in your config can't be dialed back down by a more specific `[package."…"]` block. It is the guardrail for "no matter what else is configured, never adopt anything younger than this." An `allow` can bypass a floor only when it is co-declared with it (or via an audited `--latest` / `--allow`) — see [Precedence]({{< relref "precedence.md" >}}).

## A worked example

```toml
# Repo-wide default, with a longer window for a brand-new major.
min-age = { default = "14d", major = "30d" }

# Our own registry is trusted — no cooldown.
[registry."internal.acme.io"]
min-age = "0d"

# A trusted first-party package family, exempt.
allow = ["acme/*"]

# But never, anywhere, adopt something younger than 2 days.
floor = "2d"
```

Every value above has an origin and a scope; [`cooldown config`]({{< relref "../commands/other.md" >}}) prints the resolved result, and [Precedence]({{< relref "precedence.md" >}}) explains exactly how the last two rules interact.
