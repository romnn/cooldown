---
title: outdated
weight: 1
---

# `outdated`

`outdated` reports what could update, split into what is **adoptable now** versus what is **still in cooldown**. It never mutates anything — it is the read-only "what's going on" view.

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
| **Cooldown** | `age/window` for the relevant candidate — how old it is versus the window it must clear. |
| **Status** | `adoptable`, `in cooldown`, `exempt`, `held`, or `up-to-date`. |

The summary line at the bottom counts the **whole resolved graph** (direct + transitive), even though the table shows only direct dependencies by default.

## What it shows by default

- **Direct dependencies only** in the table. Add `--transitive` to list indirect dependencies too.
- **Actionable rows only** — dependencies that are already up-to-date are hidden. Add `--all` to include them.
- **Cross-major candidates are visible.** Unlike `upgrade`, `outdated` shows a new major so it is discoverable; add `--no-major` (alias `--minor`) to stay within the current major — useful for clean CI output.

## Flags

| Flag | Effect |
|---|---|
| `--transitive` | Also list transitive (indirect) dependencies. |
| `--all` | Also list up-to-date dependencies. |
| `--hide-pinned` | Hide held rows (exact pins, commit pins) that have no actionable update. |
| `--countdown <which>` | Which still-cooling upgrade the **Cooldown** column counts down to when several newer versions exist. |
| `--exit-code[=N]` | Exit non-zero when adoptable updates exist, for CI gating. Bare `--exit-code` means `1`. |
| `--lock` | Refresh lockfiles before reading them (mutates lockfiles; ignored under `--dry-run`). |

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
