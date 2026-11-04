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
| Yarn (Classic v1) | `yarn` | npm registry | `yarn.lock` |
| Yarn (Berry v2+, read-only) | `yarn` | npm registry | `yarn.lock` |
| Bun | `bun` | npm registry | `bun.lock` |
| Deno | `deno` | npm + JSR | `deno.json` / `deno.lock` |
| Ruby | `bundler` | rubygems.org | `Gemfile` / `Gemfile.lock` |
| Elixir | `hex` | hex.pm | `mix.exs` / `mix.lock` |
| Java (Maven) | `maven` | Maven Central | `pom.xml` |
| Java (Gradle) | `gradle` | Maven Central | `gradle.lockfile` |
| Swift | `swift` | GitHub Releases | `Package.resolved` |

Yarn Classic and Yarn Berry share the `yarn` tool, its `[tool.yarn]` config, and `yarn.lock`;
cooldown tells them apart by the lock itself, since only a Berry lock opens with a `__metadata`
entry. A Berry lock written by Yarn 2 through 4.18 (lockfile versions 4 to 6 and 8 to 10) is read
for every npm release it resolves, so `outdated` and `explain` cover it. An npm alias is read as
its target's release, a patched package's own npm release as usual, and workspaces and `link:`,
`portal:`, `file:`, `patch:`, `exec:`, git, and tarball URL entries are no registry releases and
are left out. A lock that is malformed or of another lockfile version is reported
`lockfile_unreadable`.

A Berry lock records every workspace Yarn installed, each with the dependencies its manifest
declared, so cooldown takes the members from the lock and credits each one the registry releases
its own declarations bind: the lock entry a member's current declaration of a name resolves to,
through an `npm:` range, a same-name `npm:` alias, or Yarn's builtin `typescript`, `fsevents`, and
`resolve` patches. A row is direct exactly when some member owns it, and `exclude-folders` drops
the rows only an excluded member owns. An alias of another package, a `workspace:` range, and a
`file:`, `link:`, `portal:`, `exec:`, git, or tarball source bind no registry release of the name
they are declared under. A declaration whose binding cooldown cannot prove leaves its name
unattributed: every locked copy of that name is reported as a direct dependency of no member, none
is landed, an exact pin of it holds no copy, and the run warns naming each such declaration and
why. That covers a name the root `resolutions` can rewrite, a `catalog:` or `patch:` reference, a
name declared at different ranges across one manifest's install fields, a declaration the lock
records under another range or not at all, a member the lock does not list, and, in a lock of
version 6 or older, which records ranges without their protocol, a bare range when the project's
`defaultProtocol` is set to anything but `npm:` in a `.yarnrc.yml` Yarn reads or in
`YARN_DEFAULT_PROTOCOL`. A Yarn plugin, configured in any `.yarnrc.yml` Yarn reads or through
`YARN_PLUGINS`, can rewrite any declaration, so no member is attributed at all and the run warns.

The attribution reads the lock through the manifests and Yarn configuration as they are now, not
as they were at the last install. `check` verifies the lock first and evaluates only a current
one; `outdated` and `explain` read the lock as written, so a lock that is stale against its
inputs, such as a `.yarnrc.yml` or a `resolutions` entry changed since the last install, can be
misattributed there.

`check` asks Yarn itself whether a Berry lock is current, with Yarn 2.4 through 4 (the `yarn` that
runs in the project, through `yarnPath` or Corepack). It runs `yarn install --immutable` without
builds in a throwaway copy of every input Yarn reads (the lock, every `package.json` outside
`node_modules`, `.git`, and `.yarn`, the `.yarnrc.yml` files from the project up to the home
directory, `.env.yarn`, `.yarn/releases`, `.yarn/patches`, `file:` folders whole with their
permissions, and the manifests of `portal:` and `link:` targets) and never in the project, which
stays byte for byte as it was. The run is offline for Yarn, Corepack, and git, and every folder
Yarn writes is redirected into the copy. A lock is current only when Yarn reaches its fetch step,
and stale only when it reports `YN0028` (the lockfile would be modified); a stale lock is an error
that `--allow-stale-lock` waives. Anything else is `lock_unknown`, an error nothing waives: a lock
whose change needs registry metadata the offline run cannot fetch, a configuration the copy
cannot reproduce faithfully (a configured plugin, an `exec:` dependency, a workspace pattern with
`..` or an absolute path, a local package named by an absolute path, through a symbolic link, or
outside the home directory, a symbolic link whose copy would resolve outside the copy, injected
environment files other than `.env.yarn`, a lockfile name other than `yarn.lock`,
`networkSettings` or an enabled `enableNetwork`, hardened mode set to anything but off, an output
folder outside the copy, or an rc file using Yarn's `onConflict` form), an input that changes
while the copy is written, or a Yarn without a supported version.
Should any input change while `check` reads the dependencies after the verdict, the run reports
`lock_conflict` instead of evaluating a graph Yarn never verified. A change undone before that
second read goes unnoticed, the same window pnpm's verify-then-read check leaves.

Not yet supported for Berry:

- every mutation: `upgrade`, `fix`, and `--build` refuse a Berry project before running Yarn, and
  `outdated` warns that its adoptable rows were not verified against `upgrade`.

For a Berry project run by Yarn 4.10 or later, [`sync`]({{< relref "../commands/other.md" >}}#yarn-berry)
writes the native age gate, `npmMinimalAgeGate` and `npmPreapprovedPackages`, into its
`.yarnrc.yml`; a Yarn Classic project has no native age gate.

Before running Yarn on a Classic lock, cooldown checks that the Yarn the project declares
(`packageManager`) and the one `yarn --version` reports there, through `yarnPath` or Corepack, are
Yarn 1, and otherwise refuses rather than let a Berry Yarn migrate the lock. It runs Yarn with
Corepack's auto-pin off (`COREPACK_ENABLE_AUTO_PIN=0`), so the check never writes a
`packageManager` field into the root `package.json`.

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
