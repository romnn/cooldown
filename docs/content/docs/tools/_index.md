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

A cargo-hakari workspace-hack — a member whose `[dependencies]` are generated from the lock —
should be declared in [`[tool.cargo] generated-members`]({{< relref "../configuration/selectors.md" >}}#toolcargo-generated-members);
its entries then follow every upgrade instead of being proposed (and reported `blocked`) as
upgrades of their own. cooldown hints when an undeclared member carries the hakari marker.

Cargo projects are detected by `Cargo.toml`. A nested manifest is a project of its own unless the
resolve of a project the run evaluates already covers it — only a project that is actually
evaluated can cover anything, so a workspace that was pruned, ignored, or simply never scanned
covers nothing.

A workspace resolves its members: the directories its `members` globs match (`crates/*` matches
`crates/x`, not `crates/x/y`, as in cargo, and `./a` and `a/` mean the directory `a`), the root
package itself when the root manifest declares `[package]`, and the path dependencies those pull in
— which cargo makes members too, whether they lie inside the workspace directory or outside it with
their own `package.workspace` pointing back at this root — minus whatever `exclude` names, unless
`members` names it as well. A member's `dep = { workspace = true }` resolves through the root's
`[workspace.dependencies]`, so the crate that entry's `path` names is a member's dependency like any
other. Beyond the members, the resolve follows every `path` dependency they reach, through
`[dependencies]`, `[build-dependencies]`, and the `[target.<cfg>.…]` forms. A plain `[package]` with
no `[workspace]` table is the single member of its own implicit workspace and resolves the same
closure from there.

Two kinds of edge stop at the members. `[dev-dependencies]` count for members only, because cargo
locks its members with dev units enabled but activates everything beyond them as an ordinary
dependency, with dev-dependencies off — so a crate reachable only as a reached package's dev path
dependency is in nobody's lock and is a project of its own. cooldown treats an `optional` path
dependency of a reached package the same way, and that one is cooldown's own rule rather than
cargo's: the edge is not followed whether or not a feature enables it, because which features are
enabled is known only to the resolve. A crate reached only that way is therefore gated on its own
lock — it reports a missing one rather than passing silently — or dropped with
[`exclude-folders`]({{< relref "../configuration/excludes.md" >}}).

A known limit runs the other way: a path dependency reached as a non-member has its own
`[dev-dependencies]` in nobody's lock either, and cooldown gates the crate through the project that
resolves it rather than gating what only its standalone test build would fetch.

Two shapes are neither covered nor projects. A manifest declaring `[workspace]` is always a project
— cargo forbids a workspace root from being another workspace's member. And a manifest an
enclosing `[workspace]` neither lists in `members` nor `exclude`s stays with that workspace: cargo
refuses to build such a crate on its own ("current package believes it's in a workspace when it's
not"), so nothing resolves it and nothing can, which is exactly where fixture and template crates
commonly sit. A `package.workspace` pointer is the exception — it asked to be a member of the root
it names, so if that root does not list it, it is a project of its own rather than a silent one.
Below a detected Cargo project, a vendored crate (`cargo vendor` output, marked by
`.cargo-checksum.json`) and `cargo package`'s unpacked output under `target/package` are never
projects: cargo loads neither as a package of the surrounding build.

`-C`/`--dir` follows the same rule: pointing the run at a directory another project resolves runs
*that* project, wherever its root sits — a workspace may list a member that is not below it — and
scopes the report to what the selected directory declares.

A detected project without a `Cargo.lock` is a `stale_lock` error naming it: its dependencies would
otherwise resolve to whatever is newest at build time, which is exactly what the gate exists to
prevent. Generate and commit the lock (`cargo generate-lockfile`; `check --lock` and `outdated
--lock` do it in place), then run `cooldown fix` to mature the fresh resolve. `--allow-stale-lock`
downgrades the failure to a warning and skips the project on `check`, `outdated`, `upgrade`, and
`fix` (`baseline` still fails on it), so a clean summary then covers one project fewer;
[`exclude-folders`]({{< relref "../configuration/excludes.md" >}}) drops a crate that is never
built.

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
