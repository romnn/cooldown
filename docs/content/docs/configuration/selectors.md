---
title: Selectors
weight: 3
---

# Selectors

A selector scopes a policy to part of your dependency graph. The level narrows *what a rule applies
to*. From least to most specific:

```
default  <  tool  <  project  <  registry  <  package  <  tool-qualified package
```

Within a layer, the most specific selector that matches wins — see [Precedence]({{< relref "precedence.md" >}}).

## Top level — the default

Keys at the top of `cooldown.toml`, outside any table, are the **default** selector — they apply to everything unless a more specific selector overrides them:

```toml
min-age = "14d"
```

## `[tool.<name>]` — per ecosystem

Scope policy to one package manager. Every supported tool is its own name — `cargo`, `go`, `uv`, `pip`, `poetry`, `conda`, `pixi`, `npm`, `pnpm`, `yarn`, `bun`, `deno`, `bundler`, `hex`, `maven`, `gradle`, `swift` — and common aliases (like `python`, `rust`, `node`) are accepted:

```toml
[tool.uv]
min-age = "21d"       # a longer window for Python deps only
```

### `[tool.pnpm] single-copy`

The packages a pnpm resolve must never leave at two resolved copies:

```toml
[tool.pnpm]
single-copy = ["solid-js", "react", "typescript"]
```

