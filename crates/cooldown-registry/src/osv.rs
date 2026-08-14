//! The OSV advisory source (<https://osv.dev>) — the default `[advisories]` feed.
//!
//! `POST /v1/querybatch` maps the package list to advisory ids, then each unique id is expanded
//! once via `GET /v1/vulns/{id}` (both cached under [`crate::ttl::ADVISORY`]).
//! The query is batched rather than singular: one POST per chunk of the package list, plus one
//! more per pagination token a chunk returns — and the application may call again to cover
//! packages a re-lock introduced.
//! Never one per dependency, which is the cost that matters.
//! Failure stays fail-open at the caller: an unreachable feed can only fail to shorten a
//! window.
//! A response served from a cache past its TTL marks the whole fetch stale, and stale advisory
//! data may annotate but never shorten — the feed is a *loosening* input, so its trust rule is
//! the inverse of the registry's monotonic publish-time floor.

use crate::http::SharedHttp;
use async_trait::async_trait;
use cooldown_core::{
    AdvisoryFetch, AdvisorySeverity, AdvisorySource, AdvisorySourceId, CoreError,
    PackageAdvisories, RawAdvisory, RawAffectedRange, RawRangeEvent, Result,
};
use futures::stream::{self, StreamExt};
use std::collections::{BTreeMap, BTreeSet};

/// The stable identifier of the OSV source.
pub const OSV_SOURCE_ID: AdvisorySourceId = AdvisorySourceId("osv");

const DEFAULT_API_BASE: &str = "https://api.osv.dev";
/// OSV accepts up to 1000 queries per batch; stay well below it.
const QUERYBATCH_CHUNK: usize = 500;
/// Pagination convergence guard: 20 pages ≈ 20 000 advisories for one package, far beyond any
/// real feed.
///
/// Past it the fetch errors loudly instead of looping (or truncating silently).
const QUERYBATCH_MAX_PAGES: usize = 20;
/// How many `GET /v1/vulns/{id}` expansions run concurrently.
const VULN_FANOUT: usize = 8;

/// The OSV advisory source, built over the shared HTTP client (so it inherits the on-disk
/// cache, per-host concurrency cap, backoff, and offline/fresh modes).
pub struct OsvSource {
    http: SharedHttp,
    api_base: String,
}

impl OsvSource {
    /// Builds the source against the public OSV API.
    #[must_use]
    pub fn new(http: SharedHttp) -> Self {
        OsvSource {
            http,
            api_base: DEFAULT_API_BASE.to_string(),
        }
    }

    /// Builds the source against a different API base URL (tests, mirrors, air-gapped proxies).
    #[must_use]
    pub fn with_base(http: SharedHttp, api_base: impl Into<String>) -> Self {
        OsvSource {
            http,
            api_base: api_base.into(),
        }
    }

    async fn query_batch(
        &self,
        ecosystem: &str,
        packages: &[String],
        stale: &mut bool,
    ) -> Result<BTreeMap<String, Vec<String>>> {
        let mut ids_by_package: BTreeMap<String, Vec<String>> = BTreeMap::new();
        // Each entry is one still-owed query: a package plus the token of its next page (`None`
        // for the first).
        // The API paginates one query past 1000 vulnerabilities or a batch past 3000 total, so
        // token-bearing results re-enter the loop until every page is drained — a dropped tail
        // would silently miss advisories while the fetch reports success, which
        // `--fail-on-advisory-source` could never detect.
        let mut pending: Vec<(String, Option<String>)> = packages
            .iter()
            .map(|package| (package.clone(), None))
            .collect();
        let mut rounds = 0usize;
        while !pending.is_empty() {
            rounds += 1;
            if rounds > QUERYBATCH_MAX_PAGES {
                return Err(CoreError::Parse(format!(
                    "OSV querybatch pagination did not converge within {QUERYBATCH_MAX_PAGES} pages"
                )));
            }
            let mut next: Vec<(String, Option<String>)> = Vec::new();
            for chunk in pending.chunks(QUERYBATCH_CHUNK) {
                let queries: Vec<serde_json::Value> = chunk
                    .iter()
                    .map(|(package, token)| match token {
                        Some(token) => serde_json::json!({
                            "package": { "name": package, "ecosystem": ecosystem },
                            "page_token": token,
                        }),
                        None => serde_json::json!({
                            "package": { "name": package, "ecosystem": ecosystem }
                        }),
                    })
                    .collect();
                let body = serde_json::to_string(&serde_json::json!({ "queries": queries }))
                    .map_err(|e| {
                        CoreError::Serialization(format!("serialize OSV querybatch: {e}"))
                    })?;
                let url = format!("{}/v1/querybatch", self.api_base);
                let response = self
                    .http
                    .post_json(&url, &body, crate::ttl::ADVISORY)
                    .await?;
                if !response.is_success() {
                    return Err(CoreError::transient(format!(
                        "OSV querybatch returned HTTP {}",
                        response.status
                    )));
                }
                *stale |= response.stale;
                let parsed: QueryBatchResponse = serde_json::from_str(&response.body)
                    .map_err(|e| CoreError::Parse(format!("OSV querybatch response: {e}")))?;
                if parsed.results.len() != chunk.len() {
                    return Err(CoreError::Parse(format!(
                        "OSV querybatch returned {} results for {} queries",
                        parsed.results.len(),
                        chunk.len()
                    )));
                }
                for ((package, _), result) in chunk.iter().zip(parsed.results) {
                    if !result.vulns.is_empty() {
                        ids_by_package
                            .entry(package.clone())
                            .or_default()
                            .extend(result.vulns.into_iter().map(|vuln| vuln.id));
                    }
                    if let Some(token) = result.next_page_token {
                        next.push((package.clone(), Some(token)));
                    }
                }
            }
            pending = next;
        }
        // A page boundary can repeat an id; the per-package lists are assumed unique
        // downstream.
        for ids in ids_by_package.values_mut() {
            ids.sort();
            ids.dedup();
        }
        Ok(ids_by_package)
    }

