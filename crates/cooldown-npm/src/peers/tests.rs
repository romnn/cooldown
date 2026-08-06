use super::*;
use crate::lock::Npm;
use crate::manifest;
use camino::Utf8PathBuf;
use cooldown_core::{RewriteMode, UpdateKind, Version};
use indoc::{formatdoc, indoc};

fn change(name: &str, from: &str, to: &str) -> Change {
    Change {
        package: PackageId::new(Npm::ID, name, Some(NPM.to_string())),
        from: Version::new(from),
        to: Version::new(to),
        kind: UpdateKind::Minor,
        downgrade: false,
        direct: true,
        members: Vec::new(),
    }
}

/// The fumadocs shape as a pnpm lock: the root importer declares `fumadocs-core` and
/// `fumadocs-mdx`; mdx peer-requires `fumadocs-core@^16.0.0`.
const PEER_LOCK: &str = indoc! {"
    lockfileVersion: '9.0'

    importers:

      .:
        dependencies:
          fumadocs-core:
            specifier: ^16.0.0
            version: 16.11.4
          fumadocs-mdx:
            specifier: ^15.0.0
            version: 15.1.1(fumadocs-core@16.11.4)

    packages:

      fumadocs-core@16.11.4:
        resolution: {integrity: sha512-aaa}

      fumadocs-mdx@15.1.1(fumadocs-core@16.11.4):
        resolution: {integrity: sha512-bbb}
        peerDependencies:
          fumadocs-core: ^16.0.0
"};

fn plan_of(changes: Vec<Change>) -> Plan {
    Plan {
        changes,
        ..Plan::default()
    }
}

/// Gathers lock-only peer evidence (no workspace root, so no manifest source) and partitions —
/// the shape most gate tests exercise.
fn peer_partition<L: NodeLock>(plan: &Plan, lock: Option<&str>) -> PeerPartition {
    partition_peer_held::<L>(plan, &PeerEvidence::gather::<L>(None, lock))
}

/// The trap itself: a cross-major target a still-present dependent's peer range excludes is
/// held up front, naming the dependent and its verbatim range — pnpm would only warn and land
/// the break silently.
#[test]
fn peer_gate_holds_a_cross_major_target_excluded_by_a_dependent_range() {
    let plan = plan_of(vec![change("fumadocs-core", "16.11.4", "17.0.0")]);

    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&plan, Some(PEER_LOCK));

    assert!(
        retained.changes.is_empty(),
        "the gated change never resolves"
    );
    let held = skipped.first().expect("one peer hold");
    assert_eq!(held.reason, SkipReason::PeerHeld);
    assert_eq!(
        held.offending.as_ref().map(|package| package.name.as_str()),
        Some("fumadocs-mdx")
    );
    assert_eq!(
        held.detail.as_deref(),
        Some("held: fumadocs-mdx@15.1.1 requires fumadocs-core@^16.0.0")
    );
}

/// A peer range that unions majors (`^7.0.0 || ^8.0.0`, the common peer idiom) gates a move
/// beyond the union and passes one within it.
#[test]
fn peer_gate_judges_union_ranges() {
    let lock = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              eslint:
                specifier: ^8.40.0
                version: 8.57.0
              '@typescript-eslint/eslint-plugin':
                specifier: 6.21.0
                version: 6.21.0(eslint@8.57.0)

        packages:

          '@typescript-eslint/eslint-plugin@6.21.0':
            resolution: {integrity: sha512-aaa}
            peerDependencies:
              eslint: ^7.0.0 || ^8.0.0
    "};

    // 8 → 9 leaves the union: held, blaming the plugin.
    let cross = plan_of(vec![change("eslint", "8.57.0", "9.8.0")]);
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&cross, Some(lock));
    assert!(retained.changes.is_empty());
    assert_eq!(
        skipped
            .first()
            .and_then(|held| held.offending.as_ref())
            .map(|package| package.name.as_str()),
        Some("@typescript-eslint/eslint-plugin")
    );

    // 7 → 8 stays within the union: the resolver's business.
    let within = plan_of(vec![change("eslint", "7.32.0", "8.57.0")]);
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&within, Some(lock));
    assert_eq!(retained.changes.len(), 1);
    assert!(skipped.is_empty());
}

/// A *transitive* dependent never gates: the resolver may float it within its parents' ranges
/// to a sibling version whose peer range admits the target (npm does exactly this), so its
/// lock-recorded peer range is not authoritative — the real-world `eslint-plugin-jsdoc` shape.
#[test]
fn peer_gate_never_gates_on_a_transitive_dependent() {
    let lock = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              eslint:
                specifier: ^9.16.0
                version: 9.16.0
              eslint-config-treesitter:
                specifier: ^1.0.2
                version: 1.0.2(eslint@9.16.0)

        packages:

          eslint-plugin-jsdoc@50.6.0(eslint@9.16.0):
            resolution: {integrity: sha512-aaa}
            peerDependencies:
              eslint: ^7.0.0 || ^8.0.0 || ^9.0.0
    "};

    let plan = plan_of(vec![change("eslint", "9.16.0", "10.6.0")]);
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&plan, Some(lock));
    assert_eq!(
        retained.changes.len(),
        1,
        "a transitive dependent's peer range must not hold the move"
    );
    assert!(skipped.is_empty());
}

/// Fail-open rules: an in-range move is the resolver's business, and a dependent moving in the
/// same plan may lift its own peer range, so joint moves stay with the resolver too.
#[test]
fn peer_gate_passes_in_range_moves_and_joint_moves() {
    // A minor move inside the peer range never gates.
    let minor = plan_of(vec![change("fumadocs-core", "16.11.4", "16.13.0")]);
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&minor, Some(PEER_LOCK));
    assert_eq!(retained.changes.len(), 1);
    assert!(skipped.is_empty());

    // The dependent co-moves in the same plan, so the resolver decides joint feasibility.
    // This is deliberately fail-open because the lock records only the dependent's *current* peer
    // range, so
    // whether its target admits the moved package is unknowable here; the resolve that follows
    // is the authority (pnpm settles both peer contexts in its one whole-graph pass).
    let joint = plan_of(vec![
        change("fumadocs-core", "16.11.4", "17.0.0"),
        change("fumadocs-mdx", "15.1.1", "16.0.0"),
    ]);
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&joint, Some(PEER_LOCK));
    assert_eq!(retained.changes.len(), 2);
    assert!(skipped.is_empty());

    // No lock captured (a fresh project): nothing to prove, nothing gated.
    let plan = plan_of(vec![change("fumadocs-core", "16.11.4", "17.0.0")]);
    let PeerPartition { retained, skipped } = peer_partition::<crate::lock::Pnpm>(&plan, None);
    assert_eq!(retained.changes.len(), 1);
    assert!(skipped.is_empty());
}

/// A dependent declared only by *other* importers keeps its own in-range copy of the package
/// (pnpm resolves peers per importing context), so a change scoped to disjoint members passes.
#[test]
fn peer_gate_passes_a_change_whose_members_are_disjoint_from_the_dependent() {
    let lock = indoc! {"
        lockfileVersion: '9.0'

        importers:

          apps/site:
            dependencies:
              fumadocs-core:
                specifier: ^16.0.0
                version: 16.11.4

          apps/docs:
            dependencies:
              fumadocs-core:
                specifier: ^16.0.0
                version: 16.11.4
              fumadocs-mdx:
                specifier: ^15.0.0
                version: 15.1.1(fumadocs-core@16.11.4)

        packages:

          fumadocs-core@16.11.4:
            resolution: {integrity: sha512-aaa}

          fumadocs-mdx@15.1.1(fumadocs-core@16.11.4):
            resolution: {integrity: sha512-bbb}
            peerDependencies:
              fumadocs-core: ^16.0.0
    "};
    let member = |path: &str| MemberRef {
        name: path.to_string(),
        path: path.to_string(),
    };

    // fumadocs-core is declared by both importers, so it is multi-version-safe here only via
    // members: a change scoped to `apps/site` cannot break `apps/docs`'s mdx peer.
    let mut site = change("fumadocs-core", "16.11.4", "17.0.0");
    site.members = vec![member("apps/site")];
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&plan_of(vec![site]), Some(lock));
    assert_eq!(retained.changes.len(), 1, "disjoint importers pass");
    assert!(skipped.is_empty());

    // Scoped to the importer that also declares the dependent, the gate fires.
    let mut docs = change("fumadocs-core", "16.11.4", "17.0.0");
    docs.members = vec![member("apps/docs")];
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&plan_of(vec![docs]), Some(lock));
    assert!(retained.changes.is_empty());
    assert_eq!(skipped.len(), 1);
}

