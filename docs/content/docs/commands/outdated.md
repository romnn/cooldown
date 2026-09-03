---
title: outdated
weight: 1
---

# `outdated`

`outdated` reports what could update, split into what is **adoptable now** versus what is **still in cooldown**. It never mutates anything (`--lock` is the one opt-in exception) — it is the read-only "what's going on" view.

```bash
cooldown outdated
```

{{< terminal name="outdated" >}}

## Reading the table

| Column | Meaning |
|---|---|
| **Package** | The dependency name. |
| **Used by** | The workspace member(s) that declare it (`first (+N others)` when several do). |
| **Current** | The version currently locked. |
| **Adoptable** | The newest version that has already cleared its cooldown — blank (`—`) if nothing new has matured. |
| **Latest** | The newest version that exists, cooled down or not. |
| **Cooldown** | `age/window` for the relevant candidate — how old it is versus the window it must clear. A shortened advisory window appends `(security GHSA-…)`; a stricter clamp appends `(≥source)`. |
| **Status** | `adoptable`, `blocked`, `in cooldown`, `exempt`, `held`, or `up-to-date`; held rows show the reason, and a blocked row names its blocker (`blocked by eslint-config-next`) when the resolve named one. Every blocked row carries the full reason the upgrade resolve gave — a partial landing and its remedy, a workspace split and the flag that converges it, a refused duplicate copy — as `blockedReason` in `--json` and, under `--why`, below the table; the sentences are too wide for the column itself. With the [advisory feed]({{< relref "../configuration/advisories.md" >}}) enabled, a candidate that fixes an advisory affecting the current pin is annotated `⚠ fixes GHSA-… (high)`. |

The summary line at the bottom counts the **whole resolved graph** (direct + transitive), even though the table shows only direct dependencies by default. Its `held` figure counts the rows whose status is `held` (exact pins, commit pins, declared bounds, ceilings); the `warning [held]` lines under the table are a different thing — per-package holds the verification preview reports — and a second copy the preview's resolve added is a `duplicate_copy` warning, not a `held` one.

## What it shows by default

- **Direct dependencies only** in the table. Add `--transitive` to list indirect dependencies too.
- **Actionable rows only** — dependencies that are already up-to-date are hidden. Add `--all` to include them.
- **Cross-major candidates are visible.** Unlike `upgrade`, `outdated` shows a new major so it is discoverable; add `--no-major` (alias `--minor`) to stay within the current major — useful for clean CI output.

An explicit manifest upper comparator is intentional policy: `>=5 <6` holds a dependency below 6,
while the implicit ceiling in `^5` does not. A package-level config `max-major = 5` likewise holds
the dependency at 5.x. Held rows name the reason in the table (`bound >=5 <6`, `max-major 5`,
`pinned`, and so on); JSON reports the same information in the additive `heldBy` field.

Two npm-family safeguards refine what counts as adoptable:

- **The `latest` dist-tag caps adoption.** The tag is the maintainer's own "this is current"
  pointer — what a bare `npm install <pkg>` resolves to under npm's default configuration — so a
  stable release above it (a premature or abandoned major the maintainer kept releasing below,
  like a `17.0.0` published months before the `16.x` line continued) is held rather than
  proposed: the row reads `dist-tag latest 16.13.0` while **Latest** still shows the newer
  version for context. A project already pinned above the tag is unaffected (a pin beyond the tag
  deactivates the ceiling entirely — a project deliberately riding a `next` line keeps seeing
  newer releases), and `--no-respect-dist-tags` is the deliberate escape hatch. The tag — like
  every npm listing cooldown reads — comes from the public registry (`registry.npmjs.org`); a
  project resolving from a private registry or mirror whose tags diverge should opt out. In
  `cooldown.toml` the same opt-out goes under `[global]` or a command section:

  ```toml
  [global]
  respect-dist-tags = false
  ```

