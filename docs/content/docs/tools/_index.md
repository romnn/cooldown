---
title: Supported ecosystems
weight: 6
bookCollapseSection: false
---

# Supported ecosystems

`cooldown` auto-detects the package managers in a directory and drives each one with the same commands and config. Every package manager is its own `--tool`; one generic adapter is specialised per lockfile format, and adapters that mix registries route each dependency to its source. Common aliases (`python`, `rust`, `node`) are accepted wherever a `--tool` name is.

| Ecosystem | `--tool` | Registry | Reads |
|---|---|---|---|
| Rust | `cargo` | crates.io | `Cargo.toml` / `Cargo.lock` |
| Go | `go` | GOPROXY | `go.mod` / `go.sum` |
| Python (uv) | `uv` | PyPI | `pyproject.toml` / `uv.lock` |
| Python (pip) | `pip` | PyPI | `requirements.txt` |
| Python (Poetry) | `poetry` | PyPI | `pyproject.toml` / `poetry.lock` |
| Python (conda) | `conda` | anaconda.org (+ PyPI) | `conda-lock.yml` |
| Python (pixi) | `pixi` | anaconda.org (+ PyPI) | `pixi.lock` |
| npm | `npm` | npm registry | `package.json` / `package-lock.json` |
| pnpm | `pnpm` | npm registry | `pnpm-lock.yaml` |
| Yarn | `yarn` | npm registry | `yarn.lock` |
| Bun | `bun` | npm registry | `bun.lock` |
| Deno | `deno` | npm + JSR | `deno.json` / `deno.lock` |
| Ruby | `bundler` | rubygems.org | `Gemfile` / `Gemfile.lock` |
| Elixir | `hex` | hex.pm | `mix.exs` / `mix.lock` |
| Java (Maven) | `maven` | Maven Central | `pom.xml` |
| Java (Gradle) | `gradle` | Maven Central | `gradle.lockfile` |
| Swift | `swift` | GitHub Releases | `Package.resolved` |

## How each is driven

`cooldown` never treats a native package manager as the source of policy — the cooldown verdict is computed in one core evaluator. The native tool is used only to **resolve** a lockfile graph and to **apply** changes back to it. That is what keeps "adoptable" identical across ecosystems.

Publish times come from each registry's own metadata — GOPROXY `@v/<ver>.info` for Go, crates.io for Rust, PyPI / anaconda.org for Python, the npm registry and JSR for JavaScript, and GitHub Releases for SwiftPM. Adapters that mix registries (Deno's `npm:` + `jsr:`, conda + PyPI, pixi + PyPI) resolve each dependency against its own source.

## Registries and native cooldowns

Some ecosystems already ship a native cooldown — uv's `exclude-newer`, pnpm's `minimumReleaseAge`, yarn's `npmMinimalAgeGate`. Where one exists, [`cooldown sync`]({{< relref "../commands/other.md" >}}) (or the global `--sync` flag) writes the resolved policy *down* into that native config, so `cooldown.toml` stays the single source of truth and the native tool sees the same window you set once.

## Scoping to a subset

In a polyglot repository, restrict a run to one or more ecosystems with `--tool` (repeatable or comma-separated):

```bash
cooldown outdated --tool cargo,go
cooldown check --tool uv
```

`--cargo` is a shorthand for `--tool cargo` — the right default for a Rust workspace living inside a polyglot monorepo, since it skips detecting and enumerating everything else. When no `--tool` is given, every detected ecosystem is included.

> [!NOTE]
> To act on an ecosystem, its native tool must be installed and on your `PATH`. Ecosystems you don't use need nothing — detection simply skips them. See [Installation]({{< relref "../installation.md" >}}#requirements).

## Adding an ecosystem

Support for a new package manager is one new crate implementing the `Tool` / `PackageRegistry` ports, registered in one line — no change to the core evaluator, the render layer, the config schema, or any other adapter. The architecture is ports-and-adapters (hexagonal): a pure policy core that does no concrete I/O, with dependencies pointing inward.
