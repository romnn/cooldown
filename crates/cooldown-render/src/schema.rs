//! The machine-readable JSON schema for `--json` output, printed by `cooldown schema`.

use crate::model::SCHEMA_VERSION;
use serde_json::{json, Value};

/// A JSON Schema (draft 2020-12) describing the common envelope and the per-command item shapes.
pub fn json_schema() -> Value {
    let diagnostic = json!({
        "type": "object",
        "required": ["kind", "message"],
        "properties": {
            "kind": { "enum": ["transient","not_found","unknown_age","stricter_native","yanked","stale_lock","tool_failed","lockfile_unreadable"] },
            "message": { "type": "string" },
            "ecosystem": { "type": "string" },
            "project": { "type": "string" },
            "package": { "type": "string" },
            "version": { "type": "string" },
            "registry": { "type": "string" },
            "tool": { "type": "string" },
            "path": { "type": "string" }
        }
    });
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://github.com/romnn/cooldown/schema/v1",
        "title": "cooldown --json envelope",
        "type": "object",
        "required": ["schemaVersion", "command", "ok", "generatedAt", "summary", "items", "warnings", "errors"],
        "properties": {
            "schemaVersion": { "const": SCHEMA_VERSION },
            "command": { "enum": ["outdated","check","upgrade","explain","config","baseline"] },
            "ok": { "type": "boolean", "description": "mirrors the exit code (true iff 0)" },
            "generatedAt": { "type": "string", "format": "date-time" },
            "scope": { "enum": ["lockfile-graph","direct-only"], "description": "check only" },
            "artifactScope": { "enum": ["environment","all"], "description": "check only" },
            "applied": { "type": "boolean", "description": "upgrade only" },
            "lockVerified": { "type": ["boolean","null"], "description": "upgrade only" },
            "build": {
                "type": "object",
                "properties": { "requested": {"type":"boolean"}, "ok": {"type":["boolean","null"]} },
                "description": "upgrade only"
            },
            "effective": {
                "type": "object",
                "properties": { "minAgeDays": {"type":"number"}, "decidedBy": {"type":"string"} },
                "description": "explain only"
            },
            "summary": { "type": "object", "description": "command-specific counts" },
            "items": { "type": "array", "items": { "type": "object" } },
            "warnings": { "type": "array", "items": diagnostic.clone() },
            "errors": { "type": "array", "items": diagnostic }
        },
        "additionalProperties": true
    })
}

/// The schema, pretty-printed.
pub fn json_schema_string() -> String {
    serde_json::to_string_pretty(&json_schema()).expect("schema serializes")
}
