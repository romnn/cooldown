---
title: Commands
weight: 4
bookCollapseSection: true
---

# Commands

Every command shares the same policy engine and the same global flags; they differ only in what they do with the verdict. The four you reach for most:

| Command | What it does |
|---|---|
| [`outdated`]({{< relref "outdated.md" >}}) | What could update, split into adoptable-now vs in-cooldown. |
| [`upgrade`]({{< relref "upgrade.md" >}}) | Move the graph to the newest matured version; re-locks. |
| [`fix`]({{< relref "fix.md" >}}) | Downgrade too-fresh dependencies to a matured version to clear a violation. |
| [`check`]({{< relref "check.md" >}}) | The CI gate over the resolved lockfile graph (fail-closed). |

The smaller commands — `baseline`, `explain`, `config`, `init`, `schema`, and `sync` — are covered in [Other commands]({{< relref "other.md" >}}). The [CLI reference]({{< relref "cli-reference.md" >}}) lists the global flags and the exit-code contract.

## How they relate

`outdated` and `upgrade` look **forward** — what newer version could or should you move to. `check` and `fix` look at the **current** state — is anything you have resolved right now too fresh, and how do you get back to green.

All four reason over the **resolved graph** by default, not just your manifest: a `^1.4` you declared pulls in transitive dependencies, and those are part of the risk surface. How each command treats transitive dependencies is controlled by [`--transitive`]({{< relref "cli-reference.md" >}}), with a strict, act-on-them default that you opt out of, never into.

> [!NOTE]
> Any command can be run with `--json` for machine-readable output (the schema is printed by [`cooldown schema`]({{< relref "other.md" >}})), and with `--dry-run` to resolve and print a plan without mutating anything.
