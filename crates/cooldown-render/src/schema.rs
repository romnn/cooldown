//! The machine-readable JSON schema for `--json` output, printed by `cooldown schema`.

use crate::model::SCHEMA_VERSION;
use cooldown_core::DiagnosticKind;
use serde_json::{Map, Value, json};

/// A JSON Schema (draft 2020-12) describing the command-specific envelopes.
#[allow(
    clippy::too_many_lines,
    reason = "the schema is a declarative contract literal; splitting it further would obscure the envelope shape"
)]
#[must_use]
pub fn json_schema() -> Value {
    let diagnostic_kinds = DiagnosticKind::ALL
        .iter()
        .map(|kind| Value::String(kind.wire_value().to_string()))
        .collect::<Vec<_>>();
    let diagnostic = json!({
        "type": "object",
        "required": ["kind", "message"],
        "properties": {
            "kind": {
                "enum": diagnostic_kinds
            },
            "message": { "type": "string" },
            "tool": { "type": "string" },
            "project": { "type": "string" },
            "package": { "type": "string" },
            "version": { "type": "string" },
            "registry": { "type": "string" },
            "path": { "type": "string" }
        },
        "additionalProperties": false
    });

    let window = json!({
        "type": "object",
        "required": ["minAgeDays", "source"],
        "properties": {
            "minAgeDays": { "type": "number" },
            "source": { "type": "string" },
            "clampedBy": { "type": "string" },
            "shortenedBy": { "type": "string" }
        },
        "additionalProperties": false
    });

    // The advisory block on a security-relevant row (`[advisories]`): which advisories
    // `version` fixes and whether the security window was applied to it.
    // On a `check` row `version` is the locked pin; on an `outdated` row it is the
    // security-relevant candidate, which need not be the candidate whose cooldown the row
    // displays.
    let security = json!({
        "type": "object",
        "required": ["version", "fixes", "severity", "source", "applied"],
        "properties": {
            "version": { "type": "string" },
            "fixes": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
            "severity": { "enum": ["low", "moderate", "high", "critical", "unknown"] },
            "source": { "type": "string" },
            "applied": { "type": "boolean" }
        },
        "additionalProperties": false
    });

    let latest = json!({
        "type": "object",
        "required": ["version"],
        "properties": {
            "version": { "type": "string" },
            "publishedAt": { "type": "string", "format": "date-time" },
            "ageDays": { "type": "number" }
        },
        "additionalProperties": false
    });

    let member_ref = json!({
        "type": "object",
        "required": ["name", "path"],
        "properties": {
            "name": { "type": "string" },
            "path": { "type": "string" }
        },
        "additionalProperties": false
    });

    let members = json!({
        "type": "array",
        "items": { "$ref": "#/$defs/memberRef" }
    });

    // The closed reason set comes from `SkipReason::ALL` — the enum's own enumeration — so a new
    // variant lands in the published schema the moment it exists, instead of drifting behind a
    // hand-maintained string list.
    let skip_reasons: Vec<Value> = cooldown_core::SkipReason::ALL
        .iter()
        .map(|reason| Value::String(reason.wire_value().to_string()))
        .collect();
    // Same construction for the edge-binding action set.
    let edge_actions: Vec<Value> = cooldown_core::EdgeBindingAction::ALL
        .iter()
        .map(|action| Value::String(action.wire_value().to_string()))
        .collect();
    let applied_edge_actions: Vec<Value> = cooldown_core::EdgeBindingAction::ALL
        .iter()
        .filter(|action| action.is_applied())
        .map(|action| Value::String(action.wire_value().to_string()))
        .collect();
    let unapplied_edge_actions: Vec<Value> = cooldown_core::EdgeBindingAction::ALL
        .iter()
        .filter(|action| !action.is_applied())
        .map(|action| Value::String(action.wire_value().to_string()))
        .collect();
    let reasoned_edge_actions = cooldown_core::EdgeBindingAction::ALL
        .iter()
        .filter(|action| action.requires_detail())
        .map(|action| Value::String(action.wire_value().to_string()))
        .collect::<Vec<_>>();
    let recovery_statuses: Vec<Value> = crate::model::RecoveryStatus::ALL
        .iter()
        .map(|status| Value::String(status.wire_value().to_string()))
        .collect();
    let skipped = json!({
        "type": "object",
        "required": ["reason", "message"],
        "properties": {
            "reason": { "enum": skip_reasons },
            "message": { "type": "string" },
            "offending": { "type": "string" }
        },
        "additionalProperties": false
    });

    let effective = json!({
        "type": "object",
        "required": ["minAgeDays", "decidedBy"],
        "properties": {
            "minAgeDays": { "type": "number" },
            "decidedBy": { "type": "string" }
        },
        "additionalProperties": false
    });

    let defs = json!({
        "diagnostic": diagnostic,
        "window": window,
        "securityInfo": security,
        "latestInfo": latest,
        "memberRef": member_ref,
        "skippedInfo": skipped,
        "effectiveInfo": effective,
        "outdatedSummary": {
            "type": "object",
            "required": ["total", "skippedStaleProjects", "adoptable", "blocked", "inCooldown", "upToDate", "exempt", "held", "unknownAge", "errors"],
            "properties": {
                "total": { "type": "integer", "minimum": 0 },
                "skippedStaleProjects": { "type": "integer", "minimum": 0 },
                "adoptable": { "type": "integer", "minimum": 0 },
                "blocked": { "type": "integer", "minimum": 0 },
                "inCooldown": { "type": "integer", "minimum": 0 },
                "upToDate": { "type": "integer", "minimum": 0 },
                "exempt": { "type": "integer", "minimum": 0 },
                "held": { "type": "integer", "minimum": 0 },
                "unknownAge": { "type": "integer", "minimum": 0 },
                "errors": { "type": "integer", "minimum": 0 }
            },
            "additionalProperties": false
        },
        "outdatedItem": {
            "type": "object",
            "required": ["name", "tool", "project", "direct", "current", "window", "status"],
            "properties": {
                "name": { "type": "string" },
                "tool": { "type": "string" },
                "project": { "type": "string" },
                "registry": { "type": "string" },
                "direct": { "type": "boolean" },
                "current": { "type": "string" },
                "members": members,
                "window": { "$ref": "#/$defs/window" },
                "candidateAgeDays": { "type": "number" },
                "cooldownVersion": { "type": "string" },
                "status": {
                    "enum": [
                        "up_to_date",
                        "adoptable",
                        "blocked",
                        "in_cooldown",
                        "exempt",
                        "held",
                        "current_in_cooldown",
                        "unknown_age",
                        "error"
                    ]
                },
                "adoptableTarget": { "type": "string" },
                "blockedBy": { "type": "string" },
                "heldBy": { "type": "string" },
                "latest": { "$ref": "#/$defs/latestInfo" },
                "security": { "$ref": "#/$defs/securityInfo" },
                "error": { "$ref": "#/$defs/diagnostic" }
            },
            "additionalProperties": false
        },
        "checkSummary": {
            "type": "object",
            "required": ["checked", "skippedStaleProjects", "direct", "exempt", "acknowledged", "allowed", "unknownAge", "errors", "violations", "securityRelevant"],
            "properties": {
                "checked": { "type": "integer", "minimum": 0 },
                "skippedStaleProjects": { "type": "integer", "minimum": 0 },
                "direct": { "type": "integer", "minimum": 0 },
                "exempt": { "type": "integer", "minimum": 0 },
                "acknowledged": { "type": "integer", "minimum": 0 },
                "allowed": { "type": "integer", "minimum": 0 },
                "unknownAge": { "type": "integer", "minimum": 0 },
                "errors": { "type": "integer", "minimum": 0 },
                "violations": { "type": "integer", "minimum": 0 },
                "securityRelevant": { "type": "integer", "minimum": 0 }
            },
            "additionalProperties": false
        },
        "checkItem": {
            "type": "object",
            "required": ["name", "tool", "project", "direct", "current", "window", "status", "graphHeld"],
            "properties": {
                "name": { "type": "string" },
                "tool": { "type": "string" },
                "project": { "type": "string" },
                "registry": { "type": "string" },
                "members": members,
                "direct": { "type": "boolean" },
                "current": { "type": "string" },
                "publishedAt": { "type": "string", "format": "date-time" },
                "ageDays": { "type": "number" },
                "window": { "$ref": "#/$defs/window" },
                "status": { "enum": ["violation", "acknowledged", "allowed", "unknown_age", "error"] },
                "graphHeld": { "type": "boolean" },
                "graphFloor": { "type": "string" },
                "security": { "$ref": "#/$defs/securityInfo" },
                "error": { "$ref": "#/$defs/diagnostic" }
            },
            "additionalProperties": false
        },
        "upgradeSummary": {
            "type": "object",
            "required": ["applied", "skipped", "errors", "edgesCorrected", "edgesRebound", "edgesHeld", "edgesUnaddressable"],
            "properties": {
                "applied": { "type": "integer", "minimum": 0 },
                "skipped": { "type": "integer", "minimum": 0 },
                "errors": { "type": "integer", "minimum": 0 },
                "edgesCorrected": { "type": "integer", "minimum": 0 },
                "edgesRebound": { "type": "integer", "minimum": 0 },
                "edgesHeld": { "type": "integer", "minimum": 0 },
                "edgesUnaddressable": { "type": "integer", "minimum": 0 }
            },
            "additionalProperties": false
        },
        "upgradeItem": {
            "type": "object",
            "required": ["name", "tool", "project", "direct", "downgrade", "from", "to", "kind", "applied"],
            "allOf": [
                {
                    "if": {
                        "required": ["edge"],
                        "properties": {
                            "edge": {
                                "required": ["action"],
                                "properties": {
                                    "action": { "enum": unapplied_edge_actions }
                                }
                            }
                        }
                    },
                    "then": { "properties": { "applied": { "const": false } } }
                },
                {
                    "if": {
                        "required": ["edge"],
                        "properties": {
                            "edge": {
                                "required": ["action"],
                                "properties": {
                                    "action": { "enum": applied_edge_actions }
                                }
                            }
                        }
                    },
                    "then": { "properties": { "applied": { "const": true } } }
                },
                {
                    "if": { "required": ["edge"] },
                    "then": {
                        "properties": {
                            "direct": { "const": false },
                            "downgrade": { "const": false },
                            "members": { "maxItems": 0 }
                        },
                        "not": {
                            "anyOf": [
                                { "required": ["skipped"] },
                                { "required": ["error"] }
                            ]
                        }
                    }
                },
                {
                    "if": {
                        "required": ["edge"],
                        "properties": {
                            "edge": {
                                "required": ["action"],
                                "properties": {
                                    "action": { "enum": reasoned_edge_actions }
                                }
                            }
                        }
                    },
                    "then": {
                        "properties": {
                            "edge": { "required": ["detail"] }
                        }
                    }
                }
            ],
            "properties": {
                "name": { "type": "string" },
                "tool": { "type": "string" },
                "project": { "type": "string" },
                "direct": { "type": "boolean" },
                "downgrade": { "type": "boolean" },
                "registry": { "type": "string" },
                "members": members,
                "from": { "type": "string" },
                "to": { "type": "string" },
                "kind": { "enum": ["patch", "minor", "major"] },
                "applied": { "type": "boolean" },
                "skipped": { "$ref": "#/$defs/skippedInfo" },
                "error": { "$ref": "#/$defs/diagnostic" },
                "edge": { "$ref": "#/$defs/upgradeEdgeInfo" },
                "security": { "$ref": "#/$defs/securityInfo" }
            },
            "additionalProperties": false
        },
        // Present iff the row reports a lock-edge *binding* move between coexisting versions
        // (cargo `--cargo-edge-policy`); `from`/`to` then carry the binding versions (for `held`,
        // `to` is the withheld correction target and `detail` says why it was withheld).
        "upgradeEdgeInfo": {
            "type": "object",
            "required": ["dependent", "dependentVersion", "action"],
            "properties": {
                "dependent": { "type": "string" },
                "dependentVersion": { "type": "string" },
                "dependentSource": { "type": "string" },
                "action": { "enum": edge_actions },
                "detail": { "type": "string", "minLength": 1 }
            },
            "additionalProperties": false
        },
        "explainSummary": {
            "type": "object",
            "required": [],
            "properties": {},
            "additionalProperties": false
        },
        "explainStep": {
            "type": "object",
            "required": ["layer", "field", "applied", "note"],
            "properties": {
                "layer": { "type": "string" },
                "field": { "type": "string" },
                "selector": { "type": "string" },
                "minAgeDays": { "type": "number" },
                "applied": { "type": "boolean" },
                "note": { "type": "string" }
            },
            "additionalProperties": false
        },
        "configSummary": {
            "type": "object",
            "required": ["projects"],
            "properties": {
                "projects": { "type": "integer", "minimum": 0 }
            },
            "additionalProperties": false
        },
        "configItem": {
            "type": "object",
            "required": ["project", "tool", "effectiveDefaultMinAgeDays", "source", "strictNative", "layers", "advisories"],
            "properties": {
                "project": { "type": "string" },
                "tool": { "type": "string" },
                "effectiveDefaultMinAgeDays": { "type": "number" },
                "source": { "type": "string" },
                "strictNative": { "type": "boolean" },
                "layers": { "type": "array", "items": { "type": "string" } },
                "advisories": { "$ref": "#/$defs/advisoryConfigInfo" }
            },
            "additionalProperties": false
        },
        "advisoryConfigInfo": {
            "type": "object",
            "required": ["enabled", "source", "mode", "minAgeDays", "severity", "ecosystem"],
            "properties": {
                "enabled": { "type": "boolean" },
                "source": { "enum": ["osv", "github", "none"] },
                "mode": { "enum": ["flag", "shorten"] },
                "minAgeDays": { "type": "number" },
                "severity": { "enum": ["low", "moderate", "high", "critical", "unknown"] },
                "ecosystem": { "type": ["string", "null"] }
            },
            "additionalProperties": false
        },
        "baselineSummary": {
            "type": "object",
            "required": ["acknowledged", "pruned"],
            "properties": {
                "acknowledged": { "type": "integer", "minimum": 0 },
                "pruned": { "type": "integer", "minimum": 0 }
            },
            "additionalProperties": false
        },
        "baselineItem": {
            "type": "object",
            "required": ["tool", "project", "package", "version"],
            "properties": {
                "tool": { "type": "string" },
                "project": { "type": "string" },
                "package": { "type": "string" },
                "version": { "type": "string" },
                "registry": { "type": "string" }
            },
            "additionalProperties": false
        },
        "recoverySummary": {
            "type": "object",
            "required": ["recovered", "unchanged", "errors"],
            "properties": {
                "recovered": { "type": "integer", "minimum": 0 },
                "unchanged": { "type": "integer", "minimum": 0 },
                "errors": { "type": "integer", "minimum": 0 }
            },
            "additionalProperties": false
        },
        "recoveryItem": {
            "type": "object",
            "required": ["tool", "project", "status"],
            "properties": {
                "tool": { "type": "string" },
                "project": { "type": "string" },
                "status": { "enum": recovery_statuses }
            },
            "additionalProperties": false
        }
    });

    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("https://github.com/romnn/cooldown/schema/v{SCHEMA_VERSION}"),
        "title": "cooldown --json envelope",
        "$defs": defs,
        "oneOf": [
            envelope("outdated", Map::new(), vec![], "#/$defs/outdatedSummary", "#/$defs/outdatedItem"),
            envelope(
                "check",
                map(&[
                    ("scope", json!({ "enum": ["lockfile-graph", "direct-only"] })),
                    ("artifactScope", json!({ "enum": ["environment", "all"] }))
                ]),
                vec!["scope", "artifactScope"],
                "#/$defs/checkSummary",
                "#/$defs/checkItem"
            ),
            envelope(
                "upgrade",
                map(&[
                    ("applied", json!({ "type": "boolean" })),
                    ("dryRun", json!({ "type": "boolean" })),
                    ("lockStatus", json!({ "enum": ["current", "stale", "unknown", null] })),
                    ("build", json!({
                        "type": "object",
                        "required": ["requested", "ok"],
                        "properties": {
                            "requested": { "type": "boolean" },
                            "ok": { "type": ["boolean", "null"] }
                        },
                        "additionalProperties": false
                    }))
                ]),
                vec!["applied", "dryRun", "lockStatus", "build"],
                "#/$defs/upgradeSummary",
                "#/$defs/upgradeItem"
            ),
            envelope(
                "fix",
                map(&[
                    ("applied", json!({ "type": "boolean" })),
                    ("dryRun", json!({ "type": "boolean" })),
                    ("lockStatus", json!({ "enum": ["current", "stale", "unknown", null] })),
                    ("build", json!({
                        "type": "object",
                        "required": ["requested", "ok"],
                        "properties": {
                            "requested": { "type": "boolean" },
                            "ok": { "type": ["boolean", "null"] }
                        },
                        "additionalProperties": false
                    }))
                ]),
                vec!["applied", "dryRun", "lockStatus", "build"],
                "#/$defs/upgradeSummary",
                "#/$defs/upgradeItem"
            ),
            envelope(
                "explain",
                map(&[
                    ("project", json!({ "type": "string" })),
                    ("registry", json!({ "type": "string" })),
                    ("effective", json!({ "$ref": "#/$defs/effectiveInfo" }))
                ]),
                vec!["project", "effective"],
                "#/$defs/explainSummary",
                "#/$defs/explainStep"
            ),
            envelope("config", Map::new(), vec![], "#/$defs/configSummary", "#/$defs/configItem"),
            envelope(
                "baseline",
                map(&[
                    ("path", json!({ "type": "string" })),
                    ("dryRun", json!({ "type": "boolean" }))
                ]),
                vec!["path", "dryRun"],
                "#/$defs/baselineSummary",
                "#/$defs/baselineItem"
            ),
            envelope("recover", Map::new(), vec![], "#/$defs/recoverySummary", "#/$defs/recoveryItem")
        ]
    })
}

