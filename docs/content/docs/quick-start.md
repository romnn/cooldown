---
title: Quick start
weight: 3
---

# Quick start

This walks through a first run and how to read the output. It assumes `cooldown` is [installed]({{< relref "installation.md" >}}). Run it from the root of any repository — `cooldown` detects the package managers for you.

## 1. See what could update

```bash
cooldown outdated
```

`cooldown` detects the ecosystems in the directory, resolves each one, and prints a table split by what you can do about each dependency:

{{< terminal name="outdated" >}}

Read the **Status** column:

- **adoptable** — a newer version exists and has already cleared its cooldown window; safe to move to.
- **in cooldown** — a newer version exists but is still too fresh. The **Cooldown** column shows `age/window` — how old the candidate is versus the window it must clear.
- **exempt** — matched an `allow` rule, so no cooldown applies.
- **held** — pinned (an exact `==`/`=` pin or a commit pin), so `cooldown` won't move it.
- **up-to-date** — already on the latest version (hidden by default; shown here because the example opts in).

The summary line counts the whole resolved graph, even when the table shows only direct dependencies.

## 2. Move within the cooldown

```bash
cooldown upgrade
```

`upgrade` advances each dependency to the newest version that has **already matured** past its cooldown, then re-locks. Preview the plan without touching anything with `--dry-run`:

{{< terminal name="upgrade" >}}

Only versions that have cleared the window are proposed. A too-fresh version a re-lock would otherwise drag in is reconciled back down, so the new lock is gate-clean by construction.

## 3. Gate CI

```bash
cooldown check
```

`check` is the fail-closed CI gate. It evaluates the **resolved lockfile graph** — direct and transitive — and exits non-zero if anything is younger than its cooldown:

{{< terminal name="check" >}}

A clean run exits `0`; a violation exits `1`. See [Exit codes]({{< relref "commands/cli-reference.md" >}}#exit-codes) for the full contract.

## 4. When the gate goes red

If `check` fails because a dependency is too fresh, you have three options:

- **Wait** for it to mature — the intended path for a legitimate release.
- [**`baseline`**]({{< relref "commands/other.md" >}}) it — acknowledge and accept the specific version, so `check` stops failing on it.
- [**`fix`**]({{< relref "commands/fix.md" >}}) it — downgrade the violating dependency to the newest version that has already matured, so the gate passes while the protection holds.

## 5. Raise the window

The happy path is zero config — the default window is 7 days. Raising the whole repo to 14 days is one line of `cooldown.toml` at the repo root:

```toml
min-age = "14d"
```

That is the one knob most repositories ever set. Continue with [Configuration]({{< relref "configuration/_index.md" >}}) for per-tool, per-registry, and per-package policy, or the [Commands]({{< relref "commands/_index.md" >}}) reference for everything each subcommand can do.
