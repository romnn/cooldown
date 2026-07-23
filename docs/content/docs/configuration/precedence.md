---
title: Precedence
weight: 2
---

# Precedence

When more than one rule could set a value, `cooldown` resolves it with an **authority-first** model along two orthogonal axes: which *layer* a value comes from, and which *selector* it targets. This is the model [`cooldown explain`]({{< relref "../commands/other.md" >}}) renders, field by field.

## The two axes

**Layers** — low to high authority. A higher layer overrides a lower one:

1. Built-in default
2. Global config
3. Native manifest config (e.g. a tool's own `exclude-newer`)
4. Repo / project `cooldown.toml` cascade — **nearer wins** when several apply
5. `--config` file
6. `COOLDOWN_*` environment
7. CLI flags

**Selectors** — most to least specific. Within a single layer, a more specific selector breaks the tie:

```
package  >  registry  >  project  >  tool  >  default
```

So a `[package."serde_*"]` block beats a `[tool.cargo]` block beats the bare top-level default — but only *within the same layer*. Across layers, authority comes first: a `[tool.cargo]` value set by a CLI flag still beats a `[package."…"]` value set in a file, because the flag is a higher layer.

## Resolution is per field

Different fields combine differently — each is resolved on its own:

| Field | Rule | Meaning |
|---|---|---|
| `min-age` | **authority-first** | The highest layer wins; within a layer, the most specific selector breaks the tie. |
| `floor` | **max-clamped** | The strictest value across *all* layers wins — it only ever ratchets stricter, never looser. |
| `allow` | **accumulated union** | Exemptions from every layer are unioned together. |

This is why a `floor` set high in your config can't be weakened by a more specific block lower down: `floor` doesn't follow authority-first, it takes the maximum. And it is why `min-age` *can* be overridden by a more specific or higher-authority rule: that field does follow authority-first.

## How `allow` interacts with `floor`

An exemption (`allow`) can zero a window, but a `floor` is a hard minimum. The two are reconciled deliberately: an `allow` can bypass a `floor` **only when it is co-declared with it** — that is, the same config that sets the floor also grants the exemption — or via an audited `--latest` / `--allow` on the command line.

The effect: you can't accidentally exempt your way under a floor that a *different, higher* layer set. Bypassing a floor is always a deliberate, co-located, auditable decision. [`explain`]({{< relref "../commands/other.md" >}}) shows the residual floor on an exempted package so the interaction is never hidden:

{{< terminal name="explain" >}}

## Seeing the derivation

Two commands make the model concrete:

- [`cooldown config`]({{< relref "../commands/other.md" >}}) prints the fully-resolved policy with the origin of each value — the effective window and which layers produced it.
- [`cooldown explain <pkg>`]({{< relref "../commands/other.md" >}}) prints the layer-by-layer, selector-by-selector derivation for one package, marking which rules were considered and which won.

Reach for `explain` whenever you need to answer "why does *this* package have *that* window" — it is the authoritative account, not an approximation.
