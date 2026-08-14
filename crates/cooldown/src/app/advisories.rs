//! The application side of the advisory feed: one batched fetch per project, fail-open
//! plumbing, and per-dependency classification into the core's [`AdvisoryContext`].
//!
//! One project resolves against one snapshot.
//! It is fetched once before planning and only ever *extended* — never re-fetched — so a
//! candidate cannot be adopted on one round's evidence and rolled back on another's.
//!
//! Failure semantics are the inverse of the registry's: a registry outage can make a fresh
//! version look *mature*, so `check` fails closed on it; an advisory outage can only fail to
//! *shorten* a window — the ordinary, stricter window stands — so the feed fails open with a
//! loud warning (`--fail-on-advisory-source` escalates it for gates that insist).
//! Stale cached advisory data may still annotate but never shortens: the shorten mode degrades
//! to flag for that project.

use super::{ProjectCtx, RunOpts, Workspace};
use cooldown_core::{
    Advisory, AdvisoryContext, AdvisoryMode, AdvisorySourceId, AdvisorySourceKind, Dependency,
    Diagnostic, DiagnosticKind, RawAdvisory, Release, ResolveKind, ResolveQuery,
    ResolvedAdvisoryPolicy, ToolRead, classify_advisory, resolve, resolve_advisory_policy,
};
use std::collections::HashMap;

/// One project's fetched advisories plus its resolved `[advisories]` policy — everything the
/// per-dependency classification needs.
///
/// The upgrade executor shares one fetch across its fixpoint rounds behind an `Arc`, so a
/// round's planning borrows the map rather than copying it.
/// Only a top-up copies it, to add the keys a re-lock made necessary (see
/// [`extended`](Self::extended)).
pub(crate) struct ProjectAdvisories {
    by_package: HashMap<String, Vec<RawAdvisory>>,
    /// The advisory-package identities the feed was actually asked about.
    ///
    /// Separate from [`by_package`](Self::by_package), which only holds packages that *have*
    /// advisories: without this, "queried and clean" and "never queried" look identical, and a
    /// top-up could not tell which identities still need one.
    queried: std::collections::HashSet<String>,
    /// The source that served the fetch, stamped onto every classified advisory.
    source: AdvisorySourceId,
    policy: ResolvedAdvisoryPolicy,
}

/// One dependency's classified advisories, borrowing the project policy — everything an
/// [`AdvisoryContext`] needs, bundled so call sites stay two lines.
pub(crate) struct ClassifiedAdvisories<'p> {
    advisories: Vec<Advisory>,
    policy: &'p ResolvedAdvisoryPolicy,
}

impl ClassifiedAdvisories<'_> {
    /// The advisory context the advised evaluation functions borrow.
    pub(crate) fn context(&self) -> AdvisoryContext<'_> {
        AdvisoryContext {
            advisories: &self.advisories,
            policy: self.policy,
        }
    }
}

impl ProjectAdvisories {
    /// Classifies `dep`'s advisories against its fetched `releases`.
    ///
    /// `None` when no advisory names this package — evaluation then runs exactly unadvised.
    ///
    /// `releases` may be empty (the `check` path fetches only the locked release); range
    /// boundaries are then unorderable and only the exact-match tests (enumerated versions, fix
    /// versions) remain — precisely what the pin-side gate needs.
    pub(crate) fn classify(
        &self,
        adapter: &dyn ToolRead,
        dep: &Dependency,
        releases: &[Release],
    ) -> Option<ClassifiedAdvisories<'_>> {
        let raws = self
            .by_package
            .get(&adapter.advisory_package(&dep.package.name))?;
        let normalize =
            |version: &str| cooldown_core::Version::new(adapter.advisory_version(version));
        Some(ClassifiedAdvisories {
            advisories: raws
                .iter()
                .map(|raw| classify_advisory(raw, self.source, releases, &normalize))
                .collect(),
            policy: &self.policy,
        })
    }

    /// The advisory-package identities among `names` this fetch never queried, deduplicated.
    pub(crate) fn unqueried(&self, adapter: &dyn ToolRead, names: &[String]) -> Vec<String> {
        let mut packages: Vec<String> = names
            .iter()
            .map(|name| adapter.advisory_package(name))
            .filter(|package| !self.queried.contains(package))
            .collect();
        packages.sort();
        packages.dedup();
        packages
    }

    /// This snapshot extended with `fetch`'s advisories for `queried`.
    ///
    /// Additive by construction: entries already present are copied verbatim, so evidence the
    /// planner has already acted on cannot change under it.
    fn extended(&self, queried: &[String], fetch: cooldown_core::AdvisoryFetch) -> Self {
        let mut by_package = self.by_package.clone();
        for package in fetch.packages {
            if self.queried.contains(&package.package) {
                continue;
            }
            by_package
                .entry(package.package)
                .or_default()
                .extend(package.advisories);
        }
        let mut already = self.queried.clone();
        already.extend(queried.iter().cloned());
        ProjectAdvisories {
            by_package,
            queried: already,
            source: self.source,
            policy: self.policy.clone(),
        }
    }
}

