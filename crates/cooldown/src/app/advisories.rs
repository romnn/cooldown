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
//! to flag for that project — or, when the staleness arrives with an upgrade top-up after the
//! mode is settled, for exactly the topped-up packages.

use super::{AdvisoryFailureMode, ProjectCtx, ProjectProgress, RunOpts, Workspace};
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
    /// The advisory ecosystem used for the initial fetch and every later top-up.
    ecosystem: &'static str,
    /// Advisory-package identities whose evidence arrived from a stale cache *after* the
    /// project's shorten mode was settled by the initial fetch.
    ///
    /// Stale data may annotate but never shorten, and the earlier planning rounds already
    /// resolved under the settled mode, so the demotion cannot be project-wide the way the
    /// initial fetch's is — these packages classify under [`flag_policy`](Self::flag_policy)
    /// instead: their advisories still flag rows and warn on rollbacks, but never earn the
    /// security window.
    flag_only: std::collections::HashSet<String>,
    policy: ResolvedAdvisoryPolicy,
    /// [`policy`](Self::policy) with the shorten mode demoted to flag, for the
    /// [`flag_only`](Self::flag_only) packages.
    flag_policy: ResolvedAdvisoryPolicy,
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
    /// Builds one project's initial snapshot from its (already policy-vetted) fetch.
    fn from_fetch(
        fetch: cooldown_core::AdvisoryFetch,
        queried: Vec<String>,
        source: AdvisorySourceId,
        ecosystem: &'static str,
        policy: ResolvedAdvisoryPolicy,
    ) -> Self {
        let mut by_package: HashMap<String, Vec<RawAdvisory>> = HashMap::new();
        for package in fetch.packages {
            by_package
                .entry(package.package)
                .or_default()
                .extend(package.advisories);
        }
        let flag_policy = ResolvedAdvisoryPolicy {
            mode: AdvisoryMode::Flag,
            ..policy.clone()
        };
        ProjectAdvisories {
            by_package,
            queried: queried.into_iter().collect(),
            source,
            ecosystem,
            flag_only: std::collections::HashSet::new(),
            policy,
            flag_policy,
        }
    }

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
        // A dependency without an advisory identity (its source is not provably in the
        // ecosystem) is never classified, even when a same-named queried package left an entry.
        let package = dep.advisory_identity.as_ref()?;
        let raws = self.by_package.get(package)?;
        // A package whose evidence arrived stale after the shorten mode settled classifies
        // under the demoted policy: it annotates but never shortens.
        let policy = if self.flag_only.contains(package) {
            &self.flag_policy
        } else {
            &self.policy
        };
        let normalize =
            |version: &str| cooldown_core::Version::new(adapter.advisory_version(version));
        Some(ClassifiedAdvisories {
            advisories: raws
                .iter()
                .map(|raw| classify_advisory(raw, self.source, releases, &normalize))
                .collect(),
            policy,
        })
    }

    /// The advisory-package identities among `deps` this fetch never queried, deduplicated.
    /// Dependencies without an identity are not advisory packages at all, so they never count
    /// as unqueried.
    pub(crate) fn unqueried(&self, deps: &[Dependency]) -> Vec<String> {
        let mut packages: Vec<String> = deps
            .iter()
            .filter_map(|dep| dep.advisory_identity.clone())
            .filter(|package| !self.queried.contains(package))
            .collect();
        packages.sort();
        packages.dedup();
        packages
    }

    /// The advisory ecosystem this snapshot was fetched under, for diagnostics about packages
    /// outside it.
    pub(crate) fn ecosystem(&self) -> &'static str {
        self.ecosystem
    }

    /// The snapshot of a fetch that consulted no feed because no package's source was
    /// provable: no evidence, no queried identities — a later re-lock's provable packages
    /// still top it up.
    fn withheld(
        source: AdvisorySourceId,
        ecosystem: &'static str,
        policy: ResolvedAdvisoryPolicy,
    ) -> Self {
        ProjectAdvisories::from_fetch(
            cooldown_core::AdvisoryFetch {
                packages: Vec::new(),
                stale: false,
            },
            Vec::new(),
            source,
            ecosystem,
            policy,
        )
    }

    /// This snapshot extended with `fetch`'s advisories for `queried`.
    ///
    /// Additive by construction: entries already present are copied verbatim, so evidence the
    /// planner has already acted on cannot change under it.
    ///
    /// `flag_only` marks the added identities as annotate-only (stale top-up data under the
    /// shorten mode — see [`Self::flag_only`]); identities the initial fetch already covered
    /// keep their standing either way.
    fn extended(
        &self,
        queried: &[String],
        fetch: cooldown_core::AdvisoryFetch,
        flag_only: bool,
    ) -> Self {
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
        let mut flagged = self.flag_only.clone();
        for package in queried {
            if flag_only && !self.queried.contains(package) {
                flagged.insert(package.clone());
            }
            already.insert(package.clone());
        }
        ProjectAdvisories {
            by_package,
            queried: already,
            source: self.source,
            ecosystem: self.ecosystem,
            flag_only: flagged,
            policy: self.policy.clone(),
            flag_policy: self.flag_policy.clone(),
        }
    }
}

