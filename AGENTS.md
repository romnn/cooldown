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