/// The outcome of one project's advisory fetch: the data (when the feed is enabled, supported,
/// and reachable) plus any diagnostics to surface.
pub(crate) struct AdvisoryFetchOutcome {
    pub(crate) advisories: Option<ProjectAdvisories>,
    /// Non-fatal diagnostics: an unsupported tool, an unreachable feed (fail-open), a
    /// stale-cache shorten degradation, or a never-shortening security window.
    pub(crate) warnings: Vec<Diagnostic>,
    /// Fatal diagnostics: every way the enabled feed yielded no usable evidence —
    /// unreachable, unimplemented, or too stale to shorten — once `--fail-on-advisory-source`
    /// insists on it.
    pub(crate) errors: Vec<Diagnostic>,
}

impl AdvisoryFetchOutcome {
    fn inert() -> Self {
        AdvisoryFetchOutcome {
            advisories: None,
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// No usable advisory data: fail open on the window (the ordinary, stricter one stands) and
    /// loud in the output — unless the gate insisted with `--fail-on-advisory-source`.
    fn fail_open(diagnostic: Diagnostic, insisted: bool) -> Self {
        let (warnings, errors) = fail_open_split(diagnostic, insisted);
        AdvisoryFetchOutcome {
            advisories: None,
            warnings,
            errors,
        }
    }
}

/// Routes one fail-open diagnostic to the warnings or, when the gate insisted with
/// `--fail-on-advisory-source`, to the errors.
fn fail_open_split(diagnostic: Diagnostic, insisted: bool) -> (Vec<Diagnostic>, Vec<Diagnostic>) {
    if insisted {
        (Vec::new(), vec![diagnostic])
    } else {
        (vec![diagnostic], Vec::new())
    }
}

/// The outcome of extending an existing fetch: the widened snapshot (absent when nothing usable
/// came back) plus any diagnostics.
pub(crate) struct AdvisoryTopUp {
    pub(crate) advisories: Option<ProjectAdvisories>,
    pub(crate) warnings: Vec<Diagnostic>,
    pub(crate) errors: Vec<Diagnostic>,
}

impl AdvisoryTopUp {
    /// Nothing to add: the snapshot stands as it is.
    fn inert() -> Self {
        AdvisoryTopUp {
            advisories: None,
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// No usable data for the new packages: they stay unadvised, which can only fail to shorten
    /// their windows — reported like any other unusable feed.
    fn fail_open(diagnostic: Diagnostic, insisted: bool) -> Self {
        let (warnings, errors) = fail_open_split(diagnostic, insisted);
        AdvisoryTopUp {
            advisories: None,
            warnings,
            errors,
        }
    }
}

/// Whether the wired source implements the feed the policy selected.
fn source_kind_matches(kind: AdvisorySourceKind, id: AdvisorySourceId) -> bool {
    match kind {
        AdvisorySourceKind::Osv => id == cooldown_registry::OSV_SOURCE_ID,
        // `github` is parsed for forward compatibility but has no adapter yet, and `none` never
        // reaches this point.
        AdvisorySourceKind::Github | AdvisorySourceKind::None => false,
    }
}

/// The `[advisories] source` token, for diagnostics.
fn source_token(kind: AdvisorySourceKind) -> &'static str {
    match kind {
        AdvisorySourceKind::Osv => "osv",
        AdvisorySourceKind::Github => "github",
        AdvisorySourceKind::None => "none",
    }
}

impl Workspace {
    /// Fetches the advisories for one project's dependency set — batched, never per dependency —
    /// when the project's `[advisories]` policy enables it and the tool has an
    /// advisory-database ecosystem.
    ///
    /// Inert (`advisories: None`, no diagnostics) only when the policy asks for nothing: the
    /// feed disabled, `source = "none"`, or no packages to ask about.
    /// Every other way the fetch can come up empty — no wired source implements the selected
    /// `source`, no advisory ecosystem covers the tool, the feed is unreachable — is reported,
    /// fail-open, so an enabled feed never certifies a run silently.
    pub(crate) async fn fetch_project_advisories(
        &self,
        adapter: &dyn ToolRead,
        pctx: &ProjectCtx,
        project_label: &str,
        package_names: &[String],
        opts: &RunOpts,
    ) -> AdvisoryFetchOutcome {
        let mut policy = resolve_advisory_policy(&pctx.policy.layers);
        if !policy.enabled || policy.source == AdvisorySourceKind::None || package_names.is_empty()
        {
            return AdvisoryFetchOutcome::inert();
        }
        // The policy selects a feed the wired source must actually implement.
        // A `Workspace` built outside the CLI can pair a `github` policy with the OSV source,
        // and a workspace with no source at all would otherwise certify silently without
        // consulting any feed.
        let wired = self
            .advisory_source
            .as_ref()
            .filter(|source| source_kind_matches(policy.source, source.id()));
        let Some(source) = wired else {
            let diagnostic = Diagnostic::new(
                DiagnosticKind::AdvisorySourceUnavailable,
                format!(
                    "no advisory source implements `{}`; windows stay un-shortened",
                    source_token(policy.source)
                ),
            )
            .with_tool(pctx.tool.as_str())
            .with_project(project_label);
            return AdvisoryFetchOutcome::fail_open(diagnostic, opts.fail_on_advisory_source);
        };
        let mut warnings = Vec::new();
        let Some(ecosystem) = adapter.capabilities().advisory_ecosystem else {
            warnings.push(unsupported_ecosystem_warning(pctx, project_label));
            return AdvisoryFetchOutcome {
                advisories: None,
                warnings,
                errors: Vec::new(),
            };
        };

        let mut packages: Vec<String> = package_names
            .iter()
            .map(|name| adapter.advisory_package(name))
            .collect();
        packages.sort();
        packages.dedup();

        opts.progress.phase(format!(
            "querying {} advisories for {} packages",
            source.id(),
            packages.len()
        ));
        let fetch = match source.advisories(ecosystem, &packages).await {
            Ok(fetch) => fetch,
            Err(error) => {
                let diagnostic = Diagnostic::new(
                    DiagnosticKind::AdvisorySourceUnavailable,
                    format!(
                        "advisory source `{}` is unavailable ({error}); windows stay un-shortened",
                        source.id()
                    ),
                )
                .with_tool(pctx.tool.as_str())
                .with_project(project_label);
                let mut outcome =
                    AdvisoryFetchOutcome::fail_open(diagnostic, opts.fail_on_advisory_source);
                outcome.warnings.splice(..0, warnings);
                return outcome;
            }
        };

        // The feed is a loosening input, so it may only shorten while *fresh*: stale cached
        // data still annotates, never shortens.
        // That degradation withholds exactly the evidence `--fail-on-advisory-source` insists
        // on, so the gate escalates it like an outage.
        let mut errors = Vec::new();
        if fetch.stale && policy.mode == AdvisoryMode::Shorten {
            let diagnostic = Diagnostic::new(
                DiagnosticKind::AdvisorySourceUnavailable,
                "advisory data was served from a stale cache; annotating only — the security window is not applied",
            )
            .with_tool(pctx.tool.as_str())
            .with_project(project_label);
            if opts.fail_on_advisory_source {
                errors.push(diagnostic);
            } else {
                warnings.push(diagnostic);
            }
            policy.mode = AdvisoryMode::Flag;
        }
        if policy.mode == AdvisoryMode::Shorten
            && let Some(warning) = self.never_shortens_warning(pctx, project_label, &policy)
        {
            warnings.push(warning);
        }

        let mut by_package: HashMap<String, Vec<RawAdvisory>> = HashMap::new();
        for package in fetch.packages {
            by_package
                .entry(package.package)
                .or_default()
                .extend(package.advisories);
        }
        AdvisoryFetchOutcome {
            advisories: Some(ProjectAdvisories {
                by_package,
                queried: packages.into_iter().collect(),
                source: source.id(),
                policy,
            }),
            warnings,
            errors,
        }
    }

    /// Extends `existing` to cover `packages` — advisory-package identities it never queried,
    /// as [`ProjectAdvisories::unqueried`] reports them.
    ///
    /// The policy (including a shorten mode already degraded by stale data) is carried over
    /// unchanged: this is one project's *one* advisory snapshot growing, not a second fetch with
    /// its own rules.
    /// A failed top-up is fail-open like the initial fetch — the new packages stay unadvised,
    /// which can only fail to shorten their windows.
    pub(crate) async fn top_up_project_advisories(
        &self,
        adapter: &dyn ToolRead,
        pctx: &ProjectCtx,
        project_label: &str,
        existing: &ProjectAdvisories,
        packages: &[String],
        opts: &RunOpts,
    ) -> AdvisoryTopUp {
        let (Some(source), Some(ecosystem)) = (
            self.advisory_source.as_ref(),
            adapter.capabilities().advisory_ecosystem,
        ) else {
            // Neither can have changed since the fetch that produced `existing` succeeded.
            return AdvisoryTopUp::inert();
        };
        opts.progress.phase(format!(
            "querying {} advisories for {} newly resolved packages",
            source.id(),
            packages.len()
        ));
        let fetch = match source.advisories(ecosystem, packages).await {
            Ok(fetch) => fetch,
            Err(error) => {
                let diagnostic = Diagnostic::new(
                    DiagnosticKind::AdvisorySourceUnavailable,
                    format!(
                        "advisory source `{}` is unavailable ({error}); {} newly resolved packages stay un-annotated",
                        source.id(),
                        packages.len()
                    ),
                )
                .with_tool(pctx.tool.as_str())
                .with_project(project_label);
                return AdvisoryTopUp::fail_open(diagnostic, opts.fail_on_advisory_source);
            }
        };
        // Stale data may annotate but never shorten, and this project's shorten mode is already
        // settled — the earlier rounds resolved under it — so stale top-up data is dropped
        // rather than allowed to shorten a window the stale rule forbids.
        if fetch.stale && existing.policy.mode == AdvisoryMode::Shorten {
            let diagnostic = Diagnostic::new(
                DiagnosticKind::AdvisorySourceUnavailable,
                format!(
                    "advisory data for {} newly resolved packages was served from a stale cache; they stay un-annotated",
                    packages.len()
                ),
            )
            .with_tool(pctx.tool.as_str())
            .with_project(project_label);
            return AdvisoryTopUp::fail_open(diagnostic, opts.fail_on_advisory_source);
        }
        AdvisoryTopUp {
            advisories: Some(existing.extended(packages, fetch)),
            ..AdvisoryTopUp::inert()
        }
    }

    /// A security window at or above the project's *default* window cannot shorten it — holding
    /// a fix longer than a routine bump is never what anyone meant — so say so once.
    ///
    /// Scoped to the project default deliberately: a package or tool rule with a longer window
    /// can still be shortened, so the diagnostic names the default rather than claiming the
    /// security window never applies anywhere.
    /// Probing every dependency's resolved window here would mean resolving the whole graph
    /// before the feed is even queried.
    fn never_shortens_warning(
        &self,
        pctx: &ProjectCtx,
        project_label: &str,
        policy: &ResolvedAdvisoryPolicy,
    ) -> Option<Diagnostic> {
        let query = ResolveQuery {
            tool: pctx.tool,
            package: "",
            registry: None,
            project: &pctx.rel_path,
            kind: ResolveKind::EffectiveDefault,
        };
        let default_window = resolve(&pctx.policy.layers, &query, self.now()).window;
        // Probe the same mechanism the evaluation applies, so every never-shortens shape is
        // caught — including a min-age below the bare window that an unbypassed floor clamps
        // right back to the ordinary cutoff.
        // The probe id is only a placeholder `shortened_by`.
        let probe = cooldown_core::AdvisoryId("probe".to_string());
        let never_shortens =
            cooldown_core::apply_security_window(&default_window, policy, &probe, self.now())
                .is_none();
        never_shortens.then(|| {
            Diagnostic::new(
                DiagnosticKind::Config,
                format!(
                    "advisories.min-age ({:.0}d) cannot undercut the project's default window ({:.0}d, floor included); it does not shorten the default (a longer package or tool rule can still be shortened)",
                    cooldown_core::duration::duration_as_days(policy.min_age),
                    default_window.effective_min_age_days(self.now()),
                ),
            )
            .with_tool(pctx.tool.as_str())
            .with_project(project_label)
        })
    }
}

fn unsupported_ecosystem_warning(pctx: &ProjectCtx, project_label: &str) -> Diagnostic {
    Diagnostic::new(
        DiagnosticKind::AdvisoryEcosystemUnsupported,
        format!(
            "no advisory-database ecosystem covers tool `{}`; its packages are not annotated",
            pctx.tool.as_str()
        ),
    )
    .with_tool(pctx.tool.as_str())
    .with_project(project_label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AdapterSet, Baseline, ProjectCtx, Workspace};
    use async_trait::async_trait;
    use camino::Utf8PathBuf;
    use cooldown_core::config::builtin_default_layer;
    use cooldown_core::{
        AdvisoryFetch, AdvisoryPolicy, AdvisorySource, AdvisorySourceId, Capabilities, CoreError,
        DepScope, LockStatus, LockVerifyReport, NativePolicyLayer, PackageAdvisories, PolicyLayer,
        PolicyStack, Project, ProjectMarker, ToolId,
    };
    use std::sync::Arc;

    const CARGO: ToolId = ToolId("cargo");

    struct FakeReader {
        advisory_ecosystem: Option<&'static str>,
    }

    #[async_trait]
    impl ToolRead for FakeReader {
        fn id(&self) -> ToolId {
            CARGO
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                advisory_ecosystem: self.advisory_ecosystem,
                ..Capabilities::default()
            }
        }

        fn project_detection(&self) -> cooldown_core::ProjectDetection {
            cooldown_core::ProjectDetection::Primary(ProjectMarker {
                lockfile: "lock",
                manifest: "manifest",
                alternate_manifests: &[],
                workspace_root: true,
            })
        }

        async fn dependencies(
            &self,
            _project: &Project,
            _scope: DepScope,
        ) -> cooldown_core::Result<Vec<Dependency>> {
            Ok(Vec::new())
        }

        async fn native_policy(
            &self,
            _project: &Project,
        ) -> cooldown_core::Result<Option<NativePolicyLayer>> {
            Ok(None)
        }

        async fn verify_lock_current(
            &self,
            _project: &Project,
        ) -> cooldown_core::Result<LockVerifyReport> {
            Ok(LockVerifyReport {
                status: LockStatus::Current,
                detail: "current".to_string(),
            })
        }
    }

    enum FakeFeed {
        Ok {
            stale: bool,
        },
        /// Reports one advisory per queried package, named after it.
        Advising,
        Unreachable,
    }

    #[async_trait]
    impl AdvisorySource for FakeFeed {
        fn id(&self) -> AdvisorySourceId {
            AdvisorySourceId("osv")
        }

        async fn advisories(
            &self,
            _ecosystem: &str,
            packages: &[String],
        ) -> cooldown_core::Result<AdvisoryFetch> {
            match self {
                FakeFeed::Ok { stale } => Ok(AdvisoryFetch {
                    packages: packages
                        .iter()
                        .map(|package| PackageAdvisories {
                            package: package.clone(),
                            advisories: Vec::new(),
                        })
                        .collect(),
                    stale: *stale,
                }),
                FakeFeed::Advising => Ok(AdvisoryFetch {
                    packages: packages
                        .iter()
                        .map(|package| PackageAdvisories {
                            package: package.clone(),
                            advisories: vec![RawAdvisory {
                                id: format!("GHSA-{package}"),
                                aliases: Vec::new(),
                                severity: cooldown_core::AdvisorySeverity::High,
                                withdrawn: false,
                                summary: String::new(),
                                ranges: Vec::new(),
                                affected_versions: Vec::new(),
                                fixes: vec!["1.0.1".to_string()],
                            }],
                        })
                        .collect(),
                    stale: false,
                }),
                FakeFeed::Unreachable => {
                    Err(CoreError::transient("connection refused".to_string()))
                }
            }
        }
    }

    fn project_ctx(advisories: Option<AdvisoryPolicy>) -> ProjectCtx {
        let root = Utf8PathBuf::from("/repo");
        let mut layer = PolicyLayer::new(cooldown_core::Origin::Repo("cooldown.toml".into()));
        layer.advisories = advisories;
        ProjectCtx {
            tool: CARGO,
            rel_path: Utf8PathBuf::from("."),
            project: Project {
                root: root.clone(),
                kind: CARGO,
                manifest: root.join("manifest"),
                exclude_newer: None,
            },
            policy: PolicyStack {
                layers: vec![builtin_default_layer(), layer],
                strict_native: false,
            },
            edge_policy: cooldown_core::EdgePolicy::default(),
        }
    }

    fn workspace(feed: FakeFeed) -> Workspace {
        Workspace::new(
            AdapterSet::new(),
            Vec::new(),
            "2026-06-17T00:00:00Z".parse().expect("timestamp"),
            Baseline::default(),
            Utf8PathBuf::from("/repo"),
            vec![builtin_default_layer()],
        )
        .with_advisory_source(Arc::new(feed))
    }

    fn enabled_policy(mode: Option<cooldown_core::AdvisoryMode>) -> AdvisoryPolicy {
        AdvisoryPolicy {
            enabled: Some(true),
            mode,
            ..AdvisoryPolicy::default()
        }
    }

    #[tokio::test]
    async fn disabled_policy_is_inert() {
        let ws = workspace(FakeFeed::Unreachable);
        let reader = FakeReader {
            advisory_ecosystem: Some("crates.io"),
        };
        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &project_ctx(None),
                ".",
                &["serde".to_string()],
                &RunOpts::default(),
            )
            .await;
        assert!(outcome.advisories.is_none());
        assert!(outcome.warnings.is_empty());
        assert!(outcome.errors.is_empty());
    }

    /// The fail-open contract: an unreachable feed is a warning and un-shortened windows, never
    /// a changed verdict — unless the gate insists via `--fail-on-advisory-source`.
    #[tokio::test]
    async fn unreachable_feed_fails_open_unless_the_gate_insists() {
        let ws = workspace(FakeFeed::Unreachable);
        let reader = FakeReader {
            advisory_ecosystem: Some("crates.io"),
        };
        let pctx = project_ctx(Some(enabled_policy(None)));

        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &pctx,
                ".",
                &["serde".to_string()],
                &RunOpts::default(),
            )
            .await;
        assert!(outcome.advisories.is_none());
        assert!(outcome.errors.is_empty(), "fail-open by default");
        assert_eq!(outcome.warnings.len(), 1);
        assert_eq!(
            outcome.warnings[0].kind,
            DiagnosticKind::AdvisorySourceUnavailable
        );

        let opts = RunOpts {
            fail_on_advisory_source: true,
            ..RunOpts::default()
        };
        let outcome = ws
            .fetch_project_advisories(&reader, &pctx, ".", &["serde".to_string()], &opts)
            .await;
        assert!(outcome.advisories.is_none());
        assert!(outcome.warnings.is_empty());
        assert_eq!(outcome.errors.len(), 1, "the gate refuses to certify");
        assert_eq!(
            outcome.errors[0].kind,
            DiagnosticKind::AdvisorySourceUnavailable
        );
    }

