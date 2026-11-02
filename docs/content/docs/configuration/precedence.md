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
tool-qualified package  >  package  >  registry  >  project  >  tool  >  default
```

So a `[package."serde_*"]` block beats a `[tool.cargo]` block beats the bare top-level default — but only *within the same layer*. Across layers, authority comes first: a `[tool.cargo]` value set by a CLI flag still beats a `[package."…"]` value set in a file, because the flag is a higher layer.

**Patterns** — most to least narrowly spelled. When two matching rules in one layer have equally specific selectors — two `[package."…"]` rules, or two `[project."…"]` rules — their patterns decide:

1. A **literal** pattern, one without any of `*` `?` `[` `]` `{` `}` `\`, beats every glob.
2. Between globs, the **longer literal prefix** wins: the characters before the first glob character. `@corp/ui*` (prefix `@corp/ui`) beats `@corp/*` (prefix `@corp/`).
3. Between equally long prefixes, the pattern whose text sorts first wins, compared character by character by code point: case-sensitive and independent of locale.

The complete order within a field is therefore:

```
higher layer  >  more specific selector  >  literal pattern  >  longer literal prefix  >  smaller pattern text
```

It depends only on the rules themselves, never on the order the tables are written in:

```toml
[package."@corp/*"]
latest = true

[package."@corp/ui"]
min-age = "30d"
```

`@corp/ui` waits 30 days, and every other `@corp/` package is exempt.

The prefix rule favors a namespace over a suffix: for `@corp/ui-plugin`, `[package."@corp/ui*"]` beats `[package."*-plugin"]`, because its literal prefix is longer. That is a precedence rule, not a claim that one glob contains the other — neither does here. Where two overlapping globs set conflicting values, give the packages in the overlap a literal rule, and use [`explain`]({{< relref "../commands/other.md" >}}) to see which rule wins.

Project patterns rank the same way: for the project at `apps/web`, `[project."apps/web"]` beats `[project."apps/*"]`, and below it `[project."apps/web/*"]` beats `[project."apps/*"]`.

Selector specificity still comes before the pattern, and layers before both: a `[tool.npm.package."@corp/*"]` rule beats an unqualified `[package."@corp/ui"]` rule in the same layer, and a glob in a higher layer beats a literal in a lower one.

## Resolution is per field

Different fields combine differently — each is resolved on its own:

| Field | Rule | Meaning |
|---|---|---|
| `min-age` | **authority-first** | The highest layer wins; within a layer, the most specific selector breaks the tie, then the most narrowly spelled pattern. |
| `max-major` | **authority-first** | Like `min-age`, the highest layer wins, then selector specificity, then the pattern. It exists only on package rules and has no CLI or environment form. |
| `floor` | **max-clamped** | The strictest value across *all* layers wins — it only ever ratchets stricter, never looser. |
| `allow` | **accumulated union** | Exemptions from every layer are unioned together. |

Each authority-first field is ranked on its own, among only the rules that set it. A per-kind window is searched across every layer before the bare `min-age` fills in for it:

```toml
[package."@corp/*"]
min-age = { major = "30d" }

[package."@corp/ui"]
min-age = "10d"
```

`@corp/ui` waits 10 days for its locked pin and for minor and patch updates, but a major update still waits 30 days: the literal rule sets no `major` window, so the glob's is the most specific one that does. `max-major` is ranked the same way, independently of the windows.

This is why a `floor` set high in your config can't be weakened by a more specific block lower down:
`floor` doesn't follow authority-first, it takes the maximum. `min-age` and `max-major` can be
replaced by a more specific rule in the same layer or by a rule in a higher-authority layer. A repo
can therefore deliberately loosen a global `max-major`; it is a compatibility choice, not a
security floor.

## How `allow` interacts with `floor`

An exemption (`allow`) can zero a window, but a `floor` is a hard minimum. The two are reconciled deliberately: an `allow` can bypass a `floor` **only when it is co-declared with it** — that is, the same config that sets the floor also grants the exemption — or via an audited `--latest` / `--allow` on the command line.

The effect: you can't accidentally exempt your way under a floor that a *different, higher* layer set. Bypassing a floor is always a deliberate, co-located, auditable decision. [`explain`]({{< relref "../commands/other.md" >}}) shows the residual floor on an exempted package so the interaction is never hidden:

{{< terminal name="explain" >}}

## Seeing the derivation

Two commands make the model concrete:

- [`cooldown config`]({{< relref "../commands/other.md" >}}) prints the fully-resolved policy with the origin of each value — the effective window and which layers produced it.
- [`cooldown explain <pkg>`]({{< relref "../commands/other.md" >}}) prints the layer-by-layer, selector-by-selector derivation for one package, marking which rules were considered and which won.

Reach for `explain` whenever you need to answer "why does *this* package have *that* window" — it is the authoritative account, not an approximation.
