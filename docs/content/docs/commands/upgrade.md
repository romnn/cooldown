---
title: upgrade
weight: 2
---

# `upgrade`

`upgrade` moves dependencies **forward** to the newest version that has already matured past its cooldown, then re-locks. It never adopts a too-fresh version — only versions that have cleared the window are proposed. With the [advisory feed]({{< relref "../configuration/advisories.md" >}})'s `shorten` mode enabled, a candidate that fixes a qualifying advisory clears the shorter security window instead, so a security fix is adopted earlier than a routine bump.

```bash
cooldown upgrade
```

Preview the plan without touching anything with `--dry-run`:

{{< terminal name="upgrade" >}}

The **From → To** columns show the move; a row can be a `downgraded` as well as an `upgraded` when a re-lock would otherwise pull a too-fresh transitive in and it has to be reconciled back down (see [Transitive dependencies](#transitive-dependencies)).

A row that moved for security reasons says so in **Reason** — `⚠ fixes GHSA-… (high); adopted on the security window` — and carries the same `security` object in `--json`, so a fast-tracked adoption is never indistinguishable from a routine bump. A row the security window made eligible but that did not land (a skip, an error) says `eligible on the security window` instead, and a `--dry-run` row that would land says `would adopt on the security window`: the block describes the window, not the outcome.

## Lock versus manifest

By default `upgrade` moves the **lock** within your declared version constraint and leaves the manifest alone: a `^1.4` stays `^1.4` while the lock advances to the newest matured `1.x`.

When the target falls outside an **implicit** constraint — most commonly a cross-major bump
(`--major`) past a caret or compatible-release range such as `^5` or `~=5.9` — `cooldown` rewrites
the one owning manifest entry so the version can be adopted, then re-locks. Edits are
**format-preserving** (comments, key order, and spacing are kept), and for a Cargo workspace an
inherited `dep = { workspace = true }` is widened in the root `[workspace.dependencies]`.

An explicit upper comparator written by the author, such as `<6` or `>=5,<6`, is different: it
holds under the default behavior, even with `--major`. Pass `--rewrite` to cross and rewrite that
bound. The same flag still always rewrites an in-range constraint (so `^1.4` becomes `^1.5`), and
on pnpm it is also what converges a workspace *split* — see `multi_version_held` under
[Major versions](#major-versions).

One field is never rewritten, by any flag: **`peerDependencies`** (npm family). That range is not a
declaration of what the package installs but a contract it publishes to *its* consumers, and
widening shifts ranges rather than only loosening them (`>=5.6.0 <5.6.2` would become `^5.6.2`),
which would drop consumers the author still supports. A move that a workspace package's own peer
range provably excludes is reported `peer_held` instead — including a same-line move, since a narrow
bound like `>=5.6.0 <5.6.2` is broken by a mere patch bump. Edit that range yourself to authorize
the move; `--rewrite` does not cross it.

The lock-only default is honored where the tool can pin an exact in-range version without editing the manifest:

| Tool | In-range pin | Behavior |
|---|---|---|
| cargo | `update --precise` | lock-only |
| uv | `lock --upgrade-package` | lock-only |
| pnpm | `update --no-save` | lock-only |
| npm, yarn, bun | *(no such command)* | always rewrites the manifest |
| Go | `go.mod` *is* the version source | always rewrites the manifest |

Cargo resolver and edge-normalization trials run in an isolated project copy. Cooldown compares the
source manifests and lock with the preimage used for that trial, then publishes the accepted files
under one owner-only recovery record. On Unix its directory transitions are durable; other
platforms use their best available atomic replacement without promising parent-directory
durability. An interrupted publication can therefore recover the complete
old or accepted state without treating a rejected resolver trial as source-project state.

## Whole-workspace landings (pnpm)

pnpm re-resolves the whole importer graph jointly, and an exact pin is a request for *every*
importer that declares the package. pnpm can land it in some importers and leave one behind (a
subtree whose peers bind the old copy), which would split a package the workspace resolved at a
single version — a runtime break for anything with instance-level state, and one no row would
account for. Such a partial landing is rolled back: the candidate is reported as a resolver
conflict whose row names the importers that took the target and the ones that kept the old
version, and the rest of the batch is re-resolved without it. When the importer left behind
declares the package only in `peerDependencies` — pnpm auto-installs such a peer and records it
in the importer's lock entry, but `pnpm update` has no install field to advance, and `--rewrite`
cannot help because the declared range already admits the target — the row says so and names the
change that lets it move: a library adds the package to `devDependencies` as well, an application
(`private: true`) moves the entry to `dependencies`. Importers the run excludes are not
part of that contract: `pnpm update <name>@<target>` re-resolves the named package in *every*
importer whose range admits a newer version, whatever `--filter` it carried — the filtered importers
get the exact target, every other one the newest version its own range admits under the release-age
floor (verified against pnpm 10). That is the resolver's doing, not a pin cooldown placed — the
excluded importer's manifest is never rewritten and it is never named in the update — and refusing
it would let the excluded subtree veto the upgrade, so the move is committed and reported as an
`excluded_moved` warning naming the importer and the package. A package the resolver itself splits — one it was not asked
to pin, floated in only some importers — is never committed: the batch is refused with the split
named, and candidate isolation holds the candidate whose landing caused it (a resolver-conflict row
carrying that detail) while the rest of the batch lands. A split whose managed
importers all agree on one version, with only importers the run excludes left behind on the old
one, is what the exclusion asked for; it is committed and reported as a `duplicate_copy` warning
naming those importers, the same kind that names a second copy a dependent's own requirement pulls
in below the importers' view. Managed importers ending up on two versions fail the batch however many excluded
importers stayed behind, and so does a settlement that *re-declares* an excluded importer's
dependency — a changed specifier, an entry appearing or disappearing, a version its own range
provably excludes — which pnpm never does on its own: cooldown never re-resolves an importer it was
told to ignore. Either refusal leaves the lock as it was and names what the resolve did.

A second copy a *dependent's own requirement* pulls in below the importers' view
(`vite-plugin-solid@2.11` requiring `solid-js@^1.9.15` beside the apps' `1.9.14`) is the
resolver's legitimate answer to that range, so it is committed — and reported as a
`duplicate_copy` warning naming the copy and its requirer (`required at 1.9.15 by
vite-plugin-solid@2.11.14`). For a package that must never run twice — a reactive runtime, a
type checker — list it under [`[tool.pnpm] single-copy`]({{< relref
"../configuration/selectors.md" >}}#toolpnpm-single-copy) and the warning becomes a refusal: the
settlement is rejected, the lock restored, and the candidate whose landing added the copy is held
with the copy and its requirer named, exactly as a partial landing is. `--fail-on-new-duplicate`
gates every package for one run without naming any. A gated name is refused even when the second
copy is the one an exclusion asked for (an excluded importer left on the old version): the lock
the workspace installs would still hold two copies.

## Transitive dependencies

By default `upgrade` moves the **whole graph**: it advances each dependency — transitive ones included — to its newest matured version, and reconciles any too-fresh transitive a re-lock drags in back down, so the new lock is **gate-clean by construction** — a subsequent `check` won't reject it. Advancing a transitive requires an engine that can pin a package no manifest declares: cargo, pnpm, go, and uv have one; **every other adapter (npm, yarn, bun, deno, pip, …) plans direct dependencies only** (their per-package apply needs a declared requirement). A transitive whose parents exclude its matured release (an exact-pinning parent, a name resolved at several graph copies) is reported held rather than forced. `--transitive` relaxes the default:

- **`--transitive hide`** — plan direct dependencies only. A re-lock can still move transitives; such moves stay visible as collateral rows.
- **`--transitive allow`** — still advance the graph, but leave a floated-up too-fresh transitive in place (reported, not rolled back).

## Lock edge bindings (cargo)

A `Cargo.lock` records not only which versions exist but which coexisting version each dependent's
edge is **bound** to (`dependencies = ["uuid 0.8.2"]`). When a crate declares a range wide enough to
admit two locked versions at once — diesel's `uuid = ">=0.7, <2.0"` beside both a `0.8` and a `1.x`
line — cargo's incremental re-resolves can silently rebind that edge between them. The rebinding is
build-affecting (the dependent compiles against the other copy) yet invisible at the version level,
and `cargo metadata --locked` accepts either binding. `--cargo-edge-policy` decides what
`upgrade`/`fix` do about it:

- **`preserve`** (default) — restore an addressable, unambiguous crates.io edge the re-resolve
  rebound between two still-coexisting versions when its earlier binding still satisfies the
  active requirement.
- **`canonicalize`** — cooldown's owned normalization: bind each addressable, unambiguous crates.io
  edge to the **highest** locked version satisfying the dependent's active requirement, preferring
  candidates whose declared `rust-version` is workspace-compatible and falling back to the
  highest satisfying candidate when none is compatible — a cooldown-owned conservative tier
  inspired by cargo's `incompatible-rust-versions = "fallback"` rule. Unlike
  `preserve` this also heals bad bindings that predate the run, including on a run that applies no
  version change at all. It is a policy over the existing package set, not a re-run of cargo's
  resolver: a workspace on resolver v1/v2 (default `allow`) may see a fresh cargo resolve bind a
  higher, MSRV-incompatible version where `canonicalize` keeps the compatible one.
- **`none`** — leave bindings exactly as the resolver produced them.

Every corrected, withheld, unaddressable, or surviving rebind is reported as its own row
(`restored`, `canonicalized`, `held`, `unaddressable`, or `rebound`) naming the dependent whose
edge moved. Observation excludes a dependent whose own lock identity changed, an unpaired entry
that appeared or vanished, and an endpoint that did not coexist in both snapshots; those are
package-set changes rather than attributable binding-only moves. The report is audited from the
run-start and final locks, so a temporary held attempt
that a later batch resolves does not fail `--strict`, and a correction later overwritten does not
remain `applied`. Corrections are applied as targeted lock edits and re-verified with
`cargo metadata --locked`. A concrete correction rejected by the orphan guard or verification is
`held`, with `to` naming the withheld target; when the resolver also moved that edge, a separate
`rebound` row records the committed move. If a renamed multi-version or source-qualified lock
entry moves but cannot be mapped safely to one declared requirement, it is `unaddressable` rather
than being mislabeled as an ordinary rebound. The JSON summary counts edge activity apart from
version changes (`edgesCorrected`, `edgesRebound`, `edgesHeld`, and `edgesUnaddressable`); each edge row's `applied`
says whether its binding outcome is present in the committed lock, while top-level `applied` says
whether cooldown wrote a mutation. Either `held` or `unaddressable` fails a `--strict` run because
the corrective policy is incomplete. The policy is cargo-specific, so its config placement is too:
set `edge-policy` under `[tool.cargo]` in the nearest applicable `cooldown.toml`; nearer project
config wins, then an explicit `--config`, and the CLI flag has highest precedence.

## Major versions

A cross-major bump is usually breaking work you opt into, so `--major` is **off** by default for `upgrade`. With `--major` it applies to every eligible dependency; narrow it to a subset with `--package`:

```bash
cooldown upgrade --major -p 'serde*'
```

When `upgrade` holds a cross-major update back, it explains the required action:

- `needs --major` means re-run with `--major`.
- `declared_bound_held` means pass `--rewrite` to cross and rewrite the manifest's explicit
  `<`/`<=` bound.
- `max_major_held` means raise the package's `max-major` in `cooldown.toml`; no CLI flag overrides
  this ceiling.
- `dist_tag_held` (npm family) means the target sits above the registry's current `latest`
  dist-tag, so it is not what a default `npm install` would resolve to today. Version-adopting
  commands (`upgrade`, `fix`) revalidate the package document against the registry so the mutable
  tag is judged live; read-only `outdated` may see a copy up to an hour old (`--fresh` forces
  live reads everywhere). Pass
  `--no-respect-dist-tags` (or set `respect-dist-tags = false` under `[global]` or a command
  section in `cooldown.toml`) to adopt it deliberately.
- `multi_version_held` (pnpm) means the workspace declares the package under ranges that do not
  all admit the target (`~7.3.0` beside `^7.0.0` for a `7.4.0` target, `^3.4.19` beside `^4.3.3`
  for a v4 one), so pinning every importer to it would drag the narrower one off its own declared
  range; each importer is kept on its own line instead. Only a range that provably *excludes* the
  target — or one cooldown cannot judge (a `||` union, a hyphen range, a dist tag) — holds: two
  importers on `^4.17.20` and `^4.17.21` both take `4.17.22`, and one range resolved at two
  versions converges on a target it admits. An importer the run excludes (`exclude-folders`,
  `exclude-packages`, a `-C` selection) contributes no range and no version to this judgment, so
  a copy cooldown was told to ignore cannot hold an update in an included importer; the report
  says when such a copy is left behind on its old version, or re-resolved within its own range
  because pnpm's update reaches every importer whose range admits a newer version. Pass `--rewrite`
  to converge a genuine
  split: every declaring importer's range is widened to admit the target and the name is pinned
  there. A name planned at two targets (a `^22` and a `^25` line each advancing within itself
  without `--major`) cannot be pinned by one joint update — even when one permissive range such as
  `>=22` admits both — and stays held until `--major` admits one line for every importer.
- `peer_held` (npm family) means a still-present dependent's recorded peer range excludes the
  target (`held: fumadocs-mdx@15.1.1 requires fumadocs-core@^16.0.0`) — pnpm's resolver only
  warns on that break, and npm, which rejects it by default (`ERESOLVE`), commits it under
  relaxed enforcement such as `legacy-peer-deps`. Upgrade the dependent itself. On pnpm both can
  move in one run: the joint move is the whole-graph resolve's decision, and cooldown re-checks
  the result — a landing that provably breaks a recorded peer contract *between packages your
  workspace itself declares, in a context that demonstrably binds them* (npm's hoisted tree is
  judged physically, pnpm importers by their own declarations) is rejected and rolled back.
  Contracts involving transitive packages, contexts that never bind the moved copy, or unprovable
  ranges stay the resolver's call, so still review its peer warnings on a joint move. npm applies moves one at a
  time with no joint resolve, so there the target stays held while the dependent moves — and the
  dependent's own landing is verified after the fact: if its *new* peer range provably excludes
  the still-held target (which relaxed enforcement like `legacy-peer-deps` would commit with only
  a warning), that move is rolled back too. A lockstep pair whose new versions admit only each
  other therefore stays held on both sides, each row naming the other; move such a pair in one
  command (`npm install react@19 react-dom@19`) and cooldown maintains it from there. A dependent
  whose new range still admits the current version lands normally, and the
  next run releases the hold. A *workspace-local* dependent
  (a `workspace:*` package, symlinked or injected — or the root project itself, which peer-requires
  the moving package) never moves in a run, and cooldown never rewrites its published contract, so
  editing its own `peerDependencies` range is what lifts that hold. Such a hold applies to
  same-major moves too, since a narrow author-written bound can exclude a patch bump. There is
  deliberately no flag to force the break. A `peer_held` run does not fall back to the newest in-range release in the
  same pass; a run without `--major` picks that up.

Only a matured release beyond the hold is reported. A fresh release still in cooldown does not
produce an action that cannot yet be taken. Suppress command tips with `--no-suggestions`.

## Flags

| Flag | Effect |
|---|---|
| `--transitive <mode>` | `allow` or `hide` — how to treat transitive dependencies (see above). |
| `--rewrite` | Always rewrite the manifest constraint; also the only way to cross an explicit `<`/`<=` bound, and (pnpm) to converge a workspace split onto one line. |
| `--cargo-edge-policy <policy>` | cargo: `preserve` (default), `canonicalize`, or `none` — how lock edge bindings are treated after the re-resolve (see above). Config: `[tool.cargo] edge-policy`. |
| `--build` | Also compile / sync after re-locking. |
| `--major` | Allow cross-major bumps; explicit manifest bounds, config `max-major` ceilings, and the npm `latest` dist-tag still hold. |
| `--no-respect-dist-tags` | Adopt npm-family releases above the `latest` dist-tag too. |
| `--strict` | Exit `1` if the mutation cannot complete cleanly. |
| `--fail-on-new-duplicate` | pnpm: refuse a resolve that gives *any* package a second resolved copy — the candidate whose landing added it is held with the copy and its requirer named and the lock restored — instead of committing it with a `duplicate_copy` warning. Config: `[global]`/`[upgrade] fail-on-new-duplicate`; `[tool.pnpm] single-copy` gates named packages on every run. On a tool without pnpm's settlement guard the flag has no effect and the run says so. |
| `--dry-run` | Resolve and print the plan; never mutate. |

`upgrade` always re-locks. Use `--dry-run` whenever you want to see the plan first; combine it with
`--json` to feed the plan into other tooling. The JSON envelope sets its top-level `dryRun` field
to `true` for a preview and `false` for a real mutation, independently of whether every proposed
item ultimately lands.