A second copy a dependent's own requirement pulls into the graph is ordinarily committed and
reported as a `duplicate_copy` warning. For a listed name the settlement is refused instead: the
lock is restored and the candidate whose landing added the copy is held with the copy and its
requirer named (see [upgrade]({{< relref "../commands/upgrade.md" >}}#whole-workspace-landings-pnpm)).
The gate judges what a resolve *adds*, counted as distinct resolved versions: a listed name the
graph held at one copy (or not at all) before the run and at several after it, or one already
split that gains another — whether the new copy is a dependent's own requirement or an importer's
own entry. A copy that merely moves to another version leaves the count alone and is not refused.
A listed name that is *already* at several copies before a run has that standing
split reported (as a `duplicate_copy` warning) rather than refused — refusing would hold every
upgrade forever — so converge it for the listing to hold. A `--lock` refresh (`check --lock`,
`outdated --lock`) is pnpm's own `install --lockfile-only` against the manifests as written and is
not gated. `--fail-on-new-duplicate` gates every package for one run; on a tool without Cargo or pnpm's
settlement guard it has no effect and the run says so. The key is pnpm-specific and accepted only
here (like `edge-policy` under `[tool.cargo]`). It merges across config files the way
`exclude-folders` does: a plain array adds to the list inherited from a farther file, `[]` clears
it, and `{ replace = ["…"] }` replaces it — so a nested workspace that lists one runtime of its own
does not un-gate the root's. Names in `pnpm-workspace.yaml`'s
`overrides` are *not* gated by default: an override pins a version for every request that matches
its range, which already keeps a copy single where the range is exact, and a ranged override is
often about forcing a patched transitive rather than about running the package once — list the
names you mean. Entries are exact package names, not globs: a pattern such as `@scope/*` is a
config error rather than a gate that silently matches nothing.

### `[tool.cargo] generated-members`

The workspace members whose `Cargo.toml` is a *generated projection* of the resolved graph rather
than something anyone wrote — a [cargo-hakari](https://crates.io/crates/cargo-hakari)
workspace-hack, whose `[dependencies]` table is regenerated wholesale from the lock so that every
member builds with one unified feature set:

```toml
[tool.cargo]
generated-members = ["workspace-hack"]
```

Such a manifest declares a crate because *something else in the graph already depends on it*,
never because a first-party crate wants it, so its entries can only follow the lock. Without the
declaration cooldown reads them as ordinary direct dependencies: every hash-aliased
`hashbrown-3575ec1268b04181 = { package = "hashbrown", version = "0.15" }` line becomes an
upgradeable row attributed to the hack, every `0.x` line becomes a `--major` candidate, and the
resolve then reports each one `blocked`, because the version is decided by whichever real crate
pulls it in. On a large workspace that is most of the blocked set, hiding the few rows that name a
real constraint.

A declared member becomes a **follower, never a driver**:

- **Nothing it declares is direct.** A crate only the generated member declares is a transitive
  dependency of the members that reach it through the real graph — reported under `--transitive`
  and attributed `via` those members, gated by `check` exactly as before, but never proposed as a
  direct upgrade. A crate an authored member also declares is attributed to that member alone.
- **Nothing it declares holds.** Its `=` pins, `<` bounds, and caret ranges are whatever its
  generator last wrote, so they neither mark a row `held` nor cap a candidate.
- **Its requirements follow every move.** When `upgrade`/`fix` moves a crate an authored member
  declares — including across a major — the generated manifest's entry that admitted the old
  version is made to project the new one, in the same step as the authored requirement and rolled
  back with it if the move does not land. Cargo therefore never sees the projection demand a
  version the lock no longer carries, which would otherwise resolve a second copy of the crate (or
  fail) — the workspace still resolves under `cargo metadata --locked` and no crate gains an extra
  copy. Following is what a generator does when it recomputes the projection: the entry's
  requirement is bumped, with its other fields (`features`, `default-features`) kept as written,
  unless a differently-keyed sibling entry already projects the line the crate is moving *to* (the
  hash-aliased entry for that major), in which case the moving entry is removed instead — bumped,
  both would resolve to one node under two names, which cargo refuses. A feature the new major no
  longer has is cargo's rejection of that one candidate, reported `blocked` with cargo's
  explanation like any other resolver rejection.
- **The run tells you to regenerate.** A rewritten projection no longer matches its generator's
  output, so the report ends with a `stale_lock` warning naming the manifest; finish the
  upgrade the way the generator expects — `cargo hakari generate`, then re-lock — and the
  projection converges (the generator computes the hack from the graph without the hack's own
  contribution, so a followed entry regenerates to the same line).

Detection is **never inferred**. A `### BEGIN HAKARI SECTION` marker, a `package.metadata` table,
or a `workspace-hack` name changes nothing: marking a member generated narrows what cooldown
proposes and reports, and a supply-chain tool must not narrow its own scope from a heuristic that
would be invisible in review and could capture a hand-written crate that merely looks similar. The
marker is used in one direction only, as advice: a project that declares nothing and has a member
whose manifest carries it gets a `config` warning suggesting the declaration (or an explicit
`generated-members = []`, which says every member is authored and silences the hint). A declared
member whose projection has gone stale — it declares crates no authored member reaches any more,
which a regeneration would drop — gets a `stale_lock` warning too.

A move can also drag companions the plan never named — `toml 0.7 → 0.8` takes `toml_edit 0.19 →
0.22` with it — and the projection's line for the old companion would keep that old copy alive
alone. After the pin phase cooldown therefore follows every crate the generated members are the
only ones still reaching, to the line the authored graph now resolves it to, and re-resolves so
cargo drops the retained copy. A crate the authored graph no longer resolves at all is left as a
stale projection line for the regeneration.

The key is cargo-specific and accepted only under `[tool.cargo]`, like `edge-policy`. Entries are
exact member **package names** (the name `cargo metadata`, `cargo -p`, and hakari's own config
use), not paths and not globs; a pattern is a config error. Every listed name must match a
workspace member, or every command that reads the workspace reports a `config` error for the
project and evaluates none of it — `check`, `upgrade`, and `fix` exit non-zero on it, while
`outdated`, which never gates, carries it in its report — so a stale entry, a renamed crate, or a
deleted one can never quietly stop covering a manifest that still exists. The nearest
`cooldown.toml` that sets the key decides for the workspace below it (an explicit `--config` file
wins over all); lists do not merge across files, since a declaration names *this* workspace's
members. `cooldown config` prints the resolved list and the file that declared it for every cargo
project, so an audit can see that scope was narrowed and by which file. `exclude-packages` is not
a substitute: it drops the member from reports but leaves its requirements alone, so an upgrade
would still resolve a second copy for the projection's sake. The two compose as excludes always
do: a crate whose only authored declarers are excluded goes with them, mirrored line included —
the projection never keeps a row alive that its authors are out of the run for.

## `[registry."<host>"]` — per registry

Scope policy to a registry or index by host. The natural home for "our own registry is trusted":

```toml
[registry."internal.acme.io"]
min-age = "0d"
```

## Package selectors

An unqualified package rule applies to a matching name in every ecosystem:

```toml
[package."github.com/acme/*"]
min-age = "0d"

[package.glob]
min-age = "14d"
```

Package globs use the same flavor as [`allow`]({{< relref "basics.md" >}}) and [`exclude-packages`]({{< relref "excludes.md" >}}): `*` is always a wildcard and crosses `/`, so `@scope/*` covers a whole npm scope and `serde_*` a crate family. No registry permits `*` in a package name, so nothing needs escaping.

When the same name exists in several ecosystems, qualify the package rule by its tool:

```toml
[tool.uv.package.glob]
min-age = "30d"
max-major = 5

[tool.cargo.package.glob]
min-age = "14d"
```

`[tool.uv.package.glob]` applies only to the PyPI package named `glob`; it cannot accidentally
change the Cargo crate or npm package with the same name. A tool-qualified package rule is more
specific than an unqualified `[package.glob]` rule in the same config layer.

`max-major` is available only on package rules and is an integer. It is an absolute ceiling:
within-major updates remain eligible, but even `upgrade --major --rewrite` will not cross it. For
example, keep TypeScript and its Node declarations current within supported lines with:

```toml
[tool.npm.package.typescript]
max-major = 5

[tool.npm.package."@types/node"]
max-major = 24
```

Raising or removing the value in `cooldown.toml` is the only way to cross a configured
`max-major`. There is no CLI or environment override.

## Choosing a level

- Trust a whole **registry** (an internal index)? Use `[registry."…"]`.
- Loosen or tighten one **ecosystem**? Use `[tool.<name>]`.
- Pin the policy for one **package or family**? Use `[package."…"]`.
- Target a package name in one **ecosystem only**? Use `[tool.<name>.package."…"]`.

When two rules could both apply, [`explain`]({{< relref "../commands/other.md" >}}) shows which one
won and why.

## Migration note

`exclude-folders` and `exclude-packages` belong under `[tool.*]`, `[global]`, or a command table,
not under package, registry, or project selector tables. Older versions accepted those keys under
non-tool selectors and then silently ignored them. They now fail configuration parsing so a
misspelled or misplaced exclusion cannot look active when it is not.
