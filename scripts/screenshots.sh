#!/usr/bin/env bash
# Regenerate the README screenshots from the checked-in examples/polyglot project.
#
# Each screenshot is produced by `freeze` (https://github.com/charmbracelet/freeze) from real
# `cooldown` output against fixed, deliberately-outdated lockfiles — so it is reproducible from the
# repo (no external checkout) even though the exact "latest" versions drift as registries evolve.
#
# `--color always` forces ANSI because freeze captures via a pipe, not a PTY; `--log-level error`
# silences the progress notes. We copy each tool's example into a throwaway, freshly `git init`ed
# dir so it is its own repo root and the project shows as "." (rather than a temp path).
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cooldown="$repo/target/release/cooldown"
docs="$repo/docs"
example="$repo/examples/polyglot"

command -v freeze >/dev/null || { echo "freeze not found — install charmbracelet/freeze" >&2; exit 1; }
[[ -x "$cooldown" ]] || cargo build --release --bin cooldown --manifest-path "$repo/Cargo.toml"
mkdir -p "$docs"

# freeze's window-chrome flags, shared by every shot.
frame=(--window --shadow.blur 20 --border.radius 8 --padding 20)

shoot() { # <tool-subdir> <output.png> <cooldown args...>
  local sub="$1" out="$2"; shift 2
  local work; work="$(mktemp -d)"
  trap 'rm -rf "$work"' RETURN
  cp -r "$example/$sub/." "$work/"
  git init -q "$work" # make it its own repo root → project renders as "." (not the temp path)
  ( cd "$work" && freeze --execute "$cooldown $* --color always --log-level error" \
      --output "$out" "${frame[@]}" )
  echo "wrote $out"
}

shoot cargo "$docs/outdated.png" outdated --tool cargo
shoot cargo "$docs/upgrade.png"  upgrade  --tool cargo --dry-run
