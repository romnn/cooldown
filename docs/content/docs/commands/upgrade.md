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

When the target falls outside an **implicit** constraint — most commonly a cross-major bump
(`--major`) past a caret or compatible-release range such as `^5` or `~=5.9` — `cooldown` rewrites
the one owning manifest entry so the version can be adopted, then re-locks. Edits are
**format-preserving** (comments, key order, and spacing are kept), and for a Cargo workspace an
inherited `dep = { workspace = true }` is widened in the root `[workspace.dependencies]`.

An explicit upper comparator written by the author, such as `<6` or `>=5,<6`, is different: it
holds under the default behavior, even with `--major`. Pass `--rewrite` to cross and rewrite that
bound. The same flag still always rewrites an in-range constraint (so `^1.4` becomes `^1.5`).

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

When `upgrade` holds a cross-major update back, it explains the required action:

- `needs --major` means re-run with `--major`.
- `declared_bound_held` means pass `--rewrite` to cross and rewrite the manifest's explicit
  `<`/`<=` bound.
- `max_major_held` means raise the package's `max-major` in `cooldown.toml`; no CLI flag overrides
  this ceiling.

Only a matured release beyond the hold is reported. A fresh release still in cooldown does not
produce an action that cannot yet be taken. Suppress command tips with `--no-suggestions`.

## Flags

| Flag | Effect |
|---|---|
| `--transitive <mode>` | `allow` or `hide` — how to treat transitive dependencies (see above). |
| `--rewrite` | Always rewrite the manifest constraint; also the only way to cross an explicit `<`/`<=` bound. |
| `--build` | Also compile / sync after re-locking. |
| `--major` | Allow cross-major bumps; explicit manifest bounds and config `max-major` ceilings still hold. |
| `--strict` | Exit `1` if the mutation cannot complete cleanly. |
| `--dry-run` | Resolve and print the plan; never mutate. |

`upgrade` always re-locks. Use `--dry-run` whenever you want to see the plan first; combine it with `--json` to feed the plan into other tooling.
