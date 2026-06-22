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
        kind_from_current: kind,
        published_at: pub_at.map(ts),
        yanked: false,
        quality,
    }
}

pub fn yanked(mut r: Release) -> Release {
    r.yanked = true;
    r
}

pub fn dep(name: &str, current: &str, quality: ReleaseQuality) -> Dependency {
    Dependency {
        package: PackageId::new(GO, name, None),
        current: Version::new(current),
        current_quality: quality,
        direct: true,
        artifacts: Vec::new(),
        graph_floor: None,
        graph_ceiling: None,
        members: Vec::new(),
        pinned: false,
    }
}

/// Build a `ResolveContext` rooted at `.` with the major filter off.
pub fn ctx() -> CtxHolder {
    CtxHolder {
        project: camino::Utf8PathBuf::from("."),
        allow_major: false,
    }
}

/// Owns the project path so the borrowed `ResolveContext` can be produced on demand.
pub struct CtxHolder {
    pub project: camino::Utf8PathBuf,
    pub allow_major: bool,
}

impl CtxHolder {
    pub fn major(mut self) -> Self {
        self.allow_major = true;
        self
    }
    pub fn get(&self) -> ResolveContext<'_> {
        ResolveContext {
            tool: GO,
            project: &self.project,
            allow_major: self.allow_major,
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