/// Exclusion must be *proven*: a range with a branch the matcher cannot represent (an npm
/// hyphen range) yields `Unknown`, never a hold — while a fully understood union (x-wildcards
/// included) still gates.
#[test]
fn peer_gate_never_holds_on_a_range_with_an_unrepresentable_branch() {
    let lock_with_range = |range: &str| {
        formatdoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  fumadocs-core:
                    specifier: ^16.0.0
                    version: 16.11.4
                  fumadocs-mdx:
                    specifier: ^15.0.0
                    version: 15.1.1(fumadocs-core@16.11.4)

            packages:

              fumadocs-mdx@15.1.1(fumadocs-core@16.11.4):
                resolution: {{integrity: sha512-bbb}}
                peerDependencies:
                  fumadocs-core: '{range}'
        "}
    };
    let plan = plan_of(vec![change("fumadocs-core", "16.11.4", "18.0.0")]);

    // The hyphen branch is unrepresentable: current matches `^16.0.0`, but excluding 18.0.0
    // cannot be proven, so the move passes to the resolver.
    let union = lock_with_range("^16.0.0 || 17.0.0 - 17.4.0");
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&plan, Some(&union));
    assert_eq!(retained.changes.len(), 1, "unproven exclusion never holds");
    assert!(skipped.is_empty());

    // The x-wildcard union is fully understood: 18.0.0 is provably outside it.
    let wildcard = lock_with_range("^16.0.0 || 17.x");
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&plan, Some(&wildcard));
    assert!(retained.changes.is_empty());
    assert_eq!(skipped.len(), 1);
}

/// An *optional* peer gates like any other when the peer is present: optionality tolerates
/// absence (npm skips auto-installing it), not a present copy outside the declared range — and
/// the queried peer is by construction present, it is the package being upgraded.
#[test]
fn peer_gate_holds_an_optional_peer_that_is_present_but_incompatible() {
    let lock = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              typescript:
                specifier: ^5.5.0
                version: 5.5.4
              ts-linter:
                specifier: ^3.0.0
                version: 3.2.0(typescript@5.5.4)

        packages:

          ts-linter@3.2.0(typescript@5.5.4):
            resolution: {integrity: sha512-aaa}
            peerDependencies:
              typescript: '>=5 <6'
            peerDependenciesMeta:
              typescript:
                optional: true
    "};

    let plan = plan_of(vec![change("typescript", "5.5.4", "6.0.0")]);
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&plan, Some(lock));
    assert!(retained.changes.is_empty());
    assert_eq!(
        skipped
            .first()
            .and_then(|held| held.offending.as_ref())
            .map(|package| package.name.as_str()),
        Some("ts-linter")
    );
}

/// `0.1 → 0.2` crosses an npm compatibility line (caret semantics make the 0.x minor the
/// breaking axis), so it is gated exactly like a numeric major jump — while a same-line `0.1`
/// patch move stays the resolver's business.
#[test]
fn peer_gate_gates_a_zero_line_jump() {
    let lock = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              zod-mini:
                specifier: ~0.1.0
                version: 0.1.5
              zod-adapter:
                specifier: ^2.0.0
                version: 2.0.0(zod-mini@0.1.5)

        packages:

          zod-adapter@2.0.0(zod-mini@0.1.5):
            resolution: {integrity: sha512-aaa}
            peerDependencies:
              zod-mini: ~0.1.0
    "};

    let cross = plan_of(vec![change("zod-mini", "0.1.5", "0.2.0")]);
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&cross, Some(lock));
    assert!(retained.changes.is_empty(), "0.1 → 0.2 is a breaking jump");
    assert_eq!(
        skipped
            .first()
            .and_then(|held| held.offending.as_ref())
            .map(|package| package.name.as_str()),
        Some("zod-adapter")
    );

    let within = plan_of(vec![change("zod-mini", "0.1.5", "0.1.9")]);
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&within, Some(lock));
    assert_eq!(retained.changes.len(), 1);
    assert!(skipped.is_empty());
}

/// In `0.0.x` the caret admits nothing beyond the exact version (`^0.0.3` ⇔ `=0.0.3`), so even
/// a patch step is a breaking move: a dependent's `^0.0.3` provably excludes `0.0.4` and the
/// gate must consult that proof rather than exit on "same line".
#[test]
fn peer_gate_gates_a_double_zero_patch_step() {
    let lock = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              proto-kit:
                specifier: ^0.0.3
                version: 0.0.3
              proto-kit-adapter:
                specifier: ^1.0.0
                version: 1.0.0(proto-kit@0.0.3)

        packages:

          proto-kit-adapter@1.0.0(proto-kit@0.0.3):
            resolution: {integrity: sha512-aaa}
            peerDependencies:
              proto-kit: ^0.0.3
    "};

    let plan = plan_of(vec![change("proto-kit", "0.0.3", "0.0.4")]);
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&plan, Some(lock));
    assert!(
        retained.changes.is_empty(),
        "a 0.0.x step that provably breaks a peer range must hold"
    );
    assert_eq!(
        skipped
            .first()
            .and_then(|held| held.offending.as_ref())
            .map(|package| package.name.as_str()),
        Some("proto-kit-adapter")
    );
}

/// npm's package-lock attributes importer declarations by *name* only, but the physical
/// layout is instance-exact: the member's own nearest-ancestor lookup identifies its direct
/// copy, so a name resolved at several versions is no longer ambiguity.
/// Both directions
/// matter — the nested copy's stricter range must not be promoted to a blocker, and the
/// direct copy's stricter range must not be blinded by the nested split (the escape a blanket
/// split fail-open used to leave).
#[test]
fn peer_gate_resolves_npm_name_splits_physically() {
    let nested_would_block = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "fixture",
                "dependencies": { "eslint": "^8.40.0", "eslint-plugin-legacy": "^2.0.0" }
            },
            "node_modules/eslint": { "version": "8.57.0" },
            "node_modules/eslint-plugin-legacy": {
                "version": "2.0.0",
                "peerDependencies": { "eslint": "^8.0.0 || ^9.0.0" }
            },
            "node_modules/report-tool/node_modules/eslint-plugin-legacy": {
                "version": "1.0.0",
                "peerDependencies": { "eslint": "^8.0.0" }
            }
        }
    }"#};
    let plan = plan_of(vec![change("eslint", "8.57.0", "9.8.0")]);

    // The nested 1.0.0 copy would bind, but the root's lookup proves its direct copy is the
    // admitting 2.0.0 — the nested record is the transitive one and holds nothing.
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Npm>(&plan, Some(nested_would_block));
    assert_eq!(
        retained.changes.len(),
        1,
        "the nested transitive copy must not be promoted to a blocker"
    );
    assert!(skipped.is_empty());

    // The inverse split has the DIRECT copy blocking while a nested copy admits.
    // The split must
    // not blind the gate — the root's own instance is identified physically and holds.
    let direct_blocks = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "fixture",
                "dependencies": { "eslint": "^8.40.0", "eslint-plugin-legacy": "^1.0.0" }
            },
            "node_modules/eslint": { "version": "8.57.0" },
            "node_modules/eslint-plugin-legacy": {
                "version": "1.0.0",
                "peerDependencies": { "eslint": "^8.0.0" }
            },
            "node_modules/report-tool/node_modules/eslint-plugin-legacy": {
                "version": "2.0.0",
                "peerDependencies": { "eslint": "^8.0.0 || ^9.0.0" }
            }
        }
    }"#};
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Npm>(&plan, Some(direct_blocks));
    assert!(
        retained.changes.is_empty(),
        "the direct copy's contract holds despite the nested split"
    );
    assert_eq!(
        skipped
            .first()
            .and_then(|held| held.offending.as_ref())
            .map(|package| package.name.as_str()),
        Some("eslint-plugin-legacy")
    );

    // One resolved version + a declaring importer: that instance IS the direct dependency.
    let unambiguous = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "fixture",
                "dependencies": { "eslint": "^8.40.0", "eslint-plugin-legacy": "^1.0.0" }
            },
            "node_modules/eslint": { "version": "8.57.0" },
            "node_modules/eslint-plugin-legacy": {
                "version": "1.0.0",
                "peerDependencies": { "eslint": "^8.0.0" }
            }
        }
    }"#};
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Npm>(&plan, Some(unambiguous));
    assert!(retained.changes.is_empty());
    assert_eq!(
        skipped
            .first()
            .and_then(|held| held.offending.as_ref())
            .map(|package| package.name.as_str()),
        Some("eslint-plugin-legacy")
    );
}

