# Changelog

## Unreleased

- **Cargo's native `min-publish-age` is supported.** Cargo 1.100 stabilized publish-age filtering.
  cooldown now reads the crates.io age Cargo would apply to a project
  (`registries.crates-io.min-publish-age`, `registry.global-min-publish-age` or
  `CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE`) as its native cooldown window, below `cooldown.toml`
  and the command line. `cooldown sync` writes the repository window into the root
  `.cargo/config.toml` (or the `.cargo/config` Cargo reads instead), so a plain `cargo update`
  honors it too. cooldown's own cargo commands run without that age, so a stricter native age no
  longer makes Cargo refuse a version cooldown is pinning.

## v0.0.29

- **uv candidates whose wheel tags changed get their upload times instead of `unknown_age`.**
  A candidate was dated only by the wheel tags the lock records for the locked version, so a
  release that retagged its wheels matched none of them and came out undated. That happens with
  an abi3 or `requires-python` floor bump such as pymupdf 2.0's move from `cp310-abi3` to
  `cp311-abi3`, a new manylinux or macOS baseline, or a move to or from pure-Python wheels.
  `upgrade` therefore never adopted such a release. A candidate that no longer offers every wheel
  tag the lock records is now dated by its newest file, which is never earlier than the files an
  environment would install. The locked version itself stays undated when its file list lacks one
  of its locked wheels, rather than borrowing an older file's upload time.

## v0.0.28

- **The HTTP cache no longer lands in the working directory when `HOME` is unset.** Without
  `XDG_CACHE_HOME` and `HOME`, as in a Windows PowerShell or cmd session, cooldown fell back to a
  relative `.cache/cooldown` and wrote its registry responses into the project it ran in. The
  cache now falls back to the platform's home directory, the user profile on Windows. A relative
  `XDG_CACHE_HOME` or `HOME` is ignored, and with no absolute home at all cooldown stops with an
  error naming the variables to set, rather than using a shared temp directory.

- **`upgrade` and `fix` land Yarn Berry targets exactly.** Every mutation of a Berry project was
  refused before running Yarn. Under Yarn 4, cooldown now has Yarn prove the lock current first,
  then lands each candidate in a throwaway copy of the project: a lock-only install with every
  declaration of the package pinned to the target, the declared ranges bound to the target's lock
  entry, and a second lock-only install, since Yarn otherwise keeps a range bound to its old copy
  or resolves it afresh to the newest release, and Yarn's offline immutable install proves the
  result current in the copy. Only `yarn.lock` is written back, once every input of the copy is
  unchanged in the project, so manifests, install state, and caches stay as they were, and a
  failed candidate is skipped `resolver_conflict` with Yarn's error and rolled back alone. From
  Yarn 4.10, each landing runs under an approval of exactly its target and under cooldown's window
  when that is stricter than Yarn's own age gate, so a new transitive the gate quarantines fails
  the candidate; from Yarn 4.17, a landing that newly resolves another release in a
  configured scope gated below the window fails too. Yarn's global folder, whose warm caches serve
  an `--offline` landing, is replaced by one in the copy when it lies inside any directory holding
  a `package.json`. A declaration that is no registry range admitting the target (an alias, a
  `patch:` or local source), a member declaring the package at several ranges, and a range
  shared with another copy or with a member the run excludes hold the candidate `not_eligible`.
  `--build` runs `yarn install --immutable`, and `outdated` verifies a Berry project's adoptable
  rows against the landing as for the other managers. A Berry project run by Yarn 2 or 3 is
  still refused by `upgrade` and `fix`.

- **The config cascade collects every layer when the root and a project are spelled
  differently.** The walk from a project directory up to the repository root compared raw paths,
  so a root reached through a symlinked checkout, a `$HOME` fallback, or a Windows verbatim or 8.3
  spelling was not recognized as the project's ancestor. The walk then skipped every intermediate
  `cooldown.toml`, read the root file a second time as a nested layer, which a root file with
  `exclude-folders` turned into an error, or, for a root spelled through `..`, climbed past the root
  and applied config files above it. The walk now compares canonical paths, so each layer between
  the root and the project applies exactly once and nothing above the root is read. A project
  directory that does not resolve gets the root layer alone. The `$HOME` fallback root is itself
  canonical, so the Cargo recovery probe under `--dir` also stops at the same root whichever way
  `HOME` is spelled.

- **`check` verifies a Yarn Berry lock with Yarn itself instead of failing closed.** `check`
  reported every Berry lock's currency as unknown, so no Berry project was ever evaluated. It now
  runs `yarn install --immutable` without builds, with Yarn 2.4 through 4, in a throwaway copy of
  every input Yarn reads: the lock, every `package.json` outside `node_modules`, `.git`, and
  `.yarn`, the `.yarnrc.yml` files from the project up to the home directory, `file:` folders
  whole with their permissions, and the manifests of `portal:` and `link:` targets. The run is
  offline for Yarn, Corepack, and git, and every folder Yarn writes goes into the copy, so the
  project stays byte for byte as it was. Only `YN0028`, the lockfile Yarn would modify, makes a
  lock stale, which `--allow-stale-lock` waives, and only reaching the fetch step makes it
  current. Anything else is `lock_unknown`, which nothing waives: a lock whose change needs
  registry metadata the offline run cannot fetch, a configuration the copy cannot reproduce
  faithfully (a configured plugin, an `exec:` dependency, a workspace pattern or local package
  that leaves the project, a symbolic link the copy would resolve elsewhere, injected environment
  files other than `.env.yarn`, another lockfile name, per-host network settings, hardened mode,
  or an rc file using `onConflict` at its top level or for a setting the copy must reproduce, so
  the `npmPreapprovedPackages` `sync` writes still verifies), an input that changes while the
  copy is written, or an unsupported Yarn. An input that changes after the verdict, before the
  dependencies are read, is a `lock_conflict`. A Berry row now keeps its advisory identity when every lock entry of the
  release was fetched from the configured registry by name, or from an archive URL on
  registry.npmjs.org, and `yarn config get` routes the package's scope to the public registry;
  npm's `.npmrc`, which Yarn Berry never reads, no longer vetoes it.

- **A Yarn Classic member owns the release a same-name alias declares.** Member attribution read
  every `npm:` selector as an alias of another package, so a declaration such as
  `"has-flag": "npm:has-flag@^4.0.0"` owned no copy, and the release read as transitive though the
  root-manifest reading counted it direct. A registry reference to the dependency's own name,
  `npm:` or `yarn:`, scoped or bare, now binds that registry release for its member like a plain
  range, even where a workspace package carries the name, which Yarn never links for such a
  reference. Its copy is direct for the member but not eligible for an edit, with a reason naming
  the reference, since cooldown never rewrites one, and an exact reference such as
  `npm:has-flag@4.0.0` pins and holds the copy and is protected by the exact-pin guard, as in Yarn
  Berry. A `yarn:` alias of another package is now read as an alias too, rather than as a source of
  its own under the declared name.

- **Config layer tokens name repository files relative to the repository root.** A layer's
  origin token, as in `config`'s `layers`, `source`, and `generatedMembers.origin`, `explain`'s
  step `layer` and `decidedBy`, and every row's window `source` and `clampedBy`, spelled a
  `cooldown.toml` or `--config` file as `repo:<absolute path>` or `config:<absolute path>`, so the
  output differed between machines and between Windows and Unix. A file inside the repository is
  now `repo:cooldown.toml`, `repo:apps/web/cooldown.toml`, or `config:ci/cooldown.toml`,
  `/`-separated on every platform; a symlinked config file is named where the link sits. A file
  outside the repository, such as a `--config` file elsewhere, keeps its absolute path, and the
  global config stays `global`. The tokens were documented as stable, so this is a format change:
  a consumer that matched the absolute spelling must match the relative one. The JSON schema and
  `schemaVersion` are unchanged, since every field remains a string.

- **`sync` writes Yarn Berry's native age gate.** A Berry project reported `unsupported`, so a bare
  `yarn add` or `yarn up` ignored the cooldown window. `sync` now writes the default window into
  the project's `.yarnrc.yml` as `npmMinimalAgeGate`, in whole minutes rounded up, and the
  selectors and young locked versions cooldown exempts as `npmPreapprovedPackages`, written with
  `onConflict: reset` because Yarn adds up the lists of every rc file it reads. Only selectors
  Yarn matches no more widely than cooldown are written, and a leading `*`, which Yarn stops at a
  scope's `/`, is reported as narrower. From Yarn 4.17, where every configured scope has a gate of
  its own that defaults to one day, `sync` sets the gate of each scope entry in the project's file
  and warns about a scope configured elsewhere whose gate differs, without ever adding a scope
  entry, which would reroute it. A project whose Yarn predates 4.10.0, which would refuse the
  settings, is left `unsupported`, and one that declares Yarn 4.10.0 or later while an older Yarn
  runs there is refused. The rc file keeps its other lines and comments and is replaced atomically, and an
  rc file sync cannot edit without changing another setting is refused untouched. Atomic
  replacements now start owner-only on Unix when the file they replace has permissions of its
  own, so a private file's contents, such as a registry token in `.yarnrc.yml`, are never readable
  through the temporary file.

- **The pnpm integration tests no longer share pnpm's metadata cache.** Up to four of them run
  at once, and concurrent pnpm processes writing one per-user cache entry could leave it
  unreadable, so every later test resolving that package failed with
  `ERR_PNPM_CACHE_MISSING_AFTER_304` through all of its retries. Each test fixture now gives the
  pnpm it runs, and the pnpm cooldown starts for it, a private cache and store through
  `pnpm_config_cache_dir` and `pnpm_config_store_dir`. So that a fixture need not fetch every
  package's metadata again, a pnpm fixture's cache starts as hard links into a pool of the
  entries earlier fixtures finished; pnpm replaces an entry by renaming a new file over it, so no
  fixture writes another's copy, and the pool takes only entries that parse, so a damaged entry
  stays with the fixture that wrote it.

- **`--allow-stale-lock` waives only pnpm's own stale-lock refusal.** Every nonzero exit of
  pnpm's frozen lock verification counted as a stale lock, so `--allow-stale-lock` also waived a
  registry or network failure, an authentication error, an invalid `.npmrc` or
  `pnpm-workspace.yaml`, an unsupported flag, or a corrupted store, and `check` then evaluated a
  lock pnpm never confirmed and passed with a stale-lock warning. cooldown now reads the error
  code pnpm 9, 10, and 11 print: only `ERR_PNPM_OUTDATED_LOCKFILE`,
  `ERR_PNPM_LOCKFILE_CONFIG_MISMATCH`, `ERR_PNPM_NO_LOCKFILE`, and
  `ERR_PNPM_FROZEN_LOCKFILE_WITH_OUTDATED_LOCKFILE`, alone and from a run that was not
  interrupted, are a stale lock. Any other failure is a `tool_failed` error that no flag waives,
  and a mutating command that hits one after its apply reports the lock's status as unknown rather
  than stale. The verification pins pnpm's reporter, log level, and JSON output, so a `reporter`,
  `loglevel`, `json`, or `parseable` setting cannot hide the code.

