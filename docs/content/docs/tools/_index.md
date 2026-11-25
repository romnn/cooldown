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
| Yarn (Berry v2+) | `yarn` | npm registry | `yarn.lock` |
| Bun | `bun` | npm registry | `bun.lock` |
| Deno | `deno` | npm + JSR | `deno.json` / `deno.lock` |
| Ruby | `bundler` | rubygems.org | `Gemfile` / `Gemfile.lock` |
| Elixir | `hex` | hex.pm | `mix.exs` / `mix.lock` |
| Java (Maven) | `maven` | Maven Central | `pom.xml` |
| Java (Gradle) | `gradle` | Maven Central | `gradle.lockfile` |
| Swift | `swift` | GitHub Releases | `Package.resolved` |

npm's `package-lock.json` is read in `lockfileVersion` 2 or 3, which npm 7 and newer write. A
`lockfileVersion` 1 lock, written by npm 6 and older, records its nested copies only inside a
`dependencies` tree, so it is reported `lockfile_unreadable` with a message to re-lock with npm 7
or newer rather than read in part.

`check` asks npm itself whether the lock is current, with npm 10.5.1 or newer, since older
releases remove `node_modules` despite `--dry-run`: an older npm leaves the lock `lock_unknown`.
It runs `npm ci --dry-run --ignore-scripts` in the project, which installs nothing, with npm's
cache and logs kept in cooldown's cache directory, so the project, its `node_modules` included,
stays byte for byte as it was. npm's refusal of a lock out of sync with `package.json`, or of a
missing lock, is a stale lock that `--allow-stale-lock` waives, and any other npm failure is
`tool_failed`. Under `--offline` npm runs offline too: it still proves a current lock from the lock
alone, but a stale lock whose resolution needs registry metadata npm has not cached is
`tool_failed` rather than stale. npm compares versions only, so the lock is `lock_unknown` rather
than current wherever its check does not reach:

- an `npm-shrinkwrap.json` beside the lock, which npm verifies instead;
- npm resolving the project into an enclosing workspace, or a configured `workspace` filter (in an
  `.npmrc` or `npm_config_workspace`), which narrows `npm ci` to some members;
- a declaration of the root, a workspace member, or a `file:` package that differs from what the
  lock recorded for it, such as a registry range changed to a tarball URL of the same version;
- an `overrides` entry that is neither a version range nor a `$` reference to a root declaration
  that is itself a range, such as an `npm:` alias, a URL, a git source, or a local path, since the
  lock records no override.

`upgrade` and `fix` verify the lock they wrote the same way. npm has no lock-only refresh for
`check --lock`; re-lock with `npm install`.

Bun's text `bun.lock` is the lock cooldown reads; a project with only the binary `bun.lockb` is no
Bun project to it. `check` asks Bun itself whether the lock is current, with a stable Bun 1.4.5 or
newer, the oldest release measured to leave the project alone: an older or prerelease Bun leaves
the lock `lock_unknown`. It runs `bun install --frozen-lockfile --dry-run --ignore-scripts` in the
project, which installs nothing, with Bun's install cache and temporary files kept in cooldown's
cache directory and `BUN_OPTIONS` ignored, so the project, its `node_modules` included, stays byte
for byte as it was. Bun's refusal of a lock that has changes is a stale lock that
`--allow-stale-lock` waives, but only when it is Bun's sole error; any other Bun failure, such as a
registry or resolution error or a lock Bun could not load, is `tool_failed`. Under `--offline` Bun
runs offline too: it still proves a current lock from the lock alone, but a stale lock whose
resolution needs registry metadata Bun has not cached is `tool_failed` rather than stale. The lock
is `lock_unknown` rather than current wherever the probe cannot establish what Bun verifies:

- no readable text `bun.lock`;
- a configuration that disables Bun's install cache, under which Bun writes into the project:
  `[install] cache = false` or `[install.cache] disable = true` in the project's `bunfig.toml` or
  the global `.bunfig.toml`, or any line in the project's or the user's `.npmrc` that may set
  `cache` other than a plain `cache`, `cache=true`, or `cache=<path>`, such as
  `cache=false # comment`, `cache = ' false '`, or `cache=${VAR}`, and a
  `.env` file Bun loads (`.env`, `.env.local`, `.env.<NODE_ENV>`, `.env.<NODE_ENV>.local`) that
  mentions `NPM_CONFIG_USERCONFIG`, which chooses the user `.npmrc`;
- an enclosing directory whose `package.json` declares workspaces, which Bun could run the project
  as a member of;
- `trustedDependencies` in any workspace manifest or the lock, or `patchedDependencies` in the
  root manifest or the lock, since a frozen install compares neither, or a local `file:` or
  `link:` dependency in the lock, whose own manifest Bun does not compare;