/// A published peer contract is never rewritten out from under itself, so a narrow
/// author-written bound holds even a same-line move: `peerDependencies.chalk =
/// ">=5.6.0 <5.6.2"` excludes a 5.6.0 → 5.6.2 patch bump that no compatibility-line test
/// sees.
/// The alternative — letting the widen shift the contract to `^5.6.2` so the move can
/// land — silently drops the consumers on 5.6.0/5.6.1 that the author still supports.
#[test]
fn workspace_manifest_peer_holds_a_same_line_move_its_narrow_bound_excludes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
    let manifest = indoc! {r#"{
        "name": "root-lib",
        "version": "1.2.0",
        "devDependencies": { "chalk": "^5.6.0" },
        "peerDependencies": { "chalk": ">=5.6.0 <5.6.2" }
    }"#};
    std::fs::write(root.join("package.json"), manifest).expect("root manifest");
    let lock = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "root-lib",
                "version": "1.2.0",
                "devDependencies": { "chalk": "^5.6.0" },
                "peerDependencies": { "chalk": ">=5.6.0 <5.6.2" }
            },
            "node_modules/chalk": { "version": "5.6.0" }
        }
    }"#};

    let evidence = PeerEvidence::gather::<crate::lock::Npm>(Some(&root), Some(lock));
    let plan = plan_of(vec![change("chalk", "5.6.0", "5.6.2")]);
    let PeerPartition { retained, skipped } =
        partition_peer_held::<crate::lock::Npm>(&plan, &evidence);
    assert!(
        retained.changes.is_empty(),
        "a provable break holds even without crossing a compatibility line"
    );
    assert_eq!(
        skipped
            .first()
            .and_then(|held| held.offending.as_ref())
            .map(|package| package.name.as_str()),
        Some("root-lib"),
        "the hold names the local package whose contract must be edited"
    );

    // The gate holds the move *before* any widen runs, and a widen could not touch the
    // contract anyway: the published field is outside the write set, so the pre-apply
    // snapshot the post-resolve verifier judges against can never go stale.
    manifest::widen_constraints(&root, &[], "chalk", "5.6.2", RewriteMode::Always).expect("widen");
    let after = std::fs::read_to_string(root.join("package.json")).expect("read manifest");
    assert!(
        after.contains(r#""chalk": ">=5.6.0 <5.6.2""#),
        "the published peer contract must survive a widen verbatim: {after}"
    );
    assert!(
        after.contains(r#""chalk": "^5.6.2""#),
        "the install declaration is still widened: {after}"
    );
}

/// A `fix` downgrade is exempt from the pre-gate (rolling back is its whole purpose), so a
/// downgrade that lands below a local package's published peer floor is caught by the
/// post-resolve verifier instead — and blamed on the moving package, since the local dependent
/// never moves.
/// Cooldown neither commits the break nor lowers the author's published floor: it
/// reports and rolls back, leaving the remedy (relax the range, or baseline the violation) to
/// the author.
#[test]
fn workspace_manifest_peer_floor_rejects_a_downgrade_below_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
    std::fs::write(
        root.join("package.json"),
        indoc! {r#"{
            "name": "root-lib",
            "version": "1.2.0",
            "devDependencies": { "chalk": "^5.6.2" },
            "peerDependencies": { "chalk": ">=5.6.2" }
        }"#},
    )
    .expect("root manifest");
    let lock = |version: &str| {
        formatdoc! {r#"{{
            "lockfileVersion": 3,
            "packages": {{
                "": {{
                    "name": "root-lib",
                    "version": "1.2.0",
                    "devDependencies": {{ "chalk": "^5.6.2" }},
                    "peerDependencies": {{ "chalk": ">=5.6.2" }}
                }},
                "node_modules/chalk": {{ "version": "{version}" }}
            }}
        }}"#}
    };
    let before = lock("5.6.2");
    let evidence = PeerEvidence::gather::<crate::lock::Npm>(Some(&root), Some(&before));

    // The pre-gate lets the downgrade through — `fix` must be able to roll back.
    let mut downgrade = change("chalk", "5.6.2", "5.6.0");
    downgrade.downgrade = true;
    let PeerPartition { retained, .. } =
        partition_peer_held::<crate::lock::Npm>(&plan_of(vec![downgrade.clone()]), &evidence);
    assert_eq!(retained.changes.len(), 1, "a downgrade is not pre-held");

    // The landed graph is then post-verified and rejected, blaming the moved package.
    let after = lock("5.6.0");
    let baseline = PeerBaseline::gather::<crate::lock::Npm>(Some(&before), &evidence.workspace);
    let current = proven_peer_violations::<crate::lock::Npm>(&after, &evidence.workspace);
    assert_eq!(
        current
            .keys()
            .map(|id| (id.dependent.as_str(), id.range.as_str()))
            .collect::<Vec<_>>(),
        vec![("root-lib", ">=5.6.2")],
        "the downgrade provably breaks the published floor"
    );
    let rejections = plan_peer_rejections(
        &baseline,
        &current,
        &plan_of(vec![downgrade]),
        &HashSet::new(),
    )
    .expect("uniquely attributable");
    assert_eq!(
        rejections
            .first()
            .map(|rejection| rejection.offending.as_str()),
        Some("root-lib"),
        "the rejection names the contract holder, not a guess"
    );
}

/// Workspace-manifest contracts survive the pre-gate into post-resolve verification: a
/// *collateral* move — one the resolve dragged in, never a planned candidate — that breaks a
/// local package's recorded contract is a proven violation with no culpable candidate, so the
/// round escalates to candidate isolation instead of committing the break.
#[test]
fn workspace_manifest_peer_violations_are_post_verified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
    std::fs::create_dir_all(root.join("packages/shim")).expect("mkdir");
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "root-app", "dependencies": { "eslint": "^8.40.0" } }"#,
    )
    .expect("root manifest");
    std::fs::write(
        root.join("packages/shim/package.json"),
        r#"{ "name": "local-eslint-shim", "version": "0.1.0", "peerDependencies": { "eslint": "^8.0.0" } }"#,
    )
    .expect("member manifest");
    let before = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              eslint:
                specifier: ^8.40.0
                version: 8.57.0
              local-eslint-shim:
                specifier: workspace:*
                version: link:packages/shim

          packages/shim: {}
    "};
    let evidence = PeerEvidence::gather::<crate::lock::Pnpm>(Some(&root), Some(before));
    assert!(
        proven_peer_violations::<crate::lock::Pnpm>(before, &evidence.workspace).is_empty(),
        "the pre-apply graph satisfies the shim's contract"
    );

    // The resolve floated eslint across the major on its own — no planned candidate did it.
    let after = before.replace("version: 8.57.0", "version: 9.8.0");
    let baseline = PeerBaseline::gather::<crate::lock::Pnpm>(Some(before), &evidence.workspace);
    let current = proven_peer_violations::<crate::lock::Pnpm>(&after, &evidence.workspace);
    assert_eq!(
        current
            .keys()
            .map(|id| id.dependent.as_str())
            .collect::<Vec<_>>(),
        vec!["local-eslint-shim"],
        "the workspace contract is checked after the resolve, not only before it"
    );
    assert!(
        plan_peer_rejections(
            &baseline,
            &current,
            &plan_of(vec![change("unrelated", "1.0.0", "1.1.0")]),
            &HashSet::new(),
        )
        .is_err(),
        "a collateral break with no culpable candidate escalates to candidate isolation"
    );
}

/// npm's package-lock deliberately records no peers for the root project, delegating them to
/// the workspace-manifest source — and the root is not an installed instance either, so a
/// name-keyed instance lookup finds nothing and the contract goes ungated.
/// A root library
/// declaring `peerDependencies.eslint = "^8"` must hold an eslint 8→9 move: without the hold,
/// apply's manifest widening rewrites that very `peerDependencies` entry, silently changing
/// the package's own published contract.
#[test]
fn workspace_manifest_peer_holds_the_root_projects_own_contract() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
    std::fs::write(
        root.join("package.json"),
        indoc! {r#"{
            "name": "root-lib",
            "version": "1.2.0",
            "devDependencies": { "eslint": "^8.40.0" },
            "peerDependencies": { "eslint": "^8.0.0" }
        }"#},
    )
    .expect("root manifest");
    let lock = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "root-lib",
                "version": "1.2.0",
                "devDependencies": { "eslint": "^8.40.0" },
                "peerDependencies": { "eslint": "^8.0.0" }
            },
            "node_modules/eslint": { "version": "8.57.0" }
        }
    }"#};

    let evidence = PeerEvidence::gather::<crate::lock::Npm>(Some(&root), Some(lock));
    let plan = plan_of(vec![change("eslint", "8.57.0", "9.8.0")]);
    let PeerPartition { retained, skipped } =
        partition_peer_held::<crate::lock::Npm>(&plan, &evidence);
    assert!(
        retained.changes.is_empty(),
        "the root project's own peer contract must gate the move"
    );
    assert_eq!(
        skipped
            .first()
            .and_then(|held| held.offending.as_ref())
            .map(|package| package.name.as_str()),
        Some("root-lib"),
        "the hold names the local package whose contract must be edited deliberately"
    );

    // The same contract, post-verified: a resolver move that lands eslint 9 anyway is a
    // proven violation of the root's recorded contract, so the apply can roll it back.
    let after = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "root-lib",
                "version": "1.2.0",
                "devDependencies": { "eslint": "^8.40.0" },
                "peerDependencies": { "eslint": "^8.0.0" }
            },
            "node_modules/eslint": { "version": "9.8.0" }
        }
    }"#};
    assert!(
        proven_peer_violations::<crate::lock::Npm>(lock, &evidence.workspace).is_empty(),
        "the pre-apply graph satisfies the contract"
    );
    let violations = proven_peer_violations::<crate::lock::Npm>(after, &evidence.workspace);
    assert_eq!(
        violations
            .keys()
            .map(|id| (id.dependent.as_str(), id.package.as_str()))
            .collect::<Vec<_>>(),
        vec![("root-lib", "eslint")],
        "the landed graph provably breaks the root's contract"
    );
}

