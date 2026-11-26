#!/usr/bin/env bash
# Regenerate the README screenshots from the checked-in examples/polyglot project.
#
# Each screenshot is real `cooldown` output captured by `freeze`
# (https://github.com/charmbracelet/freeze) against the example's deliberately-arranged lockfile and
# `cooldown.toml`, so it is reproducible from the repo (no external checkout). The cargo example is
# tuned so one `outdated` run lands on every status — adoptable, in cooldown, exempt, held, and
# up-to-date — instead of a wall of "adoptable".
#
# These PNG's are for the README only: GitHub markdown can't render the docs site's live HTML
# terminals, so it needs static images. The docs *site* renders the same `cooldown` output as HTML
# via scripts/terminals.sh (same inputs, different renderer), so the docs build does not depend on
# this script — run it by hand (`task docs:screenshots`) to refresh the README shots.
#
# Four flags keep the capture faithful and reproducible:
#   * `--now <date>` pins cooldown's evaluation clock to a fixed instant (a debug-build-only flag), so
#     the `age/window` countdowns and the in-cooldown row don't drift as the wall clock and the
#     registry move on. That is why we build and run the DEBUG binary, not `--release`.
#   * `--color always` forces ANSI, since the capture is a pipe, not a terminal.
#     `--log-level error` silences the diagnostic log.
#   * `--no-progress` suppresses the live progress display so freeze captures only the final report,
#     not the intermediate per-dependency fetch frames (which would render as a very tall image).
#   * `--language text` stops freeze from syntax-highlighting the captured output — otherwise it
#     recolors version numbers and identifiers, fighting cooldown's own ANSI colors.
#
# Two more capture quirks are worked around per shot:
#   * freeze ignores the `\e[39m` "reset foreground to default" code, so a colored cell's color bleeds
#     into the following uncolored cells (e.g. the magenta "Adoptable" leaking into "Latest" and
#     "Cooldown"). Rewriting `\e[39m` to a full reset `\e[0m`, which freeze does honor, stops it.
#   * freeze would replay an `--execute` command in an 80-column PTY and wrap the tables, so the
#     output is captured first and freeze renders the capture file instead.
# Each tool's example is copied into a throwaway, freshly `git init`ed dir so it is its own repo root
# and the project renders as "." (rather than a temp path).
#
# Every shot is rendered twice, so the README can follow the reader's color scheme through
# `<picture>`: `<name>.png` on freeze's dark window, and `<name>-light.png` on white. freeze draws
# cooldown's 256-color codes from a fixed palette meant for a dark background, whose bright yellow
# and cyan wash out on white, so the light pass rewrites each code cooldown uses into a darker
# truecolor one first.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cooldown="$repo/target/debug/cooldown"
docs="$repo/docs/static/images"
example="$repo/examples/polyglot"

# The fixed "as-of" instant the screenshots are evaluated at. Bump it (and re-pick the example pins in
# Cargo.lock / cooldown.toml if a registry has moved on) when regenerating against newer data.
now="2026-06-22"

command -v freeze >/dev/null || { echo "freeze not found — install charmbracelet/freeze" >&2; exit 1; }
# Always (re)build rather than `[[ -x ]] ||`: the debug binary is what every dev/test run produces,
# so a guard could pick up a stale one built before `--now` existed and the per-shot `--now` would
# error. A no-op `cargo build` when already current is cheap.
cargo build --bin cooldown --manifest-path "$repo/Cargo.toml"
mkdir -p "$docs"

# freeze's window-chrome flags, shared by every shot.
frame=(--window --shadow.blur 20 --border.radius 8 --padding 20)

# The light pass's palette: each color cooldown emits, as a truecolor that reads on white. The tables
# use 256-color indices; the tip card uses the basic cyan (36) and green (32).
light_palette='my %rgb = (2 => "26;127;55", 8 => "101;109;118", 9 => "207;34;46", 10 => "26;127;55", 11 => "154;103;0", 13 => "130;80;223", 14 => "9;105;218", 240 => "208;215;222", 32 => "26;127;55", 36 => "9;105;218"); s/38;5;(\d+)/exists $rgb{$1} ? "38;2;$rgb{$1}" : "38;5;$1"/ge; s/\e\[(1;)?(3[26])m/"\e[" . ($1 \/\/ "") . "38;2;$rgb{$2}m"/ge;'

shoot() { # <tool-subdir> <name> <cooldown args...>
  local sub="$1" name="$2"; shift 2
  local work; work="$(mktemp -d)"
  trap 'rm -rf "$work"' RETURN
  cp -r "$example/$sub/." "$work/"
  git init -q "$work" # make it its own repo root → project renders as "." (not the temp path)
  # A pipe is not a terminal, so cooldown renders its tables at full width. Like
  # scripts/terminals.sh, a shot must exit 0 and carry no `error [...]` diagnostic, which `outdated`
  # reports for a stale lock while still exiting 0.
  ( cd "$work" && COOLDOWN_NO_TRACE=true "$cooldown" "$@" --now "$now" --color always \
      --no-progress --log-level error ) | perl -pe 's/\e\[39m/\e[0m/g' > "$work/dark.ansi"
  if grep -q '^error \[' < <(perl -pe 's/\e\[[0-9;]*m//g' "$work/dark.ansi"); then
    cat "$work/dark.ansi" >&2
    echo "cooldown $* reported an error; not writing $docs/$name.png" >&2
    exit 1
  fi
  perl -pe "$light_palette" "$work/dark.ansi" > "$work/light.ansi"
  # freeze renders the captured ANSI from the file; it waits on stdin unless it has none.
  freeze --language text --output "$docs/$name.png" "${frame[@]}" "$work/dark.ansi" </dev/null
  freeze --language text --output "$docs/$name-light.png" "${frame[@]}" \
    --background '#ffffff' --theme github "$work/light.ansi" </dev/null
  echo "wrote $docs/$name.png and $docs/$name-light.png"
}

# `--countdown soonest` so the `log` row counts down to the next version to unlock (an intermediate),
# showing off the soonest horizon; it is a no-op for the single-candidate rows.
shoot cargo outdated outdated --tool cargo --countdown soonest
shoot cargo upgrade  upgrade  --tool cargo --dry-run
