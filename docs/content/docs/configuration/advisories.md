---
title: Advisories
weight: 5
---

# The advisory feed

A cooldown knows how *old* a version is and nothing about whether it is *security-relevant*. That
blind spot cuts both ways: a candidate that fixes a published CVE waits out its window as a silent
hold, and the pin a security bot's PR just gave you fails the very next [`check`]({{< relref
"../commands/check.md" >}}) for being two days old. The advisory feed supplies the one missing
input — advisories from [OSV](https://osv.dev), matched against each dependency's own release
list — so a security fix is at least *visible*, and optionally *fast-tracked*.

```toml
[advisories]
enabled = true
```

The feed is **off by default**: turning it on adds a network dependency and a new thing to trust,
so that is a deliberate choice. With just `enabled = true` you get the safe mode — `flag`.

## `flag` — annotate, change nothing

Under `flag` (the default `mode`), a candidate that fixes an advisory affecting the current pin is
annotated in [`outdated`]({{< relref "../commands/outdated.md" >}}):

```text
 Package  Current  Adoptable  Latest  Cooldown  Status
──────────────────────────────────────────────────────────────────────────────
 time     0.1.45   —          0.3.41  6d/7d     in cooldown ⚠ fixes GHSA-wcg3-cvx6-7396 (moderate)
```

On the `check` side, a locked version that is itself an advisory's fix version counts toward the
summary's `security-relevant` tally. Verdicts are unchanged in both places — `flag` only makes the
security signal visible. In `--json`, annotated rows carry a `security` object (`version`,
`fixes`, `severity`, `source`, `applied`). `version` names the release the block describes: the
locked pin on a `check` row, and on an `outdated` row the security-relevant candidate — which is
not always the candidate whose cooldown the row displays, since a fix that actually earned the
security window wins over a newer, merely-annotated one.

## `shorten` — a separate security window

```toml
[advisories]
enabled  = true
mode     = "shorten"
min-age  = "1d"       # the security window
severity = "high"     # the minimum severity that earns it (default: high)
```

Under `shorten`, a version the feed explicitly lists as a qualifying advisory's **fix** resolves
against the **security window** (`min-age` above) instead of the ordinary one:

- **Candidate side** — the fix candidate matures after the security window, so [`upgrade`]({{<
  relref "../commands/upgrade.md" >}}) adopts it earlier and `outdated` shows it adoptable sooner.
  A newer candidate that merely falls outside the affected ranges is *annotated* but not
  fast-tracked: only the exact fix version is evidence the `check` gate can later re-certify from
  the locked release alone, so fast-tracking anything else would adopt a version the gate rolls
  back.
- **Pin side** — a locked version that *is* the fix satisfies the security window too, so merging a
  security bot's bump stops failing the next `check` run.

An `upgrade` row that moved for security reasons says so — `⚠ fixes GHSA-… (high); adopted on the
security window` in the Reason column, and the same `security` object in `--json` — so a
fast-tracked adoption is never indistinguishable from a routine bump in the report.
The `outdated` and `check` cooldown cells also append `(security GHSA-…)` when the displayed
window was shortened; a surviving floor remains visible beside it as `(≥source)`.
In JSON, the same provenance is the window's `shortenedBy` field, kept separate from `clampedBy`
because shortening loosens a window while clamping tightens it.

The security window is **min-clamped**: it only ever *shortens* the ordinary window, never extends
it. A dependency whose ordinary window is already shorter is unaffected, and a security `min-age`
at or above the project *default* window earns a config warning, since it cannot shorten that
default — a package or tool rule with a longer window can still be shortened, so the warning names
the default rather than claiming the security window never applies.

`severity` is the qualifying threshold — one of `low`, `moderate`, `high`, `critical`, compared
against the advisory's normalized rating. An advisory *below* the threshold (or one whose source
reports no usable rating, normalized to `unknown`) still annotates but never shortens.

Ratings are normalized from the most package-specific source available: an OSV document's
per-package `severity[]` entry wins over its document-wide one, which wins over the GitHub-style
qualitative rating. CVSS v3 and v4 vectors are scored to a rating; a CVSS v2-only record has no
scorer and stays `unknown`.

## What the feed never does

- It never **fails `check` for a vulnerable pin**. Vulnerability gating stays with `govulncheck`,
  `cargo audit`, Dependabot, and friends — duplicating it here would make exit `1` ambiguous
  between "too fresh" and "vulnerable". The feed only annotates, or loosens a window for a fix.
