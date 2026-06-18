# polyglot example

Throwaway projects used only to generate the README screenshots, via `task screenshots`
(see `scripts/screenshots.sh`). Each project's lockfile pins deliberately-old dependency versions
so `cooldown` always has updates to show; the exact "latest" versions drift as registries evolve,
but the demo stays valid and regenerates from this repo (no external checkout).

- `cargo/` — a standalone Cargo project (its `[workspace]` keeps it out of the parent workspace).

Adding `go/` (`go.mod` + `go.sum`) and `python/` (`pyproject.toml` + `uv.lock`) follows the same
recipe: pin old versions, commit the lockfile, then `task screenshots`.
