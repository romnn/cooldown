---
title: Security model
weight: 8
---

# Security model

`cooldown` is a supply-chain control, so it is worth being precise about what it defends, what it doesn't, and the properties that make the defense hold.

## Threat model

The target is the **smash-and-grab window**: a malicious version is published to a registry and is detected and yanked within hours to a few days. A cooldown delays *adoption* until that window has passed, so the community's tooling and reports run before the code reaches your builds.

It is **not a malware scanner** — it never inspects code — and it does not replace the tools that do a different job. It pairs with `govulncheck`, `cargo audit`, and advisory feeds, which catch known vulnerabilities and published advisories rather than not-yet-known malicious releases.

## The risk surface is the resolved graph

What you declare is not what you ship — the resolved lockfile is. A single `^1.4` pulls in transitive dependencies, and a re-lock can silently advance any of them to a brand-new version. So [`check`]({{< relref "commands/check.md" >}}) evaluates **direct and transitive** dependencies by default, and a [`floor`]({{< relref "configuration/precedence.md" >}}) applies to transitives too.

For a too-fresh transitive you genuinely can't act on — one the graph pins, or one you'd rather not block CI on — the gate can be relaxed, but only deliberately:

- `check --transitive allow` — keep it visible but non-fatal.
- `check --transitive hide` — a direct-only gate.

The strict, whole-graph default stays **opt-out, never opt-in**.

## Mutations honor the selected graph gate

For Cargo, `upgrade` and `fix` evaluate resolver changes in an isolated project. If a re-lock leaves
a new too-fresh, non-acknowledged dependency in the graph, the trial rejects that change before
anything becomes visible in the source. The complete accepted manifest-and-lock state is published
under one project-visible recovery record whose exact digest is anchored in cooldown's
owner-private coordination namespace beneath Git's common directory on Unix. Recovery revalidates
the complete expected project state and the exact marker and authority identities immediately
before consuming that evidence. A Cargo source mutation that reaches this path without anchored
authority — outside a Git worktree — fails closed, because untrusted bytes cannot authorize
restoration.

Other ecosystems currently run resolver trials in place under the project lease. Cooldown records the files each trial may change, rolls a rejected trial back only when those files still match the observed resolver output, and stops if independent drift makes restoration unsafe. These ecosystems do not yet use Cargo's persistent whole-project recovery record.

Cargo selects that same in-place path on platforms where cooldown cannot prove the coordination
namespace is private to the current user — Unix ownership bits today, so Windows always. Those runs
keep the rollback guarantees of the paragraph above but have no persistent recovery record, so an
interrupted mutation is repaired by hand rather than by `cooldown recover`.

Every mutation that reports success has passed the package manager's verification and the selected cooldown transitive policy. The default whole-graph policy leaves the graph gate-clean by construction. `--transitive allow` deliberately retains visible fresh transitives, while `--transitive hide` excludes transitives from the gate.

Cooldown coordinates access through target-derived lock files. Git repositories use Git's common directory. A non-Git project uses `.cooldown/locks`, including for read commands, so add `.cooldown/` to that project's ignore rules if it is not already ignored. These non-Git lock files coordinate cooperating cooldown processes but do not authorize recovery.

## Cache hardening

Publish times are cached, and the cache is treated as adversarial input:

- A cached publish time may **never move earlier** on refresh — it is a monotonic floor.
- A **backdated** upstream timestamp (a release claiming to be older than what was already recorded) is rejected, not trusted.

This closes the obvious bypass — an attacker backdating a release so it appears to have already cleared the window.

Two flags control how the cache is used at the boundary: `--offline` turns every cache miss into `unknown-age` (never a false "ok"), and `--fresh` ignores the cache entirely and re-fetches — the right choice for a CI gate that must not pass on stale data.

## Escape hatches are explicit and audited

Loosening the policy is always deliberate and always visible:

- Exemptions (`--latest`, `--allow`, config `allow`) are audited — every one shows up in [`cooldown explain`]({{< relref "commands/other.md" >}}).
- A [`floor`]({{< relref "configuration/precedence.md" >}}) bounds config-level loosening: it is max-clamped across layers, so a more specific block can't quietly weaken it, and an `allow` can only bypass it when co-declared with it (or via an audited CLI flag).

There is no silent path to "adopt anything" — every one leaves a trail.

## Reporting a vulnerability

For a security issue in `cooldown` itself, please use GitHub's private vulnerability reporting on the [repository](https://github.com/romnn/cooldown) rather than a public issue.
