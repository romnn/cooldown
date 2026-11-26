# polyglot example

Throwaway projects used to generate the README screenshots (`task screenshots`, see
`scripts/screenshots.sh`) and to exercise each adapter end to end. Every project pins deliberately
old dependency versions so `cooldown` always has updates to show; the exact "latest" versions drift
as registries evolve, but the demo stays valid and regenerates from this repo (no external checkout).
The screenshots are captured at a fixed instant via the debug-only `--now` flag, so they regenerate
identically. Run live (a release build, no `--now`), the day-counts have moved on, so versions that
were still cooling at the capture instant may have cleared their windows since.

- `cargo/` — a standalone Cargo project (`Cargo.toml` + `Cargo.lock` + `cooldown.toml`; its
  `[workspace]` keeps it out of the parent workspace). Tuned so one `outdated --countdown soonest` run
  lands on **every** status at once: a few old caret pins (**adoptable**); `hex` is an exact `=`
  pin (**held**); `once_cell` is pinned to its latest (**up-to-date**); `itertools` is waived by an
  `allow` rule (**exempt**); and two contrasting **in cooldown** rows — `bytes`, whose only newer
  release is still cooling, and `log`, pinned below a burst of releases so `--countdown soonest`
  counts down to the *next* version to unlock (an intermediate) rather than the newest. `lettre` is
  pinned below a security fix still inside its 60-day window: the
  [advisory feed](../../docs/content/docs/configuration/advisories.md)'s `shorten` mode makes the fix
  adoptable on a 1-day security window and names the advisory. Its lock was matured to the
  screenshots' fixed instant with `cooldown fix --now`, so `upgrade` moves only what it plans. This
  is the project the README screenshots are shot from; run live, the `log` and `lettre` rows clear
  their windows as time passes.
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
