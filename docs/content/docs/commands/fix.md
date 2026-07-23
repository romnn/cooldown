---
title: fix
weight: 3
---

# `fix`

`fix` is the dual of [`upgrade`]({{< relref "upgrade.md" >}}): it **downgrades** each violating dependency to the newest version that has *already* matured past its cooldown, so [`check`]({{< relref "check.md" >}}) passes while the protection holds. It never moves a dependency forward, and it only touches dependencies that are actually in violation.

```bash
cooldown fix
```

When nothing is too fresh, `fix` is a no-op:

```text
Nothing to fix.

0 applied · 0 skipped · 0 errors
```

When there are violations, each is rolled back to a matured version:

```text
 Package   Used by        From      To        Status        Reason
─────────────────────────────────────────────────────────────────────
 left-pad  app            3.1.0     3.0.9     downgraded    too fresh (2d/7d)
```

## The whole-graph default

By default `fix` works on the **whole resolved graph** — the same surface `check` gates. A too-fresh **transitive** dependency is rolled back to the newest matured version the graph still allows, not just direct dependencies.

This is safe by construction: the graph floor *is* a version every requirer already accepts, and a mature direct dependency was built against versions from before the window anyway — so a fresh transitive it didn't ask for is the riskier state, not the rollback.

`--transitive` relaxes this, mirroring `check`:

- **`--transitive hide`** — direct-only: ignore transitive dependencies entirely.
- **`--transitive allow`** — report too-fresh transitives but leave them in place; still fix direct dependencies.

## What `fix` won't silently do

`fix` is conservative — it reports, rather than forces, the cases where a downgrade would be wrong or impossible:

- **A graph-pinned transitive.** If no lower version satisfies the dependency's requirers, it can't be rolled back on its own. `fix` reports it so you can address the dependency that forces the fresh pin, instead of breaking resolution.
- **An exact pin.** A pinned violation is left in place with a warning, since a pin is a deliberate choice. Pass `--downgrade-pinned` to downgrade and rewrite it too.
- **No matured fallback.** A violation with no older matured version to fall back to is reported — [`baseline`]({{< relref "other.md" >}}) it or wait — rather than downgraded to nothing.

## Flags

| Flag | Effect |
|---|---|
| `--transitive <mode>` | `allow` or `hide` — how to treat too-fresh transitive dependencies (see above). |
| `--downgrade-pinned` | Downgrade and rewrite exact-pinned dependencies too (off by default). |
| `--strict` | Exit `1` if the fix cannot complete cleanly. |
| `--dry-run` | Resolve and print the plan; never mutate. |

## `fix` versus `baseline`

Both clear a red `check`, but they mean different things:

- **`fix`** removes the risk — you end up on an older, matured version.
- [**`baseline`**]({{< relref "other.md" >}}) accepts the risk — you stay on the fresh version but record it as acknowledged, so `check` adopts it cleanly. Reach for `baseline` when you have a reason to trust the specific release and can't (or won't) roll back.
