# Changelog

## Unreleased

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
  accepts the prepared capability, and Cargo requires one carrying provenance from an
  adapter-created isolated project. `ApplyReport`
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
  source publication, so project content alone cannot claim restoration authority. Cargo source
  mutations outside a Git worktree, or on platforms without verified owner-private authority, fail
  closed until a trusted external authority is available. Publication includes parent-directory
  durability on Unix and best-effort directory persistence elsewhere.
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

## v0.0.12

- Add tool-qualified package selectors such as `[tool.uv.package.glob]` and package-only
  `max-major` ceilings.
- Respect explicit manifest `<`/`<=` upper bounds during major upgrades; `--rewrite` is the
  deliberate escape hatch for crossing and rewriting them.
- Report declared-bound and `max-major` holds in terminal and JSON output.
- Reject `exclude-folders` and `exclude-packages` under package, registry, and project selectors.
  Earlier versions accepted these misplaced keys but silently ignored them; exclusion lists belong
  under `[tool.*]`, `[global]`, or a command table.
