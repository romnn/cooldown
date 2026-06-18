//! The committed `.cooldown-baseline.toml`: currently-young deps recorded as **acknowledged**, so a
//! full-graph `check` can be adopted in an existing repo without a wall of pre-existing violations.
//!
//! Each entry is fully scoped — `(ecosystem, project, package, version, registry)` — so the same
//! young version reintroduced in another project later is not silently grandfathered. A clean
//! ratchet: baseline once, then the set only shrinks.

use cooldown_core::CoreError;
use jiff::Timestamp;
use jiff::civil::Date;

/// The committed baseline file name (`.cooldown-baseline.toml`), resolved against the repo root.
pub const BASELINE_FILE: &str = ".cooldown-baseline.toml";

/// One acknowledged entry: a currently-young pin recorded so `check` does not flag it.
///
/// The acknowledgement is keyed on the full scope `(ecosystem, project, package, version,
/// registry)`; the remaining fields are advisory metadata for human review.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AckEntry {
    /// The ecosystem token (e.g. `"go"`, `"rust"`, `"python"`).
    pub ecosystem: String,
    /// The project path relative to the repo root.
    pub project: String,
    /// The package name as the ecosystem reports it.
    pub package: String,
    /// The acknowledged version; a version change drops the acknowledgement (the ratchet).
    pub version: String,
    /// The registry the package resolves to, when the ecosystem distinguishes registries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// The version's publish time at the moment it was recorded (advisory).
    #[serde(
        rename = "published-at",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub published_at: Option<String>,
    /// The resolved cooldown window in days at the moment it was recorded (advisory).
    #[serde(
        rename = "window-days",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub window_days: Option<f64>,
    /// A human-readable note explaining why the entry exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// An ISO-8601 date after which the acknowledgement no longer applies; an unparsable value errs
    /// toward keeping the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

impl AckEntry {
    /// Does this entry match a pin's full scope?
    fn matches(
        &self,
        ecosystem: &str,
        project: &str,
        package: &str,
        version: &str,
        registry: Option<&str>,
    ) -> bool {
        self.ecosystem == ecosystem
            && self.project == project
            && self.package == package
            && self.version == version
            && self.registry.as_deref() == registry
    }

