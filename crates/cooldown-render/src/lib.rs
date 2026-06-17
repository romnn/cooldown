//! Presentation for the cooldown CLI: the stable, versioned `--json` envelope (one shape across
//! ecosystems and commands) and the colorful TTY tables. `--json` never changes the exit code; it
//! only swaps the renderer.

pub mod model;
pub mod schema;
pub mod tty;

pub use model::*;
pub use schema::{json_schema, json_schema_string};

use serde::Serialize;

/// Serialize an [`Envelope`] to pretty JSON.
pub fn to_json<M: Serialize, S: Serialize, I: Serialize>(env: &Envelope<M, S, I>) -> String {
    serde_json::to_string_pretty(env).expect("envelope serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_shape_is_stable() {
        let env = Envelope::new(
            "outdated",
            true,
            "2026-06-17T13:00:00Z".to_string(),
            OutdatedMeta {},
            OutdatedSummary {
                total: 1,
                adoptable: 1,
                in_cooldown: 0,
                up_to_date: 0,
                exempt: 0,
                held: 0,
                unknown_age: 0,
                errors: 0,
            },
            vec![OutdatedItem {
                name: "golang.org/x/mod".into(),
                ecosystem: "go".into(),
                project: ".".into(),
                registry: Some("proxy.golang.org".into()),
                direct: true,
                current: "v0.17.0".into(),
                window: Window {
                    min_age_days: 7.0,
                    source: "default".into(),
                    clamped_by: None,
                },
                status: OutdatedStatus::Adoptable,
                adoptable_target: Some("v0.18.0".into()),
                latest: Some(LatestInfo {
                    version: "v0.18.0".into(),
                    published_at: Some("2026-05-01T00:00:00Z".into()),
                    age_days: Some(47.0),
                }),
                error: None,
            }],
        );
        let json = to_json(&env);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schemaVersion"], 1);
        assert_eq!(v["command"], "outdated");
        assert_eq!(v["ok"], true);
        assert_eq!(v["items"][0]["adoptableTarget"], "v0.18.0");
        assert_eq!(v["items"][0]["window"]["minAgeDays"], 7.0);
        assert!(v["warnings"].is_array());
        assert!(v["errors"].is_array());
    }

    #[test]
    fn check_meta_flattens_to_top_level() {
        let env = Envelope::new(
            "check",
            false,
            "2026-06-17T13:00:00Z".to_string(),
            CheckMeta {
                scope: "lockfile-graph".into(),
                artifact_scope: "environment".into(),
            },
            CheckSummary {
                checked: 10,
                direct: 3,
                exempt: 1,
                acknowledged: 0,
                unknown_age: 0,
                errors: 0,
                violations: 1,
            },
            Vec::<CheckItem>::new(),
        );
        let v: serde_json::Value = serde_json::from_str(&to_json(&env)).unwrap();
        assert_eq!(v["scope"], "lockfile-graph");
        assert_eq!(v["artifactScope"], "environment");
        assert_eq!(v["summary"]["violations"], 1);
    }

    #[test]
    fn schema_is_valid_json() {
        let s = json_schema_string();
        let _: serde_json::Value = serde_json::from_str(&s).unwrap();
    }
}
