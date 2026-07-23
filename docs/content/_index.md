---
title: cooldown
type: docs
bookToc: false
---

<div class="cd-hero">
  <div class="cd-hero__text">
    <h1>cooldown</h1>
    <p class="cd-hero__lead">A unified, language-agnostic <strong>dependency-cooldown</strong> CLI: refuse to adopt any dependency version younger than a minimum release age — across every package manager, from one policy core. Open source, MIT / Apache-2.0.</p>
    <div class="cd-hero__cmd">cooldown check</div>
    <div class="cd-hero__actions">
      <a class="cd-btn cd-btn--primary" href="{{< relref "/docs/introduction.md" >}}">Read the docs</a>
      <a class="cd-btn" href="https://github.com/romnn/cooldown">Source on GitHub</a>
    </div>
  </div>
  <div class="cd-hero__shot">
    {{< terminal name="outdated" >}}
  </div>
</div>

<div class="cd-badges">

[![build status](https://img.shields.io/github/actions/workflow/status/romnn/cooldown/build.yaml?label=build)](https://github.com/romnn/cooldown/actions/workflows/build.yaml)
[![test status](https://img.shields.io/github/actions/workflow/status/romnn/cooldown/test.yaml?label=test)](https://github.com/romnn/cooldown/actions/workflows/test.yaml)
[![crates.io](https://img.shields.io/crates/v/cooldown)](https://crates.io/crates/cooldown)
[![docs.rs](https://img.shields.io/docsrs/cooldown/latest?label=docs.rs)](https://docs.rs/cooldown)

</div>

## Why a cooldown

Supply-chain attacks on package registries overwhelmingly follow a smash-and-grab pattern: a malicious version is published and is detected and yanked within hours to a few days. A **cooldown** — a minimum release age — is the cheapest, highest-leverage defense: refuse to adopt any version younger than _N_ days, so the community's immune system runs before the code reaches your builds.

Cooldown support exists today, but it is fragmented per tool (uv `exclude-newer`, pnpm `minimumReleaseAge`, yarn `npmMinimalAgeGate`, …), each with a different name, config surface, and UX. `cooldown` collapses them into **one tool, one mental model**: it auto-detects the languages in a directory and exposes the same subcommands, flags, config, and output for all of them. The verdict is computed in one core evaluator; native package managers are used only as resolution and apply engines, never as the source of policy.

<div class="cd-cards">
  <div class="cd-card">
    <h3>Whole-graph gate</h3>
    <p>The risk surface is the resolved lockfile — direct <em>and</em> transitive — so <code>check</code> reasons over the whole graph, not just what you declared.</p>
  </div>
  <div class="cd-card">
    <h3>One tool, every ecosystem</h3>
    <p>Cargo, Go, uv/pip/poetry, npm/pnpm/yarn/bun/deno, Bundler, Hex, Maven/Gradle, SwiftPM — the same commands and config for all.</p>
  </div>
  <div class="cd-card">
    <h3>Move, fix, or gate</h3>
    <p><code>upgrade</code> within the window, <code>fix</code> to downgrade violations to a matured version, and <code>check</code> as a fail-closed CI gate.</p>
  </div>
  <div class="cd-card">
    <h3>Explicit, audited policy</h3>
    <p>One <code>cooldown.toml</code>, an authority-first precedence model, and escape hatches that always show up in <code>explain</code>.</p>
  </div>
</div>

## Example

```bash
# Install a prebuilt binary
brew install --cask romnn/tap/cooldown

# What could update — "adoptable now" vs "in cooldown"
cooldown outdated

# Move to the newest version older than the cooldown, then re-lock
cooldown upgrade

# CI gate: exit non-zero if anything resolved is younger than the cooldown
cooldown check
```

The happy path is zero config. Raising the whole repo to 14 days is one line of `cooldown.toml`:

```toml
min-age = "14d"
```

## Documentation

- [Introduction]({{< relref "/docs/introduction.md" >}}) and [Installation]({{< relref "/docs/installation.md" >}}).
- [Quick start]({{< relref "/docs/quick-start.md" >}}) — a first run and how to read the output.
- [Commands]({{< relref "/docs/commands/_index.md" >}}) — `outdated`, `upgrade`, `fix`, `check`, and the rest.
- [Configuration]({{< relref "/docs/configuration/_index.md" >}}) — the policy surface and its precedence model.
- [Supported ecosystems]({{< relref "/docs/tools/_index.md" >}}) — every package manager, and how each is driven.
- [Continuous integration]({{< relref "/docs/ci/_index.md" >}}) — the gate in GitHub Actions.
