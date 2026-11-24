---
title: Other commands
weight: 5
---

# Other commands

Beyond the four workhorses, `cooldown` has a handful of smaller commands for acknowledging risk, understanding policy, and scaffolding.

## `baseline`

Record currently-young dependencies as **acknowledged**, so `check` adopts them cleanly instead of failing:

```bash
cooldown baseline
```

Use it when a young dependency is one you have a reason to trust and you don't want to roll it back. A baseline is an explicit, reviewable record — it says "I have seen this specific version and accept it." Once a baselined version ages past its window (or is no longer present), it is dead weight; `--prune` drops those stale entries:

```bash
cooldown baseline --prune
```

`baseline` accepts the risk; [`fix`]({{< relref "fix.md" >}}) removes it. Reach for `fix` when you can roll back, and `baseline` when you can't or won't.

## `explain`

Show **why** a package is where it is: the decision — the same verdict `outdated` reaches for it,
with the reason behind a blocked one — and the window that decision was made under, every layer
and rule that applied, in precedence order:

```bash
cooldown explain <package>     # alias: cooldown why <package>
```

{{< terminal name="explain" >}}

The header leads with the decision. One `verdict:` line per resolved version of the package
shows what `outdated` shows — the current and adoptable versions, the status (`blocked by …`,
`held (declared bound <6)`, `in cooldown`), the newest release when it is neither the current
nor the adoptable version (`latest 5.1.0,`), and the candidate's age against its window — and a
blocked verdict is followed by `reason:`, the sentence `upgrade` prints for that row (a partial
landing and the importer it left behind, a workspace split and the flag that converges it, a
refused duplicate copy and its requirer). The verdict is the row `outdated --transitive` reports
for the package, from the same `upgrade --dry-run` over the project's whole plan — the package
manager's resolver against a throwaway copy of the project, never the project itself — since what
lands for one package can depend on every other move in the batch. It therefore takes as long as
`upgrade --dry-run` on that project; `--package` narrows the plan as it does for `upgrade`. Under
`--offline` the dry run is skipped and the run says so; the verdict is then the per-package one. `declared
by:` lists every workspace member's declaration — the declared range, the version that member's
own lock entry resolves to, and the manifest fields naming it (`[peerDependencies]` is how a
peer-only declaration shows), one line per distinct range when a member declares the package in
several fields (a caret in `dependencies` beside an exact pin in `devDependencies`) — with
`(excluded)` on the members the run ignores; it appears for
the tools whose adapter records per-member declarations (the npm family). `--json` carries these
as `verdicts` (the `outdated` rows) and `declarations`.

Each row of the table below is one layer/selector that was considered; the `Applied` column marks the ones that won, and the `Note` explains why. This is the tool for answering "why is this dependency exempt?" or "which `cooldown.toml` set this window?" — see [Precedence]({{< relref "../configuration/precedence.md" >}}) for the model it renders.

When workspace members that declare the package are excluded from the run (`exclude-folders`,
`exclude-packages`, or a `-C` selection), the header lists them as `excluded members`, and
`--json` carries them as `excludedMembers`: their declarations are neither evaluated nor moved,
and on pnpm they no longer count as workspace evidence — a copy on another line in an excluded
importer cannot hold an update in an included one.

## `config`

Print the **fully-resolved** configuration, with the origin of each value:

```bash
cooldown config
```

{{< terminal name="config" >}}

Where `explain` answers "why this one package," `config` answers "what is the effective policy here, and which layers produced it." It is the first thing to run when a repo's behavior surprises you.

Each project also reports its resolved [`[advisories]`]({{< relref "../configuration/advisories.md" >}}) policy and the advisory-database ecosystem covering its tool — the place to look when an enabled feed annotates nothing. Mixed-registry tools such as conda, pixi, and Deno currently have no single project-wide ecosystem mapping, even though individual PyPI or npm packages in their locks may be covered by OSV.