fn envelope(
    command: &'static str,
    meta_properties: Map<String, Value>,
    meta_required: Vec<&'static str>,
    summary_ref: &'static str,
    item_ref: &'static str,
) -> Value {
    let mut properties = map(&[
        ("schemaVersion", json!({ "const": SCHEMA_VERSION })),
        ("command", json!({ "const": command })),
        ("ok", json!({ "type": "boolean" })),
        (
            "generatedAt",
            json!({ "type": "string", "format": "date-time" }),
        ),
        ("summary", json!({ "$ref": summary_ref })),
        (
            "items",
            json!({ "type": "array", "items": { "$ref": item_ref } }),
        ),
        (
            "warnings",
            json!({ "type": "array", "items": { "$ref": "#/$defs/diagnostic" } }),
        ),
        (
            "errors",
            json!({ "type": "array", "items": { "$ref": "#/$defs/diagnostic" } }),
        ),
    ]);
    properties.extend(meta_properties);

    let mut required = vec![
        "schemaVersion",
        "command",
        "ok",
        "generatedAt",
        "summary",
        "items",
        "warnings",
        "errors",
    ];
    required.extend(meta_required);

    json!({
        "type": "object",
        "required": required,
        "properties": properties,
        "additionalProperties": false
    })
}

fn map(entries: &[(&str, Value)]) -> Map<String, Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect()
}

