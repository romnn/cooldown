---
title: FAQ
weight: 9
---

# FAQ

## What is the default cooldown window?

**7 days.** Nothing younger than a week is adoptable until you say otherwise. Raise it repo-wide with one line — `min-age = "14d"` — or opt a package, registry, or ecosystem in or out with a [selector]({{< relref "configuration/selectors.md" >}}).

## Does `cooldown` block me from installing packages?

No. It doesn't intercept your package manager or your installs. It reasons about a **lockfile graph**: `check` reports and gates, `outdated` shows what's available, and `upgrade` / `fix` move the lock deliberately. You stay in control of when anything changes.

## Is this a malware scanner?

No — it never inspects code. It is a *timing* control: it refuses to be the first to adopt a brand-new release, so the window in which a smash-and-grab attack is live and undetected passes before the code reaches you. Run it alongside `govulncheck`, `cargo audit`, and advisory feeds, which catch a different class of problem. See the [Security model]({{< relref "security.md" >}}).

## Why did `check` fail on a transitive dependency I don't control?

Because the resolved graph — not just what you declared — is the real risk surface, and a re-lock can pull a brand-new transitive in. You have options: [`fix`]({{< relref "commands/fix.md" >}}) rolls it back to a matured version, `check --transitive allow` keeps it visible but non-fatal, and `check --transitive hide` gates direct dependencies only. If the graph pins it and nothing lower satisfies its requirers, `cooldown` names the dependency forcing the fresh pin so you can address the cause.

## A dependency shows as `unknown-age`. What does that mean?

`cooldown` couldn't determine that version's publish time (a registry gap, or a cache miss under `--offline`). By default that's a warning, not a failure — a `check` won't turn red on it. Make it fatal with `--fail-on-unknown-age` when you want the gate to insist on a known age.

## How is this different from Dependabot or Renovate?

Those tools *propose* updates; `cooldown` gates on *age*. They're complementary: let Renovate open the PRs, and let `cooldown check` make sure none of them adopts something too fresh. `cooldown upgrade` also only ever moves to versions that have already matured, so the two compose cleanly.

## Does it need network access?

Yes, to read publish times from each registry — unless you run `--offline` against a warm cache, in which case a cache miss becomes `unknown-age` rather than a guess. In CI, prefer `--fresh` so the gate re-fetches and can't pass on stale data.

## Can I pin an absolute cutoff instead of a rolling window?

Yes — `freeze = "2026-06-01"` (or `--freeze`) evaluates against a fixed instant: nothing published
after that date is ever adoptable, no matter when the run happens. Note what is frozen: the
*publication-time* eligibility. The registry's *current* state still applies — an unpublished or
yanked version disappears, and on the npm family the mutable `latest` dist-tag still caps
adoption (judged live by version-adopting commands, from a copy at most an hour old by read-only
ones), so a maintainer retagging can change which frozen-eligible release the tag ceiling admits
(`--no-respect-dist-tags` makes a frozen run tag-independent).

## How do I stay on TypeScript 5.x but keep taking 5.x updates?

Set a package-specific ceiling:

```toml
[tool.npm.package.typescript]
max-major = 5
```

This permits matured 5.x releases and holds 6.x even under `upgrade --major`. Alternatively, write
an explicit `<6` upper bound in `package.json`; that bound holds by default and can be crossed only
when you deliberately pass `--rewrite`.

## Why isn't the highest version on npm adoptable?

Because the registry's own `latest` dist-tag points below it. The tag is the maintainer's "this is
current" pointer — what a bare `npm install <pkg>` resolves to — so a stable release above it is
usually a premature or abandoned major the maintainer kept releasing below (a `17.0.0` published
months before the `16.x` line continued). `cooldown` holds such releases (`dist-tag latest 16.13.0`
in the table) instead of proposing a version the ecosystem itself would not install.
`--no-respect-dist-tags` (or `respect-dist-tags = false` under `[global]` or a command section in
`cooldown.toml`) is the deliberate escape hatch.

## Why did `upgrade` also *downgrade* some packages?

When advancing the graph would pull a too-fresh transitive in, `upgrade` reconciles it back down to a matured version so the resulting lock is gate-clean. A single `upgrade` run can therefore show both `upgraded` and `downgraded` rows — that's the whole-graph guarantee at work, not a mistake. See [`upgrade`]({{< relref "commands/upgrade.md" >}}).

## Is the Rust API stable?

No. The CLI (and its `--json` output, whose schema [`cooldown schema`]({{< relref "commands/other.md" >}}) prints) is the supported interface. The crates exist for the project's own binaries and integration tests and carry no stability guarantees.