- **Reported paths are relative and `/`-separated instead of absolute.** A diagnostic about a
  project's lock or manifest from `check`, `outdated`, `upgrade`, or `fix`, such as `lock_unknown`,
  `stale_lock`, or `lockfile_unreadable`, named the manifest by its absolute path, so a JSON
  report leaked the checkout's location and differed between machines and between Windows and
  Unix. Its `path` is now relative to the project root (`package.json`, `Cargo.toml`), as the
  adapters' own diagnostics already were, beside the scan-relative `project`. A `sync` row's
  `path` is relative to the directory its `project` names, `baseline` reports its file as
  `.cooldown-baseline.toml`, and a `recover` warning's `path` is relative to the repository root,
  or left out for an artifact outside it, such as one in a linked worktree's shared Git directory.
  A `recover` row's `project` is `/`-separated on Windows as well.

- **Yarn Classic aliases report the package they install.** The Classic lock's rows split each
  entry key at commas and took the package name up to the last `@`, so an `npm:` alias such as
  `isnum@npm:is-number@^6.0.0` became a phantom registry package `isnum@npm:is-number` that was
  evaluated and reported while its real target never was, and a quoted key holding a comma, such
  as a `file:` directory named `dir,other@1.0.0`, gave a phantom `other`. The rows are now read
  through the same port of Yarn's lockfile grammar as member attribution, so an `npm:` or `yarn:`
  alias is the target's release under the target's own name, with its `resolved` origin, its
  advisory identity, and no member's ownership. A release only aliases install is no longer a
  declaration's copy or a landing's evidence, unless a registry selector of its own name resolves
  it from a registry tarball. A `yarn.lock` Yarn's own reader refuses, such as one holding merge
  conflicts or a newer lockfile version, is now an unreadable lock instead of being listed line by
  line.
- **Yarn Berry workspace members are attributed through the lock's own workspace entries.** A
  Berry run credited nothing per member: the rows followed the root `package.json`, so a
  dependency only a member declares read as transitive, and the run warned about it. A Berry lock
  records every workspace Yarn installed with the dependencies its manifest declared, so cooldown
  now takes the members from the lock and credits each one the registry release its own current
  declaration of a name resolves to, through an `npm:` range, a same-name `npm:` alias, or Yarn's
  builtin `typescript`, `fsevents`, and `resolve` patches, never a copy that merely satisfies the
  range. A row is direct exactly when some member owns it, `exclude-folders` drops the rows only an
  excluded member owns, and an exact pin, `npm:4.1.0` and `npm:chalk@4.1.0` spellings included,
  holds the copy it binds. A declaration whose binding cooldown cannot prove leaves its whole name
  unattributed, every copy direct for no member and named in a warning with the reason: a name the
  root `resolutions` can rewrite, a `catalog:` or `patch:` reference, a name declared at different
  ranges across one manifest's install fields, a declaration the lock records under another range
  or not at all, a member the lock does not list, and, in a lock of version 6 or older, a bare
  range unless every `.yarnrc.yml` Yarn reads and `YARN_DEFAULT_PROTOCOL` leave `defaultProtocol`
  at `npm:`. A configured Yarn plugin, in any `.yarnrc.yml` or `YARN_PLUGINS`, can rewrite any
  declaration, so it turns attribution off with a warning. The attribution reads the lock through
  the manifests and configuration as they are now, so a lock stale against them can be
  misattributed; it does not reproduce the bindings Yarn last installed.
- **Yarn Classic workspace pins are protected from a sibling's resolve.** A Classic `yarn.lock`
  records no workspace member, so cooldown attributed nothing per member: every locked copy of a
  name the root manifest declares was a direct dependency without members, and the guard that
  refuses a resolve moving a manifest's exact pin protected no Yarn pin at all. cooldown now
  discovers the members from the root `workspaces` field the way Yarn 1 does, in either field
  shape with `*`, `?`, and `**` globs, a private root, and named, versioned members, and credits
  each member the lock entry its own selector names, looked up as an exact `yarn.lock` key, never
  a copy that merely satisfies the range. That selector is the one Yarn's manifest normalization
  keeps when a name sits in several fields, so a `devDependencies` pin beside a `dependencies`
  caret is judged at the caret's copy, and two members pinning different versions each own their
  own. A landing that moves such a pin in any member, an excluded one or one the plan does not
  name included, is rolled back alone as a resolver conflict naming the pin, a selector cooldown
  rewrote is read under its replacement, and a change lands only where every declaring member's
  own entry reached the target, judged against the members and manifests captured before the
  first trial. A copy whose selector binds it through a git, URL, `file:`, or `link:` source, a
  name the root `resolutions` can rewrite, a workspace package's own name, or a `resolved` URL
  that is no registry tarball stays the member's direct dependency, but no manifest edit is proven
  to move it, so `upgrade` holds it `not_eligible` with the member and the reason, and no pin is
  judged through it. An alias, a descriptor the lock records ambiguously or not at all, and an
  entry whose `version`, `name`, or `resolved` is no string bind no copy. A workspace whose members cannot all
  be discovered, such as one with a brace glob or a symbolic link on the way, and a lock Yarn's
  own reader refuses keep the root-manifest reading, and `check`, `outdated`, and `upgrade` now
  warn that its member attribution and pin protection are lost.

- **npm credits each locked copy only to the members whose own lookup reaches it.**
  `package-lock.json` records a workspace member's declarations as ranges, and cooldown credited
  every locked version of a declared name to each member declaring it. A copy another dependency
  nests, such as `strip-ansi`'s own `ansi-regex@5.0.0` beside the root's `ansi-regex: "^4.1.0"`,
  therefore became the root's direct dependency, and `upgrade` without `--major` planned its 5.x
  patch against the root, rewriting `^4.1.0` to `^5.0.1`. cooldown now resolves each declaration
  through the lock's install tree from the member's own directory, as Node does, so a member owns
  only the copy its lookup reaches, hoisted or nested in its own `node_modules`. Every other copy
  of the name is transitive, with no member, bound, or pin, and an alias's release stays unowned
  even beside a genuine nested copy. Pins and declared bounds follow the same copy: an exact pin
  holds its own copy whatever range binds another one, and never pins an unrelated nested copy.
  A dependency a member installs from git or from a local or remote tarball stays that member's
  direct dependency, so reports, `check`, exclusions, and the peer gate treat it as before, but it
  takes no pin or bound and `upgrade` and `fix` skip it as not eligible, since npm's exact pin
  would replace its source with a registry release; a remote tarball is recognized by the URL its
  declaration names, and an alias installing a registry release of the same version no longer
  hides such a copy. A linked workspace package owns no registry copy of its name, even when
  another dependency nests one at the same version. The per-package apply of npm, Yarn, and Bun
  now skips any transitive candidate as not eligible before reading a manifest, instead of falling
  back to the root's declaration of the same name. A lockfile v1, which records no install tree,
  keeps its root-manifest reading.

- **A cargo upgrade no longer moves a crate the workspace declares as a side effect of another
  pin.** A precise pin lets cargo float the target's dependencies to the newest versions their
  requirements admit, so pinning `pest_derive` could carry the planned `pest` past its target to a
  release younger than the cooldown, and the re-pin back down then dragged the directly declared
  `psm` along. Every lock-changing step of a cargo apply (each precise pin, its unlocking retry,
  seeded and atomic resolves, widening, and projection follows) is now judged against the lock it
  started from: a node an authored package declares keeps its exact version unless a planned
  change moves that very node, a planned target already reached stays reached, and a planned crate
  still at its original version either stays or lands on its target. A path package the upgraded
  graph no longer reaches, such as an obsolete `[patch]` shim, takes its declarations with it. A step that breaks this is rolled back, manifest
  edits included, and its candidate is held with the move it would have made. A pin that carries
  its own crate past the target, as one into a widened major does, is re-pinned to the target
  before it is judged. Declarations are matched by exact version, so a workspace that declares
  `syn` 1 still lets a transitive `syn` 2 move, a plan for one member's `foo` 1 cannot unify
  another member's declared `foo` 2.0.0 onto its target, and a collateral row for a declared crate
  is now reported as direct.

- **Which `allow` entry is reported as the source no longer depends on list order.** When several
  `allow` entries matched one package, cooldown named the last matching entry of the highest layer,
  so `allow = ["@corp/ui", "@corp/*"]` reported `@corp/*` for `@corp/ui` while the reversed list
  reported `@corp/ui`. The reported entry is now the highest-ranked match under the ordering
  `min-age` uses: the highest layer, then the more specific selector, then the narrower pattern,
  with the first entry winning a tie. The decision is unchanged, since every matching entry still
  applies; only the source that `explain` and `minAgeSource` name can differ.

- **pnpm's stale-lock check reads specifiers as npm does, so a floated exact pin is caught.**
  After a resolve, cooldown checks each `pnpm-lock.yaml` importer's version against its recorded
  specifier, but it parsed the specifier as a Rust semver requirement. A bare `6.0.0` read as
  `^6.0.0`, and a `v6.0.0`, `=v6.0.0`, or `= v6.0.0` pin was not read at all, so a resolve that
  floated an exact pin to `6.1.0` passed unnoticed. The check now uses the npm-family matcher and
  reports only a version the specifier provably excludes, `||` unions and comparator sets
  included. A newly floated pin now sends the round through the override repair, and fails it as
  a stale lock when the repair leaves the pin floated. A range cooldown cannot read, such as
  `^v6.0.0`, `>=v6`, `v6.x`, or a hyphen range, is still never reported, and a mismatch the lock
  already carried before the resolve, such as an override, stays grandfathered. The npm-family
  matcher itself now judges a prerelease beside a partial as node-semver does, so
  `1.x >=1.2.3-beta.1` admits `1.2.3-beta.2`, while `^1 >=1.0.0-alpha` still rejects
  `1.0.0-beta`, and it drops a `>=0.0.0` floor, so `^0.0 >=0.0.0-alpha` admits `0.0.0-beta`.

- **An npm alias in a lockfile v1 is read as its target, not as a version of the alias name.**
  A v1 `package-lock.json` records `"foo": "npm:other@2.0.0"` as an entry `foo` at version
  `npm:other@2.0.0`, which cooldown took verbatim, so the alias target never reached evaluation.
  cooldown now reads such an entry as `other@2.0.0` with its `resolved` origin, scoped targets
  included, and skips a `file:`, git, or tarball entry, which names no registry release. A
  release only an alias installs is never direct and takes no declared bound from the root's own
  `other` declaration, unless a genuine registry copy of the same release sits under `other`
  itself. `upgrade` and `fix` skip such a release as not eligible instead of rewriting that
  unrelated declaration, and, for any npm lock version, an alias's release no longer proves or
  hides the landing of a move of the genuine `other`.