/// Returns the schema from [`json_schema`] as a pretty-printed JSON string.
///
/// This backs `cooldown schema`, which prints the document to stdout.
///
/// # Errors
///
/// Returns the underlying [`serde_json::Error`] if the schema [`Value`] cannot
/// be serialized. The schema is a fixed literal of JSON-representable values, so
/// this does not fail in practice.
///
/// # Examples
///
/// ```
/// use cooldown_render::json_schema_string;
///
/// let s = json_schema_string()?;
/// assert!(s.contains("\"cooldown --json envelope\""));
/// # Ok::<(), serde_json::Error>(())
/// ```
pub fn json_schema_string() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&json_schema())
}

#[cfg(test)]
mod tests {
    use super::json_schema;
    use crate::model::{
        AdvisoryConfigInfo, BaselineItem, BaselineMeta, BaselineSummary, BuildInfo, CheckItem,
        CheckMeta, CheckStatus, CheckSummary, ConfigItem, ConfigMeta, ConfigSummary, EffectiveInfo,
        Envelope, ExplainMeta, ExplainStep, ExplainSummary, LatestInfo, OutdatedItem, OutdatedMeta,
        OutdatedStatus, OutdatedSummary, RecoveryItem, RecoveryMeta, RecoveryStatus,
        RecoverySummary, SecurityInfo, SkippedInfo, UpgradeEdgeInfo, UpgradeItem, UpgradeMeta,
        UpgradeSummary, Window,
    };
    use color_eyre::eyre;
    use cooldown_core::{
        Diagnostic, DiagnosticKind, LockStatus, MemberRef, SkipReason, UpdateKind,
    };
    use serde::Serialize;
    use serde_json::Value;
    use std::collections::BTreeSet;