/// The feed query's scope for one project: the distinct advisory-package identities its
/// dependencies carry, plus how many packages carried none.
///
/// Computed eagerly from the dependency list (rather than borrowing it) so callers can hand the
/// dependencies to the release fetch while the advisory fetch runs concurrently.
pub(crate) struct AdvisoryPackages {
    /// The identities to query, sorted and deduplicated.
    identities: Vec<String>,
    /// The distinct package names that carry no advisory identity — their resolved source is
    /// not provably in the tool's advisory ecosystem (see [`Dependency::advisory_identity`]).
    /// Reported once per fetch (and remembered by the upgrade executor so a later top-up
    /// reports only *newly* introduced ones), so an enabled feed never narrows its coverage
    /// silently.
    excluded: Vec<String>,
}

impl AdvisoryPackages {
    pub(crate) fn from_deps(deps: &[Dependency]) -> Self {
        let mut identities: Vec<String> = deps
            .iter()
            .filter_map(|dep| dep.advisory_identity.clone())
            .collect();
        identities.sort();
        identities.dedup();
        let mut excluded: Vec<String> = deps
            .iter()
            .filter(|dep| dep.advisory_identity.is_none())
            .map(|dep| dep.package.name.clone())
            .collect();
        excluded.sort_unstable();
        excluded.dedup();
        AdvisoryPackages {
            identities,
            excluded,
        }
    }

    /// The package names withheld from the feed, for the executor's top-up accounting.
    pub(crate) fn excluded_names(&self) -> &[String] {
        &self.excluded
    }

    fn is_empty(&self) -> bool {
        self.identities.is_empty() && self.excluded.is_empty()
    }
}

/// The outcome of fetching or extending one project's advisories.
pub(crate) struct AdvisoryOutcome {
    pub(crate) advisories: Option<ProjectAdvisories>,
    /// Non-fatal diagnostics: an unsupported tool, an unreachable feed (fail-open), a
    /// stale-cache shorten degradation, or a never-shortening security window.
    pub(crate) warnings: Vec<Diagnostic>,
    /// Fatal diagnostics: every way the enabled feed yielded no usable evidence —
    /// unreachable, unimplemented, or too stale to shorten — once `--fail-on-advisory-source`
    /// insists on it.
    pub(crate) errors: Vec<Diagnostic>,
}