    async fn fetch_vuln(&self, id: &str, ecosystem: &str) -> Result<(OsvVuln, bool)> {
        let url = format!("{}/v1/vulns/{id}", self.api_base);
        let response = self.http.get(&url, crate::ttl::ADVISORY).await?;
        if !response.is_success() {
            return Err(CoreError::transient(format!(
                "OSV vulnerability {id} returned HTTP {}",
                response.status
            )));
        }
        let vuln: OsvVuln = serde_json::from_str(&response.body)
            .map_err(|e| CoreError::Parse(format!("OSV vulnerability {id} ({ecosystem}): {e}")))?;
        Ok((vuln, response.stale))
    }
}

#[async_trait]
impl AdvisorySource for OsvSource {
    fn id(&self) -> AdvisorySourceId {
        OSV_SOURCE_ID
    }

    async fn advisories(&self, ecosystem: &str, packages: &[String]) -> Result<AdvisoryFetch> {
        let mut stale = false;
        let ids_by_package = self.query_batch(ecosystem, packages, &mut stale).await?;

        // Expand each unique advisory once; several packages can share one advisory document.
        let unique_ids: BTreeSet<String> = ids_by_package.values().flatten().cloned().collect();
        let fetched: Vec<Result<(OsvVuln, bool)>> = stream::iter(unique_ids)
            .map(|id| async move { self.fetch_vuln(&id, ecosystem).await })
            .buffer_unordered(VULN_FANOUT)
            .collect()
            .await;
        let mut vulns: BTreeMap<String, OsvVuln> = BTreeMap::new();
        for result in fetched {
            let (vuln, vuln_stale) = result?;
            stale |= vuln_stale;
            vulns.insert(vuln.id.clone(), vuln);
        }

        // One vulnerability, several documents: OSV serves alias-connected records (GHSA +
        // RUSTSEC/PYSEC + CVE) as separate documents, and `aliases` is the symmetric,
        // transitive dedup key.
        // Group the fetched documents into alias components so each vulnerability annotates a
        // package once, with the union of its component's evidence.
        let components = AliasComponents::build(&vulns);
        let packages = ids_by_package
            .into_iter()
            .map(|(package, ids)| {
                let mut seen: BTreeSet<&str> = BTreeSet::new();
                let advisories = ids
                    .iter()
                    .filter_map(|id| components.component_of(id))
                    .filter(|(root, _)| seen.insert(root))
                    .map(|(_, docs)| merged_raw_advisory(docs, ecosystem, &package))
                    .collect();
                PackageAdvisories {
                    package,
                    advisories,
                }
            })
            .collect();
        Ok(AdvisoryFetch { packages, stale })
    }
}

/// The fetched documents grouped into alias-connected components.
///
/// Union-find over identifier strings: a document's `aliases` connect it to ids that may
/// themselves be other fetched documents, or to unfetched ids (a shared `CVE-…`) that still
/// join two documents transitively — OSV defines aliases as symmetric and transitive.
struct AliasComponents<'v> {
    /// Fetched document id → its component's representative key.
    root_by_id: BTreeMap<&'v str, String>,
    /// Representative key → the component's documents, sorted by id for determinism.
    members: BTreeMap<String, Vec<&'v OsvVuln>>,
}

impl<'v> AliasComponents<'v> {
    fn build(vulns: &'v BTreeMap<String, OsvVuln>) -> Self {
        fn find(parent: &BTreeMap<String, String>, id: &str) -> String {
            let mut current = id.to_string();
            while let Some(next) = parent.get(&current) {
                if *next == current {
                    break;
                }
                current = next.clone();
            }
            current
        }
        let mut parent: BTreeMap<String, String> = BTreeMap::new();
        for (id, vuln) in vulns {
            for alias in &vuln.aliases {
                let id_root = find(&parent, id);
                let alias_root = find(&parent, alias);
                if id_root != alias_root {
                    parent.insert(alias_root, id_root);
                }
            }
        }
        let mut root_by_id: BTreeMap<&str, String> = BTreeMap::new();
        let mut members: BTreeMap<String, Vec<&OsvVuln>> = BTreeMap::new();
        for (id, vuln) in vulns {
            let root = find(&parent, id);
            root_by_id.insert(id.as_str(), root.clone());
            members.entry(root).or_default().push(vuln);
        }
        for docs in members.values_mut() {
            docs.sort_by(|a, b| a.id.cmp(&b.id));
        }
        AliasComponents {
            root_by_id,
            members,
        }
    }