/// A workspace contract binds through its manifest's OWN directory, never through a same-named
/// package elsewhere in the tree: a registry dependency that happens to share the local
/// package's name must not stand in for it and fabricate a hold.
#[test]
fn workspace_manifest_peer_ignores_a_same_name_registry_decoy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
    std::fs::create_dir_all(root.join("packages/shim")).expect("mkdir");
    std::fs::create_dir_all(root.join("apps/site")).expect("mkdir");
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "root-app", "workspaces": ["apps/*", "packages/*"] }"#,
    )
    .expect("root manifest");
    std::fs::write(
        root.join("apps/site/package.json"),
        r#"{ "name": "site", "dependencies": { "eslint": "^8.40.0", "toolkit": "^1.0.0" } }"#,
    )
    .expect("app manifest");
    // The local package declares the peer, but its own directory holds a nested eslint copy
    // the change never rewrites.
    // A registry `toolkit` of the same name sits hoisted at the
    // root and *does* resolve the rewritten copy — a name-keyed lookup would let it stand in
    // for the local package and fabricate a hold.
    std::fs::write(
        root.join("packages/shim/package.json"),
        r#"{ "name": "toolkit", "version": "0.1.0", "peerDependencies": { "eslint": "^8.0.0" } }"#,
    )
    .expect("member manifest");
    let lock = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "root-app" },
            "apps/site": {
                "name": "site",
                "dependencies": { "eslint": "^8.40.0", "toolkit": "^1.0.0" }
            },
            "packages/shim": { "name": "toolkit", "version": "0.1.0" },
            "packages/shim/node_modules/eslint": { "version": "8.57.0" },
            "node_modules/toolkit": { "version": "1.0.0" },
            "node_modules/eslint": { "version": "8.57.0" }
        }
    }"#};

    let evidence = PeerEvidence::gather::<crate::lock::Npm>(Some(&root), Some(lock));
    assert!(
        evidence
            .workspace
            .iter()
            .any(|peer| peer.origin == "packages/shim"),
        "the local package's contract is collected with its origin"
    );
    let mut eslint = change("eslint", "8.57.0", "9.8.0");
    eslint.members = vec![MemberRef {
        name: "site".to_string(),
        path: "apps/site".to_string(),
    }];
    let PeerPartition { retained, skipped } =
        partition_peer_held::<crate::lock::Npm>(&plan_of(vec![eslint]), &evidence);
    assert_eq!(
        retained.changes.len(),
        1,
        "the same-named registry copy must not bind the local contract: {skipped:?}"
    );
    assert!(
        proven_peer_violations::<crate::lock::Npm>(lock, &evidence.workspace).is_empty(),
        "the local package's own nested copy satisfies its contract"
    );
}

/// A *workspace-local* package's peer contract lives only in its own `package.json` — pnpm
/// records a linked package in the lock without its peer metadata — so the gate must read the
/// member manifests: a cross-major move that provably breaks a linked dependent's peer range
/// is held, blamed on the local package.
/// The binding contexts are the package's own directory
/// plus its link consumers, so a change scoped to an unrelated importer still passes.
#[test]
fn workspace_manifest_peer_holds_a_cross_major_move() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
    std::fs::create_dir_all(root.join("packages/shim")).expect("mkdir");
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "root-app", "dependencies": { "eslint": "^8.40.0" } }"#,
    )
    .expect("root manifest");
    std::fs::write(
        root.join("packages/shim/package.json"),
        r#"{ "name": "local-eslint-shim", "version": "0.1.0", "peerDependencies": { "eslint": "^8.0.0" } }"#,
    )
    .expect("member manifest");
    let lock = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              eslint:
                specifier: ^8.40.0
                version: 8.57.0
              local-eslint-shim:
                specifier: workspace:*
                version: link:packages/shim

          packages/shim: {}
    "};

    let evidence = PeerEvidence::gather::<crate::lock::Pnpm>(Some(&root), Some(lock));
    assert_eq!(
        evidence.workspace.len(),
        1,
        "the shim's manifest peer is collected"
    );
    assert_eq!(
        evidence
            .workspace
            .first()
            .map(|peer| peer.contexts.clone())
            .unwrap_or_default(),
        vec![".".to_string(), "packages/shim".to_string()],
        "the peer binds in the shim's own dir and its link consumer"
    );

    // The provably breaking cross-major move is held, blamed on the local package.
    let plan = plan_of(vec![change("eslint", "8.57.0", "9.8.0")]);
    let PeerPartition { retained, skipped } =
        partition_peer_held::<crate::lock::Pnpm>(&plan, &evidence);
    assert!(retained.changes.is_empty());
    assert_eq!(
        skipped.first().and_then(|held| held.detail.as_deref()),
        Some("held: local-eslint-shim@0.1.0 requires eslint@^8.0.0")
    );

    // Scoped to an importer outside the peer's binding contexts, the same move passes.
    let mut scoped = change("eslint", "8.57.0", "9.8.0");
    scoped.members = vec![MemberRef {
        name: "other".into(),
        path: "apps/other".into(),
    }];
    let PeerPartition { retained, skipped } =
        partition_peer_held::<crate::lock::Pnpm>(&plan_of(vec![scoped]), &evidence);
    assert_eq!(retained.changes.len(), 1);
    assert!(skipped.is_empty());
}

/// An *injected* workspace dependency (`dependenciesMeta.*.injected`) is recorded as a
/// root-relative `file:` version with a `(peer@x)` context suffix, not a `link:` — a second
/// encoding of the same domain fact (a locally consumed package whose peers live in its
/// manifest).
/// The gate must reach the same hold through it: the injected shim's manifest peer
/// range holds the cross-major move, blamed on the shim.
#[test]
fn workspace_manifest_peer_holds_an_injected_cross_major_move() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
    std::fs::create_dir_all(root.join("packages/shim")).expect("mkdir");
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "root-app", "dependencies": { "eslint": "^8.40.0" } }"#,
    )
    .expect("root manifest");
    std::fs::write(
        root.join("packages/shim/package.json"),
        r#"{ "name": "local-eslint-shim", "version": "0.1.0", "peerDependencies": { "eslint": "^8.0.0" } }"#,
    )
    .expect("member manifest");
    let lock = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              eslint:
                specifier: ^8.40.0
                version: 8.57.1
              local-eslint-shim:
                specifier: workspace:*
                version: 'file:packages/shim(eslint@8.57.1)'
            dependenciesMeta:
              local-eslint-shim:
                injected: true

          packages/shim: {}

        packages:

          local-eslint-shim@file:packages/shim:
            resolution: {directory: packages/shim, type: directory}
            peerDependencies:
              eslint: ^8.0.0
    "};

    let evidence = PeerEvidence::gather::<crate::lock::Pnpm>(Some(&root), Some(lock));
    assert_eq!(
        evidence
            .workspace
            .first()
            .map(|peer| peer.contexts.clone())
            .unwrap_or_default(),
        vec![".".to_string(), "packages/shim".to_string()],
        "the peer binds in the shim's own dir and its injecting consumer"
    );

    let plan = plan_of(vec![change("eslint", "8.57.1", "10.8.0")]);
    let PeerPartition { retained, skipped } =
        partition_peer_held::<crate::lock::Pnpm>(&plan, &evidence);
    assert!(retained.changes.is_empty());
    assert_eq!(
        skipped.first().and_then(|held| held.detail.as_deref()),
        Some("held: local-eslint-shim@0.1.0 requires eslint@^8.0.0")
    );
}

/// The injected path itself may end in a parenthesized directory group carrying `@` —
/// `file:packages/shim(foo@bar)(eslint@8.57.1)` is real pnpm 11 output for a member named
/// `shim(foo@bar)` — so the gate must recover the path against the importer set and find the
/// manifest there; any scalar-only suffix split would read the wrong directory (or none) and
/// lose the hold.
#[test]
fn workspace_manifest_peer_holds_across_a_parenthesized_injected_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
    std::fs::create_dir_all(root.join("packages/shim(foo@bar)")).expect("mkdir");
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "root-app", "dependencies": { "eslint": "^8.40.0" } }"#,
    )
    .expect("root manifest");
    std::fs::write(
        root.join("packages/shim(foo@bar)/package.json"),
        r#"{ "name": "local-eslint-shim", "version": "0.1.0", "peerDependencies": { "eslint": "^8.0.0" } }"#,
    )
    .expect("member manifest");
    let lock = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              eslint:
                specifier: ^8.40.0
                version: 8.57.1
              local-eslint-shim:
                specifier: workspace:*
                version: 'file:packages/shim(foo@bar)(eslint@8.57.1)'

          'packages/shim(foo@bar)': {}
    "};

    let evidence = PeerEvidence::gather::<crate::lock::Pnpm>(Some(&root), Some(lock));
    let plan = plan_of(vec![change("eslint", "8.57.1", "10.8.0")]);
    let PeerPartition { retained, skipped } =
        partition_peer_held::<crate::lock::Pnpm>(&plan, &evidence);
    assert!(retained.changes.is_empty());
    assert_eq!(
        skipped.first().and_then(|held| held.detail.as_deref()),
        Some("held: local-eslint-shim@0.1.0 requires eslint@^8.0.0")
    );
}

