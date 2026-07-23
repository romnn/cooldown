---
title: Introduction
weight: 1
---

# Introduction

`cooldown` is a single CLI that enforces a **dependency cooldown** — a minimum release age — across every package manager it supports. It refuses to adopt any dependency version younger than _N_ days, so a freshly published (possibly compromised) release has time to be caught before it reaches your builds.

## The problem

Supply-chain attacks on package registries overwhelmingly follow a **smash-and-grab** pattern: a malicious version is published, and it is detected and yanked within hours to a few days. The window in which the bad version is live and adoptable is short — which is exactly what a cooldown targets.

A minimum release age is the cheapest, highest-leverage defense against that window. It is not a malware scanner and does not inspect code; it simply refuses to be the first to adopt anything, letting the wider community's tooling and reports run first. It pairs naturally with `govulncheck`, `cargo audit`, and advisory feeds, which catch different classes of problem.

The catch is that the risk surface is not what you *declared* — it is what you *resolved*. A single-line `^1.4` can pull in dozens of transitive dependencies, any of which a re-lock can silently advance to a brand-new version. So the gate has to reason over the **whole resolved graph**, direct and transitive, not just your manifest.

## The fragmentation problem

Several ecosystems already ship a cooldown, but each is its own island:

| Tool | Knob |
|---|---|
| uv | `exclude-newer` |
| pnpm | `minimumReleaseAge` |
| yarn | `npmMinimalAgeGate` |
| … | different name, config surface, and UX each time |

If your repo is polyglot, that means learning and maintaining a different mechanism per language — and many ecosystems have no cooldown at all.

## The approach

`cooldown` collapses all of that into **one tool, one mental model**. It auto-detects the languages in a directory and exposes the same subcommands, flags, config, and (pretty + JSON) output for every one of them:

{{< terminal name="outdated" >}}

The cooldown verdict is computed in a **single core evaluator**. Native package managers (`cargo`, `go`, `uv`, `pnpm`, …) are used only as resolution and apply engines — to produce a lockfile graph and to write changes back — never as the source of policy. That keeps the meaning of "adoptable" identical no matter which ecosystem a dependency comes from.

## What you can do with it

| Goal | Command |
|---|---|
| See what could update, split into adoptable-now vs in-cooldown | [`cooldown outdated`]({{< relref "commands/outdated.md" >}}) |
| Move to the newest matured version and re-lock | [`cooldown upgrade`]({{< relref "commands/upgrade.md" >}}) |
| Downgrade too-fresh dependencies to clear a violation | [`cooldown fix`]({{< relref "commands/fix.md" >}}) |
| Gate CI: fail if anything resolved is younger than the cooldown | [`cooldown check`]({{< relref "commands/check.md" >}}) |
| Understand why a package has the window it has | [`cooldown explain`]({{< relref "commands/other.md" >}}) |

The default window is **7 days**; opting out is always explicit (`--latest`).

> [!NOTE]
> **Supported surface.** The CLI is the supported, stable interface. The crates also expose a Rust API, but it exists only for the tool's own binaries and integration tests and has no stability guarantees — do not depend on it.
