//! Shared builders for the core test suites.
#![allow(
    dead_code,
    reason = "shared test builders; each integration-test binary links only the subset it uses"
)]
#![allow(
    clippy::expect_used,
    reason = "test-support helpers: panicking on malformed fixture input is the intended immediate test failure (clippy.toml sets allow-expect-in-tests)"
)]

use cooldown_core::*;
use jiff::Timestamp;

pub const GO: ToolId = ToolId("go");

/// A fixed `now` for deterministic boundary maths. The default 7d window cuts off at
/// `2026-06-10T00:00:00Z`.
pub fn now() -> Timestamp {
    ts("2026-06-17T00:00:00Z")
}

pub fn ts(s: &str) -> Timestamp {
    s.parse().expect("valid RFC3339 timestamp")
}

pub fn order(bytes: &[u8]) -> ReleaseOrder {
    ReleaseOrder(bytes.to_vec())
}

/// Build a release. `pub_at` is an RFC3339 string or `None` for unknown age.
pub fn rel(
    v: &str,
    ord: &[u8],
    major: &str,
    kind: Option<UpdateKind>,
    pub_at: Option<&str>,
    quality: ReleaseQuality,
) -> Release {
    Release {
        version: Version::new(v),
        order: order(ord),
        major: MajorKey(major.to_string()),
        major_number: major.parse().ok(),
        kind_from_current: kind,
        beyond_declared_bound: false,
        beyond_latest_tag: false,
        published_at: pub_at.map(ts),
        yanked: false,
        quality,
    }
}

/// Mark a release as ordered above the registry's `latest` dist-tag.
pub fn above_tag(mut r: Release) -> Release {
    r.beyond_latest_tag = true;
    r
}

pub fn yanked(mut r: Release) -> Release {
    r.yanked = true;
    r
}

pub fn dep(name: &str, current: &str, quality: ReleaseQuality) -> Dependency {
    Dependency {
        package: PackageId::new(GO, name, None),
        advisory_identity: Some(name.to_string()),
        current: Version::new(current),
        current_quality: quality,
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

/// Build a `ResolveContext` rooted at `.` with the major filter off.
pub fn ctx() -> CtxHolder {
    CtxHolder {
        project: camino::Utf8PathBuf::from("."),
        allow_major: false,
        honor_declared_bounds: true,
        honor_latest_tag: true,
    }
}

/// Owns the project path so the borrowed `ResolveContext` can be produced on demand.
pub struct CtxHolder {
    pub project: camino::Utf8PathBuf,
    pub allow_major: bool,
    pub honor_declared_bounds: bool,
    pub honor_latest_tag: bool,
}

impl CtxHolder {
    pub fn major(mut self) -> Self {
        self.allow_major = true;
        self
    }
    pub fn rewrite_bounds(mut self) -> Self {
        self.honor_declared_bounds = false;
        self
    }
    pub fn ignore_dist_tags(mut self) -> Self {
        self.honor_latest_tag = false;
        self
    }
    pub fn get(&self) -> ResolveContext<'_> {
        ResolveContext {
            tool: GO,
            project: &self.project,
            allow_major: self.allow_major,
            honor_declared_bounds: self.honor_declared_bounds,
            honor_latest_tag: self.honor_latest_tag,
        }
    }
}

/// The built-in default 7d layer plus any extra layers parsed from TOML at the given origins.
pub fn layers_from(extra: Vec<PolicyLayer>) -> Vec<PolicyLayer> {
    let mut v = vec![cooldown_core::config::builtin_default_layer()];
    v.extend(extra);
    v
}

/// Parse a TOML config string into a layer at the given origin.
pub fn layer(toml: &str, origin: Origin) -> PolicyLayer {
    cooldown_core::config::parse_config(toml, origin).expect("valid config")
}