/// npm's sequential per-package path never judges a pair jointly, so a co-moving dependent
/// grants NO exemption there: the excluded target stays held against the dependent's current
/// range while the dependent's own move proceeds (the next run reads its new range). pnpm's
/// whole-graph resolve keeps the joint exemption — see
/// `peer_gate_passes_in_range_moves_and_joint_moves`.
#[test]
fn peer_gate_grants_no_co_move_exemption_on_the_per_package_path() {
    let lock = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "fixture",
                "dependencies": { "eslint": "^8.40.0", "eslint-plugin-legacy": "^1.0.0" }
            },
            "node_modules/eslint": { "version": "8.57.0" },
            "node_modules/eslint-plugin-legacy": {
                "version": "1.0.0",
                "peerDependencies": { "eslint": "^8.0.0" }
            }
        }
    }"#};

    let joint = plan_of(vec![
        change("eslint", "8.57.0", "9.8.0"),
        change("eslint-plugin-legacy", "1.0.0", "2.0.0"),
    ]);
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Npm>(&joint, Some(lock));
    let retained_names: Vec<&str> = retained
        .changes
        .iter()
        .map(|change| change.package.name.as_str())
        .collect();
    assert_eq!(
        retained_names,
        vec!["eslint-plugin-legacy"],
        "the dependent's own move proceeds; the excluded target does not ride along"
    );
    assert_eq!(
        skipped
            .first()
            .and_then(|held| held.offending.as_ref())
            .map(|package| package.name.as_str()),
        Some("eslint-plugin-legacy")
    );
}

/// The sequential path's post-condition: a landed candidate whose lock now provably violates a
/// peer contract the pre-candidate lock did not (the dependent moved alone and its *new* range
/// excludes the still-held peer — what `legacy-peer-deps` commits with only a warning) is a
/// break the candidate caused.
/// A contract the graph already broke, or an unchanged lock, is
/// never re-attributed to the candidate.
#[test]
fn post_apply_diff_detects_only_new_proven_peer_violations() {
    let before = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "fixture",
                "dependencies": { "react": "^18.3.1", "react-dom": "^18.3.1" }
            },
            "node_modules/react": { "version": "18.3.1" },
            "node_modules/react-dom": {
                "version": "18.3.1",
                "peerDependencies": { "react": "^18.3.1" }
            }
        }
    }"#};
    let after = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "fixture",
                "dependencies": { "react": "^18.3.1", "react-dom": "^19.1.0" }
            },
            "node_modules/react": { "version": "18.3.1" },
            "node_modules/react-dom": {
                "version": "19.1.0",
                "peerDependencies": { "react": "^19.1.0" }
            }
        }
    }"#};

    let violation = first_new_peer_violation::<crate::lock::Npm>(Some(before), after)
        .expect("the dependent's new range provably excludes the held peer");
    assert_eq!(
        (
            violation.dependent.as_str(),
            violation.dependent_version.as_str(),
            violation.package.as_str(),
            violation.range.as_str(),
        ),
        ("react-dom", "19.1.0", "react", "^19.1.0")
    );

    assert!(
        first_new_peer_violation::<crate::lock::Npm>(Some(before), before).is_none(),
        "an unchanged lock introduces nothing"
    );
    assert!(
        first_new_peer_violation::<crate::lock::Npm>(Some(after), after).is_none(),
        "a pre-existing violation is not re-attributed to the candidate"
    );
}

/// The post-condition holds only on proof, the gate's shared rule: a dependent whose own
/// context binds a *satisfying* nested copy, an absent peer (possibly optional), or a range
/// the translator cannot prove all yield nothing.
#[test]
fn post_apply_diff_fails_open_without_proof() {
    let satisfied_nested = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "fixture", "dependencies": { "react-dom": "^19.1.0" } },
            "node_modules/react": { "version": "18.3.1" },
            "node_modules/react-dom": {
                "version": "19.1.0",
                "peerDependencies": { "react": "^19.1.0" }
            },
            "node_modules/react-dom/node_modules/react": { "version": "19.1.0" }
        }
    }"#};
    assert!(
        first_new_peer_violation::<crate::lock::Npm>(None, satisfied_nested).is_none(),
        "the dependent's own lookup binds its satisfying nested copy, not the root's 18.x"
    );

    let absent_peer = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "fixture", "dependencies": { "react-dom": "^19.1.0" } },
            "node_modules/react-dom": {
                "version": "19.1.0",
                "peerDependencies": { "react": "^19.1.0" }
            }
        }
    }"#};
    assert!(
        first_new_peer_violation::<crate::lock::Npm>(None, absent_peer).is_none(),
        "an absent peer may be legitimately optional"
    );

    let unprovable_range = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "fixture", "dependencies": { "react-dom": "^19.1.0" } },
            "node_modules/react": { "version": "18.3.1" },
            "node_modules/react-dom": {
                "version": "19.1.0",
                "peerDependencies": { "react": "next" }
            }
        }
    }"#};
    assert!(
        first_new_peer_violation::<crate::lock::Npm>(None, unprovable_range).is_none(),
        "an unprovable range is ignorance, not proof of exclusion"
    );

    // eslint moved to 10 while a *transitive* plugin's (optional) peer range still names ^9 —
    // the shape every eslint plugin creates.
    // The dependent is not importer-declared, so its
    // recorded range is not authoritative (the pre-apply gate's own directness rule) and the
    // resolver's acceptance stands.
    let transitive_dependent = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "fixture", "dependencies": { "eslint": "^10.0.0" } },
            "node_modules/eslint": { "version": "10.6.0" },
            "node_modules/eslint-plugin-jsdoc": {
                "version": "50.6.1",
                "peerDependencies": { "eslint": "^9.0.0" }
            }
        }
    }"#};
    assert!(
        first_new_peer_violation::<crate::lock::Npm>(None, transitive_dependent).is_none(),
        "a transitive dependent's stale peer range must not veto the accepted move"
    );

    // A direct plugin whose violated peer is *transitive* — `@typescript-eslint/parser`,
    // present only as the plugin's auto-installed peer, lagging behind the plugin's new major.
    // The resolver owns a transitive peer's placement (it accepted this graph and can
    // re-place the copy per context), so its lag must not veto the direct move — only a
    // contract between two importer-declared packages gates.
    let transitive_peer = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "fixture", "dependencies": { "plugin": "^8.0.0" } },
            "node_modules/plugin": {
                "version": "8.0.0",
                "peerDependencies": { "parser": "^8.0.0" }
            },
            "node_modules/parser": { "version": "7.18.0" }
        }
    }"#};
    assert!(
        first_new_peer_violation::<crate::lock::Npm>(None, transitive_peer).is_none(),
        "a lagging transitive peer must not veto the direct move the resolver accepted"
    );

    // The root's own lookup resolves plugin@1 — the physically proven direct copy — so the
    // nested plugin@2's peer range is a transitive contract that must not masquerade as a
    // direct one (the pre-gate's directness rule, shared via `direct_dependent_members`).
    let split_resolved_dependent = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "fixture", "dependencies": { "eslint": "^10.0.0", "plugin": "^1.0.0" } },
            "node_modules/eslint": { "version": "10.6.0" },
            "node_modules/plugin": { "version": "1.0.0" },
            "node_modules/other/node_modules/plugin": {
                "version": "2.0.0",
                "peerDependencies": { "eslint": "^9.0.0" }
            }
        }
    }"#};
    assert!(
        first_new_peer_violation::<crate::lock::Npm>(None, split_resolved_dependent).is_none(),
        "a nested copy is not the direct instance any member's lookup resolves"
    );
}

/// Contextual binding replaces the global-singleton requirement: a peer resolved at several
/// versions is judged per context — npm by the dependent instance's own lookup, pnpm by each
/// importer's declared copy — so a genuine break against the bound copy is proven even while
/// another version exists elsewhere in the graph.
#[test]
fn post_apply_diff_binds_peers_per_context() {
    // npm: the dependent's context binds the root host@1.0.0; an unrelated nested host@0.9.0
    // also exists.
    // The graph-wide split must not suppress the proven break against the copy
    // the dependent actually sees.
    let npm_bound_break = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "fixture", "dependencies": { "plugin": "^3.0.0", "host": "^1.0.0" } },
            "node_modules/plugin": {
                "version": "3.0.0",
                "peerDependencies": { "host": "^2.0.0" }
            },
            "node_modules/host": { "version": "1.0.0" },
            "node_modules/report-tool/node_modules/host": { "version": "0.9.0" }
        }
    }"#};
    let violation = first_new_peer_violation::<crate::lock::Npm>(None, npm_bound_break)
        .expect("the bound root copy provably violates; the nested split must not blind it");
    assert_eq!(
        (violation.package.as_str(), violation.range.as_str()),
        ("host", "^2.0.0")
    );

    // pnpm: importers bind their own declared copies — apps/a's plugin sees apps/a's
    // host@1.0.0 (a proven break) even though apps/b resolves host@2.0.0.
    let pnpm_importer_break = indoc! {"
        lockfileVersion: '9.0'

        importers:

          apps/a:
            dependencies:
              plugin:
                specifier: ^3.0.0
                version: 3.0.0
              host:
                specifier: ^1.0.0
                version: 1.0.0

          apps/b:
            dependencies:
              host:
                specifier: ^2.0.0
                version: 2.0.0

        packages:

          plugin@3.0.0:
            resolution: {integrity: sha512-p3}
            peerDependencies:
              host: ^2.0.0

          host@1.0.0:
            resolution: {integrity: sha512-h1}

          host@2.0.0:
            resolution: {integrity: sha512-h2}
    "};
    let violation = first_new_peer_violation::<crate::lock::Pnpm>(None, pnpm_importer_break)
        .expect("apps/a's own declared copy provably violates despite the cross-importer split");
    assert_eq!(violation.dependent.as_str(), "plugin");
}

