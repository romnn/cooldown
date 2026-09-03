---
title: Exclusions
weight: 4
---

# Exclusions

Two independent knobs trim what a run looks at: `exclude-folders` (prune directories from detection) and `exclude-packages` (drop packages from reports). Both live under the flag-default sections — `[global]`, a `[<command>]` override, or `[tool.<name>]` for one ecosystem — and a plain array **adds** to what the other sections and config files contribute (a prune set, so order is irrelevant; see [Clearing or replacing an inherited list](#clearing-or-replacing-an-inherited-list) for the other merge modes). They belong in the repo-root `cooldown.toml`, the global config, or a `--config` file: a nested `cooldown.toml` sets policy only, and an exclude list there is a config error.

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

## Clearing or replacing an inherited list

Lists layer across files (the global config → the repo-root `cooldown.toml` → `--config`) and across sections (`[global]` → `[<command>]`), and a plain array **adds** to what it inherits. To undo an inherited list, name the merge mode instead:

| Form | Effect on the inherited list |
|---|---|
| `exclude-folders = ["a"]` | adds `a` |
| `exclude-folders = []` | **clears** it |
| `exclude-folders = { replace = ["a"] }` | **replaces** it with `a` |
| `exclude-folders = { extend = ["a"] }` | adds `a` (the explicit spelling of the plain array) |

Each key merges on its own. A `[tool.cargo]` replacement swaps only the inherited `[tool.cargo]` list — the `[global]` list is a different key and is still combined with it at scan time — and a `[outdated]` replacement shadows `[global]` for `outdated` alone. A later layer's plain array adds to a replacement rather than reviving what it dropped. `exclude-packages` merges the same way, and a misspelt merge key is a config error.

Within one file the sections resolve first — `[global]`, then the `[<command>]` override — and only then do the files fold, lowest precedence first. A repo-root `[outdated]` replacement therefore drops everything the global config contributed for `outdated`, its `[global]` and `[outdated]` lists alike, and a plain array in a later `--config` file adds to that result.

Two spellings are rejected because they cannot mean what they say: `{ extend = [] }` changes nothing (write `[]` to clear), and two alias tables of one tool (`[tool.rust]` beside `[tool.cargo]`) that both set the same list in the same file would leave the winner to table order (one setting `exclude-folders` beside one setting `exclude-packages` is fine).

```toml
# ~/.config/cooldown/config.toml — the org-wide default
[tool.cargo]
exclude-folders = ["vendor"]
```

```toml
# cooldown.toml — this repo audits its vendored crates
[tool.cargo]
exclude-folders = []                            # drop the inherited per-tool list

[outdated]
exclude-folders = { replace = ["fixtures"] }    # for `outdated`, ignore [global] and prune only fixtures
```

## Running from an excluded directory

The excludes trim the *default* scan; naming a directory outranks them. `cooldown -C incubator check` — or `cd incubator && cooldown check` — never prunes `incubator` or the directories on the way to it (dot-directories included), even when the repo root lists it in `exclude-folders`, and the workspace member containing the selected directory is exempt from `exclude-folders` and `exclude-packages` alike. Members below the selection (`-C crates` in a workspace whose members live under `crates/`) are in scope, and a glob that matches them *below* the selection still drops them. Dependencies the tool attributes to no member (a Go module's, or the transitive rows of tools that attribute only direct dependencies) stay in scope for every command, so a selection never hides a transitive from the gate, and `fix`/`upgrade` under `-C <member>` may move one that only a sibling member needs, since a shared lock cannot change for one member alone. Without this rule a run pointed at an excluded subtree would check nothing and report a clean result.

A selection the run cannot honor is a usage error (exit `2`) rather than an empty result: one the run's own `--exclude-folders` names (drop one of the two), and one a `.gitignore`/`.ignore` rule hides, which the scan never reaches — pass `--no-gitignore` to enter it. A selection nothing covers (no project or workspace member under it, or none with dependencies) evaluates zero dependencies and says so with a config warning.

## On the command line

Both have a CLI form — `--exclude-folders <glob>` and `--exclude-packages <glob>` (repeatable) — that **replaces** the `[global]` / `[<command>]` config lists for that run (per-tool `[tool.*]` excludes still apply). CLI globs are validated the same way, so a malformed pattern is a config error, and so is a `--exclude-folders` glob that names the `-C` directory:

```bash
cooldown outdated --exclude-folders 'e2e' --exclude-folders '/vendor'
```

> [!NOTE]
> `exclude-folders` is about **where** to look; `exclude-packages` is about **what** to report. A dependency can be dropped by either — its folder being pruned from detection, or its name matching a package exclude.