    #[test]
    fn schema_properties_match_serialized_models() {
        let schema = json_schema();
        assert_definition_keys_match(&schema);
        assert_envelope_keys_match(&schema);
    }

    #[test]
    fn held_by_serializes_with_the_additive_camel_case_field() {
        let value = serde_json::to_value(outdated_item()).expect("item serializes");
        assert_eq!(value["heldBy"], "bound <2");
    }

    #[test]
    fn edge_discriminator_conditions_enforce_row_invariants() -> eyre::Result<()> {
        let schema = json_schema();
        assert_eq!(
            schema["$defs"]["upgradeEdgeInfo"]["properties"]["detail"]["minLength"],
            1
        );
        let conditions = schema["$defs"]["upgradeItem"]["allOf"]
            .as_array()
            .ok_or_else(|| eyre::eyre!("missing edge action conditions"))?;

        let unapplied = conditions[0]["if"]["properties"]["edge"]["properties"]["action"]["enum"]
            .as_array()
            .ok_or_else(|| eyre::eyre!("missing unapplied edge actions"))?;
        for action in cooldown_core::EdgeBindingAction::ALL {
            let listed = unapplied.contains(&Value::String(action.wire_value().to_string()));
            assert_eq!(listed, !action.is_applied());
        }
        assert_eq!(
            conditions[0]["then"]["properties"]["applied"]["const"],
            false
        );
        let applied = conditions[1]["if"]["properties"]["edge"]["properties"]["action"]["enum"]
            .as_array()
            .ok_or_else(|| eyre::eyre!("missing applied edge actions"))?;
        for action in cooldown_core::EdgeBindingAction::ALL {
            let listed = applied.contains(&Value::String(action.wire_value().to_string()));
            assert_eq!(listed, action.is_applied());
        }
        assert_eq!(
            conditions[1]["then"]["properties"]["applied"]["const"],
            true
        );
        assert_eq!(
            conditions[2]["then"]["properties"]["direct"]["const"],
            false
        );
        assert_eq!(
            conditions[2]["then"]["properties"]["downgrade"]["const"],
            false
        );
        assert_eq!(
            conditions[2]["then"]["properties"]["members"]["maxItems"],
            0
        );
        let prohibited = conditions[2]["then"]["not"]["anyOf"]
            .as_array()
            .ok_or_else(|| eyre::eyre!("missing edge-only prohibited properties"))?;
        assert_eq!(prohibited[0]["required"][0], "skipped");
        assert_eq!(prohibited[1]["required"][0], "error");
        let reasoned = conditions[3]["if"]["properties"]["edge"]["properties"]["action"]["enum"]
            .as_array()
            .ok_or_else(|| eyre::eyre!("missing reasoned edge actions"))?;
        assert_eq!(
            conditions[3]["then"]["properties"]["edge"]["required"][0],
            "detail"
        );
        for action in cooldown_core::EdgeBindingAction::ALL {
            let listed = reasoned.contains(&Value::String(action.wire_value().to_string()));
            assert_eq!(listed, action.requires_detail());
        }
        Ok(())
    }