/// Peers resolve against the dependent's importing context — and the context is defined by
/// the layout the manager materializes. pnpm isolates importers by declaration, so a package
/// moved in a *disjoint* importer cannot break a dependent that never sees it. npm's default
/// layout *hoists*: the same disjoint declarations meet at the root `node_modules`, so there
/// the contract genuinely binds — and only a physically shadowed dependent (its own nested
/// copy) stays out of reach.
#[test]
fn post_apply_diff_requires_importer_context_overlap() {
    let split_pnpm_importers = indoc! {"
        lockfileVersion: '9.0'

        importers:

          apps/a:
            dependencies:
              plugin:
                specifier: ^1.0.0
                version: 1.0.0

          apps/b:
            dependencies:
              host:
                specifier: ^2.0.0
                version: 2.0.0

        packages:

          plugin@1.0.0:
            peerDependencies:
              host: ^1.0.0

          host@2.0.0:
            resolution: {integrity: sha512-test}
    "};
    assert!(
        first_new_peer_violation::<crate::lock::Pnpm>(None, split_pnpm_importers).is_none(),
        "disjoint pnpm importers keep their own contexts — no contract binds"
    );

    // The same declarations under npm: both packages hoist to the root `node_modules`, so
    // `plugin`'s nearest-ancestor lookup reaches the violating `host` copy no matter which
    // member declared it — declaration disjointness must not suppress the proven break.
    let hoisted_npm_members = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "root" },
            "apps/a": { "name": "a", "dependencies": { "plugin": "^1.0.0" } },
            "apps/b": { "name": "b", "dependencies": { "host": "^2.0.0" } },
            "node_modules/plugin": {
                "version": "1.0.0",
                "peerDependencies": { "host": "^1.0.0" }
            },
            "node_modules/host": { "version": "2.0.0" }
        }
    }"#};
    assert!(
        first_new_peer_violation::<crate::lock::Npm>(None, hoisted_npm_members).is_some(),
        "hoisted npm packages bind across disjoint members — the break is real"
    );

    // Physical isolation is what fails open on npm: host exists only inside apps/b's own
    // subtree, so plugin's ancestor lookup never reaches it and no contract binds.
    let isolated_npm_members = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "root" },
            "apps/a": { "name": "a", "dependencies": { "plugin": "^1.0.0" } },
            "apps/b": { "name": "b", "dependencies": { "host": "^2.0.0" } },
            "node_modules/plugin": {
                "version": "1.0.0",
                "peerDependencies": { "host": "^1.0.0" }
            },
            "apps/b/node_modules/host": { "version": "2.0.0" }
        }
    }"#};
    assert!(
        first_new_peer_violation::<crate::lock::Npm>(None, isolated_npm_members).is_none(),
        "a peer copy the dependent cannot physically reach binds nothing"
    );

    let overlapping_pnpm = indoc! {"
        lockfileVersion: '9.0'

        importers:

          apps/a:
            dependencies:
              plugin:
                specifier: ^1.0.0
                version: 1.0.0
              host:
                specifier: ^2.0.0
                version: 2.0.0

        packages:

          plugin@1.0.0:
            peerDependencies:
              host: ^1.0.0

          host@2.0.0:
            resolution: {integrity: sha512-test}
    "};
    assert!(
        first_new_peer_violation::<crate::lock::Pnpm>(None, overlapping_pnpm).is_some(),
        "the same shape inside one importer is a genuine proven violation"
    );
}

/// Counterfactual attribution: the after-lock proves the *pair* incompatible, not who broke
/// it.
/// A dependent whose range did not change (`^1 || ^2`) is innocent when the peer jumped
/// past it (1→3): the peer's candidate is rejected and the dependent's own move survives —
/// dependent-first guessing would discard the maximal safe subset.
#[test]
fn peer_rejections_attribute_the_causally_culpable_candidate() {
    let before = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "fixture", "dependencies": { "plugin": "^1.0.0", "host": "^1.0.0" } },
            "node_modules/plugin": {
                "version": "1.0.0",
                "peerDependencies": { "host": "^1.0.0 || ^2.0.0" }
            },
            "node_modules/host": { "version": "1.0.0" }
        }
    }"#};
    let after = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "fixture", "dependencies": { "plugin": "^2.0.0", "host": "^3.0.0" } },
            "node_modules/plugin": {
                "version": "2.0.0",
                "peerDependencies": { "host": "^1.0.0 || ^2.0.0" }
            },
            "node_modules/host": { "version": "3.0.0" }
        }
    }"#};
    let baseline = PeerBaseline::gather::<crate::lock::Npm>(Some(before), &[]);
    let current = proven_peer_violations::<crate::lock::Npm>(after, &[]);
    let active = plan_of(vec![
        change("plugin", "1.0.0", "2.0.0"),
        change("host", "1.0.0", "3.0.0"),
    ]);

    let rejections = plan_peer_rejections(&baseline, &current, &active, &HashSet::new())
        .expect("uniquely attributable");
    assert_eq!(rejections.len(), 1, "exactly the culpable candidate");
    assert_eq!(
        rejections.first().map(|rejection| rejection.index),
        Some(1),
        "host (the peer whose jump the unchanged old range provably excludes) is rejected"
    );
    assert_eq!(
        rejections
            .first()
            .map(|rejection| rejection.offending.as_str()),
        Some("plugin"),
        "the rejection blames the contract's other party"
    );

    // The mirror shape: the dependent's NEW range excludes even the old peer — the dependent
    // is independently culpable and the peer's own move survives.
    let dependent_broke = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "fixture", "dependencies": { "plugin": "^2.0.0", "host": "^1.0.0" } },
            "node_modules/plugin": {
                "version": "2.0.0",
                "peerDependencies": { "host": "^9.0.0" }
            },
            "node_modules/host": { "version": "1.0.0" }
        }
    }"#};
    let current = proven_peer_violations::<crate::lock::Npm>(dependent_broke, &[]);
    let rejections = plan_peer_rejections(&baseline, &current, &active, &HashSet::new())
        .expect("uniquely attributable");
    assert_eq!(
        rejections.first().map(|rejection| rejection.index),
        Some(0),
        "plugin (whose new range excludes even the old host) is rejected"
    );

    // Neither side uniquely provable (the new range admits the old peer AND the old range
    // admits the new peer — an interaction only the pair exhibits): rejection would be a
    // guess, so the round aborts for candidate isolation.
    let interaction = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "fixture", "dependencies": { "plugin": "^2.0.0", "host": "^2.0.0" } },
            "node_modules/plugin": {
                "version": "2.0.0",
                "peerDependencies": { "host": "^1.0.0" }
            },
            "node_modules/host": { "version": "2.0.0" }
        }
    }"#};
    let current = proven_peer_violations::<crate::lock::Npm>(interaction, &[]);
    let active = plan_of(vec![
        change("plugin", "1.0.0", "2.0.0"),
        change("host", "1.0.0", "2.0.0"),
    ]);
    assert!(
        plan_peer_rejections(&baseline, &current, &active, &HashSet::new()).is_err(),
        "an interaction violation must go to candidate isolation, not a guess"
    );
}