- It never **extends a window**. Every advisory-driven effect is monotone in the loosening
  direction, and only for rows that fix an advisory.
- It never **shortens on stale data** — see the trust rules below.
- It never **blocks a rollback**. A security bump younger than even the security window still
  fails `check`, and [`fix`]({{< relref "../commands/fix.md" >}}) still rolls it back to the
  newest matured version — which the same advisory may mark affected. That is the same
  "not a vulnerability gate" rule as above, so the verdict is unchanged; what the feed adds is
  that the rollback is no longer silent. The run warns, naming the advisory it re-enters
  (`advisory_rollback` in `--json`) and pointing at
  [`baseline`]({{< relref "../commands/other.md" >}}) as the way to keep the fix and still pass
  the gate.

## Failure and trust rules

The feed's failure semantics are deliberately the *inverse* of the registry's:

- **An unreachable feed fails open, loudly.** A registry outage can make a fresh version look
  mature, so `check` fails closed on it (exit `4`). An advisory outage can only fail to *shorten* a
  window — the ordinary, stricter window stands — so it is a warning, not an error. A CI gate that
  refuses to certify without the feed can escalate it: `check --fail-on-advisory-source` turns the
  warning into an error (exit `4`). That covers every way the enabled feed can yield no usable
  evidence — unreachable, no wired source implements the selected `source`, or data too stale to
  shorten. A mixed-registry tool with no safe single database mapping stays a warning under the
  flag: the project-wide query cannot run, so no feed outage is involved.
- **Stale cache data annotates but never shortens.** Cached advisory responses live for 24 hours.
  Past that (e.g. under `--offline`), the data may still flag rows, but `shorten` degrades to
  `flag` for that project, with a warning saying so. When staleness arrives later — an upgrade's
  re-lock introduces packages the settled snapshot must grow to cover — the demotion is scoped to
  exactly those packages: their evidence still annotates and still warns when a `fix` rollback
  would re-enter an advisory, but never earns the security window. The registry cache's rule is a monotonic
  *floor* because publish times are a tightening input; the advisory feed is a *loosening* input,
  so its trust rule points the other way.
- **A `floor` still clamps the security window.** By default the worst a poisoned or lying feed
  can do is drop a window to your [`floor`]({{< relref "precedence.md" >}}). To let the security
  window undercut a floor, declare `bypass-floor = true` **in the floor's own layer** — the same
  rule an `allow` exemption follows, so a repo config cannot use a CVE as a lever against an
  org-level floor. Each floor is decided separately: a bypass lifts only the floors its own layer
  declared, and the largest floor left standing still clamps the security window.
- **Uncertain range evidence never loosens.** Version-range boundaries are matched against the
  dependency's own fetched release list, then sorted and folded into affected spans — the events
  describe a vulnerable/not-vulnerable state, and the order a feed lists them in is a courtesy,
  not a guarantee. A boundary that cannot be located there (a typo'd version, a range on another
  release line) drops that whole range — ordering the rest would mean guessing where the missing
  boundary sat — so range membership never testifies: never a silent pass. Only *exact* evidence
  remains for such an advisory: a version the feed explicitly lists as a fix still flags (and
  still earns the security window — an exact fix-version match needs no ordering), while "it
  looks outside the remaining ranges" proves nothing and is ignored. The conventional "from the
  beginning" boundaries (`0`, `0.0.0`, `0.0.0-0`) name no release and are read as unbounded
  rather than as unfindable.

## Precedence

`[advisories]` participates in the normal [layer model]({{< relref "precedence.md" >}}), with
per-field combine rules chosen so that layering never loosens by accident:

| Field | Rule |
|---|---|
| `enabled`, `source`, `min-age` | **Authority-first** — the highest layer that sets each wins. |
| `mode` | **Monotone toward `flag`** — among the layers that set it, an explicit `flag` wins. A repo can always decline fast-tracking; a lower layer can never force it past that. |
| `severity` | **Max across layers** — the threshold only ratchets up. |
| `bypass-floor` | Honored only against a floor declared **in the same layer**, per floor. |

