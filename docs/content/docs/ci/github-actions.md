---
title: GitHub Actions
weight: 1
---

# GitHub Actions

A minimal workflow that gates every pull request on the cooldown. It installs the prebuilt binary with [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall), then runs the gate:

```yaml
name: cooldown
on:
  pull_request:
  push:
    branches: [main]

jobs:
  cooldown:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: cargo-bins/cargo-binstall@main
      - run: cargo binstall -y cooldown
      # Ignore the local cache so the gate can't pass on a stale publish time.
      - run: cooldown check --fresh
```

`cooldown check` exits non-zero on a violation, which fails the job. Nothing else is needed — the gate reads the committed lockfiles and evaluates the resolved graph.

## Scoping the gate

In a polyglot repository you can gate one ecosystem per job, or all at once:

```yaml
      - run: cooldown check --fresh --tool cargo
      - run: cooldown check --fresh --tool uv,npm
```

Splitting into separate jobs gives you independent status checks (a red Rust gate and a green Python gate are distinguishable at a glance).

## Softening transitive noise

By default the gate fails on a too-fresh **transitive** dependency, because the resolved graph is the real risk surface. If you'd rather see those but not block on the ones you can't act on, keep them visible and non-fatal:

```yaml
      - run: cooldown check --fresh --transitive allow
```

The strict default is opt-out, never opt-in — see [`check`]({{< relref "../commands/check.md" >}}).

## Nightly "what's adoptable" report

Separate from the gate, a scheduled job can surface updates that have matured, without failing anything:

```yaml
name: adoptable
on:
  schedule:
    - cron: "0 6 * * 1"      # Monday mornings
  workflow_dispatch:

jobs:
  outdated:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: cargo-bins/cargo-binstall@main
      - run: cargo binstall -y cooldown
      # Exit 1 only if something is adoptable, so the run's status reflects it.
      - run: cooldown outdated --fresh --no-major --exit-code
```

`--exit-code` turns the informational [`outdated`]({{< relref "../commands/outdated.md" >}}) into a soft signal, and `--no-major` keeps cross-major bumps from nagging every week.

## JSON for other tooling

Every command takes `--json`, so a workflow can post results elsewhere. The schema is printed by [`cooldown schema`]({{< relref "../commands/other.md" >}}):

```yaml
      - run: cooldown check --fresh --json > cooldown.json
```