/// npm's pre-apply gate judges visibility physically: hoisting lets a dependent declared by a
/// *different* member bind the moving copy at the root `node_modules` (declaration
/// disjointness holds nothing back), while a dependent whose own nested copy shadows the
/// moving one is out of reach and must not hold it.
#[test]
fn peer_gate_judges_npm_visibility_physically() {
    let member_ref = |name: &str, path: &str| MemberRef {
        name: name.to_string(),
        path: path.to_string(),
    };
    let hoisted = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "root" },
            "apps/a": { "name": "a", "dependencies": { "plugin": "^1.0.0" } },
            "apps/b": { "name": "b", "dependencies": { "host": "^2.0.0" } },
            "node_modules/plugin": {
                "version": "1.0.0",
                "peerDependencies": { "host": "^1.0.0 || ^2.0.0" }
            },
            "node_modules/host": { "version": "2.0.0" }
        }
    }"#};
    let mut host = change("host", "2.0.0", "3.0.0");
    host.members = vec![member_ref("b", "apps/b")];
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Npm>(&plan_of(vec![host.clone()]), Some(hoisted));
    assert!(
        retained.changes.is_empty(),
        "the hoisted contract binds across disjoint members"
    );
    assert_eq!(
        skipped.first().map(|skip| skip.reason),
        Some(SkipReason::PeerHeld)
    );

    // The dependent's own nested copy shadows the moving root instance: `plugin` never
    // resolves the copy this change rewrites, so nothing binds and the move is free.
    let shadowed = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "root" },
            "apps/a": { "name": "a", "dependencies": { "plugin": "^1.0.0" } },
            "apps/b": { "name": "b", "dependencies": { "host": "^2.0.0" } },
            "node_modules/plugin": {
                "version": "1.0.0",
                "peerDependencies": { "host": "^1.0.0 || ^2.0.0" }
            },
            "node_modules/plugin/node_modules/host": { "version": "1.0.0" },
            "node_modules/host": { "version": "2.0.0" }
        }
    }"#};
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Npm>(&plan_of(vec![host]), Some(shadowed));
    assert_eq!(
        retained.changes.len(),
        1,
        "a physically shadowed dependent holds nothing: {skipped:?}"
    );

    // Directory identity, not version equality: the dependent binds its own nested host@1
    // while the change rewrites apps/b's separate host@1 — same version, different physical
    // copy, so the plugin's copy survives the move untouched and must not hold it.
    let same_version_elsewhere = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "root" },
            "apps/a": { "name": "a", "dependencies": { "plugin": "^1.0.0" } },
            "apps/b": { "name": "b", "dependencies": { "host": "^1.0.0" } },
            "node_modules/plugin": {
                "version": "1.0.0",
                "peerDependencies": { "host": "^1.0.0" }
            },
            "node_modules/plugin/node_modules/host": { "version": "1.0.0" },
            "apps/b/node_modules/host": { "version": "1.0.0" }
        }
    }"#};
    let mut scoped = change("host", "1.0.0", "2.0.0");
    scoped.members = vec![member_ref("b", "apps/b")];
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Npm>(&plan_of(vec![scoped]), Some(same_version_elsewhere));
    assert_eq!(
        retained.changes.len(),
        1,
        "an unrelated same-version copy must not conflate into a hold: {skipped:?}"
    );
}

/// The workspace-manifest source obeys the same layout rule as the lock source: under npm's
/// hoisted tree, the local package's own directory resolves the moving copy no matter which
/// member the change is scoped to, so context disjointness holds nothing back — while pnpm's
/// isolated layout keeps the disjoint change out of the contract's reach.
#[test]
fn workspace_manifest_peer_judges_npm_visibility_physically() {
    let lock = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "root-app" },
            "apps/site": { "name": "site", "dependencies": { "eslint": "^8.40.0" } },
            "packages/shim": { "name": "local-eslint-shim", "version": "0.1.0" },
            "node_modules/local-eslint-shim": { "resolved": "packages/shim", "link": true },
            "node_modules/eslint": { "version": "8.57.0" }
        }
    }"#};
    let install = crate::lock::Npm::install_paths(lock);
    let shim_peer = || WorkspacePeer {
        requirement: crate::lock::PeerRequirement {
            dependent: "local-eslint-shim".to_string(),
            dependent_version: "0.1.0".to_string(),
            package: "eslint".to_string(),
            range: "^8.0.0".to_string(),
        },
        origin: "packages/shim".to_string(),
        contexts: vec!["packages/shim".to_string()],
    };
    let mut eslint = change("eslint", "8.57.0", "9.0.0");
    eslint.members = vec![MemberRef {
        name: "site".to_string(),
        path: "apps/site".to_string(),
    }];

    assert!(
        workspace_peer_hold(&eslint, &[shim_peer()], &HashSet::new(), install.as_ref()).is_some(),
        "the shim's directory resolves the hoisted eslint the disjoint change rewrites"
    );
    assert!(
        workspace_peer_hold(&eslint, &[shim_peer()], &HashSet::new(), None).is_none(),
        "without a physical layout the disjoint contexts keep the contract out of reach"
    );

    // Directory identity again: the shim binds its OWN nested eslint copy while the change
    // rewrites apps/site's separate copy at the same version — the shim's copy survives the
    // move, so nothing may hold.
    let nested_lock = indoc! {r#"{
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "root-app" },
            "apps/site": { "name": "site", "dependencies": { "eslint": "^8.40.0" } },
            "packages/shim": { "name": "local-eslint-shim", "version": "0.1.0" },
            "node_modules/local-eslint-shim": { "resolved": "packages/shim", "link": true },
            "packages/shim/node_modules/eslint": { "version": "8.57.0" },
            "apps/site/node_modules/eslint": { "version": "8.57.0" }
        }
    }"#};
    let nested = crate::lock::Npm::install_paths(nested_lock);
    assert!(
        workspace_peer_hold(&eslint, &[shim_peer()], &HashSet::new(), nested.as_ref()).is_none(),
        "an unrelated same-version copy must not conflate into a workspace-manifest hold"
    );
}

