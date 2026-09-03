---
title: Selectors
weight: 3
---

# Selectors

A selector scopes a policy to part of your dependency graph. The level narrows *what a rule applies
to*. From least to most specific:

```
default  <  tool  <  project  <  registry  <  package  <  tool-qualified package
```

Within a layer, the most specific selector that matches wins — see [Precedence]({{< relref "precedence.md" >}}).

## Top level — the default

Keys at the top of `cooldown.toml`, outside any table, are the **default** selector — they apply to everything unless a more specific selector overrides them:

```toml
min-age = "14d"
```

## `[tool.<name>]` — per ecosystem

Scope policy to one package manager. Every supported tool is its own name — `cargo`, `go`, `uv`, `pip`, `poetry`, `conda`, `pixi`, `npm`, `pnpm`, `yarn`, `bun`, `deno`, `bundler`, `hex`, `maven`, `gradle`, `swift` — and common aliases (like `python`, `rust`, `node`) are accepted:

```toml
[tool.uv]
min-age = "21d"       # a longer window for Python deps only
```

### `[tool.pnpm] single-copy`

The packages a pnpm resolve must never leave at two resolved copies:

```toml
[tool.pnpm]
single-copy = ["solid-js", "react", "typescript"]
```

A second copy a dependent's own requirement pulls into the graph is ordinarily committed and
reported as a `duplicate_copy` warning. For a listed name the settlement is refused instead: the
lock is restored and the candidate whose landing added the copy is held with the copy and its
requirer named (see [upgrade]({{< relref "../commands/upgrade.md" >}}#whole-workspace-landings-pnpm)).
`--fail-on-new-duplicate` gates every package for one run. The key is pnpm-specific and accepted
only here (like `edge-policy` under `[tool.cargo]`), and the nearest `cooldown.toml` that sets it
wins outright — a nested workspace states its own list in full. Names in `pnpm-workspace.yaml`'s
`overrides` are *not* gated by default: an override pins a version for every request that matches
its range, which already keeps a copy single where the range is exact, and a ranged override is
often about forcing a patched transitive rather than about running the package once — list the
names you mean.

## `[registry."<host>"]` — per registry

Scope policy to a registry or index by host. The natural home for "our own registry is trusted":

```toml
[registry."internal.acme.io"]
min-age = "0d"
```

## Package selectors

An unqualified package rule applies to a matching name in every ecosystem:

```toml
[package."github.com/acme/*"]
min-age = "0d"

[package.glob]
min-age = "14d"
```

Package globs use the same flavor as [`allow`]({{< relref "basics.md" >}}) and [`exclude-packages`]({{< relref "excludes.md" >}}): `*` is always a wildcard and crosses `/`, so `@scope/*` covers a whole npm scope and `serde_*` a crate family. No registry permits `*` in a package name, so nothing needs escaping.

When the same name exists in several ecosystems, qualify the package rule by its tool:

```toml
[tool.uv.package.glob]
min-age = "30d"
max-major = 5

[tool.cargo.package.glob]
min-age = "14d"
```

`[tool.uv.package.glob]` applies only to the PyPI package named `glob`; it cannot accidentally
change the Cargo crate or npm package with the same name. A tool-qualified package rule is more
specific than an unqualified `[package.glob]` rule in the same config layer.

`max-major` is available only on package rules and is an integer. It is an absolute ceiling:
within-major updates remain eligible, but even `upgrade --major --rewrite` will not cross it. For
example, keep TypeScript and its Node declarations current within supported lines with:

```toml
[tool.npm.package.typescript]
max-major = 5

[tool.npm.package."@types/node"]
max-major = 24
```

Raising or removing the value in `cooldown.toml` is the only way to cross a configured
`max-major`. There is no CLI or environment override.

## Choosing a level

- Trust a whole **registry** (an internal index)? Use `[registry."…"]`.
- Loosen or tighten one **ecosystem**? Use `[tool.<name>]`.
- Pin the policy for one **package or family**? Use `[package."…"]`.
- Target a package name in one **ecosystem only**? Use `[tool.<name>.package."…"]`.

When two rules could both apply, [`explain`]({{< relref "../commands/other.md" >}}) shows which one
won and why.

## Migration note

`exclude-folders` and `exclude-packages` belong under `[tool.*]`, `[global]`, or a command table,
not under package, registry, or project selector tables. Older versions accepted those keys under
non-tool selectors and then silently ignored them. They now fail configuration parsing so a
misspelled or misplaced exclusion cannot look active when it is not.