    /// The component containing the fetched document `id`: its representative key plus every
    /// member document.
    ///
    /// `None` when `id` was not fetched.
    fn component_of(&self, id: &str) -> Option<(&str, &[&'v OsvVuln])> {
        let root = self.root_by_id.get(id)?;
        let docs = self.members.get(root)?;
        Some((root.as_str(), docs))
    }
}

/// Merges one alias component's documents into a single [`RawAdvisory`] scoped to `(ecosystem,
/// package)`.
///
/// Only the members that actually list `package` contribute evidence, severity, and the primary
/// id: severity feeds the shorten threshold, so a sibling record about a *different* package must
/// not raise it (aliases link a vulnerability, and one advisory can rate two packages
/// differently).
/// Every member id and alias still becomes an alias, which is what makes the component one
/// advisory rather than several.
///
/// A withdrawn member is likewise excluded whenever any active one remains: the merged advisory
/// is active if any contributor is, so a withdrawn sibling would otherwise lend its (higher)
/// severity and its obsolete ranges to a live advisory.
/// POST queries already omit withdrawn records, but the per-id document fetch does not, so the
/// mix is reachable through a query/fetch race or a mirror.
///
/// The primary id prefers a `GHSA-…` identifier (the reviewed, cross-ecosystem form) and falls
/// back to the smallest contributing id.
/// Evidence is the union of the contributors' — they describe one vulnerability, so each source's
/// ranges, versions, and fixes testify equally — and severity is their maximum.
/// Withdrawn only when every member listing the package is withdrawn: one still-active record
/// keeps the vulnerability live.
fn merged_raw_advisory(docs: &[&OsvVuln], ecosystem: &str, package: &str) -> RawAdvisory {
    let listing: Vec<&OsvVuln> = docs
        .iter()
        .copied()
        .filter(|doc| lists_package(doc, ecosystem, package))
        .collect();
    let active = listing.iter().any(|doc| doc.withdrawn.is_none());
    let contributors: Vec<&OsvVuln> = listing
        .into_iter()
        .filter(|doc| !active || doc.withdrawn.is_none())
        .collect();
    let parts: Vec<RawAdvisory> = contributors
        .iter()
        .map(|doc| raw_advisory(doc, ecosystem, package))
        .collect();
    let contributor_ids: BTreeSet<&str> = contributors.iter().map(|doc| doc.id.as_str()).collect();
    let non_contributor_aliases: Vec<String> = docs
        .iter()
        .filter(|doc| !contributor_ids.contains(doc.id.as_str()))
        .flat_map(|doc| std::iter::once(doc.id.clone()).chain(doc.aliases.iter().cloned()))
        .collect();
    let primary = parts
        .iter()
        .map(|part| part.id.as_str())
        .filter(|id| id.starts_with("GHSA-"))
        .min()
        .or_else(|| parts.iter().map(|part| part.id.as_str()).min())
        .unwrap_or_default()
        .to_string();
    let mut aliases: BTreeSet<String> = non_contributor_aliases.into_iter().collect();
    let mut severity = AdvisorySeverity::Unknown;
    let mut withdrawn = true;
    let mut summary = parts
        .iter()
        .find(|part| part.id == primary && !part.summary.is_empty())
        .map(|part| part.summary.clone())
        .unwrap_or_default();
    let mut ranges: Vec<RawAffectedRange> = Vec::new();
    let mut affected_versions: Vec<String> = Vec::new();
    let mut fixes: Vec<String> = Vec::new();
    for part in parts {
        if part.id != primary {
            aliases.insert(part.id);
        }
        aliases.extend(part.aliases);
        severity = severity.max(part.severity);
        withdrawn &= part.withdrawn;
        if summary.is_empty() {
            summary = part.summary;
        }
        for range in part.ranges {
            if !ranges.contains(&range) {
                ranges.push(range);
            }
        }
        affected_versions.extend(part.affected_versions);
        fixes.extend(part.fixes);
    }
    aliases.remove(&primary);
    affected_versions.sort();
    affected_versions.dedup();
    fixes.sort();
    fixes.dedup();
    RawAdvisory {
        id: primary,
        aliases: aliases.into_iter().collect(),
        severity,
        withdrawn,
        summary,
        ranges,
        affected_versions,
        fixes,
    }
}

#[derive(serde::Deserialize)]
struct QueryBatchResponse {
    #[serde(default)]
    results: Vec<QueryResult>,
}

#[derive(serde::Deserialize, Default)]
struct QueryResult {
    #[serde(default)]
    vulns: Vec<VulnRef>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(serde::Deserialize)]
struct VulnRef {
    id: String,
}

#[derive(serde::Deserialize)]
struct OsvVuln {
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    details: Option<String>,
    /// An RFC3339 withdrawal instant; its presence means the advisory is withdrawn.
    #[serde(default)]
    withdrawn: Option<String>,
    /// The document-wide `severity[]` entries (CVSS scores/vectors).
    #[serde(default)]
    severity: Vec<OsvSeverity>,
    #[serde(default)]
    affected: Vec<OsvAffected>,
    #[serde(default)]
    database_specific: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct OsvAffected {
    #[serde(default)]
    package: Option<OsvPackage>,
    /// Package-scoped `severity[]` entries, overriding the document-wide ones for this package.
    #[serde(default)]
    severity: Vec<OsvSeverity>,
    #[serde(default)]
    ranges: Vec<OsvRange>,
    #[serde(default)]
    versions: Vec<String>,
}

/// One canonical OSV `severity[]` entry: a type tag plus a score (for the CVSS types, the
/// vector string).
#[derive(serde::Deserialize)]
struct OsvSeverity {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    score: String,
}

#[derive(serde::Deserialize)]
struct OsvPackage {
    #[serde(default)]
    ecosystem: String,
    #[serde(default)]
    name: String,
}

#[derive(serde::Deserialize)]
struct OsvRange {
    #[serde(rename = "type", default)]
    range_type: String,
    #[serde(default)]
    events: Vec<serde_json::Map<String, serde_json::Value>>,
}

/// Normalizes one OSV document into a [`RawAdvisory`] scoped to `(ecosystem, package)`.
///
/// Only that package's `affected[]` entries contribute ranges and versions — an OSV document
/// can span several packages.
/// `GIT` (commit) ranges carry no registry versions and are skipped; a `SEMVER`/`ECOSYSTEM`
/// range's events are carried through verbatim, because assembling them into spans requires
/// sorting them, which needs the version *ordering* only the core has once the boundaries are
/// mapped onto a release list.
/// `fixed` versions are collected here as well — an exact fix-version match is the one piece of
/// evidence that needs no ordering.
fn raw_advisory(vuln: &OsvVuln, ecosystem: &str, package: &str) -> RawAdvisory {
    let mut ranges = Vec::new();
    let mut affected_versions = Vec::new();
    let mut fixes = Vec::new();
    let mut scoped_severity: Vec<&OsvSeverity> = Vec::new();
    for affected in &vuln.affected {
        if !affected_matches(affected, ecosystem, package) {
            continue;
        }
        scoped_severity.extend(affected.severity.iter());
        affected_versions.extend(affected.versions.iter().cloned());
        for range in &affected.ranges {
            if !matches!(range.range_type.as_str(), "SEMVER" | "ECOSYSTEM") {
                continue;
            }
            let mut events = Vec::new();
            for event in &range.events {
                let field = |key: &str| {
                    event
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                };
                if let Some(introduced) = field("introduced") {
                    events.push(RawRangeEvent::Introduced(introduced));
                } else if let Some(fixed) = field("fixed") {
                    fixes.push(fixed.clone());
                    events.push(RawRangeEvent::Fixed(fixed));
                } else if let Some(last) = field("last_affected") {
                    events.push(RawRangeEvent::LastAffected(last));
                } else if let Some(limit) = field("limit") {
                    events.push(RawRangeEvent::Limit(limit));
                }
            }
            if !events.is_empty() {
                ranges.push(RawAffectedRange { events });
            }
        }
    }
    fixes.sort();
    fixes.dedup();
    RawAdvisory {
        id: vuln.id.clone(),
        aliases: vuln.aliases.clone(),
        severity: severity_of(vuln, &scoped_severity),
        withdrawn: vuln.withdrawn.is_some(),
        summary: vuln
            .summary
            .clone()
            .or_else(|| {
                vuln.details
                    .as_ref()
                    .and_then(|d| d.lines().next().map(str::to_string))
            })
            .unwrap_or_default(),
        ranges,
        affected_versions,
        fixes,
    }
}

/// Whether one `affected[]` entry is about `(ecosystem, package)`.
///
/// `SwiftURL` names packages by repository URL and GitHub URLs are case-insensitive, so the
/// lockfile's casing need not match the advisory's; only that ecosystem compares
/// case-insensitively.
fn affected_matches(affected: &OsvAffected, ecosystem: &str, package: &str) -> bool {
    affected.package.as_ref().is_some_and(|p| {
        p.ecosystem == ecosystem
            && (p.name == package
                || (ecosystem == "SwiftURL" && p.name.eq_ignore_ascii_case(package)))
    })
}

/// Whether an OSV document says anything about `(ecosystem, package)` at all.
fn lists_package(vuln: &OsvVuln, ecosystem: &str, package: &str) -> bool {
    vuln.affected
        .iter()
        .any(|affected| affected_matches(affected, ecosystem, package))
}

/// The normalized severity, most package-specific usable rating first.
///
/// Precedence: the matching `affected[]` entries' `severity[]` (one document can span several
/// packages with different ratings), then the document-wide `severity[]`, then the GHSA-style
/// qualitative rating in `database_specific.severity`.
/// Each level falls through when it yields no usable rating, and within a level the maximum
/// rating wins.
/// The result normalizes to [`Unknown`](AdvisorySeverity::Unknown) when nothing usable remains
/// — which never meets a shorten threshold.
fn severity_of(vuln: &OsvVuln, scoped: &[&OsvSeverity]) -> AdvisorySeverity {
    let max_of = |entries: &mut dyn Iterator<Item = &OsvSeverity>| {
        entries
            .map(normalize_severity)
            .max()
            .unwrap_or(AdvisorySeverity::Unknown)
    };
    let scoped_max = max_of(&mut scoped.iter().copied());
    if scoped_max != AdvisorySeverity::Unknown {
        return scoped_max;
    }
    let top_max = max_of(&mut vuln.severity.iter());
    if top_max != AdvisorySeverity::Unknown {
        return top_max;
    }
    vuln.database_specific
        .as_ref()
        .and_then(|extra| extra.get("severity"))
        .and_then(serde_json::Value::as_str)
        .and_then(AdvisorySeverity::parse)
        .unwrap_or(AdvisorySeverity::Unknown)
}

/// Normalizes one `severity[]` entry.
///
/// CVSS v3/v4 vectors are scored with the `RustSec` `cvss` crate; CVSS v2 has no scorer here and
/// normalizes to `Unknown` (a documented gap — v2-only records are ancient).
/// Unrecognized types still accept a plain qualitative token (an `Ubuntu`-style `medium`).
fn normalize_severity(entry: &OsvSeverity) -> AdvisorySeverity {
    match entry.kind.as_str() {
        "CVSS_V3" => entry
            .score
            .parse::<cvss::v3::Base>()
            .ok()
            .map(|base| qualitative(base.severity()))
            .unwrap_or(AdvisorySeverity::Unknown),
        "CVSS_V4" => entry
            .score
            .parse::<cvss::v4::Vector>()
            .ok()
            .map(|vector| qualitative(vector.score().severity()))
            .unwrap_or(AdvisorySeverity::Unknown),
        _ => AdvisorySeverity::parse(&entry.score).unwrap_or(AdvisorySeverity::Unknown),
    }
}

/// Maps a computed CVSS qualitative rating onto the advisory scale.
///
/// `None` (a 0.0 score) maps to `Unknown` deliberately: an explicitly zero-severity advisory
/// must never meet even a `low` shorten threshold.
fn qualitative(severity: cvss::Severity) -> AdvisorySeverity {
    match severity {
        cvss::Severity::None => AdvisorySeverity::Unknown,
        cvss::Severity::Low => AdvisorySeverity::Low,
        cvss::Severity::Medium => AdvisorySeverity::Moderate,
        cvss::Severity::High => AdvisorySeverity::High,
        cvss::Severity::Critical => AdvisorySeverity::Critical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpOptions;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn vuln_from(json: serde_json::Value) -> OsvVuln {
        serde_json::from_value(json).expect("OSV vulnerability document")
    }

    /// A minimal one-shot HTTP responder: answers `connections` sequential requests, routing by
    /// the request line's path prefix.
    async fn serve(
        listener: tokio::net::TcpListener,
        connections: usize,
        routes: Vec<(&'static str, String)>,
    ) {
        for _ in 0..connections {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 16 * 1024];
            let Ok(read) = socket.read(&mut buf).await else {
                continue;
            };
            let request = String::from_utf8_lossy(&buf[..read]).to_string();
            let body = routes
                .iter()
                .find(|(prefix, _)| {
                    request
                        .lines()
                        .next()
                        .is_some_and(|line| line.contains(prefix))
                })
                .map(|(_, body)| body.clone())
                .unwrap_or_else(|| "{}".to_string());
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    }

    #[tokio::test]
    async fn osv_source_batches_queries_and_expands_vulnerabilities() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let querybatch = serde_json::json!({
            "results": [
                { "vulns": [{ "id": "GHSA-x", "modified": "2026-01-01T00:00:00Z" }] },
                {}
            ]
        })
        .to_string();
        let vuln = serde_json::json!({
            "id": "GHSA-x",
            "summary": "bad",
            "database_specific": { "severity": "CRITICAL" },
            "affected": [{
                "package": { "ecosystem": "crates.io", "name": "widget" },
                "ranges": [{
                    "type": "SEMVER",
                    "events": [{ "introduced": "0" }, { "fixed": "1.0.2" }]
                }]
            }]
        })
        .to_string();
        tokio::spawn(serve(
            listener,
            2,
            vec![("/v1/querybatch", querybatch), ("/v1/vulns/GHSA-x", vuln)],
        ));

        let dir = tempfile::tempdir().expect("tempdir");
        let http = SharedHttp::new(dir.path(), HttpOptions::default()).expect("shared http");
        let source = OsvSource::with_base(http, format!("http://{addr}"));

        let fetch = source
            .advisories(
                "crates.io",
                &["widget".to_string(), "clean-widget".to_string()],
            )
            .await
            .expect("advisories");

        assert!(!fetch.stale);
        assert_eq!(fetch.packages.len(), 1, "only the affected package appears");
        let package = &fetch.packages[0];
        assert_eq!(package.package, "widget");
        assert_eq!(package.advisories.len(), 1);
        let advisory = &package.advisories[0];
        assert_eq!(advisory.id, "GHSA-x");
        assert_eq!(advisory.severity, AdvisorySeverity::Critical);
        assert_eq!(advisory.fixes, vec!["1.0.2".to_string()]);
    }

    /// A token-bearing querybatch result is re-queried until exhausted — the tail pages' ids
    /// must land in the result, never be dropped while the fetch reports success.
    #[tokio::test]
    async fn querybatch_pagination_drains_every_page() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let page1 = serde_json::json!({
            "results": [
                { "vulns": [{ "id": "GHSA-a" }], "next_page_token": "t1" }
            ]
        })
        .to_string();
        let page2 = serde_json::json!({
            "results": [
                { "vulns": [{ "id": "GHSA-b" }] }
            ]
        })
        .to_string();
        let doc = |id: &str| {
            serde_json::json!({
                "id": id,
                "affected": [{
                    "package": { "ecosystem": "crates.io", "name": "widget" },
                    "versions": ["1.0.0"]
                }]
            })
            .to_string()
        };
        let routes = [
            ("/v1/vulns/GHSA-a", doc("GHSA-a")),
            ("/v1/vulns/GHSA-b", doc("GHSA-b")),
        ];
        // POSTs are answered in arrival order (page 1, then page 2); GETs route by path.
        tokio::spawn(async move {
            let mut posts = 0usize;
            for _ in 0..4usize {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 16 * 1024];
                let Ok(read) = socket.read(&mut buf).await else {
                    continue;
                };
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let first = request.lines().next().unwrap_or_default().to_string();
                let body = if first.contains("/v1/querybatch") {
                    posts += 1;
                    if posts == 1 {
                        page1.clone()
                    } else {
                        page2.clone()
                    }
                } else {
                    routes
                        .iter()
                        .find(|(prefix, _)| first.contains(prefix))
                        .map(|(_, body)| body.clone())
                        .unwrap_or_else(|| "{}".to_string())
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let http = SharedHttp::new(dir.path(), HttpOptions::default()).expect("shared http");
        let source = OsvSource::with_base(http, format!("http://{addr}"));
        let fetch = source
            .advisories("crates.io", &["widget".to_string()])
            .await
            .expect("advisories");

        assert_eq!(fetch.packages.len(), 1);
        let mut ids: Vec<&str> = fetch.packages[0]
            .advisories
            .iter()
            .map(|advisory| advisory.id.as_str())
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["GHSA-a", "GHSA-b"],
            "the second page's id arrived"
        );
    }

    /// Package-scoped `severity[]` beats the document-wide entries, which beat the GHSA-style
    /// `database_specific.severity` — the most package-specific usable rating wins.
    #[test]
    fn severity_prefers_package_scoped_cvss_over_document_wide() {
        let vuln = vuln_from(serde_json::json!({
            "id": "PYSEC-2026-1",
            // Document-wide: 9.8 (critical).
            "severity": [{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" }],
            "affected": [{
                "package": { "ecosystem": "PyPI", "name": "widget" },
                // Package-scoped: a low base score for *this* package.
                "severity": [{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:L/AC:H/PR:H/UI:R/S:U/C:L/I:N/A:N" }],
                "versions": ["1.0.0"]
            }]
        }));
        assert_eq!(
            raw_advisory(&vuln, "PyPI", "widget").severity,
            AdvisorySeverity::Low
        );
    }

    /// The canonical `severity[]` CVSS vectors are scored — the shape RUSTSEC/GO/PYSEC records
    /// use, which previously all normalized to `Unknown` and could never shorten.
    #[test]
    fn severity_scores_cvss_vectors() {
        let v3 = vuln_from(serde_json::json!({
            "id": "RUSTSEC-2026-0002",
            "severity": [{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" }],
            "affected": [{
                "package": { "ecosystem": "crates.io", "name": "widget" },
                "versions": ["1.0.0"]
            }]
        }));
        assert_eq!(
            raw_advisory(&v3, "crates.io", "widget").severity,
            AdvisorySeverity::Critical
        );

        let v4 = vuln_from(serde_json::json!({
            "id": "GHSA-v4v4",
            "severity": [{ "type": "CVSS_V4", "score": "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N" }],
            "affected": [{
                "package": { "ecosystem": "crates.io", "name": "widget" },
                "versions": ["1.0.0"]
            }]
        }));
        assert_eq!(
            raw_advisory(&v4, "crates.io", "widget").severity,
            AdvisorySeverity::Critical
        );

        // A v2-only rating has no scorer here: Unknown, which never meets a shorten threshold.
        let v2 = vuln_from(serde_json::json!({
            "id": "PYSEC-old",
            "severity": [{ "type": "CVSS_V2", "score": "AV:N/AC:L/Au:N/C:P/I:P/A:P" }],
            "affected": [{
                "package": { "ecosystem": "PyPI", "name": "widget" },
                "versions": ["1.0.0"]
            }]
        }));
        assert_eq!(
            raw_advisory(&v2, "PyPI", "widget").severity,
            AdvisorySeverity::Unknown
        );
    }

    /// Alias-connected documents (GHSA + RUSTSEC sharing a CVE) merge into one advisory: the
    /// GHSA id leads, evidence is the union, severity the maximum — one vulnerability, one
    /// annotation, instead of a duplicate per source record.
    #[test]
    fn alias_connected_documents_merge_into_one_advisory() {
        let ghsa = vuln_from(serde_json::json!({
            "id": "GHSA-aaaa",
            "aliases": ["CVE-2026-0001"],
            "summary": "ghsa summary",
            "database_specific": { "severity": "MODERATE" },
            "affected": [{
                "package": { "ecosystem": "crates.io", "name": "widget" },
                "ranges": [{
                    "type": "SEMVER",
                    "events": [{ "introduced": "0" }, { "fixed": "1.0.2" }]
                }]
            }]
        }));
        let rustsec = vuln_from(serde_json::json!({
            "id": "RUSTSEC-2026-0001",
            "aliases": ["CVE-2026-0001"],
            "severity": [{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" }],
            "affected": [{
                "package": { "ecosystem": "crates.io", "name": "widget" },
                "ranges": [{
                    "type": "ECOSYSTEM",
                    "events": [{ "introduced": "0" }, { "fixed": "1.0.2" }]
                }]
            }]
        }));
        let mut vulns = BTreeMap::new();
        vulns.insert("GHSA-aaaa".to_string(), ghsa);
        vulns.insert("RUSTSEC-2026-0001".to_string(), rustsec);

        let components = AliasComponents::build(&vulns);
        let (root_a, docs) = components.component_of("GHSA-aaaa").expect("component");
        let (root_b, _) = components
            .component_of("RUSTSEC-2026-0001")
            .expect("component");
        assert_eq!(root_a, root_b, "the shared CVE connects the two documents");

        let merged = merged_raw_advisory(docs, "crates.io", "widget");
        assert_eq!(merged.id, "GHSA-aaaa");
        assert!(merged.aliases.contains(&"RUSTSEC-2026-0001".to_string()));
        assert!(merged.aliases.contains(&"CVE-2026-0001".to_string()));
        assert_eq!(
            merged.severity,
            AdvisorySeverity::Critical,
            "maximum across members"
        );
        assert_eq!(merged.fixes, vec!["1.0.2".to_string()]);
        assert_eq!(merged.ranges.len(), 1, "identical ranges collapse");
        assert!(!merged.withdrawn);
        assert_eq!(merged.summary, "ghsa summary");
    }

    /// A component member that says nothing about this package still merges as an *alias*, but
    /// must not lend it evidence or severity: severity feeds the shorten threshold, so a sibling
    /// record rating a different package critical cannot fast-track this one.
    #[test]
    fn merging_ignores_a_members_rating_for_another_package() {
        let ghsa = vuln_from(serde_json::json!({
            "id": "GHSA-aaaa",
            "aliases": ["CVE-2026-0002"],
            "database_specific": { "severity": "LOW" },
            "affected": [{
                "package": { "ecosystem": "crates.io", "name": "widget" },
                "versions": ["1.0.0"]
            }]
        }));
        // Same vulnerability, a different crate, rated critical there.
        let sibling = vuln_from(serde_json::json!({
            "id": "RUSTSEC-2026-0002",
            "aliases": ["CVE-2026-0002"],
            "severity": [{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" }],
            "affected": [{
                "package": { "ecosystem": "crates.io", "name": "other-crate" },
                "versions": ["2.0.0"]
            }]
        }));
        let mut vulns = BTreeMap::new();
        vulns.insert("GHSA-aaaa".to_string(), ghsa);
        vulns.insert("RUSTSEC-2026-0002".to_string(), sibling);

        let components = AliasComponents::build(&vulns);
        let (_, docs) = components.component_of("GHSA-aaaa").expect("component");
        let merged = merged_raw_advisory(docs, "crates.io", "widget");

        assert_eq!(merged.id, "GHSA-aaaa");
        assert_eq!(
            merged.severity,
            AdvisorySeverity::Low,
            "widget's own rating"
        );
        assert_eq!(merged.affected_versions, vec!["1.0.0".to_string()]);
        assert!(
            merged.aliases.contains(&"RUSTSEC-2026-0002".to_string()),
            "the sibling still merges as an alias"
        );
    }

    /// A withdrawn member contributes nothing but its identifier while an active one remains:
    /// the merged advisory is active, so a withdrawn record's rating would raise the live
    /// advisory over the shorten threshold and its ranges would testify as current evidence.
    #[test]
    fn a_withdrawn_member_lends_no_evidence_to_an_active_one() {
        let active = vuln_from(serde_json::json!({
            "id": "GHSA-aaaa",
            "aliases": ["CVE-2026-0003"],
            "database_specific": { "severity": "LOW" },
            "affected": [{
                "package": { "ecosystem": "crates.io", "name": "widget" },
                "versions": ["1.0.0"],
                "ranges": [{
                    "type": "ECOSYSTEM",
                    "events": [{ "introduced": "0" }, { "fixed": "1.0.1" }]
                }]
            }]
        }));
        let retracted = vuln_from(serde_json::json!({
            "id": "RUSTSEC-2026-0003",
            "aliases": ["CVE-2026-0003"],
            "withdrawn": "2026-01-01T00:00:00Z",
            "severity": [{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" }],
            "affected": [{
                "package": { "ecosystem": "crates.io", "name": "widget" },
                "versions": ["9.9.9"],
                "ranges": [{
                    "type": "ECOSYSTEM",
                    "events": [{ "introduced": "0" }, { "fixed": "9.9.9" }]
                }]
            }]
        }));
        let mut vulns = BTreeMap::new();
        vulns.insert("GHSA-aaaa".to_string(), active);
        vulns.insert("RUSTSEC-2026-0003".to_string(), retracted);

        let components = AliasComponents::build(&vulns);
        let (_, docs) = components.component_of("GHSA-aaaa").expect("component");
        let merged = merged_raw_advisory(docs, "crates.io", "widget");

        assert!(!merged.withdrawn);
        assert_eq!(
            merged.severity,
            AdvisorySeverity::Low,
            "the active record's rating decides"
        );
        assert_eq!(merged.fixes, vec!["1.0.1".to_string()]);
        assert_eq!(merged.affected_versions, vec!["1.0.0".to_string()]);
        assert_eq!(merged.ranges.len(), 1);
        assert!(
            merged.aliases.contains(&"RUSTSEC-2026-0003".to_string()),
            "the withdrawn record still merges as an alias"
        );
    }

    /// `limit` events are carried through verbatim, `*` included: the wildcard removes the
    /// ceiling however many finite limits sit beside it, which only the core can decide once the
    /// whole range is mapped.
    #[test]
    fn limit_events_are_carried_through_verbatim() {
        let vuln = vuln_from(serde_json::json!({
            "id": "GHSA-lim",
            "affected": [{
                "package": { "ecosystem": "Go", "name": "example.com/widget" },
                "ranges": [
                    { "type": "SEMVER", "events": [{ "introduced": "1.0.0" }, { "limit": "1.2.0" }] },
                    { "type": "SEMVER", "events": [{ "introduced": "2.0.0" }, { "limit": "*" }] }
                ]
            }]
        }));
        let raw = raw_advisory(&vuln, "Go", "example.com/widget");
        assert_eq!(
            raw.ranges,
            vec![
                RawAffectedRange {
                    events: vec![
                        RawRangeEvent::Introduced("1.0.0".to_string()),
                        RawRangeEvent::Limit("1.2.0".to_string()),
                    ],
                },
                RawAffectedRange {
                    events: vec![
                        RawRangeEvent::Introduced("2.0.0".to_string()),
                        RawRangeEvent::Limit("*".to_string()),
                    ],
                },
            ]
        );
    }

    #[test]
    fn raw_advisory_maps_ranges_fixes_and_severity() {
        let vuln = vuln_from(serde_json::json!({
            "id": "GHSA-v778-237x-gjrc",
            "aliases": ["CVE-2025-0001"],
            "summary": "terrapin-style attack",
            "database_specific": { "severity": "HIGH" },
            "affected": [{
                "package": { "ecosystem": "Go", "name": "golang.org/x/crypto" },
                "ranges": [{
                    "type": "SEMVER",
                    "events": [
                        { "introduced": "0" },
                        { "fixed": "0.17.0" },
                        { "introduced": "0.19.0" },
                        { "fixed": "0.31.0" }
                    ]
                }]
            }, {
                // Another package's entry must not leak into this one's ranges.
                "package": { "ecosystem": "Go", "name": "golang.org/x/other" },
                "ranges": [{ "type": "SEMVER", "events": [{ "introduced": "0" }] }]
            }]
        }));

        let raw = raw_advisory(&vuln, "Go", "golang.org/x/crypto");
        assert_eq!(raw.id, "GHSA-v778-237x-gjrc");
        assert_eq!(raw.severity, AdvisorySeverity::High);
        assert!(!raw.withdrawn);
        assert_eq!(raw.fixes, vec!["0.17.0".to_string(), "0.31.0".to_string()]);
        // One OSV range with four events, not two pre-assembled spans: pairing them is the
        // core's job, once their versions can be ordered.
        assert_eq!(
            raw.ranges,
            vec![RawAffectedRange {
                events: vec![
                    RawRangeEvent::Introduced("0".to_string()),
                    RawRangeEvent::Fixed("0.17.0".to_string()),
                    RawRangeEvent::Introduced("0.19.0".to_string()),
                    RawRangeEvent::Fixed("0.31.0".to_string()),
                ],
            }]
        );
    }

    #[test]
    fn raw_advisory_skips_git_ranges_and_flags_withdrawn() {
        let vuln = vuln_from(serde_json::json!({
            "id": "GHSA-w",
            "withdrawn": "2026-01-01T00:00:00Z",
            "affected": [{
                "package": { "ecosystem": "crates.io", "name": "widget" },
                "ranges": [{
                    "type": "GIT",
                    "events": [{ "introduced": "abc123" }, { "fixed": "def456" }]
                }],
                "versions": ["1.0.0"]
            }]
        }));

        let raw = raw_advisory(&vuln, "crates.io", "widget");
        assert!(raw.withdrawn);
        assert!(
            raw.ranges.is_empty(),
            "GIT ranges carry no registry versions"
        );
        assert_eq!(raw.affected_versions, vec!["1.0.0".to_string()]);
        assert_eq!(raw.severity, AdvisorySeverity::Unknown);
    }

    #[test]
    fn raw_advisory_handles_last_affected_and_summary_fallback() {
        let vuln = vuln_from(serde_json::json!({
            "id": "RUSTSEC-2026-0001",
            "details": "first line\nsecond line",
            "affected": [{
                "package": { "ecosystem": "crates.io", "name": "widget" },
                "ranges": [{
                    "type": "ECOSYSTEM",
                    "events": [{ "introduced": "1.0.0" }, { "last_affected": "1.2.0" }]
                }]
            }]
        }));

        let raw = raw_advisory(&vuln, "crates.io", "widget");
        assert_eq!(raw.summary, "first line");
        assert!(raw.fixes.is_empty());
        assert_eq!(
            raw.ranges,
            vec![RawAffectedRange {
                events: vec![
                    RawRangeEvent::Introduced("1.0.0".to_string()),
                    RawRangeEvent::LastAffected("1.2.0".to_string()),
                ],
            }]
        );
    }
}