/// One contract shape, several instances: violation identity is per dependent instance, and
/// baseline coverage is per binding member.
/// A split copy floated onto the broken range in a
/// member the baseline never covered is a NEW break; a dependent merely re-recorded at a new
/// patch (same member, same range, contract already broken) introduces nothing.
#[test]
fn post_apply_diff_distinguishes_instances_of_one_contract_shape() {
    let before = split_shape_before();
    // apps/b's plugin floats 2 → 3; the new range is the SAME string apps/a's broken copy
    // already recorded (`^1.0.0`), so a shape-keyed diff would collapse the two instances
    // and grandfather the fresh break.
    let after = split_shape_after();
    let violation = first_new_peer_violation::<crate::lock::Pnpm>(Some(&before), &after)
        .expect("the apps/b float is a new break even under an old shape");
    assert_eq!(
        (
            violation.dependent.as_str(),
            violation.dependent_version.as_str()
        ),
        ("plugin", "3.0.0"),
        "the fresh instance, not apps/a's grandfathered one, is attributed"
    );

    // The counterpart: apps/a's already-broken copy re-recorded at a patch bump stays
    // grandfathered — same member, same range, nothing newly broken.
    let rerecorded = indoc! {"
        lockfileVersion: '9.0'

        importers:

          apps/a:
            dependencies:
              plugin:
                specifier: ^1.0.0
                version: 1.0.1
              host:
                specifier: ^2.0.0
                version: 2.0.0

          apps/b:
            dependencies:
              plugin:
                specifier: ^2.0.0
                version: 2.0.0
              host:
                specifier: ^2.0.0
                version: 2.0.0

        packages:

          plugin@1.0.1:
            resolution: {integrity: sha512-p11}
            peerDependencies:
              host: ^1.0.0

          plugin@2.0.0:
            resolution: {integrity: sha512-p2}
            peerDependencies:
              host: ^1.0.0 || ^2.0.0

          host@2.0.0:
            resolution: {integrity: sha512-h2}
    "};
    assert!(
        first_new_peer_violation::<crate::lock::Pnpm>(Some(&before), rerecorded).is_none(),
        "a re-recorded instance of an already-broken contract is not re-attributed"
    );
}

/// Culprit matching is instance-aware.
/// The rejected candidate must have LANDED the violating
/// instance — same name, exact landed version, overlapping member — so a same-named change in
/// another member is never blamed for it; and a multi-version name, which the whole-graph
/// resolve deliberately never pins ([`prepare_whole_graph_inputs`]), is no candidate at all —
/// implicating it aborts to candidate isolation instead of uselessly rejecting an unpinned
/// change.
#[test]
fn peer_rejections_match_the_landed_instance_not_the_name() {
    let member_ref = |name: &str, path: &str| MemberRef {
        name: name.to_string(),
        path: path.to_string(),
    };
    let before = split_shape_before();
    let after = split_shape_after();
    let baseline = PeerBaseline::gather::<crate::lock::Pnpm>(Some(&before), &[]);
    let current = proven_peer_violations::<crate::lock::Pnpm>(&after, &[]);

    // Two same-named candidates: only the apps/b change landed the violating 3.0.0 instance.
    let mut decoy = change("plugin", "1.0.0", "1.5.0");
    decoy.members = vec![member_ref("a", "apps/a")];
    let mut mover = change("plugin", "2.0.0", "3.0.0");
    mover.members = vec![member_ref("b", "apps/b")];
    let active = plan_of(vec![decoy, mover]);
    let rejections = plan_peer_rejections(&baseline, &current, &active, &HashSet::new())
        .expect("uniquely attributable");
    assert_eq!(
        rejections.first().map(|rejection| rejection.index),
        Some(1),
        "the landed instance's own change is rejected, never the same-named decoy"
    );
    assert_eq!(rejections.len(), 1);
    assert_eq!(
        rejections
            .first()
            .map(|rejection| rejection.offending.as_str()),
        Some("host")
    );

    // The same violation with `plugin` multi-version — the resolve never pinned it, its
    // float is resolver latitude, and the peer did not move either: nobody is uniquely
    // culpable, so the round aborts to candidate isolation.
    let multi: HashSet<String> = std::iter::once("plugin".to_string()).collect();
    assert!(
        plan_peer_rejections(&baseline, &current, &active, &multi).is_err(),
        "an unpinned multi-version name must not be rejected as a candidate"
    );
}

/// The pnpm lock behind the split-instance tests: apps/a's `plugin@1` already violates
/// against the shared `host@2` (grandfathered), while apps/b's `plugin@2` range still admits
/// it.
fn split_shape_before() -> String {
    indoc! {"
        lockfileVersion: '9.0'

        importers:

          apps/a:
            dependencies:
              plugin:
                specifier: ^1.0.0
                version: 1.0.0
              host:
                specifier: ^2.0.0
                version: 2.0.0

          apps/b:
            dependencies:
              plugin:
                specifier: ^2.0.0
                version: 2.0.0
              host:
                specifier: ^2.0.0
                version: 2.0.0

        packages:

          plugin@1.0.0:
            resolution: {integrity: sha512-p1}
            peerDependencies:
              host: ^1.0.0

          plugin@2.0.0:
            resolution: {integrity: sha512-p2}
            peerDependencies:
              host: ^1.0.0 || ^2.0.0

          host@2.0.0:
            resolution: {integrity: sha512-h2}
    "}
    .to_string()
}

/// [`split_shape_before`] after apps/b's plugin floated to 3.0.0, whose range re-records the
/// same `^1.0.0` string apps/a's copy already carries.
fn split_shape_after() -> String {
    indoc! {"
        lockfileVersion: '9.0'

        importers:

          apps/a:
            dependencies:
              plugin:
                specifier: ^1.0.0
                version: 1.0.0
              host:
                specifier: ^2.0.0
                version: 2.0.0

          apps/b:
            dependencies:
              plugin:
                specifier: ^3.0.0
                version: 3.0.0
              host:
                specifier: ^2.0.0
                version: 2.0.0

        packages:

          plugin@1.0.0:
            resolution: {integrity: sha512-p1}
            peerDependencies:
              host: ^1.0.0

          plugin@3.0.0:
            resolution: {integrity: sha512-p3}
            peerDependencies:
              host: ^1.0.0

          host@2.0.0:
            resolution: {integrity: sha512-h2}
    "}
    .to_string()
}

/// The moving-dependent exemption is recomputed to a fixed point: a dependent whose own move is
/// peer-held stays in place, so it stops exempting the package it pins — the plan cannot leak a
/// break through a co-move that never happens.
#[test]
fn peer_gate_recomputes_when_the_exempting_dependent_is_itself_held() {
    let lock = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              fumadocs-core:
                specifier: ^16.0.0
                version: 16.11.4
              fumadocs-mdx:
                specifier: ^15.0.0
                version: 15.1.1(fumadocs-core@16.11.4)
              docs-kit:
                specifier: ^1.0.0
                version: 1.0.0(fumadocs-mdx@15.1.1)

        packages:

          fumadocs-core@16.11.4:
            resolution: {integrity: sha512-aaa}

          fumadocs-mdx@15.1.1(fumadocs-core@16.11.4):
            resolution: {integrity: sha512-bbb}
            peerDependencies:
              fumadocs-core: ^16.0.0

          docs-kit@1.0.0(fumadocs-mdx@15.1.1):
            resolution: {integrity: sha512-ccc}
            peerDependencies:
              fumadocs-mdx: ^15.0.0
    "};

    // mdx's own move is held by docs-kit's peer range, so mdx stays at 15.1.1 — and the second
    // round therefore holds core, which round one had exempted for mdx's planned co-move.
    let plan = plan_of(vec![
        change("fumadocs-core", "16.11.4", "17.0.0"),
        change("fumadocs-mdx", "15.1.1", "16.0.0"),
    ]);
    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&plan, Some(lock));
    assert!(retained.changes.is_empty());
    let blame_for = |name: &str| {
        skipped
            .iter()
            .find(|held| held.change.package.name == name)
            .and_then(|held| held.offending.as_ref())
            .map(|package| package.name.as_str())
    };
    assert_eq!(blame_for("fumadocs-mdx"), Some("docs-kit"));
    assert_eq!(blame_for("fumadocs-core"), Some("fumadocs-mdx"));
}

/// A same-name move in a *disjoint* importer never exempts: the held copy's importer keeps its
/// version, so the peer range still binds there. (The moving copy here is also a
/// multi-version-declared name, which the resolve later skips as `MultiVersionHeld` — one more
/// reason it must not count as moving.)
#[test]
fn peer_gate_ignores_a_same_name_move_in_a_disjoint_importer() {
    let lock = indoc! {"
        lockfileVersion: '9.0'

        importers:

          apps/site:
            dependencies:
              fumadocs-mdx:
                specifier: ^16.0.0
                version: 16.0.0

          apps/docs:
            dependencies:
              fumadocs-core:
                specifier: ^16.0.0
                version: 16.11.4
              fumadocs-mdx:
                specifier: ~15.1.0
                version: 15.1.1(fumadocs-core@16.11.4)

        packages:

          fumadocs-core@16.11.4:
            resolution: {integrity: sha512-aaa}

          fumadocs-mdx@15.1.1(fumadocs-core@16.11.4):
            resolution: {integrity: sha512-bbb}
            peerDependencies:
              fumadocs-core: ^16.0.0

          fumadocs-mdx@16.0.0:
            resolution: {integrity: sha512-ccc}
    "};
    let member = |path: &str| MemberRef {
        name: path.to_string(),
        path: path.to_string(),
    };

    let mut site_mdx = change("fumadocs-mdx", "16.0.0", "16.2.0");
    site_mdx.members = vec![member("apps/site")];
    let mut docs_core = change("fumadocs-core", "16.11.4", "17.0.0");
    docs_core.members = vec![member("apps/docs")];

    let PeerPartition { retained, skipped } =
        peer_partition::<crate::lock::Pnpm>(&plan_of(vec![docs_core, site_mdx]), Some(lock));
    // The mdx move in apps/site cannot lift apps/docs's mdx@15.1.1 peer pin on core.
    assert_eq!(retained.changes.len(), 1, "only the mdx move survives");
    assert_eq!(
        skipped
            .first()
            .and_then(|held| held.offending.as_ref())
            .map(|package| package.name.as_str()),
        Some("fumadocs-mdx")
    );
    assert_eq!(
        skipped
            .first()
            .map(|held| held.change.package.name.as_str()),
        Some("fumadocs-core")
    );
}

#[test]
fn peer_conflict_blocker_names_a_unique_peer_suffixed_sibling() {
    // `pkg-b` carries a `(shared@1.4.0)` peer suffix, so its identity depends on the peer choice
    // that excluded the held `pkg-a`.
    // With a single such sibling, blame is unambiguous and `pkg-b` is named.
    let lock = "lockfileVersion: '9.0'\n\npackages:\n\n  pkg-a@1.0.0:\n    resolution: {integrity: sha512-a}\n\n  pkg-b@2.0.0(shared@1.4.0):\n    resolution: {integrity: sha512-b}\n\n  shared@1.4.0:\n    resolution: {integrity: sha512-c}\n";
    assert_eq!(
        peer_conflict_blocker(lock, "pkg-a"),
        Some("pkg-b".to_string())
    );
    // The held package's own peer-suffixed key never blames itself.
    let self_only = "lockfileVersion: '9.0'\n\npackages:\n\n  pkg-a@1.0.0(shared@2.0.0):\n    resolution: {integrity: sha512-a}\n";
    assert_eq!(peer_conflict_blocker(self_only, "pkg-a"), None);
}

/// lockfileVersion 9 (pnpm 9-11) writes suffix-free `packages:` keys and keeps the peer-resolved
/// identities under `snapshots:` — the section the blame scan must read on a modern lock, where a
/// `packages:`-only scan finds nothing and every held conflict would self-blame.
#[test]
fn peer_conflict_blocker_reads_v9_snapshot_identities() {
    let lock = indoc::indoc! {"
        lockfileVersion: '9.0'

        packages:

          pkg-a@1.0.0:
            resolution: {integrity: sha512-a}

          pkg-b@2.0.0:
            resolution: {integrity: sha512-b}

          shared@1.4.0:
            resolution: {integrity: sha512-c}

        snapshots:

          pkg-a@1.0.0: {}

          pkg-b@2.0.0(shared@1.4.0):
            dependencies:
              shared: 1.4.0

          shared@1.4.0: {}
    "};
    assert_eq!(
        peer_conflict_blocker(lock, "pkg-a"),
        Some("pkg-b".to_string())
    );
}

#[test]
fn peer_conflict_blocker_is_generic_when_blame_is_ambiguous() {
    // Two distinct peer-suffixed siblings make blame ambiguous, so attribution stays generic.
    let lock = "lockfileVersion: '9.0'\n\npackages:\n\n  pkg-b@2.0.0(shared@1.0.0):\n    resolution: {integrity: sha512-b}\n\n  pkg-c@2.0.0(shared@1.0.0):\n    resolution: {integrity: sha512-c}\n";
    assert_eq!(peer_conflict_blocker(lock, "pkg-a"), None);
}
