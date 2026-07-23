---
title: Documentation
bookToc: false
bookFlatSection: false
---

# Documentation

`cooldown` is a language-agnostic CLI that refuses to adopt any dependency version younger than a minimum release age. It auto-detects the package managers in a directory and exposes the same commands, flags, and config for all of them, computing the cooldown verdict in one core evaluator. This documentation takes you from installation to the full policy model.

## Start here

- **[Introduction]({{< relref "introduction.md" >}})** — the threat it addresses and how it thinks about a dependency graph.
- **[Installation]({{< relref "installation.md" >}})** — install the `cooldown` binary.
- **[Quick start]({{< relref "quick-start.md" >}})** — your first run and how to read the output.

## Go deeper

- **[Commands]({{< relref "commands/_index.md" >}})** — `outdated`, `upgrade`, `fix`, `check`, the smaller commands, and the CLI reference.
- **[Configuration]({{< relref "configuration/_index.md" >}})** — the `cooldown.toml` policy surface, its authority-first precedence model, selectors, and exclusions.
- **[Supported ecosystems]({{< relref "tools/_index.md" >}})** — every package manager and how each is resolved and applied.
- **[Continuous integration]({{< relref "ci/_index.md" >}})** — wire the gate into GitHub Actions.
- **[Security model]({{< relref "security.md" >}})** — the threat model, the risk surface, and the cache hardening.
- **[FAQ]({{< relref "faq.md" >}})** — answers to common questions.

> [!NOTE]
> The **CLI is the supported interface.** The Rust crates exist for the project's own binaries and integration tests and carry no stability guarantees — do not depend on them.
