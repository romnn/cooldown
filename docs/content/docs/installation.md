---
title: Installation
weight: 2
---

# Installation

`cooldown` is a single self-contained binary. Once it is on your `PATH`, it works from any repository — it detects the package managers in the directory itself.

## Homebrew

A prebuilt binary is available through the author's tap, which avoids compiling from source:

```bash
brew install --cask romnn/tap/cooldown
```

## From crates.io

```bash
cargo install --locked cooldown
```

`--locked` builds against the versions in the published `Cargo.lock`, which is the most reproducible option.

## With mise

If you manage tools with [mise](https://mise.jdx.dev), pin `cooldown` alongside the rest of your toolchain:

```bash
mise use github:romnn/cooldown
```

## Verify the installation

```bash
cooldown --version
# or
cooldown --help
```

If `cooldown` is not found, confirm that the install directory (`~/.cargo/bin` for `cargo install`) is on your `PATH`.

## Requirements

`cooldown` computes the cooldown verdict itself, but it drives the **native package manager** for each ecosystem to resolve and apply changes. To act on a given ecosystem, that tool has to be installed and on your `PATH`:

- **Rust** — `cargo`
- **Go** — `go`
- **Python** — `uv`, `pip`, `poetry` (and `conda` / `pixi` for those lockfiles)
- **JavaScript / TypeScript** — `npm`, `pnpm`, `yarn`, `bun`, `deno`
- **Ruby** — `bundler`; **Elixir** — `mix`; **Java** — `maven` / `gradle`; **Swift** — `swift`

A read-only command that only reads an existing lockfile (for example `cooldown check`) needs less than one that re-resolves or applies (`upgrade`, `fix`, or any run with `--lock` / `--sync`). Ecosystems you don't use need nothing installed — detection simply skips them. Network access to each registry is required unless you run `--offline` against a warm cache.

## Use in CI

In continuous integration, install the binary once and run the gate. `cargo install` works everywhere but compiles from source; [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall) fetches the prebuilt release instead:

```yaml
- uses: cargo-bins/cargo-binstall@main
- run: cargo binstall -y cooldown
- run: cooldown check
```

See [Continuous integration]({{< relref "ci/_index.md" >}}) for complete workflows.