- **A git-sourced Bun package no longer reads as a registry package that does not exist.**
  cooldown split each `bun.lock` identity at its last `@`, so `pkg@git+ssh://git@github.com/…`
  became a package named `pkg@git+ssh://git` at a version with no `:`, which the non-registry
  check let through. It now splits at the first `@` after an optional scope and keeps only
  registry identities, so git, tarball, `file:`, `link:`, and `workspace:` sources are dropped.

- **Bun workspace members are attributed their own copies, and their exact pins are protected.**
  cooldown read a `bun.lock` without its workspace members, so every dependency was the root
  manifest's, a member's own declaration was not upgradeable, and an exact pin in a member
  manifest was not checked after a sibling candidate's `bun install`. cooldown now reads each
  member's declarations from the lock's `workspaces` and binds them through the package keys the
  way Bun's own loader does: `<member-name>/<dependency>` first, then the root key. `outdated`
  and `upgrade` show and land each member's own copy, `exclude` applies to Bun members, a package
  every declaration pins exactly is held at planning (`pinned`), and a candidate whose install
  moves any member's exact pin, an excluded member's included, is rolled back as
  `resolver_conflict` naming the pin while the rest of the batch lands. This covers text lock
  versions 1 to 3; a version 0 lock and a member whose name a root dependency also carries stay
  unattributed and unprotected as before. A dependency a member also declares in
  `peerDependencies`, which Bun binds by version rather than by key, stays direct for that member
  but its pin is not protected and its landing is judged by the name's newest copy. The binary
  `bun.lockb` is still not read. Yarn locks still record no per-member
  resolution, so their exact pins are judged only before the install.

- **A Yarn Berry `yarn.lock` is read for its registry releases instead of as an empty graph.**
  cooldown read only Yarn Classic (v1) lockfiles. A Berry (v2+) lock, which opens with a
  `__metadata:` entry, matched none of the Classic grammar, so `outdated` reported nothing and
  `upgrade` found nothing to do on a project it never read. cooldown now reads the lock Yarn 2
  through 4.18 writes, lockfile versions 4 to 6 and 8 to 10, and evaluates every npm release it
  resolves, so `outdated` and `explain` cover a Berry project. An npm alias is read as
  its target's release, which no declaration of the target's name claims unless a descriptor of
  that name resolves to it too, a patched package's own npm release is read like any other, and
  workspaces and `link:`, `portal:`, `file:`, `patch:`, `exec:`, git, and tarball URL entries are
  left out, whatever version, checksum, or link type they record. A malformed lock, a descriptor
  bound to two different packages, and a lockfile version no released Yarn wrote, such as the 7
  of Yarn 4's release candidates, are reported as `lockfile_unreadable`, never read as a smaller
  graph. cooldown also no longer runs a Yarn Berry executable on a Classic lock, which its first install would migrate to
  the Berry format: before running Yarn on a Classic lock it checks the root `packageManager` and
  the version `yarn --version` reports in the project, through any `yarnPath` or Corepack, and
  refuses a Berry Yarn with what to do instead. Every Yarn run turns Corepack's auto-pin off, so
  neither that probe nor an install writes a `packageManager` field into the root manifest, and a
  lock that changed between the Classic and Berry formats after cooldown read it stops the run as
  a lock conflict before Yarn writes anything, leaving the files as they are.
- **The `excluded_moved` warning no longer credits pnpm for a move cooldown's repair makes.** The
  warning for an excluded workspace member whose copy moved under an unchanged declaration said
  pnpm's update re-resolves the package in every importer whatever the filter, and that the copy
  went to the newest version its range admits. Against pnpm 11.8.0 a filtered update left the
  excluded copy in place; the move observed came from cooldown's override repair, which runs when
  pnpm's native release-age gate rejects the starting lock or the targeted update leaves the lock
  inconsistent. The repair re-resolves the whole workspace under temporary overrides for the
  planned targets, so it lands the planned target, which can be older than the newest version the
  range admits. The warning now states only what happened (`the resolve moved chalk in pkgs/a
  (4.1.0 → 4.1.2) within the range that excluded importer declares; its manifest was not
  touched`), and says the move stayed in range only when cooldown can prove it: under a
  declaration it cannot judge, such as a hyphen range, the warning makes no range claim. The
  `upgrade` docs describe the repair. The kind, the package it carries, and when it fires are
  unchanged, and an excluded member's exact manifest pin is still refused rather than moved.

- **A literal package rule beats a glob in the same config file, whatever the table order.**
  Two matching `[package."…"]` rules of one layer used to tie, and the tie went to whichever
  pattern sorted first as text, so with `[package."@corp/*"] latest = true` beside
  `[package."@corp/ui"] min-age = "30d"`, `@corp/ui` resolved to `latest` because `*` sorts before
  `u`. Equally specific package and project rules are now ranked by their pattern: a literal beats
  every glob, a longer literal prefix beats a shorter one (`@corp/ui*` beats `@corp/*`, and beats
  `*-plugin` for `@corp/ui-plugin`), and only between equally long prefixes does the pattern that
  sorts first win. Layer authority and selector specificity still come first, and each window
  field and `max-major` is ranked on its own among the rules that set it. `@corp/ui` now waits its
  30 days. A policy with overlapping package or project globs can resolve to a different window or
  `max-major` ceiling than before, and pnpm `sync` can export or withhold a different set of
  `minimumReleaseAgeExclude` entries; run `cooldown explain <package>` to see which rule now wins.

- **npm attributes an aliased install to the package it actually installs.** An npm alias
  (`"foo": "npm:other@1.0.0"`, or an override to one) installs `other` at `node_modules/foo`, and
  `package-lock.json` records `name: "other"` there. cooldown read that entry as a release of
  `foo`: `outdated` and `upgrade` reported `foo` at `other`'s version, the pin guard judged an
  exact `foo` pin against `other`'s version, and the landing check could accept the alias's
  version as `foo` reaching its target. The entry is now reported as `other` (with its recorded
  origin) wherever cooldown lists or evaluates locked packages, and a lookup of `foo` that reaches
  the alias binds no version of `foo`: an exact `foo` pin is neither satisfied nor violated by it,
  it proves no landing and no peer hold or violation, and it still hides any farther copy of
  `foo` the way npm's own lookup does. A `foo` pin never makes `other` pinned, and the alias
  declaration is not rewritten when `other` moves. A version of `other` that only an alias
  installs is also never credited to a member's ordinary `other` declaration: with
  `"other": "^1.0.0"` beside `"foo": "npm:other@2.0.0"`, `upgrade` no longer plans the aliased 2.x
  copy as a direct move that rewrites `^1.0.0` across a major; a local or workspace package of
  the same name and version does not count as that release either. A workspace link is identified
  by its target, whose name npm omits when the directory implies it (`packages/@scope/other` is
  `@scope/other`), and a link whose target the lock does not fully record still hides any farther
  copy. Only npm lockfile v2/v3 changed; the v1
  format is unaffected.

- **npm and pnpm hold a `v`-prefixed exact pin like a bare one.** node-semver reads `v1.2.3`,
  `=v1.2.3` and `= v1.2.3` as the exact version `1.2.3`, but cooldown's planner read them as
  movable ranges: an Auto upgrade widened such a pin to `^target`, and since the pin guard refuses
  that move after the resolve, the run ended in a resolver conflict instead of the up-front
  `pin_held` skip a `1.2.3` pin gets. All three spellings are now exact pins everywhere cooldown
  plans and lands: lock pin detection, hidden install-field pins, range matching, and `--major`
  ceiling holds. Without `--rewrite` (or `fix --downgrade-pinned` for a downgrade) such a pin is
  held as `pin_held` with no resolver run; with it the pin is rewritten and stays exact, a bare
  `v1.2.3` becoming the bare target and an `=` spelling becoming `=target`, never a caret. A pin
  that already admits the target keeps its spelling. Only whole exact pins read the `v` prefix:
  ranges such as `^v1.2.3`, `>=v1` or `v1.x` keep their conservative handling.

- **pnpm `sync` writes a package-wide exemption only when cooldown exempts every release.** A
  `latest`/`allow` selector used to land in `minimumReleaseAgeExclude` whenever its own rule set
  `latest` or `allow`, so a plain `pnpm install` could adopt a brand-new major of `@corp/ui` that
  cooldown still holds back when `[min-age] major = "30d"` applies to it, or when a later layer,
  a `--min-age` flag, or another layer's `floor` overrides the exemption. A selector is now written
  only when the fully layered policy exempts the locked version and every major, minor and patch
  update of every package it matches; any other selector is withheld and reported as a
  `stricter_native` warning naming it, and when nothing else remains `sync` writes an explicit
  `minimumReleaseAgeExclude: []`. Packages can therefore lose a broad native exemption, including
  ones a withheld glob matches that are exempt on their own; the glob check is conservative and
  withholds a glob whenever a rule for one of its packages could set a window. The young locked
  versions cooldown admits, including `allow`-exempt ones, keep exact-version entries, so the
  current lock still installs; other releases of those packages wait for pnpm's own window.
  The default window `sync` writes (pnpm's `minimumReleaseAge`, uv's `exclude-newer`) no longer
  becomes `latest` because of a `[package."*"] latest = true` or `allow = ["*"]` rule; it falls
  back to the bare `min-age` then. When pnpm's window cannot be written (a `freeze` or `latest`
  default), `sync` no longer leaves `minimumReleaseAgeExclude` untouched: it drops the entries
  without versions it would not write, keeps every version-qualified entry, and writes an explicit
  `[]` when a selector was withheld or left out and nothing remains. `sync` also never removes an
  existing `minimumReleaseAgeExclude` any more: a list it empties, including one under a policy
  that no longer names any selector, stays as `[]`, so exclusions pnpm inherits from user or
  global configuration (such as `["*"]`) stay masked.

- **A resolve that moves a manifest-only exact pin is refused.** pnpm records one specifier per
  importer, so an exact pin in a second install field (`devDependencies: 20.14.5` beside
  `dependencies: ^20.11.1`), or in a member the run excludes, was invisible to the lock checks: a
  sibling candidate's resolve could move the package past it and the move was reported as
  collateral. The exact pins of every manifest the lock names (`v`-prefixed spellings included)
  are now read before the apply, whatever the plan touches and whether or not the manifest is a
  link, and a settled lock that newly violates one at its owning member is refused with the
  manifest, field, pin, and resulting version named; the recovery holds the culprit as a resolver
  conflict and lands the rest. npm judges each candidate against the state the earlier landings
  left, so only the candidate that moved the pin is rolled back. `--rewrite` and
  `fix --downgrade-pinned` exempt only the active change's own rewritten pins, judged against the
  version cooldown wrote. The guarantee covers pnpm and npm lockfile v2/v3, the formats that record
  a resolution per member; yarn, bun, and npm v1 locks attribute none, so their exact pins are not
  yet protected this way.

