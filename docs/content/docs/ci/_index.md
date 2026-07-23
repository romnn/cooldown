---
title: Continuous integration
weight: 7
bookCollapseSection: true
---

# Continuous integration

[`cooldown check`]({{< relref "../commands/check.md" >}}) is a fail-closed gate — it exits non-zero when anything in the resolved graph is younger than its cooldown — which makes it a natural CI step. Add it to pull-request checks and a too-fresh dependency stops the merge until it either matures, is [fixed]({{< relref "../commands/fix.md" >}}), or is explicitly [baselined]({{< relref "../commands/other.md" >}}).

- **[GitHub Actions]({{< relref "github-actions.md" >}})** — a complete workflow.

## What to run in CI

- **`cooldown check`** — the gate itself.
- **`--fresh`** — ignore the local cache and always hit the registry, so a CI run can't pass on a stale cached publish time.
- **`--transitive allow`** *(optional)* — if you want the whole graph evaluated but not every too-fresh transitive to block a merge, keep them visible but non-fatal.

```bash
cooldown check --fresh
```

The gate reads an existing lockfile; it does not need the ecosystem's tool to *resolve* unless you also pass `--lock`. Keep CI deterministic by committing your lockfiles and letting `check` read them as-is.