    /// Whether the entry is still in force at `now` (its `until` has not passed).
    fn in_force(&self, now: Timestamp) -> bool {
        match &self.until {
            None => true,
            Some(s) => match s.parse::<Date>() {
                Ok(until) => {
                    let today = now.to_zoned(jiff::tz::TimeZone::UTC).date();
                    until >= today
                }
                Err(_) => true, // unparsable `until` errs on the side of keeping the ack
            },
        }
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct BaselineToml {
    #[serde(default, rename = "acknowledged")]
    acknowledged: Vec<AckEntry>,
}

/// The loaded baseline set: every acknowledged pin from the committed file.
#[derive(Debug, Default, Clone)]
pub struct Baseline {
    /// The acknowledged entries, in file order.
    pub entries: Vec<AckEntry>,
}

impl Baseline {
    /// Load from a file, or an empty set if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] if the file exists but is not valid baseline TOML, or
    /// [`CoreError::Io`] if it exists but cannot be read. A missing file is not an error.
    pub fn load(path: &camino::Utf8Path) -> Result<Self, CoreError> {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                let parsed: BaselineToml = toml::from_str(&content)
                    .map_err(|e| CoreError::Config(format!("{path}: {e}")))?;
                Ok(Baseline {
                    entries: parsed.acknowledged,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Baseline::default()),
            Err(e) => Err(CoreError::Io(format!("{path}: {e}"))),
        }
    }

    /// Whether a pin is acknowledged: an exact `(ecosystem, project, package, version, registry)`
    /// match that is still in force.
    #[must_use]
    pub fn is_acknowledged(
        &self,
        ecosystem: &str,
        project: &str,
        package: &str,
        version: &str,
        registry: Option<&str>,
        now: Timestamp,
    ) -> bool {
        self.entries
            .iter()
            .any(|e| e.matches(ecosystem, project, package, version, registry) && e.in_force(now))
    }

    /// Serialize to the committed TOML format, with a generated header comment.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Io`] if the entries cannot be serialized to TOML.
    pub fn to_toml(&self) -> Result<String, CoreError> {
        let header =
            "# .cooldown-baseline.toml \u{2014} generated by `cooldown baseline`; review in PRs\n";
        let body = toml::to_string_pretty(&BaselineToml {
            acknowledged: self.entries.clone(),
        })
        .map_err(|e| CoreError::Io(format!("serialize baseline: {e}")))?;
        Ok(format!("{header}{body}"))
    }

    /// Write the baseline to `path` in the committed TOML format.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Io`] if serialization fails or the file cannot be written.
    pub fn save(&self, path: &camino::Utf8Path) -> Result<(), CoreError> {
        std::fs::write(path, self.to_toml()?)?;
        Ok(())
    }
}

impl crate::app::Workspace {
    /// The currently-young pins across the resolved graph, as baseline entries (the set
    /// `cooldown baseline` records as acknowledged).
    ///
    /// Per-dependency registry/release failures are skipped silently; only a project-level
    /// dependency-enumeration failure aborts.
    ///
    /// # Errors
    ///
    /// Returns the [`CoreError`] from an ecosystem adapter if a project's dependency graph cannot
    /// be enumerated.
    pub async fn baseline_entries(
        &self,
        opts: &super::RunOpts,
    ) -> Result<Vec<AckEntry>, CoreError> {
        use cooldown_core::{ArtifactScope, DepScope, Status, TargetContext, check_pin};
        use futures::stream::{self, StreamExt};

        let mut entries = Vec::new();
        for pctx in self.scoped_projects(opts) {
            let Some(adapter) = self.adapter(pctx.ecosystem) else {
                continue;
            };
            let deps = adapter.dependencies(&pctx.project, DepScope::Graph).await?;
            let deps: Vec<_> = deps
                .into_iter()
                .filter(|d| Self::package_in_scope(opts, &d.package.name))
                .collect();
            let tctx = TargetContext {
                project: &pctx.project,
                environments: &[],
                artifacts: if opts.all_artifacts {
                    ArtifactScope::All
                } else {
                    ArtifactScope::Environment
                },
            };
            let rctx = Self::resolve_ctx(pctx, opts);
            let tctx_ref = &tctx;
            let fetched: Vec<_> = stream::iter(deps)
                .map(|dep| async move {
                    let r = adapter.locked_release(&dep, tctx_ref).await;
                    (dep, r)
                })
                .buffer_unordered(opts.fanout())
                .collect()
                .await;

            for (dep, result) in fetched {
                let Ok(locked) = result else { continue };
                let pv = check_pin(&dep, &locked, &pctx.policy.layers, &rctx, self.now());
                if pv.status == Status::CurrentInCooldown {
                    entries.push(AckEntry {
                        ecosystem: pctx.ecosystem.as_str().to_string(),
                        project: pctx.rel_path.to_string(),
                        package: dep.package.name.clone(),
                        version: dep.current.to_string(),
                        registry: dep.package.registry.clone(),
                        published_at: pv.published_at.map(|p| p.to_string()),
                        window_days: Some(super::round2(
                            pv.window.effective_min_age_days(self.now()),
                        )),
                        reason: Some("recorded by `cooldown baseline`".to_string()),
                        until: None,
                    });
                }
            }
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        "2026-06-17T00:00:00Z".parse().unwrap()
    }

    fn entry() -> AckEntry {
        AckEntry {
            ecosystem: "go".into(),
            project: "services/api".into(),
            package: "k8s.io/api".into(),
            version: "0.36.2".into(),
            registry: Some("proxy.golang.org".into()),
            published_at: None,
            window_days: Some(14.0),
            reason: None,
            until: None,
        }
    }

    #[test]
    fn exact_scope_match_only() {
        let b = Baseline {
            entries: vec![entry()],
        };
        assert!(b.is_acknowledged(
            "go",
            "services/api",
            "k8s.io/api",
            "0.36.2",
            Some("proxy.golang.org"),
            now()
        ));
        // Different project → NOT grandfathered.
        assert!(!b.is_acknowledged(
            "go",
            "services/other",
            "k8s.io/api",
            "0.36.2",
            Some("proxy.golang.org"),
            now()
        ));
        // Different version → not matched (ratchet: version change drops the ack).
        assert!(!b.is_acknowledged(
            "go",
            "services/api",
            "k8s.io/api",
            "0.36.3",
            Some("proxy.golang.org"),
            now()
        ));
    }

    #[test]
    fn expired_until_drops_ack() {
        let mut e = entry();
        e.until = Some("2026-01-01".into());
        let b = Baseline { entries: vec![e] };
        assert!(!b.is_acknowledged(
            "go",
            "services/api",
            "k8s.io/api",
            "0.36.2",
            Some("proxy.golang.org"),
            now()
        ));
    }

    #[test]
    fn roundtrip_toml() {
        let b = Baseline {
            entries: vec![entry()],
        };
        let s = b.to_toml().unwrap();
        let back = toml::from_str::<BaselineToml>(&s).unwrap();
        assert_eq!(back.acknowledged.len(), 1);
        assert_eq!(back.acknowledged[0].package, "k8s.io/api");
    }
}
