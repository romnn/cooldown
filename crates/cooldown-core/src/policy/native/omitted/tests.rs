use super::{NativeEncoding, OmissionReason, OmittedField, omitted_native_fields};
use crate::config::{builtin_default_layer, parse_config};
use crate::model::ToolId;
use crate::{Origin, PolicyContext, PolicyLayer};
use camino::Utf8Path;
use indoc::{formatdoc, indoc};
use jiff::{SignedDuration, Timestamp};

const PNPM: ToolId = ToolId("pnpm");
const UV: ToolId = ToolId("uv");
const CARGO: ToolId = ToolId("cargo");

fn now() -> Timestamp {
    "2026-01-15T00:00:00Z".parse().expect("timestamp")
}

fn layer(config: &str, path: &str) -> PolicyLayer {
    parse_config(config, Origin::Repo(path.into())).expect("config")
}

/// The built-in default below the repository's `cooldown.toml`.
fn repo_layers(config: &str) -> Vec<PolicyLayer> {
    vec![builtin_default_layer(), layer(config, "cooldown.toml")]
}

/// Each omitted field as `<source> <field>`, the way a reader finds it in `explain`.
fn keys(omitted: &[OmittedField]) -> Vec<String> {
    omitted
        .iter()
        .map(|field| format!("{} {}", field.source(), field.field.name()))
        .collect()
}

/// The registry each tool's writer declares every package it evaluates to carry.
fn registry_of(target: ToolId) -> Option<&'static str> {
    match target.as_str() {
        "pnpm" => Some("npm"),
        "uv" => Some("pypi"),
        "cargo" => Some("crates.io"),
        _ => None,
    }
}

/// The fields a project-scoped tool's native config leaves out for the project at `project`.
fn project_scope(
    layers: &[PolicyLayer],
    project: &str,
    target: ToolId,
    package_windows: bool,
) -> Vec<OmittedField> {
    let project = Utf8Path::new(project);
    let encoding = NativeEncoding {
        default_layers: layers,
        default_project: project,
        registry: registry_of(target),
        package_windows,
        writer_exemptions: true,
    };
    omitted_native_fields(
        &[PolicyContext { layers, project }],
        target,
        &encoding,
        now(),
    )
}

/// The consumer of a repo-level file at `project` with `layers`.
fn consumer<'a>(project: &'a str, layers: &'a [PolicyLayer]) -> PolicyContext<'a> {
    PolicyContext {
        layers,
        project: Utf8Path::new(project),
    }
}

