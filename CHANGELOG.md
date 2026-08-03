# Changelog

## Unreleased

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
  now fails when an adapter's read and write identifiers differ. `apply_with_observer` returns an
  adapter-boundary postimage, using the `ApplyAttempt` enum to keep adapter-owned pending recovery
  outside the application's rollback authority. `ToolWrite::normalize_lock_edges` also accepts the
  prepared capability, and Cargo requires one carrying provenance from an adapter-created isolated
  project. `ApplyReport`
  carries non-fatal committed warnings, final edge audits return an `EdgeNormalizationReport`, and
  `CoreError`/`DiagnosticKind` include `PendingRecovery`.
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
  visible corrections whose directory durability is uncertain.
- **Breaking library API:** `ToolWrite::mutation_execution` can provide an
  `IsolatedMutationStrategy` that stages and publishes its own faithful resolver trial. Cargo uses
  this boundary so resolver and edge-normalization trials touch only a disposable project copy;
  the source project receives the accepted manifests and lock through one adapter-owned recovery
  protocol after its complete input and output preimage is revalidated.
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
  record guards checked publication of the accepted manifests and lock. Publication includes
  parent-directory durability on Unix and best-effort directory persistence elsewhere.
  Unknown or unreferenced recovery artifacts are reported and left untouched. The new `recover`
  command discovers exact ignored or hidden targets without loading policy, manifests, baselines,
  or registries, then completes or restores a validated interrupted publication without continuing
  into another mutation. Project reads and
  native-policy sync share target-derived project leases under the Git common directory (or a
  project-local non-Git state directory), while repository-scoped native state has its own
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
