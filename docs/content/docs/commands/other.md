---
title: Other commands
weight: 5
---

# Other commands

Beyond the four workhorses, `cooldown` has a handful of smaller commands for acknowledging risk, understanding policy, and scaffolding.

## `baseline`

Record currently-young dependencies as **acknowledged**, so `check` adopts them cleanly instead of failing:

```bash
cooldown baseline
```

Use it when a young dependency is one you have a reason to trust and you don't want to roll it back. A baseline is an explicit, reviewable record — it says "I have seen this specific version and accept it." Once a baselined version ages past its window (or is no longer present), it is dead weight; `--prune` drops those stale entries:

```bash
cooldown baseline --prune
```

`baseline` accepts the risk; [`fix`]({{< relref "fix.md" >}}) removes it. Reach for `fix` when you can roll back, and `baseline` when you can't or won't.

## `explain`

Show **why** a package has the window it has — every layer and rule that applied, in precedence order:

```bash
cooldown explain <package>     # alias: cooldown why <package>
```

{{< terminal name="explain" >}}

Each row is one layer/selector that was considered; the `Applied` column marks the ones that won, and the `Note` explains why. This is the tool for answering "why is this dependency exempt?" or "which `cooldown.toml` set this window?" — see [Precedence]({{< relref "../configuration/precedence.md" >}}) for the model it renders.

## `config`

Print the **fully-resolved** configuration, with the origin of each value:

```bash
cooldown config
```

{{< terminal name="config" >}}

Where `explain` answers "why this one package," `config` answers "what is the effective policy here, and which layers produced it." It is the first thing to run when a repo's behavior surprises you.

## `init`

Scaffold a documented starter `cooldown.toml`:

```bash
cooldown init
```

It writes a commented file you can trim to taste, and **refuses to clobber** an existing `cooldown.toml` — so it is always safe to run.

## `schema`

Print the machine-readable JSON schema for `--json` output:

```bash
cooldown schema
```

Use it to validate or generate types for anything that consumes `cooldown --json`. The JSON envelope is a supported interface; the schema is its contract.

## `sync`

Write the resolved policy **down into native configs** — for example uv's `exclude-newer` — so `cooldown.toml` stays the single source of truth and native tooling sees the same window:

```bash
cooldown sync
```

The same behavior is available as a global `--sync` flag on any command, which syncs before the command runs (a no-op under `--dry-run`). See [Supported ecosystems]({{< relref "../tools/_index.md" >}}) for which tools have a native cooldown that `sync` can write to.
