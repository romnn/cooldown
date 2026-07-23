---
title: upgrade
weight: 2
---

# `upgrade`

`upgrade` moves dependencies **forward** to the newest version that has already matured past its cooldown, then re-locks. It never adopts a too-fresh version — only versions that have cleared the window are proposed.

```bash
cooldown upgrade
```

Preview the plan without touching anything with `--dry-run`:

{{< terminal name="upgrade" >}}

The **From → To** columns show the move; a row can be a `downgraded` as well as an `upgraded` when a re-lock would otherwise pull a too-fresh transitive in and it has to be reconciled back down (see [Transitive dependencies](#transitive-dependencies)).

## Lock versus manifest

By default `upgrade` moves the **lock** within your declared version constraint and leaves the manifest alone: a `^1.4` stays `^1.4` while the lock advances to the newest matured `1.x`.

When the target falls **outside** the constraint — most commonly a cross-major bump (`--major`) past a caret range, or a capped Python range like `>=1,<2` — `cooldown` rewrites the one owning manifest entry so the version can be adopted at all, then re-locks. Edits are **format-preserving** (comments, key order, and spacing are kept), and for a Cargo workspace an inherited `dep = { workspace = true }` is widened in the root `[workspace.dependencies]`.

Pass `--rewrite` to always rewrite the declared constraint to the adopted version, even for an in-range move (so `^1.4` becomes `^1.5`).

The lock-only default is honored where the tool can pin an exact in-range version without editing the manifest:

| Tool | In-range pin | Behavior |
|---|---|---|
| cargo | `update --precise` | lock-only |
| uv | `lock --upgrade-package` | lock-only |
| pnpm | `update --no-save` | lock-only |
| npm, yarn, bun | *(no such command)* | always rewrites the manifest |
| Go | `go.mod` *is* the version source | always rewrites the manifest |

## Transitive dependencies

By default `upgrade` moves the **whole graph**: it advances each dependency to its newest matured version, and reconciles any too-fresh transitive a re-lock drags in back down, so the new lock is **gate-clean by construction** — a subsequent `check` won't reject it. `--transitive` relaxes this:

- **`--transitive hide`** — direct-only: leave transitive dependencies untouched.
- **`--transitive allow`** — still advance the graph, but leave a floated-up too-fresh transitive in place (reported, not rolled back).

## Major versions

A cross-major bump is usually breaking work you opt into, so `--major` is **off** by default for `upgrade`. With `--major` it applies to every eligible dependency; narrow it to a subset with `--package`:

```bash
cooldown upgrade --major -p 'serde*'
```

When `upgrade` holds a cross-major update back, it prints a tip with the `--major` command that would take it (suppress tips with `--no-suggestions`).

## Flags

| Flag | Effect |
|---|---|
| `--transitive <mode>` | `allow` or `hide` — how to treat transitive dependencies (see above). |
| `--rewrite` | Always rewrite the manifest constraint, even for an in-range move. |
| `--build` | Also compile / sync after re-locking. |
| `--major` | Allow cross-major bumps (off by default for `upgrade`). |
| `--strict` | Exit `1` if the mutation cannot complete cleanly. |
| `--dry-run` | Resolve and print the plan; never mutate. |

`upgrade` always re-locks. Use `--dry-run` whenever you want to see the plan first; combine it with `--json` to feed the plan into other tooling.
