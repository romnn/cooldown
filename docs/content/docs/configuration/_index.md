---
title: Configuration
weight: 5
bookCollapseSection: true
---

# Configuration

The policy surface is a single file, `cooldown.toml`, at the root of your repository. One schema is used everywhere — the same keys mean the same thing for every ecosystem.

The happy path is **zero config**: the built-in default window is 7 days. Most repositories that configure anything set exactly one line:

```toml
min-age = "14d"
```

From there you can shape policy along two axes — **which layer** a value comes from and **which selector** it targets — resolved by an authority-first model.

- **[Basics]({{< relref "basics.md" >}})** — the `cooldown.toml` keys: `min-age`, `latest`, `freeze`, `allow`, `floor`, and durations.
- **[Precedence]({{< relref "precedence.md" >}})** — how layers and selectors combine, field by field.
- **[Selectors]({{< relref "selectors.md" >}})** — per-tool, per-registry, and per-package policy.
- **[Exclusions]({{< relref "excludes.md" >}})** — trim folders and packages out of a run.

> [!NOTE]
> Run [`cooldown config`]({{< relref "../commands/other.md" >}}) to print the fully-resolved policy with the origin of each value, and [`cooldown explain <pkg>`]({{< relref "../commands/other.md" >}}) to see the derivation for one package. When behavior surprises you, those two commands are the answer.
