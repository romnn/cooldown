#!/usr/bin/env bash
# Regenerate the documentation terminal snippets under docs/assets/terminals/.
#
# Each snippet is the real, colored output of `cooldown` against the committed
# examples/polyglot/cargo workspace, converted from ANSI to HTML with
# `terminal-to-html` (https://github.com/buildkite/terminal-to-html), which mise
# provides via its github backend. This mirrors scripts/screenshots.sh, which
# produces the PNG screenshots with `freeze` from the same workspace — same
# inputs, different renderer.
#
# The snippets are committed, so the Hugo site builds without terminal-to-html;
# run this only to regenerate them. The `terminal` shortcode inlines each snippet.
#
# Reproducibility mirrors scripts/screenshots.sh:
#   * `--now <date>` pins cooldown's evaluation clock to a fixed instant (a
#     debug-build-only flag) so the age/window countdowns don't drift as the wall
#     clock and the registry move on — which is why we build and run the DEBUG
#     binary, not `--release`. The example's Cargo.lock and cooldown.toml are
#     tuned so one `outdated` run lands on every status.
#   * `--color always` forces ANSI even though the capture is a pipe, not a PTY.
#   * `--no-progress` drops the human-facing progress display (which degrades to
#     plain lines on a non-TTY stderr) so only the final report is captured.
# Each shot runs in a throwaway, freshly `git init`ed copy of the example so it is
# its own repo root and the project renders as `.` rather than a temp path.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cooldown="$repo/target/debug/cooldown"
examples="$repo/examples/polyglot"
out="$repo/docs/assets/terminals"

# The fixed "as-of" instant the snippets are evaluated at — kept in lockstep with
# scripts/screenshots.sh. Bump both (and re-pick the example pins in
# Cargo.lock / cooldown.toml if a registry has moved on) when regenerating.
now="2026-06-22"

command -v terminal-to-html >/dev/null || {
  echo "terminal-to-html not found — run 'mise install' (provided via the github backend)" >&2
  exit 1
}
# Always (re)build the debug binary: it is what every dev/test run produces, so a
# guard could pick up a stale one built before `--now` existed and the per-shot
# `--now` would error. A no-op `cargo build` when already current is cheap.
cargo build --bin cooldown --manifest-path "$repo/Cargo.toml"
mkdir -p "$out"

# Render one snippet. Usage: shoot <name> <example-subdir> <cooldown args...>
#
# The throwaway dir's absolute path (`$work`) leaks into `explain`/`config`, which
# print the cooldown.toml's location — rewrite it back to `.` so the committed
# snippet is deterministic (the random mktemp name would otherwise churn every run).
shoot() {
  local name="$1" sub="$2"; shift 2
  local work; work="$(mktemp -d)"
  cp -r "$examples/$sub/." "$work/"
  git init -q "$work" # its own repo root → project renders as "." (not the temp path)
  {
    printf '\033[1;32m$\033[0m cooldown %s\n\n' "$*"
    ( cd "$work" && "$cooldown" "$@" --now "$now" --color always --no-progress 2>/dev/null || true )
  } | sed "s#$work#.#g" | terminal-to-html > "$out/$name.html"
  rm -rf "$work"
  echo "wrote $out/$name.html"
}

# The cargo example is tuned so one `outdated` run lands on every status —
# adoptable, in cooldown, exempt, held, and up-to-date. `--countdown soonest`
# makes the `log` row count down to the next intermediate version to unlock.
# This is also the landing-page hero.
shoot outdated cargo outdated --tool cargo --countdown soonest

# The upgrade plan (dry run) — cross-major bumps included via --major.
shoot upgrade cargo upgrade --tool cargo --major --dry-run

# The CI gate over the resolved graph. The example lock is intentionally stale (it
# is arranged for `outdated`), so `--allow-stale-lock` demotes that to a warning
# and lets the gate evaluate and pass over the resolved graph.
shoot check cargo check --tool cargo --allow-stale-lock

# Why a package has the window it has — every layer and rule that applied.
# `itertools` is exempt via `allow`, so its derivation shows the exemption zeroing
# the window on top of the default layers.
shoot explain cargo explain itertools --tool cargo

# The fully-resolved config, with the origin of each value.
shoot config cargo config --tool cargo