- a workspace pattern other than a literal path or a path ending in `/*`;
- workspace members other than those the lock records, such as a new member named like a locked
  registry package, or a member's name, version, or declarations differing from what the lock
  recorded for it, which Bun's frozen check passes.

`upgrade` and `fix` verify the lock they wrote the same way.

`check` asks Poetry itself whether a `poetry.lock` is current, with a stable Poetry 2.3 or newer,
the first release whose content hash covers PEP 735 `[dependency-groups]`: an older or prerelease
Poetry leaves the lock `lock_unknown`. It runs `poetry check --lock --no-plugins` in the project,
with the `poetry` that `upgrade` runs (`COOLDOWN_POETRY` or `PATH`), Poetry's cache directory set
to one in cooldown's cache directory, and Python writing no bytecode. Poetry's check reads only
`pyproject.toml` and the lock, so the probe needs no network, creates no virtualenv, and leaves the
project byte for byte as it was. Poetry's warnings, such as one about deprecated `[tool.poetry]`
metadata, do not fail the check. Poetry's refusal of a lock whose content hash no longer matches
`pyproject.toml`, or of a missing lock, is a stale lock that `--allow-stale-lock` waives, but only
when it is Poetry's sole error and everything else Poetry prints is a warning; any other Poetry
failure, such as an invalid configuration or a missing README, is `tool_failed`. Poetry's content
hash covers the declarations, not what the lock resolved them to, so the lock is `lock_unknown`
rather than current wherever the probe cannot establish that the declarations decide the
resolution and that cooldown judges what the lock resolved:

- no `pyproject.toml` at the project root, where Poetry would check an enclosing project instead;
- `pyproject.toml` or the lock changing while Poetry checks them;
- a `lock-version` that is missing or outside the formats Poetry's installer reads, a stable
  version from 1 up to but excluding 3, which Poetry's check passes but its installer refuses;
- a lock that lists no packages while `pyproject.toml` declares some;
- a URL, git, or path origin, including a `develop = true` path, in a declaration or in the lock's
  dependencies, whose content can change while its declaration stays the same;
- a locked package from any source other than PyPI, or a `[[tool.poetry.source]]` in
  `pyproject.toml`, since cooldown judges each package's age by its PyPI release;
- `project.dynamic` in `pyproject.toml`, which changes how Poetry combines the `[project]` and
  `[tool.poetry]` declarations;
- a plugin Poetry would load, required in `[tool.poetry.requires-plugins]` or installed in the
  project's `.poetry/plugins` directory;
- a lock whose content hash may not cover `[dependency-groups]`, which newer Poetry compares an
  older Poetry's lock without: any lock written by Poetry 2.2, one written by an older Poetry while
  `pyproject.toml` declares dependency groups, and one without a readable generator comment unless
  its `lock-version` predates 2.1 and no dependency group is declared.

A hand-edited version of a registry package in the lock still passes, since Poetry compares no
locked version, and cooldown evaluates the version the lock records. Every package
`pyproject.toml` declares is a direct dependency, whether in `[project]`, an extra, a PEP 735
dependency group, or a `[tool.poetry]` main, group, or legacy development table. `upgrade` and
`fix` move a package with `poetry add`, which writes to the main dependencies, so they hold any
package that `project.dependencies` and `[tool.poetry.dependencies]` do not declare as
`not_eligible` rather than add it there. A
`requirements.txt` has no native notion of being current, so a pip project's lock stays
`lock_unknown`.

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
folder outside the copy, or an rc file using Yarn's `onConflict` form at its top level or for one
of those settings, `packageExtensions`, or `yarnPath`), an input that changes
while the copy is written, or a Yarn without a supported version.
Should any input change while `check` reads the dependencies after the verdict, the run reports
`lock_conflict` instead of evaluating a graph Yarn never verified. A change undone before that
second read goes unnoticed, the same window pnpm's verify-then-read check leaves.

`upgrade` and `fix` land each candidate of a Berry project run by Yarn 4 at exactly its target.
The run first has Yarn prove the lock current, as `check` does, and refuses a stale lock or one
Yarn cannot judge before changing anything. Each candidate then resolves in a throwaway copy of the
project in two lock-only installs (`yarn install --mode=update-lockfile`): the first with every
declaration of the package pinned to the target, the second with the declarations restored and
the lock binding each declared range to the target's entry, which Yarn would otherwise keep bound
to the old copy or resolve afresh to the range's newest release. Yarn then proves the settled lock
current in the copy, and only the lock is written back, once every input the copy holds is
unchanged in the project; manifests, install state, and caches stay as they were. A candidate
that fails is skipped `resolver_conflict` with Yarn's error and leaves the lock as it was, and
`--build` runs `yarn install --immutable` in the project once the run has landed.