- **npm-family dependencies are judged by every install field that declares them.** A package in
  both `dependencies` and `devDependencies` (or `optionalDependencies`) of one manifest is capped by
  the strictest explicit bound across them, and moves lock-only only when every one admits the
  target. An exact pin beside a range, in another field or another workspace member, is no longer
  rewritten by the Auto widen: the move is reported with a new `pin_held` skip reason naming the
  pin, and `--rewrite` (or `fix --downgrade-pinned` for a downgrade) rewrites it, except that on
  pnpm a pin beside another workspace member's range stays held until it is edited. A held downgrade
  fails `fix --strict`; a held major still tries its in-line release. `explain` lists one
  declaration per distinct range. The skip-reason set is part of the JSON contract, so
  `schemaVersion` is now `7`.

- **pnpm `sync` writes only the selectors pnpm matches the same way.** pnpm's
  `minimumReleaseAgeExclude` knows only `*` and a leading `!`, so a `latest`/`allow` selector using
  other glob syntax (`@types/{node,react}`, `react-?om`, `[a-c]*`, `**/foo`) exempted different
  packages on pnpm's side. Such a selector is now left out of the native list and reported as a
  `stricter_native` warning, so pnpm's own age gate keeps applying to the packages it matches; an
  admitted exact version is dropped only when a written selector covers it under pnpm's matcher.
  When every selector is left out and nothing else remains, `sync` writes an explicit
  `minimumReleaseAgeExclude: []` instead of removing the key, so inherited exclusions stay masked.

## v0.0.27

- **Cargo upgrades no longer raise an in-range requirement by default.** Without `--rewrite`, a
  widen leaves a requirement that already admits the target as written: `shared`'s `pest = "2"`
  stays `"2"` for pest 2.9.3, so a nested workspace that resolves the same path crate keeps a
  valid lock. A precise pin that another locked crate blocks (pest 2.9.3 pins
  `psm = "=0.1.24"` while `stacker` holds psm 0.1.32) is retried with that crate released, for a
  bounded number of newly named blockers, and lands only when every planned target stays put; the
  released crate's move is reported as a collateral row. A crate the project declares directly is
  never released outside the plan, so the pin is held instead.

- **Cargo diagnostics stay plain under `CARGO_TERM_COLOR=always`.** cooldown runs cargo with color
  turned off, so the conflicts it parses and the details it reports carry no escape codes when the
  environment requests color, as CI runners commonly do.

- **A lock another project's manifest edit leaves unresolvable is repaired in the same run.** A
  lock current at the start of the run that later fails to resolve after one of the manifests it
  reads changed is re-resolved by releasing only the locked crates Cargo's conflict names, then
  gated by a `fix` pass like a lock the run staled. Previously the project failed with "cannot
  faithfully isolate this Cargo project".

## v0.0.26

- **uv honors package-level `min-age` rules.** A `[tool.uv.package."<name>"]` rule, or an
  unqualified `[package."<name>"]` one, now reaches uv's own per-package cutoff. `sync` writes it
  into the repository `uv.toml` as `exclude-newer-package` beside `exclude-newer` (a `0d`,
  `latest`, or `allow` package as `false`) and removes the entry again when the rule goes away.
  Every uv command cooldown runs passes the same `--exclude-newer-package` overrides, so `check`,
  `fix`, and `upgrade` no longer fail to resolve an exempted release that uv previously filtered
  out under the default window.

## v0.0.25

- **Previews and dry runs stage declared npm patch files.** `outdated`, `explain`, and
  `upgrade --dry-run` resolve in a throwaway copy, which now includes every patch named by
  `patchedDependencies` in `pnpm-workspace.yaml` or `package.json` (pnpm and bun). pnpm hashes
  those files while resolving, so a project with patched dependencies previously failed every
  trial with `ENOENT` and reported each candidate as blocked.

## v0.0.24

- **Cargo retries reuse rejections for identical resolution inputs within a run.** Tentative
  widens, individual precise pins, and failed seeds skip Cargo when the complete manifest, lock,
  follower, and Cargo configuration bytes match a restored resolver rejection. Keys include the
  original project root, ancestor and Cargo-home configs, referenced local inputs, resolution
  environment, and metadata mode. Uncaptured source directories, optional input read failures,
  and non-resolver failures bypass reuse. Reused pins retain Cargo's explanation; interruption
  and topology checks still run. Debug logs count memo hits and misses for pin batches and whole
  graph resolves. The global `--no-memo` flag
  (or `COOLDOWN_NO_MEMO=true`) disables rejection reuse while retaining HTTP caching;
  `--fresh` / `--no-cache` disables both.

- **Cargo major upgrades widen candidates together before retrying individual pins.** Each widen
  round seeds all remaining targets in one resolved transaction, including targets refused under
  the previous manifest constraints. Partial landings are restored and replayed with only their
  retained manifest edits; earlier member targets stay protected through subsequent individual
  widens and no-op pin batches. Failed probes and individual widens restore exact manifest and
  lock bytes, permissions, and captured absence, while individual retries still provide Cargo's
  rejection explanations. Debug logs report round and process counts plus input fingerprints for
  individual attempts; fingerprint hashing runs only when debug logging is enabled.

- **An interrupted run rolls back instead of leaving its trial state behind.** A signal killed
  cooldown wherever it was, so `Ctrl-C` during a pnpm `upgrade` left the temporary resolver
  overrides in `pnpm-workspace.yaml` (re-quoted, with the block's comments gone), and any in-place
  trial left its half-applied manifests and lock. The first `SIGINT`, `SIGTERM`, or `SIGHUP` now
  stops the package manager cooldown is running together with its descendants (killing them after
  10 seconds if they ignore the request), refuses to start another or to accept, publish, or sync a
  result, and lets the run unwind through its normal rollback before exiting `130`. A second signal
  quits immediately.

- **pnpm no longer hangs on an invisible prompt.** Run from a terminal, cooldown handed pnpm the
  terminal as its input while capturing its output, so when pnpm 11 asked whether to add an
  immature version to `minimumReleaseAgeExclude`, the question never appeared and the run waited
  forever. Package managers now run with no input, so pnpm fails the step instead of asking, which
  cooldown already handles.

- **pnpm keeps exact exemptions for releases admitted by cooldown.** An upgrade through a
  shortened security or package window could leave a young version in `pnpm-lock.yaml` that
  pnpm's native `minimumReleaseAge` rejected, failing final verification and a developer's next
  install. Successful settlement now persists version-qualified `minimumReleaseAgeExclude`
  entries with the lock, and rejected trials restore both files. `sync` evaluates the current
  lock with the check policy, including advisories and baseline acknowledgements, to retain
  needed entries, add missing ones, and remove obsolete ones. Tolerated transitive violations
  receive no exemptions. Evaluation failures warn and preserve existing exact entries while
  still syncing the window and policy globs.

- **Cargo pin batches use one seeded resolve first.** Planned targets are seeded into `Cargo.lock`
  together and reconciled once. Exact landings and collateral dependency updates are kept; a
  seeded target landing at an unrequested version or a resolver failure restores the validated
  pre-seed state and splits the batch deterministically. Only failed leaves, lone changes, and
  targets left at their original version need individual precise pins, preserving Cargo's
  per-candidate rejection explanations, and a target refused on its own stays out of later seeds
  for the rest of the run instead of failing and bisecting every batch that carries it. Generated
  workspace-hack edits follow verified landings only. Local failures and interruptions propagate without bisection, and paths rejected by the
  mutation journal are never used for seed restoration. Progress counts native resolver invocations;
  debug logs report resolve, seeded, landed, bisected, and per-crate counts.

- **Cargo upgrades settle on a cooled, coherent graph.** Before holding an upgrade for fresh
  transitives, `upgrade` tries the existing `fix` cascade and re-plans until fresh versions are
  pinned to mature releases or the graph stops changing. If the target still cannot land, it tries
  up to three joint lower-target trials for the rejected candidates, then up to three individual
  trials per rejected candidate when joint trials cannot settle the group. Every fallback retains
  the accepted upgrades and reports the version that lands. A batch that introduces an incompatible
  line of an authored member's direct dependency is held with the crate and versions named; generated
  workspace-hack declarations do not drive that guard, and different package sources are judged
  separately. `--fail-on-new-duplicate` also gates Cargo's growth in distinct resolved versions.
  The residual cooldown gate compares exact package/version identities, so replacing one fresh
  version with another no longer passes just because the number of violating copies stayed
  constant.

## v0.0.23

- **`include-hidden` scans named dot-directories.** Detection skips every directory whose name
  starts with `.`, so a repository keeping real projects under `.github/scripts` or an agent-tooling
  directory had them silently left out of every run, including the `check` gate.
  `include-hidden = [".agents", ".github"]` under `[global]` or a `[<command>]` table (or
  `--include-hidden <glob>`, which replaces the config list for one run) walks those directories
  like any other. Patterns use the `exclude-folders` `.gitignore` semantics and merge the same way,
  and each one is matched against the dot-directory itself, so a virtualenv or cache nested inside
  (`.github/scripts/.venv`) stays skipped unless it is named too. `.git` is never scanned,
  `exclude-folders` still prunes an included directory or anything below it, and gitignore handling
  is unchanged. Without the key, detection is exactly as before.

## v0.0.22

- **A requirement rewrite keeps the trailing comment on its line.** `upgrade` replaced the whole
  value of a bumped entry, which dropped a same-line comment such as
  `html5ever = "0.39"  # must match markup5ever_rcdom` — often the only record of why the
  requirement is what it is. The Cargo manifest rewrite and the `sync` value writer now carry the
  old value's spacing and trailing comment over to the new one.

- **A lock the run staled itself no longer fails `upgrade` or `fix`.** In a monorepo whose
  cargo-fuzz crates and nested workspace reach root workspace members through `path` dependencies,
  `upgrade --major` rewrote the root's `[workspace.dependencies]` (`html5ever = "0.39"` to
  `"0.40.1"`), which staled the other projects' locks mid-run; each then failed with
  `Cargo.lock is stale in …/incubator` and the run exited `4`, although every lock had been current
  when it started. A source-mutating run over several projects of one tool now records which locks
  are current before anything moves (for the tools `--lock` can refresh: cargo and pnpm). A
  recorded lock found stale at its project's turn is refreshed with the `--lock` command, reported
  as a `stale_lock` warning naming the cause, and gated by a `fix` pass under the run's
  `--transitive` mode before the requested pass, since the upgrade would otherwise accept whatever
  too-fresh release the refresh resolved as its baseline. A lock a later project stales after its
  own turn is caught by a probe once every project has run, and its project runs again the same
  way, at most once per recorded project before a lock still stale is reported as an error. A lock
  that was stale before the run began keeps the ordinary `stale_lock` handling, and `--dry-run` is
  unaffected.