    #[test]
    fn ceiling_skips_rank_beside_needs_major() {
        for reason in [
            SkipReason::NeedsMajor,
            SkipReason::DeclaredBoundHeld,
            SkipReason::MaxMajorHeld,
            SkipReason::DistTagHeld,
        ] {
            let mut item = upgrade_version_item();
            item.skipped = Some(SkippedInfo {
                reason,
                message: reason.message().to_string(),
                offending: None,
            });
            assert_eq!(item.sort_rank(), 2);
        }
    }

    /// Walks `SkipReason::ALL` — the enum's own enumeration, not a copy — so a variant absent
    /// from the published schema's `reason` enum fails here, whatever variant it is.
    #[test]
    fn skipped_reason_schema_accepts_every_serialized_variant() {
        let schema = json_schema();
        let allowed = schema["$defs"]["skippedInfo"]["properties"]["reason"]["enum"]
            .as_array()
            .expect("skip reason enum");
        for reason in SkipReason::ALL {
            let serialized = serde_json::to_value(reason).expect("reason serializes");
            assert!(
                allowed.contains(&serialized),
                "schema is missing {serialized}"
            );
        }
        assert_eq!(
            allowed.len(),
            SkipReason::ALL.len(),
            "the schema enum must not carry stale extra values either"
        );
    }