`cooldown explain <pkg>` appends the `[advisories]` derivation to its trace whenever any layer
configures the table — one step per layer *and field* (`advisories.mode`, `advisories.min-age`, …).
For the combined fields above — `enabled`, `source`, `mode`, `min-age`, `severity` — exactly one
step per field is marked applied, so a value that lost to a higher layer or to a combine rule says
so.

`bypass-floor` is the exception, because it never combines into a single project-wide value. It is
traced where it is actually decided — against each floor candidate, during window resolution — so
a declaring layer gets one applied step per floor it lifted, naming that floor's selector and
duration. A layer whose floors have all been removed already (by an `allow`, or because it
declared none) gets a single step marked considered instead, and an explicit
`bypass-floor = false` — the default — is not traced at all.

`cooldown config` reports the resolved policy alongside the tool's advisory ecosystem, which is
where to look when an enabled feed annotates nothing.

## Ecosystem coverage

Each tool maps to its OSV ecosystem name. Tools without one are reported (a warning naming the
tool), never silently skipped:

| Tool | OSV ecosystem |
|---|---|
| `cargo` | crates.io |
| `go` | Go |
| `uv`, `pip`, `poetry` | PyPI (names normalized per PEP 503) |
| `npm`, `pnpm`, `yarn`, `bun` | npm |
| `maven`, `gradle` | Maven |
| `bundler` | RubyGems |
| `hex` | Hex |
| `swift` | SwiftURL (GitHub-hosted packages, queried as the lowercase repository URL) |
| `conda`, `pixi` | — mixed conda/PyPI graphs are not mapped to one ecosystem |
| `deno` | — mixed jsr/npm graphs are not mapped to one ecosystem |

`conda-lock.yml` and `pixi.lock` can contain PyPI packages that OSV covers in isolation.
Cooldown currently queries one advisory ecosystem per tool project, so it does not send those
PyPI entries separately from the conda entries in the same resolved graph; the warning means the
mixed tool has no safe single mapping, not that every package in its lock is absent from OSV.
The same limitation applies to Deno's mixed JSR/npm graph.

The mapping is necessary but not sufficient: a package is only queried when its origin is
*proven* to be the ecosystem's public registry (see [Registry identity](#registry-identity)
below). For `pnpm`, `bun`, `yarn` (classic), `maven`, and `gradle` no resolution record or
confirmable configuration proves per-package origin, so their packages are currently never
queried — the mapping exists, the evidence does not.

### Registry identity

An OSV ecosystem names the packages of one public registry — `PyPI` means pypi.org, `npm` means
registry.npmjs.org — so a package from a private or alternate registry that merely shares a
public package's name is a *different package*: matching it against the public advisory would
annotate it with someone else's vulnerabilities and, under `shorten`, loosen its window on the
strength of someone else's fix. A package therefore carries an advisory identity only on
*positive* origin evidence, in one of three forms: a per-package resolution record naming the
public registry; a fully enumerable configuration surface shown clean (pip's requirements
tree, followed through its `-r`/`-c` includes); or the package manager itself confirming its
effective routing when the feed runs (npm, pip). Configuration can veto an identity the records grant — a
registry override says future resolves route elsewhere — but its absence never grants one,
because several resolvers read configuration no file walk can locate. Everything unproven is
withheld from the feed entirely — never queried (its name is not sent), never matched — and the
narrowed coverage is reported as a warning, once per run and again for packages a re-lock
introduces; withheld packages simply keep their ordinary, stricter windows.

What each tool can prove, from the records and configuration it reads:

| Tool | Origin evidence |
|---|---|
| `uv` | per package: the `uv.lock` entry's index URL must be the canonical PyPI index; index configuration (`[tool.uv]` in the manifest or any ancestor `pyproject.toml`, project/ancestor/user/system `uv.toml`, `UV_*` variables) vetoes, and a build requirement — having no lock entry — rests on that same surface being clean |
| `poetry` | per package: a `poetry.lock` entry without a `[package.source]` table came from PyPI — unless `pyproject.toml` declares any `[[tool.poetry.source]]`, which withdraws every claim |
| `pip` | per tree, twice over: every requirements file (following `-r`/`-c` includes) and every `PIP_*` routing variable (the index options plus `PIP_REQUIREMENT`/`PIP_CONSTRAINT`, which splice in an unseen file) must be visibly clean, and when the feed runs, pip itself is asked for its effective configuration (`pip config list` — the only authority on the interpreter-prefix site file that shims relocate beyond any file walk); one directive, unreadable include, config hit, or failed query withholds every pin |
| `npm` | per entry, twice over: the lock's `resolved` URL must name registry.npmjs.org (its integrity hash pins the artifact to what that registry served), and when the feed runs, npm itself is asked for its *effective* routing (`npm config list`, all layers merged — global and builtin included); a package that routing reroutes, or every package when the query fails, is withheld |
| `yarn` (classic), `pnpm`, `bun` | never at feed time: pnpm and bun lock formats record no per-entry origin, and yarn classic merges `.yarnrc` files up the directory tree with no reliably parseable effective-config query — so no identity survives to the feed |
| `maven`, `gradle` | never: neither format records a per-artifact repository, and the routing that decides real origin — `settings.xml` mirrors, `settings.gradle` repository blocks, init scripts, parent-pom repositories — lives outside the files the adapter reads |
| `bundler` | per section: the `Gemfile.lock` `GEM` remote must be rubygems.org (`GIT`/`PATH` sections never qualify) |
| `hex` | per package: the `mix.lock` entry's repo element must be `hexpm` |
| `cargo` | by construction: non-crates.io sources are never evaluated in the first place, and the lock's source id plus checksum chain pin both origin and content |
| `go`, `swift` | by construction: the module path / repository URL *is* the package's global identity |

Per-invocation flags (`pip install -i …`, `mvn -s …`) are outside any of this — they are not
ambient configuration, and cooldown's own operations never pass them. A plain mirror or proxy
of the public registry is indistinguishable from a shadowing private index, so packages behind
one are withheld too. Every decline costs only an un-shortened window — a missed loosening,
never a loosened one.

## Reference

All `[advisories]` keys:

| Key | Values | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `false` | Consult the feed at all. |
| `source` | `osv` \| `github` \| `none` | `osv` | Which feed. `github` parses for forward compatibility but no client implements it yet, so selecting it warns (see the failure rules) rather than failing the run; `none` is the explicit "no feed". |
| `mode` | `flag` \| `shorten` | `flag` | Annotate only, or also apply the security window. |
| `min-age` | duration | `"1d"` | The security window (only meaningful under `shorten`). |
| `severity` | `low` \| `moderate` \| `high` \| `critical` | `high` | Minimum normalized severity that earns the security window. |
| `bypass-floor` | bool | `false` | Let the security window undercut a `floor` (honored only against a floor declared in the same layer). |

CLI and environment equivalents:

| Flag / env | Effect |
|---|---|
| `--advisories` / `--no-advisories`, `COOLDOWN_ADVISORIES` | Enable/disable the feed for this run. The variable takes `1`/`true`/`yes`/`on` or `0`/`false`/`no`/`off`; anything else is a config error rather than a silently ignored value. |
| `--advisory-min-age <DUR>`, `COOLDOWN_ADVISORY_MIN_AGE` | The security window. Setting it also enables the feed and selects `shorten` (still monotone: an explicit config `flag` declines it). |
| `--advisory-severity <SEV>`, `COOLDOWN_ADVISORY_SEVERITY` | The severity threshold. Setting it also enables the feed. |
| `check --fail-on-advisory-source` | Make an unusable feed — unreachable, unimplemented, or too stale to shorten — an error (exit `4`) instead of a warning. |

> [!NOTE]
> One OSV round trip is made per project and ecosystem (a batched `querybatch`, then one document
> fetch per unique advisory), cached for 24 hours alongside the registry cache — enabling the feed
> does not add a per-dependency network cost. The batch is re-queried only to follow a pagination
> token (which needs a package with more than 1000 advisories) or, during `upgrade`/`fix`, to
> cover packages a re-lock newly introduced — never to re-ask about a package already queried, so
> one run's evidence cannot shift under it mid-flight.

> [!IMPORTANT]
> **What the query sends.** Each `querybatch` request carries the ecosystem plus the normalized
> name of every package in the project's resolved graph whose origin is provably the ecosystem's
> public registry (see [Registry identity](#registry-identity) — packages withheld there are not
> sent at all). For Go that is the module path, for Swift the repository URL, which can reveal
> private module or repository identities to the feed operator. No versions, manifests, or
> dependency edges are sent, and nothing says which package is actually in use beyond its
> presence in the list. `--offline` sends nothing: cached advisory data is used as far as it
> goes (annotate-only once stale), and missing data only means un-annotated rows.
