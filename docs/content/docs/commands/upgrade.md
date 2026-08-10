---
title: upgrade
weight: 2
---

# `upgrade`

`upgrade` moves dependencies **forward** to the newest version that has already matured past its cooldown, then re-locks. It never adopts a too-fresh version — only versions that have cleared the window are proposed.

```bash
cooldown upgrade
```

Preview the plan without touching anything with `--dry-run`:

{{< terminal name="upgrade" >}}

The **From → To** columns show the move; a row can be a `downgraded` as well as an `upgraded` when a re-lock would otherwise pull a too-fresh transitive in and it has to be reconciled back down (see [Transitive dependencies](#transitive-dependencies)).

## Lock versus manifest

By default `upgrade` moves the **lock** within your declared version constraint and leaves the manifest alone: a `^1.4` stays `^1.4` while the lock advances to the newest matured `1.x`.

When the target falls outside an **implicit** constraint — most commonly a cross-major bump
(`--major`) past a caret or compatible-release range such as `^5` or `~=5.9` — `cooldown` rewrites
the one owning manifest entry so the version can be adopted, then re-locks. Edits are
**format-preserving** (comments, key order, and spacing are kept), and for a Cargo workspace an
inherited `dep = { workspace = true }` is widened in the root `[workspace.dependencies]`.

An explicit upper comparator written by the author, such as `<6` or `>=5,<6`, is different: it
holds under the default behavior, even with `--major`. Pass `--rewrite` to cross and rewrite that
bound. The same flag still always rewrites an in-range constraint (so `^1.4` becomes `^1.5`).

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
| `--rewrite` | Always rewrite the manifest constraint; also the only way to cross an explicit `<`/`<=` bound. |
| `--cargo-edge-policy <policy>` | cargo: `preserve` (default), `canonicalize`, or `none` — how lock edge bindings are treated after the re-resolve (see above). Config: `[tool.cargo] edge-policy`. |
| `--build` | Also compile / sync after re-locking. |
| `--major` | Allow cross-major bumps; explicit manifest bounds, config `max-major` ceilings, and the npm `latest` dist-tag still hold. |
| `--no-respect-dist-tags` | Adopt npm-family releases above the `latest` dist-tag too. |
| `--strict` | Exit `1` if the mutation cannot complete cleanly. |
| `--dry-run` | Resolve and print the plan; never mutate. |

`upgrade` always re-locks. Use `--dry-run` whenever you want to see the plan first; combine it with `--json` to feed the plan into other tooling.
