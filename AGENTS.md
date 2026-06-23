# Rust coding guidelines

Repo-specific conventions for `cooldown`. These apply on top of the personal
agent guidelines (`~/CLAUDE.md` / `~/.codex/AGENTS.md`); where this file is
silent, those still hold.

## Task runner

The repo drives everything through [Task](https://taskfile.dev)
(`taskfile.yaml`). Run a `task …` command **exactly as written** — the flags and
feature-matrix behavior are deliberate; do not substitute a hand-rolled
`cargo …` equivalent.

Everyday loop:

| Task | Runs | Purpose |
|------|------|---------|
| `task format` | `cargo fmt --all` | format the workspace |
| `task check` | `cargo check --workspace --all-targets` | fast type-check |
| `task build` | `cargo build --all-targets` | debug build |
| `task test` | `cargo nextest run --workspace --all-targets` | run tests (nextest) |
| `task test:doc` | `cargo test --doc --workspace` | doctests (nextest does not run these) |
| `task lint` | `cargo clippy --all-targets --no-deps` + `sg scan` | clippy + ast-grep rules |
| `task fix` / `task lint:fix` | `cargo clippy --fix …` + `sg scan` | auto-apply clippy fixes |
| `task typos` | `typos` | spell-check |
| `task audit` / `task unused` / `task outdated` | `cargo audit` / `udeps` / `outdated` | dependency hygiene |

Format before committing; `task check` / `task test` are the tight inner loop.

### Feature combinations (`:fc`)

The `:fc` variants rerun the same command across **every valid combination of
crate features** via [`cargo fc`](https://crates.io/crates/cargo-feature-combinations),
rather than only the default set or `--all-features`. Feature-gated code can
compile, lint, and test cleanly with all features on yet break under some
subset (an import behind one feature, a path only reached under another) — `:fc`
is what surfaces that.

| Task | |
|------|---|
| `task check:fc` | check every feature combination |
| `task build:fc` | build every feature combination |
| `task test:fc` | test every feature combination |
| `task lint:fc` | clippy every feature combination, then `sg scan` |

`task lint:fc` is the lint gate, and `task test:fc` the test gate — "done" means
green there, not just on the non-`:fc` tasks. They run N builds so they are
slower; keep them for the end of a change rather than the inner loop.

## Testing

Tests split into two buckets: fast, hermetic **unit** tests and slow,
network-driven **integration** tests. The split is enforced by a nextest binary
filter (`.config/nextest.toml`), not by `#[ignore]`.

### Cargo aliases

| Alias | Runs | Scope |
|-------|------|-------|
| `cargo t` | `nextest run -P default` | unit tests only (offline, fast) |
| `cargo ti` | `nextest run -P integration` | the real-tool integration tests only |
| `cargo tci` | `nextest run -P ci` | **everything** — unit + the non-hermetic integration tests |

`cargo t` is the inner loop; it never hits the network. `cargo tci` runs the
full suite including the integration tests that drive the real package managers.

### Nextest profiles

Three profiles in `.config/nextest.toml`, distinguished by `default-filter`:

- `default` — **unit only**: excludes every integration binary/module.
- `integration` — **integration only**: exactly the binaries/modules `default`
  excludes (generous slow-timeout for the network round-trips).
- `ci` — **all**: `default-filter = 'all()'`.

### Task targets

| Task | Profile | Scope |
|------|---------|-------|
| `task test` (alias `test:unit`) | `default` | unit only |
| `task test:integration` | `integration` | integration only |
| `task test:all` (alias `test:ci`) | `ci` | everything |

### What makes a test an integration test

A test is treated as an integration test (and so kept out of `cargo t` /
`task test`) when **any** of these match:

- its binary is named `convergence_*`, or
- its binary is named `e2e*`, or
- its binary is named `integration_test` / `integration_tests`, or
- it lives in an `integration_test::` / `integration_tests::` module.

The filter both profiles use is
`binary(/^(convergence_|e2e|integration_tests?$)/) or test(/integration_tests?::/)`.
The three binary prefixes share one `binary()` predicate on purpose: nextest
errors if a `binary()` regex matches zero binaries, and only `convergence_*`
binaries exist today — one alternation regex keeps `e2e*` /
`integration_test(s)` future-proof without tripping that check. These tests drive the real package
managers (`uv`, `cargo`, `go`, `pnpm`, provisioned via `mise.toml`) against live
registries, so they need the toolchain plus network access. Each one calls
`skip_if_missing!` to skip cleanly when its tool is absent. To add a new
integration test, name its binary `convergence_*` (or one of the forms above) or
put it in an `integration_test(s)::` module — no `#[ignore]` needed; the filter
picks it up. Run it via `cargo ti` / `cargo tci` / `task test:integration`.

## Multiline strings

Use the [`indoc`](https://docs.rs/indoc) macros for every multiline string
literal so indentation follows the code, not the left margin:

- `indoc!` for a static multiline literal.
- `formatdoc!` for a multiline literal with `{}` interpolation (same arguments
  as `format!`).

Both strip the leading whitespace common to all lines, so the string can be
indented to match its surrounding block instead of being un-indented back to
column zero. Do **not** hand-write multiline strings as a raw literal jammed
against the margin, and do **not** glue lines together with `\n` and `+` /
`push_str`.

```rust
// no — breaks the surrounding indentation, hard to read at the margin
const STARTER_CONFIG: &str = r#"# cooldown.toml
min-age = "7d"
"#;

// yes — indented with the code, dedented at compile time
use indoc::indoc;

const STARTER_CONFIG: &str = indoc! {r#"
    # cooldown.toml
    min-age = "7d"
"#};
```

```rust
use indoc::formatdoc;

let report = formatdoc! {"
    scanned {count} packages
    excluded {skipped}
", count = count, skipped = skipped};
```

A single-line string stays a plain `"…"` literal — reserve the macros for the
multiline case.

## Errors

- Library crates (everything except the `cooldown` binary) return the
  `thiserror`-derived `CoreError`. Never introduce `anyhow`.
- No `unwrap`, `expect`, or `panic!` in non-test code. Propagate with `?` and a
  typed error; reserve panics for tests and genuinely-unreachable invariants
  (and then prefer documenting the invariant).
- `color-eyre` is for the CLI binary only — it formats errors for the human at
  the terminal and must not leak into the library layers.

## Lints

- Fix clippy findings for real rather than silencing them. `#[allow(...)]` is a
  last resort and must carry a `reason = "…"`.
- `task lint:fc` (see [Task runner](#task-runner)) is the gate — clippy across
  every feature combination plus the ast-grep rules in `rules/`. Run the task as
  written; do not substitute a hand-rolled `cargo clippy`.
- `unwrap`/`expect`/`panic!`/indexing are denied in production code and allowed
  only in tests (`clippy.toml`), reinforcing the error rule above.

## Comments

Comments explain *why*, not *what*. No decorative section-banner comments
(`// ==== … ====`, `// ---- … ----`): if a file needs them to navigate, split
it into well-named modules instead. Never remove an existing accurate comment —
migrations, intricate queries, and domain logic carry intent that the code
alone does not.