    #[test]
    fn recovery_status_schema_accepts_every_typed_variant() -> eyre::Result<()> {
        let schema = json_schema();
        let allowed = schema["$defs"]["recoveryItem"]["properties"]["status"]["enum"]
            .as_array()
            .ok_or_else(|| eyre::eyre!("recovery status enum is not an array"))?;
        for status in RecoveryStatus::ALL {
            assert!(allowed.contains(&Value::String(status.wire_value().to_string())));
        }
        assert_eq!(allowed.len(), RecoveryStatus::ALL.len());
        Ok(())
    }

    fn assert_definition_keys_match(schema: &Value) {
        assert_def_keys(schema, "diagnostic", diagnostic());
        assert_def_keys(schema, "window", window());
        assert_def_keys(schema, "latestInfo", latest_info());
        assert_def_keys(schema, "memberRef", member());
        assert_def_keys(schema, "skippedInfo", skipped_info());
        assert_def_keys(schema, "effectiveInfo", effective_info());
        assert_def_keys(schema, "outdatedSummary", outdated_summary());
        assert_def_keys(schema, "outdatedItem", outdated_item());
        assert_def_keys(schema, "checkSummary", check_summary());
        assert_def_keys(schema, "checkItem", check_item());
        assert_def_keys(schema, "upgradeSummary", upgrade_summary());
        let upgrade_keys: BTreeSet<String> = serialized_keys(upgrade_version_item())
            .union(&serialized_keys(upgrade_edge_item()))
            .cloned()
            .collect();
        assert_eq!(
            upgrade_keys,
            schema_keys(&schema["$defs"]["upgradeItem"]),
            "$defs.upgradeItem is out of sync"
        );
        assert_def_keys(schema, "upgradeEdgeInfo", upgrade_edge_info());
        assert_def_keys(schema, "explainSummary", ExplainSummary {});
        assert_def_keys(schema, "explainStep", explain_step());
        assert_def_keys(schema, "configSummary", config_summary());
        assert_def_keys(schema, "configItem", config_item());
        assert_def_keys(schema, "baselineSummary", baseline_summary());
        assert_def_keys(schema, "baselineItem", baseline_item());
        assert_def_keys(schema, "recoverySummary", recovery_summary());
        assert_def_keys(schema, "recoveryItem", recovery_item());
    }

    fn assert_envelope_keys_match(schema: &Value) {
        assert_envelope_keys(
            schema,
            "outdated",
            Envelope::new(
                "outdated",
                true,
                generated_at(),
                OutdatedMeta {},
                outdated_summary(),
                vec![outdated_item()],
            ),
        );
        assert_envelope_keys(
            schema,
            "check",
            Envelope::new(
                "check",
                false,
                generated_at(),
                check_meta(),
                check_summary(),
                vec![check_item()],
            ),
        );
        assert_envelope_keys(
            schema,
            "upgrade",
            Envelope::new(
                "upgrade",
                true,
                generated_at(),
                upgrade_meta(),
                upgrade_summary(),
                vec![upgrade_version_item()],
            ),
        );
        assert_envelope_keys(
            schema,
            "fix",
            Envelope::new(
                "fix",
                true,
                generated_at(),
                upgrade_meta(),
                upgrade_summary(),
                vec![upgrade_version_item()],
            ),
        );
        assert_envelope_keys(
            schema,
            "explain",
            Envelope::new(
                "explain",
                true,
                generated_at(),
                explain_meta(),
                ExplainSummary {},
                vec![explain_step()],
            ),
        );
        assert_envelope_keys(
            schema,
            "config",
            Envelope::new(
                "config",
                true,
                generated_at(),
                ConfigMeta {},
                config_summary(),
                vec![config_item()],
            ),
        );
        assert_envelope_keys(
            schema,
            "baseline",
            Envelope::new(
                "baseline",
                true,
                generated_at(),
                baseline_meta(),
                baseline_summary(),
                vec![baseline_item()],
            ),
        );
        assert_envelope_keys(
            schema,
            "recover",
            Envelope::new(
                "recover",
                true,
                generated_at(),
                RecoveryMeta {},
                recovery_summary(),
                vec![recovery_item()],
            ),
        );
    }

