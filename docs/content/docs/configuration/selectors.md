---
title: Selectors
weight: 3
---

# Selectors

A selector scopes a policy to part of your dependency graph. The same keys ([`min-age`]({{< relref "basics.md" >}}), `allow`, `floor`, …) work at every level; the level just narrows *what they apply to*. From least to most specific:

```
default  <  tool  <  project  <  registry  <  package
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

## `[registry."<host>"]` — per registry

Scope policy to a registry or index by host. The natural home for "our own registry is trusted":

```toml
[registry."internal.acme.io"]
min-age = "0d"
```

## `[package."<glob>"]` — per package

The most specific selector: scope policy to package names by glob:

```toml
[package."github.com/acme/*"]
min-age = "0d"

[package.serde]
min-age = "3d"
```

Package globs use the same flavor as [`allow`]({{< relref "basics.md" >}}) and [`exclude-packages`]({{< relref "excludes.md" >}}): `*` is always a wildcard and crosses `/`, so `@scope/*` covers a whole npm scope and `serde_*` a crate family. No registry permits `*` in a package name, so nothing needs escaping.

## Choosing a level

- Trust a whole **registry** (an internal index)? Use `[registry."…"]`.
- Loosen or tighten one **ecosystem**? Use `[tool.<name>]`.
- Pin the policy for one **package or family**? Use `[package."…"]`.

Because names differ per ecosystem (`my-pkg` vs `@scope/my-pkg`), a package rule that is ecosystem-specific can also live under a tool — the keys nest as you'd expect. When two rules could both apply, [`explain`]({{< relref "../commands/other.md" >}}) shows which one won and why.
