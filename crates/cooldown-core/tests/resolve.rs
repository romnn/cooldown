//! The precedence-matrix suite: layer × selector × field, including the floor max-clamp and the
//! allow-vs-floor security rule. These pin authority-first resolution by example.

mod common;
use common::*;
use cooldown_core::*;

use camino::{Utf8Path, Utf8PathBuf};
use jiff::SignedDuration;

fn days(n: i64) -> SignedDuration {
    SignedDuration::from_hours(24 * n)
}

fn repo(toml: &str) -> PolicyLayer {
    layer(toml, Origin::Repo(Utf8PathBuf::from("cooldown.toml")))
}
fn global(toml: &str) -> PolicyLayer {
    layer(toml, Origin::Global)
}
fn native(toml: &str) -> PolicyLayer {
    layer(toml, Origin::Native)
}
fn cli(toml: &str) -> PolicyLayer {
    layer(toml, Origin::Cli)
}

fn q<'a>(pkg: &'a str, project: &'a Utf8Path, kind: ResolveKind) -> ResolveQuery<'a> {
    ResolveQuery {
        ecosystem: GO,
        package: pkg,
        registry: None,
        project,
        kind,
    }
}

fn win(layers: &[PolicyLayer], pkg: &str, kind: ResolveKind) -> ResolvedWindow {
    let proj = Utf8PathBuf::from(".");
    resolve(layers, &q(pkg, &proj, kind), now()).window
}

/// The plan's worked example: a global, *specific* `[package."left-pad"] = 30d` LOSES to a repo,
/// *general* top-level `min-age = 14d`. Layer dominates selector.
#[test]
fn authority_first_layer_dominates_selector() {
    let layers = vec![
        config::builtin_default_layer(),
        global("[package.\"left-pad\"]\nmin-age = \"30d\""),
        repo("min-age = \"14d\""),
    ];
    let w = win(&layers, "left-pad", ResolveKind::CurrentPin);
    assert_eq!(w.spec, WindowSpec::MinAge(days(14)));
    assert_eq!(
        w.decided_by,
        Origin::Repo(Utf8PathBuf::from("cooldown.toml"))
    );
}

/// Within one layer, the most specific selector wins.
#[test]
fn within_layer_specificity_breaks_tie() {
    let layers = vec![
        config::builtin_default_layer(),
        repo("min-age = \"14d\"\n[package.\"left-pad\"]\nmin-age = \"30d\""),
    ];
    assert_eq!(
        win(&layers, "left-pad", ResolveKind::CurrentPin).spec,
        WindowSpec::MinAge(days(30))
    );
    assert_eq!(
        win(&layers, "other", ResolveKind::CurrentPin).spec,
        WindowSpec::MinAge(days(14))
    );
}

/// `repo > native > global > default` for `min-age`.
#[test]
fn layer_authority_order() {
    let base = vec![config::builtin_default_layer(), global("min-age = \"10d\"")];
    assert_eq!(
        win(&base, "x", ResolveKind::CurrentPin).spec,
        WindowSpec::MinAge(days(10))
    );

    let mut with_native = base.clone();
    with_native.push(native("min-age = \"20d\""));
    assert_eq!(
        win(&with_native, "x", ResolveKind::CurrentPin).spec,
        WindowSpec::MinAge(days(20))
    );

    let mut with_repo = with_native.clone();
    with_repo.push(repo("min-age = \"14d\""));
    assert_eq!(
        win(&with_repo, "x", ResolveKind::CurrentPin).spec,
        WindowSpec::MinAge(days(14))
    );
}

/// A floor max-clamps the window up; a repo `min-age = 0d` is clamped by a global `floor`.
#[test]
fn floor_max_clamps_window() {
    let layers = vec![
        config::builtin_default_layer(),
        global("floor = \"7d\""),
        repo("[package.\"some-tool\"]\nmin-age = \"0d\""),
    ];
    let w = win(&layers, "some-tool", ResolveKind::CurrentPin);
    assert_eq!(w.spec, WindowSpec::MinAge(days(0)), "selected window is 0d");
    assert!(
        (w.effective_min_age_days(now()) - 7.0).abs() < 1e-9,
        "but clamped up to 7d"
    );
    assert_eq!(
        w.clamped_by(now()).map(cooldown_core::Origin::token),
        Some("global".to_string())
    );
}

/// An `allow` from any layer exempts against an ordinary window (no floor present).
#[test]
fn allow_union_exempts_against_window() {
    let layers = vec![
        config::builtin_default_layer(),
        global("allow = [\"left-pad\"]"),
        repo("min-age = \"14d\""),
    ];
    let w = win(&layers, "left-pad", ResolveKind::CurrentPin);
    assert!(w.exempt);
    assert!(
        w.effective_min_age_days(now()).abs() < 1e-9,
        "an exempt window resolves to a 0-day cooldown"
    );
}

