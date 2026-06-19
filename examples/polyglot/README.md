# polyglot example

Throwaway projects used to generate the README screenshots (`task screenshots`, see
`scripts/screenshots.sh`) and to exercise each adapter end to end. Every project pins deliberately
old dependency versions so `cooldown` always has updates to show; the exact "latest" versions drift
as registries evolve, but the demo stays valid and regenerates from this repo (no external checkout).

- `cargo/` — a standalone Cargo project (`Cargo.toml` + `Cargo.lock`; its `[workspace]` keeps it out
  of the parent workspace). Caret constraints with an old lock, so `upgrade` can move them.
- `go/`   — a Go module (`go.mod` + `go.sum`). `require`s old versions of a few well-known modules.
- `python/` — a uv project (`pyproject.toml` + `uv.lock`). Dependencies are `==`-pinned to old
  versions so the lock stays in sync (no stale-lock error); `outdated`/`check` work, but `upgrade`
  is a no-op since the pins are hard.

More Python tools (all PyPI / PEP 440, reusing the uv adapter's registry client):

- `pip/`    — a pinned `requirements.txt`.
- `poetry/` — `pyproject.toml` + `poetry.lock`.
- `conda/`  — `conda-lock.yml`, mixing conda-channel packages (anaconda.org) with `pip` ones.
- `pixi/`   — `pixi.lock`, likewise mixing conda and PyPI packages.

The JavaScript/TypeScript ecosystem ships one example per package manager — same set of old, pinned
npm dependencies, each recorded in that manager's own lockfile format (so each is its own `--tool`):

- `npm/`  — `package.json` + `package-lock.json` (lockfile v3).
- `pnpm/` — `package.json` + `pnpm-lock.yaml`.
- `yarn/` — `package.json` + `yarn.lock` (Yarn classic v1).
- `bun/`  — `package.json` + `bun.lock` (Bun's text lockfile).
- `deno/` — `deno.json` + `deno.lock`, mixing `npm:` dependencies (npm registry) with `jsr:` ones
  (the JSR registry); `cooldown` resolves each from the registry it belongs to.

And one example per remaining ecosystem:

- `ruby/`   — `Gemfile` + `Gemfile.lock` (Bundler, rubygems.org).
- `elixir/` — `mix.exs` + `mix.lock` (Hex, hex.pm).
- `maven/`  — `pom.xml` (Maven Central).
- `gradle/` — `build.gradle` + `gradle.lockfile` (Maven Central).
- `swift/`  — `Package.swift` + `Package.resolved` (SwiftPM; publish times via GitHub Releases).

Most of these lockfiles are hand-written to a real format (the toolchains aren't all installed here),
but `cooldown` reads committed lockfiles, so `outdated`/`check` work against the live registries. The
publish-time registries themselves are exercised end to end.

Try any of them (run from inside a tool's dir, or pass `--tool <name>` from here):

```bash
cooldown outdated --tool go
cooldown check --tool uv
cooldown outdated --tool pnpm
cooldown check --tool deno
cooldown outdated --tool maven
cooldown outdated --tool swift
```
