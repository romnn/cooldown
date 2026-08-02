# Changelog

## Unreleased

- **Breaking library API:** adapter parsing and lookup helpers now return named records instead of
  positional tuples. This affects Go path-version splitting; npm lock parsing and install-tree
  resolution; and the public Hex, Maven, pip, and RubyGems resolved-pin parsers. Callers should use
  the documented fields on `PathVersionSplit`, `NameVersion`, `ResolvedInstance`, `ResolvedDep`,
  and `ResolvedPin`.
- **Breaking library API:** `ToolWrite::recover_pending_mutation` now returns whether recovery
  changed project state, and `ToolWrite::ensure_no_pending_mutation` lets the same mutation-side
  adapter fail a shared read session without performing recovery. `ProjectMutationState` pairs a
  rollback journal with the exact post-apply state it may replace.
- Add a cargo `--cargo-edge-policy` for resolved lock **edge bindings** (`preserve` |
  `canonicalize` | `none`; config `[tool.cargo] edge-policy` — cargo-specific, so the key is
  tool-scoped and rejected under any other selector). An incremental re-resolve can silently
  rebind an edge between two coexisting versions a wide declared range admits (diesel's
  `uuid = ">=0.7, <2.0"` beside a `0.8` and a `1.x` line) — a build-affecting change that is
  invisible per-version and passes `cargo metadata --locked`. `preserve` (the default) restores
  such churn to the pre-upgrade binding; `canonicalize` binds each unambiguous crates.io edge to
  the highest satisfying locked version — preferring candidates whose declared `rust-version` is
  workspace-compatible and falling back to the highest satisfying one when none is, using a
  cooldown-owned conservative tier inspired by cargo's `incompatible-rust-versions = "fallback"`
  rule — healing pre-existing bad bindings too, also on a run with no version change to apply;
  `none` only observes. Every corrected, withheld,
  unaddressable, or surviving rebind is reported as its own
  `restored`/`canonicalized`/`held`/`unaddressable`/`rebound` row, preserving the dependent's full
  source-bearing lock identity. The `upgrade`/`fix` summary counts edge activity apart from
  version changes (`edgesCorrected`/`edgesRebound`/`edgesHeld`/`edgesUnaddressable`); row-level
  `applied` says whether the binding outcome is committed, and a `held` or `unaddressable` row
  fails `--strict`. Speculative lock corrections use atomically published, drift-checked recovery
  records, and outer manifest/lock rollback refuses to overwrite independent edits. The new
  `recover` command discovers recovery artifacts without loading policy, manifests, baselines, or
  registries, then restores a validated interrupted transaction without continuing into another
  mutation. Project reads and native-policy sync share the same resource leases. Config follows the
  per-project repository cascade, and the closed JSON contract is schema v4.

## v0.0.12

- Add tool-qualified package selectors such as `[tool.uv.package.glob]` and package-only
  `max-major` ceilings.
- Respect explicit manifest `<`/`<=` upper bounds during major upgrades; `--rewrite` is the
  deliberate escape hatch for crossing and rewriting them.
- Report declared-bound and `max-major` holds in terminal and JSON output.
- Reject `exclude-folders` and `exclude-packages` under package, registry, and project selectors.
  Earlier versions accepted these misplaced keys but silently ignored them; exclusion lists belong
  under `[tool.*]`, `[global]`, or a command table.
