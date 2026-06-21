//! The machine-readable JSON schema for `--json` output, printed by `cooldown schema`.

use crate::model::SCHEMA_VERSION;
use serde_json::{Map, Value, json};

/// A JSON Schema (draft 2020-12) describing the command-specific envelopes.
#[allow(
    clippy::too_many_lines,
    reason = "the schema is a declarative contract literal; splitting it further would obscure the envelope shape"
)]
#[must_use]
pub fn json_schema() -> Value {
    let diagnostic = json!({
        "type": "object",
        "required": ["kind", "message"],
        "properties": {
            "kind": {
                "enum": [
                    "transient",
                    "not_found",
                    "unknown_age",
                    "stricter_native",
                    "yanked",
                    "stale_lock",
                    "tool_failed",
                    "tool_spawn_failed",
                    "lockfile_unreadable",
                    "filesystem",
                    "path_encoding",
                    "serialization",
                    "lock_conflict",
                    "system",
                    "config",
                    "parse",
                    "held"
                ]
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
            "clampedBy": { "type": "string" }
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

    let skipped = json!({
        "type": "object",
        "required": ["reason", "message"],
        "properties": {
            "reason": { "enum": ["graph_held", "transitive_in_cooldown", "resolver_conflict", "not_eligible", "needs_major"] },
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
        "latestInfo": latest,
        "memberRef": member_ref,
        "skippedInfo": skipped,
        "effectiveInfo": effective,
        "outdatedSummary": {
            "type": "object",
            "required": ["total", "adoptable", "inCooldown", "upToDate", "exempt", "held", "unknownAge", "errors"],
            "properties": {
                "total": { "type": "integer", "minimum": 0 },
                "adoptable": { "type": "integer", "minimum": 0 },
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
                        "in_cooldown",
                        "exempt",
                        "held",
                        "current_in_cooldown",
                        "unknown_age",
                        "error"
                    ]
                },
                "adoptableTarget": { "type": "string" },
                "latest": { "$ref": "#/$defs/latestInfo" },
                "error": { "$ref": "#/$defs/diagnostic" }
            },
            "additionalProperties": false
        },
        "checkSummary": {
            "type": "object",
            "required": ["checked", "direct", "exempt", "acknowledged", "allowed", "unknownAge", "errors", "violations"],
            "properties": {
                "checked": { "type": "integer", "minimum": 0 },
                "direct": { "type": "integer", "minimum": 0 },
                "exempt": { "type": "integer", "minimum": 0 },
                "acknowledged": { "type": "integer", "minimum": 0 },
                "allowed": { "type": "integer", "minimum": 0 },
                "unknownAge": { "type": "integer", "minimum": 0 },
                "errors": { "type": "integer", "minimum": 0 },
                "violations": { "type": "integer", "minimum": 0 }
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
                "error": { "$ref": "#/$defs/diagnostic" }
            },
            "additionalProperties": false
        },
        "upgradeSummary": {
            "type": "object",
            "required": ["applied", "skipped", "errors"],
            "properties": {
                "applied": { "type": "integer", "minimum": 0 },
                "skipped": { "type": "integer", "minimum": 0 },
                "errors": { "type": "integer", "minimum": 0 }
            },
            "additionalProperties": false
        },
        "upgradeItem": {
            "type": "object",
            "required": ["name", "tool", "project", "direct", "from", "to", "kind", "applied"],
            "properties": {
                "name": { "type": "string" },
                "tool": { "type": "string" },
                "project": { "type": "string" },
                "direct": { "type": "boolean" },
                "registry": { "type": "string" },
                "members": members,
                "from": { "type": "string" },
                "to": { "type": "string" },
                "kind": { "enum": ["patch", "minor", "major"] },
                "applied": { "type": "boolean" },
                "skipped": { "$ref": "#/$defs/skippedInfo" },
                "error": { "$ref": "#/$defs/diagnostic" }
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
            "required": ["project", "tool", "effectiveDefaultMinAgeDays", "source", "strictNative", "layers"],
            "properties": {
                "project": { "type": "string" },
                "tool": { "type": "string" },
                "effectiveDefaultMinAgeDays": { "type": "number" },
                "source": { "type": "string" },
                "strictNative": { "type": "boolean" },
                "layers": { "type": "array", "items": { "type": "string" } }
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
        }
    });

    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://github.com/romnn/cooldown/schema/v1",
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
                    ("lockVerified", json!({ "type": ["boolean", "null"] })),
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
                vec!["applied", "lockVerified", "build"],
                "#/$defs/upgradeSummary",
                "#/$defs/upgradeItem"
            ),
            envelope(
                "fix",
                map(&[
                    ("applied", json!({ "type": "boolean" })),
                    ("lockVerified", json!({ "type": ["boolean", "null"] })),
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
                vec!["applied", "lockVerified", "build"],
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
            )
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