- **Peer contracts block infeasible majors.** A matured cross-major target that a still-present
  dependent's recorded peer range excludes (`fumadocs-mdx` peer-requires `fumadocs-core@^16` while
  the target is `17.0.0`) is reported `blocked by fumadocs-mdx` instead of `adoptable` — pnpm's
  resolver only warns on that break, and npm, which rejects it by default, commits it under
  relaxed enforcement such as `legacy-peer-deps`. The way forward is to upgrade the
  dependent — or, when the dependent is a *workspace-local* package (a `workspace:*` package,
  symlinked or injected), to edit its own `peerDependencies` range, since a local package never
  moves in a run. On pnpm both can move in the same run: its whole-graph resolve decides the pair's
  joint feasibility, and the result is re-checked — a landing that provably breaks a recorded
  peer contract *between packages your workspace itself declares, in a context that demonstrably
  binds them* (npm's hoisted tree is judged physically, pnpm importers by their own declarations)
  is rejected. Contracts involving transitive packages, contexts that never bind the moved copy,
  or unprovable ranges stay the resolver's call, so still review its peer warnings on a joint
  move. npm has no joint
  resolve, so there the target
  stays blocked while the dependent moves; a dependent move that would itself break the contract
  is rolled back after the fact (see [`upgrade`]({{< relref "upgrade.md" >}})), and otherwise the
  next run reads the dependent's new peer range and releases the block if it now admits the
  target.

## Flags

| Flag | Effect |
|---|---|
| `--transitive` | Also list transitive (indirect) dependencies. |
| `--all` | Also list up-to-date dependencies. |
| `--hide-pinned` | Hide held rows (exact pins, commit pins) that have no actionable update. |
| `--why` | Print, below the table, why each blocked row is blocked — the reason the upgrade resolve gives, as `upgrade --dry-run` would show for that row. `--json` always carries it as `blockedReason`. |
| `--countdown <which>` | Which still-cooling upgrade the **Cooldown** column counts down to when several newer versions exist. |
| `--exit-code[=N]` | Exit non-zero when adoptable updates exist, for CI gating. Bare `--exit-code` means `1`. |
| `--lock` | Refresh lockfiles before reading them (mutates lockfiles; ignored under `--dry-run`). Cargo runs `cargo update --workspace` (a locked version the manifests still admit stays put; one they no longer admit is re-resolved or dropped) and pnpm runs `pnpm install --lockfile-only`; the other tools have no standalone refresh, say so with a warning, and read the existing lock as-is. Only the projects the run evaluates are refreshed (`-C incubator` refreshes the nested workspace's lock, not the enclosing one's), and every such project is refreshed before `--package` narrows the rows. A refresh the tool rejects is an error even under `--allow-stale-lock`, and `--offline` rejects the flag outright because the refresh needs the resolver's network access. |

### `--countdown`

When several newer versions are cooling at once, the **Cooldown** column can only show one. `--countdown` picks which:

- **`soonest`** (default) — count down to the *next* version to mature. An intermediate release can clear the window days before the newest one does, so this shows the soonest unlock. The candidate is named in parentheses when it differs from **Latest** (e.g. `28d/30d (0.4.30)`).
- **`latest`** — count down to the newest version, the longest wait.

It is display-only: it changes which candidate's `age/window` you see, never what is adoptable.

### `--exit-code`

`outdated` is informational and exits `0` by default. `--exit-code` turns it into a soft gate — for a nightly job that should flag when adoptable updates have piled up:

```bash
cooldown outdated --exit-code       # exit 1 if anything is adoptable
cooldown outdated --exit-code=2     # or a custom code
```

Pair it with `--no-major` to ignore cross-major bumps that you don't want the job to nag about.

## Presentation

Several [global flags]({{< relref "cli-reference.md" >}}) shape the table without changing the policy: `--list-packages` (one source package per line instead of `first (+N others)`), `--paths` (show **Used by** as workspace paths), and `--show-projects` (attribute each row to its project in a multi-project repo).