    fn assert_def_keys<T: Serialize>(schema: &Value, def_name: &str, value: T) {
        let actual = serialized_keys(value);
        let expected = schema_keys(&schema["$defs"][def_name]);
        assert_eq!(actual, expected, "$defs.{def_name} is out of sync");
    }

    fn assert_envelope_keys<M, S, I>(schema: &Value, command: &'static str, env: Envelope<M, S, I>)
    where
        M: Serialize,
        S: Serialize,
        I: Serialize,
    {
        let actual = serialized_keys(env);
        let one_of = schema["oneOf"].as_array().expect("schema oneOf");
        let command_schema = one_of
            .iter()
            .find(|entry| entry["properties"]["command"]["const"] == command)
            .unwrap_or_else(|| panic!("schema for {command}"));
        let expected = schema_keys(command_schema);
        assert_eq!(actual, expected, "{command} envelope is out of sync");
    }

    fn serialized_keys<T: Serialize>(value: T) -> BTreeSet<String> {
        let value = serde_json::to_value(value).expect("sample serializes");
        value
            .as_object()
            .expect("sample object")
            .keys()
            .cloned()
            .collect()
    }

    fn schema_keys(schema: &Value) -> BTreeSet<String> {
        schema["properties"]
            .as_object()
            .expect("schema properties")
            .keys()
            .cloned()
            .collect()
    }

    fn generated_at() -> String {
        "2026-06-17T13:00:00Z".to_string()
    }

    fn diagnostic() -> Diagnostic {
        Diagnostic::new(DiagnosticKind::Held, "held")
            .with_tool("cargo")
            .with_project(".")
            .with_package("serde")
            .with_version("1.0.0")
            .with_registry("crates.io")
            .with_path("Cargo.toml")
    }

    fn recovery_summary() -> RecoverySummary {
        RecoverySummary {
            recovered: 1,
            unchanged: 2,
            errors: 0,
        }
    }

    fn recovery_item() -> RecoveryItem {
        RecoveryItem {
            tool: "cargo".to_string(),
            project: ".".to_string(),
            status: RecoveryStatus::Restored,
        }
    }

    fn member() -> MemberRef {
        MemberRef {
            name: "app".to_string(),
            path: "crates/app".to_string(),
        }
    }

    fn window() -> Window {
        Window {
            min_age_days: 7.0,
            source: "default".to_string(),
            clamped_by: Some("native".to_string()),
            shortened_by: Some("GHSA-v778-237x-gjrc".to_string()),
        }
    }

    fn security_info() -> SecurityInfo {
        SecurityInfo {
            version: "0.31.0".to_string(),
            fixes: vec!["GHSA-v778-237x-gjrc".to_string()],
            severity: "high".to_string(),
            source: "osv".to_string(),
            applied: true,
        }
    }

    fn latest_info() -> LatestInfo {
        LatestInfo {
            version: "1.2.3".to_string(),
            published_at: Some(generated_at()),
            age_days: Some(12.5),
        }
    }

    fn skipped_info() -> SkippedInfo {
        SkippedInfo {
            reason: SkipReason::ResolverConflict,
            message: "held: conflicts with typer".to_string(),
            offending: Some("typer".to_string()),
        }
    }

    fn effective_info() -> EffectiveInfo {
        EffectiveInfo {
            min_age_days: 7.0,
            decided_by: "default".to_string(),
        }
    }

    fn outdated_summary() -> OutdatedSummary {
        OutdatedSummary {
            total: 1,
            adoptable: 1,
            blocked: 0,
            in_cooldown: 0,
            up_to_date: 0,
            exempt: 0,
            held: 0,
            unknown_age: 0,
            errors: 0,
            skipped_stale_projects: 0,
        }
    }

    fn outdated_item() -> OutdatedItem {
        OutdatedItem {
            name: "serde".to_string(),
            tool: "cargo".to_string(),
            project: ".".to_string(),
            registry: Some("crates.io".to_string()),
            direct: true,
            current: "1.0.0".to_string(),
            members: vec![member()],
            window: window(),
            candidate_age_days: Some(12.5),
            cooldown_version: Some("1.2.0".to_string()),
            status: OutdatedStatus::Adoptable,
            adoptable_target: Some("1.2.3".to_string()),
            blocked_by: Some("typer".to_string()),
            held_by: Some(cooldown_core::HeldReason::DeclaredBound("<2".to_string()).into()),
            latest: Some(latest_info()),
            security: Some(security_info()),
            error: Some(diagnostic()),
        }
    }

    fn check_meta() -> CheckMeta {
        CheckMeta {
            scope: "lockfile-graph".to_string(),
            artifact_scope: "environment".to_string(),
        }
    }

    fn check_summary() -> CheckSummary {
        CheckSummary {
            checked: 1,
            direct: 1,
            exempt: 0,
            acknowledged: 0,
            allowed: 0,
            unknown_age: 0,
            errors: 0,
            violations: 1,
            security_relevant: 0,
            skipped_stale_projects: 0,
        }
    }

    fn check_item() -> CheckItem {
        CheckItem {
            name: "serde".to_string(),
            tool: "cargo".to_string(),
            project: ".".to_string(),
            members: vec![member()],
            registry: Some("crates.io".to_string()),
            direct: true,
            current: "1.0.0".to_string(),
            published_at: Some(generated_at()),
            age_days: Some(1.0),
            window: window(),
            status: CheckStatus::Violation,
            graph_held: true,
            graph_floor: Some("1.0.0".to_string()),
            security: Some(security_info()),
            error: Some(diagnostic()),
        }
    }

    fn upgrade_meta() -> UpgradeMeta {
        UpgradeMeta {
            applied: true,
            dry_run: false,
            lock_status: Some(LockStatus::Current),
            build: BuildInfo {
                requested: true,
                ok: Some(true),
            },
        }
    }

    fn upgrade_summary() -> UpgradeSummary {
        UpgradeSummary {
            applied: 1,
            skipped: 1,
            errors: 0,
            edges_corrected: 1,
            edges_rebound: 1,
            edges_held: 1,
            edges_unaddressable: 1,
        }
    }

    fn upgrade_version_item() -> UpgradeItem {
        UpgradeItem {
            name: "serde".to_string(),
            tool: "cargo".to_string(),
            project: ".".to_string(),
            direct: true,
            downgrade: false,
            members: vec![member()],
            registry: Some("crates.io".to_string()),
            from: "1.0.0".to_string(),
            to: "1.2.3".to_string(),
            kind: UpdateKind::Minor,
            applied: false,
            skipped: Some(skipped_info()),
            error: Some(diagnostic()),
            edge: None,
            security: Some(security_info()),
        }
    }

    fn upgrade_edge_item() -> UpgradeItem {
        UpgradeItem {
            name: "serde".to_string(),
            tool: "cargo".to_string(),
            project: ".".to_string(),
            direct: false,
            downgrade: false,
            members: Vec::new(),
            registry: Some("crates.io".to_string()),
            from: "1.0.0".to_string(),
            to: "1.2.3".to_string(),
            kind: UpdateKind::Minor,
            applied: false,
            skipped: None,
            error: None,
            edge: Some(upgrade_edge_info()),
            security: None,
        }
    }

    fn upgrade_edge_info() -> UpgradeEdgeInfo {
        UpgradeEdgeInfo {
            dependent: "diesel".to_string(),
            dependent_version: "2.3.11".to_string(),
            dependent_source: Some("registry+https://example.invalid/index".to_string()),
            action: cooldown_core::EdgeBindingAction::Held,
            detail: Some("would orphan its last lock reference".to_string()),
        }
    }

    fn explain_meta() -> ExplainMeta {
        ExplainMeta {
            project: ".".to_string(),
            registry: Some("crates.io".to_string()),
            effective: effective_info(),
        }
    }

    fn explain_step() -> ExplainStep {
        ExplainStep {
            layer: "default".to_string(),
            field: "minAge".to_string(),
            selector: Some("serde".to_string()),
            min_age_days: Some(7.0),
            applied: true,
            note: "default window".to_string(),
        }
    }

    fn config_summary() -> ConfigSummary {
        ConfigSummary { projects: 1 }
    }

    fn config_item() -> ConfigItem {
        ConfigItem {
            project: ".".to_string(),
            tool: "cargo".to_string(),
            effective_default_min_age_days: 7.0,
            source: "default".to_string(),
            strict_native: true,
            layers: vec!["default".to_string(), "workspace".to_string()],
            advisories: AdvisoryConfigInfo {
                enabled: true,
                source: "osv".to_string(),
                mode: "shorten".to_string(),
                min_age_days: 1.0,
                severity: "high".to_string(),
                ecosystem: Some("crates.io".to_string()),
            },
        }
    }

    fn baseline_meta() -> BaselineMeta {
        BaselineMeta {
            path: ".cooldown-baseline.toml".to_string(),
            dry_run: true,
        }
    }

    fn baseline_summary() -> BaselineSummary {
        BaselineSummary {
            acknowledged: 1,
            pruned: 0,
        }
    }

    fn baseline_item() -> BaselineItem {
        BaselineItem {
            tool: "cargo".to_string(),
            project: ".".to_string(),
            package: "serde".to_string(),
            version: "1.0.0".to_string(),
            registry: Some("crates.io".to_string()),
        }
    }
}