- **A Cargo project with no `Cargo.lock` no longer escapes the gate.** Detection keyed on
  `Cargo.lock`, so a `Cargo.toml` without one was either not a project at all (`no supported tool
  detected`, exit 3) or — nested under a detected root — silently assumed to belong to it, and
  `check` reported green over dependencies that were never evaluated. The confirmed case is a
  workspace whose `fuzz/` crate declares its own `[workspace]` and gitignores its lock, as
  cargo-fuzz does: a crate with no lock resolves to the newest release of everything at build time,
  which is precisely what cooldown exists to prevent. The marker is now `Cargo.toml`, and which
  `Cargo.toml` is a project of its own is decided by cargo's own ownership rules
  (`Workspace::find_root`, plus the `path`-dependency closure) rather than by where a lock happens
  to sit. A nested manifest is a project of its own unless the resolve of a project the run
  evaluates already covers it, computed from those projects outward: a workspace resolves its
  members — the directories its `members` globs match, the root package when the root manifest
  declares `[package]`, and the in-workspace `path` dependencies those pull in, which cargo makes
  members too, whether they lie inside the workspace directory or outside it with their own
  `package.workspace` pointing back at this root, minus whatever `exclude` names unless `members`
  names it as well — and then every `path` dependency those members reach, through
  `[dependencies]`, `[build-dependencies]`, and the `[target.<cfg>.…]` forms. A `members` entry
  spelled `./a` or `a/` means the directory `a`, and a member's `dep = { workspace = true }`
  resolves through the root's `[workspace.dependencies]`. Two kinds of edge stop at the members.
  `[dev-dependencies]` count for members only, since cargo locks its members with dev units enabled
  but activates everything beyond them as an ordinary dependency with dev-dependencies off, so a
  crate reachable only as a reached package's dev path dependency is in nobody's lock and is a
  project of its own. An `optional` path dependency of a reached package is not followed either —
  that one is cooldown's own rule, not cargo's: the edge is skipped whether or not a feature
  enables it, because only the resolve knows which features are on, so such a crate is gated on its
  own lock (reporting a missing one rather than passing silently) or dropped with
  `exclude-folders`. (The mirror is a known limit: a non-member's own `[dev-dependencies]` are in
  nobody's lock either, and cooldown gates the crate through the project that resolves it rather
  than gating what only its standalone test build would fetch.)
  A plain `[package]` is the single member of its own implicit workspace and resolves the same
  closure. Only projects the run actually evaluates can cover anything: a workspace that was
  pruned, ignored, or never scanned covers nothing. A manifest
  declaring `[workspace]` is always a project, since cargo forbids a workspace root from being
  another workspace's member; a manifest an enclosing root neither lists nor `exclude`s stays with
  that workspace instead, because cargo refuses to build such a crate standalone and nothing else
  resolves it, which is exactly where fixture crates commonly sit; and a `package.workspace`
  pointer at a root that does not list it is a project of its own rather than a silent member.
  Below a detected project a vendored crate (`cargo vendor` output, marked by
  `.cargo-checksum.json`) and `cargo package`'s unpacked output under `target/package` are never
  projects: cargo loads neither as a package of the surrounding build. `-C`/`--dir` follows the
  same rule: pointing the run at a directory another project resolves now runs that project,
  wherever its root sits, instead of the project that merely encloses the directory — which could
  read a lock saying nothing about it.

  Two green-to-red transitions follow, both deliberate. A repository whose only Cargo project
  gitignores its lock now fails with exit 4 and a `stale_lock` error where it previously said `no
  supported tool detected` (exit 3), and one that also holds a non-Cargo project now fails where it
  previously passed. And a crate the enclosing workspace `exclude`s is a project now whether or not
  it has a lock — the ownership rule decides detection, not the lock — so an excluded fixture,
  example, or fuzz crate that *does* have one is gated for the first time and can surface
  violations in a repository that was green yesterday. The `stale_lock` error names the project and
  the remedy: run `cargo generate-lockfile` there and commit the lock, then `cooldown fix` to mature
  the fresh resolve, since a lock generated from scratch resolves everything to its newest release
  — or drop the directory with `exclude-folders` if the crate is never built. `check --lock` and
  `outdated --lock` generate the missing lock in place. `--allow-stale-lock` downgrades the failure
  to a warning saying the project went unevaluated, and now does so on `upgrade` and `fix` too:
  before, those still failed on a lock met before any mutation, contradicting the flag's own
  documented promise. `baseline` still fails on it, since skipping a project there would let
  `--prune` drop that project's acknowledgements.

  For adapter authors: `ToolRead::nested_lockfile_root_escapes` became
  `ToolRead::nested_ownership(primary: &[Utf8PathBuf], nested: &[Utf8PathBuf]) ->
  Vec<NestedOwnership>` — handed both sides of the scan so it can only answer from projects that
  exist, batched so an adapter can memoize its manifest loads across a whole workspace's members,
  and answering `Enclosing` / `Standalone` / `Root(path)`, where the named path must be one of
  `primary` or a directory the same batch answered `Standalone`. `ProjectDetection` collapsed into `ProjectMarker`
  (`ToolRead::project_marker`, field `marker` rather than `lockfile`), and
  `validate_manifests_without_lock` is gone — a `resolver.lockfile-path` project is still rejected,
  now per project at run time rather than at detect time. Left for later: bundler, hex, and swift
  have the same shape (an unambiguous manifest without its lock is silently not a project); the
  npm-family and pyproject-based tools cannot, since a lockless `package.json` or `pyproject.toml`
  does not say which tool owns it.

## v0.0.21

- **A cargo-hakari workspace-hack follows upgrades instead of being one.** A workspace-hack's
  `[dependencies]` are generated from the lock — one entry per crate and compatibility line,
  hash-aliased when several majors coexist — and cooldown read every entry as an ordinary direct
  dependency: each `0.x` line was a `--major` candidate, each candidate was attributed to the hack,
  and the resolve then reported each one `blocked`, since the version is decided by whichever real
  crate pulls it in. On a 445-member workspace that was 24 of 30 blocked rows. A member declared
  in the new `[tool.cargo] generated-members = ["workspace-hack"]` is a follower: nothing it
  declares is direct (a crate only it declares is a transitive of the members that reach it, still
  gated by `check`), nothing it declares holds a row, and when `upgrade`/`fix` moves a crate an
  authored member declares, the hack's entry for that version — hash-aliased or not — follows in
  the same step (bumped, or dropped when the sibling entry for the target major already projects
  that line, as a regeneration would merge them), so `cargo metadata --locked` still passes and
  no crate gains a second copy for the projection's sake — including a companion the move drags
  to another line (`toml_edit` behind `toml`), whose projection follows after the pin phase. A
  run that rewrote a projection ends
  — and the hack's own edges moving is that follow, so they are no longer reported as `rebound`
  edge rows (the real sweep's 11 rebound rows were all the hack's). Such a run
  with a `stale_lock` warning telling you to regenerate it (`cargo hakari generate`); a dry run
  or `outdated`'s policy preview says it would. Detection is never inferred:
  the declaration is the only way in, by exact member name, and a name that matches no member is
  a config error in every command, so a stale entry cannot quietly stop covering a manifest. An
  undeclared member whose manifest carries the `### BEGIN HAKARI SECTION` marker earns a hint (an
  explicit `generated-members = []` silences it), a declared member whose projection declares
  crates nothing authored reaches any more is reported stale, and `cooldown config` prints the
  resolved list with the file that declared it (`--json`: `generatedMembers`). Adapters gain
  `ToolRead::manifest_notices` and a `generated_members` capability; the JSON `configItem`
  gains an optional `generatedMembers` object.

## v0.0.20