/// The fields a repo-level uv file resolved from `repo` leaves out for `consumers`.
fn repo_scope(repo: &[PolicyLayer], consumers: &[PolicyContext<'_>]) -> Vec<OmittedField> {
    let encoding = NativeEncoding {
        default_layers: repo,
        default_project: Utf8Path::new("."),
        registry: registry_of(UV),
        package_windows: true,
        writer_exemptions: false,
    };
    omitted_native_fields(consumers, UV, &encoding, now())
}

/// The default and floor the written default folds in are encoded: the bare ones, the tool's,
/// those of a project rule matching the project, and those of a catch-all package rule, plain or
/// qualified for the tool.
#[test]
fn the_written_default_encodes_its_own_fields() {
    let layers = repo_layers(indoc! {r#"
        min-age = "7d"
        floor = "1d"

        [tool.pnpm]
        min-age = "14d"
        floor = "2d"

        [project."apps/*"]
        min-age = "21d"
        floor = "3d"

        [package."*"]
        min-age = "30d"
        floor = "4d"

        [tool.pnpm.package."*"]
        min-age = "31d"
    "#});

    assert_eq!(
        keys(&project_scope(&layers, "apps/web", PNPM, false)),
        Vec::<String>::new()
    );
}

/// Per-kind windows have no native place on any selector, even one whose default is encoded.
#[test]
fn per_kind_fields_are_never_encoded() {
    let layers = repo_layers(indoc! {r#"
        [min-age]
        default = "7d"
        major = "30d"
        minor = "14d"

        [tool.pnpm.min-age]
        patch = "3d"
    "#});

    assert_eq!(
        keys(&project_scope(&layers, ".", PNPM, false)),
        [
            "repo:cooldown.toml major",
            "repo:cooldown.toml minor",
            "repo:cooldown.toml:tool=pnpm patch",
        ]
    );
}

/// A per-kind window looser than the written default is reported too, since the classifier never
/// compares windows.
#[test]
fn a_looser_per_kind_window_is_still_reported() {
    let layers = repo_layers(indoc! {r#"
        [min-age]
        default = "7d"
        patch = "1d"
    "#});

    let omitted = project_scope(&layers, ".", PNPM, false);

    assert_eq!(keys(&omitted), ["repo:cooldown.toml patch"]);
    assert_eq!(
        omitted.first().map(|field| field.notice(PNPM)).as_deref(),
        Some(
            "`patch = \"1d\"` from `repo:cooldown.toml` is not represented in pnpm's native \
             config, so a bare pnpm command does not apply it"
        )
    );
}

/// A glob package rule other than `*` gates only some packages, so its default and floor are
/// reported, even for a glob that also matches the empty name the written default resolves.
#[test]
fn glob_package_fields_are_not_encoded() {
    let layers = repo_layers(indoc! {r#"
        [package."internal-*"]
        min-age = "30d"
        floor = "10d"

        [package."{**/,internal-*}"]
        min-age = "60d"
    "#});

    assert_eq!(
        keys(&project_scope(&layers, ".", PNPM, false)),
        [
            "repo:cooldown.toml:package=internal-* default",
            "repo:cooldown.toml:package=internal-* floor",
            "repo:cooldown.toml:package={**/,internal-*} default",
        ]
    );
}

/// uv's package table carries a literal package's default and floor, but not its per-kind
/// windows, while pnpm has no package table at all.
#[test]
fn only_a_package_table_encodes_literal_package_windows() {
    let layers = repo_layers(indoc! {r#"
        [package."numpy"]
        min-age = { default = "30d", major = "60d" }
        floor = "10d"
    "#});

    assert_eq!(
        keys(&project_scope(&layers, ".", UV, true)),
        ["repo:cooldown.toml:package=numpy major"]
    );
    assert_eq!(
        keys(&project_scope(&layers, ".", PNPM, false)),
        [
            "repo:cooldown.toml:package=numpy default",
            "repo:cooldown.toml:package=numpy major",
            "repo:cooldown.toml:package=numpy floor",
        ]
    );
}

/// A rule that cannot gate the tool's packages in the project is skipped: one qualified for
/// another tool or another project, and a rule for a registry other than the one every pnpm
/// package carries.
/// `latest` and `allow` are left to the native exemption lists.
#[test]
fn rules_that_cannot_gate_the_project_are_skipped() {
    let layers = repo_layers(indoc! {r#"
        allow = ["left-pad"]

        [tool.cargo]
        min-age = { default = "7d", major = "30d" }

        [tool.cargo.package."serde"]
        min-age = "30d"

        [registry."crates.io"]
        min-age = { default = "7d", major = "30d" }

        [project."libs/*"]
        min-age = { default = "7d", major = "30d" }

        [package."@corp/*"]
        latest = true
    "#});

    assert_eq!(
        keys(&project_scope(&layers, "apps/web", PNPM, false)),
        Vec::<String>::new()
    );
}

/// A repo-level file is written from the root's default, so a project rule that matches a
/// consumer but not `.` is reported, while one matching `.` is encoded.
#[test]
fn repo_scope_reports_project_rules_that_miss_the_root() {
    let repo = repo_layers(indoc! {r#"
        [project."apps/*"]
        min-age = "30d"

        [project."*"]
        min-age = "14d"
    "#});

    assert_eq!(
        keys(&repo_scope(&repo, &[consumer("apps/web", &repo)])),
        ["repo:cooldown.toml:project=apps/* default"]
    );
}

/// A layer only a consumer has, such as a nested `cooldown.toml`, never reached the repo-level
/// default, so every gating field in it is reported, the bare default included.
/// Two consumers sharing the repository's rules report each of them once.
#[test]
fn repo_scope_reports_every_field_of_a_consumer_only_layer() {
    let repo = repo_layers(indoc! {r#"
        [min-age]
        default = "7d"
        major = "14d"
    "#});
    let mut nested = repo.clone();
    nested.push(layer(
        indoc! {r#"
            [min-age]
            default = "10d"
            major = "30d"
        "#},
        "apps/web/cooldown.toml",
    ));

    assert_eq!(
        keys(&repo_scope(
            &repo,
            &[consumer(".", &repo), consumer("apps/web", &nested)]
        )),
        [
            "repo:cooldown.toml major",
            "repo:apps/web/cooldown.toml default",
            "repo:apps/web/cooldown.toml major",
        ]
    );
}

/// A native layer is the tool's own config read back, which the tool applies by itself, so a
/// consumer's nearer native window and per-package entries are never reported as missing from
/// the tool's native config.
#[test]
fn native_layers_are_never_reported() {
    let repo = repo_layers(r#"min-age = "7d""#);
    let mut nested = repo.clone();
    let native = parse_config(
        indoc! {r#"
            min-age = "1d"

            [package."types-*"]
            min-age = "3d"
        "#},
        Origin::Native,
    )
    .expect("config");
    nested.insert(1, native);

    assert_eq!(
        keys(&repo_scope(&repo, &[consumer("apps/api", &nested)])),
        Vec::<String>::new()
    );
}

/// A floor that binds today replaces the freeze in the written default with a rolling age, which
/// stops matching cooldown once the floor reaches past the freeze date.
#[test]
fn a_freeze_under_a_binding_floor_is_reported_with_its_end() {
    // On 2026-01-15 a 30-day floor cuts off at 2025-12-16, before the freeze.
    let layers = repo_layers(indoc! {r#"
        freeze = "2026-01-10T00:00:00Z"
        floor = "30d"
    "#});

    let omitted = project_scope(&layers, ".", PNPM, false);

    assert_eq!(keys(&omitted), ["repo:cooldown.toml default"]);
    assert_eq!(
        omitted.first().and_then(|field| field.reason),
        Some(OmissionReason::FreezeUnderFloor {
            floor: SignedDuration::from_hours(24 * 30),
            until: "2026-02-09T00:00:00Z".parse().expect("timestamp"),
        })
    );
    assert_eq!(
        omitted.first().map(|field| field.notice(PNPM)).as_deref(),
        Some(
            "`freeze = \"2026-01-10T00:00:00Z\"` from `repo:cooldown.toml` is written to pnpm's \
             native config as the 30d floor that binds today, so from 2026-02-09T00:00:00Z a bare \
             pnpm command admits releases published after the freeze"
        )
    );
}

/// A floor that does not bind today never will, since the freeze cutoff stays while the floor's
/// moves later, so the written freeze keeps matching.
#[test]
fn a_freeze_under_a_non_binding_floor_is_not_reported() {
    // On 2026-01-15 a 3-day floor cuts off at 2026-01-12, after the freeze.
    let layers = repo_layers(indoc! {r#"
        freeze = "2026-01-10T00:00:00Z"
        floor = "3d"
    "#});

    assert_eq!(
        keys(&project_scope(&layers, ".", PNPM, false)),
        Vec::<String>::new()
    );
}

/// A written package window is checked for a freeze its floor replaces, just as the default is.
#[test]
fn a_package_window_freeze_under_a_binding_floor_is_reported() {
    let layers = repo_layers(indoc! {r#"
        [package."numpy"]
        freeze = "2026-01-10T00:00:00Z"
        floor = "30d"
    "#});

    let omitted = project_scope(&layers, ".", UV, true);

    assert_eq!(keys(&omitted), ["repo:cooldown.toml:package=numpy default"]);
    std::assert_matches!(
        omitted.first().and_then(|field| field.reason),
        Some(OmissionReason::FreezeUnderFloor { .. })
    );
}

/// A freeze already reported for its selector keeps one entry, which gains the floor's end.
#[test]
fn a_freeze_reason_joins_an_existing_entry() {
    let layers = repo_layers(indoc! {r#"
        floor = "30d"

        [package."{**/,internal-*}"]
        freeze = "2026-01-10T00:00:00Z"
    "#});

    let omitted = project_scope(&layers, ".", PNPM, false);

    assert_eq!(
        keys(&omitted),
        ["repo:cooldown.toml:package={**/,internal-*} default"]
    );
    std::assert_matches!(
        omitted.first().and_then(|field| field.reason),
        Some(OmissionReason::FreezeUnderFloor { .. })
    );
}

/// One freeze written as the default under one floor and as a package window under a longer one
/// drifts first in the default, so its single entry names the default's earlier end.
#[test]
fn a_freeze_drifting_in_several_windows_names_the_earliest_end() {
    let layers = repo_layers(indoc! {r#"
        freeze = "2026-01-10T00:00:00Z"
        floor = "30d"

        [package."numpy"]
        floor = "60d"
    "#});

    let omitted = project_scope(&layers, ".", UV, true);

    assert_eq!(keys(&omitted), ["repo:cooldown.toml default"]);
    let freeze: Timestamp = "2026-01-10T00:00:00Z".parse().expect("timestamp");
    assert_eq!(
        omitted.first().and_then(|field| field.reason),
        Some(OmissionReason::FreezeUnderFloor {
            floor: SignedDuration::from_hours(24 * 30),
            until: freeze + SignedDuration::from_hours(24 * 30),
        })
    );
}

/// The fields a repo-level Cargo file resolved from `repo` leaves out for `consumers`: Cargo has
/// one registry-wide age and no package table, and the writer takes no exemptions.
fn cargo_repo_scope(repo: &[PolicyLayer], consumers: &[PolicyContext<'_>]) -> Vec<OmittedField> {
    cargo_repo_scope_in(repo, consumers, registry_of(CARGO))
}

/// [`cargo_repo_scope`] for a writer that declares `registry` as its packages' registry.
fn cargo_repo_scope_in(
    repo: &[PolicyLayer],
    consumers: &[PolicyContext<'_>],
    registry: Option<&str>,
) -> Vec<OmittedField> {
    let encoding = NativeEncoding {
        default_layers: repo,
        default_project: Utf8Path::new("."),
        registry,
        package_windows: false,
        writer_exemptions: false,
    };
    omitted_native_fields(consumers, CARGO, &encoding, now())
}

/// `min-age = "0d"`, `latest = true` and `allow` exempt a package alike, so a native config that
/// cannot carry the exemption reports each spelling with the same notice.
#[test]
fn every_spelling_of_an_exemption_is_reported_alike() {
    let spellings = [
        indoc! {r#"
            min-age = "14d"

            [package."itoa"]
            min-age = "0d"
        "#},
        indoc! {r#"
            min-age = "14d"

            [package."itoa"]
            latest = true
        "#},
        indoc! {r#"
            min-age = "14d"
            allow = ["itoa"]
        "#},
    ];
    for config in spellings {
        let layers = repo_layers(config);
        let notices: Vec<String> = cargo_repo_scope(&layers, &[consumer(".", &layers)])
            .iter()
            .map(|field| field.notice(CARGO))
            .collect();
        assert_eq!(
            notices,
            [
                "the cooldown exemption from `repo:cooldown.toml:package=itoa` is not represented \
                 in cargo's native config, so a bare cargo command does not apply it"
            ],
            "{config}"
        );
    }
}

/// Only uv's package table carries an exemption, for a literal package: no writer writes an
/// exempt default, a `*` exemption leaves the project window in the default, and any other glob
/// reaches neither.
#[test]
fn only_a_package_table_carries_an_exemption() {
    let layers = repo_layers(indoc! {r#"
        latest = true
        allow = ["ruff", "types-*", "*"]
    "#});

    assert_eq!(
        keys(&repo_scope(&layers, &[consumer(".", &layers)])),
        [
            "repo:cooldown.toml exemption",
            "repo:cooldown.toml:package=types-* exemption",
            "repo:cooldown.toml:package=* exemption",
        ]
    );
}

/// `latest = true` and `min-age = "0d"` as the default window are reported alike by every writer,
/// since none writes an exempt default and a bare command keeps the native config's own cutoff.
#[test]
fn every_writer_reports_an_exempt_default_alike() {
    for config in ["latest = true\n", "min-age = \"0d\"\n"] {
        let layers = repo_layers(config);
        let consumers = [consumer(".", &layers)];
        let cases = [
            (CARGO, cargo_repo_scope(&layers, &consumers)),
            (UV, repo_scope(&layers, &consumers)),
            (PNPM, project_scope(&layers, ".", PNPM, false)),
        ];
        for (target, omitted) in cases {
            let notices: Vec<String> = omitted.iter().map(|field| field.notice(target)).collect();
            assert_eq!(
                notices,
                [format!(
                    "the cooldown exemption from `repo:cooldown.toml` is not represented in \
                     {target}'s native config, so a bare {target} command does not apply it"
                )],
                "{config} for {target}"
            );
        }
    }
}

/// `[package."*"]`'s `min-age = "0d"`, `latest = true` and `allow = ["*"]` exempt every package
/// alike, so a native config whose default keeps the project window reports each spelling with the
/// same notice, while a writer with an exemption list takes each one.
#[test]
fn every_spelling_of_a_catch_all_exemption_is_reported_alike() {
    let spellings = [
        indoc! {r#"
            min-age = "14d"

            [package."*"]
            min-age = "0d"
        "#},
        indoc! {r#"
            min-age = "14d"

            [package."*"]
            latest = true
        "#},
        indoc! {r#"
            min-age = "14d"
            allow = ["*"]
        "#},
    ];
    for config in spellings {
        let layers = repo_layers(config);
        let notices: Vec<String> = cargo_repo_scope(&layers, &[consumer(".", &layers)])
            .iter()
            .map(|field| field.notice(CARGO))
            .collect();
        assert_eq!(
            notices,
            [
                "the cooldown exemption from `repo:cooldown.toml:package=*` is not represented \
                 in cargo's native config, so a bare cargo command does not apply it"
            ],
            "{config}"
        );
        assert_eq!(
            keys(&project_scope(&layers, ".", PNPM, false)),
            Vec::<String>::new(),
            "{config}"
        );
    }
}

/// A rule that exempts a package and also sets a window keeps the window as its own field.
#[test]
fn an_exemption_beside_a_window_reports_both() {
    let layers = repo_layers(indoc! {r#"
        allow = ["itoa"]

        [package."itoa"]
        min-age = { default = "30d", major = "0d" }
    "#});

    assert_eq!(
        keys(&cargo_repo_scope(&layers, &[consumer(".", &layers)])),
        [
            "repo:cooldown.toml:package=itoa exemption",
            "repo:cooldown.toml:package=itoa default",
            "repo:cooldown.toml:package=itoa major",
        ]
    );
}

/// A writer that takes the exemptions, as pnpm's does, writes `latest` and `allow` to its own
/// list or reports them, while a zero window stays a window.
#[test]
fn a_writer_that_takes_exemptions_keeps_a_zero_window_a_window() {
    let layers = repo_layers(indoc! {r#"
        allow = ["react"]

        [package."lodash"]
        latest = true

        [package."zod"]
        min-age = "0d"
    "#});

    assert_eq!(
        keys(&project_scope(&layers, ".", PNPM, false)),
        ["repo:cooldown.toml:package=zod default"]
    );
}

/// A registry rule for the registry every Cargo package carries gates each of them, so the
/// written default folds in its window and floor, as it does the bare ones.
#[test]
fn the_written_default_encodes_the_tools_registry_fields() {
    let layers = repo_layers(indoc! {r#"
        min-age = "14d"

        [registry."crates.io"]
        min-age = "21d"
        floor = "30d"
    "#});

    assert_eq!(
        keys(&cargo_repo_scope(&layers, &[consumer(".", &layers)])),
        Vec::<String>::new()
    );
}

/// The registry's per-kind windows and its exemption reach no native config, so they are
/// reported like those of any other rule the written default folds in.
#[test]
fn the_registry_per_kind_windows_and_exemption_are_reported() {
    let layers = repo_layers(indoc! {r#"
        min-age = "14d"

        [registry."crates.io".min-age]
        major = "60d"
    "#});

    assert_eq!(
        keys(&cargo_repo_scope(&layers, &[consumer(".", &layers)])),
        ["repo:cooldown.toml:registry=crates.io major"]
    );

    for exemption in ["latest = true", "min-age = \"0d\""] {
        let layers = repo_layers(&formatdoc! {r#"
            min-age = "14d"

            [registry."crates.io"]
            {exemption}
        "#});
        let notices: Vec<String> = cargo_repo_scope(&layers, &[consumer(".", &layers)])
            .iter()
            .map(|field| field.notice(CARGO))
            .collect();
        assert_eq!(
            notices,
            [
                "the cooldown exemption from `repo:cooldown.toml:registry=crates.io` is not \
                 represented in cargo's native config, so a bare cargo command does not apply it"
            ],
            "{exemption}"
        );
    }
}

/// A rule for a registry no package of the tool carries gates none of them, so it is skipped:
/// an alternate Cargo registry, whose crates cooldown never age-evaluates, and a crates.io rule
/// on uv's row.
#[test]
fn a_rule_for_another_registry_is_skipped() {
    let layers = repo_layers(indoc! {r#"
        min-age = "14d"

        [registry."corp"]
        min-age = { default = "30d", major = "60d" }
        floor = "30d"

        [registry."crates.io"]
        floor = "30d"
    "#});
    let consumers = [consumer(".", &layers)];

    assert_eq!(
        keys(&cargo_repo_scope(&layers, &consumers)),
        Vec::<String>::new()
    );
    assert_eq!(keys(&repo_scope(&layers, &consumers)), Vec::<String>::new());
}

/// A writer that declares no registry may evaluate packages of any registry, so it cannot fold
/// a registry rule in and reports each of its gating fields.
#[test]
fn an_undeclared_registry_reports_every_registry_rule() {
    let layers = repo_layers(indoc! {r#"
        min-age = "14d"

        [registry."crates.io"]
        min-age = "21d"
        floor = "30d"
    "#});

    assert_eq!(
        keys(&cargo_repo_scope_in(
            &layers,
            &[consumer(".", &layers)],
            None
        )),
        [
            "repo:cooldown.toml:registry=crates.io default",
            "repo:cooldown.toml:registry=crates.io floor",
        ]
    );
}

/// A registry floor declared only in a nested `cooldown.toml` never reached the repo-level
/// default, so it is reported although its registry matches.
#[test]
fn repo_scope_reports_a_consumer_only_registry_floor() {
    let repo = repo_layers(r#"min-age = "14d""#);
    let mut nested = repo.clone();
    nested.push(layer(
        indoc! {r#"
            [registry."crates.io"]
            floor = "30d"
        "#},
        "apps/api/cooldown.toml",
    ));

    assert_eq!(
        keys(&cargo_repo_scope(
            &repo,
            &[consumer(".", &repo), consumer("apps/api", &nested)]
        )),
        ["repo:apps/api/cooldown.toml:registry=crates.io floor"]
    );
}

/// A registry floor binds the written default as a bare one does, so a freeze it replaces there
/// is reported with the floor's end.
#[test]
fn a_freeze_under_a_binding_registry_floor_is_reported() {
    // On 2026-01-15 a 30-day floor cuts off at 2025-12-16, before the freeze.
    let layers = repo_layers(indoc! {r#"
        freeze = "2026-01-10T00:00:00Z"

        [registry."crates.io"]
        floor = "30d"
    "#});

    let omitted = cargo_repo_scope(&layers, &[consumer(".", &layers)]);

    assert_eq!(keys(&omitted), ["repo:cooldown.toml default"]);
    assert_eq!(
        omitted.first().and_then(|field| field.reason),
        Some(OmissionReason::FreezeUnderFloor {
            floor: SignedDuration::from_hours(24 * 30),
            until: "2026-02-09T00:00:00Z".parse().expect("timestamp"),
        })
    );
}

/// uv's package table carries a literal package's window under the `pypi` floor that binds it, so
/// a freeze that floor replaces there is reported too.
#[test]
fn a_package_window_freeze_under_a_registry_floor_is_reported() {
    let layers = repo_layers(indoc! {r#"
        [registry.pypi]
        floor = "30d"

        [package."numpy"]
        freeze = "2026-01-10T00:00:00Z"
    "#});

    let omitted = project_scope(&layers, ".", UV, true);

    assert_eq!(keys(&omitted), ["repo:cooldown.toml:package=numpy default"]);
    std::assert_matches!(
        omitted.first().and_then(|field| field.reason),
        Some(OmissionReason::FreezeUnderFloor { .. })
    );
}