/// The security rule: a repo `allow` cannot undercut an org (global) floor; a co-declared global
/// `allow` can; an explicit CLI `allow` always can.
#[test]
fn allow_vs_floor_security_rule() {
    let layers = vec![
        config::builtin_default_layer(),
        global("floor = \"7d\"\nallow = [\"github.com/acme/*\"]"),
        repo("allow = [\"some-tool\"]"),
    ];

    // Repo allow for some-tool is a different layer than the global floor → cannot bypass.
    let st = win(&layers, "some-tool", ResolveKind::CurrentPin);
    assert!(!st.exempt, "repo allow must not bypass the org floor");
    assert!((st.effective_min_age_days(now()) - 7.0).abs() < 1e-9);

    // Global allow for @acme/* is co-declared with the floor → bypasses.
    let acme = win(&layers, "github.com/acme/widget", ResolveKind::CurrentPin);
    assert!(
        acme.exempt,
        "co-declared global allow bypasses its own floor"
    );

    // CLI allow always bypasses (audited human override).
    let with_cli = vec![
        config::builtin_default_layer(),
        global("floor = \"7d\""),
        cli("allow = [\"some-tool\"]"),
    ];
    let st2 = win(&with_cli, "some-tool", ResolveKind::CurrentPin);
    assert!(st2.exempt, "CLI allow must always bypass a floor");
}

/// Per-kind fallthrough: a per-kind window wins for its kind; absent one, the bare `min-age`
/// applies. `CurrentPin` always uses the bare `min-age`.
#[test]
fn per_kind_fallthrough() {
    let layers = vec![
        config::builtin_default_layer(),
        global("[package.\"ex\"]\nmin-age = { major = \"30d\" }"),
        repo("min-age = \"14d\""),
    ];
    // Major: only the global per-kind sets `major` → 30d (per-kind wins for its kind).
    assert_eq!(
        win(&layers, "ex", ResolveKind::Candidate(UpdateKind::Major)).spec,
        WindowSpec::MinAge(days(30))
    );
    // Minor: nobody set `minor` → fall through to the bare `min-age` → repo's 14d.
    assert_eq!(
        win(&layers, "ex", ResolveKind::Candidate(UpdateKind::Minor)).spec,
        WindowSpec::MinAge(days(14))
    );
    // The pin always uses the bare `min-age` → 14d.
    assert_eq!(
        win(&layers, "ex", ResolveKind::CurrentPin).spec,
        WindowSpec::MinAge(days(14))
    );
}

/// The `minAgeSource` string is `<origin>:<selector>`.
#[test]
fn min_age_source_string() {
    let layers = vec![
        config::builtin_default_layer(),
        repo("[package.\"left-pad\"]\nmin-age = \"30d\""),
    ];
    let w = win(&layers, "left-pad", ResolveKind::CurrentPin);
    assert_eq!(w.source(), "repo:cooldown.toml:package=left-pad");
}

/// A `lang` selector applies only to its ecosystem.
#[test]
fn lang_selector_scopes_by_ecosystem() {
    let layers = vec![
        config::builtin_default_layer(),
        repo("[lang.go]\nmin-age = \"21d\"\n[lang.python]\nmin-age = \"30d\""),
    ];
    // GO query picks the go lang rule (21d), not python's.
    let w = win(&layers, "x", ResolveKind::CurrentPin);
    assert_eq!(w.spec, WindowSpec::MinAge(days(21)));
}

/// Regression (critical): a repo `allow` co-declared with a repo `floor` must NOT escape a
/// separate, lower-layer org/global floor. The global floor remains as a residual clamp.
#[test]
fn codeclared_allow_cannot_escape_a_separate_global_floor() {
    let layers = vec![
        config::builtin_default_layer(),
        global("floor = \"10d\""),
        repo("floor = \"30d\"\nallow = [\"evil-pkg\"]"),
    ];
    let w = win(&layers, "evil-pkg", ResolveKind::CurrentPin);
    // The repo allow bypasses the repo 30d floor (same layer) but NOT the global 10d floor.
    assert!(
        !w.exempt,
        "must not be fully exempt while an org floor remains"
    );
    assert!(
        (w.effective_min_age_days(now()) - 10.0).abs() < 1e-9,
        "residual global floor of 10d must still clamp, got {}",
        w.effective_min_age_days(now())
    );
    assert_eq!(
        w.clamped_by(now()).map(cooldown_core::Origin::token),
        Some("global".to_string())
    );
}