A Yarn with the native age gate (4.10 or later) resolves each landing under an approval of
exactly the target, beside the approvals Yarn's configuration already holds, and under cooldown's
window when that is stricter than Yarn's own gate, so a new transitive the gate quarantines fails
the candidate. Yarn 4.17 and later gate each configured `npmScopes` entry by that scope's own gate
instead, so a landing that newly resolves another release in a scope gated below the window fails
too. The installs keep Yarn's own network settings and global folder, whose metadata cache and
mirror serve an `--offline` landing; a global folder inside the project, or inside any directory
holding a `package.json`, is replaced by one in the copy.

A candidate is held `not_eligible`, naming the declaration, when one of its declarations is no
registry range admitting the target (an `npm:` alias, a `patch:`, or a local source), when
a member declares it at several ranges, or when binding the declared ranges would move a copy the
candidate does not own: a range the lock binds to another copy, or one a member excluded from the
run declares too. `upgrade` and `fix` refuse a Berry project run by any Yarn but Yarn 4, which
the other commands still read. A `--dry-run` copy recreates in-tree symbolic links as absolute links and drops
directory permissions, so a project relying on them reports the preview as an error and writes
nothing, while the real run lands.

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

### Which binary runs

cooldown runs each package manager as the program its name finds on `PATH`.
`COOLDOWN_<BIN>` names a different program, with the binary's name in upper case and `-` spelled
`_`: `COOLDOWN_CARGO`, `COOLDOWN_GO`, `COOLDOWN_UV`, `COOLDOWN_PNPM`.

When `PATH` leads to a [mise](https://mise.jdx.dev) shim, every command first runs mise, which picks
the tool and its environment for the command's directory.
That costs tens of milliseconds per call, and a cargo `upgrade` makes hundreds of calls.
cooldown still goes through the shim, because the shim also applies the environment, wrappers and
installs from the project's `mise.toml`, and a direct call would skip them.

A tool that mise does not manage reaches its real binary through the shim's fallback anyway, so
naming that binary directly loses nothing and skips the overhead.
For a rustup-managed cargo, set `COOLDOWN_CARGO="$HOME/.cargo/bin/cargo"`, or put `~/.cargo/bin`
ahead of mise's shims on `PATH`.
An override names a program and nothing else: it applies no mise environment, so use it only for a
tool whose mise configuration you do not need.

### When cargo crashes

Cargo has been seen to crash with a signal, such as a segmentation fault, right after printing an
ordinary resolver conflict that the same command otherwise reports normally. A cargo read that a
signal kills runs once more: a `--locked` read always, and a resolve in cooldown's private staged
copy while that copy is still the one cooldown prepared and its `Cargo.lock` still holds the bytes it
had, or is still absent. Nothing is restored, so a resolve that changed the lock before it crashed is
not run again. The second run's result
stands; only a second crash fails the project, and its error says cargo was killed by the signal
after printing its output, instead of presenting that output as the cause.

## Registries and native cooldowns

Some ecosystems already ship a native cooldown — uv's `exclude-newer`, pnpm's `minimumReleaseAge`, yarn's `npmMinimalAgeGate`, Cargo's `min-publish-age`. Where one exists, [`cooldown sync`]({{< relref "../commands/other.md" >}}) (or the global `--sync` flag) writes the resolved policy *down* into that native config, so `cooldown.toml` stays the single source of truth and the native tool sees the same window you set once.

### Cargo's own publish age

Cargo 1.100 and newer can skip crates.io releases younger than a configured age.
cooldown reads that age as the project's native cooldown, the way Cargo itself resolves it for a
command run in the project:

- `registries.crates-io.min-publish-age` wins over `registry.global-min-publish-age`, whichever
  file or variable sets them.
- The nearest `.cargo/config` (or `.cargo/config.toml` when there is no `config`) in the project's
  ancestors wins, and the Cargo home's file ranks last.
- `CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE` overrides a file's global age.
- `resolver.incompatible-publish-age = "allow"` (or `CARGO_RESOLVER_INCOMPATIBLE_PUBLISH_AGE=allow`)
  switches Cargo's enforcement off, so cooldown reads no age either.

A `min-age` under `[package.metadata.cooldown]` or `[workspace.metadata.cooldown]` in `Cargo.toml`
speaks for cooldown directly and wins over Cargo's age.
Like every native window, it ranks below `cooldown.toml` and the command line; see
[`--fail-on-stricter-native`]({{< relref "../commands/check.md" >}}) to fail when a policy overrides
a stricter one.
An age set in a file brought in through Cargo's `include` is not seen.

cooldown applies the window itself and runs its own cargo commands without Cargo's crates.io age, so
a stricter native age never makes Cargo refuse a version the policy admits.
Ages for other registries are left to Cargo.
To give cooldown's own cargo commands a crates.io age anyway, set `COOLDOWN_CARGO_MIN_PUBLISH_AGE`
in Cargo's spelling (`"14 days"`); it bounds what Cargo can resolve but is not read as policy.

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
