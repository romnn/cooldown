# Changelog

## Unreleased

- **A new duplicate copy can be refused, and its requirer is named.** After `solid-js` was
  correctly rolled back, upgrading `vite-plugin-solid` pulled a *second* `solid-js` into the graph
  and the run committed the lock with only a warning: `@solidjs/start`, used by four production
  apps, bound 1.9.15 while the apps bound 1.9.14 — two Solid runtimes with separate owner/observer
  registries, breaking reactivity with `tsc` and the test suite green. The detection was right and
  the severity wrong for such packages. `[tool.pnpm] single-copy = ["solid-js", "react"]` now
  names the packages that must stay single-copy: a settlement that adds a second copy of one is
  refused, the lock restored, and the candidate whose landing added it held with the copy named,
  exactly as a partial landing is — even when the second copy is the one an exclusion asked for.
  `--fail-on-new-duplicate` (config `fail-on-new-duplicate`) gates every package for a run without
  naming any. The warning for an ungated copy now names the requirer from the settled lock's own
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
  excluded importer left on the old copy, now carry their own `duplicate_copy` kind. The
  diagnostic-kind set is part of the JSON contract, so `schemaVersion` is now `6`.

- **`check`'s stale-lock error names the project.** `upgrade` reported `Cargo.lock is stale in
  /repo/incubator; run …` while `check` said only `Cargo.lock is stale; run …` for the same
  condition — in a repository with five Cargo locks (a parked nested workspace, standalone fuzz
  and wasm projects) the one to refresh had to be inferred from the dependency count dropping
  between runs. The lock-currency probe's detail now names the project root for cargo, uv, and
  the npm family (`pnpm-lock.yaml is stale in /repo/apps: <pnpm's own reason>`), in the error, in
  the `--allow-stale-lock` warning, and in `outdated`.

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