impl AdvisoryOutcome {
    fn inert() -> Self {
        AdvisoryOutcome {
            advisories: None,
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// No usable advisory data: fail open on the window (the ordinary, stricter one stands) and
    /// loud in the output — unless the gate insisted with `--fail-on-advisory-source`.
    fn fail_open(diagnostic: Diagnostic, mode: AdvisoryFailureMode) -> Self {
        let (warnings, errors) = fail_open_split(diagnostic, mode);
        AdvisoryOutcome {
            advisories: None,
            warnings,
            errors,
        }
    }

    /// Records diagnostics in their severity channels and returns usable advisory data.
    pub(crate) fn record(
        self,
        warnings: &mut Vec<Diagnostic>,
        errors: &mut Vec<Diagnostic>,
    ) -> Option<ProjectAdvisories> {
        warnings.extend(self.warnings);
        errors.extend(self.errors);
        self.advisories
    }

    /// Records every diagnostic into a caller that intentionally has one combined channel.
    pub(crate) fn record_flat(
        self,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Option<ProjectAdvisories> {
        diagnostics.extend(self.warnings);
        diagnostics.extend(self.errors);
        self.advisories
    }
}

/// Routes one fail-open diagnostic to the warnings or, when the gate insisted with
/// `--fail-on-advisory-source`, to the errors.
fn fail_open_split(
    diagnostic: Diagnostic,
    mode: AdvisoryFailureMode,
) -> (Vec<Diagnostic>, Vec<Diagnostic>) {
    if mode.is_error() {
        (Vec::new(), vec![diagnostic])
    } else {
        (vec![diagnostic], Vec::new())
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
    kind.as_str()
}

/// Resolves the policy only when it asks the application to consult a feed.
pub(crate) fn advisory_fetch_policy(pctx: &ProjectCtx) -> Option<ResolvedAdvisoryPolicy> {
    let policy = resolve_advisory_policy(&pctx.policy.layers);
    (policy.enabled && policy.source != AdvisorySourceKind::None).then_some(policy)
}

impl Workspace {
    /// Fetches the advisories for one project's dependency set — batched, never per dependency —
    /// when the project's `[advisories]` policy enables it and the tool has an
    /// advisory-database ecosystem.
    ///
    /// Inert (`advisories: None`, no diagnostics) only when the policy asks for nothing: the
    /// feed disabled, `source = "none"`, or no packages to ask about.
    /// Every other way the fetch can come up empty — no wired source implements the selected
    /// `source`, no single advisory ecosystem maps the tool, the feed is unreachable — is reported,
    /// fail-open, so an enabled feed never certifies a run silently.
    pub(crate) async fn fetch_project_advisories(
        &self,
        adapter: &dyn ToolRead,
        pctx: &ProjectCtx,
        project_label: &str,
        packages: AdvisoryPackages,
        opts: &RunOpts,
        progress: &ProjectProgress,
    ) -> AdvisoryOutcome {
        let Some(mut policy) = advisory_fetch_policy(pctx) else {
            return AdvisoryOutcome::inert();
        };
        if packages.is_empty() {
            return AdvisoryOutcome::inert();
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
            return AdvisoryOutcome::fail_open(diagnostic, opts.advisory_failure);
        };
        let mut warnings = Vec::new();
        let Some(ecosystem) = adapter.capabilities().advisory_ecosystem else {
            warnings.push(unsupported_ecosystem_warning(pctx, project_label));
            return AdvisoryOutcome {
                advisories: None,
                warnings,
                errors: Vec::new(),
            };
        };
        let Some(identities) =
            queryable_identities(packages, pctx, project_label, ecosystem, &mut warnings)
        else {
            // Every package's source is outside the ecosystem: nothing to send, so the feed is
            // not consulted at all — but the snapshot still exists, so a later re-lock that
            // introduces a provable package tops it up instead of going unadvised.
            return AdvisoryOutcome {
                advisories: Some(ProjectAdvisories::withheld(source.id(), ecosystem, policy)),
                warnings,
                errors: Vec::new(),
            };
        };

        progress.phase(format!(
            "querying {} advisories for {} packages",
            source.id(),
            identities.len()
        ));
        let fetch = match source.advisories(ecosystem, &identities).await {
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
                let mut outcome = AdvisoryOutcome::fail_open(diagnostic, opts.advisory_failure);
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
            if opts.advisory_failure.is_error() {
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

        AdvisoryOutcome {
            advisories: Some(ProjectAdvisories::from_fetch(
                fetch,
                identities,
                source.id(),
                ecosystem,
                policy,
            )),
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
    /// A stale top-up under the settled shorten mode still merges, annotate-only (see
    /// [`ProjectAdvisories::flag_only`]); a *failed* top-up is fail-open like the initial fetch
    /// — the new packages stay unadvised, which can only fail to shorten their windows.
    pub(crate) async fn top_up_project_advisories(
        &self,
        pctx: &ProjectCtx,
        project_label: &str,
        existing: &ProjectAdvisories,
        packages: &[String],
        opts: &RunOpts,
        progress: &ProjectProgress,
    ) -> AdvisoryOutcome {
        let Some(source) = self
            .advisory_source
            .as_ref()
            .filter(|source| source.id() == existing.source)
        else {
            let diagnostic = Diagnostic::new(
                DiagnosticKind::AdvisorySourceUnavailable,
                format!(
                    "advisory source `{}` no longer matches this project's snapshot; {} newly resolved packages stay un-annotated",
                    existing.source,
                    packages.len()
                ),
            )
            .with_tool(pctx.tool.as_str())
            .with_project(project_label);
            return AdvisoryOutcome::fail_open(diagnostic, opts.advisory_failure);
        };
        progress.phase(format!(
            "querying {} advisories for {} newly resolved packages",
            source.id(),
            packages.len()
        ));
        let fetch = match source.advisories(existing.ecosystem, packages).await {
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
                return AdvisoryOutcome::fail_open(diagnostic, opts.advisory_failure);
            }
        };
        // Stale data may annotate but never shorten, and this project's shorten mode is already
        // settled — the earlier rounds resolved under it — so the demotion is scoped to the
        // topped-up packages instead of the project: their evidence still flags rows and warns
        // on rollbacks, but classifies under the flag policy and never earns the security
        // window.
        // The withheld shortening is exactly the evidence `--fail-on-advisory-source` insists
        // on, so the gate escalates the demotion like the initial fetch's.
        let stale_flag_only = fetch.stale && existing.policy.mode == AdvisoryMode::Shorten;
        let (mut warnings, mut errors) = (Vec::new(), Vec::new());
        if stale_flag_only {
            let diagnostic = Diagnostic::new(
                DiagnosticKind::AdvisorySourceUnavailable,
                format!(
                    "advisory data for {} newly resolved packages was served from a stale cache; annotating them only — the security window is not applied",
                    packages.len()
                ),
            )
            .with_tool(pctx.tool.as_str())
            .with_project(project_label);
            if opts.advisory_failure.is_error() {
                errors.push(diagnostic);
            } else {
                warnings.push(diagnostic);
            }
        }
        AdvisoryOutcome {
            advisories: Some(existing.extended(packages, fetch, stale_flag_only)),
            warnings,
            errors,
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

/// The identities the fetch may query, warning once when some of the project's packages were
/// withheld; `None` when nothing at all is provable and the feed must not be consulted.
fn queryable_identities(
    packages: AdvisoryPackages,
    pctx: &ProjectCtx,
    project_label: &str,
    ecosystem: &'static str,
    warnings: &mut Vec<Diagnostic>,
) -> Option<Vec<String>> {
    let AdvisoryPackages {
        identities,
        excluded,
    } = packages;
    if !excluded.is_empty() {
        warnings.push(excluded_sources_warning(
            pctx,
            project_label,
            ecosystem,
            excluded.len(),
        ));
    }
    (!identities.is_empty()).then_some(identities)
}

/// Packages whose resolved source is not provably in the tool's advisory ecosystem are
/// withheld from the feed entirely — neither sent nor matched (see
/// [`Dependency::advisory_identity`]). Withholding can only fail to *shorten* their windows, so
/// it is a warning, never an error: the ordinary, stricter windows stand.
fn excluded_sources_warning(
    pctx: &ProjectCtx,
    project_label: &str,
    ecosystem: &str,
    excluded: usize,
) -> Diagnostic {
    Diagnostic::new(
        DiagnosticKind::AdvisoryEcosystemUnsupported,
        format!(
            "{excluded} package(s) cannot be proven to resolve from the {ecosystem} advisory ecosystem; they are not queried or annotated and their windows stay un-shortened"
        ),
    )
    .with_tool(pctx.tool.as_str())
    .with_project(project_label)
}

/// The top-up variant of [`excluded_sources_warning`]: a re-lock introduced packages whose
/// source is not provably in the ecosystem, narrowing coverage the initial report never
/// mentioned. The executor calls this once per newly seen package set — including when the
/// re-lock brought nothing queryable at all.
pub(crate) fn top_up_excluded_warning(
    pctx: &ProjectCtx,
    project_label: &str,
    ecosystem: &str,
    excluded: usize,
) -> Diagnostic {
    Diagnostic::new(
        DiagnosticKind::AdvisoryEcosystemUnsupported,
        format!(
            "{excluded} newly resolved package(s) cannot be proven to resolve from the {ecosystem} advisory ecosystem; they are not queried or annotated and their windows stay un-shortened"
        ),
    )
    .with_tool(pctx.tool.as_str())
    .with_project(project_label)
}

fn unsupported_ecosystem_warning(pctx: &ProjectCtx, project_label: &str) -> Diagnostic {
    Diagnostic::new(
        DiagnosticKind::AdvisoryEcosystemUnsupported,
        format!(
            "tool `{}` is not mapped to one advisory-database ecosystem; its packages are not annotated",
            pctx.tool.as_str()
        ),
    )
    .with_tool(pctx.tool.as_str())
    .with_project(project_label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ProjectProgress;
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
        Advising {
            stale: bool,
        },
        OtherSource,
        Unreachable,
    }

    #[async_trait]
    impl AdvisorySource for FakeFeed {
        fn id(&self) -> AdvisorySourceId {
            match self {
                FakeFeed::OtherSource => AdvisorySourceId("other"),
                FakeFeed::Ok { .. } | FakeFeed::Advising { .. } | FakeFeed::Unreachable => {
                    AdvisorySourceId("osv")
                }
            }
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
                FakeFeed::OtherSource => Ok(AdvisoryFetch {
                    packages: packages
                        .iter()
                        .map(|package| PackageAdvisories {
                            package: package.clone(),
                            advisories: Vec::new(),
                        })
                        .collect(),
                    stale: false,
                }),
                FakeFeed::Advising { stale } => Ok(AdvisoryFetch {
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
                    stale: *stale,
                }),
                FakeFeed::Unreachable => {
                    Err(CoreError::transient("connection refused".to_string()))
                }
            }
        }
    }

    /// A minimal dependency whose advisory identity is its name — the common adapter case.
    fn dep(name: &str) -> Dependency {
        Dependency {
            package: cooldown_core::PackageId::new(CARGO, name.to_string(), None),
            advisory_identity: Some(name.to_string()),
            current: cooldown_core::Version::new("1.0.0".to_string()),
            current_quality: cooldown_core::ReleaseQuality::Stable,
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: Vec::new(),
            pinned: false,
            hold_edges: Vec::new(),
        }
    }

    /// The fetch scope for `names`, every one of them carrying an identity.
    fn queries(names: &[&str]) -> AdvisoryPackages {
        AdvisoryPackages {
            identities: names.iter().map(ToString::to_string).collect(),
            excluded: Vec::new(),
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
            single_copy: Vec::new(),
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
                queries(&["serde"]),
                &RunOpts::default(),
                &ProjectProgress::default(),
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
                queries(&["serde"]),
                &RunOpts::default(),
                &ProjectProgress::default(),
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
            advisory_failure: AdvisoryFailureMode::Error,
            ..RunOpts::default()
        };
        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &pctx,
                ".",
                queries(&["serde"]),
                &opts,
                &ProjectProgress::default(),
            )
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
                queries(&["conda-thing"]),
                &RunOpts::default(),
                &ProjectProgress::default(),
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
                queries(&["serde"]),
                &RunOpts::default(),
                &ProjectProgress::default(),
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
                queries(&["serde"]),
                &RunOpts::default(),
                &ProjectProgress::default(),
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
            advisory_failure: AdvisoryFailureMode::Error,
            ..RunOpts::default()
        };
        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &project_ctx(Some(enabled_policy(Some(
                    cooldown_core::AdvisoryMode::Shorten,
                )))),
                ".",
                queries(&["serde"]),
                &opts,
                &ProjectProgress::default(),
            )
            .await;
        assert_eq!(outcome.errors.len(), 1);
        assert!(outcome.errors[0].message.contains("stale cache"));
        // The data still annotates — only the shortening is withheld.
        let advisories = outcome.advisories.expect("fetched");
        assert_eq!(advisories.policy.mode, cooldown_core::AdvisoryMode::Flag);
    }

    /// Packages without an advisory identity are withheld from the feed, but never silently:
    /// the fetch warns once, still queries the provable identities — and when nothing at all
    /// is provable it skips the feed entirely while still building the (empty) snapshot, so a
    /// later re-lock's provable packages can top it up.
    #[tokio::test]
    async fn unproven_sources_are_withheld_loudly() {
        let ws = workspace(FakeFeed::Advising { stale: false });
        let reader = FakeReader {
            advisory_ecosystem: Some("crates.io"),
        };
        let pctx = project_ctx(Some(enabled_policy(None)));

        let mut deps = vec![dep("serde"), dep("internal-utils")];
        deps[1].advisory_identity = None;
        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &pctx,
                ".",
                AdvisoryPackages::from_deps(&deps),
                &RunOpts::default(),
                &ProjectProgress::default(),
            )
            .await;
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].message.contains("advisory ecosystem"));
        let advisories = outcome.advisories.expect("fetched");
        assert!(
            advisories.by_package.contains_key("serde"),
            "the provable identity is still queried"
        );
        assert!(
            !advisories.by_package.contains_key("internal-utils"),
            "the unproven package is never sent to the feed"
        );

        // Nothing provable at all: the feed is not consulted, the snapshot still exists.
        let unreachable = workspace(FakeFeed::Unreachable);
        let mut deps = vec![dep("internal-utils")];
        deps[0].advisory_identity = None;
        let outcome = unreachable
            .fetch_project_advisories(
                &reader,
                &pctx,
                ".",
                AdvisoryPackages::from_deps(&deps),
                &RunOpts::default(),
                &ProjectProgress::default(),
            )
            .await;
        assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
        let advisories = outcome
            .advisories
            .expect("an all-withheld fetch still yields a snapshot for later top-ups");
        assert!(advisories.by_package.is_empty());
        assert!(advisories.queried.is_empty());
    }

    /// A re-lock introduces packages the initial fetch never asked about, and an unadvised pin
    /// is gated as an ordinary cooldown violation — so the snapshot grows to cover them.
    ///
    /// It grows *additively*: identities already queried keep their entries (including the
    /// empty ones), because a planner decision made on the first snapshot must not be re-judged
    /// against different evidence later.
    #[tokio::test]
    async fn a_top_up_covers_new_packages_without_disturbing_queried_ones() {
        let ws = workspace(FakeFeed::Advising { stale: false });
        let reader = FakeReader {
            advisory_ecosystem: Some("crates.io"),
        };
        let pctx = project_ctx(Some(enabled_policy(None)));
        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &pctx,
                ".",
                queries(&["serde"]),
                &RunOpts::default(),
                &ProjectProgress::default(),
            )
            .await;
        let initial = outcome.advisories.expect("fetched");
        assert!(initial.by_package.contains_key("serde"));

        // A package the first fetch did ask about is never re-queried, however many times it is
        // offered.
        let deps = vec![dep("serde"), dep("tokio")];
        assert_eq!(initial.unqueried(&deps), vec!["tokio".to_string()]);

        let topped_up = ws
            .top_up_project_advisories(
                &pctx,
                ".",
                &initial,
                &["tokio".to_string()],
                &RunOpts::default(),
                &ProjectProgress::default(),
            )
            .await;
        let merged = topped_up.advisories.expect("topped up");
        assert!(merged.by_package.contains_key("tokio"));
        assert_eq!(
            merged.by_package.get("serde").map(Vec::len),
            initial.by_package.get("serde").map(Vec::len),
            "the original entry is carried over untouched"
        );
        assert!(merged.unqueried(&deps).is_empty());
    }

    /// Stale data may annotate but never shorten, and a project's shorten mode is settled by its
    /// first fetch — the rounds before this one already resolved under it — so stale top-up
    /// evidence merges annotate-only: the topped-up packages classify under the demoted flag
    /// policy while the initially fetched ones keep the settled shorten mode.
    #[tokio::test]
    async fn a_stale_top_up_annotates_but_never_shortens() {
        let reader = FakeReader {
            advisory_ecosystem: Some("crates.io"),
        };
        let shorten = project_ctx(Some(enabled_policy(Some(
            cooldown_core::AdvisoryMode::Shorten,
        ))));
        let initial = workspace(FakeFeed::Advising { stale: false })
            .fetch_project_advisories(
                &reader,
                &shorten,
                ".",
                queries(&["serde"]),
                &RunOpts::default(),
                &ProjectProgress::default(),
            )
            .await
            .advisories
            .expect("fetched");
        assert_eq!(initial.policy.mode, cooldown_core::AdvisoryMode::Shorten);

        let topped_up = workspace(FakeFeed::Advising { stale: true })
            .top_up_project_advisories(
                &shorten,
                ".",
                &initial,
                &["tokio".to_string()],
                &RunOpts::default(),
                &ProjectProgress::default(),
            )
            .await;
        assert_eq!(topped_up.warnings.len(), 1);
        assert!(topped_up.warnings[0].message.contains("stale cache"));
        let merged = topped_up
            .advisories
            .expect("stale evidence still annotates");
        assert!(
            merged.by_package.contains_key("tokio"),
            "the topped-up advisory is kept for annotations and rollback warnings"
        );
        assert!(
            merged.flag_only.contains("tokio"),
            "…but classifies under the demoted flag policy"
        );
        assert_eq!(merged.flag_policy.mode, cooldown_core::AdvisoryMode::Flag);
        assert!(
            !merged.flag_only.contains("serde"),
            "the initially fetched package keeps the settled shorten mode"
        );
        assert_eq!(merged.policy.mode, cooldown_core::AdvisoryMode::Shorten);

        // The gate escalates the withheld shortening like the initial fetch's stale degradation,
        // and the evidence still merges.
        let topped_up = workspace(FakeFeed::Advising { stale: true })
            .top_up_project_advisories(
                &shorten,
                ".",
                &initial,
                &["tokio".to_string()],
                &RunOpts {
                    advisory_failure: AdvisoryFailureMode::Error,
                    ..RunOpts::default()
                },
                &ProjectProgress::default(),
            )
            .await;
        assert_eq!(topped_up.errors.len(), 1);
        assert!(topped_up.advisories.is_some());

        // Under `flag` nothing can be shortened in the first place, so the same data merges.
        let flag = project_ctx(Some(enabled_policy(None)));
        let initial = workspace(FakeFeed::Advising { stale: false })
            .fetch_project_advisories(
                &reader,
                &flag,
                ".",
                queries(&["serde"]),
                &RunOpts::default(),
                &ProjectProgress::default(),
            )
            .await
            .advisories
            .expect("fetched");
        let topped_up = workspace(FakeFeed::Ok { stale: true })
            .top_up_project_advisories(
                &flag,
                ".",
                &initial,
                &["tokio".to_string()],
                &RunOpts::default(),
                &ProjectProgress::default(),
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
        let initial = workspace(FakeFeed::Advising { stale: false })
            .fetch_project_advisories(
                &reader,
                &pctx,
                ".",
                queries(&["serde"]),
                &RunOpts::default(),
                &ProjectProgress::default(),
            )
            .await
            .advisories
            .expect("fetched");

        let topped_up = workspace(FakeFeed::Unreachable)
            .top_up_project_advisories(
                &pctx,
                ".",
                &initial,
                &["tokio".to_string()],
                &RunOpts::default(),
                &ProjectProgress::default(),
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

    #[tokio::test]
    async fn a_top_up_cannot_switch_advisory_sources() {
        let reader = FakeReader {
            advisory_ecosystem: Some("crates.io"),
        };
        let pctx = project_ctx(Some(enabled_policy(None)));
        let initial = workspace(FakeFeed::Advising { stale: false })
            .fetch_project_advisories(
                &reader,
                &pctx,
                ".",
                queries(&["serde"]),
                &RunOpts::default(),
                &ProjectProgress::default(),
            )
            .await
            .advisories
            .expect("fetched");

        let topped_up = workspace(FakeFeed::OtherSource)
            .top_up_project_advisories(
                &pctx,
                ".",
                &initial,
                &["tokio".to_string()],
                &RunOpts::default(),
                &ProjectProgress::default(),
            )
            .await;
        assert!(topped_up.advisories.is_none());
        assert_eq!(topped_up.warnings.len(), 1);
        assert!(topped_up.warnings[0].message.contains("no longer matches"));
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
                queries(&["serde"]),
                &RunOpts::default(),
                &ProjectProgress::default(),
            )
            .await;
        assert!(outcome.advisories.is_none());
        assert!(outcome.errors.is_empty(), "fail-open by default");
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].message.contains("github"));

        let opts = RunOpts {
            advisory_failure: AdvisoryFailureMode::Error,
            ..RunOpts::default()
        };
        let outcome = ws
            .fetch_project_advisories(
                &reader,
                &pctx,
                ".",
                queries(&["serde"]),
                &opts,
                &ProjectProgress::default(),
            )
            .await;
        assert_eq!(outcome.errors.len(), 1, "the gate refuses to certify");
    }
}
