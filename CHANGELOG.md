# Changelog

## Unreleased

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
