---
title: Exclusions
weight: 4
---

# Exclusions

Two independent knobs trim what a run looks at: `exclude-folders` (prune directories from detection) and `exclude-packages` (drop packages from reports). Both live under the flag-default sections — `[global]`, a `[<command>]` override, or `[tool.<name>]` for one ecosystem — and **concatenate** across them (a prune set; order is irrelevant).

Every pattern is compiled when the config is loaded, so a bad glob is a **config error** (exit `2`), not a surprise mid-scan.

```toml
[global]
exclude-folders = ["examples", "/build", "third_party/grammars"]
exclude-packages = ["internal-*"]

[outdated]
exclude-folders = ["fixtures"]      # adds to the global set, for `outdated` only

[tool.npm]
exclude-folders = ["e2e"]           # per-ecosystem folder excludes
exclude-packages = ["@scope/*"]     # package-name format differs per ecosystem
```

## `exclude-folders`

Prunes directories from project detection (in addition to `.gitignore`), and also drops a dependency whose declaring workspace members all sit under an excluded path — handy when one root lockfile covers a whole monorepo.

It uses the **same `.gitignore` semantics** the scan already honors, so there is one model to learn:

| Pattern | Matches |
|---|---|
| `target` | every `target/` directory, at **any depth** |
| `target/` | identical — a trailing slash is allowed and ignored |
| `/build` | only the top-level `build/` (a leading slash anchors to the scan root) |
| `third_party/grammars` | the root-relative path `third_party/grammars` (an interior slash anchors) |
| `**/snapshots` | every `snapshots/` at any depth (explicit, same as the bare name) |

## `exclude-packages`

Drops a workspace member from reports when its **package name** matches a glob — the same glob flavor as the `[package."…"]` [selector]({{< relref "selectors.md" >}}). `*` is always a wildcard (no registry permits `*` in a package name, so nothing needs escaping) and crosses `/`, so `@scope/*` covers a whole npm scope and `serde_*` a crate family.

Because names differ per ecosystem (`my-pkg` vs `@scope/my-pkg`), reach for `[tool.<name>].exclude-packages` when a pattern is ecosystem-specific; a `[global]` entry applies to every tool.

## On the command line

Both have a CLI form — `--exclude-folders <glob>` and `--exclude-packages <glob>` (repeatable) — that **replaces** the `[global]` / `[<command>]` config lists for that run (per-tool `[tool.*]` excludes still apply). CLI globs are validated the same way, so a malformed pattern is a config error:

```bash
cooldown outdated --exclude-folders 'e2e' --exclude-folders '/vendor'
```

> [!NOTE]
> `exclude-folders` is about **where** to look; `exclude-packages` is about **what** to report. A dependency can be dropped by either — its folder being pruned from detection, or its name matching a package exclude.
