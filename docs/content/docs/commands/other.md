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

When workspace members that declare the package are excluded from the run (`exclude-folders`,
`exclude-packages`, or a `-C` selection), the header lists them as `excluded members`, and
`--json` carries them as `excludedMembers`: their declarations are neither evaluated nor moved,
and on pnpm they no longer count as workspace evidence — a copy on another line in an excluded
importer cannot hold an update in an included one.

## `config`

Print the **fully-resolved** configuration, with the origin of each value:

```bash
cooldown config
```

{{< terminal name="config" >}}

Where `explain` answers "why this one package," `config` answers "what is the effective policy here, and which layers produced it." It is the first thing to run when a repo's behavior surprises you.

Each project also reports its resolved [`[advisories]`]({{< relref "../configuration/advisories.md" >}}) policy and the advisory-database ecosystem covering its tool — the place to look when an enabled feed annotates nothing. Mixed-registry tools such as conda, pixi, and Deno currently have no single project-wide ecosystem mapping, even though individual PyPI or npm packages in their locks may be covered by OSV.

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

## `recover`

Settle Cargo project state left by an interrupted cooldown mutation, then stop without resolving
dependencies or applying another change:

```bash
cooldown recover
```

Recovery currently supports Cargo only. Selecting another ecosystem with `--tool` is rejected
instead of reporting an empty successful run.

Read-only commands fail closed when they find a pending transaction; they never repair it as a
side effect. Recovery validates every recorded manifest and lock state before changing anything.
It accepts an already complete publication or restores a mixed interrupted publication to its
complete preimage, and leaves the project and recovery evidence untouched on unknown drift.
The project-visible record is trusted only when its exact digest matches an owner-private authority
under the repository's Git common directory. Non-Git projects do not grant recovery authority from
their project-local coordination files, so recoverable Cargo source mutations currently require a
Git worktree.
It discovers Cargo's public marker and orphaned private state/publication artifacts without loading
policy, manifests, baselines, or registries, so malformed normal-run inputs cannot prevent
recovery. Setup failures also use the schema-v4 recovery envelope under `--json`.

Repository recovery also inspects trusted authority records and their interrupted private
publication names. This finds transactions that stopped after authority became durable but before
the project-visible marker was published, including projects hidden from the ordinary repository
walk.

A repository-root recovery scan includes hidden and gitignored projects but deliberately skips
bulk dependency, build, and cache directories: `.cache`, `.venv`, `node_modules`, `target`, and
`vendor`. To recover an explicitly targeted Cargo project inside one of those directories, run
`cooldown recover -C path/to/project --cargo`. An explicit scope scans only that directory's
subtree and relevant ancestors, so unreadable or malformed repository siblings cannot block the
selected project.