- **Held rows say why.** A candidate the whole-graph resolve kept below its target and one the
  resolve rejected outright shared one sentence, `the resolver rejected this change`, and the
  rejected one's explanation was cut at the resolver's first line, so a `--json` `blockedReason`
  ended at `No solution found when resolving dependencies:`. The two outcomes are now distinct
  skip reasons: `graph_ceiling_held` (the resolve succeeded; another package's requirement caps
  the candidate, and the way forward is that package maturing or moving) and `resolver_conflict`
  (the resolve failed with the candidate in it). A rejection carries the resolver's whole
  explanation from its failure sentence on, folded onto one line, so uv's `Because
  litellm==1.91.5 depends on openai>=2.20.0,<3.0.0 … your project's requirements are
  unsatisfiable` reaches the row and `blockedReason`. A uv ceiling names the capping package
  with the requirement and marker it declares, read from the lock for a workspace source and
  from the release's PyPI metadata for a registry package, since a modern `uv.lock` records only
  the resolved edge for those; a cap two packages share no longer falls back to an unattributed
  message, and a cap the candidate's own release creates names the sibling it cannot have. When
  one of the capping package's next releases lifts the cap, the row says which release and where
  it stands against the window. The ceiling reason applies to every whole-graph adapter (cargo,
  go, pnpm, deno, and the app-side landing check), so a JSON consumer that keyed the held set on
  `resolver_conflict` alone must accept `graph_ceiling_held` too.

## v0.0.19

- **Ecosystems run in parallel.** `outdated`, `check`, `upgrade`, `fix`, and `baseline` drove
  every project one after another, so a polyglot repository waited on crates.io, then PyPI, then
  the npm registry in turn. Each detected tool now runs in its own lane and the lanes run
  concurrently, whether the tools live in different directories or share one; a lane's own
  projects still run in order, since concurrent invocations of one package manager block on its
  own cache lock. Only tools that rewrite the same manifest take turns under `upgrade`, `fix`,
  or `--lock`: uv and poetry both own `pyproject.toml` and every npm-family tool owns
  `package.json`, so two of them at one root, or one nested in the other's workspace, run in
  turn (`--dry-run` mutates a throwaway copy and reads side by side too). To make that true the
  lease that serializes cooldown runs on a project is now held per manifest family rather than
  per directory, so a cargo run and a uv run at one root never wait on each other, in one
  process or two, while uv and poetry still do. Each adapter declares the file its lease
  guards, so pixi, which is detected by `pixi.lock`, takes turns with uv and poetry when its
  tables live in `pyproject.toml`. The lease file is now named for the directory and the
  family, so a cooldown older than this release does not see this release's leases, nor the
  reverse: do not run the two side by side in one repository. `--build` steps run one at a
  time across ecosystems, in this process and against other cooldown processes at the same
  directory, since every ecosystem installs into the environment shared there (`node_modules`,
  `.venv`); so do the upgrades of tools whose native command installs as it pins (`poetry add`,
  `conda install`, `bundle update`, `mix deps.update`, `swift package update`). `--jobs <N>` (config `jobs`, `COOLDOWN_JOBS`)
  caps how many tools run at once; `--jobs 1` runs them one after another. Reports, their
  order, and the exit code are unchanged (`baseline` now evaluates every project before
  reporting a failing one). The interactive progress display shows one block per live tool,
  and the plain transcript names the tool and project on every per-project line.

- **`explain <pkg>` explains the decision, not only the window.** `explain` printed the
  min-age layer table and nothing else, so four packages blocked for four different reasons
  produced byte-identical output, and the only way to learn why one was blocked was a full
  `upgrade --dry-run` over the whole graph — four minutes on a 2100-package workspace — to read
  one row. `explain` now leads with the decision: one `verdict:` line per resolved version (the
  `outdated` row — status, current and adoptable versions, blocker, cooldown position) and, for a
  blocked one, `reason:` with the sentence `upgrade` prints, computed by the same evaluation and
  upgrade-policy verification `outdated` runs (the resolver against a throwaway copy), scoped to
  the one package so it takes seconds rather than minutes — judged on its own, so a hold that only
  arises between two candidates of one batch shows in `outdated` but not here, and skipped under
  `--offline` with a note saying so (`outdated` now says so too); then `declared by:`, every
  member's declared range, resolved version, and manifest fields (`[peerDependencies]` shows a
  peer-only declaration), with the excluded members marked, manifest-only declarations included;
  then the window trace as before. `--json` carries `verdicts` and `declarations` beside
  `excludedMembers`, which 0.0.18 already emitted as a top-level key — the verification's
  "appears for none of them" was run with the 0.0.17 binary on `PATH`.

- **Every blocked `outdated` row carries its reason.** In one table five rows were `blocked` and
  only `typescript` said why (`blocked by eslint-config-next`); `pdfjs-dist`, `solid-js`,
  `tailwind-merge`, and `zustand` were blocked for four different reasons `upgrade` states
  precisely, and the bare status read as "no known reason". Each blocked row now carries the
  reason the upgrade resolve gave — the same sentence its skip row prints — as `blockedReason`
  in `--json`, and `outdated --why` prints it below the table. The status column keeps naming the
  blocker when one was named; the reasons are sentences too wide for it, so the flag is the
  answer on the terminal.

- **A new duplicate copy can be refused, and its requirer is named.** After `solid-js` was
  correctly rolled back, upgrading `vite-plugin-solid` pulled a *second* `solid-js` into the graph
  and the run committed the lock with only a warning: `@solidjs/start`, used by four production
  apps, bound 1.9.15 while the apps bound 1.9.14 — two Solid runtimes with separate owner/observer
  registries, breaking reactivity with `tsc` and the test suite green. The detection was right and
  the severity wrong for such packages. `[tool.pnpm] single-copy = ["solid-js", "react"]` now
  names the packages that must stay single-copy: a settlement that adds a second copy of one is
  refused, the lock restored, and the candidate whose landing added it held with the copy named,
  exactly as a partial landing is — even when the second copy is the one an exclusion asked for,
  and when the name was absent before the run and arrives at two copies at once, held by
  dependents or by importers. A listed name already at several copies before the run has that
  standing split reported rather than refused, since no candidate introduced it; a copy a run adds
  beside it is refused like any other, and a copy that merely moves to another version is not,
  since the count of distinct copies is what the gate judges. `--fail-on-new-duplicate` (config `fail-on-new-duplicate`) gates every package for
  a run without naming any, and says so when the run's tool has no settlement guard to honor it.
  The list merges across config files like `exclude-folders` (a plain array adds, `[]` clears,
  `{ replace = […] }` replaces), so a nested workspace listing one runtime of its own cannot
  un-gate the root's; entries are exact package names, and a glob such as `@scope/*` is a config
  error rather than a gate that silently matches nothing. The warning for an ungated copy now names the requirer from the settled lock's own
  edges (`required at 1.9.15 by vite-plugin-solid@2.11.14`) instead of `pulled in by another
  package's requirement`, which had forced a hand-grep of the `snapshots:` section. Names in
  `pnpm-workspace.yaml`'s `overrides` are deliberately not gated by default: an exact override
  already keeps its copy single, and a ranged one is as often about forcing a patched transitive
  as about running a package once — the default is unchanged, list the names you mean.

- **A partial pnpm landing names its structural cause.** Three rollbacks in one workspace shared
  one cause the row did not name: an importer declared the package only in `peerDependencies`,
  which pnpm auto-installs and records in that importer's lock entry, but `pnpm update` has no
  install field to advance, so its copy stayed at 5.0.14 while every sibling took 5.0.15 — and
  `--rewrite` was inert, correctly, since the declared range already admitted the target. The
  row now checks the importers left behind and says `` @airtype/pdf-view-solid declares zustand
  only in `peerDependencies`, which `pnpm update` cannot advance; declare it in `devDependencies`
  as well to let it move `` (an application, `private: true`, is told to move the entry to
  `dependencies` instead). The rollback itself is unchanged.

- **A second copy the resolve adds is a `duplicate_copy` warning, not a `held` one.** An
  `outdated` run summarized `8 held` while its warning block held thirty `warning [held]` lines,
  and a reader reconciling the two could not: the summary counts rows whose status is `held`
  (pins, bounds, ceilings), while every one of those warnings was a new duplicate copy the
  preview's resolve had committed (the `@rollup/rollup-*` platform packages at two versions after
  `rollup` was rolled back) — nothing held, a copy added. Those warnings, and the one for an
  excluded importer left on the old copy, now carry their own `duplicate_copy` kind, and the
  warning for an excluded importer whose copy pnpm re-resolved within its own range carries
  `excluded_moved` — a move, not a hold. The diagnostic-kind set is part of the JSON contract, so
  `schemaVersion` is now `6`.

- **`check`'s stale-lock error names the project.** `upgrade` reported `Cargo.lock is stale in
  /repo/incubator; run …` while `check` said only `Cargo.lock is stale; run …` for the same
  condition — in a repository with five Cargo locks (a parked nested workspace, standalone fuzz
  and wasm projects) the one to refresh had to be inferred from the dependency count dropping
  between runs. The lock-currency probe's detail now names the project root for cargo, go, uv,
  and the npm family (`pnpm-lock.yaml is stale in /repo/apps: <pnpm's own reason>`), in the
  error, in the `--allow-stale-lock` warning, and in `outdated`. A detail probed in a throwaway
  copy (`--dry-run`, the `outdated` preview) names the source project, not the temp directory the
  copy lived in.

## v0.0.18

- **A pnpm pin that lands in only some importers is rolled back, never committed half-applied.**
  The joint `pnpm update` could land a candidate in most importers and leave a peer-bound one on
  the old version, then report the candidate as a resolver rejection while committing a lock that
  held both copies — a duplicate `solid-js` no row accounted for, invisible to `lock re-verified`
  and to `check`. The apply now judges every exact pin per importer: a partial landing rejects
  the candidate, restores the lock, and
  re-resolves the rest of the batch, with the skip row naming the importers that took the target
  and those that did not. A package the resolver splits on its own — floated in only some
  importers without being pinned — is refused with the split named, so candidate isolation holds
  the responsible candidate instead of certifying a lock with a new duplicate copy; a
  split whose managed importers agree on one version, with only excluded importers left behind on
  the old one, is committed and reported as a warning naming them. A candidate now counts as
  landed only when *every* declaring importer reached the target.
- **pnpm workspace splits are judged against the target.** Two importers declaring a package under
  different ranges (`^2.11.12` and `^2.11.13`), or resolving one range at two versions, were held
  as "declared with incompatible ranges" even when every range admitted the matured target — seven
  in-range updates in one sweep, two of them with open Dependabot PRs. A name now splits only when
  some declared range provably excludes the target or cannot be judged (a `||` union, a hyphen
  range, a dist tag, a bare exact version): `^4.17.20` beside `^4.17.21` takes `4.17.22`, while
  `~7.3.0` beside `^7.0.0` still holds `7.4.0` and `^3.4.19` beside `^4.3.3` still holds the v4
  line. The hold's row now names the ranges that exclude the target and the action that converges
  them. A line declared through a `catalog:` specifier is held with the catalog row while a sibling
  line on a plain range that admits the target still lands, and a git or tarball resolution in one
  importer no longer counts as a version line beside a registry one. A second copy the resolve adds below
  the importers' view — a transitive requirement pulling an older line — is now named by a warning
  instead of passing silently.
- **`exclude-folders`/`exclude-packages` reach the pnpm split evidence.** An excluded importer's
  declaration still counted toward the workspace split (two excluded importers on `mongoose@9.8.0`
  vetoed the one managed importer's `9.9.3`), and could still be reached by a pin. Excluded
  members now travel with every mutation plan: they contribute neither a range nor a resolved
  version to the split judgment, are never named in the update, their manifests are never
  rewritten (the workspace root's included, which the widen path used to treat as an owner of
  every declaration), and their exact pins and declared bounds no longer hold or loosen a managed
  row. `pnpm update` itself re-resolves the named package in every importer whose range admits a
  newer version whatever the filter (verified against pnpm 10: the filtered importers get the exact
  target, every other one the newest version its own range admits under the release-age floor), so
  an excluded importer's copy can move too; that move is committed and reported as a warning naming
  the importer and the package rather than blocking the upgrade, which would reinstate the veto. A
  settlement that changes an excluded importer's entry beyond that (a changed specifier, an entry
  appearing or disappearing, a version its range provably excludes) is refused and the responsible
  candidate held with the drift named.
  `explain <pkg>` lists the
  excluded members declaring the package (`excludedMembers` in `--json`) instead of dropping them
  silently. With the target-aware split rule above, removing the exclusion no longer restores the
  hold either: an importer on the same range resolved at another version converges with the rest.
- **`--rewrite` converges a genuine pnpm split.** The split hold ran before the rewrite mode was
  consulted, so `--rewrite --major` on a `^2.6.0`/`^3.6.0` split was byte-for-byte identical to
  the run without it and the row named no way forward. Under `--rewrite` a split name is now
  pinned to its target and every declaring importer's range widened to admit it; without it the
  hold stands and its row names `--rewrite`. A name planned at two targets (a `^22` and a `^25`
  line each advancing within itself without `--major`) cannot be pinned by one joint update — even
  when one permissive range admits both targets — and stays held with a row saying what would
  converge it rather than blaming the ranges.
- **The advisory feed covers pnpm workspaces.** Every pnpm package was withheld from the feed
  because `pnpm-lock.yaml` records no per-entry `resolved` URL. A lock entry served from the
  configured registry by name carries only `resolution: {integrity: …}` (a `tarball`, `repo`/
  `commit`, or `directory` field marks anything else), and `pnpm config list` states the effective
  registry, so pnpm identities are now granted from that pair: the entry must be registry-shaped,
  no readable `.npmrc` or `pnpm-workspace.yaml` routing key may reroute it (the workspace file
  ranks above the project `.npmrc`, as pnpm ranks them), and at feed time pnpm's
  effective `registry` must be stated and public — an unstated registry or a failing query
  withholds every package, an `@scope:registry` override only its scope, all through the existing
  `advisory_ecosystem_unsupported` warning.

## v0.0.17

- **Cargo's lock-currency probe no longer fails on a checkout whose crates are not cached.** The
  probe behind `check`/`outdated` ran `cargo metadata --locked --offline`, but `cargo metadata`
  reads every package's manifest, so on a fresh checkout (a CI runner that had not built yet) it
  failed to download the crates and reported a tool failure, and in offline mode cargo narrows
  the resolver to cached versions, which turned a genuinely stale lock into a resolver conflict
  instead of the stale-lock diagnostic. The probe now runs the same `cargo metadata --locked` the
  graph read uses, so a stale lock is reported as stale and a current one is read after cargo
  fetches what it needs.
- **Running from an excluded directory scans it instead of reporting nothing.**
  `cooldown -C incubator check` (or `cd incubator && cooldown check`) under a repo-root config that
  lists `incubator` in `exclude-folders` pruned the directory during detection, scoped the
  enclosing project to zero members, and exited clean having checked no dependency at all. Naming
  a directory now outranks an exclude glob: the selected path and the directories leading to it
  (dot-directories included) are never pruned, the workspace member containing it is exempt from
  `exclude-folders` and `exclude-packages`, members below it (`-C crates`) are in scope unless a
  glob matches them below the selection, and dependencies the tool attributes to no member (a Go
  module's, or the transitive rows of tools that attribute only direct dependencies) stay in scope
  for every command, so a selection never hides a transitive from the gate and `fix`/`upgrade`
  under `-C <member>` may move one only a sibling member needs. A selection the run cannot
  honor is a usage error instead of an empty result: one the run's own `--exclude-folders` names,
  one a `.gitignore`/`.ignore` rule hides (the error names `--no-gitignore`), or a `-C` path that
  is not a directory. A selection nothing covers evaluates zero dependencies and says so with a
  config warning.
- **Exclude lists can be cleared or replaced by a nearer config.** A plain
  `exclude-folders`/`exclude-packages` array still adds to the list it inherits from a
  lower-precedence file or from `[global]`; an explicit `[]` now clears that list (it was a silent
  no-op), and `{ replace = [...] }` swaps it for the given patterns (`{ extend = [...] }` is the
  explicit spelling of the plain array; an empty `extend` is rejected as a likely mistake). Each
  key merges on its own — a `[tool.cargo]` replacement affects only the inherited `[tool.cargo]`
  list, and a `[outdated]` replacement shadows `[global]` for `outdated` alone — and the sections
  resolve within each file before the files fold, so a repo-root `[outdated]` replacement drops
  everything the global config contributed for `outdated`. Two alias tables of one tool that both
  set the same exclude list, and an exclude list in a nested `cooldown.toml` (which sets policy
  only), are config errors now instead of one table silently winning or the list being silently
  ignored.
- **`check --lock` and `outdated --lock` refresh `Cargo.lock`.** The cargo adapter had no
  standalone lock refresh, so the flag was a silent no-op for Rust projects and a stale lock still
  failed closed. It now runs `cargo update --workspace`, the minimal refresh: a missing lock is
  generated and the workspace members' own requirements re-resolved, while a locked version the
  manifests still admit stays where it is — the refresh never floats the existing graph; what a
  manifest change invalidates is re-resolved or dropped, and what it adds is gated like everything
  else. Only the projects the run evaluates are
  refreshed (`-C incubator` leaves the enclosing workspace's lock alone). A refresh cargo rejects
  is an error even under `--allow-stale-lock`, `--lock --offline` is rejected up front as a usage
  error, the flag remains a no-op under `--dry-run`, and a tool without a standalone refresh now
  says so with a warning instead of silently reading the existing lock.

## v0.0.16

- **`fix` plans co-moving families instead of reporting them graph-held.** A family of too-fresh
  versions whose members' requirements floor each other (icu's libraries beside their exact-pinned
  data crates) held itself in place: every member was reported "the resolved graph requires that
  version" while the floors came from siblings `fix` wanted to move — a hold conditioned on the
  very resolution being questioned. The cargo adapter now attributes each graph floor and exact-pin
  ceiling to its requirer node, and fix planning discounts constraint edges whose requirer is
  itself a violation in the same round: the whole family enters one plan. When the per-package pin
  passes cannot land such a group (icu's tilde requirements admit no member-by-member order), the
  adapter seeds every rejected planned node at its target in the staged lock and reconciles the
  family with one resolver invocation — membership changes included: a crate that exists only in
  the fresh era drops out of the lock with it, and a failed reconcile restores the lock and keeps
  the recorded rejections. A genuine hold — a *compliant* requirer demanding the too-fresh version — is still
  left in place, and its warning now names that requirer and its requirement instead of the generic
  graph-held text. Applies to `fix` and to `upgrade`'s transitive reconcile alike.
- **A downgrade no longer strands a dependent's lock edge on an old coexisting release line.**
  When a planned downgrade replaced a node (`uuid 1.25.0` → `1.24.0`) beside a surviving old line
  (`0.8.2`, kept alive by other dependents), cargo's incremental re-resolve could rebind a
  wide-ranged dependent's edge (diesel's `uuid >=0.7, <2`) onto the old line — a build-affecting
  move `cargo metadata --locked` accepts silently. The `preserve` edge policy (the default) now
  restores line continuity: an edge whose binding target vanished is rebound to the unique
  surviving same-line successor whenever the dependent's declared requirement admits it.
- **Lease acquisition waits for a busy project lock instead of failing immediately.** Concurrent
  `cooldown` invocations against one repository (a task runner fanning per-tool gates out in
  parallel) raced on the per-project lock and the losers died with `lock conflict`. All project and
  repository-resource leases now wait for a foreign holder — up to 10 minutes by default,
  configurable via the `COOLDOWN_LOCK_WAIT` environment variable (`"30s"`, `"10m"`; `"0"` or
  `"none"` for the old fail-fast) — printing one `blocking waiting for …` note (naming the
  recorded holder where the blocker is necessarily the exclusive owner). Contention with a
  lock the same process already holds still fails immediately (waiting on oneself never ends), and
  the recovery entry point keeps fail-fast semantics: a live concurrent run means there is nothing
  valid to recover.

## v0.0.15

- **Nested cargo workspaces the enclosing workspace excludes are projects of their own.** Cargo
  project detection kept only the topmost `Cargo.lock` directory, so an excluded nested workspace
  (a monorepo's incubator workspace, a cargo-fuzz project, a nested wasm guest workspace) was
  invisible: its dependencies escaped the cooldown entirely, and a run from inside it scoped the
  enclosing project to zero members and reported an empty, healthy-looking result. The scan now
  carries the dropped nested roots and appeals each one to the new
  `ToolRead::nested_lockfile_root_escapes` hook; the cargo adapter accepts a nested root exactly
  when its manifest declares a top-level `[workspace]` table — a shape cargo forbids from being a
  member, so the enclosing resolve never covered it. Mutation staging learned the matching
  topology: a staged package outside the selected workspace now brings the workspace-root manifest
  its `workspace = true` keys inherit from, instead of dying in the isolated copy with "failed to
  find a workspace root".
- **`upgrade` now advances matured in-range transitives** (the Dependabot-alert class no direct
  pin drags): planning walks the whole resolved graph by default, each transitive advancing within
  its major line, gated by the new `ToolWrite::supports_transitive_advance` capability
  (default `false`). Cargo, Go, and uv advance through their existing pin engines; pnpm advances
  packages no importer declares through temporary major-line-qualified `pnpm-workspace.yaml`
  overrides validated by an override-free settlement install; every other adapter (npm, yarn, bun,
  deno, pip, …) keeps direct-only upgrade planning. A transitive whose dependents exclude its
  matured release, whose name importers declare elsewhere, or whose name resolves to several graph
  copies is reported held with the specific reason. `--transitive hide` restores direct-only
  planning.
- **`--allow-stale-lock` now skips the project's dependency evaluation** instead of evaluating the
  stale graph: a lock the tool cannot prove current yields a counted, warned skip rather than
  verdicts derived from entries the next resolve would rewrite.
- **Go import rewrites are lexical.** A cross-major bump rewrites `.go` import paths with a
  quote-aware scanner instead of plain text replacement: no more doubled `/v2` suffixes, imports in
  strings/comments handled by quote parity, gopkg.in's `pkg.v1` low-major paths rewritten
  correctly, a `+incompatible` module moving onto a `/vN` path rewritten from its base path (and a
  skipped move reported under that base identity), and symlinked trees and nested `go.mod` module
  directories skipped exactly as the `go` tool skips them.
- **pnpm lock repair and truthful holds.** A lock a resolve leaves floated (an importer copy
  outside its declared range) is repaired through the override engine before being declared stale;
  `catalog:`/`workspace:`/`npm:` specifiers are never rewritten by constraint widening, and a
  candidate every declaring importer manages through a `catalog:` entry is held with an explicit
  not-eligible reason instead of surfacing as a resolver conflict; peer-conflict
  blame reads the lockfileVersion 9 `snapshots:` section; a copy that lands beneath a newer
  same-name duplicate is reported as applied (pnpm) instead of vanishing, or rolled back as a
  phantom conflict (npm); and a `pnpm-lock.yaml` that is malformed or written by pnpm 8 or older
  (lockfileVersion < 9) is a clear error instead of an empty, healthy-looking dependency graph —
  including the post-apply diff and transitive-advance verdict reads, which fail the batch rather
  than reporting every candidate unreached.
- **Failures surface instead of hiding**: a candidate dropped during mutation recovery is a
  per-candidate error rather than a silent skip behind `0 errors`, registry fetches retry a lone
  transient transport failure once before failing the row, and resolver-quoted URL credentials are
  redacted even inside comma-glued URL lists — as well as in tool stderr quoted by failure
  diagnostics, semicolon-separated query tokens, and credentials containing unencoded reserved
  characters.
- **`fix` reports net rows.** A package moved across several fix rounds (a collateral float and
  its later downgrade) collapses into one net row, matching `upgrade`'s report contract.
- **`upgrade --strict` tolerates not-eligible holds.** A hold cooldown cannot act on (a
  catalog-managed dependency, a pin with no rewritable requirement) keeps the dependency on its
  already-matured version, so it no longer fails `upgrade --strict`; `fix --strict` still fails
  it, because there the same hold leaves a live policy violation in the graph.
- **`outdated` agrees with `upgrade` for rewritten identities**: a held cross-path Go move
  (`+incompatible` → `/vN`) reported under its base path now reclassifies the outdated item as
  blocked, preserving the documented blocked-equals-held contract.
- **Declared requirements and bounds join by package identity**, so a dependency shipping a
  custom `[lib] name` keeps its edge attribution and its member's deliberate upper bound (a
  widen can no longer rewrite a cap it failed to see), and a plain declaration beside a rename
  of the same package stays on its own node.
- **Preview copies stage local-source topology faithfully**: out-of-tree editable and archive
  sources, in-tree archive files, in-tree directory sources under pruned locations (`vendor/`,
  `testdata/`), and in-tree symlinks are reproduced in the throwaway resolve copy, so
  `--dry-run`/`outdated` previews no longer fail on local-source projects the real resolver
  handles.
- **Breaking library API:** `ToolRead::project_detection` replaces `project_marker` and
  `probe_manifest_without_lock`. Ordinary adapters should wrap their marker in
  `ProjectDetection::Primary`; adapters that must inspect manifest-only roots should use
  `PrimaryWithValidation` and implement the batched `validate_manifests_without_lock` hook.
- **Breaking library API:** adapter parsing and lookup helpers now return named records instead of
  positional tuples. This affects Go path-version splitting; npm lock parsing and install-tree
  resolution; and the public Hex, Maven, pip, and RubyGems resolved-pin parsers. Callers should use
  the documented fields on `PathVersionSplit`, `NameVersion`, `ResolvedInstance`, `ResolvedDep`,
  and `ResolvedPin`.
- **Breaking library API:** `ToolWrite::recover_pending_mutation` now returns a typed
  `MutationRecovery` that distinguishes an accepted publication, a restored preimage, artifact-only
  cleanup, and unchanged state while retaining non-fatal cleanup or durability diagnostics.
  `ToolWrite::ensure_no_pending_mutation` lets the same mutation-side adapter fail a shared read
  session without performing recovery. `ProjectMutationState` pairs a rollback journal with the
  post-apply state observed before any conditional restore.
- **Breaking library API:** `ToolWrite::apply` and `apply_with_observer` now accept a
  `PreparedMutation` instead of independently supplied project, plan, and journal values. Callers
  construct in-place operations through `PreparedMutation::prepare`; isolated adapters require
  `PreparedMutation::prepare_isolated` and reject in-place dispatch. Resilient retries derive
  authorized subsets through `PreparedMutation::subset` while sharing one immutable journal.
  `ToolWrite::mutation_tool` binds the capability to its tool family, and `AdapterSet` registration
  now fails when an adapter's read and write identifiers differ or when that tool family is already
  registered. `AdapterSet::register_read` consequently returns `Result`. `apply_with_observer`
  returns an adapter-boundary postimage. `ApplyAttempt` is now an opaque value constructed through
  `PreparedMutation`; callers inspect its validated `ApplyAttemptOutcome`, preventing a custom
  adapter from pairing another project or write set with a finished result. Adapter-owned pending
  recovery remains outside the application's rollback authority. `ToolWrite::normalize_lock_edges` also
  accepts the prepared capability, and Cargo requires one whose provenance matches the execution
  mode it selected for the platform. `ApplyReport`
  carries non-fatal committed warnings, final edge audits return an `EdgeNormalizationReport`, and
  `CoreError`/`DiagnosticKind` include `PendingRecovery` and `DurabilityUncertain` so a visible
  write whose directory sync failed is not misclassified as lock contention.
  `ProjectMutationFile` also records standard file permissions and rejects non-regular or
  multiply-linked paths so a rollback can restore the file contents and modes without following a
  symlink or mutating an external hard-link alias. Its fields and the
  journal's file list are now private; callers should use `ProjectMutationJournal::capture`, the
  recovery-only `ProjectMutationJournal::from_snapshot`, and the read-only accessors. Journals and
  observed states are bound to the canonical source-project identity, preventing malformed write
  sets and cross-project restoration. `ProjectMutationState::capture` now accepts a validated
  journal instead of an arbitrary file slice. Writable outputs whose ancestors are symbolic links
  are rejected because they cannot be governed by the selected project's mutation lease.
  `capture_state`, `restore_if_unchanged`, and `restore` now use that bound identity instead of
  accepting a project root at each call. Callers can therefore distinguish rollback conflicts from
  visible corrections whose directory durability is uncertain. A multi-file rollback conflict
  emits an indeterminate-state error and never counts previously verified rows as applied.
  `BaselineViolation` now carries a complete `PackageId`, so source-distinct packages cannot share
  transitive-policy authority.
- **Breaking library API:** `ToolWrite::mutation_execution` can provide an
  `IsolatedMutationStrategy` that stages and publishes its own faithful resolver trial. Cargo uses
  this boundary so resolver and edge-normalization trials touch only a disposable project copy;
  the source project receives the accepted manifests and lock through one adapter-owned recovery
  protocol after its complete input and output preimage is revalidated. Strategy preparation and
  mutation recovery now receive the captured `ProjectCoordination` identity. Git-backed
  coordination exposes a `RecoveryAuthority` on Unix tied to the captured common-directory
  identity; project-local non-Git coordination and platforms without verified owner-private
  authority deliberately cannot authorize restoration. Standalone
  `recover_interrupted_mutation` now acquires its own `ProjectWriteLease` instead of accepting a
  freely supplied coordination value, and `recovery_authority_projects` accepts an explicit
  `RecoveryScope` so callers cannot conflate repository-wide and targeted recovery. Recovery scopes
  are now created through the fallible `RecoveryScope::repository` and `RecoveryScope::explicit`
  constructors, which bind lexical spellings to one canonical project identity.
  `recovery_authority_projects` returns both attributed projects and warnings for malformed
  authority that could not be attributed safely. Shared `ProjectReadLease`, `ProjectWriteLease`,
  `RepositoryResourceReadLease`, and `RepositoryResourceWriteLease` types now own the coordinated
  lock protocols used by the application and standalone recovery.
  `ProjectCoordination::resolve_existing` resolves that identity without creating a coordination
  namespace, for validation and discovery paths that must remain side-effect-free.
- **Breaking library API:** `Workspace::explain` now returns `Result<ExplainOutcome>` so pending
  project mutation state and other access-session failures cannot be silently reduced to a missing
  registry.
- Add a cargo `--cargo-edge-policy` for resolved lock **edge bindings** (`preserve` |
  `canonicalize` | `none`; config `[tool.cargo] edge-policy` — cargo-specific, so the key is
  tool-scoped and rejected under any other selector). An incremental re-resolve can silently
  rebind an edge between two coexisting versions a wide declared range admits (diesel's
  `uuid = ">=0.7, <2.0"` beside a `0.8` and a `1.x` line) — a build-affecting change that is
  invisible per-version and passes `cargo metadata --locked`. `preserve` (the default) restores
  such churn to the pre-upgrade binding; `canonicalize` binds each unambiguous crates.io edge to
  the highest satisfying locked version — preferring candidates whose declared `rust-version` is
  workspace-compatible and falling back to the highest satisfying one when none is, using a
  cooldown-owned conservative tier inspired by cargo's `incompatible-rust-versions = "fallback"`
  rule — healing pre-existing bad bindings too, also on a run with no version change to apply;
  `none` only observes. Every corrected, withheld, unaddressable, or surviving rebind that can be
  paired across a stable dependent identity and endpoints coexisting in both snapshots is reported
  as its own
  `restored`/`canonicalized`/`held`/`unaddressable`/`rebound` row, preserving the dependent's full
  source-bearing lock identity. The `upgrade`/`fix` summary counts edge activity apart from
  version changes (`edgesCorrected`/`edgesRebound`/`edgesHeld`/`edgesUnaddressable`); row-level
  `applied` says whether the binding outcome is committed, and a `held` or `unaddressable` row
  fails `--strict`. Cargo resolver and correction trials run in an isolated project copy. Once the
  complete result passes Cargo and cooldown verification, one owner-only, whole-project recovery
  record guards checked publication of the accepted manifests and lock. Its exact digest is
  anchored beneath Git's owner-private common-directory coordination namespace on Unix before
  source publication, so project content alone cannot claim restoration authority. A Cargo source
  mutation that reaches publication without anchored authority — outside a Git worktree — fails
  closed until a trusted external authority is available. On platforms that cannot prove the
  coordination namespace is private to the current user (Unix ownership bits today, so Windows
  always), Cargo instead selects the in-place trial-and-rollback execution every other ecosystem
  uses, keeping its rollback guarantees but without a persistent recovery record. The
  pending-recovery preflight every mutation runs settles as unchanged when a project carries no
  recovery artifacts, so a project that could never have published one is not refused; artifacts
  that are present still require authority. Publication
  includes parent-directory durability on Unix and best-effort directory persistence elsewhere.
  Unknown or unreferenced recovery artifacts are reported and left untouched. The new Cargo-only
  `recover` command discovers public markers, orphaned private artifacts, and trusted authority
  records whose project marker was never published without loading policy, manifests, baselines,
  or registries. Project-visible artifacts use a bounded repository scan; an explicit
  `-C <project>` scans only that subtree and relevant ancestors, including targets inside pruned
  bulk directories without traversing unrelated repository siblings. An unreadable subtree or
  ancestor is skipped with a visible warning naming the path instead of failing the whole
  recovery run.
  Recovery setup failures honor `--json` with the schema-v4 envelope. The command then completes or
  restores a validated interrupted publication without continuing into another mutation. Project
  reads and native-policy sync share target-derived project leases under the Git common directory
  (or a project-local non-Git state directory), while repository-scoped native state has its own
  tool-qualified lease independent of project discovery. User-visible source identities redact
  credentials, non-provenance query values, and non-commit fragments. Config follows the
  per-project repository cascade, and the closed JSON contract is schema v4.

## v0.0.14

- **Interactive progress reports candidate work honestly.** A candidate counted as done whenever a
  resolver operation touched it, so a bisected batch showed work finished before it settled and the
  display could complete while decisions were still pending. Progress now tracks decided candidates
  as its own determinate measure and shows resolver operations and policy passes separately.
- **Cargo constraint widening no longer rewrites an already-widened requirement.** A requirement
  that already carries the normalized target constraint is reported unchanged instead of counting
  as an edit, so a repeated run stops rewriting the same manifest — and a member match that needed
  no change no longer falls through to the workspace-root fallback and widened a root declaration
  the member never inherits from.

## v0.0.13

- **npm-family upgrades respect the registry's `latest` dist-tag.** A stable release published
  above the maintainer's own `latest` pointer (a premature or abandoned major kept above a
  continuing line) is held with `dist-tag latest <version>` rather than proposed, since it is not
  what a default `npm install` resolves to. A project already pinned above the tag is unaffected,
  and `--no-respect-dist-tags` (config `respect-dist-tags = false`) is the deliberate opt-out —
  the right choice for a private registry or mirror whose tags diverge from the public one.
- **npm-family upgrades respect recorded peer contracts.** A target a still-present dependent's
  `peerDependencies` range excludes is reported `blocked`/`peer_held` naming the dependent instead
  of adoptable: pnpm's resolver only warns on that break and npm commits it under relaxed
  enforcement such as `legacy-peer-deps`. On pnpm both packages can move in one run — the joint
  move is the whole-graph resolve's decision, and a landing that provably breaks a contract between
  workspace-declared packages is rejected and rolled back. `peerDependencies` is never rewritten by
  constraint widening, `--rewrite` included: that range is a contract published to consumers, not a
  declaration of what the package installs.

## v0.0.12

- Add tool-qualified package selectors such as `[tool.uv.package.glob]` and package-only
  `max-major` ceilings.
- Respect explicit manifest `<`/`<=` upper bounds during major upgrades; `--rewrite` is the
  deliberate escape hatch for crossing and rewriting them.
- Report declared-bound and `max-major` holds in terminal and JSON output.
- Reject `exclude-folders` and `exclude-packages` under package, registry, and project selectors.
  Earlier versions accepted these misplaced keys but silently ignored them; exclusion lists belong
  under `[tool.*]`, `[global]`, or a command table.
