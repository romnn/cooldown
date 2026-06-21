# TODO

## `fix --major` does not cross major boundaries for Go

`fix` downgrades too-fresh dependencies; `fix --major` is meant to do so even
across a major boundary, repo-wide. This works for the registry tools
(cargo / npm / uv) where one package name spans majors — the manifest is
rewritten to the literal lower version. For **Go it is silently a no-op across
majors**, for two compounding reasons:

1. **Discovery is upward-only.** `crates/cooldown-go/src/tool/releases.rs:227`
   probes only *higher* majors (`next_major = current_major + 1`, walking up to
   `+8`). A module's lower-major `/vN` paths never enter the candidate set, so
   the fix evaluator — which keeps only older releases
   (`r.order < current.order`, `crates/cooldown-core/src/evaluate.rs:409`) —
   never sees a cross-major downgrade target.
2. **The import path would not be rewritten.** Even if such a target appeared,
   `plan_fix_changes` builds the `Change` with `package: dep.package.clone()`
   (`crates/cooldown/src/app/upgrade/executor.rs:413`), not `target_package()`
   (used only by the upgrade path, `executor.rs:252`). A Go major downgrade
   `foo/v3 → foo/v2` changes the import path, so the change would carry the
   wrong module path.

Both are pre-existing and latent — not a regression. To close the gap:

- Add **downward** Go major-path discovery (probe `current_major - 1` down to
  v1 / no-suffix), scoped to the fix/`AllowCrossMajor` path.
- Use `target_package()` in `plan_fix_changes` so a cross-major Go downgrade
  rewrites the `/vN` import-path suffix.

Until then, `fix --major` is correct for cargo / npm / uv and a no-op across
majors for Go.
