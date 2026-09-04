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

## Rolling back a security bump

With the [advisory feed]({{< relref "../configuration/advisories.md" >}}) enabled, a pin that is itself an advisory's fix version resolves against the shorter security window — so most security bumps never reach `fix` at all. One that is younger than even *that* window still fails `check`, and `fix` still rolls it back to the newest matured version, which the same advisory may mark affected.

The verdict is deliberately unchanged: `fix` exists to make `check` pass, and cooldown is not a vulnerability gate. What the feed adds is that the trade is stated rather than made silently:

```text
warning [advisory_rollback]  left-pad@3.1.0 is younger than its cooldown; rolling it back to
3.0.9 re-enters GHSA-wcg3-cvx6-7396 — `baseline` the current pin instead to keep the fix
```

The warning describes a rollback that actually happened: it is attached to the batch that landed the downgrade, so a batch the apply rejects or the post-apply gate rolls back takes the warning down with it. Under `--dry-run` it speaks in the conditional instead (`would roll it back to 3.0.9, re-entering …`), like the rest of a dry run's report.

[`baseline`]({{< relref "other.md" >}}) is the way to keep the fix and still pass the gate: it acknowledges the young pin instead of undoing it.

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
| `--cargo-edge-policy <policy>` | cargo: `preserve` (default), `canonicalize`, or `none` — how lock edge bindings are treated after the re-resolve (see [upgrade]({{< relref "upgrade.md" >}}#lock-edge-bindings-cargo)). Config: `[tool.cargo] edge-policy`. |
| `--strict` | Exit `1` if the fix cannot complete cleanly. |
| `--fail-on-new-duplicate` | pnpm: refuse a resolve that gives *any* package a second resolved copy instead of committing it with a `duplicate_copy` warning (see [upgrade]({{< relref "upgrade.md" >}}#whole-workspace-landings-pnpm)). On a tool without pnpm's settlement guard the flag has no effect and the run says so. |
| `--dry-run` | Resolve and print the plan; never mutate. |

## `fix` versus `baseline`

Both clear a red `check`, but they mean different things:

- **`fix`** removes the risk — you end up on an older, matured version.
- [**`baseline`**]({{< relref "other.md" >}}) accepts the risk — you stay on the fresh version but record it as acknowledged, so `check` adopts it cleanly. Reach for `baseline` when you have a reason to trust the specific release and can't (or won't) roll back.