A cargo project also reports its [`generated-members`]({{< relref "../configuration/selectors.md" >}}#toolcargo-generated-members) declaration — the workspace-hack members whose declarations follow the lock rather than drive candidates — and the config file that declared it (`--json`: `generatedMembers`), or that nothing declares the key, so an audit can see whether the run's scope was narrowed and by which file.

Each layer is named by an origin token: `default`, `global`, `native`, `env`, `cli`, `repo:<path>` for a `cooldown.toml` in the cascade, or `config:<path>` for the `--config` file. The same tokens appear in `explain`'s trace and in every report's window `clampedBy` and `source`; a `source` or `decidedBy` appends `:<selector>` when a rule decided the window, as in `repo:cooldown.toml:package=left-pad`. A file inside the repository is named by its `/`-separated path relative to the repository root, such as `repo:apps/web/cooldown.toml`, so the output reads the same on every machine and platform. A file outside the repository, such as a `--config` file elsewhere, has no such path and keeps its absolute one, which is specific to the machine; the global config is always `global`.

## `init`

Scaffold a documented starter `cooldown.toml`:

```bash
cooldown init
```

It writes a commented file you can trim to taste, and **refuses to clobber** an existing `cooldown.toml` — so it is always safe to run.

## `schema`

Print the machine-readable JSON schema for `--json` output:

```bash
cooldown schema
```

Use it to validate or generate types for anything that consumes `cooldown --json`. The JSON envelope is a supported interface; the schema is its contract.

## `sync`

Write the resolved policy **down into native configs** — for example uv's `exclude-newer` — so `cooldown.toml` stays the single source of truth and native tooling sees the same window:

```bash
cooldown sync
```

The same behavior is available as a global `--sync` flag on any command, which syncs before the command runs (a no-op under `--dry-run`). See [Supported ecosystems]({{< relref "../tools/_index.md" >}}) for which tools have a native cooldown that `sync` can write to.

The default window `sync` writes is the one a package no rule names gets: the bare `min-age`, or a package rule that matches every name, such as `[package."*"]`, when it sets one. When that window is an exemption (`latest`, `min-age = "0d"` or `allow` on such a package rule), `sync` writes the bare `min-age` instead, since the native window still applies to every package whose native exemption is withheld.

When the default window is itself an exemption, `latest = true` or `min-age = "0d"`, `sync` leaves each native age setting as it is and reports the exemption below: a bare command keeps whatever cutoff its config sets, including one an earlier `sync` wrote. Writing an opt-out would not reach every bare command alike, since a removed setting exposes a broader one from the user's or the registry's configuration.

A native config holds one default window, and for uv the windows of packages a rule names exactly, so most policies say more than it can carry. `sync` reports every rule field that can hold a release back and that the written config does not carry, as a `config` warning naming the field, the file and the rule it comes from:

```toml
# cooldown.toml
[min-age]
default = "7d"
major = "30d" # reported: no native config has a window per update kind

[package."internal-*"]
min-age = "30d" # reported: a glob package rule has no native form
```

cooldown's own commands still enforce every one of these fields; the warning says only that a bare `pnpm install`, `uv lock` or `cargo update` does not. The report depends on where a field is declared, never on its value, so a per-kind window shorter than the default, such as `patch = "1d"`, is reported too. Reported are:

- every `major`, `minor` and `patch` window;
- the window and `floor` of a package rule whose pattern is anything but `*`, including a literal package for pnpm, Yarn and Cargo, which have no per-package window;
- for uv and Cargo, whose one file serves every project in the repository, a project rule that does not match the repository root, and every field of a `cooldown.toml` below the root, which the root's default never sees;
- a `freeze` whose `floor` still binds: `sync` writes the floor's rolling age, which stops matching cooldown on the date the warning names, once the floor reaches past the freeze date;
- an exempt default window, for every tool, as ``the cooldown exemption from `repo:cooldown.toml` is not represented in uv's native config``;
- for uv and Cargo, a package exemption their file cannot carry: `allow`, `latest = true`, or `min-age = "0d"`, which all read the same, as ``the cooldown exemption from `repo:cooldown.toml:package=itoa` is not represented in cargo's native config``. Cargo has no per-package setting, and uv's package table carries only a literal package, so every other package rule is reported for both, `*` included, whose exemption leaves the bare `min-age` in the written default.

For pnpm and Yarn, a package rule's `latest` and `allow`, and a `*` rule's `min-age = "0d"`, are not reported here: the native exemption lists described below cover them, and the writer reports each selector it withholds or leaves out. Any other `min-age = "0d"` package rule stays a window for them, reported like any other. `max-major` is no window and never reported.

cooldown judges every package of a tool as coming from one registry: `crates.io` for Cargo, `pypi` for uv, and `npm` for pnpm and Yarn, whichever index or scoped registry actually serves it. A `[registry."…"]` rule for that registry therefore gates every package the native config governs, so `sync` writes its window and `floor` like the bare ones: with `min-age = "14d"` and `[registry."crates.io"] floor = "30d"`, Cargo gets `min-publish-age = "30 days"`, and a `[registry.pypi]` floor reaches each of uv's package windows too. Its `major`, `minor` and `patch` windows and its exemption are reported like any other rule's. A rule for any other registry, such as an alternate Cargo registry whose crates cooldown never age-checks, gates none of the tool's packages and is not reported.

Two boundaries stay outside the report. A native config nearer to a project than the file `sync` writes, such as a nested uv project's own `[tool.uv]` table or a nearer `.cargo` configuration, shadows the written file for that project. And a setting of one index in the written file itself, such as a uv `[[index]]` with `exclude-newer = false`, overrides the written cutoff for the packages that index serves.

### pnpm

For pnpm, `sync` writes the project's `pnpm-workspace.yaml`: the default window as
`minimumReleaseAge` (in whole minutes, rounded up so the gate is never shorter than the window), and
every `latest` or `allow` package selector that exempts every release it matches as a
`minimumReleaseAgeExclude` entry, followed by the exact versions of young locked packages cooldown
admits.

A `minimumReleaseAgeExclude` entry exempts every version of every package it names, so `sync`
writes a selector there only when cooldown exempts all of them too: for the locked version and
every major, minor and patch update, with no `floor` that still binds, after every config layer is
applied. A selector cooldown still gates in part is withheld and reported as a `stricter_native`
warning, even when only some of the packages it matches are gated:

```toml
# cooldown.toml
[min-age]
default = "7d"
major = "30d"

[package."@corp/*"]
latest = true # majors of @corp/* still wait 30 days, so `@corp/*` is withheld
```

The young locked versions cooldown admits under a withheld selector are written as exact
versions instead, so the current lock still installs. `sync` admits locked versions only and
does not look at available updates, so any other release of those packages stays behind pnpm's
own `minimumReleaseAge`. The check is conservative for glob selectors: when a rule for a package
inside the glob, such as `[package."@corp/ui"]`, could set a window, the glob is withheld even if
cooldown happens to resolve that package as exempt.

pnpm matches those entries more simply than cooldown matches its selectors: only `*` is a wildcard
(and crosses `/`), a leading `!` negates, and every other character is literal. So `sync` writes a
selector only when pnpm reads it the same way: letters, digits, `.`, `_`, `~`, `-`, `/`, single `*`
wildcards, and a leading `@` scope, as in `@types/*` or `*-plugin`. Any other selector, such as
`@types/{node,react}`, `react-?om` or `[a-c]*`, is left out and reported as a `stricter_native`
warning: cooldown still exempts the packages it matches, but pnpm's own age gate keeps applying to
them unless another written selector covers them. A selector containing `**` is left out too, since cooldown gives `**/` a recursive-directory
meaning pnpm lacks. To exempt such packages natively as well, write them as separate selectors pnpm
can read (`[package."@types/node"]` and `[package."@types/react"]`).

When the policy has selectors but nothing can be written, whether they were withheld or left out,
`sync` keeps an explicit `minimumReleaseAgeExclude: []`, so pnpm does not fall back to exclusions
inherited from user or global configuration. For the same reason `sync` never removes an existing
`minimumReleaseAgeExclude`: a list it empties stays as `[]`. Without any selectors an absent list
stays absent.

When the default window is a `freeze` or `latest`, which pnpm's minute count cannot express,
`sync` leaves `minimumReleaseAge` as it is, reporting a freeze as a `config` warning, but still
removes every `minimumReleaseAgeExclude` entry without versions that it would not write, keeping
every version-qualified entry as written, so no package gains an exemption. A list it empties
stays as `[]`, and when a selector was withheld or left out it writes
`minimumReleaseAgeExclude: []` even where the key or the file was absent.

### Yarn Berry

For a Yarn Berry project, `sync` writes the project's `.yarnrc.yml` (or the file
`YARN_RC_FILENAME` names): the default window as `npmMinimalAgeGate`, in whole minutes rounded up,
and the `npmPreapprovedPackages` Yarn's gate approves:

```yaml
# .yarnrc.yml, as written by `cooldown sync` for `min-age = "7d"`
npmPreapprovedPackages:
  onConflict: reset
  value:
    - "@corp/*"
    - "is-number@7.0.0"
npmMinimalAgeGate: 10080
```

Every other line of the file, comments included, stays as it is, and the file is replaced
atomically. `sync` runs no Yarn command except `yarn --version`.

Yarn introduced both settings in 4.10.0 and refuses an rc file that sets a setting it does not
know, so `sync` first checks the Yarn the root `package.json` declares as its `packageManager`
and the one `yarn --version` reports in the project. A project whose Yarn predates 4.10.0 is
reported `unsupported` and left alone, like a Yarn Classic project; a project that declares 4.10.0
or later while an older Yarn runs there is an error, and nothing is written.

The list holds the `latest` and `allow` selectors cooldown proves exempt, as for pnpm, followed by
the exact versions of young locked packages cooldown admits. Yarn approves a release when any
entry matches it, and its wildcard `*` does not cross a scope's `/`, so `sync` writes a selector
only when Yarn matches no name cooldown does not: letters, digits, `.`, `_`, `~`, `-`, single `*`
wildcards, and a leading `@scope/`, as in `@corp/*` or `eslint-*`. A selector that opens with `*`
and has no scope, such as `*-plugin`, is written but matches only unscoped packages in Yarn, and
is reported as a `stricter_native` warning; any other selector, such as `@types/{node,react}`, is
left out and reported the same way.

Yarn adds up the lists of every `.yarnrc.yml` it reads, the ones above the project and in the home
directory included, and an empty list there removes nothing. `sync` therefore writes the list with
`onConflict: reset`, so the project's list is the whole list Yarn applies.

When the default window is a `freeze` or `latest`, `sync` leaves `npmMinimalAgeGate` as it is, and
reports a freeze as a warning, since a rolling minute count cannot express it. It still removes
from an existing list every entry without a version range that it would not write, keeps every
ranged entry, and writes the list in reset form; where no list existed, it writes an empty one only
when a selector was withheld or left out.

From Yarn 4.17.0, every scope configured under `npmScopes` has an age gate of its own, one day
unless the entry sets one, instead of the global `npmMinimalAgeGate`. `sync` sets
`npmMinimalAgeGate` in each scope entry of the project's own `.yarnrc.yml`, written as a plain
block mapping, and leaves the entry's registry and authentication settings alone. It never adds a
scope entry, since a new entry would route the scope's packages to Yarn's default registry, so a
scope configured elsewhere, such as in the home directory's `.yarnrc.yml`, is reported when its
gate differs: a `config` warning when it is shorter, a `stricter_native` one when it is longer.
A `YARN_NPM_MINIMAL_AGE_GATE` or `YARN_NPM_PREAPPROVED_PACKAGES` variable overrides the file for
every Yarn run in its environment, so `sync` warns when the environment it runs in sets one.

Yarn applies the gate only when it picks a version afresh: a version the lock already resolves is
reused, however young. The exact entries let the locked versions cooldown admits resolve again
when the lock is regenerated.

### uv

For uv, `sync` writes the repository-root `uv.toml`: the default window as `exclude-newer`, and
every package-level rule that applies to uv (`[tool.uv.package."…"]` or an unqualified
`[package."…"]`) as an `exclude-newer-package` entry:

```toml
# cooldown.toml
min-age = "14d"

[tool.uv.package."onnxruntime"]
min-age = "0d"
```

```toml
# uv.toml, as written by `cooldown sync`
exclude-newer = "14 days"
exclude-newer-package = { onnxruntime = false }
```

An age is written as a relative span (`"30 days"`), a `freeze` as its RFC 3339 instant, and a
package without a window (`0d`, `latest`, or `allow`) as `false`, so uv applies no cutoff to it.
cooldown owns the `exclude-newer-package` table: every sync rewrites it from the policy, so removing
a package rule removes its entry again.
Run `cooldown sync` (or pass `--sync`) after changing package rules: uv merges the command-line
overrides below into the table key by key, so an entry left in `uv.toml` for a rule that no longer
exists still applies until the next sync removes it.
Only exact names in their normalized PyPI spelling (`types-pyyaml`, not `Types_PyYAML`) are
written, since those are the names the gate matches against `uv.lock`; glob rules such as
`[package."types-*"]` have no `exclude-newer-package` form, stay enforced by the gate alone, and are
reported as a `config` warning, as are a package rule's per-kind windows.

Every uv command cooldown runs (`uv lock --check` under `check`, the re-locks under `fix` and
`upgrade`, and `uv sync` under `--build`) passes the project's resolved policy the same way, as
`--exclude-newer` and `--exclude-newer-package <name>=<cutoff>` arguments.
For a project the repository `uv.toml` governs, whose policy is the repository's own (no nearer
`cooldown.toml` changes it), the lock cooldown accepts is therefore the lock a plain `uv lock`
produces under the synced file.

uv reads only the nearest configuration file, so a project below the repository root whose
`pyproject.toml` has its own `[tool.uv]` table ignores the repository `uv.toml`.
cooldown's own commands still pin the policy for such a project on the command line, but a plain
`uv lock` there follows that project's `[tool.uv]` table, which `sync` does not write.

### Cargo

For Cargo 1.100 or newer, `sync` writes the default window as crates.io's publish age into the
repository root's Cargo configuration, so a plain `cargo update` skips releases younger than the
window too:

```toml
# .cargo/config.toml, as written by `cooldown sync`
[registries.crates-io]
min-publish-age = "2 weeks"
```

The file is the one Cargo actually reads: an existing `.cargo/config` shadows `.cargo/config.toml`,
so `sync` writes into it instead.
Every other setting in the file is kept, and an equal age spelled differently, such as
`"14 days"` for two weeks, is left as written.
Cargo reads configuration from every ancestor directory, so the repository-root file reaches every
Cargo project below it that sets no age of its own.

Cargo has one age per registry and nothing more, so `sync` cannot write everything a policy says:

- A `freeze` or an opt-out leaves the existing age alone, with a warning.
- A package rule has no Cargo equivalent, so a plain cargo command applies the default age to that
  package too; `sync` reports each such window or `floor` as a `config` warning.
- Cargo enforces no age at all while `resolver.incompatible-publish-age` is `allow`; `sync` writes
  the age anyway and warns that Cargo will ignore it.

cooldown enforces the window itself and never lets Cargo apply its age inside cooldown's own runs:
a cutoff Cargo applied mid-upgrade could refuse a version that cooldown still needs on the way to
its target.
See [Cargo's own publish age]({{< relref "../tools/_index.md" >}}#cargos-own-publish-age) for how
cooldown reads it.

## `recover`

Settle Cargo project state left by an interrupted cooldown mutation, then stop without resolving
dependencies or applying another change:

```bash
cooldown recover
```

Recovery currently supports Cargo only. Selecting another ecosystem with `--tool` is rejected
instead of reporting an empty successful run.

Read-only commands fail closed when they find a pending transaction; they never repair it as a
side effect. Recovery validates every recorded manifest and lock state before changing anything.
It accepts an already complete publication or restores a mixed interrupted publication to its
complete preimage, and leaves the project and recovery evidence untouched on unknown drift.
The project-visible record is trusted only when its exact digest matches an owner-private authority
under the repository's Git common directory. Non-Git projects do not grant recovery authority from
their project-local coordination files, so recoverable Cargo source mutations currently require a
Git worktree.
It discovers Cargo's public marker and orphaned private state/publication artifacts without loading
policy, manifests, baselines, or registries, so malformed normal-run inputs cannot prevent
recovery. Setup failures also use the schema-v4 recovery envelope under `--json`.

Repository recovery also inspects trusted authority records and their interrupted private
publication names. This finds transactions that stopped after authority became durable but before
the project-visible marker was published, including projects hidden from the ordinary repository
walk.

A repository-root recovery scan includes hidden and gitignored projects but deliberately skips
bulk dependency, build, and cache directories: `.cache`, `.venv`, `node_modules`, `target`, and
`vendor`. To recover an explicitly targeted Cargo project inside one of those directories, run
`cooldown recover -C path/to/project --cargo`. An explicit scope scans only that directory's
subtree and relevant ancestors, so unreadable or malformed repository siblings cannot block the
selected project.
