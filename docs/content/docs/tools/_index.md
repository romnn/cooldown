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

Cargo projects must currently use the workspace-root `Cargo.lock`.
Cooldown fails explicitly when Cargo's `resolver.lockfile-path` configuration or
`CARGO_RESOLVER_LOCKFILE_PATH` selects a custom location, because safely staging, normalizing, and
recovering that alternate file requires it to become part of the adapter's typed lock identity.
Cargo configuration `include`, legacy `paths`, path-backed config patches, local registries, and
file-backed registry indices are also rejected until cooldown can snapshot their complete local
input closure. This includes `CARGO_REGISTRIES_<NAME>_INDEX` overrides.
Cooldown also rejects a temporary staging location whose ancestors contain Cargo configuration
outside the active Cargo home, or a Rust toolchain file, because those files could affect only the
isolated trial and not the source project Cargo first described. This check runs when staging is
prepared. Cargo discovers ancestor configuration again when each subprocess starts, so a file
created in that ancestor chain after the check remains an unavoidable race; use a temporary
directory hierarchy that other users cannot modify when this local threat matters.

A symlink used to locate the Cargo project root is supported because cooldown canonicalizes that
root before coordinating access. A symlink inside a writable project path, such as a symlinked
workspace member whose manifest may be rewritten, is rejected because the project lease cannot
govern the resolved target safely.

Cargo mutations that publish manifests or a lockfile currently require a Git worktree on Unix.
Cooldown stores recovery authority beneath Git's common directory and verifies its Unix ownership
and permissions so ordinary project content cannot claim permission to restore source files.
The Git metadata directory and common directory must be owned by the current Unix user, so
repositories exposed through another user's or root-owned bind mount fail closed.
Read-only commands and isolated previews remain available elsewhere, but a source mutation fails
closed until cooldown can prove a trusted external recovery namespace on that platform.

## How each is driven

`cooldown` never treats a native package manager as the source of policy — the cooldown verdict is computed in one core evaluator. The native tool is used only to **resolve** a lockfile graph and to **apply** changes back to it. That is what keeps "adoptable" identical across ecosystems.

Publish times come from each registry's own metadata — GOPROXY `@v/<ver>.info` for Go, crates.io for Rust, PyPI / anaconda.org for Python, the npm registry and JSR for JavaScript, and GitHub Releases for SwiftPM. Adapters that mix registries (Deno's `npm:` + `jsr:`, conda + PyPI, pixi + PyPI) resolve each dependency against its own source.

## Registries and native cooldowns

Some ecosystems already ship a native cooldown — uv's `exclude-newer`, pnpm's `minimumReleaseAge`, yarn's `npmMinimalAgeGate`. Where one exists, [`cooldown sync`]({{< relref "../commands/other.md" >}}) (or the global `--sync` flag) writes the resolved policy *down* into that native config, so `cooldown.toml` stays the single source of truth and the native tool sees the same window you set once.

## Several ecosystems at once

In a polyglot repository every detected ecosystem runs in its own lane, and the lanes run concurrently: while `cargo` waits on crates.io, `uv` is already resolving against PyPI, whether they live in different directories or share one. A lane runs its own projects one after another, since concurrent invocations of one package manager block on that manager's own cache lock. Only ecosystems that rewrite the same manifest take turns under `upgrade`, `fix`, or `--lock`: uv and poetry both own `pyproject.toml`, and every npm-family tool owns `package.json`, so two of them at one root, or one nested in the other's workspace, run in turn (`--dry-run` mutates a throwaway copy, so it reads side by side too). Each adapter declares the file its lease guards: pixi is detected by `pixi.lock` but rewrites `pixi.toml`, or the `pyproject.toml` hosting its tables, and takes turns with uv and poetry in the latter case. `--build` steps run one at a time across ecosystems, since every ecosystem installs into the environment shared at a root (`node_modules`, `.venv`); so do the upgrades of tools whose native command installs as it pins (`poetry add`, `conda install`, `bundle update`, `mix deps.update`, `swift package update`), and `outdated`'s preview of such an update, which runs the same command on a copy. `--jobs <N>` caps how many ecosystems run at once; `--jobs 1` runs them one after another. The reports, their order, and the exit code are the same as a sequential run's. The interactive progress display shows one block per live ecosystem, and the plain transcript names the tool and project on every per-project line.

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
