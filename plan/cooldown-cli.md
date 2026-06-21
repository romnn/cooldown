# `cooldown` — a unified, language-agnostic dependency-cooldown CLI

## Motivation

Supply-chain attacks on package registries overwhelmingly follow a smash-and-grab pattern: a
malicious version is published, and it is detected and yanked within hours to a few days. A
**cooldown** (a.k.a. minimum release age) is the cheapest, highest-leverage defense — refuse to
adopt any version younger than N days so the community's immune system runs before the code reaches
our builds. The risk surface is the **resolved lockfile** (direct _and_ transitive), because that is
what actually compiles, so the gate must reason over the whole graph.

The problem is that cooldown support today is **fragmented per ecosystem**, with a different name,
config surface, and UX in each:

| Ecosystem         | Native cooldown                                  | Notes                                                                                                                                            |
| ----------------- | ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| Python (uv)       | `exclude-newer` / `exclude-newer-package`        | both accept RFC3339, friendly duration, or ISO duration; `exclude-newer-package` also takes `false` (per-package exempt). PEP 700 `upload-time`. |
| Node (pnpm)       | `minimumReleaseAge` (+ exclude list)             | pnpm 10.16                                                                                                                                       |
| Node (yarn)       | `npmMinimalAgeGate` (minutes)                    | yarn 4.10.0                                                                                                                                      |
| Node (bun / deno) | `minimumReleaseAge` / `--minimum-dependency-age` | bun 1.3 / deno 2.6                                                                                                                               |
| pip               | `--uploaded-prior-to` (datetime or duration)     | pip 26.0; needs an index exposing upload-time metadata                                                                                           |
| Rust (cargo)      | **none native**                                  | `rust-lang/cargo#15973` open; third-party `cargo-cooldown`                                                                                       |
| Go                | **none**                                         | nothing in the toolchain or `go.mod`                                                                                                             |

`cooldown` is a **standalone, self-contained open-source tool**: a single binary that carries its
own full cooldown implementation for every ecosystem and needs **no cooldown helper tools** (it does
invoke the project's own `go`/`cargo`/`uv` binaries as resolution/apply engines, never as the source
of policy). It collapses the scattered, single-ecosystem tools people reach for today —
`gomajor`/`gomod-age`/`go-mod-outdated` for Go, `cargo-cooldown`/`cargo-outdated` for Rust, ad-hoc
shell gates, and each package manager's bespoke cooldown config — into one tool with one mental
model.

**Goal:** one tool — `cooldown` — that auto-detects the language(s) in a directory and exposes the
_same_ subcommands, flags, config, and (pretty + JSON) output for all of them. The cooldown verdict
is computed in **one core evaluator** for every ecosystem; native package managers are used only as
**resolution/apply engines**, never as the source of policy truth. Default to a 7-day cooldown;
opting out is explicit.

## Prior art (and why we still need this)

Checked before proposing — none of these is what we want:

- [cooldowns.dev](https://cooldowns.dev/) — documentation + a `cooldowns.sh` helper that
  _configures_ each package manager's native cooldown. No unified outdated/upgrade/check CLI, no
  auto-detection.
- [Renovate `minimumReleaseAge`](https://docs.renovatebot.com/key-concepts/minimum-release-age/) /
  Dependabot `cooldown` — multi-ecosystem, but they are bots/CI services, not a local CLI, and they
  don't give you a local `outdated`/`upgrade` flow.
- Native per-ecosystem features (table above) — fragmented; no shared UX or JSON.

Conclusion: the unified _local CLI_ niche is open.

## Goals / non-goals

**Goals**

- One self-contained binary, auto-detecting the ecosystem(s) from the working dir.
- Identical subcommands across languages: `outdated`, `upgrade`, `check`.
- Cooldown-aware everywhere: "outdated" means _adoptable now_, with in-cooldown versions shown
  separately, not conflated.
- Shared colorful TTY output **and** a stable, versioned `--json` with one envelope.
- One policy core: the cooldown verdict is computed identically for every ecosystem; package
  managers are resolution/apply engines only.
- Config: `cooldown.toml` is the policy surface and wins over native; native config is read as a
  compatibility input; env and CLI win over everything. Default **7d**; opting out is explicit
  (`--latest`).

**Non-goals (initially)**

- Being a package manager. We orchestrate the real tools (`go`, `cargo`, `uv`); we don't replace
  resolution.
- **Acting** on transitive dependencies. `check` _evaluates_ the full resolved graph (direct +
  transitive), but `upgrade` only **changes** direct deps. If a direct upgrade's re-lock would drag
  in a too-fresh (non-baselined, non-allowed) transitive, `upgrade` **does not commit that lock**:
  the app applies changes **one at a time** (single-change plans), checks the resulting graph, and
  on a fresh transitive **restores the lockfile snapshot** and skips that direct change as
  `SkipReason::TransitiveInCooldown` — never writing a violating lock (see § upgrade).
- Every ecosystem on day one — MVP is Go + the policy core; Rust, Python, Node, and
  `sync`/advisory-bypass come after (§ Phasing).

## CLI design

The same verbs work in every ecosystem; everything else is a flag.

### Subcommands

```text
cooldown outdated       what could update — split into "adoptable now" vs "in cooldown"
cooldown upgrade        move to the newest version older than the cooldown; always re-locks, --build to compile
cooldown check          exit non-zero if anything resolved is younger than the cooldown (CI gate)
cooldown baseline       record currently-young deps as acknowledged, so `check` can be adopted cleanly
cooldown explain <pkg>  why <pkg> has the window it has — every layer and rule that applied (alias: why)
cooldown config         the fully-resolved config, with the origin of each value
cooldown init           scaffold a documented starter cooldown.toml (refuses to clobber)
cooldown schema         print the machine-readable JSON schema for `--json` output
cooldown sync           write the resolved policy down into native configs (opt-in; later phase)
```

`outdated` / `upgrade` / `check` are the daily drivers. `explain` and `config` keep the override
system from being a black box — that auditability is what makes a powerful config feel _safe_.

### Global flags

**Policy** flags (`--min-age`, `--latest`, `--freeze`, `--min-age-{major,minor,patch}`, `--allow`)
have a matching config key and a `COOLDOWN_*` env var; flags win. **Invocation** flags (`--dry-run`,
`--package`, `--lang`, `--direct-only`, `--include-indirect`, `--allow-stale-lock`,
`--fresh`/`--no-cache`, `--offline`, `--config`, `-C`, `--json`) are per-run controls with a
`COOLDOWN_*` env var for CI but **no config-file key**.

```text
--min-age <dur>            window: "7d", "2 weeks", "36h", ISO-8601 "P7D" (default 7d)
--latest                   opt OUT (window = 0) — the explicit, audited escape hatch
--min-age-major <dur>      per-kind windows (also --min-age-minor, --min-age-patch)
--major                    candidate filter: allow major version changes (default: within current major;
                           for upgrade/fix applies to every eligible dep unless narrowed with --package)
--freeze <date>            absolute cutoff instead of a rolling window (reproducible)
--package <glob>           scope the command to matching packages (repeatable)
--allow <glob>             exempt matching packages from the cooldown (repeatable, audited)
--lang <name>              restrict to ecosystem(s); repeatable / comma-separated (default: all detected)
--direct-only              evaluate only direct deps (default for check is the full graph)
--all-artifacts            (check) gate every artifact in a universal lock, not just env-relevant ones
--include-indirect         (outdated only) include transitive deps in the report (default: direct)
--allow-stale-lock         downgrade a stale/absent lock from failure (the default) to a warning
--dry-run, -n              resolve and print the plan; never mutate
--offline                  cache only; cache misses become UnknownAge (never a false "ok")
--no-cache / --fresh       ignore the local cache; always hit the registry (use in CI gates)
--fail-on-unknown-age      make `check` fail (not just warn) on deps with no publish time
--fail-on-stricter-native  make `check`/`config` fail when repo policy overrides a stricter native value
--no-fail-on-stricter-native  override a config-set `strict-native` (the only way to turn it off)
--strict                   (upgrade) fail (exit 1) if any planned change was skipped (MVS/resolver conflict)
--build                    (upgrade) also compile/sync after re-locking (off by default; may be expensive)
--no-native / --no-global  ignore a config layer (reproducibility / debugging)
--config <path>            load one extra, highest-precedence file layer (still below env/flags)
-C, --dir <path>           run as if from <path>
--json                     machine-readable output (never changes the exit code)
```

`--latest`, `--freeze`, and `--min-age` are **mutually exclusive** (clap error → exit 2). `--latest`
is the single opt-out spelling (`--no-min-age` is a hidden alias). A no-match `--package`/`--lang`
on a mutating or `explain` command is exit 2.

### Exit codes

`check` is the CI gate, so non-zero is its contract. The taxonomy is shared and independent of
`--json` (which only changes format):

| Code | Meaning                                                                                                                                                    |
| ---- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0    | clean / nothing to do                                                                                                                                      |
| 1    | policy violation — `check` found deps younger than their window (and not baselined); or `upgrade --strict` left a planned change unapplied                 |
| 2    | usage / config error — bad duration, unknown `--lang`, mutually-exclusive flags, no-match `--package`, parse error, or `--fail-on-stricter-native` tripped |
| 3    | no ecosystem detected                                                                                                                                      |
| 4    | stale/absent lock (unless `--allow-stale-lock`); registry unreachable; `uv`/`go`/`cargo` failed; or (with `--fail-on-unknown-age`) an unknown-age dep      |

By default `check` _surfaces_ UnknownAge but doesn't fail on it; `--fail-on-unknown-age` flips that.
`upgrade` defaults to _succeed with a report of skips_; `--strict` makes an unmovable planned change
exit 1.

### How `--major`, per-kind windows, and scope compose

Shared vocabulary, different jobs:

- `--major` is a **candidate filter** — which jumps are eligible (default: within the current
  major).
- `min-age.major` / `--min-age-major` is the **window** applied to a jump already _classified_ as
  major.
- `--package` / `--lang` are **command scope** and are orthogonal to the `[package.*]` / `[lang.*]`
  _policy_ selectors.

Cross-major Go upgrades rewrite import paths repo-wide, so bare `cooldown upgrade --major` (no
`--package`) applies to **every** eligible dependency; narrow it with `--package` to take only a
subset. `upgrade --include-indirect` is an error (acting on transitive is a non-goal).

### `check` semantics (precise)

`check` is the security gate; its behavior is fully specified so all ecosystems mean the same thing:

- **Unit of evaluation:** the resolved pins actually built — `Cargo.lock` and `uv.lock` for Rust /
  Python, and for Go the resolved module graph (`go list -m all`, with `go.mod` as the version
  source and `go.sum` as integrity metadata, _not_ a lockfile). Not the manifest constraints.
  `outdated`/`upgrade` reason over manifest constraints to _propose_ targets; `check` judges what is
  _resolved_.
- **Lock must be current (fail-closed).** A lock missing or stale relative to its manifest makes
  `check` meaningless, so `check` verifies it first via a concrete per-ecosystem probe — Go
  `go mod tidy -diff` (expect no diff), Rust `cargo metadata --locked` (errors if the lock would
  change), uv `uv lock --check` — and **fails by default (exit 4)** if it is stale or absent.
  `--allow-stale-lock` downgrades that to a warning for local/adoption use. Separate from `--fresh`,
  which is only registry-cache bypass.
- **Scope:** the full graph (direct + transitive) by default; `--direct-only` for a fast path. The
  floor applies to transitive too. For a universal lock with multiple artifacts per pin, `check`
  gates the **environment-relevant** artifacts (conservative: newest-relevant upload time);
  `--all-artifacts` gates every recorded artifact.
- **Freshness (the gate):** the pin's metadata comes from `locked_release(dep, ctx)` — its `quality`
  (pseudo/yanked/stable) and the publish instant of its locked artifacts. A locked version younger
  than its resolved window is a **violation** (status `CurrentInCooldown`) — **regardless of why it
  is pinned** — unless baselined, matched by an `allow`, or covered by `--latest`; a `None` publish
  instant is `unknown_age`; a yanked pin is surfaced as a warning. There is no implicit pass.
- **Graph-held is a diagnostic, not an exemption.** A too-fresh dep the graph requires newer (Go
  MVS, a Cargo `=` pin) still **fails** `check`; the violation is annotated `graphHeld: true` (+
  `graphFloor`) to say `upgrade` cannot downgrade it, so you baseline/allow it deliberately.
  (`upgrade` reports the same as `SkipReason::GraphHeld`.)
- **Unknown age:** surfaced as `UnknownAge`, never a silent pass; fails only under
  `--fail-on-unknown-age`.
- **Pseudo-versions / commit pins:** exempt (no tagged version to quarantine against); counted in
  the `exempt` tally.
- **Exit:** `1` if any non-baselined, non-allowed violation remains; `4` for a stale/absent lock
  (unless `--allow-stale-lock`) or unknown-age under `--fail-on-unknown-age`; else `0`.

### Baseline (adopting the gate without noise)

Turning on a full-graph `check` in an existing repo would immediately flag every pre-existing young
transitive — a behavioral break from today's direct-only Go script. `cooldown baseline` writes a
committed `.cooldown-baseline.toml` recording the currently-young deps as **acknowledged**. Each
entry is **fully scoped** — `(ecosystem, project/lockfile, package, version, registry)` — so the
same young version reintroduced in another project later is _not_ silently grandfathered:

```toml
# .cooldown-baseline.toml — generated by `cooldown baseline`; review in PRs
[[acknowledged]]
ecosystem    = "go"
project      = "services/api"            # the lockfile/project this pin belongs to
package      = "k8s.io/api"
version      = "0.36.2"
registry     = "proxy.golang.org"
published-at = "2026-06-12T12:38:38Z"    # recorded for audit + drift detection
window-days  = 14                        # resolved window at acknowledgement time
reason       = "graph-held; cannot downgrade"
until        = "2026-08-01"              # optional expiry
```

`check` acknowledges an entry only on an **exact**
`(ecosystem, project, package, version, registry)` match (reported under a distinct `acknowledged`
count, not a violation) until the `version` changes or `until` passes — a clean ratchet: baseline
once, then the set only shrinks. `cooldown baseline --prune` drops entries whose version has aged
past the **currently resolved** window (not the recorded `window-days`, which is kept for
audit/drift) or is no longer present.

### Environment variables

**Policy** env vars mirror the config keys: `COOLDOWN_MIN_AGE=14d`, `COOLDOWN_MIN_AGE_MAJOR=30d`,
`COOLDOWN_LATEST=1`, `COOLDOWN_FREEZE=…`, `COOLDOWN_ALLOW=…`. **Invocation** env vars are CI
conveniences with **no config-file key**: `COOLDOWN_LANG=go`, `COOLDOWN_DRY_RUN=1`,
`COOLDOWN_ALLOW_STALE_LOCK=1`, `COOLDOWN_OFFLINE=1`, `COOLDOWN_CONFIG=/etc/cooldown.toml`.

### The happy path is zero config

```bash
cooldown outdated      # auto-detects Go/Rust/Python/Node; 7-day window
cooldown upgrade       # adopt everything >= 7 days old, re-lock to verify (--build also compiles)
cooldown check         # CI gate over the resolved lockfile graph
```

Raising the whole repo to 14 days is one line of `cooldown.toml`. Everything past that is opt-in.

## Configuration (TOML)

### Discovery

- **Global** — `${XDG_CONFIG_HOME:-~/.config}/cooldown/config.toml`.
- **Repo / project** — the cascade is computed **per detected project**: every `cooldown.toml` from
  the repo root down to **that project's directory**, merged so a **nearer file wins** (like
  `.editorconfig`). This is the load-bearing detail for root-run monorepos — each project sees its
  own child `cooldown.toml`, not just the root's. The walk auto-stops at the repo root, resolved
  robustly: a `.git` _directory_, a `.git` _file_ (worktrees/submodules — follow to the real
  worktree root), else the nearest ancestor with a `cooldown.toml`, else `$HOME`. `cooldown config`
  prints the detected root (misdetecting it would silently drop an org floor).
- **Native** — each project's own manifest config, read in place, normalized once by the core, and
  scoped to **that project** (never merged into a shared native layer, so one project's native
  default can't leak into a sibling).
- `--config`/`COOLDOWN_CONFIG` add one **shared** file as the top file layer — above the repo
  cascade, below env/CLI — applied to every project in the run.

### One schema, used everywhere

The global file and every `cooldown.toml` share this schema. `min-age` is either a **duration
scalar** (the simple case) or a **table** of per-kind windows — never both in one file (that would
be invalid TOML). The top level is the default; tables narrow it by selector.

```toml
min-age = "14d"                 # the one knob most repos ever set (scalar form)

# per-kind windows use the TABLE form of min-age instead of the scalar above:
#   min-age = { default = "14d", major = "30d", minor = "14d", patch = "7d" }
# (env/CLI: min-age.major <-> --min-age-major <-> COOLDOWN_MIN_AGE_MAJOR)

[lang.python]                   # per ecosystem
min-age = "21d"
[lang.go]
min-age = "14d"

[registry."internal.acme.io"]   # per registry / index
min-age = "0d"                  # our own registry — trusted

[package."github.com/acme/*"]   # per package (glob) — most specific
min-age = "0d"
[package."left-pad"]
min-age = "30d"

[project."packages/python/*"]   # per subtree, from a repo-root file
min-age = "21d"

allow = ["acme/*", "internal-*"]  # exemption set — ACCUMULATED across layers (see resolution)
floor = "3d"                      # hard minimum, MAX-clamped across layers (a separate mechanism)
strict-native = true              # treat repo overriding a STRICTER native value as an error, not a warning
```

`latest = true` is sugar for `min-age = "0d"`; `freeze = "2026-06-01"` pins an absolute cutoff
(reproducible) instead of a rolling window. Durations accept `"7d"`, `"2 weeks"`, ISO-8601 `"P7D"`.
The TOML table form maps to the internal `min-age = { default, major, minor, patch }` (each
optional; `default` is the bare window). Within any single selector/rule, `latest`, `freeze`, a
scalar `min-age`, and the `min-age` table are **mutually exclusive** — a config-validation error
(exit 2), the same rule the CLI enforces for `--latest`/`--freeze`/`--min-age`.

### Precedence & resolution — _authority-first_

Two orthogonal axes. **Layers** (where a value comes from), low → high:

1. built-in default (`min-age = 7d`)
2. global user config
3. **native** manifest config (read; normalized) — beats the built-in / global default
4. repo / project `cooldown.toml` cascade (nearer file wins)
5. an explicit `--config` / `COOLDOWN_CONFIG` file — a **shared** top file layer (one per run)
6. `COOLDOWN_*` environment variables
7. CLI flags

**Selectors** (what a value applies to), most → least specific: `package` > `registry` > `project` >
`lang` > top-level default.

> **Resolution is per field, and each field has its own combine rule** (not a single
> winner-takes-all):
>
> - **`min-age` / per-kind windows — authority-first.** The winner is the highest **layer** that
>   sets it; _within_ a layer, the most specific **selector** breaks the tie. Layer dominates
>   selector. (Worked: a global `[package."left-pad"] min-age = "30d"` (specific, layer 2) **loses**
>   to a repo top-level `min-age = "14d"` (general, layer 4) → 14d. A less-authoritative layer
>   cannot override repo policy by being more specific.) Per-kind fallthrough: for a `Candidate(K)`
>   the window is the highest/most-specific rule that sets `min-age.K`, else that scope's bare
>   `min-age`; `CurrentPin` (the `check` gate) always uses the bare `min-age` — an already-locked
>   version has no from→to kind.
> - **`floor` — max-clamp.** The effective window clamps **up** to `max(floor)` over _all_ layers.
>   Floors only ratchet stricter; no layer can lower another's floor. As an absolute cutoff:
>   `effective_cutoff = min(selected_cutoff, now − max_floor)` — where `selected_cutoff` is the
>   `freeze` date or `now − min-age` — so a floor still tightens even a frozen policy.
> - **`allow` — accumulated union.** Exemptions from every layer are unioned (a lower layer's
>   `allow` still exempts against ordinary windows). But an `allow` entry **bypasses a floor only if
>   declared at the floor's layer or higher**; a CLI `--latest`/`--allow` is always honored and
>   always audited. So a repo `allow = ["*"]` cannot undercut an _org_ (global) floor.
> - **`strict-native` — security-monotone.** True if **any** config layer sets it; no layer can turn
>   it off — the only override is an explicit CLI `--no-fail-on-stricter-native`.

This is **authority-first precedence**: the most authoritative layer wins for scalar policy, while
security-monotone fields (`floor`, `allow`-vs-floor) can only tighten. A **precedence-matrix test
suite** pins the cross product of {layer × selector × field} so the rule can't silently drift.

**Stricter native than repo.** Because layers dominate, a repo `min-age = "14d"` also overrides a
_stricter_ native value (e.g. a uv project pinned to 30d). That is intended — `cooldown.toml` is the
policy surface — but it weakens the project's stated intent, so `cooldown` **warns** by default and
**fails** under `strict-native = true` / `--fail-on-stricter-native`. The clean fix is to move the
stricter value into a nearer `cooldown.toml` (which wins via the cascade).

`cooldown explain <pkg>` prints the field-by-field derivation; golden `explain` traces are part of
the test suite so the resolution semantics are pinned by example.

> **Decision (locked): `repo cooldown.toml` > `native manifest` > `global`.** Reading native config
> means adopting `cooldown` never _regresses_ an existing `exclude-newer`; un-migrated projects keep
> their declared window — so native beats the built-in/global default. But `cooldown.toml` is the
> policy surface and outranks native, because in a monorepo one project's manifest must not silently
> _weaken_ the org/repo baseline. A sub-project that needs something different states it in a nearer
> `cooldown.toml`; an org-wide minimum no repo may weaken is a `floor` in the global config.
> _Rejected — native > repo_ ("most specific wins"): lets any project loosen the baseline, the exact
> weakening we defend against
> ([`astral-sh/uv#19408`](https://github.com/astral-sh/uv/issues/19408)).

### Optional: sync into native configs (later phase)

`cooldown sync` (opt-in, post-MVP) writes the resolved policy _back down_ into each native config so
the package managers also enforce it when run directly. Notes:

- **uv:** write a **relative span** (`exclude-newer = "14 days"`, recorded as `exclude-newer-span`),
  never a resolved timestamp, so `uv lock --check` stays stable. `exclude-newer-package` _does_
  accept per-package durations and `false`, so per-package windows and exemptions sync faithfully;
  only **per-kind** (uv has no major/minor/patch axis) and **per-registry** windows are
  inexpressible — `sync` flattens per-registry to its member packages where it can, else
  warns/refuses.
- `cooldown` is **self-contained**: it reads PyPI publish times and owns the uv field on `sync`
  itself. If some other tool in a project already manages `exclude-newer`, `cooldown sync` can be
  told to read-only **defer** to it (`sync.defer-to = "<tool>"`) to avoid two writers — a config
  option, never a dependency.

## Production scenarios

> These illustrate the **end state** across all supported ecosystems; the MVP delivers Go first (§
> Phasing), and a Go-only project behaves identically using just the Go-relevant rows.

### 1. Solo service — zero boilerplate

No config files. `cooldown outdated` / `upgrade` / `check` — three verbs, 7-day window, nothing to
learn.

### 2. A polyglot monorepo — one line

`./cooldown.toml`:

```toml
min-age = "14d"
```

A repo with Go, Rust, Python, and Node modules. Every ecosystem now uses 14d (over the 7d built-in);
a Python project's native `exclude-newer` is read, but the repo's 14d wins. CI runs a single
`cooldown check`, and whatever per-ecosystem update scripts existed collapse to the same three
verbs.

### 3. Risk-tiered windows + per-ecosystem trust

```toml
min-age = { default = "7d", patch = "3d", minor = "7d", major = "30d" }

[lang.node]
min-age = "21d"          # npm is the most-attacked registry
[lang.python]
min-age = "21d"
```

`cooldown outdated` evaluates each candidate against the window for _its_ kind: a `lodash` patch is
adoptable at 3d while `cooldown outdated --major` keeps a `lodash` major "in cooldown" until 30d
(the verdict is per candidate).

### 4. Product company — first-party is trusted

```toml
min-age = "14d"
[registry."npm.acme.internal"]
min-age = "0d"
[package."@acme/*"]
min-age = "0d"
[package."github.com/acme/*"]
min-age = "0d"
```

A freshly published `@acme/ui` is adoptable immediately (more specific selector, same layer);
`react` waits 14d.

### 5. Org-wide floor a repo can't weaken

Global `~/.config/cooldown/config.toml` (org-managed): `floor = "7d"`, `allow = ["@acme/*"]`. A repo
sets `[package."some-tool"] min-age = "0d"`. `cooldown explain some-tool` → **7d**: the repo's 0d is
clamped by the global floor, and the repo's `allow` sits below the floor's layer so it can't bypass
it. A dev can still take it once with an audited `cooldown upgrade --package some-tool --latest`.

### 6. Take a CVE fix that is inside the window

`cooldown check` runs beside `govulncheck` / `cargo audit` in CI. A fix lands two days ago — inside
the 14d window — so cooldown holds it while the scanner flags the unpatched CVE. Resolve it
deliberately:

```bash
cooldown upgrade --package golang.org/x/crypto --latest   # audited, one-off
```

or a reviewed config exception
(`[package."golang.org/x/crypto"] min-age = "0d"  # CVE-2026-xxxx; PR #123`). `check` evaluates the
_currently pinned_ version's freshness against the bare scope `min-age`. (Automatic advisory-driven
bypass is a later phase — § Later work.)

### 7. CI stricter than local; a sandbox opts out

Nightly CI: `COOLDOWN_MIN_AGE=21d cooldown check --fresh` (never trust a stale local cache). A
throwaway `playground/cooldown.toml` with `latest = true` skips the wait — a nearer file wins over
the root's 14d (still subject to any org floor).

## JSON output

Every subcommand takes `--json` and emits **one common envelope**, identical in shape across
ecosystems and commands — one parser for everything. `cooldown schema` prints the machine-readable
schema.

```json
{
  "schemaVersion": 1,
  "command": "outdated",
  "ok": true,
  "generatedAt": "2026-06-17T13:00:00Z",
  "summary": { "...": "command-specific counts" },
  "items": ["...command-specific item objects..."],
  "warnings": [
    {
      "kind": "yanked",
      "ecosystem": "python",
      "project": "services/api",
      "package": "x",
      "version": "1.2.0",
      "registry": "pypi",
      "message": "locked version is yanked"
    }
  ],
  "errors": [
    {
      "kind": "stale_lock",
      "ecosystem": "go",
      "project": "services/api",
      "path": "go.mod",
      "message": "lock is stale; run `go mod tidy`"
    }
  ]
}
```

- `command` ∈ the subcommand name; `ok` mirrors the exit code (`true` ⇔ 0).
- `summary` and the `items[]` element shape are command-specific but documented; `warnings`/`errors`
  are always present (a partial failure is reported, not silently dropped). Both arrays hold the
  same **`Diagnostic`** shape:
  `{ kind, message, ecosystem?, project?, package?, version?, registry?, tool?, path? }` —
  `kind`/`message` are required, the rest populated when applicable (`tool` for `tool_failed`,
  `path` for `stale_lock`/`lockfile_unreadable`, `project`/`registry` so a consumer can map back to
  a baseline key). `kind` ∈
  `transient | not_found | unknown_age | stricter_native | yanked | stale_lock | tool_failed | lockfile_unreadable`.
- **Where errors live (exactly one place each).** A failure attributable to one dependency is an
  `items[]` entry with `status:"error"` (carrying its `kind`); a failure _not_ attributable to a
  single dependency — a whole-index outage, an unreadable lockfile, a failed `go`/`cargo`/`uv`
  invocation — is a top-level `errors[]` entry. Nothing is duplicated, so consumers never
  double-count.
- **Partial-failure rule.** `check` is **fail-closed**: any `items[]` finding with `status:"error"`
  **or** any top-level `errors[]` entry forces `ok:false` and **exit 4** — you cannot certify what
  you couldn't evaluate. On `outdated`/`explain` both are informational and don't change the exit.
  `check.summary.errors` counts both (they are disjoint), alongside `unknownAge` and `violations`.

**Stability policy:** SemVer-style — additive fields don't bump `schemaVersion`; a
removal/retype/semantic change does; consumers ignore unknown fields; the `status` and
`minAgeSource` enums are part of the contract. Conventions: RFC3339 UTC timestamps; ages/windows are
float **days**, display-only (the boundary comparison is on the underlying instant); `minAgeSource`
is `<origin>` or `<origin>:<selector>` (origin ∈
`default|global|native|repo:<path>|config:<path>|env|cli`).

**`items[]` per command:**

- `outdated` —
  `{ name, ecosystem, project, registry, direct, current, window:{minAgeDays,source,clampedBy}, status, adoptableTarget, latest:{version,publishedAt,ageDays}, error?:Diagnostic }`
  (`error` present iff `status:"error"`). `status` ∈
  `up_to_date | adoptable | in_cooldown | exempt | held | current_in_cooldown | unknown_age | error`.
  `adoptableTarget` is named to avoid colliding with `status:"adoptable"`.
- `check` — **findings**
  `{ name, ecosystem, project, registry, direct, current, publishedAt, ageDays, window, status, graphHeld:bool, graphFloor?, error?:Diagnostic }`
  where `status` ∈ `violation | acknowledged | unknown_age | error`; `summary` =
  `{ checked, direct, exempt, acknowledged, unknownAge, errors, violations }`; top-level `scope` ∈
  `lockfile-graph | direct-only` and `artifactScope` ∈ `environment | all` (per `--all-artifacts`).
- `upgrade` / `--dry-run` — items
  `{ name, ecosystem, project, registry, from, to, kind, applied:bool, skipped?:{reason,message,offending?}, error?:Diagnostic }`;
  top-level `applied:bool`, `lockVerified:bool|null` (re-lock result; `null` for `--dry-run`, which
  never mutates), `build:{requested:bool, ok:bool|null}` (`--build`).
- `explain` — items are trace steps `{ layer, field, selector?, minAgeDays, applied, note }`;
  top-level `project`, `registry`, `effective:{minAgeDays,decidedBy}`.

## Architecture

The **design** is ports-and-adapters (hexagonal): a pure policy core, an
`Ecosystem`/`PackageRegistry` port pair, per-ecosystem adapters, shared registry/ HTTP plumbing,
presentation, and a CLI composition root. **One rule:** dependencies point inward at the core, which
does no concrete I/O. The cooldown verdict lives in the core for every ecosystem; adapters are I/O
shims.

**For the MVP, this materializes as a single crate `cooldown`** (the standalone published binary)
with internal **modules** mirroring those seams — the trait boundaries are real (so logic doesn't
leak and adapters can't diverge), but they're module boundaries, not crate boundaries, on day one:

```text
cooldown/ (one crate, [[bin]] name = "cooldown")
  core/        domain model · evaluate() · resolve() · ports (Ecosystem, PackageRegistry) · CoreError   (no I/O)
  registry/    shared HTTP client · on-disk cache · per-host concurrency/backoff
  adapters/
    go/        Ecosystem + GOPROXY registry  (MVP)
    cargo/     Ecosystem + crates.io index   (later)
    uv/        Ecosystem + PyPI (PEP 700)     (later)
    npm/       Ecosystem + npm registry       (later)
  render/      TTY tables + the JSON envelope
  app/         the use cases (Workspace: outdated/upgrade/check/explain) over the ports
  main.rs      clap · config discovery/loading · wiring · dispatch
```

**Later, once the seams survive real use and we want to publish reusable library crates for
embedding,** the modules extract mechanically into a workspace: `cooldown-core`,
`cooldown-registry`, `cooldown-render`, and the adapter crates **`cooldown-go` / `cooldown-cargo` /
`cooldown-uv` / `cooldown-npm`** (named by each ecosystem's canonical tool/registry — unambiguous,
no "Rust edition" confusion), plus the `cooldown` binary. The extraction is trivial because the
seams already exist; we don't pay the 8-crate tax before the design is proven.

### Domain model (core)

Versions are **opaque to the core.** Go pseudo-versions, `/vN` majors, `+incompatible`, PEP 440 and
semver share no parse rules, so the core never parses a version — the ecosystem hands back releases
already classified, carrying an opaque ordering token and the update-kind relative to the current
pin.

```rust
pub struct Version(String);        // canonical display form; Eq + Display
pub struct MajorKey(String);       // opaque "same major?" token; compared for EQUALITY only
pub struct ReleaseOrder(Vec<u8>);  // opaque total-Ord token, meaningful only within one package
pub struct PackageId { pub ecosystem: EcosystemId, pub name: String, pub registry: Option<String> }

pub enum ReleaseQuality { Stable, Prerelease, Pseudo, Incompatible }  // Incompatible is adoptable; Prerelease excluded unless the current pin is itself a prerelease

pub struct Release {
    pub version: Version,
    pub order: ReleaseOrder,                    // core sorts/compares with this (debug_assert sortedness)
    pub major: MajorKey,
    pub kind_from_current: Option<UpdateKind>,  // ecosystem-classified jump vs the current pin
    pub published_at: Option<jiff::Timestamp>,  // aggregate over the selected artifacts (env-relevant, else all): newest upload, but None if ANY selected artifact's time is unknown (conservative); None => unknown age, never mature
    pub yanked: bool,
    pub quality: ReleaseQuality,
}

pub struct ArtifactId(String);   // a non-empty id for one locked artifact (a uv wheel/sdist); version-granular ecosystems leave `artifacts` empty
// a universal lock can record several artifacts per version; the adapter fills the env-relevant set
pub struct Dependency { pub package: PackageId, pub current: Version, pub current_quality: ReleaseQuality, pub direct: bool, pub artifacts: Vec<ArtifactId>, pub graph_floor: Option<Version> }
// current_quality lets `evaluate` apply the prerelease rule in core; INVARIANT: == locked_release(dep,ctx).quality (adapter derives both from the same lock entry)
// graph_floor: the lowest version the resolved graph permits for this package (MVS floor / `=` pin),
// read from the lock; `check_pin` sets PinVerdict.graph_held when a too-fresh pin sits at that floor.
pub enum UpdateKind { Major, Minor, Patch }    // Copy + Eq (no Ord)
pub enum Status {   // graph-held is NOT a status: it's a `graph_held` flag on a CurrentInCooldown violation
    UpToDate, Adoptable, InCooldown, Exempt, Held, CurrentInCooldown, UnknownAge,
}
```

`MajorKey` gates `--major` (same-major vs not) by equality only; the minor/patch distinction comes
from `kind_from_current`. The verdict is **per candidate** (a patch can be adoptable while a major
still cools):

```rust
pub struct Candidate { pub version: Version, pub kind: UpdateKind, pub window: ResolvedWindow, pub status: Status }
pub struct Verdict { pub status: Status, pub adoptable_target: Option<Version>, pub latest: Option<Version>, pub candidates: Vec<Candidate> }
```

The cooldown decision lives in **two pure functions** — the single source of truth for every
ecosystem, including Python. Both take the _unresolved_ layers (the window depends on each version's
kind/registry) and resolve internally via `resolve`. `evaluate` drives `outdated`/`upgrade` over the
candidate set; `check_pin` is the gate over the currently-locked release (from `locked_release`):

```rust
pub fn evaluate(dep: &Dependency, releases: &[Release], layers: &[PolicyLayer],
                ctx: &ResolveContext, now: jiff::Timestamp) -> Verdict;          // candidates
pub fn check_pin(dep: &Dependency, locked: &Release, layers: &[PolicyLayer],
                 ctx: &ResolveContext, now: jiff::Timestamp) -> PinVerdict;      // the locked pin (check)
// resolves with ResolveKind::CurrentPin (bare min-age); graph_held/graph_floor come from dep.graph_floor
// PinVerdict { status, window: ResolvedWindow, graph_held: bool, graph_floor: Option<Version> }
```

No concrete I/O, no clock, no version parsing — truth-table tests over both (fresh stable →
`InCooldown`; pseudo → `Held`/exempt; `None` publish → never mature; yanked never adoptable;
prereleases excluded unless the current pin is itself a prerelease; current pin younger than window
→ `CurrentInCooldown` violation; a graph-pinned fresh pin is that same violation with a `graph_held`
annotation, never a pass; downgrades not gated).

#### Supporting types

```rust
pub struct EcosystemId(&'static str);   // Copy + 'static; registered by the adapter
pub struct Project { pub root: Utf8PathBuf, pub kind: EcosystemId, pub manifest: Utf8PathBuf }  // Clone
pub enum DepScope { Direct, Graph }     // Graph = full resolved lockfile
pub struct Change { pub package: PackageId, pub from: Version, pub to: Version, pub kind: UpdateKind }
pub struct Plan { pub changes: Vec<Change> }
pub struct Skipped { pub package: PackageId, pub reason: SkipReason }   // GraphHeld, TransitiveInCooldown, ResolverConflict, …
pub struct ApplyReport { pub applied: Vec<Change>, pub skipped: Vec<Skipped> }   // skips are Ok DATA, not Err
pub enum ArtifactScope { Environment, All }     // from --all-artifacts
pub struct Environment { /* platform / abi / python-version / markers a lock must satisfy */ }
pub struct TargetContext<'a> { pub project: &'a Project, pub environments: &'a [Environment], pub artifacts: ArtifactScope }
pub struct VerifyReport { pub ok: bool, pub detail: String }
pub struct PolicyLayer { pub origin: Origin, pub rules: Vec<Rule>, pub strict_native: Option<bool> }  // a config layer may set strict-native
pub struct PolicyStack { pub layers: Vec<PolicyLayer>, pub strict_native: bool }  // strict_native = monotone OR across layers (CLI --no-fail-on-stricter-native forces off) — carried so loading can't drop it
pub enum Origin { Default, Global, Native, Repo(Utf8PathBuf), Config(Utf8PathBuf), Env, Cli }  // Config = explicit --config file
pub struct Resolution { pub window: ResolvedWindow, pub trace: Vec<TraceStep> }
pub struct ResolvedWindow { pub spec: WindowSpec, pub decided_by: Origin, pub clamped_by: Option<Origin> }

pub type Result<T, E = CoreError> = std::result::Result<T, E>;
#[non_exhaustive]
pub enum CoreError { NotFound, Transient(BoxError), Tool { tool: String, status: i32 }, Parse(String) /* … */ }
impl CoreError { pub fn is_transient(&self) -> bool; }   // retry classification, kept separate from display
```

Adapter-internal errors (`reqwest`, `io`, `serde`) convert into `CoreError` at the port boundary via
`From`. Non-fatal apply skips are `Ok(ApplyReport { skipped })` data; only genuine I/O/tooling
failures are `Err`.

### Ports (traits)

**`Ecosystem`** — the one port the use cases speak to. Capabilities, not opinions: it reads state,
yields classified releases, and executes changes; it never decides the cooldown (the core does) and
never builds a `Rule`/`WindowSpec` (window normalization happens once, in core). Object-safe via
`async_trait` (deliberate: the adapter set is tiny and built once; `AFIT` alone isn't
`dyn`-compatible and we want `Box<dyn Ecosystem>`; `trait-variant` is the lighter alternative if
desired).

```rust
#[async_trait]
pub trait Ecosystem: Send + Sync {
    fn id(&self) -> EcosystemId;
    fn capabilities(&self) -> Capabilities;   // has_pseudo, has_incompatible, has_dist_tags, can_sync …
    async fn detect(&self, root: &Utf8Path) -> Result<Vec<Project>>;
    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>>;
    /// Classified candidate releases (order + kind_from_current + publish times), via the registry.
    /// `ctx` supplies the project + target environment + artifact scope, so each candidate's publish
    /// instant follows the candidate invariant (newest env-relevant artifact, else newest of all).
    async fn releases(&self, dep: &Dependency, ctx: &TargetContext) -> Result<Vec<Release>>;
    /// The CURRENTLY-LOCKED version as a Release: its `quality` (= `dep.current_quality`; stable/pseudo/yanked/incompatible)
    /// and the publish instant of its locked artifacts (newest, per `ctx` scope; `None` => unknown
    /// age). This is what `check` evaluates for the pin — distinct from the candidate set above.
    async fn locked_release(&self, dep: &Dependency, ctx: &TargetContext) -> Result<Release>;
    /// Native cooldown config translated into the unified rule model — per-package windows,
    /// exemptions, and exclude lists included. Each rule's window stays RAW so the core normalizes
    /// absolute-vs-rolling exactly once. Go => None.
    async fn native_policy(&self, project: &Project) -> Result<Option<NativePolicyLayer>>;
    async fn apply(&self, project: &Project, plan: &Plan) -> Result<ApplyReport>;   // mechanics: rewrites, MVS, resolver. Applies the plan + reports applied/skipped; NO intra-plan rollback — the app drives trials/rollback
    async fn build(&self, project: &Project) -> Result<VerifyReport>;  // OPT-IN compile/sync (--build); `apply` already guarantees a consistent, resolvable lock
    async fn write_native(&self, _p: &Project, _r: &ResolvedPolicy) -> Result<SyncReport> { Ok(SyncReport::Unsupported) }
}
pub struct NativePolicyLayer { pub rules: Vec<NativeRule> }            // the adapter knows its native structure
pub struct NativeRule { pub selector: Selector, pub window: RawWindow }
pub enum RawWindow { AbsoluteDate(jiff::Timestamp), RelativeDuration(jiff::SignedDuration), OptOut }

// The core converts a NativePolicyLayer into a normal PolicyLayer (Origin::Native) exactly once,
// per NativeRule by selector:
//   RelativeDuration(d) -> Rule.window.default = WindowSpec::MinAge(d)
//   AbsoluteDate(t)     -> Rule.window.default = WindowSpec::Freeze(t)
//   OptOut              -> Rule.allow = true   (an exemption; no window) — e.g. uv exclude-newer-package = false
pub fn normalize_native(native: NativePolicyLayer) -> PolicyLayer;
```

**`PackageRegistry`** — the finer-grained port each adapter is _built from_ (constructor-injected),
reusable and fakeable in unit tests. For ecosystems where a package manager drives resolution (uv,
cargo), the adapter **still reads the lock graph and fetches publish metadata through this port
itself** — the package manager is used to _resolve/apply_ a chosen window, never as the source of
cooldown truth.

```rust
#[async_trait]
pub trait PackageRegistry: Send + Sync {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>>;   // RawRelease carries per-artifact upload times (below)
    /// Publish instant of the LOCKED pin: for artifact-granular ecosystems the NEWEST of the given
    /// artifacts, but `None` if ANY of them has an unknown time (conservative → UnknownAge);
    /// version-level otherwise. The `check` gate uses this.
    async fn published_at(&self, pkg: &PackageId, version: &Version, artifacts: &[ArtifactId]) -> Result<Option<jiff::Timestamp>>;
}
pub struct RawRelease { pub version: Version, pub published_at: Option<jiff::Timestamp>, pub yanked: bool, pub artifacts: Vec<RawArtifact> }
pub struct RawArtifact { pub id: ArtifactId, pub published_at: Option<jiff::Timestamp> /* None = upload time unknown, e.g. private index */, /* + platform/abi/markers */ }
// version-granular ecosystems leave `artifacts` empty (use the version-level published_at); artifact-
// granular ones (PyPI) populate per-file upload times so the adapter computes the newest ENV-RELEVANT
// artifact (or newest of ALL under --all-artifacts) — but None if ANY selected artifact's upload
// time is unknown (conservative: a partially-known release is never treated as mature).
```

### Policy resolution (core, pure)

```rust
pub enum Selector { Default, Lang(EcosystemId), Registry(String), Project(Glob), Package(Glob) }
pub enum WindowSpec { MinAge(jiff::SignedDuration), Latest, Freeze(jiff::Timestamp) }
pub struct ByKind { pub default: Option<WindowSpec>, pub major: Option<WindowSpec>, pub minor: Option<WindowSpec>, pub patch: Option<WindowSpec> }
pub struct Rule { pub selector: Selector, pub window: ByKind, pub allow: bool, pub floor: Option<jiff::SignedDuration> }
pub struct ConfigRoot { pub rules: Vec<Rule>, pub strict_native: bool }  // root-level; strict_native combines MONOTONE across config layers (any true => on); CLI --no-fail-on-stricter-native overrides
pub enum ResolveKind { CurrentPin, Candidate(UpdateKind) }   // CurrentPin => the scope's bare min-age; Candidate(k) => the per-kind window
pub struct ResolveQuery<'a> { pub ecosystem: EcosystemId, pub package: &'a str, pub registry: Option<&'a str>, pub project: &'a Utf8Path, pub kind: ResolveKind }
/// Per-field combine (authority-first min-age, max-clamp floor, union allow) + trace.
pub fn resolve(layers: &[PolicyLayer], query: &ResolveQuery) -> Resolution;
```

`ByKind` is a fixed-field struct (no `Ord` on `UpdateKind`, no heap alloc) mapping the `min-age`
table field-for-field. `evaluate()` calls `resolve()` per candidate kind; `explain` returns the
trace.

### Application services (use cases, `app` module)

A `Workspace` bundles the detected adapters, the layered policy, and a **single `now` snapshotted
once** for the whole run (consistency over freshness — two deps evaluated 30s apart must use the
same boundary).

```rust
pub struct Workspace { ecosystems: Vec<Box<dyn Ecosystem>>, projects: Vec<ProjectCtx>, now: jiff::Timestamp }
// Policy is PER PROJECT, not workspace-wide: the shared layers (built-in default, global, env, CLI)
// are common, but the native layer and the repo cascade (root -> this project's dir) are scoped to
// each project — so sibling projects never leak policy into one another.
pub struct ProjectCtx { pub ecosystem: EcosystemId, pub project: Project, pub policy: PolicyStack }
// policy.layers, low -> high: [Default, Global, Native(project), RepoCascade(root..project), ExplicitConfig(shared --config), Env, Cli]
// policy.strict_native carries the monotone-combined root option so it survives loading
impl Workspace {
    pub async fn outdated(&self, opts: OutdatedOpts) -> Report;
    pub async fn check(&self, opts: CheckOpts) -> CheckReport;     // DepScope::Graph; per pin: locked_release(dep,ctx) -> check_pin(…); honors baseline
    pub async fn upgrade(&self, opts: UpgradeOpts) -> Result<UpgradeReport>;  // app-driven: snapshot lock; per change apply() a single-change plan; if it adds a too-fresh transitive, restore snapshot + skip (TransitiveInCooldown); then optional build
    pub async fn explain(&self, pkg: &PackageId) -> Resolution;
}
```

Per-dependency port failures fold into the report (so one flaky registry doesn't abort the run);
orchestration failures return `Result`. Concurrency lives in one place: registry fan-out via
`buffer_unordered(N)` with a per-host cap. **Mutating commands take a per-project advisory file
lock** so a concurrent `cargo`/`go`/`uv` can't corrupt the lockfile.

### Composition root and adding an ecosystem

`main.rs` is the only place that knows the full cast. It (1) parses flags and **snapshots `now`**;
(2) builds the **shared** layers (built-in default, global config, env, CLI flags) and the adapter
set (each sharing one `reqwest::Client` + cache); (3) detects projects across ecosystems; (4) for
**each** project assembles a `ProjectCtx` whose layer stack — low→high — is
`[default, global, normalize_native(native_policy(…)), root→project cooldown.toml cascade, explicit --config (shared), env, cli]`
(env/CLI stay on top; the project's native + cascade slot in at layers 3–4); (5) runs the use case
over every `ProjectCtx` and renders (`--json` swaps only the renderer).

```rust
let http = SharedHttp::new(cache_dir);
let ecosystems: Vec<Box<dyn Ecosystem>> = vec![
    Box::new(go::GoEcosystem::new(go::GoProxy::new(&http))),
    // cargo::, uv::, npm:: added here as adapters land — one line each
];
```

**Adding an ecosystem is one new module under `adapters/` implementing the ports, registered in one
line** — no change to `core`, `render`, the config schema, or any other adapter. The per-adapter
checklist is fixed: `detect`, `dependencies`, `releases` + classification (`order`,
`kind_from_current`, `quality`, `MajorKey`), `locked_release`, `native_policy` (or `None`),
`apply`/`build` if mutating, declared `capabilities`, and inheriting the conformance suite.

### Adapter implementation strategy ("combine, don't reinvent")

Native tools are **resolution/apply engines only**; verdicts always come from the core evaluator.

- **Go — no native engine, implement directly.** GOPROXY `.info` publish times parsed to typed
  instants (never compared as lexicographic strings), `x/mod` `IsPseudoVersion` semantics (never a
  hand-rolled regex), `/vN` discovery + import rewriting
  ([`gomajor`](https://github.com/icholy/gomajor)), `go list -u -m -json` for outdated, `go get` +
  `go mod tidy` for apply (best-effort under MVS). An empty publish time surfaces as `UnknownAge`,
  never a silent skip.
- **Rust — cargo + crates.io index.** Outdated detection à la
  [`cargo-outdated`](https://github.com/kbknapp/cargo-outdated); publish-age reads à la
  [`cargo-cooldown`](https://crates.io/crates/cargo-cooldown) /
  [`cargo-stale`](https://github.com/18o/cargo-stale); apply via
  `cargo update -p <pkg> --precise <ver>`. `=` pins → best-effort (skip-and-report).
- **Python — own the verdict, drive uv for resolution/apply.** The uv adapter reads the **`uv.lock`
  graph** and fetches **PyPI PEP-700 publish times** _itself_ (via `PackageRegistry`), and computes
  verdicts in the **core** — uv is _not_ the policy source. uv is invoked to _re-resolve/apply_ a
  chosen window (`uv lock --exclude-newer …`) and to write the lock. Shell out to the `uv` binary
  (stable); uv-as-a-git-dependency is a non-goal (no stable library API).

### Cross-cutting concerns

- **Async & runtime.** `tokio` (one runtime); registries async over `reqwest`; traits object-safe
  via `async_trait`.
- **Time & paths.** `now` is threaded as a value (pure domain). The boundary comparison is
  UTC-instant vs UTC-instant with **no tolerance** (trusts NTP) — documented, so it's a conscious
  choice. **Crate choice:** `jiff` — `jiff::Timestamp` for instants, `jiff::SignedDuration` for
  windows. It parses RFC3339 plus friendly (`"2 weeks"`) and ISO-8601 (`P7D`) durations out of the
  box; windows normalize to a fixed `SignedDuration` (days = 24h) so the boundary stays a pure
  UTC-instant comparison, matching the no-tolerance rule above. `camino::Utf8Path` for paths.
- **Network, caching & offline.** On-disk cache (XDG) of release metadata with provenance (URL +
  fetch time + ETag), TTL + ETag refresh. **Trust hardening:** a cached publish time may never move
  _earlier_ on refresh (monotonic floor) — a backdated upstream timestamp is flagged, not trusted.
  `--offline` is cache-only (misses → `UnknownAge`); `--no-cache`/`--fresh` forces the registry (CI
  gates use it); on network failure `check` returns exit 4, never a false `ok`. Honor
  `GOPROXY`/`GOPRIVATE`/`HTTPS_PROXY`/`NO_PROXY` and the crates.io sparse index; per-host
  concurrency cap + 429 backoff + descriptive User-Agent.
- **Errors.** `thiserror` enums in the library modules (`CoreError`, `is_transient`); the binary
  reports the top level via `color_eyre` (colored CLI reports). No `unwrap`/`expect` off the happy
  path.
- **"Unknown age" is never "mature."** Enforced once in the core.
- **Candidate↔locked consistency.** A candidate's publish instant is the newest upload among
  environment-relevant artifacts (else newest across all artifacts), the same basis `check` uses on
  the eventually-locked artifact — so a version `outdated` calls _adoptable_ can never be rejected
  by `check` after it locks. `outdated → upgrade → check` never disagree on freshness.
- **Testing & conformance — _before_ adapter work.**
  - _core_: pure truth-table tests for `evaluate()` and `check_pin()`; the **precedence-matrix**
    suite for `resolve()` (layer × selector × field, incl. floor/allow); golden `explain` traces —
    no async, no network.
  - _adapters_: drive each `Ecosystem` against a **fake `PackageRegistry`** + a temp project; golden
    before→after lockfile/manifest tests for `apply`.
  - _conformance_: one generic harness over every adapter — **universal** invariants (fresh stable →
    `InCooldown`; unknown publish → `UnknownAge`, never mature; yanked never adoptable; floor clamps
    a lower per-package window; current pin younger than window → `CurrentInCooldown`; a
    graph-pinned fresh pin → still a violation, annotated `graph_held`, never a pass) and
    **capability-gated** ones via `capabilities()` (pseudo → `Held`/exempt; `+incompatible`
    adoptable; dist-tags).
  - _check truth table_ (a core security contract, pinned by tests): the cross product of {fresh? ·
    baselined (exact scope)? · `allow`-matched? · `--latest`? · graph-held? · pseudo? ·
    unknown-age?} → {pass · violation · exempt · acknowledged}, plus the baseline-match rule (exact
    `(ecosystem, project, package, version, registry)`).
  - _policy isolation_: two sibling projects with different child `cooldown.toml` and different
    native config, evaluated in one root run, must each resolve under only their own cascade +
    native layer — a regression test proving no cross-project policy leakage.
  - _cli_: `insta` snapshots of the TTY and `--json` envelope.

## Per-language support & gotchas

### Enforceability matrix

`cooldown` always _computes_ correctly (it filters releases by publish time itself); `sync` and
native delegation degrade where native config is less expressive:

|                      | Go                                                  | Rust                                                         | Python (uv)                                                                                               | Node              |
| -------------------- | --------------------------------------------------- | ------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------- | ----------------- |
| native config layer  | none (repo>global>default)                          | `[package.metadata.cooldown]` (native — can't override repo) | `[tool.uv]` (read)                                                                                        | pnpm/yarn/bun     |
| `registry` selector  | effectively proxy.golang.org (trust via path globs) | alt/private registries (UnknownAge w/o index times)          | PyPI vs private index                                                                                     | scopes/registries |
| `sync` fidelity      | n/a (no native)                                     | partial                                                      | global + **per-package durations & exempts** OK; **per-kind / per-registry** inexpressible (flatten/warn) | partial           |
| downgrade to enforce | best-effort; MVS-held still fails check             | best-effort (`=` pins)                                       | resolver-bounded                                                                                          | resolver-bounded  |

### Go (MVP — implement fully)

- No manifest metadata → config from `cooldown.toml`/global/CLI; native layer empty.
- `/vN` majors live at a different module path → `MajorKey` encodes the path major-suffix; `--major`
  probes `path/v2`, `/v3`, …; `apply --major` rewrites imports.
- `+incompatible` has a real proxy publish time → `Stable`/adoptable, not prerelease.
- Pseudo-versions (`x/mod` `IsPseudoVersion`) are commit-pinned → `Held` in `outdated`, exempt in
  `check`.
- MVS: a too-fresh dep the graph requires newer can't be downgraded → it still **fails** `check`,
  annotated `graphHeld` (+ `graphFloor`); `upgrade` reports `SkipReason::GraphHeld`. Resolve by
  baselining or allowing it.

### Rust (later)

Read `[package.metadata.cooldown]` (native layer; can't override repo). Workspaces resolve at the
root; skip yanked; prereleases are excluded unless the current pin is itself a prerelease (an
explicit `--prerelease` opt-in is post-MVP). Publish times from the crates.io sparse index
(`created_at`); private registries → `UnknownAge`.

### Python (later)

The core owns verdicts; the uv adapter reads `uv.lock` + PyPI publish times itself and drives uv
only for resolution/apply. `exclude-newer-package` supports per-package durations and `false`. PEP
700 `upload-time`: PyPI provides it; private indexes may not → `UnknownAge`. No workspace → many
independent locks (the adapter enumerates project dirs). `cooldown` is fully self-sufficient on the
Python side — it reads the lock graph and PyPI itself and can own the uv `sync` field (§ sync).

### Node / TS (later)

Native `minimumReleaseAge` (pnpm/yarn/bun) + registry `time` map; dist-tags and deprecated versions
need classification.

## Security model

- **Threat model:** the smash-and-grab window. The cooldown delays _adoption_; it is not a malware
  scanner and pairs with `govulncheck`/`cargo audit`/advisory feeds.
- **Risk surface is the resolved graph.** `check` evaluates direct + transitive by default; the
  floor applies to transitive. `upgrade` applies **one change at a time** and diffs the resulting
  graph: if it adds a too-fresh, non-acknowledged transitive, the lockfile snapshot is restored and
  the change skipped (`TransitiveInCooldown`) rather than committed — so a passing `upgrade` never
  leaves a lock a subsequent `check` would reject.
- **Integrity stays the package manager's job** (`go.sum`, `Cargo.lock`/uv hashes); `cooldown`
  composes with it. The metadata cache is hardened (provenance + monotonic publish-time floor) so it
  can't be used to _lower_ an age.
- **Escape hatches are explicit and audited** (`--latest`/`--allow`/config `allow`, all in
  `explain`); a floor bounds config-level loosening to its layer or above.
- **Reproducibility** via `freeze`/`exclude-newer-span`; org policy via the global layer.

## Later work (post-MVP)

- **`sync`** into native configs (§ sync) — relative spans, uv per-package durations, and
  coexistence with any external writer of the same field.
- **Automatic advisory-driven bypass.** Consume `govulncheck`/`cargo audit`/PyPI advisory feeds so a
  version that _fixes_ an advisory affecting the current pin can bypass the cooldown (as
  Renovate/Dependabot do). Deferred deliberately: doing it right means fully modeling
  `{ advisory source, affected range, fixed version, status }` and a new
  `security_override_available` status + JSON shape — too much surface for the MVP, and the manual
  path (scenario 6) covers it meanwhile.
- **Rust, Python, Node adapters; crate extraction; plugin/dynamic registration.**

## Reference tools to port / reuse

| Ecosystem | Tool                                                                                                                | Use                                       |
| --------- | ------------------------------------------------------------------------------------------------------------------- | ----------------------------------------- |
| Go        | [`gomod-age`](https://github.com/fchimpan/gomod-age)                                                                | age gating via GOPROXY timestamps         |
| Go        | [`gomajor`](https://github.com/icholy/gomajor)                                                                      | major discovery + `/vN` import rewriting  |
| Go        | `go-mod-outdated` / `go list -u -m -json`                                                                           | outdated detection                        |
| Rust      | [`cargo-outdated`](https://github.com/kbknapp/cargo-outdated) / [`cargo-stale`](https://github.com/18o/cargo-stale) | outdated / fast index reads               |
| Rust      | [`cargo-cooldown`](https://crates.io/crates/cargo-cooldown)                                                         | min-publish-age logic                     |
| Python    | [uv](https://docs.astral.sh/uv/reference/settings/)                                                                 | resolution/apply engine (`exclude-newer`) |
| All       | Renovate / Dependabot cooldown                                                                                      | semantics reference                       |

Watch [`rust-lang/cargo#15973`](https://github.com/rust-lang/cargo/issues/15973); if native cargo
cooldown lands, the Rust adapter shrinks.

## What it consolidates

One tool and one UX in place of the per-ecosystem patchwork teams stitch together today:

- **Go** — `gomod-age` (age gating), `gomajor` (major discovery + `/vN` rewriting),
  `go-mod-outdated` (`go list -u -m -json` formatting), and hand-rolled shell gates.
- **Rust** — `cargo-outdated` / `cargo-stale` (outdated) and `cargo-cooldown` (age).
- **Python / Node** — each package manager's bespoke `exclude-newer` / `minimumReleaseAge` config,
  configured and enforced uniformly instead.

`cooldown` complements CI bots (Renovate / Dependabot cooldown) rather than replacing them: the bots
are the remote PR layer, `cooldown` is the local CLI and gate that uses the same notion of "too
fresh."

## Distribution & install

Prebuilt release artifacts per OS/arch (musl static for Linux CI) + `cargo install` fallback; CI
pins a version. **Windows:** an explicit support statement (shelling out to `go`/`cargo`/`uv` and
`/vN` import rewriting have Windows pitfalls) — support first-class or declare Linux/macOS-only
initially. Ship a copy-paste GitHub Actions `cooldown check` recipe beside the usual
`govulncheck`/`cargo audit` jobs.

## Open questions / risks

- **Crate naming** (only relevant after the MVP single-crate split): `cooldown-go` /
  `cooldown-cargo` / `cooldown-uv` / `cooldown-npm` (recommended) vs `cooldown-<lang>`.
- **Clock-skew tolerance:** none (trust NTP) is the current choice — confirm for locked-down CI.

## Rough phasing

The MVP is the **policy model + Go + the JSON envelope + full-graph check**, fronted by tests:

1. **Config schema + policy core** — valid TOML (scalar-vs-table `min-age`, exact env/CLI mapping),
   `evaluate()`, `check_pin()`, `resolve()` (authority-first; floor max-clamp; allow union),
   `CoreError`, `Origin`. Plus the **conformance + precedence-matrix + golden-`explain`** test
   suites _before_ any adapter.
2. **`registry` module** (HTTP + cache + monotonic floor) and the **JSON envelope** in `render`.
3. **Go adapter** — typed GOPROXY timestamps + `IsPseudoVersion`; ship
   `outdated`/`upgrade`/`check`/`baseline`.
4. **Rust adapter** (crates.io index + `cargo update --precise`).
5. **Python adapter** (read `uv.lock` + PyPI; drive uv for resolve/apply).
6. **Later:** `sync`, advisory-driven bypass, Node adapter, crate extraction.