    /// A tool with no advisory-database ecosystem is reported, not silently skipped.
    #[tokio::test]
    async fn unsupported_ecosystem_warns_once() {
        let ws = workspace(FakeFeed::Ok { stale: false });
        let reader = FakeReader {
            advisory_ecosystem: None,
        };
        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &project_ctx(Some(enabled_policy(None))),
                ".",
                &["conda-thing".to_string()],
                &RunOpts::default(),
            )
            .await;
        assert!(outcome.advisories.is_none());
        assert_eq!(outcome.warnings.len(), 1);
        assert_eq!(
            outcome.warnings[0].kind,
            DiagnosticKind::AdvisoryEcosystemUnsupported
        );
    }

    /// Stale advisory data may annotate but never shorten: the shorten mode degrades to flag
    /// with a warning saying so.
    #[tokio::test]
    async fn stale_feed_degrades_shorten_to_flag() {
        let ws = workspace(FakeFeed::Ok { stale: true });
        let reader = FakeReader {
            advisory_ecosystem: Some("crates.io"),
        };
        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &project_ctx(Some(enabled_policy(Some(
                    cooldown_core::AdvisoryMode::Shorten,
                )))),
                ".",
                &["serde".to_string()],
                &RunOpts::default(),
            )
            .await;
        let advisories = outcome.advisories.expect("fetched");
        assert_eq!(advisories.policy.mode, cooldown_core::AdvisoryMode::Flag);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.message.contains("stale cache")),
            "the degradation is loud: {:?}",
            outcome.warnings
        );

        // Fresh data keeps the shorten mode.
        let ws = workspace(FakeFeed::Ok { stale: false });
        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &project_ctx(Some(enabled_policy(Some(
                    cooldown_core::AdvisoryMode::Shorten,
                )))),
                ".",
                &["serde".to_string()],
                &RunOpts::default(),
            )
            .await;
        let advisories = outcome.advisories.expect("fetched");
        assert_eq!(advisories.policy.mode, cooldown_core::AdvisoryMode::Shorten);
    }

    /// Stale data withholds exactly the evidence the gate insisted on, so
    /// `--fail-on-advisory-source` escalates the degradation like an outage instead of certifying
    /// on a warning.
    #[tokio::test]
    async fn the_gate_escalates_a_stale_shorten_degradation() {
        let ws = workspace(FakeFeed::Ok { stale: true });
        let reader = FakeReader {
            advisory_ecosystem: Some("crates.io"),
        };
        let opts = RunOpts {
            fail_on_advisory_source: true,
            ..RunOpts::default()
        };
        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &project_ctx(Some(enabled_policy(Some(
                    cooldown_core::AdvisoryMode::Shorten,
                )))),
                ".",
                &["serde".to_string()],
                &opts,
            )
            .await;
        assert_eq!(outcome.errors.len(), 1);
        assert!(outcome.errors[0].message.contains("stale cache"));
        // The data still annotates — only the shortening is withheld.
        let advisories = outcome.advisories.expect("fetched");
        assert_eq!(advisories.policy.mode, cooldown_core::AdvisoryMode::Flag);
    }

    /// A re-lock introduces packages the initial fetch never asked about, and an unadvised pin
    /// is gated as an ordinary cooldown violation — so the snapshot grows to cover them.
    ///
    /// It grows *additively*: identities already queried keep their entries (including the
    /// empty ones), because a planner decision made on the first snapshot must not be re-judged
    /// against different evidence later.
    #[tokio::test]
    async fn a_top_up_covers_new_packages_without_disturbing_queried_ones() {
        let ws = workspace(FakeFeed::Advising);
        let reader = FakeReader {
            advisory_ecosystem: Some("crates.io"),
        };
        let pctx = project_ctx(Some(enabled_policy(None)));
        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &pctx,
                ".",
                &["serde".to_string()],
                &RunOpts::default(),
            )
            .await;
        let initial = outcome.advisories.expect("fetched");
        assert!(initial.by_package.contains_key("serde"));

        // A package the first fetch did ask about is never re-queried, however many times it is
        // offered.
        let names = vec!["serde".to_string(), "tokio".to_string()];
        assert_eq!(
            initial.unqueried(&reader, &names),
            vec!["tokio".to_string()]
        );

        let topped_up = ws
            .top_up_project_advisories(
                &reader,
                &pctx,
                ".",
                &initial,
                &["tokio".to_string()],
                &RunOpts::default(),
            )
            .await;
        let merged = topped_up.advisories.expect("topped up");
        assert!(merged.by_package.contains_key("tokio"));
        assert_eq!(
            merged.by_package.get("serde").map(Vec::len),
            initial.by_package.get("serde").map(Vec::len),
            "the original entry is carried over untouched"
        );
        assert!(merged.unqueried(&reader, &names).is_empty());
    }

    /// Stale data may annotate but never shorten, and a project's shorten mode is settled by its
    /// first fetch — the rounds before this one already resolved under it — so stale top-up data
    /// is declined outright rather than allowed to shorten a window the stale rule forbids.
    #[tokio::test]
    async fn a_stale_top_up_is_declined_under_the_shorten_mode() {
        let reader = FakeReader {
            advisory_ecosystem: Some("crates.io"),
        };
        let shorten = project_ctx(Some(enabled_policy(Some(
            cooldown_core::AdvisoryMode::Shorten,
        ))));
        let initial = workspace(FakeFeed::Advising)
            .fetch_project_advisories(
                &reader,
                &shorten,
                ".",
                &["serde".to_string()],
                &RunOpts::default(),
            )
            .await
            .advisories
            .expect("fetched");
        assert_eq!(initial.policy.mode, cooldown_core::AdvisoryMode::Shorten);

        let topped_up = workspace(FakeFeed::Ok { stale: true })
            .top_up_project_advisories(
                &reader,
                &shorten,
                ".",
                &initial,
                &["tokio".to_string()],
                &RunOpts::default(),
            )
            .await;
        assert!(topped_up.advisories.is_none(), "stale data cannot shorten");
        assert_eq!(topped_up.warnings.len(), 1);
        assert!(topped_up.warnings[0].message.contains("stale cache"));

        // Under `flag` nothing can be shortened in the first place, so the same data merges.
        let flag = project_ctx(Some(enabled_policy(None)));
        let initial = workspace(FakeFeed::Advising)
            .fetch_project_advisories(
                &reader,
                &flag,
                ".",
                &["serde".to_string()],
                &RunOpts::default(),
            )
            .await
            .advisories
            .expect("fetched");
        let topped_up = workspace(FakeFeed::Ok { stale: true })
            .top_up_project_advisories(
                &reader,
                &flag,
                ".",
                &initial,
                &["tokio".to_string()],
                &RunOpts::default(),
            )
            .await;
        assert!(topped_up.advisories.is_some());
        assert!(topped_up.warnings.is_empty());
    }

    /// A failed top-up is fail-open like the initial fetch: the new packages stay unadvised,
    /// which can only fail to shorten their windows, and the snapshot is left as it was.
    #[tokio::test]
    async fn a_failed_top_up_keeps_the_existing_snapshot() {
        let reader = FakeReader {
            advisory_ecosystem: Some("crates.io"),
        };
        let pctx = project_ctx(Some(enabled_policy(None)));
        let initial = workspace(FakeFeed::Advising)
            .fetch_project_advisories(
                &reader,
                &pctx,
                ".",
                &["serde".to_string()],
                &RunOpts::default(),
            )
            .await
            .advisories
            .expect("fetched");

        let topped_up = workspace(FakeFeed::Unreachable)
            .top_up_project_advisories(
                &reader,
                &pctx,
                ".",
                &initial,
                &["tokio".to_string()],
                &RunOpts::default(),
            )
            .await;
        assert!(topped_up.advisories.is_none());
        assert!(topped_up.errors.is_empty(), "fail-open by default");
        assert_eq!(topped_up.warnings.len(), 1);
        assert_eq!(
            topped_up.warnings[0].kind,
            DiagnosticKind::AdvisorySourceUnavailable
        );
    }

    /// A policy selecting a feed no wired source implements must not certify silently: the run
    /// consults no feed at all, so it is a fail-open warning (an error under the gate), not
    /// inertness.
    #[tokio::test]
    async fn a_source_no_wired_feed_implements_is_reported() {
        let ws = workspace(FakeFeed::Ok { stale: false });
        let reader = FakeReader {
            advisory_ecosystem: Some("crates.io"),
        };
        let github = AdvisoryPolicy {
            enabled: Some(true),
            source: Some(cooldown_core::AdvisorySourceKind::Github),
            ..AdvisoryPolicy::default()
        };
        let pctx = project_ctx(Some(github));

        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &pctx,
                ".",
                &["serde".to_string()],
                &RunOpts::default(),
            )
            .await;
        assert!(outcome.advisories.is_none());
        assert!(outcome.errors.is_empty(), "fail-open by default");
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].message.contains("github"));

        let opts = RunOpts {
            fail_on_advisory_source: true,
            ..RunOpts::default()
        };
        let outcome = ws
            .fetch_project_advisories(&reader, &pctx, ".", &["serde".to_string()], &opts)
            .await;
        assert_eq!(outcome.errors.len(), 1, "the gate refuses to certify");
    }
}
